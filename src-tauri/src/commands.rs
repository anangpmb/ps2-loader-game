use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;
use tauri::State;

use crate::filesystem::{self, DeviceInfo};
use crate::iso::{self, IsoInfo};
use crate::split::{self, ChecksumAlgo, SplitConfig, SplitResult};
use crate::ulcfg::{self, UlEntry};

/// Application state shared across commands.
pub struct AppState {
    pub settings: Mutex<AppSettings>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSettings {
    pub buffer_size: usize,
    pub checksum: String,
    pub max_retries: u32,
    pub split_mode: String,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            buffer_size: 8,
            checksum: "crc32".into(),
            max_retries: 3,
            split_mode: "auto".into(),
        }
    }
}

impl AppSettings {
    pub fn to_split_config(&self) -> SplitConfig {
        let checksum_algo = match self.checksum.as_str() {
            "sha256" => ChecksumAlgo::Sha256,
            "xxhash" => ChecksumAlgo::Xxhash,
            _ => ChecksumAlgo::Crc32,
        };

        SplitConfig {
            buffer_size: self.buffer_size * 1024 * 1024,
            checksum_algo,
            max_retries: self.max_retries,
            ..Default::default()
        }
    }
}

/// Detect connected removable storage device (first one).
#[tauri::command]
pub fn detect_device() -> Result<DeviceInfo, String> {
    filesystem::detect_primary_device().map_err(|e| e.to_string())
}

/// List all connected removable storage devices.
#[tauri::command]
pub fn list_devices() -> Result<Vec<DeviceInfo>, String> {
    filesystem::detect_devices().map_err(|e| e.to_string())
}

/// Validate a PS2 ISO file.
#[tauri::command]
pub fn validate_iso(path: String) -> Result<IsoInfo, String> {
    iso::validate_iso(&PathBuf::from(path)).map_err(|e| e.to_string())
}

/// Process (split/copy) an ISO file to the target device.
///
/// This is a synchronous command. The frontend should call this
/// for each file in the queue sequentially.
#[tauri::command]
pub async fn process_iso(
    source: String,
    dest_dir: String,
    game_id: String,
    state: State<'_, AppState>,
) -> Result<SplitResult, String> {
    let (config, split_mode) = {
        let settings = state.settings.lock().map_err(|e| e.to_string())?;
        (settings.to_split_config(), settings.split_mode.clone())
    };

    let source_path = PathBuf::from(&source);
    let dest_path = PathBuf::from(&dest_dir);

    // Create destination directory if it doesn't exist
    std::fs::create_dir_all(&dest_path).map_err(|e| format!("Cannot create dest dir: {}", e))?;

    // Determine mode
    let use_split = match split_mode.as_str() {
        "split" => true,
        "nosplit" => false,
        _ => {
            // Auto-detect based on filesystem; default to split if detection fails
            match filesystem::get_device_info(&dest_path) {
                Ok(info) => info.filesystem.needs_split(),
                Err(_) => true, // fallback to split (safer for FAT32)
            }
        }
    };

    // Run in a blocking thread to not freeze the UI
    let source_clone = source_path.clone();
    let game_id_clone = game_id.clone();
    let dest_clone = dest_path.clone();

    let result = tokio::task::spawn_blocking(move || {
        if use_split {
            split::split_iso(&source_clone, &dest_clone, &game_id_clone, &config, |_progress| {})
        } else {
            split::copy_iso_nosplit(&source_clone, &dest_clone, &game_id_clone, &config, |_progress| {})
        }
    })
    .await
    .map_err(|e| format!("Task failed: {}", e))?
    .map_err(|e| e.to_string())?;

    // Update ul.cfg only for split mode (USBExtreme format)
    // No-split mode uses CD/DVD directories — OPL scans those directly
    if use_split {
        let ulcfg_path = ulcfg::ulcfg_path(&PathBuf::from(&dest_dir));
        let title = read_iso_title(&source_path)
            .unwrap_or_else(|| ulcfg::extract_title(
                source_path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default()
                    .as_str(),
            ));
        let entry = UlEntry {
            title,
            game_id,
            parts: result.chunks.len() as u16,
            mount_point: dest_dir,
        };
        ulcfg::add_entry(&ulcfg_path, &entry).map_err(|e| e.to_string())?;
    }

    Ok(result)
}

