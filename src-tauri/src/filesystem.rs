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
    use std::process::Command;

    let output = Command::new("diskutil")
        .args(["list", "-plist", "external"])
        .output()
        .map_err(|e| FsError::DetectionFailed(e.to_string()))?;

    if !output.status.success() {
        return Err(FsError::DetectionFailed(
            String::from_utf8_lossy(&output.stderr).to_string(),
        ));
    }

    // Parse diskutil output to find mount points
    // For simplicity, check /Volumes/ for external drives
    let mut devices = Vec::new();
    let volumes_dir = Path::new("/Volumes");

    if volumes_dir.exists() {
        for entry in std::fs::read_dir(volumes_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                if let Ok(info) = get_device_info_macos(&path) {
                    devices.push(info);
                }
            }
        }
    }

    Ok(devices)
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

        unsafe {
            let path_c = CString::new(path.to_string_lossy().as_bytes()).unwrap();
            let mut stat: libc::statvfs = mem::zeroed();
            if libc::statvfs(path_c.as_ptr(), &mut stat) == 0 {
                let free = stat.f_bavail as u64 * stat.f_frsize as u64;
                let total = stat.f_blocks as u64 * stat.f_frsize as u64;
                return (free, total);
            }
        }
        (0, 0)
    }

    #[cfg(windows)]
    {
        (0, 0) // placeholder; wmic handles it on Windows
    }
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
