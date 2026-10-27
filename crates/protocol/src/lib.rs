// SPDX-License-Identifier: Apache-2.0
// Copyright The Peerspan Authors

//! Messages between the runtime and the init inside its microVM, and the names they share.
//!
//! The guest opens one vsock stream to the host. The host sends one [`Message::Run`], followed,
//! when [`Command::archive`] is set, by the workspace archive as [`Message::ArchiveData`] frames
//! and [`Message::ArchiveEnd`]. The guest answers with [`Message::Started`] or
//! [`Message::Failed`], output chunks, the changes when [`Command::collect_changes`] is set, and
//! finally [`Message::Exited`]. Each message is a frame: a one-byte tag, a big-endian `u32`
//! payload length, then the payload.

use std::io::{self, ErrorKind, Read, Write};

/// The vsock port the guest connects to; the launcher maps it to the supervisor's socket.
pub const AGENT_PORT: u32 = 1024;

/// virtio-fs tag of the read-only root filesystem.
pub const ROOTFS_TAG: &str = "rootfs";

/// virtio-fs tag of the workspace.
pub const WORKSPACE_TAG: &str = "workspace";

/// Where the workspace is mounted inside the guest's root filesystem.
pub const WORKSPACE_PATH: &str = "/workspace";

/// The guest's network, when it has one: a fixed address on a /24 behind the host's user-mode
/// network, with the gateway and DNS server the host serves. Fixed addresses keep DHCP off the
/// boot path; every VM has its own network, so they never clash.
pub const GUEST_ADDRESS: [u8; 4] = [10, 0, 2, 15];
pub const GUEST_PREFIX: u8 = 24;
pub const GUEST_GATEWAY: [u8; 4] = [10, 0, 2, 2];
pub const GUEST_DNS: [u8; 4] = [10, 0, 2, 3];

/// Largest payload a reader accepts, so a corrupt frame cannot force a huge allocation.
pub const MAX_PAYLOAD: usize = 16 << 20;

const TAG_RUN: u8 = 1;
const TAG_STDOUT: u8 = 2;
const TAG_STDERR: u8 = 3;
const TAG_EXITED: u8 = 4;
const TAG_ARCHIVE_DATA: u8 = 5;
const TAG_ARCHIVE_END: u8 = 6;
const TAG_STARTED: u8 = 7;
const TAG_FAILED: u8 = 8;
const TAG_CHANGES: u8 = 9;
const TAG_DELETED: u8 = 10;
const TAG_PHASES: u8 = 11;

/// Largest chunk of archive or changes data a writer puts in one frame.
pub const DATA_CHUNK: usize = 64 * 1024;

/// A command for the guest to run.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Command {
    /// Program and arguments; the program is looked up on `PATH` when it has no slash.
    pub argv: Vec<String>,
    /// Working directory inside the guest.
    pub cwd: String,
    /// The complete environment, as name and value pairs.
    pub env: Vec<(String, String)>,
    /// Whether a workspace archive follows, to unpack into the empty workspace first.
    pub archive: bool,
    /// Whether to send the files the command changed relative to the archive.
    pub collect_changes: bool,
    /// Whether the guest has a network device to configure before the command starts.
    pub network: bool,
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
    /// Guest to host: how the command ended; at most one of the two is set.
    Exited {
        code: Option<i32>,
        signal: Option<i32>,
    },
    /// Host to guest: a piece of the workspace archive.
    ArchiveData(Vec<u8>),
    /// Host to guest: the end of the workspace archive.
    ArchiveEnd,
    /// Guest to host: the command is running.
    Started,
    /// Guest to host: the run failed before the command started, for the reason given.
    Failed(String),
    /// Guest to host: a piece of `changes.tar`, the files new or changed since the archive.
    Changes(Vec<u8>),
    /// Guest to host: a piece of `deleted.txt`, the paths removed since the archive.
    Deleted(Vec<u8>),
    /// Guest to host, just before [`Message::Started`]: when init reached each step, as
    /// microseconds of the guest's boot clock, which starts with the kernel.
    Phases(Vec<(String, u64)>),
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
            Message::Exited { code, signal } => (TAG_EXITED, encode_exit(*code, *signal)),
            Message::ArchiveData(bytes) => (TAG_ARCHIVE_DATA, bytes.clone()),
            Message::ArchiveEnd => (TAG_ARCHIVE_END, Vec::new()),
            Message::Started => (TAG_STARTED, Vec::new()),
            Message::Failed(reason) => (TAG_FAILED, reason.as_bytes().to_vec()),
            Message::Changes(bytes) => (TAG_CHANGES, bytes.clone()),
            Message::Deleted(bytes) => (TAG_DELETED, bytes.clone()),
            Message::Phases(phases) => (TAG_PHASES, encode_phases(phases)),
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
            TAG_EXITED => decode_exit(&payload)?,
            TAG_ARCHIVE_DATA => Message::ArchiveData(payload),
            TAG_ARCHIVE_END => empty(&payload, Message::ArchiveEnd)?,
            TAG_STARTED => empty(&payload, Message::Started)?,
            TAG_FAILED => Message::Failed(
                String::from_utf8(payload).map_err(|_| invalid("failure reason is not UTF-8"))?,
            ),
            TAG_CHANGES => Message::Changes(payload),
            TAG_DELETED => Message::Deleted(payload),
            TAG_PHASES => Message::Phases(decode_phases(&payload)?),
            other => return Err(invalid(format!("unknown message tag {other}"))),
        };
        Ok(Some(message))
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, message.into())
}

