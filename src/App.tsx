import { useCallback, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import "./App.css";

type SyncSummary = {
  device: {
    vendor_id: number;
    product_id: number;
    manufacturer?: string | null;
    product?: string | null;
  };
  files_synced: number;
  skipped_entries: number;
  directories_created: number;
  bytes_uploaded: number;
  remote_path: string;
  local_root: string;
};

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
  const [syncing, setSyncing] = useState(false);
  const [status, setStatus] = useState("");
  const [error, setError] = useState("");
  const [summary, setSummary] = useState<SyncSummary | null>(null);

  const canSync = useMemo(() => {
    return (
      localPath.trim().length > 0 &&
      devicePath.trim().length > 0 &&
      !syncing
    );
  }, [devicePath, localPath, syncing]);

  const startSync = useCallback(async () => {
    if (!canSync) return;
    setSyncing(true);
    setError("");
    setStatus("Starting USB sync…");
    try {
      const result = await invoke<SyncSummary>("sync_folders", {
        localPath,
        devicePath,
      });
      setSummary(result);
      setStatus("Sync finished successfully.");
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
  }, [canSync, devicePath, localPath]);

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
        <label htmlFor="local-path">Local folder</label>
        <input
          id="local-path"
          value={localPath}
          placeholder="e.g. /Users/me/Documents/export"
          onChange={(event) => setLocalPath(event.currentTarget.value)}
        />

        <label htmlFor="device-path">Device folder</label>
        <input
          id="device-path"
          value={devicePath}
          placeholder="/sdcard/AndroidSync"
          onChange={(event) => setDevicePath(event.currentTarget.value)}
        />

        <button type="submit" disabled={!canSync}>
          {syncing ? "Syncing…" : "Start sync"}
        </button>
      </form>

      {status && <p className="status">{status}</p>}
      {error && <p className="error">{error}</p>}

      {summary && (
        <section className="summary">
          <h2>Last sync</h2>
          <ul>
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
