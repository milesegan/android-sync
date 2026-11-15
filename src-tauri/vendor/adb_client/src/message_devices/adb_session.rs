use std::{
    io::{Cursor, Read, Seek},
    time::Duration,
};

use byteorder::ReadBytesExt;

use crate::{
    AdbStatResponse, BinaryDecodable, Result, RustADBError,
    message_devices::{
        adb_message_transport::ADBMessageTransport,
        adb_transport_message::ADBTransportMessage,
        message_commands::{MessageCommand, MessageSubcommand},
        utils::BinaryEncodable,
    },
};

const BUFFER_SIZE: usize = 65535;

/// Represent a session between an `ADBDevice` and remote `adbd`.
#[derive(Debug)]
pub struct ADBSession<T: ADBMessageTransport> {
    transport: T,
    local_id: u32,
    remote_id: u32,
}

impl<T: ADBMessageTransport> ADBSession<T> {
    /// Create a new session with the given transport and IDs.
    pub const fn new(transport: T, local_id: u32, remote_id: u32) -> Self {
        Self {
            transport,
            local_id,
            remote_id,
        }
    }

    pub const fn get_transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    pub const fn local_id(&self) -> u32 {
        self.local_id
    }

    pub const fn remote_id(&self) -> u32 {
        self.remote_id
    }

    pub(crate) fn close(&mut self) -> Result<()> {
        self.transport.write_message(ADBTransportMessage::try_new(
            MessageCommand::Clse,
            self.local_id,
            self.remote_id,
            &[],
        )?)
    }

    /// Receive a message and acknowledge it by replying with an `OKAY` command
    pub(crate) fn recv_and_reply_okay(&mut self) -> Result<ADBTransportMessage> {
        let message = self.transport.read_message()?;
        if message.header().command() == MessageCommand::Clse {
            self.close()?;
            return Ok(message);
        }
        self.reply_okay()?;
        Ok(message)
    }

    fn reply_okay(&mut self) -> Result<()> {
        self.transport.write_message(ADBTransportMessage::try_new(
            MessageCommand::Okay,
            self.local_id,
            self.remote_id,
            &[],
        )?)
    }

    /// Expect a message with an `OKAY` command after sending a message.
    pub(crate) fn send_and_expect_okay(
        &mut self,
        message: ADBTransportMessage,
    ) -> Result<ADBTransportMessage> {
        self.transport.write_message(message)?;

        self.transport.read_message().and_then(|message| {
            message.assert_command(MessageCommand::Okay)?;
            Ok(message)
        })
    }

    pub(crate) fn recv_file<W: std::io::Write>(
        &mut self,
        mut output: W,
    ) -> std::result::Result<(), RustADBError> {
        let mut len: Option<u64> = None;
        loop {
            let payload = self.recv_and_reply_okay()?.into_payload();
            let mut rdr = Cursor::new(&payload);
            while rdr.position() != payload.len() as u64 {
                match len.take() {
                    Some(0) | None => {
                        rdr.seek_relative(4)?;
                        len.replace(u64::from(rdr.read_u32::<byteorder::LittleEndian>()?));
                    }
                    Some(length) => {
                        let remaining_bytes = payload.len() as u64 - rdr.position();
                        if length < remaining_bytes {
                            std::io::copy(&mut rdr.by_ref().take(length), &mut output)?;
                        } else {
                            std::io::copy(&mut rdr.take(remaining_bytes), &mut output)?;
                            len.replace(length - remaining_bytes);
                            // this payload is now exhausted
                            break;
                        }
                    }
                }
            }
            if Cursor::new(&payload[(payload.len() - 8)..(payload.len() - 4)])
                .read_u32::<byteorder::LittleEndian>()?
                == MessageSubcommand::Done as u32
            {
                break;
            }
        }
        Ok(())
    }

