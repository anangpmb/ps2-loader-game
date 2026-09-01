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

/// Size-only media type fallback used when the source ISO is not available
/// (migration, ul.cfg recovery). Prefer `iso::detect_media_type` when the ISO
/// path is known — it reads the UDF marker and is accurate for small DVD games.
fn detect_media(size: u64) -> u8 {
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

/// Get device info (filesystem type, free/total space) for a manually selected path.
#[tauri::command]
pub fn get_device_info_for_path(path: String) -> Result<DeviceInfo, String> {
    filesystem::get_device_info(std::path::Path::new(&path)).map_err(|e| e.to_string())
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

    let _file_size = std::fs::metadata(&source_path)
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

    // Game ID = region code from ISO header (e.g. "SLUS_217.46"), with the passed id as fallback.
    let game_id = iso::extract_startup(&source_path).unwrap_or(game_id);

    // Display title — preference order:
    //   1. Filename stem after " - " separator (e.g. "SLUS_200.00 - God of War.iso" → "God of War")
    //   2. ISO9660 volume label (e.g. "GOD OF WAR") if filename has no ` - ` separator or looks
    //      like a raw game ID
    //   3. Filename stem as-is (last resort — may be a game ID like "SLUS_200.00")
    let filename_title = ulcfg::extract_title(
        source_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
            .as_str(),
    );
    // A title "looks like a game ID" when it matches the SLUS_XXX.XX / SCES_XXX.XX pattern.
    let looks_like_game_id = |s: &str| -> bool {
        let bytes = s.as_bytes();
        if bytes.len() < 9 { return false; }
        bytes[..4].iter().all(|b| b.is_ascii_uppercase())
            && bytes[4] == b'_'
            && bytes[5..8].iter().all(|b| b.is_ascii_digit())
            && bytes[8] == b'.'
            && bytes[9..].iter().all(|b| b.is_ascii_digit())
    };
    let title = if looks_like_game_id(&filename_title) {
        // Filename is a raw game ID — prefer the ISO volume label (more readable).
        iso::extract_volume_label_from_path(&source_path)
            .filter(|lbl| !lbl.is_empty() && !looks_like_game_id(lbl))
            .unwrap_or(filename_title)
    } else {
        filename_title
    };
    // Truncate to 32 chars (ul.cfg NAME field limit) BEFORE computing the CRC.
    // encode_entry writes at most 32 bytes; if title is longer OPL reads only 32
    // bytes from ul.cfg, recomputes a different CRC, and can't find the chunks.
    let title = if title.len() > 32 {
        title.chars().take(32).collect::<String>()
    } else {
        title
    };
    let crc_hex = opl_crc::crc32_hex(&title);
    // Detect media type once here (UDF VRS check) — used by both split and nosplit paths.
    let media = iso::detect_media_type(&source_path);

    // Run in a blocking thread to not freeze the UI
    let source_clone = source_path.clone();
    let game_id_clone = game_id.clone();
    let crc_clone = crc_hex.clone();
    let dest_clone = dest_path.clone();

    let result = tokio::task::spawn_blocking(move || {
        if use_split {
            split::split_iso(&source_clone, &dest_clone, &crc_clone, &game_id_clone, &config, |_p| {})
        } else {
            split::copy_iso_nosplit(&source_clone, &dest_clone, &game_id_clone, media, &config, |_p| {})
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
            title: title.clone(),
            game_id: game_id.clone(),
            parts: result.chunks.len() as u16,
            media,
            mount_point: dest_dir,
        };
        ulcfg::add_entry(&ulcfg_path, &entry).map_err(|e| e.to_string())?;
    }

    // ── Post-copy health checks ──
    // These do not abort the copy (which already succeeded) but surface issues
    // that could prevent the game from booting on real PS2 hardware via OPL.
    let mut warnings: Vec<String> = Vec::new();

    // Game ID > 12 chars: ul.cfg image field is 15 bytes ("ul." prefix uses 3).
    if game_id.len() > 12 {
        warnings.push(format!(
            "Game ID \"{}\" is {} chars; only 12 fit in ul.cfg's image field. \
             OPL may not locate chunk files.",
            game_id,
            game_id.len()
        ));
    }

    // Media type must be 0x12 (CD) or 0x14 (DVD).
    if media != ulcfg::MEDIA_CD && media != ulcfg::MEDIA_DVD {
        warnings.push(format!(
            "Unrecognised media type 0x{:02X} (expected 0x12=CD or 0x14=DVD). \
             Game may boot with wrong sector addressing.",
            media
        ));
    }

    // Chunk-level: failed verification = data may be corrupted.
    for chunk in &result.chunks {
        if !chunk.verified {
            warnings.push(format!(
                "Chunk {:02} failed checksum verification — copy may be corrupt. \
                 Delete and re-copy the game.",
                chunk.index
            ));
        }
        if chunk.size == 0 {
            warnings.push(format!(
                "Chunk {:02} has zero size — source ISO may be truncated.",
                chunk.index
            ));
        }
    }

    // Last chunk < 512 bytes is a strong signal the ISO was cut short.
    if let Some(last) = result.chunks.last() {
        if last.size > 0 && last.size < 512 {
            warnings.push(format!(
                "Last chunk is only {} bytes — ISO appears truncated. \
                 Obtain a complete ISO and re-copy.",
                last.size
            ));
        }
    }

    // Non-split: verify the ISO landed in the right directory.
    if !use_split {
        let expected_subdir = if media == ulcfg::MEDIA_CD { "CD" } else { "DVD" };
        let iso_path = dest_path.join(expected_subdir).join(format!("{}.iso", game_id));
        if !iso_path.exists() {
            warnings.push(format!(
                "Expected ISO at {}/{}.iso but the file was not found. \
                 OPL will not see this game.",
                expected_subdir, game_id
            ));
        }
    }

    let mut result = result;
    result.warnings = warnings;

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

#[derive(Debug, Serialize, Deserialize)]
pub struct DefragResult {
    pub defragged: u32,
    pub skipped: u32,
    pub bytes_moved: u64,
    pub errors: Vec<String>,
}

/// Attempt to reduce fragmentation of split game files (`ul.*`) on the device.
///
/// **Limitation**: this is NOT true defragmentation. True defrag requires
/// kernel-level `FSCTL_MOVE_FILE` (Windows) or equivalent, which demands
/// administrator privileges and a volume handle. This command uses a
/// copy-delete-copy approach instead: it only produces contiguous files when
/// the free space that opens up after deletion is itself contiguous. If the
/// drive's free space is fragmented the rewritten file may still be fragmented.
///
/// For a guaranteed result: copy all games off the drive, use "Format for OPL"
/// to start fresh, then re-add the ISOs (fresh allocation is always contiguous).
///
/// Process per fragmented file:
///   1. Copy to a temp file in the OS temp directory.
///   2. Delete the original (releases the scattered FAT32 clusters).
///   3. Copy the temp file back to the original path (new cluster allocation).
///   4. Delete the temp file.
///
/// If step 3 fails the temp copy is left in place to avoid data loss;
/// its path is reported in `errors` so the user can recover manually.
/// Peak extra disk usage = size of the largest single chunk (≤ 1 GiB).
#[tauri::command]
pub async fn defrag_split_files(dest_dir: String) -> Result<DefragResult, String> {
    let dest_path = PathBuf::from(&dest_dir);
    tokio::task::spawn_blocking(move || defrag_impl(&dest_path))
        .await
        .map_err(|e| format!("Task join failed: {}", e))?
}

fn defrag_impl(dest_path: &std::path::Path) -> Result<DefragResult, String> {
    let mut defragged = 0u32;
    let mut skipped = 0u32;
    let mut bytes_moved = 0u64;
    let mut errors: Vec<String> = Vec::new();
    let temp_dir = std::env::temp_dir();

    // Collect fragmented ul.* chunk files (not ul.cfg).
    let mut fragmented: Vec<(std::path::PathBuf, u64)> = Vec::new();
    let rd = std::fs::read_dir(dest_path).map_err(|e| e.to_string())?;
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("ul.") || name == "ul.cfg" {
            continue;
        }
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        match filesystem::check_file_contiguity(&path) {
            Ok(c) if !c.contiguous => fragmented.push((path, c.size)),
            _ => {}
        }
    }

    for (path, size) in fragmented {
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        let temp_path = temp_dir.join(format!("ps2bt_{}.tmp", name));

        // 1. Copy to temp.
        if let Err(e) = std::fs::copy(&path, &temp_path) {
            errors.push(format!("{}: copy to temp failed: {}", name, e));
            skipped += 1;
            continue;
        }

        // Sanity-check the temp copy before touching the original.
        let temp_size = std::fs::metadata(&temp_path).map(|m| m.len()).unwrap_or(0);
        if temp_size != size {
            let _ = std::fs::remove_file(&temp_path);
            errors.push(format!("{}: temp copy size mismatch ({} vs {})", name, temp_size, size));
            skipped += 1;
            continue;
        }

        // 2. Delete original.
        if let Err(e) = std::fs::remove_file(&path) {
            let _ = std::fs::remove_file(&temp_path);
            errors.push(format!("{}: delete original failed: {}", name, e));
            skipped += 1;
            continue;
        }

        // 3. Write back with fresh cluster allocation.
        if let Err(e) = std::fs::copy(&temp_path, &path) {
            // Original is gone — leave temp in place so user can recover.
            errors.push(format!(
                "{}: write-back failed: {} — recovery copy at {}",
                name, e, temp_path.display()
            ));
            skipped += 1;
            continue;
        }

        // 4. Clean up temp.
        let _ = std::fs::remove_file(&temp_path);
        defragged += 1;
        bytes_moved += size;
    }

    Ok(DefragResult { defragged, skipped, bytes_moved, errors })
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
    match ulcfg::parse_ulcfg(&ulcfg_path) {
        Ok(entries) => {
            for entry in entries {
                let crc_hex = opl_crc::crc32_hex(&entry.title);
                let total_size =
                    sum_chunk_sizes(&dest_path, &crc_hex, &entry.game_id, entry.parts);
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
        Err(e) => {
            // Log the error so it's visible in the Tauri dev console / stderr.
            // Do not add a fake game entry — an empty game_id breaks the UI render.
            // The orphan scan below will still find chunk files and list them by game_id.
            eprintln!("[list_device_games] ul.cfg read failed: {}", e);
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
    mode: String,
    _location: String,
    new_title: String,
) -> Result<(), String> {
    if mode != "split" {
        return Err("Rename is only supported for split-mode games (ul.cfg)".into());
    }

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
        // Collect all rename pairs before touching the filesystem.
        let mut pairs: Vec<(PathBuf, PathBuf)> = Vec::new();
        if let Ok(read_dir) = std::fs::read_dir(&dest_path) {
            for entry in read_dir.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if let Some((crc, id, part)) = parse_chunk_name(&name) {
                    if id == game_id && crc == old_crc {
                        // Part index is stored as uppercase hex ("00", "0A", …) — parse as hex.
                        let part_idx = u32::from_str_radix(&part, 16).unwrap_or(0);
                        let new_name = split::chunk_file_name(&new_crc, &game_id, part_idx);
                        let old_path = entry.path();
                        let new_path = dest_path.join(&new_name);
                        if !new_path.exists() {
                            pairs.push((old_path, new_path));
                        }
                    }
                }
            }
        }

        // Execute all renames; roll back on failure to leave the game bootable.
        let mut done: Vec<(PathBuf, PathBuf)> = Vec::new();
        for (old_path, new_path) in &pairs {
            if let Err(e) = std::fs::rename(old_path, new_path) {
                // Undo already-renamed files before returning the error.
                for (was_old, was_new) in &done {
                    let _ = std::fs::rename(was_new, was_old);
                }
                return Err(format!(
                    "Failed to rename {}: {}. All changes rolled back.",
                    old_path.file_name().unwrap_or_default().to_string_lossy(),
                    e
                ));
            }
            done.push((old_path.clone(), new_path.clone()));
        }
    }

    ulcfg::write_ulcfg(&ulcfg_path, &entries).map_err(|e| e.to_string())?;
    Ok(())
}

/// Sort entries in `ul.cfg` and rewrite the file.
///
/// `sort_by` values: `"name"` | `"name-desc"` | `"size"` | `"size-desc"`.
/// Size is approximated by `parts` count (each chunk ≈ 1 GiB).
/// Returns the number of entries written.
#[tauri::command]
pub fn sort_ulcfg(dest_dir: String, sort_by: String) -> Result<usize, String> {
    let path = ulcfg::ulcfg_path(std::path::Path::new(&dest_dir));
    let mut entries = ulcfg::parse_ulcfg(&path).map_err(|e| e.to_string())?;

    match sort_by.as_str() {
        "name" => entries.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase())),
        "name-desc" => entries.sort_by(|a, b| b.title.to_lowercase().cmp(&a.title.to_lowercase())),
        "size" => entries.sort_by(|a, b| a.parts.cmp(&b.parts)),
        "size-desc" => entries.sort_by(|a, b| b.parts.cmp(&a.parts)),
        _ => {}
    }

    ulcfg::write_ulcfg(&path, &entries).map_err(|e| e.to_string())?;
    Ok(entries.len())
}

/// Migrate games written by this app's old (non-OPL) format into the real
/// USBExtreme format, so they display correctly here and boot on a real PS2.
/// Reads each old entry, computes the correct CRC from the title, renames the
/// chunks to `ul.<crc>.<game_id>.<part>`, and rewrites `ul.cfg` in the real
/// 64-byte format. Returns the number of games migrated.
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

// ── Safe Restore (copy folder ordered) ──────────────────────────────────────

/// One file entry returned by `scan_source_folder`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceFile {
    pub name: String,
    pub subdir: Option<String>, // None = root, Some("CD") / Some("DVD")
    pub size: u64,
}

/// Summary returned by `scan_source_folder`.
#[derive(Debug, Serialize, Deserialize)]
pub struct SourceScanResult {
    pub files: Vec<SourceFile>,
    pub total_bytes: u64,
    /// Immediate subdirectory names that were scanned (empty = root-only backup).
    pub subdirs_found: Vec<String>,
    /// Subdirectories that were skipped because they contain deeper nesting (> 1 level).
    pub subdirs_skipped: Vec<String>,
}

/// Progress event payload emitted during `copy_folder_ordered`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopyFolderProgress {
    pub file: String,
    pub file_index: usize, // 1-based
    pub total_files: usize,
    pub file_pct: u8,
    pub total_pct: u8,
}

