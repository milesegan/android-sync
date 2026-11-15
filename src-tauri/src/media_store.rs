use adb_client::{usb::ADBUSBDevice, ADBDeviceExt, RustADBError};
use std::sync::atomic::AtomicBool;

use crate::{check_cancelled_io, shell_escape_single_quotes, LocalFileEntry};

const FILES_URI: &str = "content://media/external/file";
const PENDING_MEDIA: &str = "is_pending=1 AND date_expires IS NULL AND \
                             owner_package_name='com.android.shell' AND media_type IN (1,2,3,4)";
// Leave room for shell quoting and the command itself.
const MAX_PATH_LIST_LEN: usize = 16_000;
// MediaStore reads metadata for each item before returning the update.
const MAX_FILES_PER_UPDATE: usize = 64;

pub(crate) fn publish_pending_files(
    device: &mut ADBUSBDevice,
    remote_root: &str,
    files: &[LocalFileEntry],
    cancelled: &AtomicBool,
) -> Result<(), RustADBError> {
    let mut shell_output = |command: &str| {
        checked_shell_output(cancelled, |output| {
            device.shell_command(&command, Some(output), None)
        })
    };
    // Android 11 introduced FUSE entries for media created through filesystem paths.
    let root = shell_escape_single_quotes(remote_root);
    let resolved_root = shell_output(&format!(
        "if [ \"$(getprop ro.build.version.sdk)\" -ge 30 ]; then realpath {root}; else echo unsupported; fi"
    ))?;
    let Some(canonical_root) = canonical_storage_root(&resolved_root)? else {
        return Ok(());
    };

    // Include files that already matched, so the next sync can finish publication
    // if a previous sync ended after writing the file.
    let mut can_batch = true;
    for selection in pending_file_selections(files, remote_root, canonical_root) {
        let selection = shell_escape_single_quotes(&selection);
        if can_batch {
            // FUSE pending files already have their final names. Disabling path
            // movement lets MediaStore publish the whole selection at once.
            let result = shell_output(&format!(
                "content update --uri {FILES_URI} --bind is_pending:i:0 \
                     --extra 'android\\:query-arg-allow-movement:b:false' \
                     --where {selection} && echo published"
            ))?;
            if result.trim() == "published" {
                continue;
            }
            if !result.contains("Unsupported argument: --extra")
                && !result.contains("isn't part of well-defined collection not allowed")
            {
                return Err(RustADBError::ADBRequestFailed(result.trim().into()));
            }
            can_batch = false;
        }

        // Older content commands can update individual item URIs.
        let query = format!("content query --uri {FILES_URI} --projection _id --where {selection}");
        let result = shell_output(&query)?;
        let pending_media = shell_escape_single_quotes(PENDING_MEDIA);
        for id in pending_file_ids(&result)? {
            let result = shell_output(&format!(
                "content update --uri {FILES_URI}/{id} --bind is_pending:i:0 \
                     --where {pending_media} && echo published"
            ))?;
            if result.trim() != "published" {
                return Err(RustADBError::ADBRequestFailed(result.trim().into()));
            }
        }
    }
    Ok(())
}

fn canonical_storage_root(output: &str) -> Result<Option<&str>, RustADBError> {
    let root = output.trim_end_matches(['\r', '\n']);
    if root == "unsupported" || root.starts_with('/') {
        return Ok(root.starts_with("/storage/").then_some(root));
    }
    Err(RustADBError::ADBRequestFailed(format!(
        "Could not resolve device folder: {root}"
    )))
}

fn pending_file_selections(
    files: &[LocalFileEntry],
    remote_root: &str,
    canonical_root: &str,
) -> Vec<String> {
    let mut selections = Vec::new();
    let mut paths = Vec::new();
    let mut length = 0;
    let selection = |paths: &[String]| {
        format!(
            "_data COLLATE NOCASE IN ({}) AND {PENDING_MEDIA}",
            paths.join(",")
        )
    };
    for file in files {
        let relative_path = &file.remote_path[remote_root.len()..];
        let path = format!("{canonical_root}{relative_path}");
        let literal = format!("'{}'", path.replace('\'', "''"));
        let quoted_len = shell_escape_single_quotes(&literal).len() + 1;
        if paths.len() == MAX_FILES_PER_UPDATE
            || (!paths.is_empty() && length + quoted_len > MAX_PATH_LIST_LEN)
        {
            selections.push(selection(&paths));
            paths.clear();
            length = 0;
        }
        length += quoted_len;
        paths.push(literal);
    }
    if !paths.is_empty() {
        selections.push(selection(&paths));
    }
    selections
}

