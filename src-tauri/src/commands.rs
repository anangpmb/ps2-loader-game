use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use tauri::State;

use crate::filesystem::{self, DeviceInfo};
use crate::iso::{self, IsoInfo};
use crate::opl_crc;
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

/// Heuristic media type for the `ul.cfg` entry: CD for small (<= 700 MiB)
/// images, DVD otherwise. The exact disc type is not always recoverable from an
/// ISO, and this matches how the vast majority of PS2 titles are distributed.
///
/// ponytail: 700MB threshold — some CD games are larger, but DVD is safer default.
/// If a CD game is misdetected as DVD, OPL may still work. Wrong CD detection
/// for a DVD game causes white screen.
fn detect_media(size: u64) -> u8 {
    // Single-layer DVD capacity ~4.37 GB
    // Games <= 700MB are likely CD, everything else is DVD
    if size <= 700 * 1024 * 1024 {
        ulcfg::MEDIA_CD
    } else {
        ulcfg::MEDIA_DVD
    }
}

/// Sum the on-disk sizes of a split game's chunk files.
fn sum_chunk_sizes(dest: &std::path::Path, crc_hex: &str, game_id: &str, parts: u16) -> u64 {
    let mut total = 0u64;
    for i in 0..parts.max(1) as u32 {
        let name = split::chunk_file_name(crc_hex, game_id, i);
        if let Ok(m) = std::fs::metadata(dest.join(name)) {
            total += m.len();
        }
    }
    total
}

/// Parse a USBExtreme chunk filename `ul.<crc>.<game_id>.<part>` into its parts.
///
/// The game id itself contains a `.` (e.g. `SLUS_217.46`), so the crc is the
/// first token, the part is the last token, and everything in between is the id.
fn parse_chunk_name(name: &str) -> Option<(String, String, String)> {
    if name == "ul.cfg" {
        return None;
    }
    let rest = name.strip_prefix("ul.")?;
    let tokens: Vec<&str> = rest.split('.').collect();
    if tokens.len() < 3 {
        return None;
    }
    let crc = tokens[0].to_string();
    let part = tokens[tokens.len() - 1].to_string();
    let game_id = tokens[1..tokens.len() - 1].join(".");
    if crc.is_empty() || game_id.is_empty() {
        return None;
    }
    Some((crc, game_id, part))
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

    let file_size = std::fs::metadata(&source_path)
        .map(|m| m.len())
        .unwrap_or(0);

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

    // Display title = filename without extension (user can rename file to change title)
    let title = ulcfg::extract_title(
        source_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
            .as_str(),
    );
    // Game ID = region code from ISO header (e.g. "SLUS_217.46")
    let game_id = iso::extract_startup(&source_path).unwrap_or(game_id);
    let crc_hex = opl_crc::crc32_hex(&title);

    // Run in a blocking thread to not freeze the UI
    let source_clone = source_path.clone();
    let game_id_clone = game_id.clone();
    let crc_clone = crc_hex.clone();
    let dest_clone = dest_path.clone();

    let result = tokio::task::spawn_blocking(move || {
        if use_split {
            split::split_iso(&source_clone, &dest_clone, &crc_clone, &game_id_clone, &config, |_p| {})
        } else {
            split::copy_iso_nosplit(&source_clone, &dest_clone, &game_id_clone, &config, |_p| {})
        }
    })
    .await
    .map_err(|e| format!("Task failed: {}", e))?
    .map_err(|e| e.to_string())?;

    // Update ul.cfg only for split mode (USBExtreme format).
    // No-split mode uses CD/DVD directories — OPL scans those directly.
    if use_split {
        let ulcfg_path = ulcfg::ulcfg_path(&dest_path);
        let entry = UlEntry {
            title,
            game_id,
            parts: result.chunks.len() as u16,
            media: detect_media(file_size),
            mount_point: dest_dir,
        };
        ulcfg::add_entry(&ulcfg_path, &entry).map_err(|e| e.to_string())?;
    }

    Ok(result)
}

