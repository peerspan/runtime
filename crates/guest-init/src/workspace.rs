// SPDX-License-Identifier: Apache-2.0
// Copyright The Peerspan Authors

//! The workspace archive: unpacking it into the empty workspace, and finding what a run changed.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};

use flate2::read::GzDecoder;
use peerspan_runtime_protocol::{DATA_CHUNK, Message};
use sha2::{Digest, Sha256};
use tar::EntryType;

/// What the archive put at each path, relative to the workspace.
pub type Manifest = BTreeMap<PathBuf, Kind>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Directory,
    Symlink,
    File([u8; 32]),
}

/// Unpacks the archive the host streams after the command into `workspace`, which is empty.
///
/// # Errors
///
/// Returns the reason the run must fail: an entry that is absolute, contains `..`, or is not a
/// directory, regular file or symbolic link, or a malformed archive. The rest of the archive
/// is drained first, so the stream stays in step.
pub fn unpack(stream: &mut impl Read, workspace: &Path) -> Result<Manifest, String> {
    let mut frames = ArchiveFrames {
        stream,
        buf: Vec::new(),
        pos: 0,
        done: false,
    };
    let result = unpack_from(&mut frames, workspace);
    if result.is_err() {
        let _ = io::copy(&mut frames, &mut io::sink());
    }
    result
}

fn unpack_from(
    frames: &mut ArchiveFrames<'_, impl Read>,
    workspace: &Path,
) -> Result<Manifest, String> {
    let mut reader = BufReader::new(frames);
    let gzip = reader
        .fill_buf()
        .map_err(|e| e.to_string())?
        .starts_with(&[0x1f, 0x8b]);
    let input: Box<dyn Read + '_> = if gzip {
        Box::new(GzDecoder::new(reader))
    } else {
        Box::new(reader)
    };
    let mut archive = tar::Archive::new(input);
    archive.set_preserve_permissions(true);
    let mut manifest = Manifest::new();
    for entry in archive
        .entries()
        .map_err(|e| format!("reading the archive: {e}"))?
    {
        let mut entry = entry.map_err(|e| format!("reading the archive: {e}"))?;
        let raw = entry
            .path()
            .map_err(|e| format!("reading the archive: {e}"))?
            .into_owned();
        let kind = match entry.header().entry_type() {
            EntryType::XGlobalHeader => continue,
            EntryType::Directory => Kind::Directory,
            EntryType::Symlink => Kind::Symlink,
            EntryType::Regular | EntryType::Continuous => Kind::File([0; 32]),
            other => {
                return Err(format!(
                    "refused archive entry {}: type {other:?}",
                    raw.display()
                ));
            }
        };
        let Some(relative) = relative(&raw) else {
            return Err(format!(
                "refused archive entry {}: path leaves the workspace",
                raw.display()
            ));
        };
        if relative.as_os_str().is_empty() {
            continue;
        }
        let unpacked = entry
            .unpack_in(workspace)
            .map_err(|e| format!("unpacking {}: {e}", raw.display()))?;
        if !unpacked {
            return Err(format!(
                "refused archive entry {}: path leaves the workspace",
                raw.display()
            ));
        }
        let kind = match kind {
            Kind::File(_) => Kind::File(
                hash(&workspace.join(&relative))
                    .map_err(|e| format!("reading {}: {e}", raw.display()))?,
            ),
            other => other,
        };
        manifest.insert(relative, kind);
    }
    Ok(manifest)
}

/// The path with `.` removed, or `None` if it is absolute or contains `..`.
fn relative(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(out)
}

/// Sends `changes.tar`, the regular files new or different since `manifest`, and `deleted.txt`,
/// the paths in `manifest` that are gone, one per line.
///
/// # Errors
///
/// Returns an error if the workspace cannot be read or the stream fails.
pub fn send_changes(
    workspace: &Path,
    manifest: &Manifest,
    stream: &mut impl Write,
) -> io::Result<()> {
    let mut present = Manifest::new();
    walk(workspace, Path::new(""), &mut present)?;

    let mut tar = tar::Builder::new(FrameWriter::new(stream, Message::Changes));
    tar.follow_symlinks(false);
    for (path, kind) in &present {
        if let Kind::File(_) = kind
            && manifest.get(path) != Some(kind)
        {
            tar.append_path_with_name(workspace.join(path), path)?;
        }
    }
    tar.into_inner()?.finish()?;

    let mut deleted = FrameWriter::new(stream, Message::Deleted);
    for path in manifest.keys().filter(|path| !present.contains_key(*path)) {
        deleted.write_all(path.as_os_str().as_encoded_bytes())?;
        deleted.write_all(b"\n")?;
    }
    deleted.finish()
}

fn walk(root: &Path, relative: &Path, out: &mut Manifest) -> io::Result<()> {
    for entry in fs::read_dir(root.join(relative))? {
        let entry = entry?;
        let path = relative.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            out.insert(path.clone(), Kind::Directory);
            walk(root, &path, out)?;
        } else if file_type.is_symlink() {
            out.insert(path, Kind::Symlink);
        } else if file_type.is_file() {
            out.insert(path.clone(), Kind::File(hash(&root.join(&path))?));
        }
    }
    Ok(())
}

fn hash(path: &Path) -> io::Result<[u8; 32]> {
    let mut hasher = Sha256::new();
    io::copy(&mut File::open(path)?, &mut hasher)?;
    Ok(hasher.finalize().into())
}

/// Reads the archive out of `ArchiveData` frames until `ArchiveEnd`.
struct ArchiveFrames<'stream, R> {
    stream: &'stream mut R,
    buf: Vec<u8>,
    pos: usize,
    done: bool,
}

impl<R: Read> Read for ArchiveFrames<'_, R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.pos == self.buf.len() {
            if self.done {
                return Ok(0);
            }
            match Message::read_from(self.stream)? {
                Some(Message::ArchiveData(data)) => {
                    self.buf = data;
                    self.pos = 0;
                }
                Some(Message::ArchiveEnd) => self.done = true,
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("expected archive data, got {other:?}"),
                    ));
                }
            }
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// Writes bytes as frames of one kind, at most [`DATA_CHUNK`] bytes each.
struct FrameWriter<'stream, W> {
    stream: &'stream mut W,
    frame: fn(Vec<u8>) -> Message,
    buf: Vec<u8>,
}

impl<'stream, W: Write> FrameWriter<'stream, W> {
    fn new(stream: &'stream mut W, frame: fn(Vec<u8>) -> Message) -> Self {
        FrameWriter {
            stream,
            frame,
            buf: Vec::with_capacity(DATA_CHUNK),
        }
    }

    /// Sends what is buffered; an empty buffer still sends one empty frame, so the host knows
    /// the file exists.
    fn finish(mut self) -> io::Result<()> {
        (self.frame)(std::mem::take(&mut self.buf)).write_to(self.stream)
    }
}

impl<W: Write> Write for FrameWriter<'_, W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let n = data.len().min(DATA_CHUNK - self.buf.len());
        self.buf.extend_from_slice(&data[..n]);
        if self.buf.len() == DATA_CHUNK {
            (self.frame)(std::mem::replace(
                &mut self.buf,
                Vec::with_capacity(DATA_CHUNK),
            ))
            .write_to(self.stream)?;
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
