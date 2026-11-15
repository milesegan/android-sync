use rand::RngExt;
use std::{
    path::Path,
    time::{Duration, Instant},
};

use crate::{
    Result, RustADBError,
    message_devices::{
        adb_message_transport::ADBMessageTransport,
        adb_session::ADBSession,
        adb_transport_message::{
            ADBTransportMessage, AUTH_RSAPUBLICKEY, AUTH_SIGNATURE, AUTH_TOKEN,
        },
        message_commands::{MessageCommand, MessageSubcommand},
        models::{ADBRsaKey, read_adb_private_key},
        utils::BinaryEncodable,
    },
    models::ADBLocalCommand,
};

/// Generic structure representing an ADB device reachable over an [`ADBMessageTransport`].
/// Structure is totally agnostic over which transport is truly used.
#[derive(Debug)]
pub struct ADBMessageDevice<T: ADBMessageTransport> {
    transport: T,
}

impl<T: ADBMessageTransport> ADBMessageDevice<T> {
    /// Instantiate a new [`ADBMessageDevice`]
    pub fn new<P: AsRef<Path>>(transport: T, adb_private_key_path: P) -> Result<Self> {
        let private_key = if let Some(private_key) = read_adb_private_key(&adb_private_key_path)? {
            private_key
        } else {
            log::warn!(
                "No private key found at path {}. Generating a new random.",
                adb_private_key_path.as_ref().display()
            );
            ADBRsaKey::new_random()?
        };

        let mut message_device = Self { transport };
        message_device.connect(&private_key)?;

        Ok(message_device)
    }

    pub(crate) const fn get_transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    /// Send initial connect
    fn connect(&mut self, private_key: &ADBRsaKey) -> Result<()> {
        self.get_transport_mut().connect()?;

        let message = ADBTransportMessage::try_new(
            MessageCommand::Cnxn,
            0x0100_0000,
            1_048_576,
            format!("host::{}\0", env!("CARGO_PKG_NAME")).as_bytes(),
        )?;

        self.get_transport_mut().write_message(message)?;

        let message = self.read_handshake_message()?;

        // Check if a client is requesting a secure connection and upgrade it if necessary
        match message.header().command() {
            MessageCommand::Stls => {
                self.get_transport_mut()
                    .write_message(ADBTransportMessage::try_new(
                        MessageCommand::Stls,
                        1,
                        0,
                        &[],
                    )?)?;
                self.get_transport_mut().upgrade_connection()?;
                log::debug!("Connection successfully upgraded from TCP to TLS");
                Ok(())
            }
            MessageCommand::Cnxn => {
                log::debug!("Unencrypted connection established");
                Ok(())
            }
            MessageCommand::Auth => {
                log::debug!("Authentication required");
                self.auth_handshake(message, private_key)
            }
            _ => Err(crate::RustADBError::WrongResponseReceived(
                message.header().command().to_string(),
                "CNXN, STLS or AUTH".to_string(),
            )),
        }
    }