/// Regenerate `ul.cfg` for a device from the split chunk files on disk.
///
/// Note: OPL locates chunk files by recomputing the CRC of the `ul.cfg` name
/// field, so an entry is only valid when its name hashes back to the CRC baked
/// into the filenames. That original name cannot be recovered from a CRC, so an
/// existing `ul.cfg` is treated as the source of truth for titles; groups on
/// disk with no matching entry are added with the game id as a placeholder title
/// (their filenames keep whatever CRC they already have).
#[tauri::command]
pub fn generate_ulcfg(dest_dir: String) -> Result<usize, String> {
    let dest_path = PathBuf::from(&dest_dir);
    let ulcfg_path = ulcfg::ulcfg_path(&dest_path);

    // Existing titles keyed by game id (source of truth for names).
    let existing: BTreeMap<String, UlEntry> = ulcfg::parse_ulcfg(&ulcfg_path)
        .unwrap_or_default()
        .into_iter()
        .map(|e| (e.game_id.clone(), e))
        .collect();

    // CRC → title map from existing entries for fallback matching.
    // ponytail: handles case where ul.cfg game_id is wrong but title is correct.
    let existing_by_crc: BTreeMap<String, String> = existing
        .values()
        .map(|e| (opl_crc::crc32_hex(&e.title), e.title.clone()))
        .collect();

    // Group chunk files on disk by game id.
    // value: (crc, parts count, total size)
    let mut groups: BTreeMap<String, (String, u16, u64)> = BTreeMap::new();
    if let Ok(read_dir) = std::fs::read_dir(&dest_path) {
        for entry in read_dir.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some((crc, game_id, _part)) = parse_chunk_name(&name) {
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                let g = groups.entry(game_id).or_insert((crc, 0, 0));
                g.1 += 1;
                g.2 += size;
            }
        }
    }

    let mut entries = Vec::new();
    for (game_id, (crc, parts, total)) in groups {
        let title = if let Some(prev) = existing.get(&game_id) {
            prev.title.clone()
        } else if let Some(title) = existing_by_crc.get(&crc) {
            // Fallback: match by CRC from chunk filename when game_id in ul.cfg is wrong.
            title.clone()
        } else {
            game_id.clone()
        };

        let media = existing
            .get(&game_id)
            .map(|e| e.media)
            .unwrap_or_else(|| detect_media(total));

        entries.push(UlEntry {
            title,
            game_id,
            parts,
            media,
            mount_point: dest_dir.clone(),
        });
    }

    let count = entries.len();
    ulcfg::write_ulcfg(&ulcfg_path, &entries).map_err(|e| e.to_string())?;
    Ok(count)
}

