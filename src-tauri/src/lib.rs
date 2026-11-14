mod adb_key;

use adb_client::usb::{find_all_connected_adb_devices, ADBUSBDevice};
use adb_client::{ADBDeviceExt, AdbStatResponse, RustADBError};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tauri::{webview::PageLoadEvent, Emitter, Manager, RunEvent, State, Window, WindowEvent};

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
    delete_extra: bool,
}

/// Why `sync_folders` did not return a summary. The frontend tells a user stop
/// apart from a failure by `kind`.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SyncFailure {
    Cancelled {
        dry_run: bool,
        files_synced: usize,
        files_deleted: usize,
        processed_files: usize,
        total_files: usize,
    },
    Failed {
        message: String,
    },
}

impl SyncFailure {
    fn failed(message: impl Into<String>) -> Self {
        SyncFailure::Failed {
            message: message.into(),
        }
    }
}

const PROGRESS_EVENT: &str = "sync-progress";
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(100);
#[cfg(target_os = "macos")]
const QUIT_MENU_ID: &str = "quit-android-sync";
const USB_AUTHORIZATION_HELP: &str =
    "Unlock your phone, start sync again, and tap Allow if “Allow USB debugging?” appears. If there is no prompt, reconnect the USB cable and try again.";

#[derive(Debug, Serialize, Clone)]
struct SyncProgressPayload {
    phase: SyncPhase,
    processed_files: usize,
    total_files: usize,
    current_file: Option<String>,
}

#[derive(Debug, Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum SyncPhase {
    Scanning,
    Connecting,
    CreatingDirectories,
    ReadingDeviceFiles,
    SyncingFiles,
    DeletingFiles,
}

struct ProgressReporter {
    window: Window,
    total_files: usize,
    processed_files: usize,
}

impl ProgressReporter {
    fn new(window: Window) -> Self {
        Self {
            window,
            total_files: 0,
            processed_files: 0,
        }
    }

    fn emit(&self, phase: SyncPhase, current_file: Option<&str>) {
        let payload = SyncProgressPayload {
            phase,
            processed_files: self.processed_files,
            total_files: self.total_files,
            current_file: current_file.map(|value| value.to_string()),
        };
        let _ = self.window.emit(PROGRESS_EVENT, payload);
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
    /// Phone paths of local entries that were not synced. Matching files on
    /// the phone are kept rather than treated as extra.
    skipped_remote_paths: Vec<String>,
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

/// Files and folders found under the phone folder, keyed by full phone path.
#[derive(Default)]
struct RemoteManifest {
    files: HashMap<String, RemoteFileMetadata>,
    directories: Vec<String>,
}

/// What deleting extra files would remove from the phone.
#[derive(Debug, Default, PartialEq)]
struct DeletionPlan {
    files: Vec<String>,
    /// Deepest first, so a folder comes before its parent.
    directories: Vec<String>,
}

#[derive(Default)]
struct SyncState {
    active: Option<Arc<AtomicBool>>,
    exiting: bool,
}

impl SyncState {
    fn request_exit(&mut self) -> bool {
        self.exiting = true;
        self.cancel()
    }

    fn cancel(&self) -> bool {
        if let Some(cancelled) = &self.active {
            cancelled.store(true, Ordering::Relaxed);
            return true;
        }
        false
    }
}

/// Marks an `io::Error` raised because the user stopped the sync, so it can be
/// recognised after passing through adb_client's error types.
#[derive(Debug)]
struct SyncCancelled;

impl std::fmt::Display for SyncCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Sync stopped")
    }
}

impl std::error::Error for SyncCancelled {}

fn cancelled_io_error() -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionAborted, SyncCancelled)
}

fn is_cancelled_io_error(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|inner| inner.is::<SyncCancelled>())
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<(), SyncError> {
    if cancelled.load(Ordering::Relaxed) {
        return Err(SyncError::Cancelled);
    }
    Ok(())
}

fn check_cancelled_io(cancelled: &AtomicBool) -> io::Result<()> {
    if cancelled.load(Ordering::Relaxed) {
        return Err(cancelled_io_error());
    }
    Ok(())
}

/// A USB handshake abandoned by a cancelled sync. It may still hold the device
/// until its read times out, so the next connection waits for it first.
static ABANDONED_CONNECTION: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);

struct SyncFileReader<'a> {
    file: File,
    cancelled: &'a AtomicBool,
}

