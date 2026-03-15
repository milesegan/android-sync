use adb_client::usb::{find_all_connected_adb_devices, ADBUSBDevice};
use adb_client::{ADBDeviceExt, AdbStatResponse, RustADBError};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io;
use std::path::{Component, Path, PathBuf};
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

    fn emit(&self, current_file: Option<&str>) {
        let payload = SyncProgressPayload {
            processed_files: self.processed_files,
            total_files: self.total_files,
            current_file: current_file.map(|value| value.to_string()),
            dry_run: self.dry_run,
        };
        let _ = self.window.emit(PROGRESS_EVENT, payload);
    }

    fn file_processed(&mut self, current_file: Option<&str>) {
        if self.processed_files >= self.total_files {
            self.total_files = self.total_files.saturating_add(1);
        }
        self.processed_files = self.processed_files.saturating_add(1);
        self.emit(current_file);
    }

    fn directory_prepared(&mut self, directory: &str) {
        self.emit(Some(directory));
    }

    fn finish(&mut self) {
        self.total_files = self.processed_files;
        self.emit(None);
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

#[derive(Debug)]
struct AndroidDeviceInfo {
    vendor_id: u16,
    product_id: u16,
    manufacturer: Option<String>,
    product: Option<String>,
}

impl AndroidDeviceInfo {
    fn from_adb_device(device: adb_client::usb::ADBDeviceInfo) -> Self {
        let (manufacturer, product) = split_device_description(device.device_description.as_str());
        Self {
            vendor_id: device.vendor_id,
            product_id: device.product_id,
            manufacturer,
            product,
        }
    }
}

struct LocalSyncPlan {
    directories: Vec<String>,
    files: Vec<LocalFileEntry>,
    skipped_entries: usize,
}

struct LocalFileEntry {
    local_path: PathBuf,
    remote_path: String,
    size_bytes: u64,
}

#[derive(Clone, Copy)]
struct RemoteFileMetadata {
    size_bytes: u64,
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
    let sync_plan = build_sync_plan(&local_root, &remote_root)?;
    let device_info = detect_android_device()?;
    let total_files = sync_plan.files.len();

    let mut stats = SyncStats {
        skipped_entries: sync_plan.skipped_entries,
        ..SyncStats::default()
    };
    let mut progress = ProgressReporter::new(window, total_files, dry_run);
    let mut created_dirs = HashSet::new();
    let mut shell_device = ADBUSBDevice::new(device_info.vendor_id, device_info.product_id)?;

    create_remote_directories(
        &mut shell_device,
        &sync_plan.directories,
        dry_run,
        &mut created_dirs,
        &mut stats,
        &mut progress,
    )?;

    let remote_files = collect_remote_file_manifest(&mut shell_device, &remote_root);
    drop(shell_device);

    let mut adb_device = ADBUSBDevice::new(device_info.vendor_id, device_info.product_id)?;

    sync_files(
        &device_info,
        &mut adb_device,
        &sync_plan.files,
        remote_files.as_ref(),
        &mut stats,
        &mut progress,
        dry_run,
    )?;

    progress.finish();

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

fn build_sync_plan(local_root: &Path, remote_root: &str) -> Result<LocalSyncPlan, SyncError> {
    let mut directories = HashSet::new();
    let mut files = Vec::new();
    let mut skipped_entries = 0;

    directories.insert(normalize_remote_dir_path(remote_root));
    collect_local_entries(
        local_root,
        local_root,
        remote_root,
        &mut directories,
        &mut files,
        &mut skipped_entries,
    )?;

    let mut directories: Vec<_> = directories.into_iter().collect();
    directories.sort_by(|a, b| {
        directory_depth(a.as_str())
            .cmp(&directory_depth(b.as_str()))
            .then_with(|| a.cmp(b))
    });

    Ok(LocalSyncPlan {
        directories,
        files,
        skipped_entries,
    })
}

fn collect_local_entries(
    root: &Path,
    current: &Path,
    remote_root: &str,
    directories: &mut HashSet<String>,
    files: &mut Vec<LocalFileEntry>,
    skipped_entries: &mut usize,
) -> Result<(), SyncError> {
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let entry_path = entry.path();

        if should_skip_entry(&entry_path) {
            *skipped_entries += 1;
            continue;
        }

        let metadata = entry.metadata()?;
        let relative_path = entry_path
            .strip_prefix(root)
            .unwrap_or_else(|_| Path::new(""));

        if metadata.is_dir() {
            let remote_dir = build_remote_path(remote_root, relative_path);
            directories.insert(normalize_remote_dir_path(remote_dir.as_str()));
            collect_local_entries(
                root,
                &entry_path,
                remote_root,
                directories,
                files,
                skipped_entries,
            )?;
        } else if metadata.is_file() {
            let remote_file = build_remote_path(remote_root, relative_path);
            files.push(LocalFileEntry {
                local_path: entry_path,
                remote_path: remote_file,
                size_bytes: metadata.len(),
            });
        } else {
            *skipped_entries += 1;
        }
    }

    Ok(())
}

fn sync_files(
    device_info: &AndroidDeviceInfo,
    device: &mut ADBUSBDevice,
    files: &[LocalFileEntry],
    remote_files: Option<&HashMap<String, RemoteFileMetadata>>,
    stats: &mut SyncStats,
    progress: &mut ProgressReporter,
    dry_run: bool,
) -> Result<(), SyncError> {
    for file in files {
        push_file(device_info, device, file, remote_files, stats, dry_run)?;
        progress.file_processed(Some(file.remote_path.as_str()));
    }

    Ok(())
}

fn push_file(
    device_info: &AndroidDeviceInfo,
    device: &mut ADBUSBDevice,
    file: &LocalFileEntry,
    remote_files: Option<&HashMap<String, RemoteFileMetadata>>,
    stats: &mut SyncStats,
    dry_run: bool,
) -> Result<(), SyncError> {
    if file_is_unchanged(device, file.remote_path.as_str(), file.size_bytes, remote_files)? {
        return Ok(());
    }

    if !dry_run {
        let mut local_file = File::open(file.local_path.as_path())?;
        match device.push(&mut local_file, &file.remote_path) {
            Ok(()) => {}
            Err(error)
                if push_completed_despite_protocol_error(&error, device_info, file)? => {}
            Err(error) => return Err(error.into()),
        }
    }

    stats.files_synced += 1;
    stats.bytes_uploaded += file.size_bytes;
    Ok(())
}

fn push_completed_despite_protocol_error(
    error: &RustADBError,
    device_info: &AndroidDeviceInfo,
    file: &LocalFileEntry,
) -> Result<bool, SyncError> {
    let RustADBError::WrongResponseReceived(actual, expected) = error else {
        return Ok(false);
    };

    if actual != "WRTE" || expected != "OKAY" {
        return Ok(false);
    }

    let mut verification_device = ADBUSBDevice::new(device_info.vendor_id, device_info.product_id)?;
    let Some(remote) = remote_metadata(&mut verification_device, file.remote_path.as_str())? else {
        return Ok(false);
    };

    Ok(u64::from(remote.file_size) == file.size_bytes)
}

fn create_remote_directories(
    device: &mut ADBUSBDevice,
    directories: &[String],
    dry_run: bool,
    created_dirs: &mut HashSet<String>,
    stats: &mut SyncStats,
    progress: &mut ProgressReporter,
) -> Result<(), SyncError> {
    let mut pending_directories = Vec::new();

    for dir in directories {
        let normalized = normalize_remote_dir_path(dir.as_str());
        if !created_dirs.insert(normalized.clone()) {
            continue;
        }

        if normalized == "/" {
            continue;
        }

        pending_directories.push(normalized.clone());
        stats.directories_created += 1;
        progress.directory_prepared(normalized.as_str());
    }

    if !dry_run {
        batch_create_remote_directories(device, &pending_directories)?;
    }

    Ok(())
}

fn batch_create_remote_directories(
    device: &mut ADBUSBDevice,
    directories: &[String],
) -> Result<(), SyncError> {
    const MAX_COMMAND_LEN: usize = 6_000;

    let mut command = String::from("mkdir -p");
    let mut has_pending = false;

    for directory in directories {
        let escaped = shell_escape_single_quotes(directory.as_str());
        if has_pending && command.len() + escaped.len() + 1 > MAX_COMMAND_LEN {
            let mut sink = io::sink();
            device.shell_command(&command, Some(&mut sink), None)?;
            command.clear();
            command.push_str("mkdir -p");
        }

        command.push(' ');
        command.push_str(&escaped);
        has_pending = true;
    }

    if has_pending {
        let mut sink = io::sink();
        device.shell_command(&command, Some(&mut sink), None)?;
    }

    Ok(())
}

fn collect_remote_file_manifest(
    device: &mut ADBUSBDevice,
    remote_root: &str,
) -> Option<HashMap<String, RemoteFileMetadata>> {
    let escaped_root = shell_escape_single_quotes(remote_root);
    let command = format!(
        "if [ -d {root} ]; then find {root} -type f -printf '%P\\t%s\\n'; fi",
        root = escaped_root
    );
    let mut output = Vec::new();

    if device
        .shell_command(&command, Some(&mut output), None)
        .is_err()
    {
        return None;
    }

    parse_remote_file_manifest(output.as_slice(), remote_root)
}

fn parse_remote_file_manifest(
    output: &[u8],
    remote_root: &str,
) -> Option<HashMap<String, RemoteFileMetadata>> {
    let mut files = HashMap::new();
    let listing = String::from_utf8_lossy(output);

    for line in listing.lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }

        let (relative_path, size_text) = trimmed.rsplit_once('\t')?;
        let size_bytes = size_text.parse::<u64>().ok()?;
        let remote_path = build_remote_path(remote_root, Path::new(relative_path));
        files.insert(remote_path, RemoteFileMetadata { size_bytes });
    }

    Some(files)
}

