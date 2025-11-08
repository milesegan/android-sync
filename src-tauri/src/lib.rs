use adb_client::{is_adb_device, ADBDeviceExt, ADBUSBDevice, AdbStatResponse, RustADBError};
use rusb::{Device, UsbContext};
use serde::Serialize;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;
use tauri::{Emitter, Window};

#[derive(Debug, Serialize)]
pub struct SyncSummary {
    device: DeviceDetails,
    files_synced: usize,
    files_deleted: usize,
    skipped_entries: usize,
    directories_created: usize,
    bytes_uploaded: u64,
    remote_path: String,
    local_root: String,
    dry_run: bool,
}

const PROGRESS_EVENT: &str = "sync-progress";

#[derive(Debug, Serialize, Clone)]
struct SyncProgressPayload {
    processed_files: usize,
    total_files: usize,
    current_file: Option<String>,
    dry_run: bool,
}

struct ProgressReporter {
    window: Window,
    total_files: usize,
    processed_files: usize,
    dry_run: bool,
}

impl ProgressReporter {
    fn new(window: Window, total_files: usize, dry_run: bool) -> Self {
        let reporter = Self {
            window,
            total_files,
            processed_files: 0,
            dry_run,
        };
        reporter.emit(None);
        reporter
    }

    fn file_processed(&mut self, current_file: Option<&str>) {
        self.advance(current_file);
    }

    fn directory_prepared(&mut self, directory: &str) {
        self.advance(Some(directory));
    }

    fn emit(&self, current_file: Option<&str>) {
        let payload = SyncProgressPayload {
            processed_files: self.processed_files,
            total_files: self.total_files,
            current_file: current_file.map(|value| value.to_string()),
            dry_run: self.dry_run,
        };
        let _ = self.window.emit(PROGRESS_EVENT, payload);
    }

    fn advance(&mut self, current_file: Option<&str>) {
        self.processed_files = self.processed_files.saturating_add(1);
        self.emit(current_file);
    }
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
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![sync_folders])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[tauri::command]
async fn sync_folders(
    window: Window,
    local_path: String,
    device_path: String,
    dry_run: bool,
) -> Result<SyncSummary, String> {
    tauri::async_runtime::spawn_blocking(move || {
        perform_sync(window, local_path, device_path, dry_run)
    })
    .await
    .map_err(|e| format!("sync task failed: {e}"))?
    .map_err(|e| e.to_string())
}