    fn read_handshake_message(&mut self) -> Result<ADBTransportMessage> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            // USB treats a timeout rounded down to zero milliseconds as unbounded.
            if remaining.as_millis() == 0 {
                return Err(std::io::Error::from(std::io::ErrorKind::TimedOut).into());
            }
            let message = self.transport.read_message_with_timeout(remaining)?;
            match message.header().command() {
                // A new CNXN closes any streams left by the previous host connection.
                MessageCommand::Clse | MessageCommand::Okay | MessageCommand::Write => continue,
                _ => return Ok(message),
            }
        }
    }

    fn auth_handshake(
        &mut self,
        message: ADBTransportMessage,
        private_key: &ADBRsaKey,
    ) -> Result<()> {
        match message.header().command() {
            MessageCommand::Auth => {
                log::debug!("Authentication required");
            }
            _ => return Ok(()),
        }

        // At this point, we should have received an AUTH message with arg0 == 1
        let auth_message = match message.header().arg0() {
            AUTH_TOKEN => message,
            v => {
                return Err(RustADBError::ADBRequestFailed(format!(
                    "Received AUTH message with type != 1 ({v})"
                )));
            }
        };

        let sign = private_key.sign(auth_message.into_payload())?;

        let message = ADBTransportMessage::try_new(MessageCommand::Auth, AUTH_SIGNATURE, 0, &sign)?;

        self.transport.write_message(message)?;

        let received_response = self.read_handshake_message()?;

        if received_response.header().command() == MessageCommand::Cnxn {
            log::info!(
                "Authentication OK, device info {}",
                String::from_utf8(received_response.into_payload())?
            );
            return Ok(());
        }
        received_response.assert_command(MessageCommand::Auth)?;

        let mut pubkey = private_key.android_pubkey_encode()?.into_bytes();
        pubkey.push(b'\0');

        let message =
            ADBTransportMessage::try_new(MessageCommand::Auth, AUTH_RSAPUBLICKEY, 0, &pubkey)?;

        self.transport.write_message(message)?;

        let response = self.read_handshake_message()?;
        response.assert_command(MessageCommand::Cnxn)?;

        log::info!(
            "Authentication OK, device info {}",
            String::from_utf8(response.into_payload())?
        );
        Ok(())
    }

    pub(crate) fn open_synchronization_session(&mut self) -> Result<ADBSession<T>> {
        self.open_session(&ADBLocalCommand::Sync)
    }

    /// Open a new ADB session with the given local command.
    pub(crate) fn open_session(&mut self, cmd: &ADBLocalCommand) -> Result<ADBSession<T>> {
        let mut rng = rand::rng();
        let local_id: u32 = rng.random();

        // adbd used to expect a null-terminated string.
        // keep doing so to maintain compatibility with older versions.
        // https://cs.android.com/android/platform/superproject/+/android-latest-release:packages/modules/adb/sockets.cpp;l=560?q=sockets.cpp
        let mut destination = cmd.to_string().into_bytes();
        if !destination.ends_with(&[0]) {
            destination.push(0);
        }

        let message = ADBTransportMessage::try_new(
            MessageCommand::Open,
            local_id, // Our 'local-id'
            0,
            &destination,
        )?;
        self.transport.write_message(message)?;

        let response = self.transport.read_message()?;

        if response.header().command() != MessageCommand::Okay {
            return Err(RustADBError::ADBRequestFailed(format!(
                "Open session failed: got {} in response instead of OKAY",
                response.header().command()
            )));
        }

        if response.header().arg1() != local_id {
            return Err(RustADBError::ADBRequestFailed(format!(
                "Open session failed: responses used {} for our local_id instead of {local_id}",
                response.header().arg1()
            )));
        }

        Ok(ADBSession::new(
            self.transport.clone(),
            local_id,
            response.header().arg0(),
        ))
    }

    pub(crate) fn end_transaction(&mut self, session: &mut ADBSession<T>) -> Result<()> {
        let quit_buffer = MessageSubcommand::Quit.with_arg(0u32);
        session.send_and_expect_okay(ADBTransportMessage::try_new(
            MessageCommand::Write,
            session.local_id(),
            session.remote_id(),
            &quit_buffer.encode(),
        )?)?;

        let response = self.transport.read_message()?;
        response.assert_command(MessageCommand::Clse)?;
        session.close()
    }
}

impl<T: ADBMessageTransport> Drop for ADBMessageDevice<T> {
    fn drop(&mut self) {
        // Best effort here
        let _ = self.get_transport_mut().disconnect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ADBTransport;
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    #[derive(Clone)]
    struct ScriptedTransport {
        incoming: Arc<Mutex<VecDeque<ADBTransportMessage>>>,
        sent: Arc<Mutex<Vec<MessageCommand>>>,
    }

    impl ADBTransport for ScriptedTransport {
        fn connect(&mut self) -> Result<()> {
            Ok(())
        }

        fn disconnect(&mut self) -> Result<()> {
            Ok(())
        }
    }

    impl ADBMessageTransport for ScriptedTransport {
        fn read_message_with_timeout(&mut self, timeout: Duration) -> Result<ADBTransportMessage> {
            assert!(!timeout.is_zero());
            self.incoming
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::TimedOut).into())
        }