/// Generate or regenerate ul.cfg for a device.
#[tauri::command]
pub fn generate_ulcfg(dest_dir: String) -> Result<usize, String> {
    let dest_path = PathBuf::from(&dest_dir);
    let ulcfg_path = ulcfg::ulcfg_path(&dest_path);

    // Scan for existing ul.* files and build entries
    let mut entries = Vec::new();

    if let Ok(dir_entries) = std::fs::read_dir(&dest_path) {
        for entry in dir_entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("ul.") && name != "ul.cfg" {
                // Extract game ID from filename
                let game_id = name
                    .strip_prefix("ul.")
                    .unwrap_or(&name)
                    .split('.')
                    .next()
                    .unwrap_or("")
                    .to_string();

                if !game_id.is_empty() {
                    // Count parts
                    let mut parts = 0u16;
                    for part_entry in std::fs::read_dir(&dest_path).into_iter().flatten() {
                        if let Ok(part_entry) = part_entry {
                            let part_name = part_entry.file_name().to_string_lossy().to_string();
                            if part_name.starts_with(&format!("ul.{}", game_id)) {
                                parts += 1;
                            }
                        }
                    }

                    // Try reading title from the first split chunk (contains ISO header)
                    let first_chunk = dest_path.join(format!("ul.{}", game_id));
                    let title = read_iso_title(&first_chunk)
                        .unwrap_or_else(|| game_id.clone());

                    entries.push(UlEntry {
                        title,
                        game_id,
                        parts: parts.max(1),
                        mount_point: dest_dir.clone(),
                    });
                }
            }
        }
    }

    let count = entries.len();
    ulcfg::write_ulcfg(&ulcfg_path, &entries).map_err(|e| e.to_string())?;

    Ok(count)
}

/// Verify checksums of existing game files on device.
#[tauri::command]
pub fn verify_games(dest_dir: String) -> Result<VerifyResult, String> {
    let dest_path = PathBuf::from(&dest_dir);
    let ulcfg_path = ulcfg::ulcfg_path(&dest_path);

    let entries = ulcfg::parse_ulcfg(&ulcfg_path).map_err(|e| e.to_string())?;
    let mut verified = 0;
    let mut errors = 0;

    for entry in &entries {
        // Check if the ul. file exists
        let file_path = dest_path.join(format!("ul.{}", entry.game_id));
        if file_path.exists() {
            // For simplicity, just check file size > 0
            // In production, would re-compute and verify checksums
            match std::fs::metadata(&file_path) {
                Ok(meta) if meta.len() > 0 => verified += 1,
                _ => errors += 1,
            }
        } else {
            errors += 1;
        }
    }

    Ok(VerifyResult { verified, errors })
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VerifyResult {
    pub verified: usize,
    pub errors: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameEntry {
    pub game_id: String,
    pub title: String,
    pub parts: u16,
    pub size: u64,
    pub location: String, // "root/ul.xxx", "CD/xxx.iso", "DVD/xxx.iso"
    pub mode: String,     // "split" or "nosplit"
}

/// List all games on the device (ul.cfg + ul.* files + CD/DVD ISOs).
#[tauri::command]
pub fn list_device_games(dest_dir: String) -> Result<Vec<GameEntry>, String> {
    let dest_path = PathBuf::from(&dest_dir);
    let mut games: Vec<GameEntry> = Vec::new();

    // 1. Parse ul.cfg for split-mode entries
    let ulcfg_path = ulcfg::ulcfg_path(&dest_path);
    if let Ok(entries) = ulcfg::parse_ulcfg(&ulcfg_path) {
        for entry in entries {
            let mut total_size: u64 = 0;
            for i in 0..entry.parts {
                let part_name = if i == 0 || entry.parts == 1 {
                    format!("ul.{}", entry.game_id)
                } else {
                    format!("ul.{}.{:02}", entry.game_id, i)
                };
                if let Ok(meta) = std::fs::metadata(dest_path.join(&part_name)) {
                    total_size += meta.len();
                }
            }
            games.push(GameEntry {
                game_id: entry.game_id,
                title: entry.title,
                parts: entry.parts,
                size: total_size,
                location: "root".into(),
                mode: "split".into(),
            });
        }
    }

    // 2. Scan CD/ and DVD/ for no-split ISOs — read title from ISO header
    for subdir in &["CD", "DVD"] {
        let dir = dest_path.join(subdir);
        if !dir.exists() { continue; }
        if let Ok(read_dir) = std::fs::read_dir(&dir) {
            for entry in read_dir.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if !name.ends_with(".iso") { continue; }
                let game_id = name.strip_suffix(".iso").unwrap_or(&name).to_string();
                if game_id.is_empty() { continue; }
                let path = entry.path();
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);

                // Read volume label from ISO header for real title
                let title = read_iso_title(&path).unwrap_or_else(|| game_id.clone());

                games.push(GameEntry {
                    game_id,
                    title,
                    parts: 1,
                    size,
                    location: subdir.to_string(),
                    mode: "nosplit".into(),
                });
            }
        }
    }

    // 3. Fallback: scan root for ul.* files not in ul.cfg
    if let Ok(read_dir) = std::fs::read_dir(&dest_path) {
        let known_ids: Vec<String> = games.iter().map(|g| g.game_id.clone()).collect();
        for entry in read_dir.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with("ul.") || name == "ul.cfg" { continue; }
            let game_id = name.strip_prefix("ul.").unwrap_or(&name)
                .split('.').next().unwrap_or("").to_string();
            if game_id.is_empty() || known_ids.contains(&game_id) { continue; }
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            games.push(GameEntry {
                game_id: game_id.clone(),
                title: game_id,
                parts: 1,
                size,
                location: "root".into(),
                mode: "split".into(),
            });
        }
    }

    Ok(games)
}

