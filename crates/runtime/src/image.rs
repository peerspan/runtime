// SPDX-License-Identifier: Apache-2.0
// Copyright The Peerspan Authors

//! OCI images as guest root filesystems.
//!
//! An image is pulled once from its registry, checked against the digests its manifest names,
//! unpacked, and kept under its manifest's digest; a tag maps to the digest it named when it
//! was pulled, until it is pulled again. Layers are unpacked by a child that has chrooted into
//! the new root, so no entry can reach outside it. The VMM serves the root as an unprivileged
//! user, so everything in it is made readable by all, and the guest sees those modes.

use std::fs::{self, File};
use std::io::{self, Read, Seek, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use rustix::fs::{FlockOperation, flock};
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Where images are kept unless set otherwise.
pub const DEFAULT_CACHE: &str = "/var/cache/peerspan-runtime/images";
const PLATFORM_OS: &str = "linux";
const PLATFORM_ARCH: &str = "amd64";
const MANIFEST_TYPES: &str = "application/vnd.oci.image.index.v1+json, \
    application/vnd.docker.distribution.manifest.list.v2+json, \
    application/vnd.oci.image.manifest.v1+json, \
    application/vnd.docker.distribution.manifest.v2+json";

/// An image ready to boot: its root and the environment its configuration sets. It holds a
/// shared lock on the image, so `prune` leaves it alone while this lasts.
#[derive(Debug)]
pub struct Image {
    pub rootfs: PathBuf,
    pub env: Vec<(String, String)>,
    _lock: File,
}

/// An image in the cache, as `images` lists it.
#[derive(Debug)]
pub struct Cached {
    pub digest: String,
    /// The references whose tags name it.
    pub tags: Vec<String>,
    pub bytes: u64,
    /// When a run last booted it.
    pub last_used: SystemTime,
}

/// An image reference: `name[:tag]` or `name@digest`, with Docker Hub as the default registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    registry: String,
    repository: String,
    /// A tag or a `sha256:` digest.
    reference: String,
}

impl Reference {
    /// Parses a reference as Docker does: `node:22` is `docker.io/library/node:22`.
    ///
    /// # Errors
    ///
    /// Fails for an empty name or a malformed digest.
    pub fn parse(text: &str) -> anyhow::Result<Reference> {
        let (name, reference) = if let Some((name, digest)) = text.split_once('@') {
            if !is_digest(digest) {
                bail!("{text}: a digest must be sha256: and 64 lowercase hex digits");
            }
            // A tag beside the digest, as in `node:22@sha256:...`, is ignored, as Docker does.
            let name = match name.rsplit_once(':') {
                Some((untagged, tag)) if !tag.contains('/') => untagged,
                _ => name,
            };
            (name, digest.to_owned())
        } else {
            match text.rsplit_once(':') {
                Some((name, tag)) if !tag.contains('/') => (name, tag.to_owned()),
                _ => (text, "latest".to_owned()),
            }
        };
        let (registry, repository) = match name.split_once('/') {
            Some((first, rest))
                if first.contains('.') || first.contains(':') || first == "localhost" =>
            {
                (first.to_owned(), rest.to_owned())
            }
            _ => ("docker.io".to_owned(), name.to_owned()),
        };
        let repository = if registry == "docker.io" && !repository.contains('/') {
            format!("library/{repository}")
        } else {
            repository
        };
        if repository.is_empty() || reference.is_empty() {
            bail!("{text} is not an image reference");
        }
        Ok(Reference {
            registry,
            repository,
            reference,
        })
    }

    fn api(&self) -> String {
        let host = if self.registry == "docker.io" {
            "registry-1.docker.io"
        } else {
            &self.registry
        };
        format!("https://{host}/v2/{}", self.repository)
    }

    /// A file name for the tag's entry in the cache.
    fn key(&self) -> String {
        format!("{}/{}:{}", self.registry, self.repository, self.reference).replace('/', "%")
    }
}

impl std::fmt::Display for Reference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let separator = if is_digest(&self.reference) { '@' } else { ':' };
        write!(
            f,
            "{}/{}{separator}{}",
            self.registry, self.repository, self.reference
        )
    }
}

