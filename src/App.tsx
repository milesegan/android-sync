import { useCallback, useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import "./App.css";

type SyncSummary = {
  device: {
    vendor_id: number;
    product_id: number;
    manufacturer?: string | null;
    product?: string | null;
  };
  files_synced: number;
  files_deleted: number;
  skipped_entries: number;
  directories_created: number;
  bytes_uploaded: number;
  remote_path: string;
  local_root: string;
  dry_run: boolean;
};

type SyncProgressEvent = {
  processed_files: number;
  total_files: number;
  current_file?: string | null;
  dry_run: boolean;
};

type SyncProgressState = {
  processed: number;
  total: number;
  currentFile: string | null;
  dryRun: boolean;
};

const LOCAL_PATH_STORAGE_KEY = "android-sync:lastLocalPath";
const PROGRESS_EVENT = "sync-progress";

const formatBytes = (bytes: number) => {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let value = bytes;
  let unitIndex = -1;
  while (value >= 1024 && unitIndex < units.length - 1) {
    value /= 1024;
    unitIndex += 1;
  }
  return `${value.toFixed(1)} ${units[unitIndex]}`;
};

const formatUsbId = (value: number) => value.toString(16).padStart(4, "0");

function App() {
  const [localPath, setLocalPath] = useState("");
  const [devicePath, setDevicePath] = useState("/sdcard/AndroidSync");
  const [dryRun, setDryRun] = useState(false);
  const [syncing, setSyncing] = useState(false);
  const [status, setStatus] = useState("");
  const [error, setError] = useState("");
  const [summary, setSummary] = useState<SyncSummary | null>(null);
  const [progress, setProgress] = useState<SyncProgressState | null>(null);

  const canSync = useMemo(() => {
    return (
      localPath.trim().length > 0 &&
      devicePath.trim().length > 0 &&
      !syncing
    );
  }, [devicePath, localPath, syncing]);

  const progressPercent = useMemo(() => {
    if (!progress || progress.total === 0) {
      return null;
    }
    const ratio = progress.total
      ? Math.min(progress.processed / progress.total, 1)
      : 0;
    return Math.round(ratio * 100);
  }, [progress]);

  useEffect(() => {
    if (typeof window === "undefined") return;
    try {
      const stored = window.localStorage.getItem(LOCAL_PATH_STORAGE_KEY);
      if (stored) {
        setLocalPath(stored);
      }
    } catch (storageError) {
      console.warn("Failed to restore last local path:", storageError);
    }
  }, []);

  useEffect(() => {
    if (typeof window === "undefined") return;
    try {
      if (localPath) {
        window.localStorage.setItem(LOCAL_PATH_STORAGE_KEY, localPath);
      } else {
        window.localStorage.removeItem(LOCAL_PATH_STORAGE_KEY);
      }
    } catch (storageError) {
      console.warn("Failed to persist local path:", storageError);
    }
  }, [localPath]);

  useEffect(() => {
    let cancelled = false;
    let unlisten: UnlistenFn | null = null;

    listen<SyncProgressEvent>(PROGRESS_EVENT, (event) => {
      const payload = event.payload;
      setProgress({
        processed: payload.processed_files,
        total: payload.total_files,
        currentFile: payload.current_file ?? null,
        dryRun: payload.dry_run,
      });
    })
      .then((fn) => {
        if (cancelled) {
          fn();
        } else {
          unlisten = fn;
        }
      })
      .catch((eventError) => {
        console.warn("Unable to listen for sync progress events:", eventError);
      });

    return () => {
      cancelled = true;
      if (unlisten) {
        unlisten();
      }
    };
  }, []);

  useEffect(() => {
    if (!syncing) {
      setProgress(null);
    }
  }, [syncing]);

  const pickLocalFolder = useCallback(async () => {
    try {
      const selection = await open({
        directory: true,
        multiple: false,
        defaultPath: localPath || undefined,
        title: "Choose a folder to sync",
      });
      if (typeof selection === "string") {
        setLocalPath(selection);
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }, [localPath]);

  const startSync = useCallback(async () => {
    if (!canSync) return;
    setSyncing(true);
    setError("");
    setProgress({
      processed: 0,
      total: 0,
      currentFile: null,
      dryRun,
    });
    setStatus(dryRun ? "Simulating sync…" : "Starting USB sync…");
    try {
      const result = await invoke<SyncSummary>("sync_folders", {
        localPath,
        devicePath,
        dryRun,
      });
      setSummary(result);
      setStatus(
        result.dry_run ? "Dry run finished successfully." : "Sync finished successfully."
      );
    } catch (err) {
      setSummary(null);
      setStatus("");
      if (err instanceof Error) {
        setError(err.message);
      } else {
        setError(String(err));
      }
    } finally {
      setSyncing(false);
    }
  }, [canSync, devicePath, dryRun, localPath]);

  return (
    <main className="container">
      <h1>Android USB Sync</h1>
      <p className="subtitle">
        Mirror a local folder onto an Android device through ADB over USB.
      </p>

      <form
        className="sync-form"
        onSubmit={(event) => {
          event.preventDefault();
          startSync();
        }}
      >
        <label id="local-path-label">Local folder</label>
        <div
          className="folder-picker"
          role="group"
          aria-labelledby="local-path-label"
          aria-live="polite"
        >
          <button
            type="button"
            className="folder-picker__button"
            onClick={pickLocalFolder}
          >
            Choose folder
          </button>
          <span
            className={
              localPath
                ? "folder-picker__path"
                : "folder-picker__path folder-picker__path--placeholder"
            }
          >
            {localPath || "No folder selected"}
          </span>
        </div>
        <p className="form-hint">
          Dotfiles such as <code>.DS_Store</code> are ignored automatically.
        </p>

        <label htmlFor="device-path">Device folder</label>
        <input
          id="device-path"
          value={devicePath}
          placeholder="/sdcard/AndroidSync"
          onChange={(event) => setDevicePath(event.currentTarget.value)}
        />

        <div className="dry-run-toggle">
          <p>
            <strong>Dry run:</strong> {dryRun ? "Enabled" : "Disabled"}
          </p>
          <button
            type="button"
            className={`dry-run-toggle__button${dryRun ? " dry-run-toggle__button--active" : ""}`}
            onClick={() => setDryRun((previous) => !previous)}
          >
            {dryRun ? "Disable dry run" : "Enable dry run"}
          </button>
        </div>

        <button type="submit" disabled={!canSync}>
          {syncing ? "Syncing…" : "Start sync"}
        </button>

        {syncing && (
          <div className="sync-progress" role="status" aria-live="polite">
            <div className="sync-progress__header">
              <strong>{dryRun ? "Simulating changes" : "Sync in progress"}</strong>
              {progressPercent !== null && (
                <span>{progressPercent}%</span>
              )}
            </div>
            <div
              className={`sync-progress__bar${progressPercent === null ? " sync-progress__bar--indeterminate" : ""}`}
              aria-hidden={progressPercent === null ? undefined : true}
            >
              <div
                className="sync-progress__bar-fill"
                style={
                  progressPercent === null
                    ? undefined
                    : { width: `${progressPercent}%` }
                }
              />
            </div>
            <p className="sync-progress__details">
              {progress && progress.total > 0
                ? `Processed ${progress.processed} of ${progress.total} files`
                : "Preparing file list…"}
            </p>
            {progress?.currentFile && (
              <code className="sync-progress__path">{progress.currentFile}</code>
            )}
          </div>
        )}
      </form>

      {status && <p className="status">{status}</p>}
      {error && <p className="error">{error}</p>}

      {summary && (
        <section className="summary">
          <h2>Last sync</h2>
          <ul>
            <li>
              <strong>Mode:</strong> {summary.dry_run ? "Dry run" : "Full sync"}
            </li>
            <li>
              <strong>Local:</strong> {summary.local_root}
            </li>
            <li>
              <strong>Device:</strong> {summary.remote_path}
            </li>
            <li>
              <strong>Files synced:</strong> {summary.files_synced}
            </li>
            <li>
              <strong>Files deleted:</strong> {summary.files_deleted}
            </li>
            <li>
              <strong>Directories touched:</strong>{" "}
              {summary.directories_created}
            </li>
            <li>
              <strong>Skipped entries:</strong> {summary.skipped_entries}
            </li>
            <li>
              <strong>Transferred:</strong> {formatBytes(summary.bytes_uploaded)}
            </li>
            <li>
              <strong>Device info:</strong>{" "}
              {summary.device.manufacturer ?? "Unknown vendor"} (
              {summary.device.product ?? "Unknown model"} @{" "}
              {formatUsbId(summary.device.vendor_id)}:
              {formatUsbId(summary.device.product_id)})
            </li>
          </ul>
        </section>
      )}
    </main>
  );
}

export default App;