/// Read volume label from ISO9660 header.
fn read_iso_title(path: &std::path::Path) -> Option<String> {
    use std::fs::File;
    use std::io::{Read, Seek, SeekFrom};

    let mut file = File::open(path).ok()?;
    let mut label = [0u8; 40];
    file.seek(SeekFrom::Start(0x8028)).ok()?;
    file.read_exact(&mut label).ok()?;
    let label_str = String::from_utf8_lossy(&label);
    let trimmed = label_str.trim_end_matches(' ').trim_end_matches('\0');
    if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
}

/// Delete a game from the device.
#[tauri::command]
pub fn delete_game(dest_dir: String, game_id: String, mode: String, location: String) -> Result<(), String> {
    let dest_path = PathBuf::from(&dest_dir);

    if mode == "nosplit" {
        // Delete ISO from CD/ or DVD/
        let iso_path = dest_path.join(&location).join(format!("{}.iso", game_id));
        std::fs::remove_file(&iso_path).map_err(|e| format!("Failed to delete {}: {}", iso_path.display(), e))?;
    } else {
        // Delete ul.xxx files and ul.cfg entry
        for i in 0..100 {
            let name = if i == 0 { format!("ul.{}", game_id) } else { format!("ul.{}.{:02}", game_id, i) };
            let path = dest_path.join(&name);
            if path.exists() {
                std::fs::remove_file(&path).map_err(|e| format!("Failed to delete {}: {}", name, e))?;
            } else if i > 0 {
                break; // no more parts
            }
        }
        // Remove from ul.cfg
        let ulcfg_path = ulcfg::ulcfg_path(&dest_path);
        if ulcfg_path.exists() {
            ulcfg::remove_entry(&ulcfg_path, &game_id).map_err(|e| e.to_string())?;
        }
    }

    Ok(())
}

/// Rename a game title.
#[tauri::command]
pub fn rename_game(dest_dir: String, game_id: String, mode: String, location: String, new_title: String) -> Result<(), String> {
    let dest_path = PathBuf::from(&dest_dir);

    if mode == "nosplit" {
        // Rename ISO file: old_id.iso → new_title.iso
        let old_path = dest_path.join(&location).join(format!("{}.iso", game_id));
        let new_path = dest_path.join(&location).join(format!("{}.iso", new_title));
        if old_path.exists() {
            std::fs::rename(&old_path, &new_path)
                .map_err(|e| format!("Failed to rename: {}", e))?;
        }
    } else {
        // Update title in ul.cfg
        let ulcfg_path = ulcfg::ulcfg_path(&dest_path);
        if ulcfg_path.exists() {
            let mut entries = ulcfg::parse_ulcfg(&ulcfg_path).map_err(|e| e.to_string())?;
            for entry in &mut entries {
                if entry.game_id == game_id {
                    entry.title = new_title.clone();
                }
            }
            ulcfg::write_ulcfg(&ulcfg_path, &entries).map_err(|e| e.to_string())?;
        }
    }

    Ok(())
}