impl Read for SyncFileReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        check_cancelled_io(self.cancelled)?;
        self.file.read(buffer)
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default()
        .manage(Mutex::new(SyncState::default()))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        // The window starts hidden so it does not flash a light background in
        // dark mode before the page has painted.
        .on_page_load(|webview, payload| {
            if payload.event() == PageLoadEvent::Finished {
                let _ = webview.window().show();
            }
        })
        .invoke_handler(tauri::generate_handler![sync_folders, cancel_sync]);

    #[cfg(target_os = "macos")]
    let builder = builder
        .menu(|app| {
            use tauri::menu::{Menu, MenuItem, MenuItemKind};

            let menu = Menu::default(app)?;
            if let Some(MenuItemKind::Submenu(app_menu)) = menu.items()?.first() {
                // Cocoa's default Quit action terminates without a Tauri ExitRequested event.
                if let Some(last_index) = app_menu.items()?.len().checked_sub(1) {
                    app_menu.remove_at(last_index)?;
                    app_menu.append(&MenuItem::with_id(
                        app,
                        QUIT_MENU_ID,
                        format!("Quit {}", app.package_info().name),
                        true,
                        Some("Cmd+Q"),
                    )?)?;
                }
            }
            Ok(menu)
        })
        .on_menu_event(|app, event| {
            if event.id() == QUIT_MENU_ID {
                app.exit(0);
            }
        });

    builder
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| match event {
            RunEvent::WindowEvent {
                event: WindowEvent::CloseRequested { api, .. },
                ..
            } => {
                if app
                    .state::<Mutex<SyncState>>()
                    .lock()
                    .unwrap()
                    .request_exit()
                {
                    api.prevent_close();
                }
            }
            RunEvent::ExitRequested { api, .. } => {
                if app
                    .state::<Mutex<SyncState>>()
                    .lock()
                    .unwrap()
                    .request_exit()
                {
                    api.prevent_exit();
                }
            }
            _ => {}
        });
}

#[tauri::command]
async fn sync_folders(
    window: Window,
    local_path: String,
    device_path: String,
    dry_run: bool,
    delete_extra: bool,
    state: State<'_, Mutex<SyncState>>,
) -> Result<SyncSummary, SyncFailure> {
    let cancelled = Arc::new(AtomicBool::new(false));
    {
        let mut state = state.lock().unwrap();
        if state.exiting {
            return Err(SyncFailure::failed("Android Sync is closing."));
        }
        if state.active.is_some() {
            return Err(SyncFailure::failed("A sync is already running."));
        }
        state.active = Some(Arc::clone(&cancelled));
    }

    let app = window.app_handle().clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        let mut progress = ProgressReporter::new(window);
        let mut stats = SyncStats::default();
        let result = perform_sync(
            &mut progress,
            &mut stats,
            local_path,
            device_path,
            dry_run,
            delete_extra,
            &cancelled,
        );
        result.map_err(|error| match error {
            SyncError::Cancelled => SyncFailure::Cancelled {
                dry_run,
                files_synced: stats.files_synced,
                files_deleted: stats.files_deleted,
                processed_files: progress.processed_files,
                total_files: progress.total_files,
            },
            error => SyncFailure::failed(error.to_string()),
        })
    })
    .await;

    let exiting = {
        let mut state = state.lock().unwrap();
        state.active = None;
        state.exiting
    };
    if exiting {
        app.exit(0);
    }
    result.map_err(|e| SyncFailure::failed(format!("sync task failed: {e}")))?
}

/// Asks the running sync to stop. Does nothing when no sync is running.
#[tauri::command]
fn cancel_sync(state: State<'_, Mutex<SyncState>>) {
    state.lock().unwrap().cancel();
}