fn pending_file_ids(output: &str) -> Result<Vec<u64>, RustADBError> {
    let output = output.trim();
    if output == "No result found." {
        return Ok(Vec::new());
    }
    if output.is_empty() {
        return Err(RustADBError::ADBRequestFailed(
            "Empty MediaStore query response".into(),
        ));
    }
    output
        .lines()
        .map(|line| {
            line.strip_prefix("Row: ")
                .and_then(|row| row.split_once(" _id="))
                .and_then(|(_, id)| id.parse().ok())
                .ok_or_else(|| RustADBError::ADBRequestFailed(line.into()))
        })
        .collect()
}

fn checked_shell_output(
    cancelled: &AtomicBool,
    run: impl FnOnce(&mut Vec<u8>) -> Result<Option<u8>, RustADBError>,
) -> Result<String, RustADBError> {
    check_cancelled_io(cancelled)?;
    let mut output = Vec::new();
    let result = run(&mut output);
    check_cancelled_io(cancelled)?;
    result?;
    Ok(String::from_utf8(output)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_during_the_last_shell_command_takes_precedence() {
        use std::sync::atomic::Ordering;

        let cancelled = AtomicBool::new(false);
        let result = checked_shell_output(&cancelled, |output| {
            output.extend_from_slice(b"published\n");
            cancelled.store(true, Ordering::Relaxed);
            Ok(None)
        });
        assert!(matches!(
            result.map_err(crate::SyncError::from),
            Err(crate::SyncError::Cancelled)
        ));
    }

    #[test]
    fn distinguishes_unsupported_storage_from_path_resolution_errors() {
        assert_eq!(canonical_storage_root("unsupported\n").unwrap(), None);
        assert_eq!(canonical_storage_root("/data/local/tmp\n").unwrap(), None);
        assert_eq!(
            canonical_storage_root("/storage/emulated/0/Music\r\n").unwrap(),
            Some("/storage/emulated/0/Music")
        );
        assert!(canonical_storage_root("realpath: No such file or directory\n").is_err());
        assert!(canonical_storage_root("").is_err());
    }

    #[test]
    fn selects_exact_paths_and_quotes_apostrophes_for_sql_and_shell() {
        let file = LocalFileEntry {
            local_path: "it's 100%_live.m4a".into(),
            remote_path: "/sdcard/Music/it's 100%_live.m4a".into(),
            size_bytes: 100,
            mtime: 0,
        };
        let selections =
            pending_file_selections(&[file], "/sdcard/Music", "/storage/emulated/0/music");
        let selection =
            "_data COLLATE NOCASE IN ('/storage/emulated/0/music/it''s 100%_live.m4a') \
                         AND is_pending=1 AND date_expires IS NULL AND \
                         owner_package_name='com.android.shell' AND media_type IN (1,2,3,4)";
        assert_eq!(selections, [selection]);
    }

    #[test]
    fn batches_respect_the_shell_length_and_item_limits() {
        for name_prefix in ["short".to_string(), "'".repeat(200)] {
            let files = (0..130)
                .map(|i| LocalFileEntry {
                    local_path: format!("{name_prefix}{i}.m4a").into(),
                    remote_path: format!("/sdcard/Music/{name_prefix}{i}.m4a"),
                    size_bytes: 100,
                    mtime: 0,
                })
                .collect::<Vec<_>>();
            let batches =
                pending_file_selections(&files, "/sdcard/Music", "/storage/emulated/0/Music");
            assert!(batches.len() > 1);
            assert!(batches.iter().all(|batch| {
                batch.matches(".m4a").count() <= MAX_FILES_PER_UPDATE
                    && shell_escape_single_quotes(batch).len() <= MAX_PATH_LIST_LEN + 200
            }));
            assert_eq!(
                batches
                    .iter()
                    .map(|batch| batch.matches(".m4a").count())
                    .sum::<usize>(),
                files.len()
            );
        }
    }

    #[test]
    fn only_accepts_numeric_item_ids_and_propagates_query_failures() {
        assert_eq!(
            pending_file_ids("Row: 0 _id=124\r\nRow: 1 _id=126\r\n").unwrap(),
            [124, 126]
        );
        assert!(pending_file_ids("No result found.\n").unwrap().is_empty());
        assert!(pending_file_ids("").is_err());
        assert!(pending_file_ids("Error while accessing provider:media\n").is_err());
        assert!(pending_file_ids("Row: 0 _id=1/2\n").is_err());
    }
}
