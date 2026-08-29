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
