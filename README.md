# Android Sync

A small Mac app that copies a folder from your Mac to an Android phone over USB.

<p align="center">
  <img src="docs/screenshot.png" alt="The Android Sync window: a folder on the Mac, a folder on the phone, a Preview only switch, and a Start sync button" width="486">
</p>

Pick a folder on your Mac, type the folder on the phone it should go to, and
press **Start sync**. Android Sync talks to the phone directly over USB using
the ADB protocol, so you don't need Android File Transfer, MTP, or a separate
`adb` install.

## What a sync does

- **Copies one way, Mac → phone.** A file is copied when it is missing on the
  phone or its size differs from the Mac copy. Everything else is left alone,
  so running it again only copies what changed.
- **Never deletes anything on the phone.** Files you removed on the Mac stay
  on the phone.
- **Creates folders** on the phone to match the folder structure on the Mac.
- **Skips dotfiles** such as `.DS_Store`, and anything that isn't a regular
  file or folder (symlinks, for example).
- **Preview only** runs the same check without copying, and reports how many
  files and bytes a real sync would transfer.
- **Cancel** stops a sync at any point, including while waiting for the phone
  to approve the connection. The file being copied when you cancel is removed
  by the phone, and the next sync copies it again.

> [!NOTE]
> Files are compared by **size only**. If you edit a file on the Mac and its
> size happens to stay exactly the same, it won't be copied again. Delete it
> on the phone, or change the destination folder, to force a fresh copy.

## Using it

1. On the phone, turn on **Developer options** and then **USB debugging**
   (Settings → About phone → tap *Build number* seven times, then
   Settings → System → Developer options).
2. Connect the phone with a USB cable and unlock it.
3. In Android Sync, choose the folder on your Mac and enter the destination on
   the phone, for example `/sdcard/Music`. Both are remembered for next time.
4. Press **Start sync**. The first time, the phone asks *“Allow USB
   debugging?”*. Tap **Allow** (tick *Always allow from this computer* to skip
   the prompt in future).

The window follows your Mac's light or dark appearance.

### USB debugging key

The phone remembers which computers it trusts by their ADB key. Android Sync
uses your existing key at `~/.android/adbkey` (or under `$ANDROID_USER_HOME`)
if you have one, so a phone that already trusts your `adb` doesn't ask again.
Otherwise it creates its own key in the app's data folder and keeps reusing it.

## Troubleshooting

| Message | What to do |
| --- | --- |
| *No Android device detected over USB* | Check the cable, unlock the phone, and make sure USB debugging is on. Some cables only charge. |
| *Multiple Android devices detected* | Unplug all but one phone. |
| *Android is waiting for USB debugging approval* or *Android did not respond* | Unlock the phone and tap **Allow** on the prompt. If no prompt appears, reconnect the cable and try again. |
| *Another app is using the Android USB debugging connection* | Only one program can use the phone's debugging connection at a time. Quit Android Studio, scrcpy and similar tools, and stop any running ADB server with `adb kill-server`. |

## Building from source

You need:

- [Rust](https://rustup.rs) 1.88 or newer
- [Node.js](https://nodejs.org) and [pnpm](https://pnpm.io)
- The [Tauri prerequisites](https://tauri.app/start/prerequisites/) for your
  platform (on macOS, the Xcode Command Line Tools)

Then:

```bash
pnpm install
```

```bash
pnpm tauri dev
```

To build a release app bundle (written to `src-tauri/target/release/bundle/`):

```bash
pnpm tauri build
```

Run the Rust tests with:

```bash
cargo test --manifest-path src-tauri/Cargo.toml
```

The app is developed and tested on macOS.

## How it's put together

Android Sync is a [Tauri 2](https://tauri.app) app with a React + TypeScript
front end.

| Path | What's there |
| --- | --- |
| `src/App.tsx`, `src/App.css` | The whole user interface |
| `src-tauri/src/lib.rs` | The sync: scanning the folder, connecting over USB, comparing against the phone and copying files |
| `src-tauri/src/adb_key.rs` | Finding or creating the USB debugging key |
| `src-tauri/vendor/adb_client` | A patched copy of the [`adb_client`](https://github.com/cocool97/adb_client) crate with faster USB writes and better recovery from interrupted transfers (see its `PATCH.md`) |

A sync scans the Mac folder, connects to the phone, creates any missing
folders, lists the files already on the phone with their sizes in one shell
command, and then pushes each new or changed file. Progress is sent to the
window as `sync-progress` events.
