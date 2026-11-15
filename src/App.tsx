import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { homeDir } from "@tauri-apps/api/path";
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
  delete_extra: boolean;
};

/** Error value of `sync_folders`, see `SyncFailure` in lib.rs. */
type SyncFailure =
  | {
      kind: "cancelled";
      dry_run: boolean;
      files_synced: number;
      files_deleted: number;
      processed_files: number;
      total_files: number;
    }
  | { kind: "failed"; message: string };

type StoppedInfo = Extract<SyncFailure, { kind: "cancelled" }>;

type SyncPhase =
  | "scanning"
  | "connecting"
  | "creating_directories"
  | "reading_device_files"
  | "syncing_files"
  | "publishing_media"
  | "deleting_files";

type SyncProgress = {
  phase: SyncPhase;
  processed_files: number;
  total_files: number;
  current_file: string | null;
};

type Outcome =
  | { kind: "complete"; summary: SyncSummary }
  | { kind: "stopped"; info: StoppedInfo }
  | { kind: "error"; message: string };

const LOCAL_PATH_STORAGE_KEY = "android-sync:lastLocalPath";
const DEVICE_PATH_STORAGE_KEY = "android-sync:lastDevicePath";
const DELETE_EXTRA_STORAGE_KEY = "android-sync:deleteExtra";
const DEFAULT_DEVICE_PATH = "/sdcard/AndroidSync";
const PROGRESS_EVENT = "sync-progress";

const PHASE_HEADINGS: Record<SyncPhase, string> = {
  scanning: "Scanning files",
  connecting: "Connecting to your phone",
  creating_directories: "Creating folders",
  reading_device_files: "Reading files on your phone",
  syncing_files: "Copying files",
  publishing_media: "Making media available",
  deleting_files: "Deleting extra files",
};