/// Final result returned by `copy_folder_ordered`.
#[derive(Debug, Serialize, Deserialize)]
pub struct CopyFolderResult {
    pub copied: usize,
    pub skipped: usize,
    pub total_bytes: u64,
    pub errors: Vec<String>,
}

/// Scan a folder and return its files sorted largest-first.
/// Handles root files plus all immediate subdirectories (1 level deep).
/// Deeper nesting is reported in `subdirs_skipped` but not copied.
#[tauri::command]
pub fn scan_source_folder(source_dir: String) -> Result<SourceScanResult, String> {
    let root = PathBuf::from(&source_dir);
    let (mut files, subdirs_found, subdirs_skipped) = collect_source_files(&root);
    files.sort_by(|a, b| b.size.cmp(&a.size));
    let total_bytes = files.iter().map(|f| f.size).sum();
    Ok(SourceScanResult { files, total_bytes, subdirs_found, subdirs_skipped })
}

/// Copy all files from `source_dir` to `dest_dir`, one at a time, largest
/// first. Emits `copy-folder-progress` Tauri events during the copy so the
/// frontend can update a progress bar. Files that already exist at the
/// destination with the correct size are skipped (resume-safe).
#[tauri::command]
pub async fn copy_folder_ordered(
    app: tauri::AppHandle,
    source_dir: String,
    dest_dir: String,
) -> Result<CopyFolderResult, String> {
    tokio::task::spawn_blocking(move || {
        copy_folder_impl(&app, &PathBuf::from(source_dir), &PathBuf::from(dest_dir))
    })
    .await
    .map_err(|e| format!("Task failed: {}", e))?
}

