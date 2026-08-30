use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use thiserror::Error;

#[derive(Error, Debug)]
#[allow(dead_code)]
pub enum UlCfgError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
    #[error("Parse error: {0}")]
    Parse(String),
}

/// OPL `ul.cfg` game entry — the real USBExtreme binary format.
///
/// Each record is 64 bytes:
/// - `0x00` (32 bytes): display name (ASCII, null-padded)
/// - `0x20` (15 bytes): `"ul."` + game id (e.g. `ul.SLUS_217.46`), null-padded
/// - `0x2F` (1 byte):  parts count (number of chunk files)
/// - `0x30` (1 byte):  media type (`0x12` = CD, `0x14` = DVD)
/// - `0x31`..`0x35`:   reserved / zero
/// - `0x35` (1 byte):  magic `0x08` (USBExtreme marker)
/// - `0x36`..`0x40`:   reserved / zero
///
/// OPL locates the chunk files by recomputing a CRC32 (see [`crate::opl_crc`]) of
/// the `name` field, so the name must hash to the CRC embedded in the filenames.
pub const ENTRY_SIZE: usize = 64;
const NAME_SIZE: usize = 32;
const IMAGE_OFFSET: usize = 0x20;
const IMAGE_SIZE: usize = 15;
const PARTS_OFFSET: usize = 0x2F;
const MEDIA_OFFSET: usize = 0x30;
const MAGIC_OFFSET: usize = 0x35;
const MAGIC: u8 = 0x08;

/// Media type bytes.
pub const MEDIA_CD: u8 = 0x12;
pub const MEDIA_DVD: u8 = 0x14;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UlEntry {
    pub title: String,
    /// Game id WITHOUT the `ul.` prefix, e.g. `SLUS_217.46`.
    pub game_id: String,
    pub parts: u16,
    pub media: u8,
    pub mount_point: String,
}

impl Default for UlEntry {
    fn default() -> Self {
        Self {
            title: String::new(),
            game_id: String::new(),
            parts: 1,
            media: MEDIA_DVD,
            mount_point: String::new(),
        }
    }
}

/// Parse an existing `ul.cfg` file (real 64-byte OPL format).
pub fn parse_ulcfg(path: &Path) -> Result<Vec<UlEntry>, UlCfgError> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    let data = fs::read(path)?;
    let mut entries = Vec::new();
    let mut offset = 0;

    while offset + ENTRY_SIZE <= data.len() {
        let record = &data[offset..offset + ENTRY_SIZE];

        let title = null_terminated_str(&record[..NAME_SIZE]);

        // Image field is "ul." + game id; strip the prefix for our model.
        let image = null_terminated_str(&record[IMAGE_OFFSET..IMAGE_OFFSET + IMAGE_SIZE]);
        let game_id = image.strip_prefix("ul.").unwrap_or(&image).to_string();

        let parts = record[PARTS_OFFSET] as u16;
        let media = record[MEDIA_OFFSET];

        if !title.is_empty() {
            entries.push(UlEntry {
                title,
                game_id,
                parts,
                media,
                mount_point: String::new(),
            });
        }

        offset += ENTRY_SIZE;
    }

    Ok(entries)
}

/// Write `ul.cfg` with the given entries in the real 64-byte OPL format.
pub fn write_ulcfg(path: &Path, entries: &[UlEntry]) -> Result<(), UlCfgError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;

    for entry in entries {
        file.write_all(&encode_entry(entry))?;
    }

    file.flush()?;
    Ok(())
}

/// Encode a single 64-byte record.
fn encode_entry(entry: &UlEntry) -> [u8; ENTRY_SIZE] {
    let mut buf = [0u8; ENTRY_SIZE];

    // Name (32 bytes, null-padded)
    let name_bytes = entry.title.as_bytes();
    let name_len = name_bytes.len().min(NAME_SIZE - 1);
    buf[..name_len].copy_from_slice(&name_bytes[..name_len]);

    // Image: "ul." + game id (15 bytes, null-padded)
    let image = format!("ul.{}", entry.game_id);
    let image_bytes = image.as_bytes();
    let image_len = image_bytes.len().min(IMAGE_SIZE - 1);
    buf[IMAGE_OFFSET..IMAGE_OFFSET + image_len].copy_from_slice(&image_bytes[..image_len]);

    // Parts (1 byte)
    buf[PARTS_OFFSET] = entry.parts.min(255) as u8;

    // Media (1 byte)
    buf[MEDIA_OFFSET] = if entry.media == MEDIA_CD { MEDIA_CD } else { MEDIA_DVD };

    // Magic marker
    buf[MAGIC_OFFSET] = MAGIC;

    buf
}