fn perform_sync(
    progress: &mut ProgressReporter,
    stats: &mut SyncStats,
    local_path: String,
    device_path: String,
    dry_run: bool,
    delete_extra: bool,
    cancelled: &AtomicBool,
) -> Result<SyncSummary, SyncError> {
    progress.emit(SyncPhase::Scanning, None);
    let local_root = canonicalize_local_root(&local_path)?;
    let remote_root = normalize_remote_path(&device_path)?;
    if delete_extra && is_protected_remote_root(&remote_root) {
        return Err(SyncError::ProtectedDeleteRoot(remote_root));
    }
    let sync_plan = build_sync_plan(&local_root, &remote_root, cancelled)?;
    progress.total_files = sync_plan.files.len();
    stats.skipped_entries = sync_plan.skipped_entries;

    check_cancelled(cancelled)?;
    progress.emit(SyncPhase::Connecting, None);
    let device_info = detect_android_device()?;
    let app_data_dir = progress
        .window
        .path()
        .app_local_data_dir()
        .map_err(SyncError::AppData)?;
    let key_path = adb_key::adb_key_path(&app_data_dir).map_err(SyncError::AdbKey)?;
    let mut shell_device = connect_with_retry(&device_info, &key_path, cancelled)?;
    check_cancelled(cancelled)?;

    let mut created_dirs = HashSet::new();

    if !dry_run {
        progress.emit(SyncPhase::CreatingDirectories, None);
    }
    create_remote_directories(
        &mut shell_device,
        &sync_plan.directories,
        dry_run,
        &mut created_dirs,
        stats,
        cancelled,
    )?;

    progress.emit(SyncPhase::ReadingDeviceFiles, None);
    check_cancelled(cancelled)?;
    let remote_manifest = collect_remote_file_manifest(&mut shell_device, &remote_root)?;

    let mut adb_device = Some(shell_device);

    sync_files(
        &device_info,
        &key_path,
        &mut adb_device,
        &sync_plan.files,
        remote_manifest.as_ref().map(|manifest| &manifest.files),
        stats,
        progress,
        dry_run,
        cancelled,
    )?;

    if delete_extra {
        check_cancelled(cancelled)?;
        let remote_manifest = remote_manifest.ok_or(SyncError::UnreadableDeviceListing)?;
        let deletions = plan_deletions(&sync_plan, &remote_root, &remote_manifest);
        if !dry_run && !(deletions.files.is_empty() && deletions.directories.is_empty()) {
            progress.emit(SyncPhase::DeletingFiles, None);
            let device = adb_device.as_mut().expect("device should be present");
            delete_remote_entries(device, &deletions, stats, cancelled)
                .map_err(|error| SyncError::adb_operation("deleting extra files", error))?;
        } else {
            stats.files_deleted = deletions.files.len();
        }
    }

    progress.emit(SyncPhase::SyncingFiles, None);

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
        delete_extra,
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

fn build_sync_plan(
    local_root: &Path,
    remote_root: &str,
    cancelled: &AtomicBool,
) -> Result<LocalSyncPlan, SyncError> {
    let mut directories = HashSet::new();
    let mut files = Vec::new();
    let mut skipped_remote_paths = Vec::new();

    directories.insert(normalize_remote_dir_path(remote_root));
    collect_local_entries(
        local_root,
        local_root,
        remote_root,
        &mut directories,
        &mut files,
        &mut skipped_remote_paths,
        cancelled,
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
        skipped_entries: skipped_remote_paths.len(),
        skipped_remote_paths,
    })
}

fn collect_local_entries(
    root: &Path,
    current: &Path,
    remote_root: &str,
    directories: &mut HashSet<String>,
    files: &mut Vec<LocalFileEntry>,
    skipped_remote_paths: &mut Vec<String>,
    cancelled: &AtomicBool,
) -> Result<(), SyncError> {
    for entry in fs::read_dir(current)? {
        check_cancelled(cancelled)?;
        let entry = entry?;
        let entry_path = entry.path();
        let relative_path = entry_path
            .strip_prefix(root)
            .unwrap_or_else(|_| Path::new(""));

        if should_skip_entry(&entry_path) {
            skipped_remote_paths.push(build_remote_path(remote_root, relative_path));
            continue;
        }

        let metadata = entry.metadata()?;

        if metadata.is_dir() {
            let remote_dir = build_remote_path(remote_root, relative_path);
            directories.insert(normalize_remote_dir_path(remote_dir.as_str()));
            collect_local_entries(
                root,
                &entry_path,
                remote_root,
                directories,
                files,
                skipped_remote_paths,
                cancelled,
            )?;
        } else if metadata.is_file() {
            let remote_file = build_remote_path(remote_root, relative_path);
            files.push(LocalFileEntry {
                local_path: entry_path,
                remote_path: remote_file,
                size_bytes: metadata.len(),
            });
        } else {
            skipped_remote_paths.push(build_remote_path(remote_root, relative_path));
        }
    }

    Ok(())
}

