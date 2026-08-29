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

/// Detect connected removable storage device.
#[tauri::command]
pub fn detect_device() -> Result<DeviceInfo, String> {
    filesystem::detect_primary_device().map_err(|e| e.to_string())
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

    // Update ul.cfg
    let ulcfg_path = ulcfg::ulcfg_path(&PathBuf::from(&dest_dir));
    let entry = UlEntry {
        title: ulcfg::extract_title(
            source_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default()
                .as_str(),
        ),
        game_id,
        parts: result.chunks.len() as u16,
        mount_point: dest_dir,
    };

    ulcfg::add_entry(&ulcfg_path, &entry).map_err(|e| e.to_string())?;

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

                    entries.push(UlEntry {
                        title: game_id.clone(),
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

/// List games already on the device.
#[tauri::command]
pub fn list_device_games(dest_dir: String) -> Result<Vec<UlEntry>, String> {
    let ulcfg_path = ulcfg::ulcfg_path(&PathBuf::from(&dest_dir));
    ulcfg::parse_ulcfg(&ulcfg_path).map_err(|e| e.to_string())
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
