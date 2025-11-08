use adb_client::{is_adb_device, ADBDeviceExt, ADBUSBDevice, RustADBError};
use rusb::{Device, UsbContext};
use serde::Serialize;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Serialize)]
pub struct SyncSummary {
    device: DeviceDetails,
    files_synced: usize,
    skipped_entries: usize,
    directories_created: usize,
    bytes_uploaded: u64,
    remote_path: String,
    local_root: String,
}

#[derive(Debug, Serialize)]
struct DeviceDetails {
    vendor_id: u16,
    product_id: u16,
    manufacturer: Option<String>,
    product: Option<String>,
}

impl From<AndroidDeviceInfo> for DeviceDetails {
    fn from(value: AndroidDeviceInfo) -> Self {
        Self {
            vendor_id: value.vendor_id,
            product_id: value.product_id,
            manufacturer: value.manufacturer,
            product: value.product,
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![sync_folders])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[tauri::command]
async fn sync_folders(local_path: String, device_path: String) -> Result<SyncSummary, String> {
    tauri::async_runtime::spawn_blocking(move || perform_sync(local_path, device_path))
        .await
        .map_err(|e| format!("sync task failed: {e}"))?
        .map_err(|e| e.to_string())
}

fn perform_sync(local_path: String, device_path: String) -> Result<SyncSummary, SyncError> {
    let local_root = canonicalize_local_root(&local_path)?;
    let remote_root = normalize_remote_path(&device_path)?;

    let device_info = detect_android_device()?;
    let mut adb_device = ADBUSBDevice::new(device_info.vendor_id, device_info.product_id)?;

    let mut created_dirs = HashSet::new();
    let mut stats = SyncStats::default();

    ensure_remote_dir(&mut adb_device, &remote_root, &mut created_dirs, &mut stats)?;
    sync_directory(
        &mut adb_device,
        &local_root,
        &local_root,
        &remote_root,
        &mut created_dirs,
        &mut stats,
    )?;

    Ok(SyncSummary {
        device: device_info.into(),
        files_synced: stats.files_synced,
        skipped_entries: stats.skipped_entries,
        directories_created: stats.directories_created,
        bytes_uploaded: stats.bytes_uploaded,
        remote_path: remote_root,
        local_root: local_root.display().to_string(),
    })
}

fn canonicalize_local_root(path: &str) -> Result<PathBuf, SyncError> {
    let candidate = PathBuf::from(path.trim());
    if candidate.as_os_str().is_empty() {
        return Err(SyncError::InvalidLocalPath(
            "Local path cannot be empty".into(),
        ));
    }

    if !candidate.exists() {
        return Err(SyncError::InvalidLocalPath(format!(
            "Local path '{}' does not exist",
            candidate.display()
        )));
    }

    if !candidate.is_dir() {
        return Err(SyncError::InvalidLocalPath(format!(
            "Local path '{}' must be a directory",
            candidate.display()
        )));
    }

    Ok(candidate.canonicalize()?)
}

fn normalize_remote_path(path: &str) -> Result<String, SyncError> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err(SyncError::InvalidRemotePath(
            "Remote path cannot be empty".into(),
        ));
    }

    let sanitized = trimmed.replace('\\', "/");
    let mut parts = Vec::new();
    for segment in sanitized.split('/') {
        match segment {
            "" | "." => continue,
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }

    if parts.is_empty() {
        return Ok("/".into());
    }

    Ok(format!("/{}", parts.join("/")))
}

fn sync_directory(
    device: &mut ADBUSBDevice,
    root: &Path,
    current: &Path,
    remote_root: &str,
    created_dirs: &mut HashSet<String>,
    stats: &mut SyncStats,
) -> Result<(), SyncError> {
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let entry_path = entry.path();
        let metadata = entry.metadata()?;
        let relative_path = entry_path
            .strip_prefix(root)
            .unwrap_or_else(|_| Path::new(""));

        if metadata.is_dir() {
            let remote_dir = build_remote_path(remote_root, relative_path);
            ensure_remote_dir(device, &remote_dir, created_dirs, stats)?;
            sync_directory(device, root, &entry_path, remote_root, created_dirs, stats)?;
        } else if metadata.is_file() {
            let remote_file = build_remote_path(remote_root, relative_path);
            let parent = relative_path
                .parent()
                .map(|p| build_remote_path(remote_root, p))
                .unwrap_or_else(|| remote_root.to_string());
            ensure_remote_dir(device, &parent, created_dirs, stats)?;
            push_file(device, &entry_path, &remote_file, metadata.len(), stats)?;
        } else {
            stats.skipped_entries += 1;
        }
    }

    Ok(())
}

