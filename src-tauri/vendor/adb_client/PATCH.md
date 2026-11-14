# Local USB patches

This is `adb_client` 3.2.3 from crates.io, published from
[upstream commit b88e332](https://github.com/cocool97/adb_client/tree/b88e3324627f4e7e38dbf7f9997ddacf73f12dbe/adb_client).

The patch in `src/message_devices/usb/usb_transport.rs` submits the remaining
ADB message data in a single bulk write and lets libusb split it into USB
packets. The zero-length packet for a packet-aligned message remains in place.

Connection setup skips old stream packets while waiting for the new handshake
and USB reads have finite timeouts. The file upload path closes the specific
ADB stream on failure so the app can stop a transfer before releasing USB.
`ADBUSBDevice::push_with_mtime` sends the file's modification time in the
push's final DONE message, which adbd applies to the file. The stock USB push
sends 0 there, which leaves every pushed file dated 1970. `push` still sends 0.

Switch the app back to the crates.io dependency when a published version
includes these changes.

## Upstream status

Checked on 2026-11-14: crates.io still has 3.2.3 as its latest release, so
none of these changes are published. Upstream `main` (eb09aa5) has the single
bulk write, skips old stream packets during the handshake, and closes a failed
push's stream when the session is dropped. It still reads USB with no timeout
by default and waits without a timeout for the first handshake reply. The app
relies on finite reads so a cancelled connection attempt finishes, so keep the
timeout change when moving to the next release. Upstream `main` sends the
current time as the modification time of a USB push, with no way to pass the
file's own, so keep `push_with_mtime` too.