fn sync_files(
    device_info: &AndroidDeviceInfo,
    key_path: &Path,
    device_opt: &mut Option<ADBUSBDevice>,
    files: &[LocalFileEntry],
    remote_files: Option<&HashMap<String, RemoteFileMetadata>>,
    stats: &mut SyncStats,
    progress: &mut ProgressReporter,
    dry_run: bool,
    cancelled: &AtomicBool,
) -> Result<(), SyncError> {
    for file in files {
        check_cancelled(cancelled)?;
        progress.emit(SyncPhase::SyncingFiles, Some(&file.remote_path));
        push_file(
            device_info,
            key_path,
            device_opt,
            file,
            remote_files,
            stats,
            dry_run,
            cancelled,
        )
        .map_err(|source| match source {
            SyncError::Cancelled => SyncError::Cancelled,
            source => SyncError::FileSync(file.remote_path.clone(), Box::new(source)),
        })?;
        progress.processed_files += 1;
    }

    Ok(())
}

fn push_file(
    device_info: &AndroidDeviceInfo,
    key_path: &Path,
    device_opt: &mut Option<ADBUSBDevice>,
    file: &LocalFileEntry,
    remote_files: Option<&HashMap<String, RemoteFileMetadata>>,
    stats: &mut SyncStats,
    dry_run: bool,
    cancelled: &AtomicBool,
) -> Result<(), SyncError> {
    let device = device_opt.as_mut().expect("device should be present");
    if file_is_unchanged(
        device,
        file.remote_path.as_str(),
        file.size_bytes,
        remote_files,
    )? {
        return Ok(());
    }

    if !dry_run {
        let mut reader = SyncFileReader {
            file: File::open(&file.local_path)?,
            cancelled,
        };
        if let Err(error) = device.push(&mut reader, &file.remote_path) {
            check_cancelled(cancelled)?;
            if !matches!(&error, RustADBError::WrongResponseReceived(actual, expected)
                if actual == "WRTE" && expected == "OKAY")
            {
                return Err(error.into());
            }

            drop(device_opt.take());
            let mut verification_device = connect_with_retry(device_info, key_path, cancelled)?;
            if !file_is_unchanged(
                &mut verification_device,
                &file.remote_path,
                file.size_bytes,
                None,
            )? {
                return Err(error.into());
            }
            *device_opt = Some(verification_device);
        }
    }

    stats.files_synced += 1;
    stats.bytes_uploaded += file.size_bytes;
    Ok(())
}

fn create_remote_directories(
    device: &mut ADBUSBDevice,
    directories: &[String],
    dry_run: bool,
    created_dirs: &mut HashSet<String>,
    stats: &mut SyncStats,
    cancelled: &AtomicBool,
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
    }

    if !dry_run {
        batch_create_remote_directories(device, &pending_directories, cancelled)
            .map_err(|error| SyncError::adb_operation("creating device folders", error))?;
    }

    Ok(())
}

fn batch_create_remote_directories(
    device: &mut ADBUSBDevice,
    directories: &[String],
    cancelled: &AtomicBool,
) -> Result<(), RustADBError> {
    for batch in shell_batches(directories) {
        check_cancelled_io(cancelled)?;
        let mut sink = io::sink();
        device.shell_command(&format!("mkdir -p {}", batch.join(" ")), Some(&mut sink), None)?;
    }
    Ok(())
}

fn collect_remote_file_manifest(
    device: &mut ADBUSBDevice,
    remote_root: &str,
) -> Result<Option<RemoteManifest>, SyncError> {
    let escaped_root = shell_escape_single_quotes(remote_root);
    let command = format!(
        "if [ -d {root} ]; then find {root} -type f -printf 'f\\t%P\\t%s\\n'; find {root} -mindepth 1 -type d -printf 'd\\t%P\\t0\\n'; fi",
        root = escaped_root
    );
    let mut output = Vec::new();

    device
        .shell_command(&command, Some(&mut output), None)
        .map_err(|error| SyncError::adb_operation("reading device files", error))?;

    Ok(parse_remote_file_manifest(output.as_slice(), remote_root))
}

