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

/// OPL ul.cfg entry.
///
/// Format (binary):
/// - 32 bytes: game title (null-terminated)
/// - 2 bytes: parts count (little-endian u16)
/// - 32 bytes: game ID (null-terminated)
///
/// Total: 66 bytes per entry
const ENTRY_SIZE: usize = 66;
const TITLE_SIZE: usize = 32;
const ID_SIZE: usize = 32;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UlEntry {
    pub title: String,
    pub game_id: String,
    pub parts: u16,
    pub mount_point: String,
}

/// Parse an existing ul.cfg file.
pub fn parse_ulcfg(path: &Path) -> Result<Vec<UlEntry>, UlCfgError> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    let data = fs::read(path)?;
    let mut entries = Vec::new();
    let mut offset = 0;

    while offset + ENTRY_SIZE <= data.len() {
        let title_bytes = &data[offset..offset + TITLE_SIZE];
        let title = null_terminated_str(title_bytes);

        let parts = u16::from_le_bytes([data[offset + TITLE_SIZE], data[offset + TITLE_SIZE + 1]]);

        let id_bytes = &data[offset + TITLE_SIZE + 2..offset + ENTRY_SIZE];
        let game_id = null_terminated_str(id_bytes);

        if !title.is_empty() {
            entries.push(UlEntry {
                title,
                game_id,
                parts,
                mount_point: String::new(),
            });
        }

        offset += ENTRY_SIZE;
    }

    Ok(entries)
}

/// Write ul.cfg with the given entries.
pub fn write_ulcfg(path: &Path, entries: &[UlEntry]) -> Result<(), UlCfgError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;

    for entry in entries {
        let mut buf = [0u8; ENTRY_SIZE];

        // Title (32 bytes, null-terminated)
        let title_bytes = entry.title.as_bytes();
        let title_len = title_bytes.len().min(TITLE_SIZE - 1);
        buf[..title_len].copy_from_slice(&title_bytes[..title_len]);

        // Parts count (2 bytes, little-endian)
        let parts_bytes = entry.parts.to_le_bytes();
        buf[TITLE_SIZE..TITLE_SIZE + 2].copy_from_slice(&parts_bytes);

        // Game ID (32 bytes, null-terminated)
        let id_bytes = entry.game_id.as_bytes();
        let id_len = id_bytes.len().min(ID_SIZE - 1);
        buf[TITLE_SIZE + 2..TITLE_SIZE + 2 + id_len].copy_from_slice(&id_bytes[..id_len]);

        file.write_all(&buf)?;
    }

    file.flush()?;
    Ok(())
}

/// Add a new entry to ul.cfg (read-modify-write).
pub fn add_entry(path: &Path, entry: &UlEntry) -> Result<(), UlCfgError> {
    let mut entries = parse_ulcfg(path)?;

    // Update existing or add new
    if let Some(existing) = entries.iter_mut().find(|e| e.game_id == entry.game_id) {
        existing.title = entry.title.clone();
        existing.parts = entry.parts;
    } else {
        entries.push(entry.clone());
    }

    write_ulcfg(path, &entries)
}

/// Remove an entry from ul.cfg by game ID.
#[allow(dead_code)]
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

/// Generate the ul.cfg file path for a given device mount point.
pub fn ulcfg_path(mount_point: &Path) -> std::path::PathBuf {
    mount_point.join("ul.cfg")
}

/// Extract game title from ISO filename.
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
    String::from_utf8_lossy(&bytes[..end]).to_string()
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
    fn test_write_parse_roundtrip() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_ulcfg");

        let entries = vec![
            UlEntry {
                title: "God of War".into(),
                game_id: "SLUS_20000".into(),
                parts: 2,
                mount_point: "/Volumes/USB".into(),
            },
            UlEntry {
                title: "Shadow of the Colossus".into(),
                game_id: "SCUS_97124".into(),
                parts: 1,
                mount_point: "/Volumes/USB".into(),
            },
        ];

        write_ulcfg(&path, &entries).unwrap();
        let parsed = parse_ulcfg(&path).unwrap();

        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].title, "God of War");
        assert_eq!(parsed[0].game_id, "SLUS_20000");
        assert_eq!(parsed[0].parts, 2);
        assert_eq!(parsed[1].title, "Shadow of the Colossus");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_add_and_remove() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_ulcfg_add");

        let entry = UlEntry {
            title: "Test Game".into(),
            game_id: "TEST_00000".into(),
            parts: 1,
            mount_point: "/tmp".into(),
        };

        add_entry(&path, &entry).unwrap();
        let entries = parse_ulcfg(&path).unwrap();
        assert_eq!(entries.len(), 1);

        let removed = remove_entry(&path, "TEST_00000").unwrap();
        assert!(removed);
        let entries = parse_ulcfg(&path).unwrap();
        assert_eq!(entries.len(), 0);

        std::fs::remove_file(&path).ok();
    }
}