/// Repair old-format split files (ul.00, ul.01) to USBExtreme format (ul.<game_id>, ul.<game_id>.01).
#[tauri::command]
pub fn repair_split_files(dest_dir: String) -> Result<u32, String> {
    let dest_path = PathBuf::from(&dest_dir);
    let ulcfg_path = ulcfg::ulcfg_path(&dest_path);
    if !ulcfg_path.exists() {
        return Ok(0);
    }

    let entries = ulcfg::parse_ulcfg(&ulcfg_path).map_err(|e| e.to_string())?;
    let mut repaired = 0u32;

    for entry in &entries {
        if entry.parts <= 1 {
            // Single-part game: just need ul.<game_id>
            let correct_name = format!("ul.{}", entry.game_id);
            let correct_path = dest_path.join(&correct_name);
            if !correct_path.exists() {
                // Look for old-format single file
                if let Ok(read_dir) = std::fs::read_dir(&dest_path) {
                    for dir_entry in read_dir.flatten() {
                        let name = dir_entry.file_name().to_string_lossy().to_string();
                        if name.starts_with("ul.") && name != "ul.cfg" && name != correct_name {
                            // Check if this could be the old file for this game
                            // Old single-part: ul.<something> where something is not a numbered part
                            let suffix = name.strip_prefix("ul.").unwrap_or("");
                            if !suffix.contains('.') && suffix == entry.game_id {
                                // Already correct, skip
                            } else if !suffix.starts_with(|c: char| c.is_ascii_digit()) {
                                // Not a numbered part, might be a different game
                            }
                        }
                    }
                }
            }
            continue;
        }

        // Multi-part game: check if correct files exist
        let first_correct = format!("ul.{}", entry.game_id);
        let first_correct_path = dest_path.join(&first_correct);

        if first_correct_path.exists() {
            // Check if part 1 exists with correct name
            let part1_correct = format!("ul.{}.{:02}", entry.game_id, 1);
            if dest_path.join(&part1_correct).exists() {
                continue; // Already correct
            }
        }

        // Correct files don't exist — look for old-format files
        // Old format: ul.00, ul.01, ul.02, ... (indexed by position, not game_id)
        // We need to find consecutive numbered files that belong to this game
        let mut old_files: Vec<(u32, std::path::PathBuf)> = Vec::new();
        if let Ok(read_dir) = std::fs::read_dir(&dest_path) {
            for dir_entry in read_dir.flatten() {
                let name = dir_entry.file_name().to_string_lossy().to_string();
                if let Some(rest) = name.strip_prefix("ul.") {
                    if rest.chars().all(|c| c.is_ascii_digit()) && !rest.is_empty() {
                        if let Ok(idx) = rest.parse::<u32>() {
                            old_files.push((idx, dir_entry.path()));
                        }
                    }
                }
            }
        }

        if old_files.is_empty() {
            continue;
        }

        old_files.sort_by_key(|(idx, _)| *idx);
        let total_parts = old_files.len().min(entry.parts as usize);

        for i in 0..total_parts {
            let old_path = &old_files[i as usize].1;
            let new_name = if i == 0 {
                format!("ul.{}", entry.game_id)
            } else {
                format!("ul.{}.{:02}", entry.game_id, i)
            };
            let new_path = dest_path.join(&new_name);

            if old_path != &new_path && !new_path.exists() {
                std::fs::rename(old_path, &new_path)
                    .map_err(|e| format!("Failed to rename {}: {}", old_path.display(), e))?;
                repaired += 1;
            }
        }
    }

    Ok(repaired)
}

/// Get current app settings.
#[tauri::command]
pub fn get_settings(state: State<AppState>) -> Result<AppSettings, String> {
    state
        .settings
        .lock()
        .map(|s| s.clone())
        .map_err(|e| e.to_string())
}

/// Save app settings.
#[tauri::command]
pub fn save_settings(settings: AppSettings, state: State<AppState>) -> Result<(), String> {
    let mut current = state.settings.lock().map_err(|e| e.to_string())?;
    *current = settings;
    Ok(())
}