/// Returns the image `reference` names, pulling it first if the cache does not have it or
/// `pull` is set.
///
/// # Errors
///
/// Fails if the image cannot be pulled, checked or unpacked.
pub fn get(cache: &Path, reference: &Reference, pull: bool) -> anyhow::Result<Image> {
    let _cache = lock(&cache_lock_path(cache), FlockOperation::LockShared)?;
    let digest = match (pull, cached_digest(cache, reference)) {
        (false, Some(digest)) => digest,
        _ => self::pull(cache, reference)?,
    };
    open(cache, &digest)
}

/// An image's environment with `overrides` applied, a later name replacing an earlier one.
pub fn merge_env(
    mut base: Vec<(String, String)>,
    overrides: impl IntoIterator<Item = (String, String)>,
) -> Vec<(String, String)> {
    for (name, value) in overrides {
        base.retain(|(existing, _)| *existing != name);
        base.push((name, value));
    }
    base
}

/// The digest of the image `reference` names here: what a tag, or an index's digest, resolved
/// to when it was pulled, or a manifest's digest itself.
fn cached_digest(cache: &Path, reference: &Reference) -> Option<String> {
    let digest = match fs::read_to_string(cache.join("tags").join(reference.key())) {
        Ok(digest) => digest.trim().to_owned(),
        Err(_) if is_digest(&reference.reference) => reference.reference.clone(),
        Err(_) => return None,
    };
    image_dir(cache, &digest).is_dir().then_some(digest)
}

fn open(cache: &Path, digest: &str) -> anyhow::Result<Image> {
    let dir = image_dir(cache, digest);
    let image_lock = lock(&dir, FlockOperation::LockShared)?;
    mark_used(&dir).with_context(|| format!("image {digest}"))?;
    let config: ImageConfig = serde_json::from_slice(
        &fs::read(dir.join("config.json")).with_context(|| format!("image {digest}"))?,
    )
    .with_context(|| format!("image {digest}: config.json"))?;
    let env = config
        .config
        .and_then(|c| c.env)
        .unwrap_or_default()
        .iter()
        .filter_map(|pair| pair.split_once('='))
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect();
    Ok(Image {
        rootfs: dir.join("rootfs"),
        env,
        _lock: image_lock,
    })
}

const LAST_USED: &str = "last-used";

/// Records that a run is booting the image in `dir`: the file's modification time is what
/// `prune --unused-days` goes by.
fn mark_used(dir: &Path) -> io::Result<()> {
    File::create(dir.join(LAST_USED)).map(drop)
}

impl Image {
    /// Records that another run is booting this image, for one held across many runs, as
    /// serve holds its image.
    pub fn mark_used(&self) {
        if let Some(dir) = self.rootfs.parent() {
            let _ = mark_used(dir);
        }
    }
}

fn cache_lock_path(cache: &Path) -> PathBuf {
    cache.join(".lock")
}

/// Opens `path`, a file or directory, creating a missing lock file, and takes `operation`, a
/// flock(2) operation, on it.
fn lock(path: &Path, operation: FlockOperation) -> anyhow::Result<File> {
    let file = if path.is_dir() {
        File::open(path)
    } else {
        if let Some(parent) = path.parent() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
        }
        File::options().create(true).append(true).open(path)
    }
    .with_context(|| format!("opening {}", path.display()))?;
    flock(&file, operation).with_context(|| format!("locking {}", path.display()))?;
    Ok(file)
}

/// The images in `cache`, with the tags that name each.
///
/// # Errors
///
/// Fails if the cache cannot be read.
pub fn list(cache: &Path) -> anyhow::Result<Vec<Cached>> {
    let tags = tag_map(cache);
    let mut images = Vec::new();
    let entries = match fs::read_dir(cache) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(images),
        Err(e) => return Err(e).with_context(|| format!("reading {}", cache.display())),
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(hex) = name.strip_prefix("sha256-") else {
            continue;
        };
        let digest = format!("sha256:{hex}");
        let last_used = fs::metadata(entry.path().join(LAST_USED))
            .or_else(|_| entry.metadata())
            .and_then(|m| m.modified())
            .unwrap_or(UNIX_EPOCH);
        images.push(Cached {
            tags: tags
                .iter()
                .filter(|(_, d)| **d == digest)
                .map(|(key, _)| key.replace('%', "/"))
                .collect(),
            bytes: disk_usage(&entry.path()),
            digest,
            last_used,
        });
    }
    images.sort_by_key(|image| std::cmp::Reverse(image.last_used));
    Ok(images)
}

