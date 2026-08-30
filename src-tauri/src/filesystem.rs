use serde::{Deserialize, Serialize};
use std::io;
use std::path::Path;
use thiserror::Error;

#[derive(Error, Debug)]
#[allow(dead_code)]
pub enum FsError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
    #[error("Filesystem detection not supported on this OS")]
    UnsupportedOs,
    #[error("No removable devices found")]
    NoDevices,
    #[error("Failed to detect filesystem: {0}")]
    DetectionFailed(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FilesystemType {
    Fat32,
    Ntfs,
    ExFat,
    Apfs,
    HfsPlus,
    Ext4,
    Unknown(String),
}

impl std::fmt::Display for FilesystemType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FilesystemType::Fat32 => write!(f, "FAT32"),
            FilesystemType::Ntfs => write!(f, "NTFS"),
            FilesystemType::ExFat => write!(f, "exFAT"),
            FilesystemType::Apfs => write!(f, "APFS"),
            FilesystemType::HfsPlus => write!(f, "HFS+"),
            FilesystemType::Ext4 => write!(f, "ext4"),
            FilesystemType::Unknown(s) => write!(f, "{}", s),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub name: String,
    pub mount_point: String,
    pub filesystem: FilesystemType,
    pub free_space: u64,
    pub total_space: u64,
    pub is_removable: bool,
    pub recommended_mode: String, // "split" or "nosplit"
}

impl FilesystemType {
    /// Whether this filesystem needs split mode (files >4GB not supported).
    pub fn needs_split(&self) -> bool {
        matches!(self, FilesystemType::Fat32)
    }

    /// Recommended processing mode for PS2 OPL.
    pub fn recommended_mode(&self) -> &str {
        if self.needs_split() {
            "split"
        } else {
            "nosplit"
        }
    }
}

/// Detect all available removable storage devices.
/// Cross-platform: uses different backends per OS.
pub fn detect_devices() -> Result<Vec<DeviceInfo>, FsError> {
    #[cfg(target_os = "macos")]
    {
        detect_devices_macos()
    }

    #[cfg(target_os = "windows")]
    {
        detect_devices_windows()
    }

    #[cfg(target_os = "linux")]
    {
        detect_devices_linux()
    }
}

/// Detect the first available removable device (convenience).
pub fn detect_primary_device() -> Result<DeviceInfo, FsError> {
    let devices = detect_devices()?;
    devices
        .into_iter()
        .find(|d| d.is_removable)
        .ok_or(FsError::NoDevices)
}

/// Get filesystem info for a specific mount point.
pub fn get_device_info(mount_point: &Path) -> Result<DeviceInfo, FsError> {
    #[cfg(target_os = "macos")]
    {
        get_device_info_macos(mount_point)
    }

    #[cfg(target_os = "windows")]
    {
        get_device_info_windows(mount_point)
    }

    #[cfg(target_os = "linux")]
    {
        get_device_info_linux(mount_point)
    }
}

// ── macOS implementation ──

#[cfg(target_os = "macos")]
fn detect_devices_macos() -> Result<Vec<DeviceInfo>, FsError> {
    // Scan /Volumes/ and check each for "Removable Media" via diskutil
    let mut devices = Vec::new();
    let volumes_dir = Path::new("/Volumes");

    if volumes_dir.exists() {
        for entry in std::fs::read_dir(volumes_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                // Check if this is a removable device
                if is_removable_macos(&path) {
                    if let Ok(info) = get_device_info_macos(&path) {
                        devices.push(info);
                    }
                }
            }
        }
    }

    // Fallback: if no removable devices found, show all non-system volumes
    if devices.is_empty() {
        if volumes_dir.exists() {
            for entry in std::fs::read_dir(volumes_dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.is_dir() {
                    let name = path.file_name().unwrap_or_default().to_string_lossy();
                    // Skip system volumes
                    if name == "Macintosh HD" || name.starts_with('.') {
                        continue;
                    }
                    if let Ok(info) = get_device_info_macos(&path) {
                        devices.push(info);
                    }
                }
            }
        }
    }

    Ok(devices)
}