        fn write_message_with_timeout(
            &mut self,
            message: ADBTransportMessage,
            _: Duration,
        ) -> Result<()> {
            self.sent.lock().unwrap().push(message.header().command());
            Ok(())
        }
    }

    #[test]
    fn handshake_skips_interrupted_stream_packets_and_authenticates() {
        use MessageCommand::{Auth, Clse, Cnxn, Okay, Write};

        let incoming = [
            ADBTransportMessage::try_new(Clse, 1, 2, &[]).unwrap(),
            ADBTransportMessage::try_new(Okay, 1, 2, &[]).unwrap(),
            ADBTransportMessage::try_new(Auth, AUTH_TOKEN, 0, &[0; 20]).unwrap(),
            ADBTransportMessage::try_new(Write, 1, 2, b"stale").unwrap(),
            ADBTransportMessage::try_new(Cnxn, 0, 0, b"device::").unwrap(),
        ];
        let mut device = ADBMessageDevice {
            transport: ScriptedTransport {
                incoming: Arc::new(Mutex::new(incoming.into())),
                sent: Arc::default(),
            },
        };

        device.connect(&ADBRsaKey::new_random().unwrap()).unwrap();

        assert!(device.transport.incoming.lock().unwrap().is_empty());
        assert_eq!(*device.transport.sent.lock().unwrap(), [Cnxn, Auth]);
    }

    #[test]
    fn acknowledges_upload_status_before_quitting_sync() {
        use MessageCommand::{Clse, Okay, Write};

        for status_first in [false, true] {
            let packet =
                |command, payload| ADBTransportMessage::try_new(command, 2, 1, payload).unwrap();
            let mut incoming = [
                packet(Okay, b""),
                packet(Okay, b""),
                packet(Write, b"OKAY\0\0\0\0"),
                packet(Okay, b""),
                packet(Clse, b""),
            ];
            if status_first {
                incoming.swap(1, 2);
            }
            let mut device = ADBMessageDevice {
                transport: ScriptedTransport {
                    incoming: Arc::new(Mutex::new(incoming.into())),
                    sent: Arc::default(),
                },
            };
            let mut session = ADBSession::new(device.transport.clone(), 1, 2);

            session
                .push_file(b"audio".as_slice(), 1_700_000_000)
                .unwrap();
            device.end_transaction(&mut session).unwrap();

            assert!(device.transport.incoming.lock().unwrap().is_empty());
            assert_eq!(
                *device.transport.sent.lock().unwrap(),
                [Write, Write, Okay, Write, Clse]
            );
        }
    }

    #[test]
    fn reports_upload_failure_across_packets() {
        use MessageCommand::{Okay, Write};

        let incoming = [
            ADBTransportMessage::try_new(Okay, 2, 1, b"").unwrap(),
            ADBTransportMessage::try_new(Write, 2, 1, b"FAIL\x06\0\0\0").unwrap(),
            ADBTransportMessage::try_new(Okay, 2, 1, b"").unwrap(),
            ADBTransportMessage::try_new(Write, 2, 1, b"EACCES").unwrap(),
        ];
        let transport = ScriptedTransport {
            incoming: Arc::new(Mutex::new(incoming.into())),
            sent: Arc::default(),
        };
        let mut session = ADBSession::new(transport.clone(), 1, 2);

        let error = session
            .push_file(b"audio".as_slice(), 1_700_000_000)
            .unwrap_err();
        assert!(
            matches!(error, RustADBError::ADBRequestFailed(message) if message.contains("EACCES"))
        );
        assert_eq!(*transport.sent.lock().unwrap(), [Write, Write, Okay, Okay]);
    }
}