/// Whether an existing `ul.cfg` is already in the real 64-byte format.
/// Used by the migration step to skip drives that are already converted.
pub fn is_real_format(path: &Path) -> bool {
    let data = match fs::read(path) {
        Ok(d) => d,
        Err(_) => return false,
    };
    if data.is_empty() || data.len() % ENTRY_SIZE != 0 {
        return false;
    }
    // First record should carry the magic byte.
    data.get(MAGIC_OFFSET) == Some(&MAGIC)
}

/// Add (or update) an entry in `ul.cfg` (read-modify-write).
pub fn add_entry(path: &Path, entry: &UlEntry) -> Result<(), UlCfgError> {
    let mut entries = parse_ulcfg(path)?;

    if let Some(existing) = entries.iter_mut().find(|e| e.game_id == entry.game_id) {
        existing.title = entry.title.clone();
        existing.parts = entry.parts;
        existing.media = entry.media;
    } else {
        entries.push(entry.clone());
    }

    write_ulcfg(path, &entries)
}

/// Remove an entry from `ul.cfg` by game id.
pub fn remove_entry(path: &Path, game_id: &str) -> Result<bool, UlCfgError> {
    let mut entries = parse_ulcfg(path)?;
    let before = entries.len();
    entries.retain(|e| e.game_id != game_id);
    let removed = entries.len() < before;

    if removed {
        write_ulcfg(path, &entries)?;
    }

    Ok(removed)
}

/// Generate the `ul.cfg` file path for a given device mount point.
pub fn ulcfg_path(mount_point: &Path) -> std::path::PathBuf {
    mount_point.join("ul.cfg")
}

/// Extract a game title from an ISO filename.
/// E.g., "SLUS_200.00 - God of War.iso" -> "God of War"
pub fn extract_title(filename: &str) -> String {
    let stem = filename
        .strip_suffix(".iso")
        .or_else(|| filename.strip_suffix(".bin"))
        .unwrap_or(filename);

    // Try to split on " - " and take the title part
    if let Some(pos) = stem.find(" - ") {
        stem[pos + 3..].to_string()
    } else {
        stem.to_string()
    }
}

fn null_terminated_str(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end])
        .trim_end_matches(' ')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_title() {
        assert_eq!(extract_title("SLUS_200.00 - God of War.iso"), "God of War");
        assert_eq!(extract_title("my_game.iso"), "my_game");
        assert_eq!(extract_title("SCUS_971.24.bin"), "SCUS_971.24");
    }

    #[test]
    fn test_record_is_64_bytes_with_magic() {
        let entry = UlEntry {
            title: "God of War".into(),
            game_id: "SLUS_217.46".into(),
            parts: 3,
            media: MEDIA_DVD,
            mount_point: String::new(),
        };
        let rec = encode_entry(&entry);
        assert_eq!(rec.len(), 64);
        assert_eq!(rec[PARTS_OFFSET], 3);
        assert_eq!(rec[MEDIA_OFFSET], MEDIA_DVD);
        assert_eq!(rec[MAGIC_OFFSET], MAGIC);
        // Image field carries the "ul." prefix.
        assert_eq!(&rec[IMAGE_OFFSET..IMAGE_OFFSET + 14], b"ul.SLUS_217.46");
    }

    #[test]
    fn test_write_parse_roundtrip() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_ulcfg_real");

        let entries = vec![
            UlEntry {
                title: "God of War".into(),
                game_id: "SLUS_217.46".into(),
                parts: 2,
                media: MEDIA_DVD,
                mount_point: String::new(),
            },
            UlEntry {
                title: "Shadow of the Colossus".into(),
                game_id: "SCUS_971.24".into(),
                parts: 1,
                media: MEDIA_CD,
                mount_point: String::new(),
            },
        ];

        write_ulcfg(&path, &entries).unwrap();

        // File is a whole number of 64-byte records and is detected as real format.
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.len() % ENTRY_SIZE as u64, 0);
        assert!(is_real_format(&path));

        let parsed = parse_ulcfg(&path).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].title, "God of War");
        assert_eq!(parsed[0].game_id, "SLUS_217.46");
        assert_eq!(parsed[0].parts, 2);
        assert_eq!(parsed[0].media, MEDIA_DVD);
        assert_eq!(parsed[1].title, "Shadow of the Colossus");
        assert_eq!(parsed[1].game_id, "SCUS_971.24");
        assert_eq!(parsed[1].media, MEDIA_CD);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_add_and_remove() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_ulcfg_real_add");
        std::fs::remove_file(&path).ok();

        let entry = UlEntry {
            title: "Test Game".into(),
            game_id: "TEST_000.00".into(),
            parts: 1,
            media: MEDIA_DVD,
            mount_point: String::new(),
        };

        add_entry(&path, &entry).unwrap();
        assert_eq!(parse_ulcfg(&path).unwrap().len(), 1);

        assert!(remove_entry(&path, "TEST_000.00").unwrap());
        assert_eq!(parse_ulcfg(&path).unwrap().len(), 0);

        std::fs::remove_file(&path).ok();
    }
}
