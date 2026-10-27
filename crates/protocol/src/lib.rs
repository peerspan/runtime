// SPDX-License-Identifier: Apache-2.0
// Copyright The Peerspan Authors

//! Messages between the runtime and the init inside its microVM, and the names they share.
//!
//! The guest opens one vsock stream to the host. The runtime sends a single [`Message::Run`];
//! the guest answers with output chunks and finally [`Message::Exit`]. Each message is a frame:
//! a one-byte tag, a big-endian `u32` payload length, then the payload.

use std::io::{self, ErrorKind, Read, Write};

/// The vsock port the guest connects to; the launcher maps it to the supervisor's socket.
pub const AGENT_PORT: u32 = 1024;

/// virtio-fs tag of the read-only root filesystem.
pub const ROOTFS_TAG: &str = "rootfs";

/// virtio-fs tag of the workspace.
pub const WORKSPACE_TAG: &str = "workspace";

/// Where the workspace is mounted inside the guest's root filesystem.
pub const WORKSPACE_PATH: &str = "/workspace";

/// Largest payload a reader accepts, so a corrupt frame cannot force a huge allocation.
pub const MAX_PAYLOAD: usize = 16 << 20;

const TAG_RUN: u8 = 1;
const TAG_STDOUT: u8 = 2;
const TAG_STDERR: u8 = 3;
const TAG_EXIT: u8 = 4;

/// A command for the guest to run.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Command {
    /// Program and arguments; the program is looked up on `PATH` when it has no slash.
    pub argv: Vec<String>,
    /// Working directory inside the guest.
    pub cwd: String,
    /// The complete environment, as name and value pairs.
    pub env: Vec<(String, String)>,
}

/// One frame on the guest's stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// Host to guest: run this command.
    Run(Command),
    /// Guest to host: bytes the command wrote to standard output.
    Stdout(Vec<u8>),
    /// Guest to host: bytes the command wrote to standard error.
    Stderr(Vec<u8>),
    /// Guest to host: the command's exit code, or 128 plus the signal that ended it.
    Exit(i32),
}

impl Message {
    /// Writes this message as one frame.
    ///
    /// # Errors
    ///
    /// Returns any error from `writer`, or `InvalidInput` if the payload exceeds
    /// [`MAX_PAYLOAD`].
    pub fn write_to(&self, writer: &mut impl Write) -> io::Result<()> {
        let (tag, payload) = match self {
            Message::Run(command) => (TAG_RUN, encode_command(command)),
            Message::Stdout(bytes) => (TAG_STDOUT, bytes.clone()),
            Message::Stderr(bytes) => (TAG_STDERR, bytes.clone()),
            Message::Exit(code) => (TAG_EXIT, code.to_be_bytes().to_vec()),
        };
        if payload.len() > MAX_PAYLOAD {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!("payload of {} bytes exceeds {MAX_PAYLOAD}", payload.len()),
            ));
        }
        let len = u32::try_from(payload.len()).expect("MAX_PAYLOAD fits in u32");
        let mut frame = Vec::with_capacity(5 + payload.len());
        frame.push(tag);
        frame.extend_from_slice(&len.to_be_bytes());
        frame.extend_from_slice(&payload);
        writer.write_all(&frame)?;
        writer.flush()
    }

    /// Reads one frame, or returns `None` if the stream ends cleanly between frames.
    ///
    /// # Errors
    ///
    /// Returns any error from `reader`, `UnexpectedEof` if the stream ends inside a frame, and
    /// `InvalidData` for an unknown tag, an oversized payload or a malformed payload.
    pub fn read_from(reader: &mut impl Read) -> io::Result<Option<Message>> {
        let mut tag = [0u8; 1];
        match reader.read_exact(&mut tag) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let mut len = [0u8; 4];
        reader.read_exact(&mut len)?;
        let len = usize::try_from(u32::from_be_bytes(len)).expect("u32 fits in usize");
        if len > MAX_PAYLOAD {
            return Err(invalid(format!(
                "payload of {len} bytes exceeds {MAX_PAYLOAD}"
            )));
        }
        let mut payload = vec![0u8; len];
        reader.read_exact(&mut payload)?;
        let message = match tag[0] {
            TAG_RUN => Message::Run(decode_command(&payload)?),
            TAG_STDOUT => Message::Stdout(payload),
            TAG_STDERR => Message::Stderr(payload),
            TAG_EXIT => {
                let code: [u8; 4] = payload
                    .try_into()
                    .map_err(|_| invalid("exit payload is not 4 bytes"))?;
                Message::Exit(i32::from_be_bytes(code))
            }
            other => return Err(invalid(format!("unknown message tag {other}"))),
        };
        Ok(Some(message))
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, message.into())
}

