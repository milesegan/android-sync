# Local USB write patch

This is `adb_client` 3.2.3 from crates.io, published from
[upstream commit b88e332](https://github.com/cocool97/adb_client/tree/b88e3324627f4e7e38dbf7f9997ddacf73f12dbe/adb_client).

The patch in `src/message_devices/usb/usb_transport.rs` submits the remaining
ADB message data in a single bulk write and lets libusb split it into USB
packets. The zero-length packet for a packet-aligned message remains in place.
Switch the app back to the crates.io dependency when a published version
includes this change.