fn file_is_unchanged(
    device: &mut ADBUSBDevice,
    remote_path: &str,
    size_bytes: u64,
    remote_files: Option<&HashMap<String, RemoteFileMetadata>>,
) -> Result<bool, SyncError> {
    if let Some(remote_files) = remote_files {
        return Ok(remote_files
            .get(remote_path)
            .map(|remote| remote.size_bytes == size_bytes)
            .unwrap_or(false));
    }

    let Some(remote) = remote_metadata(device, remote_path)? else {
        return Ok(false);
    };

    Ok(u64::from(remote.file_size) == size_bytes)
}

fn remote_metadata(
    device: &mut ADBUSBDevice,
    remote_path: &str,
) -> Result<Option<AdbStatResponse>, SyncError> {
    match device.stat(&remote_path) {
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

fn normalize_remote_dir_path(path: &str) -> String {
    if path == "/" {
        "/".to_string()
    } else {
        path.trim_end_matches('/').to_string()
    }
}

fn directory_depth(path: &str) -> usize {
    path.trim_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .count()
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

fn should_skip_entry(path: &Path) -> bool {
    path.file_name()
        .map(|name| name.to_string_lossy().starts_with('.'))
        .unwrap_or(false)
}

fn detect_android_device() -> Result<AndroidDeviceInfo, SyncError> {
    let mut matches: Vec<_> = find_all_connected_adb_devices()?
        .into_iter()
        .map(AndroidDeviceInfo::from_adb_device)
        .collect();

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

fn split_device_description(description: &str) -> (Option<String>, Option<String>) {
    let trimmed = description.trim();
    if trimmed.is_empty() || trimmed == "Unknown device" {
        return (None, None);
    }

    match trimmed.split_once(' ') {
        Some((manufacturer, product)) => (
            Some(manufacturer.to_string()),
            Some(product.trim().to_string()),
        ),
        None => (Some(trimmed.to_string()), None),
    }
}

fn shell_escape_single_quotes(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
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