const PHASE_DETAILS: Partial<Record<SyncPhase, string>> = {
  connecting: "Unlock your phone and tap Allow if “Allow USB debugging?” appears.",
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

const plural = (count: number, word: string) =>
  `${count.toLocaleString()} ${word}${count === 1 ? "" : "s"}`;

const readStored = (key: string, fallback: string) => {
  try {
    return window.localStorage.getItem(key) || fallback;
  } catch (storageError) {
    console.warn(`Failed to restore ${key}:`, storageError);
    return fallback;
  }
};

const writeStored = (key: string, value: string) => {
  try {
    if (value) {
      window.localStorage.setItem(key, value);
    } else {
      window.localStorage.removeItem(key);
    }
  } catch (storageError) {
    console.warn(`Failed to persist ${key}:`, storageError);
  }
};

const toFailure = (err: unknown): SyncFailure => {
  if (err && typeof err === "object" && "kind" in err) {
    return err as SyncFailure;
  }
  return {
    kind: "failed",
    message: err instanceof Error ? err.message : String(err),
  };
};

/** Splits a path into a part that may be truncated and a last segment that stays visible. */
const splitPath = (path: string, home: string | null) => {
  let display = path;
  if (home) {
    const trimmedHome = home.replace(/\/+$/, "");
    if (display === trimmedHome || display.startsWith(`${trimmedHome}/`)) {
      display = `~${display.slice(trimmedHome.length)}`;
    }
  }
  const index = display.lastIndexOf("/");
  if (index <= 0) return { head: "", tail: display };
  return { head: display.slice(0, index), tail: display.slice(index) };
};

/** Strips the phone folder from a remote file path so the part that differs stays visible. */
const relativeToRoot = (file: string, root: string) => {
  const trimmedRoot = `/${root.trim().split("/").filter(Boolean).join("/")}`;
  return file.startsWith(`${trimmedRoot}/`) ? file.slice(trimmedRoot.length + 1) : file;
};

function App() {
  const [localPath, setLocalPath] = useState(() => readStored(LOCAL_PATH_STORAGE_KEY, ""));
  const [devicePath, setDevicePath] = useState(() =>
    readStored(DEVICE_PATH_STORAGE_KEY, DEFAULT_DEVICE_PATH)
  );
  const [dryRun, setDryRun] = useState(false);
  const [deleteExtra, setDeleteExtra] = useState(
    () => readStored(DELETE_EXTRA_STORAGE_KEY, "true") !== "false"
  );
  const [syncing, setSyncing] = useState(false);
  const [stopping, setStopping] = useState(false);
  const [progress, setProgress] = useState<SyncProgress | null>(null);
  const [outcome, setOutcome] = useState<Outcome | null>(null);
  const [home, setHome] = useState<string | null>(null);
  const devicePathInput = useRef<HTMLInputElement>(null);

  useEffect(() => writeStored(LOCAL_PATH_STORAGE_KEY, localPath), [localPath]);
  useEffect(() => writeStored(DEVICE_PATH_STORAGE_KEY, devicePath), [devicePath]);
  useEffect(() => writeStored(DELETE_EXTRA_STORAGE_KEY, String(deleteExtra)), [deleteExtra]);

  useEffect(() => {
    homeDir()
      .then(setHome)
      .catch(() => setHome(null));
  }, []);

  useEffect(() => {
    let disposed = false;
    let unlisten: UnlistenFn | null = null;

    listen<SyncProgress>(PROGRESS_EVENT, (event) => {
      setProgress(event.payload);
    })
      .then((fn) => {
        if (disposed) {
          fn();
        } else {
          unlisten = fn;
        }
      })
      .catch((eventError) => {
        console.warn("Unable to listen for sync progress events:", eventError);
      });

    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  const hasLocalPath = localPath.trim().length > 0;
  const hasDevicePath = devicePath.trim().length > 0;

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
        setOutcome(null);
      }
    } catch (err) {
      setOutcome({ kind: "error", message: err instanceof Error ? err.message : String(err) });
    }
  }, [localPath]);

  const startSync = useCallback(async () => {
    if (syncing || !hasLocalPath || !hasDevicePath) return;
    setSyncing(true);
    setStopping(false);
    setOutcome(null);
    setProgress(null);
    try {
      const summary = await invoke<SyncSummary>("sync_folders", {
        localPath,
        devicePath,
        dryRun,
        deleteExtra,
      });
      setOutcome({ kind: "complete", summary });
    } catch (err) {
      const failure = toFailure(err);
      setOutcome(
        failure.kind === "cancelled"
          ? { kind: "stopped", info: failure }
          : { kind: "error", message: failure.message }
      );
    } finally {
      setSyncing(false);
      setStopping(false);
      setProgress(null);
    }
  }, [deleteExtra, devicePath, dryRun, hasDevicePath, hasLocalPath, localPath, syncing]);

  const cancelSync = useCallback(async () => {
    setStopping(true);
    try {
      await invoke("cancel_sync");
    } catch (err) {
      console.warn("Could not stop the sync:", err);
      setStopping(false);
    }
  }, []);

  const localParts = splitPath(localPath, home);

  return (
    <main className="app">
      <fieldset className="settings" disabled={syncing}>
        <legend className="visually-hidden">Sync settings</legend>

        <div className="route">
          <div className="route__row">
            <FolderIcon />
            <div className="route__body">
              <span className="route__label" id="local-path-label">
                From this Mac
              </span>
              {hasLocalPath ? (
                <span
                  className="route__path"
                  title={localPath}
                  aria-labelledby="local-path-label"
                >
                  {localParts.head && <span className="path-head">{localParts.head}</span>}
                  <span className="path-tail">{localParts.tail}</span>
                </span>
              ) : (
                <span className="route__empty">No folder selected</span>
              )}
            </div>
            <button
              type="button"
              className="link-button"
              onClick={pickLocalFolder}
              aria-label={hasLocalPath ? "Change folder on this Mac" : "Choose folder on this Mac"}
            >
              {hasLocalPath ? "Change" : "Choose"}
            </button>
          </div>

          <div className="route__row">
            <PhoneIcon />
            <div className="route__body">
              <label className="route__label" htmlFor="device-path">
                To your phone
              </label>
              <div className="route__edit">
                <input
                  id="device-path"
                  ref={devicePathInput}
                  className="route__input"
                  value={devicePath}
                  placeholder={DEFAULT_DEVICE_PATH}
                  spellCheck={false}
                  autoCapitalize="off"
                  autoCorrect="off"
                  title={devicePath}
                  onChange={(event) => {
                    setDevicePath(event.currentTarget.value);
                    setOutcome(null);
                  }}
                />
                <PencilIcon />
              </div>
            </div>
          </div>
        </div>

        <div className="options">
          <SwitchRow
            id="delete-extra"
            title="Delete extra files"
            hint="Remove files on your phone that aren't on this Mac"
            checked={deleteExtra}
            onChange={(value) => {
              setDeleteExtra(value);
              setOutcome(null);
            }}
          />
          <SwitchRow
            id="preview"
            title="Preview only"
            hint="List changes without copying or deleting"
            checked={dryRun}
            onChange={(value) => {
              setDryRun(value);
              setOutcome(null);
            }}
          />
        </div>
      </fieldset>

      <section className="bottom">
        {syncing ? (
          <SyncingPanel
            progress={progress}
            devicePath={devicePath}
            dryRun={dryRun}
            stopping={stopping}
            onCancel={cancelSync}
          />
        ) : (
          <>
            <div className="bottom__body">
              {outcome?.kind === "complete" && <CompletePanel summary={outcome.summary} />}
              {outcome?.kind === "stopped" && <StoppedNotice info={outcome.info} />}
              {outcome?.kind === "error" && (
                <div className="notice notice--danger" role="alert">
                  <AlertIcon />
                  <div>
                    <p className="notice__title">Sync failed</p>
                    <p className="notice__text">{outcome.message}</p>
                  </div>
                </div>
              )}
              {!outcome && (!hasLocalPath || !hasDevicePath) && (
                <p className="hint">
                  {!hasLocalPath
                    ? "Choose a folder on this Mac to sync."
                    : "Enter a folder on your phone to sync to."}
                </p>
              )}
            </div>
            {!hasLocalPath ? (
              <button type="button" className="button button--primary" onClick={pickLocalFolder}>
                Choose folder
              </button>
            ) : !hasDevicePath ? (
              <button
                type="button"
                className="button button--primary"
                onClick={() => devicePathInput.current?.focus()}
              >
                Enter phone folder
              </button>
            ) : (
              <button type="button" className="button button--primary" onClick={startSync}>
                {outcome?.kind === "complete"
                  ? dryRun
                    ? "Preview again"
                    : "Sync again"
                  : dryRun
                    ? "Preview changes"
                    : "Start sync"}
              </button>
            )}
          </>
        )}
      </section>
    </main>
  );
}