/// Check if a macOS volume is removable using diskutil.
fn is_removable_macos(mount_point: &Path) -> bool {
    use std::process::Command;
    let output = Command::new("diskutil")
        .args(["info", mount_point.to_str().unwrap_or("")])
        .output();
    if let Ok(out) = output {
        let stdout = String::from_utf8_lossy(&out.stdout);
        // Check for removable indicators — be lenient
        return stdout.contains("Removable Media")
            || stdout.contains("Protocol:                 USB")
            || stdout.contains("Protocol:                USB")
            || stdout.contains("Removable:               Yes")
            || stdout.contains("External");
    }
    false
}

#[cfg(target_os = "macos")]
fn get_device_info_macos(mount_point: &Path) -> Result<DeviceInfo, FsError> {
    use std::process::Command;

    let output = Command::new("diskutil")
        .args(["info", "-plist", mount_point.to_str().unwrap_or("")])
        .output()
        .map_err(|e| FsError::DetectionFailed(e.to_string()))?;

    if !output.status.success() {
        return Err(FsError::DetectionFailed(
            String::from_utf8_lossy(&output.stderr).to_string(),
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Parse filesystem from diskutil output
    let filesystem = if stdout.contains("FAT32") {
        FilesystemType::Fat32
    } else if stdout.contains("ExFAT") || stdout.contains("exFAT") {
        FilesystemType::ExFat
    } else if stdout.contains("NTFS") {
        FilesystemType::Ntfs
    } else if stdout.contains("APFS") {
        FilesystemType::Apfs
    } else if stdout.contains("HFS+") {
        FilesystemType::HfsPlus
    } else {
        FilesystemType::Unknown("Unknown".into())
    };

    // Get space info using statvfs
    let (free_space, total_space) = get_space_info(mount_point);

    let name = mount_point
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "Unknown".into());

    let recommended_mode = filesystem.recommended_mode().to_string();

    Ok(DeviceInfo {
        name,
        mount_point: mount_point.to_string_lossy().to_string(),
        filesystem,
        free_space,
        total_space,
        is_removable: true,
        recommended_mode,
    })
}

// ── Windows implementation ──

#[cfg(target_os = "windows")]
fn detect_devices_windows() -> Result<Vec<DeviceInfo>, FsError> {
    use std::process::Command;

    let output = Command::new("wmic")
        .args([
            "logicaldisk",
            "where",
            "DriveType=2",
            "get",
            "DeviceID,FileSystem,FreeSpace,Size,VolumeName",
            "/format:csv",
        ])
        .output()
        .map_err(|e| FsError::DetectionFailed(e.to_string()))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut devices = Vec::new();

    for line in stdout.lines().skip(1) {
        // Skip header
        let parts: Vec<&str> = line.split(',').collect();
        if parts.len() >= 6 {
            let mount = parts[1].trim();
            if !mount.is_empty() {
                if let Ok(info) = get_device_info_windows(Path::new(mount)) {
                    devices.push(info);
                }
            }
        }
    }

    Ok(devices)
}

#[cfg(target_os = "windows")]
fn get_device_info_windows(mount_point: &Path) -> Result<DeviceInfo, FsError> {
    use std::process::Command;

    let drive = mount_point
        .to_str()
        .unwrap_or("")
        .chars()
        .take(2)
        .collect::<String>();

    let output = Command::new("wmic")
        .args([
            "logicaldisk",
            "where",
            &format!("DeviceID='{}'", drive),
            "get",
            "FileSystem,FreeSpace,Size,VolumeName",
            "/format:csv",
        ])
        .output()
        .map_err(|e| FsError::DetectionFailed(e.to_string()))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().nth(1).unwrap_or("");

    let parts: Vec<&str> = line.split(',').collect();
    if parts.len() < 5 {
        return Err(FsError::DetectionFailed("Parse error".into()));
    }

    let filesystem = match parts[1].trim().to_uppercase().as_str() {
        "FAT32" => FilesystemType::Fat32,
        "NTFS" => FilesystemType::Ntfs,
        "EXFAT" => FilesystemType::ExFat,
        other => FilesystemType::Unknown(other.to_string()),
    };

    let free_space: u64 = parts[2].trim().parse().unwrap_or(0);
    let total_space: u64 = parts[4].trim().parse().unwrap_or(0);
    let name = parts[3].trim().to_string();

    let recommended_mode = filesystem.recommended_mode().to_string();

    Ok(DeviceInfo {
        name: if name.is_empty() { drive } else { name },
        mount_point: mount_point.to_string_lossy().to_string(),
        filesystem,
        free_space,
        total_space,
        is_removable: true,
        recommended_mode,
    })
}

// ── Linux implementation ──

#[cfg(target_os = "linux")]
fn detect_devices_linux() -> Result<Vec<DeviceInfo>, FsError> {
    use std::process::Command;

    let output = Command::new("lsblk")
        .args(["-J", "-o", "NAME,MOUNTPOINT,FSTYPE,SIZE,ROTA,TRAN"])
        .output()
        .map_err(|e| FsError::DetectionFailed(e.to_string()))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut devices = Vec::new();

    // Simple parse: look for mounted removable devices
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&stdout) {
        if let Some(blockdevices) = json["blockdevices"].as_array() {
            for dev in blockdevices {
                if let Some(children) = dev["children"].as_array() {
                    for child in children {
                        if let Some(mountpoint) = child["mountpoint"].as_str() {
                            if !mountpoint.is_empty() {
                                if let Ok(info) = get_device_info_linux(Path::new(mountpoint)) {
                                    devices.push(info);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(devices)
}

#[cfg(target_os = "linux")]
fn get_device_info_linux(mount_point: &Path) -> Result<DeviceInfo, FsError> {
    use std::process::Command;

    let output = Command::new("findmnt")
        .args(["-n", "-o", "FSTYPE,SIZE,AVAIL", mount_point.to_str().unwrap_or("")])
        .output()
        .map_err(|e| FsError::DetectionFailed(e.to_string()))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parts: Vec<&str> = stdout.split_whitespace().collect();

    let filesystem = match parts.first().unwrap_or(&"") {
        &"vfat" | &"fat32" => FilesystemType::Fat32,
        &"ntfs" | &"ntfs3" => FilesystemType::Ntfs,
        &"exfat" => FilesystemType::ExFat,
        &"ext4" => FilesystemType::Ext4,
        other => FilesystemType::Unknown(other.to_string()),
    };

    let (free_space, total_space) = get_space_info(mount_point);

    let name = mount_point
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "USB Drive".into());

    let recommended_mode = filesystem.recommended_mode().to_string();

    Ok(DeviceInfo {
        name,
        mount_point: mount_point.to_string_lossy().to_string(),
        filesystem,
        free_space,
        total_space,
        is_removable: true,
        recommended_mode,
    })
}

/// Get free/total space using statvfs (Unix) or GetDiskFreeSpaceEx (Windows).
fn get_space_info(path: &Path) -> (u64, u64) {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::mem;

        // Ensure path exists and is a directory
        if !path.exists() || !path.is_dir() {
            return (0, 0);
        }

        unsafe {
            let path_c = CString::new(path.to_string_lossy().as_bytes()).unwrap();
            let mut stat: libc::statvfs = mem::zeroed();
            if libc::statvfs(path_c.as_ptr(), &mut stat) == 0 && stat.f_frsize > 0 {
                let free = stat.f_bavail as u64 * stat.f_frsize as u64;
                let total = stat.f_blocks as u64 * stat.f_frsize as u64;
                if total > 0 {
                    return (free, total);
                }
            }
        }
        // Fallback: try parent directory
        if let Some(parent) = path.parent() {
            return get_space_info(parent);
        }
        (0, 0)
    }

    #[cfg(windows)]
    {
        (0, 0) // placeholder; wmic handles it on Windows
    }
}

// ── Drive Formatting ──

/// Format a drive to FAT32 with the given volume label.
/// WARNING: This erases ALL data on the drive!
pub fn format_drive_fat32(device: &DeviceInfo, label: &str) -> Result<(), FsError> {
    #[cfg(target_os = "macos")]
    {
        format_drive_macos(device, label)
    }

    #[cfg(target_os = "windows")]
    {
        format_drive_windows(device, label)
    }

    #[cfg(target_os = "linux")]
    {
        format_drive_linux(device, label)
    }
}

#[cfg(target_os = "macos")]
fn format_drive_macos(device: &DeviceInfo, label: &str) -> Result<(), FsError> {
    use std::process::Command;

    // Get the disk identifier (e.g., /dev/disk2) from mount point
    let disk_id = get_disk_identifier_macos(&device.mount_point)?;
    
    let output = Command::new("diskutil")
        .args([
            "eraseDisk",
            "FAT32",
            label,
            "MBRFormat",
            &disk_id,
        ])
        .output()
        .map_err(|e| FsError::DetectionFailed(e.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(FsError::DetectionFailed(format!(
            "diskutil eraseDisk failed: {}",
            stderr
        )));
    }

    Ok(())
}

#[cfg(target_os = "macos")]
fn get_disk_identifier_macos(mount_point: &str) -> Result<String, FsError> {
    use std::process::Command;

    let output = Command::new("diskutil")
        .args(["info", "-plist", mount_point])
        .output()
        .map_err(|e| FsError::DetectionFailed(e.to_string()))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    
    // Parse plist to get DeviceIdentifier
    // Look for <key>DeviceIdentifier</key><string>disk2s1</string>
    if let Some(start) = stdout.find("<key>DeviceIdentifier</key>") {
        let rest = &stdout[start..];
        if let Some(s_start) = rest.find("<string>") {
            let s_rest = &rest[s_start + 8..];
            if let Some(s_end) = s_rest.find("</string>") {
                let disk_id = &s_rest[..s_end];
                // Return parent disk (disk2s1 -> disk2)
                if let Some(base) = disk_id.strip_suffix(|c: char| c.is_ascii_digit() || c == 's') {
                    return Ok(format!("/dev/{}", base.trim_end_matches('s')));
                }
                return Ok(format!("/dev/{}", disk_id));
            }
        }
    }

    Err(FsError::DetectionFailed("Could not determine disk identifier".into()))
}

#[cfg(target_os = "windows")]
fn format_drive_windows(device: &DeviceInfo, label: &str) -> Result<(), FsError> {
    use std::process::Command;

    // Get drive letter (e.g., "E:" from "E:\")
    let drive = device
        .mount_point
        .chars()
        .take(2)
        .collect::<String>();

    // Use format command: format E: /FS:FAT32 /V:LABEL /Q /Y
    let output = Command::new("format")
        .args([
            &drive,
            "/FS:FAT32",
            &format!("/V:{}", label),
            "/Q",  // Quick format
            "/Y",  // Yes to confirmation
        ])
        .output()
        .map_err(|e| FsError::DetectionFailed(e.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(FsError::DetectionFailed(format!(
            "format failed: {}",
            stderr
        )));
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn format_drive_linux(device: &DeviceInfo, label: &str) -> Result<(), FsError> {
    use std::process::Command;

    // Get device path from mount point
    let dev_path = get_device_path_linux(&device.mount_point)?;
    
    let output = Command::new("mkfs.fat")
        .args([
            "-F", "32",
            "-n", label,
            &dev_path,
        ])
        .output()
        .map_err(|e| FsError::DetectionFailed(e.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(FsError::DetectionFailed(format!(
            "mkfs.fat failed: {}",
            stderr
        )));
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn get_device_path_linux(mount_point: &str) -> Result<String, FsError> {
    use std::process::Command;

    let output = Command::new("findmnt")
        .args(["-n", "-o", "SOURCE", mount_point])
        .output()
        .map_err(|e| FsError::DetectionFailed(e.to_string()))?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        return Err(FsError::DetectionFailed("Could not determine device path".into()));
    }

    Ok(stdout)
}

// ── File Contiguity Check ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContiguityResult {
    pub file: String,
    pub contiguous: bool,
    pub extents: u32,
    pub size: u64,
}

/// Check if a file's clusters are physically contiguous on disk.
/// Returns the number of extents (1 = fully contiguous).
pub fn check_file_contiguity(path: &Path) -> Result<ContiguityResult, FsError> {
    let meta = std::fs::metadata(path).map_err(FsError::Io)?;
    let size = meta.len();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    if size == 0 {
        return Ok(ContiguityResult {
            file: name,
            contiguous: true,
            extents: 1,
            size,
        });
    }

    let extents = get_extent_count(path)?;

    Ok(ContiguityResult {
        file: name,
        contiguous: extents == 1,
        extents,
        size,
    })
}

/// Check contiguity for multiple files in a directory.
pub fn check_dir_contiguity(dir: &Path, pattern: &str) -> Result<Vec<ContiguityResult>, FsError> {
    let mut results = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(FsError::Io)? {
        let entry = entry.map_err(FsError::Io)?;
        let name = entry.file_name().to_string_lossy().to_string();
        if pattern.is_empty() || name.starts_with(pattern) {
            if entry.path().is_file() {
                results.push(check_file_contiguity(&entry.path())?);
            }
        }
    }
    Ok(results)
}

#[cfg(target_os = "macos")]
fn get_extent_count(path: &Path) -> Result<u32, FsError> {
    use std::os::unix::io::AsRawFd;

    // ponytail: macOS log2phys struct — not in libc crate, define manually
    #[repr(C)]
    struct Log2phys {
        l2p_flags: i32,
        l2p_contigbytes: i64,
        l2p_devoffset: i64,
    }

    let file = std::fs::File::open(path).map_err(FsError::Io)?;
    let fd = file.as_raw_fd();
    let size = file.metadata().map_err(FsError::Io)?.len();

    if size == 0 {
        return Ok(1);
    }

    // F_LOG2PHYS gives us l2p_contigbytes: how many bytes are contiguous from this offset.
    // If it covers the whole file, it's one extent.
    let mut l2p = Log2phys {
        l2p_flags: 0,
        l2p_contigbytes: 0,
        l2p_devoffset: 0,
    };

    let ret = unsafe { libc::fcntl(fd, libc::F_LOG2PHYS, &mut l2p) };
    if ret < 0 {
        return Err(FsError::DetectionFailed(format!(
            "fcntl F_LOG2PHYS failed: {}",
            std::io::Error::last_os_error()
        )));
    }

    // l2p_contigbytes = bytes physically contiguous starting at this logical offset
    if l2p.l2p_contigbytes as u64 >= size {
        return Ok(1);
    }

    // File is fragmented — scan to count extents
    let block_size = 4096u64;
    let mut extents = 0u32;
    let mut offset = 0u64;
    let mut last_dev_end: Option<i64> = None;

    while offset < size {
        let mut l2p = Log2phys {
            l2p_flags: 0,
            l2p_contigbytes: 0,
            l2p_devoffset: 0,
        };
        // ponytail: F_LOG2PHYS_EXT not needed, F_LOG2PHYS works for extent counting
        let ret = unsafe { libc::fcntl(fd, libc::F_LOG2PHYS, &mut l2p) };
        if ret < 0 {
            break;
        }

        let dev_end = l2p.l2p_devoffset + l2p.l2p_contigbytes;
        if last_dev_end.map_or(true, |end| l2p.l2p_devoffset != end) {
            extents += 1;
        }
        last_dev_end = Some(dev_end);
        offset += l2p.l2p_contigbytes.max(block_size as i64) as u64;
    }

    Ok(extents.max(1))
}

#[cfg(target_os = "windows")]
fn get_extent_count(path: &Path) -> Result<u32, FsError> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::FSCTL_GET_RETRIEVAL_POINTERS;
    use windows_sys::Win32::System::IO::DeviceIoControl;

    #[repr(C)]
    struct StartingVcnInputBuffer {
        starting_vcn: i64,
    }

    #[repr(C)]
    struct RetrievalPointersBuffer {
        extent_count: u32,
        starting_vcn: i64,
        // Extents array follows, but we only need the count
    }

    let file = std::fs::File::open(path).map_err(FsError::Io)?;
    let handle = file.as_raw_handle() as isize;

    let start_vcn = StartingVcnInputBuffer { starting_vcn: 0 };
    let mut rpb: RetrievalPointersBuffer = unsafe { std::mem::zeroed() };
    let mut bytes_returned: u32 = 0;

    let ret = unsafe {
        DeviceIoControl(
            handle,
            FSCTL_GET_RETRIEVAL_POINTERS,
            &start_vcn as *const _ as *const _,
            std::mem::size_of::<StartingVcnInputBuffer>() as u32,
            &mut rpb as *mut _ as *mut _,
            std::mem::size_of::<RetrievalPointersBuffer>() as u32,
            &mut bytes_returned,
            std::ptr::null_mut(),
        )
    };

    if ret == 0 {
        let err = std::io::Error::last_os_error();
        // ERROR_MORE_DATA (234) means buffer too small but extent_count is valid
        if err.raw_os_error() != Some(234) {
            return Err(FsError::DetectionFailed(format!(
                "FSCTL_GET_RETRIEVAL_POINTERS failed: {}",
                err
            )));
        }
    }

    Ok(rpb.extent_count.max(1))
}

#[cfg(target_os = "linux")]
fn get_extent_count(path: &Path) -> Result<u32, FsError> {
    use std::os::unix::io::AsRawFd;

    // ponytail: fiemap structs not in libc, define manually
    #[repr(C)]
    struct FiemapExtent {
        fe_logical: u64,
        fe_physical: u64,
        fe_length: u64,
        fe_flags: u64,
        fe_reserved: [u64; 4],
    }

    #[repr(C)]
    struct Fiemap {
        fm_start: u64,
        fm_length: u64,
        fm_flags: u32,
        fm_mapped_extents: u32,
        fm_extent_count: u32,
        fm_reserved: u32,
        fm_extents: *mut FiemapExtent,
    }

    const FIEMAP: u32 = 0xC020660B;

    let file = std::fs::File::open(path).map_err(FsError::Io)?;
    let fd = file.as_raw_fd();
    let size = file.metadata().map_err(FsError::Io)?.len();

    if size == 0 {
        return Ok(1);
    }

    let mut extent = FiemapExtent {
        fe_logical: 0,
        fe_physical: 0,
        fe_length: 0,
        fe_flags: 0,
        fe_reserved: [0; 4],
    };
    let mut fiemap = Fiemap {
        fm_start: 0,
        fm_length: size,
        fm_flags: 0,
        fm_mapped_extents: 0,
        fm_extent_count: 1,
        fm_reserved: 0,
        fm_extents: &mut extent as *mut FiemapExtent,
    };

    let ret = unsafe { libc::ioctl(fd, FIEMAP, &mut fiemap) };
    if ret < 0 {
        // FIEMAP not supported — fallback to non-contiguous assumption
        return Ok(2);
    }

    // If mapped_extents == 1 and covers full file, it's contiguous
    if fiemap.fm_mapped_extents == 1 && extent.fe_length >= size {
        return Ok(1);
    }

    // Count all extents
    let mut total_extents = 0u32;
    let mut offset = 0u64;
    while offset < size {
        let mut ext = FiemapExtent {
            fe_logical: 0,
            fe_physical: 0,
            fe_length: 0,
            fe_flags: 0,
            fe_reserved: [0; 4],
        };
        let mut fm = Fiemap {
            fm_start: offset,
            fm_length: size - offset,
            fm_flags: 0,
            fm_mapped_extents: 0,
            fm_extent_count: 1,
            fm_reserved: 0,
            fm_extents: &mut ext as *mut FiemapExtent,
        };
        let r = unsafe { libc::ioctl(fd, FIEMAP, &mut fm) };
        if r < 0 || fm.fm_mapped_extents == 0 {
            break;
        }
        total_extents += 1;
        offset = ext.fe_logical + ext.fe_length;
    }

    Ok(total_extents.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filesystem_needs_split() {
        assert!(FilesystemType::Fat32.needs_split());
        assert!(!FilesystemType::Ntfs.needs_split());
        assert!(!FilesystemType::ExFat.needs_split());
    }

    #[test]
    fn test_recommended_mode() {
        assert_eq!(FilesystemType::Fat32.recommended_mode(), "split");
        assert_eq!(FilesystemType::Ntfs.recommended_mode(), "nosplit");
        assert_eq!(FilesystemType::ExFat.recommended_mode(), "nosplit");
    }
}