fn empty(payload: &[u8], message: Message) -> io::Result<Message> {
    if payload.is_empty() {
        Ok(message)
    } else {
        Err(invalid("unexpected payload"))
    }
}

/// A flags byte (bit 0: code present, bit 1: signal present), then the code and the signal as
/// big-endian `i32`, zero when absent.
fn encode_exit(code: Option<i32>, signal: Option<i32>) -> Vec<u8> {
    let flags = u8::from(code.is_some()) | (u8::from(signal.is_some()) << 1);
    let mut out = vec![flags];
    out.extend_from_slice(&code.unwrap_or(0).to_be_bytes());
    out.extend_from_slice(&signal.unwrap_or(0).to_be_bytes());
    out
}

fn decode_exit(payload: &[u8]) -> io::Result<Message> {
    let [flags, rest @ ..] = payload else {
        return Err(invalid("empty exit payload"));
    };
    let (Some((code, signal)), true) = (rest.split_first_chunk::<4>(), rest.len() == 8) else {
        return Err(invalid("exit payload is not 9 bytes"));
    };
    let signal: [u8; 4] = signal.try_into().expect("length checked");
    if *flags > 0b11 {
        return Err(invalid("unknown exit flags"));
    }
    Ok(Message::Exited {
        code: (flags & 1 != 0).then(|| i32::from_be_bytes(*code)),
        signal: (flags & 2 != 0).then(|| i32::from_be_bytes(signal)),
    })
}

fn encode_phases(phases: &[(String, u64)]) -> Vec<u8> {
    let mut out = Vec::new();
    put_count(&mut out, phases.len());
    for (name, micros) in phases {
        put_str(&mut out, name);
        out.extend_from_slice(&micros.to_be_bytes());
    }
    out
}

fn decode_phases(payload: &[u8]) -> io::Result<Vec<(String, u64)>> {
    let mut cursor = Cursor { rest: payload };
    let count = cursor.count()?;
    let phases = (0..count)
        .map(|_| Ok((cursor.string()?, cursor.u64()?)))
        .collect::<io::Result<Vec<_>>>()?;
    if !cursor.rest.is_empty() {
        return Err(invalid("trailing bytes after phases"));
    }
    Ok(phases)
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
    out.push(u8::from(command.archive));
    out.push(u8::from(command.collect_changes));
    out.push(u8::from(command.network));
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
    let [archive, collect_changes, network] = *cursor.rest else {
        return Err(invalid("command flags are not 3 bytes"));
    };
    let flag = |b: u8| match b {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(invalid("command flag is not 0 or 1")),
    };
    Ok(Command {
        argv,
        cwd,
        env,
        archive: flag(archive)?,
        collect_changes: flag(collect_changes)?,
        network: flag(network)?,
    })
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

    fn u64(&mut self) -> io::Result<u64> {
        let (head, rest) = self
            .rest
            .split_first_chunk::<8>()
            .ok_or_else(|| invalid("truncated phases"))?;
        self.rest = rest;
        Ok(u64::from_be_bytes(*head))
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
                archive: true,
                collect_changes: true,
                network: true,
            }),
            Message::Run(Command::default()),
            Message::Stdout(b"out\n".to_vec()),
            Message::Stderr(Vec::new()),
            Message::Exited {
                code: Some(-1),
                signal: None,
            },
            Message::Exited {
                code: None,
                signal: Some(9),
            },
            Message::Exited {
                code: None,
                signal: None,
            },
            Message::ArchiveData(vec![0; 3]),
            Message::ArchiveEnd,
            Message::Started,
            Message::Failed("refused entry ../x".into()),
            Message::Changes(b"tar".to_vec()),
            Message::Deleted(b"a\nb\n".to_vec()),
            Message::Phases(vec![
                ("init".into(), 120_000),
                ("connected".into(), u64::MAX),
            ]),
            Message::Phases(Vec::new()),
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
        let payload = [0u8, 0, 0, 1, 0, 0, 0, 9, b'x', 0, 0];
        frame.extend_from_slice(&u32::try_from(payload.len()).expect("small").to_be_bytes());
        frame.extend_from_slice(&payload);
        let err = Message::read_from(&mut frame.as_slice()).expect_err("truncated string");
        assert_eq!(err.kind(), ErrorKind::InvalidData);
    }
}