/// Removes images no tag names, and with `unused_for`, those no run has booted for that long,
/// with their tags. Images in use are kept, as are leftovers of pulls in progress. Returns what
/// was removed.
///
/// # Errors
///
/// Fails if the cache cannot be locked or read.
pub fn prune(cache: &Path, unused_for: Option<Duration>) -> anyhow::Result<Vec<Cached>> {
    if !cache.is_dir() {
        return Ok(Vec::new());
    }
    // No pull is storing or tagging an image while this holds.
    let _cache = lock(&cache_lock_path(cache), FlockOperation::LockExclusive)?;
    for entry in fs::read_dir(cache)?.flatten() {
        if entry.file_name().to_string_lossy().starts_with("tmp-") {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
    let now = SystemTime::now();
    let mut removed = Vec::new();
    for image in list(cache)? {
        let unused = unused_for
            .is_some_and(|limit| now.duration_since(image.last_used).unwrap_or_default() >= limit);
        if !image.tags.is_empty() && !unused {
            continue;
        }
        let dir = image_dir(cache, &image.digest);
        // A run holds the image: keep it.
        let Ok(_held) = lock(&dir, FlockOperation::NonBlockingLockExclusive) else {
            continue;
        };
        for tag in &image.tags {
            let _ = fs::remove_file(cache.join("tags").join(tag.replace('/', "%")));
        }
        fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
        removed.push(image);
    }
    Ok(removed)
}

/// Each tag file's name and the digest it holds.
fn tag_map(cache: &Path) -> Vec<(String, String)> {
    let Ok(entries) = fs::read_dir(cache.join("tags")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .filter_map(|e| {
            let digest = fs::read_to_string(e.path()).ok()?;
            Some((
                e.file_name().to_string_lossy().into_owned(),
                digest.trim().to_owned(),
            ))
        })
        .collect()
}

/// The bytes a directory's files take on disk, links not followed.
fn disk_usage(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    let mut bytes = meta.blocks() * 512;
    if meta.is_dir() {
        for entry in fs::read_dir(path).into_iter().flatten().flatten() {
            bytes += disk_usage(&entry.path());
        }
    }
    bytes
}

fn image_dir(cache: &Path, digest: &str) -> PathBuf {
    cache.join(digest.replace(':', "-"))
}

/// Pulls `reference` into the cache and returns its manifest's digest.
///
/// # Errors
///
/// Fails if the registry refuses, a blob does not match its digest, or unpacking fails.
pub fn pull(cache: &Path, reference: &Reference) -> anyhow::Result<String> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(cache.join("tags"))
        .with_context(|| format!("creating {}", cache.display()))?;
    let _cache = lock(&cache_lock_path(cache), FlockOperation::LockShared)?;
    let mut registry = Registry::new(reference);
    let (digest, manifest) = registry.manifest(&reference.reference)?;
    let dir = image_dir(cache, &digest);
    if !dir.is_dir() {
        crate::log::info("pulling an image", &[("image", &reference)]);
        let staging = cache.join(format!("tmp-{}-{}", std::process::id(), nanos()));
        let result = unpack(&mut registry, &manifest, &staging).and_then(|()| {
            match fs::rename(&staging, &dir) {
                Ok(()) => Ok(()),
                // Another pull of the same image finished first.
                Err(_) if dir.is_dir() => Ok(()),
                Err(e) => Err(e).context("storing the image"),
            }
        });
        let _ = fs::remove_dir_all(&staging);
        result.with_context(|| format!("pulling {reference}"))?;
    }
    // Recorded for digests too: an index's digest resolves to a platform's manifest.
    let tags = cache.join("tags");
    let staging = tags.join(format!(".{}-{}", reference.key(), nanos()));
    fs::write(&staging, &digest)?;
    fs::rename(&staging, tags.join(reference.key()))?;
    Ok(digest)
}

/// Downloads `manifest`'s configuration and layers into `staging` and unpacks the layers.
fn unpack(registry: &mut Registry, manifest: &Manifest, staging: &Path) -> anyhow::Result<()> {
    fs::DirBuilder::new().mode(0o700).create(staging)?;
    registry.blob(&manifest.config.digest, &staging.join("config.json"))?;
    let mut layers = Vec::new();
    for (i, layer) in manifest.layers.iter().enumerate() {
        let compressed = match layer.media_type.as_str() {
            t if t.ends_with("tar+gzip") || t.ends_with("tar.gzip") => true,
            t if t.ends_with(".tar") || t.ends_with("tar") => false,
            t if t.contains("zstd") => bail!("zstd-compressed layers are not supported yet ({t})"),
            t => bail!("unsupported layer type {t}"),
        };
        let path = staging.join(format!("layer-{i}"));
        registry.blob(&layer.digest, &path)?;
        layers.push((path, compressed));
    }
    let rootfs = staging.join("rootfs");
    fs::DirBuilder::new().mode(0o755).create(&rootfs)?;
    let mut helper = Command::new(std::env::current_exe().context("locating this executable")?);
    helper.arg("unpack-layers").arg("--root").arg(&rootfs);
    for (path, compressed) in &layers {
        let kind = if *compressed { "gzip" } else { "tar" };
        helper
            .arg("--layer")
            .arg(format!("{kind}:{}", path.display()));
    }
    let status = helper.status().context("starting the layer unpacker")?;
    for (path, _) in &layers {
        let _ = fs::remove_file(path);
    }
    if !status.success() {
        bail!("unpacking the layers failed ({status})");
    }
    Ok(())
}

/// A registry session for one repository, holding its token once one is needed.
struct Registry<'r> {
    reference: &'r Reference,
    agent: ureq::Agent,
    token: Option<String>,
}

impl<'r> Registry<'r> {
    fn new(reference: &'r Reference) -> Self {
        Registry {
            reference,
            agent: ureq::AgentBuilder::new()
                .user_agent(concat!("peerspan-runtime/", env!("CARGO_PKG_VERSION")))
                .build(),
            token: None,
        }
    }

    /// GETs `url`, fetching an anonymous token when the registry asks for one.
    fn get(&mut self, url: &str, accept: &str) -> anyhow::Result<ureq::Response> {
        for _ in 0..2 {
            let mut request = self.agent.get(url).set("Accept", accept);
            if let Some(token) = &self.token {
                request = request.set("Authorization", &format!("Bearer {token}"));
            }
            match request.call() {
                Ok(response) => return Ok(response),
                Err(ureq::Error::Status(401, response)) if self.token.is_none() => {
                    let challenge = response.header("www-authenticate").unwrap_or("").to_owned();
                    self.token = Some(self.authenticate(&challenge)?);
                }
                Err(ureq::Error::Status(code, _)) => {
                    bail!("{} answered {code} for {url}", self.reference.registry)
                }
                Err(e) => return Err(e).with_context(|| format!("fetching {url}")),
            }
        }
        bail!("{} refused the token it issued", self.reference.registry)
    }

    /// Fetches an anonymous pull token from the realm a `Bearer` challenge names.
    fn authenticate(&self, challenge: &str) -> anyhow::Result<String> {
        let Some(params) = challenge.strip_prefix("Bearer ") else {
            bail!(
                "{} needs credentials, which are not supported yet",
                self.reference.registry
            );
        };
        let param = |name: &str| {
            params.split(',').find_map(|part| {
                let (key, value) = part.trim().split_once('=')?;
                (key == name).then(|| value.trim_matches('"').to_owned())
            })
        };
        let Some(realm) = param("realm") else {
            bail!(
                "{}: token challenge without a realm",
                self.reference.registry
            );
        };
        let scope = param("scope")
            .unwrap_or_else(|| format!("repository:{}:pull", self.reference.repository));
        let mut request = self.agent.get(&realm).query("scope", &scope);
        if let Some(service) = param("service") {
            request = request.query("service", &service);
        }
        let response: TokenResponse = serde_json::from_reader(
            request
                .call()
                .with_context(|| format!("fetching a token from {realm}"))?
                .into_reader()
                .take(1 << 20),
        )
        .context("reading the token")?;
        response
            .token
            .or(response.access_token)
            .context("the token response has no token")
    }

    /// The image manifest for `reference`, a tag or digest, choosing linux/amd64 from an
    /// index, with its digest.
    fn manifest(&mut self, reference: &str) -> anyhow::Result<(String, Manifest)> {
        let url = format!("{}/manifests/{reference}", self.reference.api());
        let mut body = Vec::new();
        self.get(&url, MANIFEST_TYPES)?
            .into_reader()
            .take(4 << 20)
            .read_to_end(&mut body)?;
        let digest = format!("sha256:{:x}", Sha256::digest(&body));
        if is_digest(reference) && digest != reference {
            bail!("the manifest for {reference} has digest {digest}");
        }
        let document: Document = serde_json::from_slice(&body).context("reading the manifest")?;
        match document {
            Document {
                manifests: Some(entries),
                ..
            } => {
                let Some(entry) = entries.iter().find(|m| {
                    m.platform
                        .as_ref()
                        .is_some_and(|p| p.os == PLATFORM_OS && p.architecture == PLATFORM_ARCH)
                }) else {
                    bail!(
                        "{} has no {PLATFORM_OS}/{PLATFORM_ARCH} image",
                        self.reference
                    );
                };
                let digest = entry.digest.clone();
                self.manifest(&digest)
            }
            Document {
                config: Some(config),
                layers: Some(layers),
                ..
            } => Ok((digest, Manifest { config, layers })),
            _ => bail!(
                "{} is neither an image index nor a manifest",
                self.reference
            ),
        }
    }

    /// Downloads blob `digest` to `path`, checking its digest.
    fn blob(&mut self, digest: &str, path: &Path) -> anyhow::Result<()> {
        if !is_digest(digest) {
            bail!("unsupported blob digest {digest}");
        }
        let url = format!("{}/blobs/{digest}", self.reference.api());
        let mut reader = self.get(&url, "*/*")?.into_reader();
        let mut file = File::create(path)?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0; 256 << 10];
        loop {
            let n = reader
                .read(&mut buf)
                .with_context(|| format!("downloading {digest}"))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            file.write_all(&buf[..n])?;
        }
        let got = format!("sha256:{:x}", hasher.finalize());
        if got != digest {
            bail!("blob {digest} arrived with digest {got}");
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    token: Option<String>,
    access_token: Option<String>,
}

/// An image index or an image manifest; which one is told by the fields present.
#[derive(Deserialize)]
struct Document {
    manifests: Option<Vec<IndexEntry>>,
    config: Option<Descriptor>,
    layers: Option<Vec<Descriptor>>,
}

#[derive(Deserialize)]
struct IndexEntry {
    digest: String,
    platform: Option<Platform>,
}

#[derive(Deserialize)]
struct Platform {
    os: String,
    architecture: String,
}

#[derive(Deserialize)]
struct Descriptor {
    #[serde(rename = "mediaType", default)]
    media_type: String,
    digest: String,
}

struct Manifest {
    config: Descriptor,
    layers: Vec<Descriptor>,
}

#[derive(Deserialize)]
struct ImageConfig {
    config: Option<RunConfig>,
}

#[derive(Deserialize)]
struct RunConfig {
    #[serde(rename = "Env")]
    env: Option<Vec<String>>,
}

fn is_digest(text: &str) -> bool {
    text.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

fn nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

/// The `unpack-layers` child: opens each layer, chroots into `root`, and applies the layers in
/// order, then makes everything readable by all.
///
/// # Errors
///
/// Fails if a layer cannot be opened or read, or the chroot fails.
pub fn unpack_layers(root: &Path, layers: &[(PathBuf, bool)]) -> anyhow::Result<()> {
    let files = layers
        .iter()
        .map(|(path, gzip)| Ok((File::open(path)?, *gzip)))
        .collect::<io::Result<Vec<_>>>()
        .context("opening the layers")?;
    std::os::unix::fs::chroot(root).context("chroot into the new root")?;
    std::env::set_current_dir("/")?;
    for (file, gzip) in files {
        let mut open = || -> io::Result<Box<dyn Read>> {
            let mut file = file.try_clone()?;
            file.rewind()?;
            let file = io::BufReader::new(file);
            Ok(if gzip {
                Box::new(flate2::read::GzDecoder::new(file))
            } else {
                Box::new(file)
            })
        };
        apply_layer(Path::new("/"), &mut open).context("applying a layer")?;
    }
    make_readable(Path::new("/"))
}

/// Applies one layer to `root`, which is `/` once chrooted. The layer is read twice, from
/// `open`: first for its whiteouts, which remove what lower layers made wherever they come in
/// the archive, then for its entries.
fn apply_layer(
    root: &Path,
    open: &mut dyn FnMut() -> io::Result<Box<dyn Read>>,
) -> anyhow::Result<()> {
    let mut archive = tar::Archive::new(open()?);
    for entry in archive.entries()? {
        let entry = entry?;
        let Some((path, whiteout)) = layer_path(root, &entry)? else {
            continue;
        };
        match whiteout {
            Some(Whiteout::Opaque) => {
                let dir = path.parent().unwrap_or(root);
                for child in fs::read_dir(dir).into_iter().flatten().flatten() {
                    remove(&child.path());
                }
            }
            Some(Whiteout::Removed(hidden)) => remove(&hidden),
            None => {}
        }
    }

    let mut archive = tar::Archive::new(open()?);
    archive.set_preserve_permissions(true);
    archive.set_preserve_ownerships(true);
    archive.set_preserve_mtime(true);
    archive.set_overwrite(true);
    archive.set_unpack_xattrs(false);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let Some((path, None)) = layer_path(root, &entry)? else {
            continue;
        };
        let kind = entry.header().entry_type();
        if !(kind.is_file() || kind.is_dir() || kind.is_symlink() || kind.is_hard_link()) {
            // Devices and FIFOs: the guest's /dev comes from devtmpfs.
            continue;
        }
        fs::create_dir_all(path.parent().unwrap_or(root))?;
        if let Ok(existing) = fs::symlink_metadata(&path)
            && !(kind.is_dir() && existing.is_dir())
        {
            remove(&path);
        }
        entry
            .unpack(&path)
            .with_context(|| format!("unpacking {}", path.display()))?;
    }
    Ok(())
}

/// What a whiteout entry removes from lower layers.
enum Whiteout {
    /// Everything already in the entry's directory.
    Opaque,
    /// The path it names.
    Removed(PathBuf),
}

/// An entry's path under `root`, and the whiteout it is, if any; None for a path that climbs
/// out with `..`.
fn layer_path<R: Read>(
    root: &Path,
    entry: &tar::Entry<'_, R>,
) -> io::Result<Option<(PathBuf, Option<Whiteout>)>> {
    let Some(path) = clean_path(&entry.path()?) else {
        return Ok(None);
    };
    let path = root.join(path.strip_prefix("/").unwrap_or(&path));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let whiteout = if name == ".wh..wh..opq" {
        Some(Whiteout::Opaque)
    } else {
        name.strip_prefix(".wh.")
            .map(|hidden| Whiteout::Removed(path.parent().unwrap_or(root).join(hidden)))
    };
    Ok(Some((path, whiteout)))
}

/// The entry's path made absolute, or None if it climbs with `..`.
fn clean_path(path: &Path) -> Option<PathBuf> {
    let mut clean = PathBuf::from("/");
    for part in path.components() {
        match part {
            std::path::Component::Normal(part) => clean.push(part),
            std::path::Component::CurDir | std::path::Component::RootDir => {}
            _ => return None,
        }
    }
    (clean != Path::new("/")).then_some(clean)
}

fn remove(path: &Path) {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => {
            let _ = fs::remove_dir_all(path);
        }
        Ok(_) => {
            let _ = fs::remove_file(path);
        }
        Err(_) => {}
    }
}

/// Adds read permission for all, and search permission for all on directories and on files
/// that anyone may execute, as `chmod -R a+rX` does. Links are left alone.
fn make_readable(dir: &Path) -> anyhow::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        if meta.file_type().is_symlink() {
            continue;
        }
        let mode = meta.permissions().mode();
        let search = if meta.is_dir() || mode & 0o111 != 0 {
            0o111
        } else {
            0
        };
        let wanted = mode | 0o444 | search;
        if wanted != mode {
            fs::set_permissions(entry.path(), fs::Permissions::from_mode(wanted & 0o7777))?;
        }
        if meta.is_dir() {
            make_readable(&entry.path())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_references_as_docker_does() {
        let r = Reference::parse("node:22").unwrap();
        assert_eq!(r.to_string(), "docker.io/library/node:22");
        assert_eq!(r.api(), "https://registry-1.docker.io/v2/library/node");
        assert_eq!(Reference::parse("alpine").unwrap().reference, "latest");
        let r = Reference::parse("ghcr.io/org/tool:1.2").unwrap();
        assert_eq!(
            (r.registry.as_str(), r.repository.as_str()),
            ("ghcr.io", "org/tool")
        );
        let r = Reference::parse("localhost:5000/x").unwrap();
        assert_eq!(
            (r.registry.as_str(), r.reference.as_str()),
            ("localhost:5000", "latest")
        );
        let digest = format!("sha256:{}", "a".repeat(64));
        let r = Reference::parse(&format!("user/app@{digest}")).unwrap();
        assert_eq!(r.to_string(), format!("docker.io/user/app@{digest}"));
        assert!(Reference::parse("app@sha256:short").is_err());
        let r = Reference::parse(&format!("node:22@{digest}")).unwrap();
        assert_eq!(r.to_string(), format!("docker.io/library/node@{digest}"));
        let r = Reference::parse(&format!("localhost:5000/app:1@{digest}")).unwrap();
        assert_eq!(
            (r.registry.as_str(), r.repository.as_str()),
            ("localhost:5000", "app")
        );
    }

    fn layer(entries: &[(&str, Option<&str>)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, content) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_uid(rustix::process::getuid().as_raw().into());
            header.set_gid(rustix::process::getgid().as_raw().into());
            header.set_mtime(0);
            match content {
                Some(data) => {
                    header.set_entry_type(tar::EntryType::Regular);
                    header.set_size(data.len() as u64);
                    header.set_mode(0o600);
                }
                None => {
                    header.set_entry_type(tar::EntryType::Directory);
                    header.set_size(0);
                    header.set_mode(0o700);
                }
            }
            builder
                .append_data(&mut header, path, content.unwrap_or("").as_bytes())
                .unwrap();
        }
        builder.into_inner().unwrap()
    }

    fn apply(root: &Path, layer: &[u8]) {
        let mut open =
            || -> io::Result<Box<dyn Read>> { Ok(Box::new(io::Cursor::new(layer.to_vec()))) };
        apply_layer(root, &mut open).unwrap();
    }

    fn names(root: &Path, dir: &str) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(root.join(dir))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn whiteouts_affect_only_lower_layers_wherever_they_come() {
        let root = std::env::temp_dir().join(format!("peerspan-layers-{}", nanos()));
        fs::create_dir(&root).unwrap();
        apply(
            &root,
            &layer(&[("a/sub/old", Some("old")), ("b/keep/old", Some("old"))]),
        );
        apply(
            &root,
            &layer(&[
                // Before the marker, and with no entry for its directory.
                ("a/sub/new", Some("new")),
                ("a/.wh..wh..opq", Some("")),
                // A directory the layer lists itself still loses what was under it.
                ("b/keep/", None),
                ("b/.wh..wh..opq", Some("")),
            ]),
        );
        assert_eq!(names(&root, "a/sub"), ["new"]);
        assert!(names(&root, "b/keep").is_empty());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn whiteouts_remove_what_lower_layers_made() {
        let root = std::env::temp_dir().join(format!("peerspan-layers-{}", nanos()));
        fs::create_dir(&root).unwrap();
        let lower = layer(&[
            ("a/x", Some("x")),
            ("a/y", Some("y")),
            ("b/old", Some("old")),
            ("c/", None),
        ]);
        let upper = layer(&[
            ("a/.wh.x", Some("")),
            ("b/.wh..wh..opq", Some("")),
            ("b/new", Some("new")),
            (".wh.c", Some("")),
        ]);
        apply(&root, &lower);
        apply(&root, &upper);
        assert_eq!(names(&root, "a"), ["y"]);
        assert_eq!(names(&root, "b"), ["new"]);
        assert!(!root.join("c").exists());
        make_readable(&root).unwrap();
        let mode = fs::metadata(root.join("a/y")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o644);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn layer_paths_cannot_climb_out() {
        assert_eq!(
            clean_path(Path::new("./etc/x")),
            Some(PathBuf::from("/etc/x"))
        );
        assert_eq!(
            clean_path(Path::new("/usr/bin")),
            Some(PathBuf::from("/usr/bin"))
        );
        assert_eq!(clean_path(Path::new("a/../../etc")), None);
        assert_eq!(clean_path(Path::new("./")), None);
    }
}