fn encode_command(command: &Command) -> Vec<u8> {
    let mut out = Vec::new();
    put_count(&mut out, command.argv.len());
    for arg in &command.argv {
        put_str(&mut out, arg);
    }
    put_str(&mut out, &command.cwd);
    put_count(&mut out, command.env.len());
    for (name, value) in &command.env {
        put_str(&mut out, name);
        put_str(&mut out, value);
    }
    out
}

fn decode_command(payload: &[u8]) -> io::Result<Command> {
    let mut cursor = Cursor { rest: payload };
    let argc = cursor.count()?;
    let argv = (0..argc)
        .map(|_| cursor.string())
        .collect::<io::Result<Vec<_>>>()?;
    let cwd = cursor.string()?;
    let envc = cursor.count()?;
    let env = (0..envc)
        .map(|_| Ok((cursor.string()?, cursor.string()?)))
        .collect::<io::Result<Vec<_>>>()?;
    if !cursor.rest.is_empty() {
        return Err(invalid("trailing bytes after command"));
    }
    Ok(Command { argv, cwd, env })
}

fn put_count(out: &mut Vec<u8>, count: usize) {
    let count = u32::try_from(count).expect("counts are bounded by MAX_PAYLOAD");
    out.extend_from_slice(&count.to_be_bytes());
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    put_count(out, s.len());
    out.extend_from_slice(s.as_bytes());
}

struct Cursor<'buf> {
    rest: &'buf [u8],
}

impl Cursor<'_> {
    fn count(&mut self) -> io::Result<usize> {
        let (head, rest) = self
            .rest
            .split_first_chunk::<4>()
            .ok_or_else(|| invalid("truncated command"))?;
        self.rest = rest;
        Ok(usize::try_from(u32::from_be_bytes(*head)).expect("u32 fits in usize"))
    }

    fn string(&mut self) -> io::Result<String> {
        let len = self.count()?;
        if len > self.rest.len() {
            return Err(invalid("truncated command"));
        }
        let (bytes, rest) = self.rest.split_at(len);
        self.rest = rest;
        String::from_utf8(bytes.to_vec()).map_err(|_| invalid("command string is not UTF-8"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(message: &Message) -> Message {
        let mut buf = Vec::new();
        message.write_to(&mut buf).expect("write to Vec");
        let mut reader = buf.as_slice();
        let got = Message::read_from(&mut reader)
            .expect("read")
            .expect("one frame");
        assert!(reader.is_empty(), "frame left {} bytes", reader.len());
        got
    }

    #[test]
    fn every_message_round_trips() {
        let messages = [
            Message::Run(Command {
                argv: vec!["sh".into(), "-c".into(), "echo ünïcode".into()],
                cwd: "/workspace".into(),
                env: vec![
                    ("PATH".into(), "/bin".into()),
                    ("EMPTY".into(), String::new()),
                ],
            }),
            Message::Run(Command::default()),
            Message::Stdout(b"out\n".to_vec()),
            Message::Stderr(Vec::new()),
            Message::Exit(-1),
            Message::Exit(137),
        ];
        for message in &messages {
            assert_eq!(&round_trip(message), message);
        }
    }

    #[test]
    fn clean_end_between_frames_is_none() {
        assert!(
            Message::read_from(&mut [].as_slice())
                .expect("read")
                .is_none()
        );
    }

    #[test]
    fn end_inside_a_frame_is_an_error() {
        let mut buf = Vec::new();
        Message::Stdout(b"abc".to_vec())
            .write_to(&mut buf)
            .expect("write");
        buf.pop();
        let err = Message::read_from(&mut buf.as_slice()).expect_err("truncated");
        assert_eq!(err.kind(), ErrorKind::UnexpectedEof);
    }

    #[test]
    fn oversized_and_unknown_frames_are_rejected() {
        let mut oversized = vec![TAG_STDOUT];
        oversized.extend_from_slice(&u32::MAX.to_be_bytes());
        let err = Message::read_from(&mut oversized.as_slice()).expect_err("oversized");
        assert_eq!(err.kind(), ErrorKind::InvalidData);

        let unknown = [99u8, 0, 0, 0, 0];
        let err = Message::read_from(&mut unknown.as_slice()).expect_err("unknown tag");
        assert_eq!(err.kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn malformed_command_is_rejected() {
        let mut frame = vec![TAG_RUN];
        let payload = [0u8, 0, 0, 1, 0, 0, 0, 9, b'x'];
        frame.extend_from_slice(&u32::try_from(payload.len()).expect("small").to_be_bytes());
        frame.extend_from_slice(&payload);
        let err = Message::read_from(&mut frame.as_slice()).expect_err("truncated string");
        assert_eq!(err.kind(), ErrorKind::InvalidData);
    }
}