/// Parses `find` output lines of the form `<f|d>\t<relative path>\t<size>`.
/// Returns `None` if any line is not in that form.
fn parse_remote_file_manifest(output: &[u8], remote_root: &str) -> Option<RemoteManifest> {
    let mut manifest = RemoteManifest::default();
    let listing = String::from_utf8_lossy(output);

    for line in listing.lines() {
        let trimmed = line.trim_end_matches('\r');
        if trimmed.is_empty() {
            continue;
        }

        let (kind, rest) = trimmed.split_once('\t')?;
        let (relative_path, size_text) = rest.rsplit_once('\t')?;
        let remote_path = build_remote_path(remote_root, Path::new(relative_path));
        match kind {
            "f" => {
                let size_bytes = size_text.parse::<u64>().ok()?;
                manifest
                    .files
                    .insert(remote_path, RemoteFileMetadata { size_bytes });
            }
            "d" => manifest.directories.push(remote_path),
            _ => return None,
        }
    }

    Some(manifest)
}

/// Phone folders where deleting everything that isn't on the Mac would wipe
/// shared storage or app data.
fn is_protected_remote_root(remote_root: &str) -> bool {
    const PROTECTED: &[&str] = &[
        "/sdcard/Android",
        "/storage/emulated",
        "/storage/emulated/0",
        "/storage/emulated/0/Android",
        "/storage/self",
        "/storage/self/primary",
        "/storage/self/primary/Android",
        "/mnt/sdcard",
        "/mnt/sdcard/Android",
        "/data/local",
        "/data/local/tmp",
    ];
    directory_depth(remote_root) <= 1
        || PROTECTED
            .iter()
            .any(|protected| protected.eq_ignore_ascii_case(remote_root))
}

/// Works out which files and folders on the phone have no counterpart on the
/// Mac. Names are compared without case, because both the Mac and Android
/// shared storage usually treat names that differ only in case as the same
/// file. Hidden entries and anything matching a skipped local entry are kept.
fn plan_deletions(
    plan: &LocalSyncPlan,
    remote_root: &str,
    manifest: &RemoteManifest,
) -> DeletionPlan {
    let keep: HashSet<String> = plan
        .files
        .iter()
        .map(|file| file.remote_path.to_lowercase())
        .chain(plan.directories.iter().map(|dir| dir.to_lowercase()))
        .collect();
    let skipped: HashSet<String> = plan
        .skipped_remote_paths
        .iter()
        .map(|path| path.to_lowercase())
        .collect();
    let should_delete = |path: &String| {
        let lower = path.to_lowercase();
        !keep.contains(&lower)
            && !is_hidden_below(path, remote_root)
            && !is_at_or_below_any(&lower, &skipped)
    };

    let mut files: Vec<String> = manifest.files.keys().filter(|path| should_delete(path)).cloned().collect();
    files.sort();
    let mut directories: Vec<String> = manifest
        .directories
        .iter()
        .filter(|path| should_delete(path))
        .cloned()
        .collect();
    directories.sort_by(|a, b| {
        directory_depth(b)
            .cmp(&directory_depth(a))
            .then_with(|| a.cmp(b))
    });

    DeletionPlan { files, directories }
}

fn is_hidden_below(path: &str, remote_root: &str) -> bool {
    let relative = path
        .strip_prefix(remote_root.trim_end_matches('/'))
        .unwrap_or(path);
    relative
        .split('/')
        .any(|component| component.starts_with('.'))
}

fn is_at_or_below_any(path: &str, ancestors: &HashSet<String>) -> bool {
    let mut current = path;
    loop {
        if ancestors.contains(current) {
            return true;
        }
        match current.rfind('/') {
            Some(index) if index > 0 => current = &current[..index],
            _ => return false,
        }
    }
}

fn delete_remote_entries(
    device: &mut ADBUSBDevice,
    deletions: &DeletionPlan,
    stats: &mut SyncStats,
    cancelled: &AtomicBool,
) -> Result<(), RustADBError> {
    for batch in shell_batches(&deletions.files) {
        check_cancelled_io(cancelled)?;
        let command = format!("rm -f {}", batch.join(" "));
        let mut sink = io::sink();
        let status = device.shell_command(&command, Some(&mut sink), Some(&mut io::sink()))?;
        if let Some(code) = status.filter(|code| *code != 0) {
            return Err(RustADBError::IOError(io::Error::other(format!(
                "rm exited with status {code}"
            ))));
        }
        stats.files_deleted += batch.len();
    }

    // Folders that still hold hidden files fail to delete and are left alone.
    for batch in shell_batches(&deletions.directories) {
        check_cancelled_io(cancelled)?;
        let command = format!("rmdir {} 2>/dev/null; true", batch.join(" "));
        let mut sink = io::sink();
        device.shell_command(&command, Some(&mut sink), None)?;
    }

    Ok(())
}