/// Collect files from a backup folder up to 1 level deep.
///
/// Returns `(files, subdirs_found, subdirs_skipped)`.
/// - Root files are collected with `subdir: None`.
/// - Files inside any immediate subdirectory are collected with `subdir: Some(name)`.
/// - Sub-subdirectories (depth > 1) are ignored; their parent name goes into `subdirs_skipped`.
fn collect_source_files(
    root: &std::path::Path,
) -> (Vec<SourceFile>, Vec<String>, Vec<String>) {
    let mut files: Vec<SourceFile> = Vec::new();
    let mut subdirs_found: Vec<String> = Vec::new();
    let mut subdirs_skipped: Vec<String> = Vec::new();

    let Ok(rd) = std::fs::read_dir(root) else {
        return (files, subdirs_found, subdirs_skipped);
    };

    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_file() {
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            files.push(SourceFile { name, subdir: None, size });
        } else if path.is_dir() {
            let subdir_name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();

            let Ok(sub_rd) = std::fs::read_dir(&path) else { continue; };

            let mut has_nested_dirs = false;
            for sub in sub_rd.flatten() {
                let sub_path = sub.path();
                if sub_path.is_dir() {
                    // Flag deeper nesting — we don't recurse beyond 1 level.
                    has_nested_dirs = true;
                } else if sub_path.is_file() {
                    let size = sub.metadata().map(|m| m.len()).unwrap_or(0);
                    let name = sub_path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    files.push(SourceFile {
                        name,
                        subdir: Some(subdir_name.clone()),
                        size,
                    });
                }
            }

            subdirs_found.push(subdir_name.clone());
            if has_nested_dirs {
                subdirs_skipped.push(subdir_name);
            }
        }
    }

    subdirs_found.sort();
    subdirs_skipped.sort();
    (files, subdirs_found, subdirs_skipped)
}