/// Verify that each game's chunk files exist and are non-empty.
#[tauri::command]
pub fn verify_games(dest_dir: String) -> Result<VerifyResult, String> {
    let dest_path = PathBuf::from(&dest_dir);
    let ulcfg_path = ulcfg::ulcfg_path(&dest_path);

    let entries = ulcfg::parse_ulcfg(&ulcfg_path).map_err(|e| e.to_string())?;
    let mut verified = 0;
    let mut errors = 0;

    for entry in &entries {
        let crc_hex = opl_crc::crc32_hex(&entry.title);
        let mut ok = entry.parts > 0;
        for i in 0..entry.parts.max(1) as u32 {
            let name = split::chunk_file_name(&crc_hex, &entry.game_id, i);
            match std::fs::metadata(dest_path.join(name)) {
                Ok(m) if m.len() > 0 => {}
                _ => {
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            verified += 1;
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

/// Check contiguity of ul.* split files on the device.
/// Returns a list of results for each file found.
#[tauri::command]
pub fn check_contiguity(dest_dir: String) -> Result<Vec<filesystem::ContiguityResult>, String> {
    let dest_path = PathBuf::from(&dest_dir);
    filesystem::check_dir_contiguity(&dest_path, "ul.")
        .map_err(|e| e.to_string())
}

/// Open a native folder picker dialog and return the selected path.
/// Fallback for when the JS dialog API isn't working.
#[tauri::command]
pub async fn open_folder_dialog(app: tauri::AppHandle) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    
    let path = app.dialog()
        .file()
        .set_title("Select USB Drive or Folder")
        .blocking_pick_folder();
    
    match path {
        Some(p) => Ok(Some(p.to_string())),
        None => Ok(None),
    }
}

/// Format a drive to FAT32 and initialize it for OPL (create ul.cfg).
/// WARNING: This erases ALL data on the drive!
#[tauri::command]
pub fn format_drive_for_opl(mount_point: String, volume_label: String) -> Result<String, String> {
    let path = PathBuf::from(&mount_point);
    
    // Validate: must be an existing directory
    if !path.exists() || !path.is_dir() {
        return Err(format!("Invalid mount point: {}", mount_point));
    }

    // Get the device identifier for formatting
    let device_info = filesystem::get_device_info(&path).map_err(|e| e.to_string())?;
    
    // Format the drive
    filesystem::format_drive_fat32(&device_info, &volume_label)
        .map_err(|e| e.to_string())?;
    
    // Create ul.cfg (empty)
    let ulcfg_path = ulcfg::ulcfg_path(&path);
    ulcfg::write_ulcfg(&ulcfg_path, &[]).map_err(|e| e.to_string())?;
    
    Ok(format!("Drive formatted as FAT32 ({}). ul.cfg created. Ready to add games.", volume_label))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameEntry {
    pub game_id: String,
    pub title: String,
    pub parts: u16,
    pub size: u64,
    pub location: String, // "root", "CD", "DVD"
    pub mode: String,     // "split" or "nosplit"
}

/// List all games on the device (ul.cfg + ul.* files + CD/DVD ISOs).
#[tauri::command]
pub fn list_device_games(dest_dir: String) -> Result<Vec<GameEntry>, String> {
    let dest_path = PathBuf::from(&dest_dir);
    let mut games: Vec<GameEntry> = Vec::new();

    // 1. Parse ul.cfg for split-mode entries (one entry == one game, already grouped).
    let ulcfg_path = ulcfg::ulcfg_path(&dest_path);
    if let Ok(entries) = ulcfg::parse_ulcfg(&ulcfg_path) {
        for entry in entries {
            let crc_hex = opl_crc::crc32_hex(&entry.title);
            let total_size = sum_chunk_sizes(&dest_path, &crc_hex, &entry.game_id, entry.parts);
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

    // 2. Scan CD/ and DVD/ for no-split ISOs — read title from ISO header.
    for subdir in &["CD", "DVD"] {
        let dir = dest_path.join(subdir);
        if !dir.exists() {
            continue;
        }
        if let Ok(read_dir) = std::fs::read_dir(&dir) {
            for entry in read_dir.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if !name.ends_with(".iso") {
                    continue;
                }
                let game_id = name.strip_suffix(".iso").unwrap_or(&name).to_string();
                if game_id.is_empty() {
                    continue;
                }
                let path = entry.path();
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
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

    // 3. Fallback: group root ul.* chunk files not already covered by ul.cfg.
    let known_ids: std::collections::HashSet<String> =
        games.iter().map(|g| g.game_id.clone()).collect();
    // value: (parts count, total size)
    let mut orphans: BTreeMap<String, (u16, u64)> = BTreeMap::new();
    if let Ok(read_dir) = std::fs::read_dir(&dest_path) {
        for entry in read_dir.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some((_crc, game_id, _part)) = parse_chunk_name(&name) {
                if known_ids.contains(&game_id) {
                    continue;
                }
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                let g = orphans.entry(game_id).or_insert((0, 0));
                g.0 += 1;
                g.1 += size;
            }
        }
    }
    for (game_id, (parts, size)) in orphans {
        games.push(GameEntry {
            title: game_id.clone(),
            game_id,
            parts,
            size,
            location: "root".into(),
            mode: "split".into(),
        });
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
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Delete a game from the device.
#[tauri::command]
pub fn delete_game(
    dest_dir: String,
    game_id: String,
    mode: String,
    location: String,
) -> Result<(), String> {
    let dest_path = PathBuf::from(&dest_dir);

    if mode == "nosplit" {
        // Delete ISO from CD/ or DVD/
        let iso_path = dest_path.join(&location).join(format!("{}.iso", game_id));
        std::fs::remove_file(&iso_path)
            .map_err(|e| format!("Failed to delete {}: {}", iso_path.display(), e))?;
    } else {
        // Delete every chunk file `ul.<crc>.<game_id>.<part>` for this game id
        // (crc-agnostic — we match by the game id embedded in the filename).
        if let Ok(read_dir) = std::fs::read_dir(&dest_path) {
            for entry in read_dir.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if let Some((_crc, id, _part)) = parse_chunk_name(&name) {
                    if id == game_id {
                        std::fs::remove_file(entry.path())
                            .map_err(|e| format!("Failed to delete {}: {}", name, e))?;
                    }
                }
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

/// Rename a game title — updates ul.cfg AND renames chunk files so the CRC stays consistent.
#[tauri::command]
pub fn rename_game(
    dest_dir: String,
    game_id: String,
    _mode: String,
    _location: String,
    new_title: String,
) -> Result<(), String> {
    let dest_path = PathBuf::from(&dest_dir);
    let ulcfg_path = ulcfg::ulcfg_path(&dest_path);
    if !ulcfg_path.exists() {
        return Err("ul.cfg not found".into());
    }
    let mut entries = ulcfg::parse_ulcfg(&ulcfg_path).map_err(|e| e.to_string())?;
    let mut found = false;
    let mut old_title = String::new();
    for entry in &mut entries {
        if entry.game_id == game_id {
            old_title = entry.title.clone();
            entry.title = new_title.clone();
            found = true;
        }
    }
    if !found {
        return Err(format!("Game {} not found in ul.cfg", game_id));
    }

    // Rename chunk files: ul.<oldCRC>.<gameId>.<part> → ul.<newCRC>.<gameId>.<part>
    let old_crc = opl_crc::crc32_hex(&old_title);
    let new_crc = opl_crc::crc32_hex(&new_title);
    if old_crc != new_crc {
        if let Ok(read_dir) = std::fs::read_dir(&dest_path) {
            for entry in read_dir.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if let Some((crc, id, part)) = parse_chunk_name(&name) {
                    if id == game_id && crc == old_crc {
                        let new_name = split::chunk_file_name(&new_crc, &game_id, part.parse::<u32>().unwrap_or(0));
                        let old_path = entry.path();
                        let new_path = dest_path.join(&new_name);
                        if !new_path.exists() {
                            std::fs::rename(&old_path, &new_path)
                                .map_err(|e| format!("Failed to rename {}: {}", name, e))?;
                        }
                    }
                }
            }
        }
    }

    ulcfg::write_ulcfg(&ulcfg_path, &entries).map_err(|e| e.to_string())?;
    Ok(())
}

/// Migrate games written by this app's old (non-OPL) format into the real
/// USBExtreme format, so they display correctly here and boot on a real PS2.
///
/// Old format on disk:
/// - chunk files `ul.<game_id>` (part 0), `ul.<game_id>.01`, `ul.<game_id>.02`, ...
/// - a 66-byte `ul.cfg` record: title[0..32], parts(u16)[32..34], id[34..66]
///
/// The old `ul.cfg` preserved the real title, so migration is lossless: we read
/// each old entry, compute the correct CRC from the title, rename the chunks to
/// `ul.<crc>.<game_id>.<part>`, and rewrite `ul.cfg` in the real 64-byte format.
/// Returns the number of games migrated.
#[tauri::command]
pub fn repair_split_files(dest_dir: String) -> Result<u32, String> {
    let dest_path = PathBuf::from(&dest_dir);
    let ulcfg_path = ulcfg::ulcfg_path(&dest_path);
    if !ulcfg_path.exists() {
        return Ok(0);
    }

    // Already converted → nothing to do.
    if ulcfg::is_real_format(&ulcfg_path) {
        return Ok(0);
    }

    let old_entries = parse_old_ulcfg(&ulcfg_path).map_err(|e| e.to_string())?;
    if old_entries.is_empty() {
        return Ok(0);
    }

    // Safety guard: only migrate if there is real evidence of the OLD naming on
    // disk (a part-0 file `ul.<game_id>`). Without this, a real (USBUtil) ul.cfg
    // that happens to lack the magic byte could be misparsed and clobbered.
    let has_old_evidence = old_entries
        .iter()
        .any(|e| dest_path.join(format!("ul.{}", e.game_id)).exists());
    if !has_old_evidence {
        return Ok(0);
    }

    let mut new_entries: Vec<UlEntry> = Vec::new();
    let mut migrated = 0u32;

    for old in &old_entries {
        let crc_hex = opl_crc::crc32_hex(&old.title);
        let parts = old.parts.max(1);
        let mut total_size = 0u64;
        let mut renamed_any = false;

        for i in 0..parts as u32 {
            // Old naming: part 0 is `ul.<id>`, later parts are `ul.<id>.NN` (decimal).
            let old_name = if i == 0 {
                format!("ul.{}", old.game_id)
            } else {
                format!("ul.{}.{:02}", old.game_id, i)
            };
            let old_path = dest_path.join(&old_name);
            let new_name = split::chunk_file_name(&crc_hex, &old.game_id, i);
            let new_path = dest_path.join(&new_name);

            if old_path.exists() && old_path != new_path {
                if !new_path.exists() {
                    std::fs::rename(&old_path, &new_path)
                        .map_err(|e| format!("Failed to rename {}: {}", old_name, e))?;
                    renamed_any = true;
                }
            }
            if let Ok(m) = std::fs::metadata(&new_path) {
                total_size += m.len();
            }
        }

        if renamed_any {
            migrated += 1;
        }

        new_entries.push(UlEntry {
            title: old.title.clone(),
            game_id: old.game_id.clone(),
            parts,
            media: detect_media(total_size),
            mount_point: dest_dir.clone(),
        });
    }

    // Rewrite ul.cfg in the real 64-byte format.
    ulcfg::write_ulcfg(&ulcfg_path, &new_entries).map_err(|e| e.to_string())?;

    Ok(migrated)
}

/// Minimal reader for the app's OLD 66-byte `ul.cfg` format (migration only).
struct OldEntry {
    title: String,
    game_id: String,
    parts: u16,
}

fn parse_old_ulcfg(path: &std::path::Path) -> std::io::Result<Vec<OldEntry>> {
    const OLD_ENTRY_SIZE: usize = 66;
    const TITLE: usize = 32;

    let data = std::fs::read(path)?;
    let mut entries = Vec::new();
    let mut offset = 0;

    while offset + OLD_ENTRY_SIZE <= data.len() {
        let title = null_str(&data[offset..offset + TITLE]);
        let parts = u16::from_le_bytes([data[offset + TITLE], data[offset + TITLE + 1]]);
        let game_id = null_str(&data[offset + TITLE + 2..offset + OLD_ENTRY_SIZE]);

        if !title.is_empty() {
            entries.push(OldEntry {
                title,
                game_id,
                // Guard against garbage from a mis-detected format.
                parts: parts.clamp(1, 255),
            });
        }
        offset += OLD_ENTRY_SIZE;
    }

    Ok(entries)
}

fn null_str(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).to_string()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_chunk_name() {
        assert_eq!(
            parse_chunk_name("ul.FBDF6400.SLUS_217.46.00"),
            Some(("FBDF6400".into(), "SLUS_217.46".into(), "00".into()))
        );
        assert_eq!(
            parse_chunk_name("ul.0A1B2C3D.SCUS_971.24.02"),
            Some(("0A1B2C3D".into(), "SCUS_971.24".into(), "02".into()))
        );
        assert_eq!(parse_chunk_name("ul.cfg"), None);
        assert_eq!(parse_chunk_name("random.iso"), None);
        // Too few tokens (old-format leftover) is not a valid real chunk name.
        assert_eq!(parse_chunk_name("ul.SLUS_21746"), None);
    }

    #[test]
    fn test_detect_media() {
        assert_eq!(detect_media(600 * 1024 * 1024), ulcfg::MEDIA_CD);
        assert_eq!(detect_media(3 * 1024 * 1024 * 1024), ulcfg::MEDIA_DVD);
    }
}