/// Splits paths into shell-quoted groups that keep each command short.
fn shell_batches(paths: &[String]) -> Vec<Vec<String>> {
    const MAX_COMMAND_LEN: usize = 6_000;
    let mut batches = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut length = 0;
    for path in paths {
        let escaped = shell_escape_single_quotes(path);
        if !current.is_empty() && length + escaped.len() + 1 > MAX_COMMAND_LEN {
            batches.push(std::mem::take(&mut current));
            length = 0;
        }
        length += escaped.len() + 1;
        current.push(escaped);
    }
    if !current.is_empty() {
        batches.push(current);
    }
    batches
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

/// Opens an ADB connection without blocking cancellation. The USB handshake can
/// wait several seconds for the phone, so it runs on its own thread while this
/// thread watches the cancel flag.
fn connect_with_retry(
    device: &AndroidDeviceInfo,
    key_path: &Path,
    cancelled: &AtomicBool,
) -> Result<ADBUSBDevice, SyncError> {
    wait_for_abandoned_connection(cancelled)?;

    let (vendor_id, product_id) = (device.vendor_id, device.product_id);
    let key_path = key_path.to_path_buf();
    let (sender, receiver) = mpsc::channel();
    let handle = thread::spawn(move || {
        // If the sync was cancelled the receiver is gone, and the device is
        // dropped here, which releases USB.
        let _ = sender.send(open_device(vendor_id, product_id, &key_path));
    });

    loop {
        match receiver.recv_timeout(CANCEL_POLL_INTERVAL) {
            Ok(result) => {
                let _ = handle.join();
                return result;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if cancelled.load(Ordering::Relaxed) {
                    *ABANDONED_CONNECTION.lock().unwrap() = Some(handle);
                    return Err(SyncError::Cancelled);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = handle.join();
                return Err(SyncError::Io(io::Error::other(
                    "the USB connection thread stopped unexpectedly",
                )));
            }
        }
    }
}

fn wait_for_abandoned_connection(cancelled: &AtomicBool) -> Result<(), SyncError> {
    let Some(handle) = ABANDONED_CONNECTION.lock().unwrap().take() else {
        return Ok(());
    };
    while !handle.is_finished() {
        if cancelled.load(Ordering::Relaxed) {
            *ABANDONED_CONNECTION.lock().unwrap() = Some(handle);
            return Err(SyncError::Cancelled);
        }
        thread::sleep(CANCEL_POLL_INTERVAL);
    }
    let _ = handle.join();
    Ok(())
}

fn open_device(
    vendor_id: u16,
    product_id: u16,
    key_path: &Path,
) -> Result<ADBUSBDevice, SyncError> {
    let mut retries = 5;
    loop {
        match ADBUSBDevice::new_with_custom_private_key(vendor_id, product_id, key_path) {
            Ok(device) => return Ok(device),
            Err(error) if is_usb_claimed_elsewhere(&error) && retries > 0 => {
                thread::sleep(Duration::from_millis(100));
                retries -= 1;
            }
            Err(error) => return Err(SyncError::AdbConnection(error)),
        }
    }
}

/// libusb reports a USB interface held by another process (usually an ADB
/// server) as busy on Linux and as access denied on macOS.
fn is_usb_claimed_elsewhere(error: &RustADBError) -> bool {
    matches!(
        error,
        RustADBError::DeviceBusy | RustADBError::UsbError(rusb::Error::Access)
    )
}

fn is_pending_usb_authorization(error: &RustADBError) -> bool {
    match error {
        RustADBError::WrongResponseReceived(actual, expected) => {
            actual == "AUTH" && expected == "CNXN"
        }
        // adb_client reports a late authentication packet as a failed session open.
        RustADBError::ADBRequestFailed(message) => {
            message == "Open session failed: got AUTH in response instead of OKAY"
        }
        _ => false,
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
    /// The user stopped the sync, or the app is quitting.
    Cancelled,
    InvalidLocalPath(String),
    InvalidRemotePath(String),
    DeviceNotFound,
    MultipleDevices(Vec<(u16, u16)>),
    Usb(rusb::Error),
    AdbKey(io::Error),
    AppData(tauri::Error),
    AdbConnection(RustADBError),
    AdbOperation(&'static str, RustADBError),
    FileSync(String, Box<SyncError>),
    ProtectedDeleteRoot(String),
    UnreadableDeviceListing,
    Adb(RustADBError),
    Io(io::Error),
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncError::Cancelled => write!(f, "Sync stopped"),
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
            SyncError::AdbKey(err) => write!(f, "Could not prepare the USB debugging key. {err}"),
            SyncError::AppData(err) => write!(f, "Could not find the app data folder. {err}"),
            SyncError::AdbConnection(RustADBError::UsbError(rusb::Error::Timeout)) => write!(
                f,
                "Android did not respond while connecting over USB. {USB_AUTHORIZATION_HELP}"
            ),
            SyncError::AdbConnection(RustADBError::IOError(err))
                if err.kind() == io::ErrorKind::TimedOut =>
            {
                write!(f, "Android did not respond while connecting over USB. {USB_AUTHORIZATION_HELP}")
            }
            SyncError::AdbConnection(err) if is_usb_claimed_elsewhere(err) => write!(
                f,
                "Another app is using the Android USB debugging connection. Close it and try again. If an ADB server is running, stop it with adb kill-server."
            ),
            SyncError::AdbConnection(err) | SyncError::AdbOperation(_, err) | SyncError::Adb(err)
                if is_pending_usb_authorization(err) =>
            {
                write!(f, "Android is waiting for USB debugging approval. {USB_AUTHORIZATION_HELP}")
            }
            SyncError::AdbConnection(err) => write!(f, "Could not connect to Android over USB. {err}"),
            SyncError::AdbOperation(operation, err) => write!(f, "ADB error while {operation}. {err}"),
            SyncError::FileSync(path, err) => write!(f, "Could not sync {path}. {err}"),
            SyncError::ProtectedDeleteRoot(path) => write!(
                f,
                "Deleting extra files isn't allowed in {path}, because it would remove files that belong to other apps. Choose a folder inside it, or turn off Delete extra files."
            ),
            SyncError::UnreadableDeviceListing => write!(
                f,
                "Files were copied, but the list of files on your phone could not be read, so nothing was deleted."
            ),
            SyncError::Adb(err) => write!(f, "ADB error: {err}"),
            SyncError::Io(err) => write!(f, "File system error: {err}"),
        }
    }
}

impl std::error::Error for SyncError {}

impl SyncError {
    fn adb_operation(operation: &'static str, error: RustADBError) -> Self {
        match SyncError::from(error) {
            SyncError::Adb(error) => SyncError::AdbOperation(operation, error),
            other => other,
        }
    }
}

impl From<rusb::Error> for SyncError {
    fn from(value: rusb::Error) -> Self {
        SyncError::Usb(value)
    }
}

impl From<RustADBError> for SyncError {
    fn from(value: RustADBError) -> Self {
        match value {
            RustADBError::IOError(error) if is_cancelled_io_error(&error) => SyncError::Cancelled,
            other => SyncError::Adb(other),
        }
    }
}

impl From<io::Error> for SyncError {
    fn from(value: io::Error) -> Self {
        if is_cancelled_io_error(&value) {
            SyncError::Cancelled
        } else {
            SyncError::Io(value)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_reader_error_is_a_cancellation_through_adb_errors() {
        let error = SyncError::from(RustADBError::IOError(cancelled_io_error()));
        assert!(matches!(error, SyncError::Cancelled));

        let error = SyncError::adb_operation(
            "creating device folders",
            RustADBError::IOError(cancelled_io_error()),
        );
        assert!(matches!(error, SyncError::Cancelled));
    }

    #[test]
    fn other_aborted_connections_stay_failures() {
        let aborted = io::Error::new(io::ErrorKind::ConnectionAborted, "Sync stopped");
        assert!(matches!(SyncError::from(aborted), SyncError::Io(_)));
    }

    #[test]
    fn cancel_without_a_running_sync_is_a_no_op() {
        let state = SyncState::default();
        assert!(!state.cancel());
        assert!(!state.exiting);
    }

    #[test]
    fn cancel_sets_the_flag_without_exiting() {
        let flag = Arc::new(AtomicBool::new(false));
        let state = SyncState {
            active: Some(Arc::clone(&flag)),
            exiting: false,
        };
        assert!(state.cancel());
        assert!(flag.load(Ordering::Relaxed));
        assert!(!state.exiting);
    }

    #[test]
    fn usb_access_denied_explains_the_other_app() {
        for error in [RustADBError::DeviceBusy, RustADBError::UsbError(rusb::Error::Access)] {
            let message = SyncError::AdbConnection(error).to_string();
            assert!(message.starts_with("Another app is using"), "{message}");
        }
    }

    fn local_plan(root: &str, files: &[&str], dirs: &[&str], skipped: &[&str]) -> LocalSyncPlan {
        let path = |p: &str| format!("{root}/{p}");
        LocalSyncPlan {
            directories: std::iter::once(root.to_string())
                .chain(dirs.iter().map(|d| path(d)))
                .collect(),
            files: files
                .iter()
                .map(|f| LocalFileEntry {
                    local_path: PathBuf::from(f),
                    remote_path: path(f),
                    size_bytes: 1,
                })
                .collect(),
            skipped_entries: skipped.len(),
            skipped_remote_paths: skipped.iter().map(|s| path(s)).collect(),
        }
    }

    #[test]
    fn parses_files_and_folders_from_find() {
        let output = b"f\tAlbum/01 Song.flac\t1234\nd\tAlbum\t0\nf\tname\twith tab.mp3\t5\n";
        let manifest = parse_remote_file_manifest(output, "/sdcard/Music").unwrap();
        assert_eq!(manifest.files["/sdcard/Music/Album/01 Song.flac"].size_bytes, 1234);
        assert_eq!(manifest.files["/sdcard/Music/name\twith tab.mp3"].size_bytes, 5);
        assert_eq!(manifest.directories, ["/sdcard/Music/Album"]);
        assert!(parse_remote_file_manifest(b"garbage\n", "/sdcard/Music").is_none());
    }

    #[test]
    fn plans_deleting_only_what_is_not_on_the_mac() {
        let root = "/sdcard/Music";
        let plan = local_plan(root, &["A/song.mp3", "Keep.mp3"], &["A"], &["Linked", ".DS_Store"]);
        let mut manifest = RemoteManifest::default();
        for file in [
            "A/song.mp3",
            "A/old.mp3",
            "keep.MP3",
            "Gone/x.mp3",
            ".nomedia",
            "A/.thumbs/t.jpg",
            "Linked/inside.mp3",
        ] {
            manifest
                .files
                .insert(format!("{root}/{file}"), RemoteFileMetadata { size_bytes: 1 });
        }
        manifest.directories = vec![
            format!("{root}/A"),
            format!("{root}/Gone"),
            format!("{root}/Gone/Deeper"),
            format!("{root}/A/.thumbs"),
            format!("{root}/Linked"),
        ];

        assert_eq!(
            plan_deletions(&plan, root, &manifest),
            DeletionPlan {
                files: vec![format!("{root}/A/old.mp3"), format!("{root}/Gone/x.mp3")],
                directories: vec![format!("{root}/Gone/Deeper"), format!("{root}/Gone")],
            }
        );
    }

    #[test]
    fn refuses_to_delete_in_storage_roots() {
        for root in ["/", "/sdcard", "/storage/emulated/0", "/sdcard/android", "/data"] {
            assert!(is_protected_remote_root(root), "{root}");
        }
        for root in ["/sdcard/Music", "/storage/emulated/0/Podcasts", "/sdcard/Android/media"] {
            assert!(!is_protected_remote_root(root), "{root}");
        }
    }

    #[test]
    fn shell_batches_quote_and_split_long_lists() {
        let paths: Vec<String> = (0..2000).map(|i| format!("/sdcard/it's {i}")).collect();
        let batches = shell_batches(&paths);
        assert!(batches.len() > 1);
        assert!(batches.iter().all(|b| b.iter().map(|p| p.len() + 1).sum::<usize>() <= 6_000));
        assert_eq!(batches.iter().map(Vec::len).sum::<usize>(), 2000);
        assert_eq!(batches[0][0], "'/sdcard/it'\\''s 0'");
    }

    #[test]
    fn failure_serialises_with_a_kind() {
        let value = serde_json::to_value(SyncFailure::Cancelled {
            dry_run: false,
            files_synced: 3,
            files_deleted: 0,
            processed_files: 5,
            total_files: 9,
        })
        .unwrap();
        assert_eq!(value["kind"], "cancelled");
        let value = serde_json::to_value(SyncFailure::failed("boom")).unwrap();
        assert_eq!(value["kind"], "failed");
        assert_eq!(value["message"], "boom");
    }
}