fn perform_sync(
    window: Window,
    local_path: String,
    device_path: String,
    dry_run: bool,
) -> Result<SyncSummary, SyncError> {
    let local_root = canonicalize_local_root(&local_path)?;
    let remote_root = normalize_remote_path(&device_path)?;
    let total_files = count_local_files(&local_root)?;
    let remote_directories = collect_remote_directories(&local_root, &remote_root)?;
    let directories_to_create = remote_directories
        .iter()
        .filter(|dir| normalize_remote_dir_path(dir.as_str()) != "/")
        .count();

    let device_info = detect_android_device()?;

    let mut created_dirs = HashSet::new();
    let mut stats = SyncStats::default();
    let mut progress = ProgressReporter::new(
        window,
        total_files.saturating_add(directories_to_create),
        dry_run,
    );

    create_remote_directories(
        &device_info,
        &remote_directories,
        dry_run,
        &mut created_dirs,
        &mut stats,
        &mut progress,
    )?;

    let mut adb_device = ADBUSBDevice::new(device_info.vendor_id, device_info.product_id)?;

    ensure_remote_dir(
        &mut adb_device,
        &remote_root,
        &mut created_dirs,
        &mut stats,
        dry_run,
    )?;
    sync_directory(
        &mut adb_device,
        &local_root,
        &local_root,
        &remote_root,
        &mut created_dirs,
        &mut stats,
        &mut progress,
        dry_run,
    )?;

    Ok(SyncSummary {
        device: device_info.into(),
        files_synced: stats.files_synced,
        files_deleted: stats.files_deleted,
        skipped_entries: stats.skipped_entries,
        directories_created: stats.directories_created,
        bytes_uploaded: stats.bytes_uploaded,
        remote_path: remote_root,
        local_root: local_root.display().to_string(),
        dry_run,
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
    progress: &mut ProgressReporter,
    dry_run: bool,
) -> Result<(), SyncError> {
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let entry_path = entry.path();
        let metadata = entry.metadata()?;

        if should_skip_entry(&entry_path) {
            stats.skipped_entries += 1;
            continue;
        }

        let relative_path = entry_path
            .strip_prefix(root)
            .unwrap_or_else(|_| Path::new(""));

        if metadata.is_dir() {
            let remote_dir = build_remote_path(remote_root, relative_path);
            ensure_remote_dir(device, &remote_dir, created_dirs, stats, dry_run)?;
            sync_directory(
                device,
                root,
                &entry_path,
                remote_root,
                created_dirs,
                stats,
                progress,
                dry_run,
            )?;
        } else if metadata.is_file() {
            let remote_file = build_remote_path(remote_root, relative_path);
            let parent = relative_path
                .parent()
                .map(|p| build_remote_path(remote_root, p))
                .unwrap_or_else(|| remote_root.to_string());
            ensure_remote_dir(device, &parent, created_dirs, stats, dry_run)?;
            push_file(device, &entry_path, &remote_file, &metadata, stats, dry_run)?;
            progress.file_processed(Some(remote_file.as_str()));
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
    metadata: &fs::Metadata,
    stats: &mut SyncStats,
    dry_run: bool,
) -> Result<(), SyncError> {
    if file_is_unchanged(device, remote_path, metadata)? {
        return Ok(());
    }

    if !dry_run {
        let mut file = File::open(local_path)?;
        device.push(&mut file, &remote_path)?;
    }
    stats.files_synced += 1;
    stats.bytes_uploaded += metadata.len();
    Ok(())
}

fn ensure_remote_dir(
    device: &mut ADBUSBDevice,
    remote_dir: &str,
    created_dirs: &mut HashSet<String>,
    stats: &mut SyncStats,
    dry_run: bool,
) -> Result<(), SyncError> {
    let normalized = normalize_remote_dir_path(remote_dir);

    if !created_dirs.insert(normalized.clone()) {
        return Ok(());
    }

    if normalized != "/" {
        if !dry_run {
            let mut sink = io::sink();
            device.shell_command(&["mkdir", "-p", normalized.as_str()], &mut sink)?;
        }
        stats.directories_created += 1;
    }

    Ok(())
}

fn normalize_remote_dir_path(path: &str) -> String {
    if path == "/" {
        "/".to_string()
    } else {
        path.trim_end_matches('/').to_string()
    }
}

fn collect_remote_directories(local_root: &Path, remote_root: &str) -> Result<Vec<String>, SyncError> {
    let mut directories = HashSet::new();
    directories.insert(normalize_remote_dir_path(remote_root));
    collect_remote_directories_recursive(local_root, local_root, remote_root, &mut directories)?;

    let mut list: Vec<_> = directories.into_iter().collect();
    list.sort_by(|a, b| {
        directory_depth(a.as_str())
            .cmp(&directory_depth(b.as_str()))
            .then_with(|| a.cmp(b))
    });
    Ok(list)
}

fn collect_remote_directories_recursive(
    root: &Path,
    current: &Path,
    remote_root: &str,
    directories: &mut HashSet<String>,
) -> Result<(), SyncError> {
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        if should_skip_entry(&path) {
            continue;
        }

        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            let relative = path
                .strip_prefix(root)
                .unwrap_or_else(|_| Path::new(""));
            let remote_dir = build_remote_path(remote_root, relative);
            directories.insert(normalize_remote_dir_path(remote_dir.as_str()));
            collect_remote_directories_recursive(root, &path, remote_root, directories)?;
        }
    }

    Ok(())
}

fn directory_depth(path: &str) -> usize {
    path.trim_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .count()
}

fn create_remote_directories(
    device_info: &AndroidDeviceInfo,
    directories: &[String],
    dry_run: bool,
    created_dirs: &mut HashSet<String>,
    stats: &mut SyncStats,
    progress: &mut ProgressReporter,
) -> Result<(), SyncError> {
    let needs_device = !dry_run
        && directories
            .iter()
            .any(|dir| normalize_remote_dir_path(dir.as_str()) != "/");

    let mut shell_device = if needs_device {
        Some(ADBUSBDevice::new(
            device_info.vendor_id,
            device_info.product_id,
        )?)
    } else {
        None
    };

    for dir in directories {
        let normalized = normalize_remote_dir_path(dir.as_str());
        if !created_dirs.insert(normalized.clone()) {
            continue;
        }

        if normalized == "/" {
            continue;
        }

        if let Some(device) = shell_device.as_mut() {
            if !dry_run {
                let mut sink = io::sink();
                device.shell_command(&["mkdir", "-p", normalized.as_str()], &mut sink)?;
            }
        }

        stats.directories_created += 1;
        progress.directory_prepared(normalized.as_str());
    }

    Ok(())
}

fn file_is_unchanged(
    device: &mut ADBUSBDevice,
    remote_path: &str,
    metadata: &fs::Metadata,
) -> Result<bool, SyncError> {
    let Some(remote) = remote_metadata(device, remote_path)? else {
        return Ok(false);
    };

    if u64::from(remote.file_size) != metadata.len() {
        return Ok(false);
    }

    Ok(true)
}

fn remote_metadata(
    device: &mut ADBUSBDevice,
    remote_path: &str,
) -> Result<Option<AdbStatResponse>, SyncError> {
    match device.stat(remote_path) {
        Ok(stat) => Ok(Some(stat)),
        Err(error) => match error {
            RustADBError::ADBRequestFailed(message) => {
                if adb_missing_file(&message) {
                    Ok(None)
                } else {
                    Err(SyncError::Adb(RustADBError::ADBRequestFailed(message)))
                }
            }
            other => Err(other.into()),
        },
    }
}

fn adb_missing_file(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("no such file")
        || lower.contains("not found")
        || lower.contains("does not exist")
        || lower.contains("failed to lstat")
        || lower.contains("failed to stat")
}

fn file_modified_seconds(metadata: &fs::Metadata) -> Option<u64> {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
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

fn count_local_files(root: &Path) -> Result<usize, SyncError> {
    let mut total = 0;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if should_skip_entry(&entry.path()) {
            continue;
        }
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            total += count_local_files(&entry.path())?;
        } else if metadata.is_file() {
            total += 1;
        }
    }
    Ok(total)
}

fn should_skip_entry(path: &Path) -> bool {
    path.file_name()
        .map(|name| name.to_string_lossy().starts_with('.'))
        .unwrap_or(false)
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
    files_deleted: usize,
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