fn push_file(
    device: &mut ADBUSBDevice,
    local_path: &Path,
    remote_path: &str,
    size_hint: u64,
    stats: &mut SyncStats,
) -> Result<(), SyncError> {
    let mut file = File::open(local_path)?;
    device.push(&mut file, &remote_path)?;
    stats.files_synced += 1;
    stats.bytes_uploaded += size_hint;
    Ok(())
}

fn ensure_remote_dir(
    device: &mut ADBUSBDevice,
    remote_dir: &str,
    created_dirs: &mut HashSet<String>,
    stats: &mut SyncStats,
) -> Result<(), SyncError> {
    let normalized = if remote_dir == "/" {
        "/".to_string()
    } else {
        remote_dir.trim_end_matches('/').to_string()
    };

    if !created_dirs.insert(normalized.clone()) {
        return Ok(());
    }

    if normalized != "/" {
        let mut sink = io::sink();
        device.shell_command(&["mkdir", "-p", normalized.as_str()], &mut sink)?;
        stats.directories_created += 1;
    }

    Ok(())
}

fn build_remote_path(remote_root: &str, relative: &Path) -> String {
    let mut pieces = Vec::new();
    for component in relative.components() {
        if let Component::Normal(part) = component {
            let text = part.to_string_lossy();
            if !text.is_empty() {
                pieces.push(text.into_owned());
            }
        }
    }

    if pieces.is_empty() {
        return remote_root.to_string();
    }

    if remote_root == "/" {
        format!("/{}", pieces.join("/"))
    } else {
        format!("{}/{}", remote_root.trim_end_matches('/'), pieces.join("/"))
    }
}

fn detect_android_device() -> Result<AndroidDeviceInfo, SyncError> {
    let devices = rusb::devices()?;
    let mut matches = Vec::new();

    for device in devices.iter() {
        let Ok(descriptor) = device.device_descriptor() else {
            continue;
        };

        if !is_adb_device(&device, &descriptor) {
            continue;
        }

        matches.push(AndroidDeviceInfo::from_usb_device(device, descriptor));
    }

    match matches.len() {
        0 => Err(SyncError::DeviceNotFound),
        1 => Ok(matches.remove(0)),
        _ => Err(SyncError::MultipleDevices(
            matches
                .iter()
                .map(|info| (info.vendor_id, info.product_id))
                .collect(),
        )),
    }
}

#[derive(Debug)]
struct AndroidDeviceInfo {
    vendor_id: u16,
    product_id: u16,
    manufacturer: Option<String>,
    product: Option<String>,
}

impl AndroidDeviceInfo {
    fn from_usb_device<T: UsbContext>(
        device: Device<T>,
        descriptor: rusb::DeviceDescriptor,
    ) -> Self {
        let vendor_id = descriptor.vendor_id();
        let product_id = descriptor.product_id();

        let (manufacturer, product) = device
            .open()
            .ok()
            .map(|handle| {
                let manufacturer = handle.read_manufacturer_string_ascii(&descriptor).ok();
                let product = handle.read_product_string_ascii(&descriptor).ok();
                (manufacturer, product)
            })
            .unwrap_or((None, None));

        Self {
            vendor_id,
            product_id,
            manufacturer,
            product,
        }
    }
}

#[derive(Default)]
struct SyncStats {
    files_synced: usize,
    skipped_entries: usize,
    directories_created: usize,
    bytes_uploaded: u64,
}

#[derive(Debug)]
enum SyncError {
    InvalidLocalPath(String),
    InvalidRemotePath(String),
    DeviceNotFound,
    MultipleDevices(Vec<(u16, u16)>),
    Usb(rusb::Error),
    Adb(RustADBError),
    Io(io::Error),
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncError::InvalidLocalPath(msg) => write!(f, "{msg}"),
            SyncError::InvalidRemotePath(msg) => write!(f, "{msg}"),
            SyncError::DeviceNotFound => write!(
                f,
                "No Android device detected over USB. Ensure USB debugging is enabled."
            ),
            SyncError::MultipleDevices(devs) => {
                write!(
                    f,
                    "Multiple Android devices detected ({:?}). Connect only one device.",
                    devs
                )
            }
            SyncError::Usb(err) => write!(f, "USB error: {err}"),
            SyncError::Adb(err) => write!(f, "ADB error: {err}"),
            SyncError::Io(err) => write!(f, "File system error: {err}"),
        }
    }
}

impl std::error::Error for SyncError {}

impl From<rusb::Error> for SyncError {
    fn from(value: rusb::Error) -> Self {
        SyncError::Usb(value)
    }
}

impl From<RustADBError> for SyncError {
    fn from(value: RustADBError) -> Self {
        SyncError::Adb(value)
    }
}

impl From<io::Error> for SyncError {
    fn from(value: io::Error) -> Self {
        SyncError::Io(value)
    }
}
