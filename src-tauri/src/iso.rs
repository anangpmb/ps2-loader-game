use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum IsoError {
    #[error("File not found: {0}")]
    NotFound(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Invalid ISO: {0}")]
    InvalidFormat(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IsoInfo {
    pub valid: bool,
    pub size: u64,
    pub format: Option<String>,
    pub error: Option<String>,
    pub game_id: Option<String>,
    pub title: Option<String>,
}

/// Validate a PS2 ISO file.
/// Checks:
/// 1. File exists and is readable
/// 2. ISO9660 primary volume descriptor at sector 16 (offset 0x8000)
/// 3. UDF recognition volume descriptor (fallback)
/// 4. SYSTEM.CNF presence (PS2-specific marker)
pub fn validate_iso(path: &Path) -> Result<IsoInfo, IsoError> {
    if !path.exists() {
        return Err(IsoError::NotFound(path.display().to_string()));
    }

    let metadata = std::fs::metadata(path)?;
    let size = metadata.len();

    if size < 0x8000 + 2048 {
        return Ok(IsoInfo {
            valid: false,
            size,
            format: None,
            error: Some("File too small to be a valid ISO".into()),
            game_id: None,
            title: None,
        });
    }

    let mut file = File::open(path)?;

    // Check ISO9660 primary volume descriptor at sector 16
    let mut buf = [0u8; 6];
    file.seek(SeekFrom::Start(0x8000))?;
    file.read_exact(&mut buf)?;

    let format = if buf[0] == 0x01 && &buf[1..6] == b"CD001" {
        Some("ISO9660".to_string())
    } else {
        // Check for UDF at sector 256
        let mut udf_buf = [0u8; 5];
        file.seek(SeekFrom::Start(0x80000))?; // sector 256 * 2048
        if file.read_exact(&mut udf_buf).is_ok() && &udf_buf == b"BEA01" {
            Some("UDF".to_string())
        } else {
            None
        }
    };

    let format = match format {
        Some(f) => f,
        None => {
            return Ok(IsoInfo {
                valid: false,
                size,
                format: None,
                error: Some("Not a valid ISO9660/UDF image".into()),
                game_id: None,
                title: None,
            });
        }
    };

    // Try to extract game ID from SYSTEM.CNF
    // PS2 SYSTEM.CNF typically contains: BOOT2 = cdrom0:\SLUS_XXX.XX;1
    let game_id = extract_game_id(&mut file).ok();

    // Extract volume label from ISO9660 primary volume descriptor
    // Volume label is at offset 0x8028 (40 bytes) from sector 16 start
    let title = if format == "ISO9660" {
        extract_volume_label(&mut file)
    } else {
        None
    };

    Ok(IsoInfo {
        valid: true,
        size,
        format: Some(format),
        error: None,
        game_id,
        title,
    })
}

/// Determine whether a PS2 ISO is CD (0x12) or DVD (0x14) media.
///
/// Detection order:
/// 1. **UDF VRS check** — PS2 DVD titles use an ISO 9660 + UDF Bridge disc
///    layout; the UDF Volume Recognition Sequence begins at sector 256
///    (byte offset 0x80000) with the descriptor tag `"BEA01"`. CD-only titles
///    use plain ISO 9660 with no UDF, so that sector either doesn't exist or
///    doesn't carry `"BEA01"`.
/// 2. **Size fallback** — If the UDF check is inconclusive (sector 256 missing
///    or unreadable), any image > 700 MiB is treated as DVD.
///
/// Returns `0x14` (DVD) or `0x12` (CD).
pub fn detect_media_type(path: &Path) -> u8 {
    const UDF_SECTOR_OFFSET: u64 = 256 * 2048; // 0x80000
    const CD_MAX_BYTES: u64 = 700 * 1024 * 1024;
    const MEDIA_DVD: u8 = 0x14;
    const MEDIA_CD: u8 = 0x12;

    if let Ok(mut file) = File::open(path) {
        let file_size = file.metadata().map(|m| m.len()).unwrap_or(0);

        // UDF VRS "BEA01" at sector 256 is present on DVD, absent on CD.
        if file_size > UDF_SECTOR_OFFSET + 5 {
            let mut buf = [0u8; 5];
            if file.seek(SeekFrom::Start(UDF_SECTOR_OFFSET)).is_ok()
                && file.read_exact(&mut buf).is_ok()
                && &buf == b"BEA01"
            {
                return MEDIA_DVD;
            }
        }

        // Size fallback: images over CD capacity must be DVD.
        if file_size > CD_MAX_BYTES {
            return MEDIA_DVD;
        }
    }

    MEDIA_CD
}

/// Read the raw PS2 startup id from an ISO's SYSTEM.CNF, WITH the dot preserved.
///
/// Returns e.g. `SLUS_217.46` (verbatim from `BOOT2 = cdrom0:\SLUS_217.46;1`).
/// This is the form used for USBExtreme chunk filenames and the `ul.cfg` image
/// field — unlike [`extract_game_id`], which strips the dot for display.
pub fn extract_startup(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    extract_startup_raw(&mut file)
}

fn extract_startup_raw(file: &mut File) -> Option<String> {
    let search_size: u64 = 4 * 1024 * 1024;
    let file_size = file.metadata().ok()?.len();
    let read_size = search_size.min(file_size);

    file.seek(SeekFrom::Start(0)).ok()?;
    let mut buffer = vec![0u8; read_size as usize];
    file.read_exact(&mut buffer).ok()?;

    let search_str = String::from_utf8_lossy(&buffer);
    for line in search_str.lines() {
        let trimmed = line.trim();
        if trimmed.contains("BOOT2") && trimmed.contains("cdrom0:") {
            if let Some(start) = trimmed.find('\\') {
                let after_slash = &trimmed[start + 1..];
                if let Some(end) = after_slash.find(';') {
                    let id = after_slash[..end].trim();
                    if !id.is_empty() {
                        return Some(id.to_string());
                    }
                }
            }
        }
    }
    None
}

/// Extract PS2 game ID from SYSTEM.CNF embedded in the ISO.
/// Searches for the BOOT2 line pattern: cdrom0:\XXXX_XXX.XX;1
fn extract_game_id(file: &mut File) -> Result<String, IsoError> {
    // Read a chunk of data to search for SYSTEM.CNF content
    // The BOOT2 line is usually in the first few MB of the ISO
    let search_size: u64 = 4 * 1024 * 1024; // 4MB should be enough
    let file_size = file.metadata()?.len();
    let read_size = search_size.min(file_size);

    file.seek(SeekFrom::Start(0))?;
    let mut buffer = vec![0u8; read_size as usize];
    file.read_exact(&mut buffer)?;

    // Search for BOOT2 pattern
    let search_str = String::from_utf8_lossy(&buffer);
    for line in search_str.lines() {
        let trimmed = line.trim();
        if trimmed.contains("BOOT2") && trimmed.contains("cdrom0:") {
            // Extract the ID portion: cdrom0:\SLUS_200.00;1
            if let Some(start) = trimmed.find('\\') {
                let after_slash = &trimmed[start + 1..];
                if let Some(end) = after_slash.find(';') {
                    let id = &after_slash[..end];
                    // Normalize: remove dots -> SLUS_20000
                    let normalized = id.replace('.', "");
                    return Ok(normalized);
                }
            }
        }
    }

    Err(IsoError::InvalidFormat("SYSTEM.CNF not found".into()))
}

/// Extract volume label from ISO9660 primary volume descriptor.
/// Volume label is at offset 0x40 (64 bytes) from the start of the PVD.
/// PVD starts at sector 16 (offset 0x8000), so label is at 0x8040.
/// Actually: PVD type (1 byte at 0x8000), then standard ID (5 bytes), version (1 byte),
/// then volume flags (1 byte), then volume label at offset 0x8028 (40 bytes).
fn extract_volume_label(file: &mut File) -> Option<String> {
    // Read the volume label field (40 bytes at offset 0x8028)
    let mut label = [0u8; 40];
    file.seek(SeekFrom::Start(0x8028)).ok()?;
    file.read_exact(&mut label).ok()?;

    // The label may be space-padded and/or D-characters
    let label_str = String::from_utf8_lossy(&label);
    let trimmed = label_str.trim_end_matches(' ').trim_end_matches('\0');

    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_validate_nonexistent() {
        let result = validate_iso(Path::new("/nonexistent.iso"));
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_too_small() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_small.iso");
        let mut f = File::create(&path).unwrap();
        f.write_all(&[0u8; 100]).unwrap();

        let result = validate_iso(&path).unwrap();
        assert!(!result.valid);
        assert!(result.error.is_some());

        std::fs::remove_file(&path).ok();
    }
}