fn copy_folder_impl(
    app: &tauri::AppHandle,
    source_path: &std::path::Path,
    dest_path: &std::path::Path,
) -> Result<CopyFolderResult, String> {
    std::fs::create_dir_all(dest_path)
        .map_err(|e| format!("Cannot create dest dir: {}", e))?;

    let (mut files, _subdirs_found, _subdirs_skipped) = collect_source_files(source_path);
    files.sort_by(|a, b| b.size.cmp(&a.size));

    let total_files = files.len();
    let total_bytes: u64 = files.iter().map(|f| f.size).sum();
    let mut global_bytes: u64 = 0;
    let mut copied = 0usize;
    let mut skipped = 0usize;
    let mut errors: Vec<String> = Vec::new();

    for (idx, file) in files.iter().enumerate() {
        // Build dest path — create subdir (CD/ / DVD/) if needed.
        let dest_file = if let Some(ref sub) = file.subdir {
            let sub_dir = dest_path.join(sub);
            if let Err(e) = std::fs::create_dir_all(&sub_dir) {
                errors.push(format!("Cannot create {}/: {}", sub, e));
                global_bytes += file.size;
                continue;
            }
            sub_dir.join(&file.name)
        } else {
            dest_path.join(&file.name)
        };

        // Skip if dest already has the right size (resume-safe).
        if dest_file.exists() {
            if std::fs::metadata(&dest_file).map(|m| m.len()).unwrap_or(0) == file.size {
                skipped += 1;
                global_bytes += file.size;
                continue;
            }
        }

        match stream_copy_file(app, source_path, &dest_file, file, idx + 1, total_files, global_bytes, total_bytes) {
            Ok(written) => {
                global_bytes += written;
                copied += 1;
            }
            Err(e) => {
                errors.push(e);
                global_bytes += file.size;
            }
        }
    }

    Ok(CopyFolderResult { copied, skipped, total_bytes, errors })
}