function SwitchRow({
  id,
  title,
  hint,
  checked,
  onChange,
}: {
  id: string;
  title: string;
  hint: string;
  checked: boolean;
  onChange: (value: boolean) => void;
}) {
  return (
    <div className="option">
      <div className="option__text">
        <span className="option__title" id={`${id}-label`}>
          {title}
        </span>
        <span className="option__hint" id={`${id}-hint`}>
          {hint}
        </span>
      </div>
      <button
        type="button"
        role="switch"
        className="switch"
        aria-checked={checked}
        aria-labelledby={`${id}-label`}
        aria-describedby={`${id}-hint`}
        onClick={() => onChange(!checked)}
      >
        <span className="switch__thumb" />
      </button>
    </div>
  );
}

function SyncingPanel({
  progress,
  devicePath,
  dryRun,
  stopping,
  onCancel,
}: {
  progress: SyncProgress | null;
  devicePath: string;
  dryRun: boolean;
  stopping: boolean;
  onCancel: () => void;
}) {
  const phase = progress?.phase ?? "scanning";
  const counting = phase === "syncing_files" && (progress?.total_files ?? 0) > 0;
  const percent = counting
    ? Math.round((progress!.processed_files / progress!.total_files) * 100)
    : null;
  const heading =
    phase === "syncing_files" && dryRun ? "Checking files" : PHASE_HEADINGS[phase];
  const detail = counting
    ? `${progress!.processed_files.toLocaleString()} of ${plural(progress!.total_files, "file")}`
    : PHASE_DETAILS[phase];
  const currentFile =
    phase === "syncing_files" && progress?.current_file
      ? splitPath(relativeToRoot(progress.current_file, devicePath), null)
      : null;

  return (
    <>
      <div className="bottom__body">
        <div className="progress" role="status" aria-live="polite">
          <div className="progress__header">
            <span className="progress__heading">{stopping ? "Stopping…" : heading}</span>
            {percent !== null && <span className="progress__percent">{percent}%</span>}
          </div>
          <div
            className={`progress__track${percent === null ? " progress__track--indeterminate" : ""}`}
            role="progressbar"
            aria-label={heading}
            aria-valuemin={0}
            aria-valuemax={100}
            aria-valuenow={percent ?? undefined}
          >
            <div
              className="progress__fill"
              style={percent === null ? undefined : { width: `${percent}%` }}
            />
          </div>
          {detail && <p className="progress__detail">{detail}</p>}
          {currentFile && (
            <p className="progress__file" title={progress!.current_file!}>
              {currentFile.head && <span className="path-head">{currentFile.head}</span>}
              <span className="path-tail">{currentFile.tail}</span>
            </p>
          )}
        </div>
      </div>
      <button
        type="button"
        className="button button--secondary"
        onClick={onCancel}
        disabled={stopping}
      >
        {stopping ? "Stopping…" : "Cancel"}
      </button>
    </>
  );
}