    pub(crate) fn push_file<R: std::io::Read>(&mut self, mut reader: R, mtime: u32) -> Result<()> {
        let mut buffer = vec![0; BUFFER_SIZE].into_boxed_slice();
        let amount_read = reader.read(&mut buffer)?;
        let subcommand_data = MessageSubcommand::Data.with_arg(u32::try_from(amount_read)?);

        let mut serialized_message = subcommand_data.encode();
        serialized_message.append(&mut buffer[..amount_read].to_vec());

        let message = ADBTransportMessage::try_new(
            MessageCommand::Write,
            self.local_id(),
            self.remote_id(),
            &serialized_message,
        )?;

        self.send_and_expect_okay(message)?;

        loop {
            let mut buffer = vec![0; BUFFER_SIZE].into_boxed_slice();

            match reader.read(&mut buffer) {
                Ok(0) => {
                    // The device applies DONE's argument as the file's mtime.
                    let subcommand_data = MessageSubcommand::Done.with_arg(mtime);

                    let message = ADBTransportMessage::try_new(
                        MessageCommand::Write,
                        self.local_id(),
                        self.remote_id(),
                        &subcommand_data.encode(),
                    )?;

                    return self.finish_push(message);
                }
                Ok(size) => {
                    let subcommand_data = MessageSubcommand::Data.with_arg(u32::try_from(size)?);

                    let mut serialized_message = subcommand_data.encode();
                    serialized_message.append(&mut buffer[..size].to_vec());

                    let message = ADBTransportMessage::try_new(
                        MessageCommand::Write,
                        self.local_id(),
                        self.remote_id(),
                        &serialized_message,
                    )?;

                    self.send_and_expect_okay(message)?;
                }
                Err(e) => {
                    return Err(RustADBError::IOError(e));
                }
            }
        }
    }

    fn finish_push(&mut self, done: ADBTransportMessage) -> Result<()> {
        self.transport.write_message(done)?;
        let mut acknowledged = false;
        let mut status = Vec::with_capacity(8);
        let mut complete = false;

        // A sync status can span several WRTE packets, with the transport
        // acknowledgement for DONE arriving at any point.
        while !acknowledged || !complete {
            let response = self.read_push_response()?;
            match response.header().command() {
                MessageCommand::Okay if !acknowledged => acknowledged = true,
                MessageCommand::Write if !complete && !response.payload().is_empty() => {
                    self.reply_okay()?;
                    status.extend_from_slice(response.payload());
                    complete = Self::check_push_status(&status)?;
                }
                command => {
                    return Err(RustADBError::ADBRequestFailed(format!(
                        "Unexpected {command} after ADB upload"
                    )));
                }
            }
        }
        Ok(())
    }

    fn check_push_status(status: &[u8]) -> Result<bool> {
        match status {
            b"OKAY\0\0\0\0" => Ok(true),
            [b'F', b'A', b'I', b'L', a, b, c, d, message @ ..] => {
                let length = u32::from_le_bytes([*a, *b, *c, *d]) as usize;
                if length <= BUFFER_SIZE && message.len() == length {
                    let message = String::from_utf8_lossy(message);
                    return Err(RustADBError::ADBRequestFailed(format!(
                        "Device rejected file upload: {message}"
                    )));
                }
                if length <= BUFFER_SIZE && message.len() < length {
                    return Ok(false);
                }
                Err(RustADBError::ADBRequestFailed(
                    "Invalid ADB sync failure length".into(),
                ))
            }
            partial if partial.len() < 8 => Ok(false),
            _ => Err(RustADBError::ADBRequestFailed(
                "Invalid ADB sync status after upload".into(),
            )),
        }
    }

    fn read_push_response(&mut self) -> Result<ADBTransportMessage> {
        let response = self.transport.read_message()?;
        let header = response.header();
        if header.arg0() != self.remote_id || header.arg1() != self.local_id {
            return Err(RustADBError::ADBRequestFailed(
                "Upload response belongs to a different ADB stream".into(),
            ));
        }
        Ok(response)
    }

    pub(crate) fn stat_with_explicit_ids(&mut self, remote_path: &str) -> Result<AdbStatResponse> {
        let stat_buffer = MessageSubcommand::Stat.with_arg(u32::try_from(remote_path.len())?);
        let message = ADBTransportMessage::try_new(
            MessageCommand::Write,
            self.local_id(),
            self.remote_id(),
            &stat_buffer.encode(),
        )?;
        self.send_and_expect_okay(message)?;
        self.send_and_expect_okay(ADBTransportMessage::try_new(
            MessageCommand::Write,
            self.local_id(),
            self.remote_id(),
            remote_path.as_bytes(),
        )?)?;

        let response = self.transport.read_message()?;
        // Skip first 4 bytes as this is the literal "STAT".
        // Interesting part starts right after

        AdbStatResponse::decode(&response.into_payload()[4..])
    }
}

impl<T: ADBMessageTransport> Drop for ADBSession<T> {
    fn drop(&mut self) {
        // Some devices repeat the trailing CLSE. Bound cleanup so a faulty
        // peer cannot keep the session alive by continuously sending packets.
        let deadline = std::time::Instant::now() + Duration::from_millis(20);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.as_millis() == 0
                || self.transport.read_message_with_timeout(remaining).is_err()
            {
                break;
            }
        }
    }
}