/// Stream-copy one file, emitting progress events every time total_pct advances.
fn stream_copy_file(
    app: &tauri::AppHandle,
    source_root: &std::path::Path,
    dest_path: &std::path::Path,
    file: &SourceFile,
    file_index: usize,
    total_files: usize,
    global_bytes_before: u64,
    total_bytes: u64,
) -> Result<u64, String> {
    use tauri::Emitter;
    use std::io::{Read, Write};

    let src_path = if let Some(ref sub) = file.subdir {
        source_root.join(sub).join(&file.name)
    } else {
        source_root.join(&file.name)
    };

    let src_file = std::fs::File::open(&src_path)
        .map_err(|e| format!("{}: open failed: {}", file.name, e))?;
    let mut reader = std::io::BufReader::with_capacity(8 * 1024 * 1024, src_file);

    let dest_file = std::fs::OpenOptions::new()
        .write(true).create(true).truncate(true)
        .open(dest_path)
        .map_err(|e| format!("{}: create dest failed: {}", file.name, e))?;
    let mut writer = std::io::BufWriter::with_capacity(8 * 1024 * 1024, dest_file);

    // Emit "starting this file" event.
    let _ = app.emit("copy-folder-progress", CopyFolderProgress {
        file: file.name.clone(),
        file_index,
        total_files,
        file_pct: 0,
        total_pct: if total_bytes > 0 { (global_bytes_before * 100 / total_bytes) as u8 } else { 0 },
    });

    let mut buf = vec![0u8; 8 * 1024 * 1024];
    let mut bytes_written: u64 = 0;
    let mut last_emitted_pct: u8 = 0;

    loop {
        let n = reader.read(&mut buf)
            .map_err(|e| format!("{}: read failed: {}", file.name, e))?;
        if n == 0 { break; }
        writer.write_all(&buf[..n])
            .map_err(|e| format!("{}: write failed: {}", file.name, e))?;
        bytes_written += n as u64;

        let file_pct = if file.size > 0 { (bytes_written * 100 / file.size).min(100) as u8 } else { 100 };
        let total_pct = if total_bytes > 0 {
            ((global_bytes_before + bytes_written) * 100 / total_bytes).min(100) as u8
        } else { 100 };

        // Emit only when total percentage advances to keep IPC traffic low.
        if total_pct > last_emitted_pct || file_pct == 100 {
            last_emitted_pct = total_pct;
            let _ = app.emit("copy-folder-progress", CopyFolderProgress {
                file: file.name.clone(),
                file_index,
                total_files,
                file_pct,
                total_pct,
            });
        }
    }

    writer.flush().map_err(|e| format!("{}: flush failed: {}", file.name, e))?;
    Ok(bytes_written)
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