function CompletePanel({ summary }: { summary: SyncSummary }) {
  const device = [summary.device.manufacturer, summary.device.product]
    .filter(Boolean)
    .join(" ");
  const preview = summary.dry_run;
  const tiles = [
    { value: summary.files_synced.toLocaleString(), label: preview ? "Files to copy" : "Files copied" },
    { value: formatBytes(summary.bytes_uploaded), label: preview ? "To transfer" : "Transferred" },
    {
      value: summary.delete_extra ? summary.files_deleted.toLocaleString() : "Off",
      label: preview ? "To delete" : "Deleted",
    },
    { value: summary.skipped_entries.toLocaleString(), label: "Skipped" },
  ];

  return (
    <div className="complete" role="status">
      <p className="complete__title">
        <CheckIcon />
        {preview ? "Preview complete" : "Sync complete"}
      </p>
      <dl className="stats">
        {tiles.map((tile) => (
          <div className="stats__tile" key={tile.label}>
            <dt className="stats__label">{tile.label}</dt>
            <dd className="stats__value">{tile.value}</dd>
          </div>
        ))}
      </dl>
      {device && <p className="complete__device">{device}</p>}
    </div>
  );
}

function StoppedNotice({ info }: { info: StoppedInfo }) {
  const checkedAny = info.total_files > 0 && info.processed_files > 0;
  let text: string;
  if (info.dry_run) {
    text = checkedAny
      ? `Checked ${info.processed_files.toLocaleString()} of ${plural(info.total_files, "file")}.`
      : "Stopped before any files were checked.";
  } else {
    const deleted =
      info.files_deleted > 0 ? `, and deleted ${plural(info.files_deleted, "extra file")}` : "";
    text = checkedAny
      ? `Checked ${info.processed_files.toLocaleString()} of ${plural(
          info.total_files,
          "file"
        )} and copied ${info.files_synced.toLocaleString()}${deleted}. Start again to pick up where it left off.`
      : info.files_deleted > 0
        ? `Deleted ${plural(info.files_deleted, "extra file")} before stopping.`
        : "Stopped before any files were copied.";
  }

  return (
    <div className="notice notice--warning" role="status">
      <StopIcon />
      <div>
        <p className="notice__title">{info.dry_run ? "Preview stopped" : "Sync stopped"}</p>
        <p className="notice__text">{text}</p>
      </div>
    </div>
  );
}

const iconProps = {
  width: 16,
  height: 16,
  viewBox: "0 0 16 16",
  fill: "none",
  stroke: "currentColor",
  strokeWidth: 1.4,
  strokeLinecap: "round" as const,
  strokeLinejoin: "round" as const,
  "aria-hidden": true,
  focusable: false,
};

function FolderIcon() {
  return (
    <svg {...iconProps} className="icon route__icon">
      <path d="M1.75 4.25c0-.69.56-1.25 1.25-1.25h3l1.5 1.5H13c.69 0 1.25.56 1.25 1.25v6.25c0 .69-.56 1.25-1.25 1.25H3c-.69 0-1.25-.56-1.25-1.25z" />
    </svg>
  );
}

function PhoneIcon() {
  return (
    <svg {...iconProps} className="icon route__icon">
      <rect x="4.25" y="1.75" width="7.5" height="12.5" rx="1.5" />
      <path d="M7 12h2" />
    </svg>
  );
}

function PencilIcon() {
  return (
    <svg {...iconProps} width={12} height={12} className="icon route__pencil">
      <path d="M10.5 2.5l3 3-8 8H2.5v-3z" />
    </svg>
  );
}

function CheckIcon() {
  return (
    <svg {...iconProps} className="icon">
      <circle cx="8" cy="8" r="6.25" />
      <path d="M5.25 8.25l1.75 1.75 3.75-4" />
    </svg>
  );
}

function StopIcon() {
  return (
    <svg {...iconProps} className="icon notice__icon">
      <circle cx="8" cy="8" r="6.25" />
      <path d="M6.25 5.75v4.5M9.75 5.75v4.5" />
    </svg>
  );
}

function AlertIcon() {
  return (
    <svg {...iconProps} className="icon notice__icon">
      <circle cx="8" cy="8" r="6.25" />
      <path d="M8 4.75v3.75M8 11h.01" />
    </svg>
  );
}

export default App;
