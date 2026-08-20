//! Durable archive primitives for ModelKeep.
//!
//! The archive stores materialized files under an immutable commit directory.
//! A revision becomes visible only after all files and its manifest have been
//! written and synchronized to disk.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use sha2::{Digest, Sha256};

pub mod http;
pub mod importer;
pub mod singleflight;

static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub enum ArchiveError {
    Io(io::Error),
    InvalidPath(String),
    AlreadyPublished(PathBuf),
    IntegrityMismatch(String),
}

impl fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "archive I/O error: {error}"),
            Self::InvalidPath(path) => write!(f, "unsafe archive path: {path}"),
            Self::IntegrityMismatch(message) => write!(f, "archive integrity mismatch: {message}"),
            Self::AlreadyPublished(path) => {
                write!(f, "revision is already published: {}", path.display())
            }
        }
    }
}

impl std::error::Error for ArchiveError {}

impl From<io::Error> for ArchiveError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub type ArchiveResult<T> = Result<T, ArchiveError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedFile {
    pub path: PathBuf,
    pub size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub start: u64,
    pub end: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeError {
    Invalid,
    Unsatisfiable,
}

#[derive(Debug, Deserialize)]
struct Manifest {
    files: Vec<ManifestFile>,
}

#[derive(Debug, Deserialize)]
struct ManifestFile {
    path: String,
    size: u64,
    sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveFile {
    pub path: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishRequest {
    pub repo_id: String,
    pub requested_revision: String,
    pub commit: String,
    pub files: Vec<ArchiveFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFile {
    pub path: String,
    pub source: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourcePublishRequest {
    pub repo_id: String,
    pub requested_revision: String,
    pub commit: String,
    pub source_root: PathBuf,
    pub files: Vec<SourceFile>,
}

#[derive(Debug, Clone)]
pub struct Archive {
    root: PathBuf,
}

impl Archive {
    pub fn new(root: impl Into<PathBuf>) -> ArchiveResult<Self> {
        let root = root.into();
        fs::create_dir_all(root.join("models"))?;
        fs::create_dir_all(root.join("tmp"))?;
        Ok(Self { root })
    }

    /// Publishes one complete immutable revision.
    pub fn publish_revision(&self, request: PublishRequest) -> ArchiveResult<PathBuf> {
        let (namespace, name) = validate_repo_id(&request.repo_id)?;
        validate_component(&request.requested_revision)?;
        validate_revision(&request.commit)?;
        if request.files.is_empty() {
            return Err(ArchiveError::InvalidPath("revision has no files".into()));
        }

        let revisions = self
            .root
            .join("models")
            .join(namespace)
            .join(name)
            .join("revisions");
        fs::create_dir_all(&revisions)?;
        let published = revisions.join(&request.commit);
        if published.exists() {
            return Err(ArchiveError::AlreadyPublished(published));
        }

        let staging = self.staging_path(&request.commit);
        fs::create_dir_all(&staging)?;
        let result = self.write_revision(&staging, &request);
        if let Err(error) = result {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }

        if let Err(error) = fs::rename(&staging, &published) {
            let _ = fs::remove_dir_all(&staging);
            if published.exists() {
                return Err(ArchiveError::AlreadyPublished(published));
            }
            return Err(error.into());
        }
        sync_directory(&revisions)?;
        Ok(published)
    }

    /// Updates a mutable ref only after the target revision is published.
    pub fn update_ref(&self, repo_id: &str, reference: &str, commit: &str) -> ArchiveResult<()> {
        let (namespace, name) = validate_repo_id(repo_id)?;
        validate_component(reference)?;
        validate_revision(commit)?;
        let revision = self
            .root
            .join("models")
            .join(namespace)
            .join(name)
            .join("revisions")
            .join(commit);
        if !revision.is_dir() {
            return Err(
                io::Error::new(io::ErrorKind::NotFound, "revision is not published").into(),
            );
        }

        let refs = revision.parent().unwrap().parent().unwrap().join("refs");
        fs::create_dir_all(&refs)?;
        let temporary = refs.join(format!(".{reference}.part"));
        let mut file = File::create(&temporary)?;
        file.write_all(commit.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, refs.join(reference))?;
        sync_directory(&refs)?;
        Ok(())
    }

    pub fn revision_path(&self, repo_id: &str, commit: &str) -> ArchiveResult<PathBuf> {
        let (namespace, name) = validate_repo_id(repo_id)?;
        validate_revision(commit)?;
        Ok(self
            .root
            .join("models")
            .join(namespace)
            .join(name)
            .join("revisions")
            .join(commit))
    }

    pub fn resolve_file(
        &self,
        repo_id: &str,
        commit: &str,
        relative_path: &str,
    ) -> ArchiveResult<ResolvedFile> {
        let relative = validate_relative_file_path(relative_path)?;
        let revision = self.revision_path(repo_id, commit)?;
        let revision_root = fs::canonicalize(&revision)?;
        let candidate = fs::canonicalize(revision.join(relative))?;
        if !candidate.starts_with(&revision_root) || !candidate.is_file() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "archive file not found").into());
        }
        Ok(ResolvedFile {
            size: fs::metadata(&candidate)?.len(),
            path: candidate,
        })
    }

    pub fn verify_revision(&self, repo_id: &str, commit: &str) -> ArchiveResult<usize> {
        let revision = self.revision_path(repo_id, commit)?;
        let manifest_path = revision.join(".modelkeep-manifest.json");
        let manifest: Manifest = serde_json::from_slice(&fs::read(&manifest_path)?)
            .map_err(|error| ArchiveError::IntegrityMismatch(error.to_string()))?;
        let mut verified = 0;
        for entry in &manifest.files {
            let resolved = self.resolve_file(repo_id, commit, &entry.path)?;
            if resolved.size != entry.size {
                return Err(ArchiveError::IntegrityMismatch(entry.path.clone()));
            }
            let bytes = fs::read(&resolved.path)?;
            if sha256(&bytes) != entry.sha256 {
                return Err(ArchiveError::IntegrityMismatch(entry.path.clone()));
            }
            verified += 1;
        }
        Ok(verified)
    }

    pub fn publish_revision_from_directory(
        &self,
        request: SourcePublishRequest,
    ) -> ArchiveResult<PathBuf> {
        let (namespace, name) = validate_repo_id(&request.repo_id)?;
        validate_component(&request.requested_revision)?;
        validate_revision(&request.commit)?;

        if request.files.is_empty() {
            return Err(ArchiveError::InvalidPath("revision has no files".into()));
        }
        let source_root = fs::canonicalize(&request.source_root)?;
        if !source_root.is_dir() {
            return Err(ArchiveError::InvalidPath(
                request.source_root.display().to_string(),
            ));
        }
        let revisions = self
            .root
            .join("models")
            .join(namespace)
            .join(name)
            .join("revisions");
        fs::create_dir_all(&revisions)?;
        let published = revisions.join(&request.commit);
        if published.exists() {
            return Err(ArchiveError::AlreadyPublished(published));
        }
        let staging = self.staging_path(&request.commit);
        fs::create_dir_all(&staging)?;
        let result = self.write_source_revision(&staging, &source_root, &request);
        if let Err(error) = result {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
        if let Err(error) = fs::rename(&staging, &published) {
            let _ = fs::remove_dir_all(&staging);
            if published.exists() {
                return Err(ArchiveError::AlreadyPublished(published));
            }
            return Err(error.into());
        }
        sync_directory(&revisions)?;
        Ok(published)
    }

    fn staging_path(&self, commit: &str) -> PathBuf {
        let sequence = STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        self.root
            .join("tmp")
            .join(format!("revision-{commit}-{sequence}"))
    }

    fn write_revision(&self, staging: &Path, request: &PublishRequest) -> ArchiveResult<()> {
        let mut entries = Vec::with_capacity(request.files.len());
        for archive_file in &request.files {
            let relative = validate_relative_file_path(&archive_file.path)?;
            let destination = staging.join(relative);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&destination)?;
            file.write_all(&archive_file.bytes)?;
            file.sync_all()?;
            entries.push((
                archive_file.path.as_str(),
                archive_file.bytes.len() as u64,
                sha256(&archive_file.bytes),
            ));
        }

        let manifest = staging.join(".modelkeep-manifest.json");
        let mut file = File::create(manifest)?;
        write_manifest(
            &mut file,
            &request.repo_id,
            &request.requested_revision,
            &request.commit,
            &entries,
        )?;
        file.sync_all()?;
        sync_directory(staging)?;
        Ok(())
    }
    fn write_source_revision(
        &self,
        staging: &Path,
        source_root: &Path,
        request: &SourcePublishRequest,
    ) -> ArchiveResult<()> {
        let mut entries = Vec::with_capacity(request.files.len());
        for source_file in &request.files {
            let relative = validate_relative_file_path(&source_file.path)?;
            let source = fs::canonicalize(&source_file.source)?;
            if !source.starts_with(source_root) || !source.is_file() {
                return Err(ArchiveError::InvalidPath(source_file.path.clone()));
            }
            let destination = staging.join(relative);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut input = File::open(source)?;
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(destination)?;
            let mut hasher = Sha256::new();
            let mut buffer = [0u8; 1024 * 1024];
            let mut size = 0u64;
            loop {
                let read = input.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                output.write_all(&buffer[..read])?;
                hasher.update(&buffer[..read]);
                size += read as u64;
            }
            output.sync_all()?;
            entries.push((
                source_file.path.as_str(),
                size,
                hex_digest(hasher.finalize().as_slice()),
            ));
        }
        let mut file = File::create(staging.join(".modelkeep-manifest.json"))?;
        write_manifest(
            &mut file,
            &request.repo_id,
            &request.requested_revision,
            &request.commit,
            &entries,
        )?;
        file.sync_all()?;
        sync_directory(staging)?;
        Ok(())
    }
}

fn validate_repo_id(repo_id: &str) -> ArchiveResult<(&str, &str)> {
    let mut parts = repo_id.split('/');
    let namespace = parts.next().unwrap_or_default();
    let name = parts.next().unwrap_or_default();
    if parts.next().is_some() || namespace.is_empty() || name.is_empty() {
        return Err(ArchiveError::InvalidPath(repo_id.into()));
    }
    validate_component(namespace)?;
    validate_component(name)?;
    Ok((namespace, name))
}

fn validate_revision(revision: &str) -> ArchiveResult<()> {
    if revision.is_empty()
        || revision.len() > 128
        || !revision.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(ArchiveError::InvalidPath(revision.into()));
    }
    Ok(())
}

fn validate_component(value: &str) -> ArchiveResult<()> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || value.contains('/')
        || value.contains('\\')
        || value
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
    {
        return Err(ArchiveError::InvalidPath(value.into()));
    }
    Ok(())
}

fn validate_relative_file_path(value: &str) -> ArchiveResult<&Path> {
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || value
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(ArchiveError::InvalidPath(value.into()));
    }
    Ok(path)
}

fn write_manifest(
    output: &mut File,
    repo_id: &str,
    requested_revision: &str,
    commit: &str,
    entries: &[(&str, u64, String)],
) -> io::Result<()> {
    write!(
        output,
        "{{\"version\":1,\"repo_type\":\"model\",\"repo_id\":\"{}\",\"requested_revision\":\"{}\",\"commit\":\"{}\",\"archived_at\":{},\"files\":[",
        json_escape(repo_id),
        json_escape(requested_revision),
        commit,
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
    )?;
    for (index, (path, size, digest)) in entries.iter().enumerate() {
        if index > 0 {
            output.write_all(b",")?;
        }
        write!(
            output,
            "{{\"path\":\"{}\",\"size\":{},\"sha256\":\"{}\"}}",
            json_escape(path),
            size,
            digest
        )?;
    }
    output.write_all(b"]}\n")
}

fn json_escape(value: &str) -> String {
    value
        .chars()
        .flat_map(|character| match character {
            '"' => "\\\"".chars().collect::<Vec<_>>(),
            '\\' => "\\\\".chars().collect(),
            _ => vec![character],
        })
        .collect()
}

fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

pub fn parse_range(value: &str, size: u64) -> Result<Option<ByteRange>, RangeError> {
    if !value.starts_with("bytes=") || value[6..].contains(',') {
        return Err(RangeError::Invalid);
    }
    let value = &value[6..];
    let (start, end) = value.split_once('-').ok_or(RangeError::Invalid)?;
    if size == 0 {
        return Err(RangeError::Unsatisfiable);
    }
    if start.is_empty() {
        let suffix = end.parse::<u64>().map_err(|_| RangeError::Invalid)?;
        if suffix == 0 {
            return Err(RangeError::Unsatisfiable);
        }
        return Ok(Some(ByteRange {
            start: size.saturating_sub(suffix),
            end: size - 1,
        }));
    }
    let start = start.parse::<u64>().map_err(|_| RangeError::Invalid)?;
    if start >= size {
        return Err(RangeError::Unsatisfiable);
    }
    let end = if end.is_empty() {
        size - 1
    } else {
        end.parse::<u64>()
            .map_err(|_| RangeError::Invalid)?
            .min(size - 1)
    };
    if start > end {
        return Err(RangeError::Unsatisfiable);
    }
    Ok(Some(ByteRange { start, end }))
}

fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive() -> (Archive, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        (archive, directory)
    }

    fn request(commit: &str, content: &[u8]) -> PublishRequest {
        PublishRequest {
            repo_id: "org/model".into(),
            requested_revision: "main".into(),
            commit: commit.into(),
            files: vec![ArchiveFile {
                path: "config.json".into(),
                bytes: content.into(),
            }],
        }
    }

    #[test]
    fn publishes_revision_and_manifest() {
        let (archive, _) = archive();
        let path = archive
            .publish_revision(request("aaaaaaaa", b"{}"))
            .unwrap();
        assert_eq!(fs::read(path.join("config.json")).unwrap(), b"{}");
        let manifest = fs::read_to_string(path.join(".modelkeep-manifest.json")).unwrap();
        assert!(manifest.contains("\"commit\":\"aaaaaaaa\""));
        assert!(manifest.contains("\"size\":2"));
    }

    #[test]
    fn publishes_from_directory_without_loading_source_into_request() {
        let (archive, directory) = archive();
        let source = directory.path().join("source");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::write(source.join("nested/model.bin"), vec![7u8; 1024 * 1024 + 3]).unwrap();
        let published = archive
            .publish_revision_from_directory(SourcePublishRequest {
                repo_id: "org/model".into(),
                requested_revision: "main".into(),
                commit: "cccccccc".into(),
                source_root: source.clone(),
                files: vec![SourceFile {
                    path: "nested/model.bin".into(),
                    source: source.join("nested/model.bin"),
                }],
            })
            .unwrap();
        assert_eq!(
            fs::metadata(published.join("nested/model.bin"))
                .unwrap()
                .len(),
            1024 * 1024 + 3
        );
        assert_eq!(archive.verify_revision("org/model", "cccccccc").unwrap(), 1);
    }

    #[test]
    fn published_revision_cannot_be_overwritten() {
        let (archive, _) = archive();
        archive
            .publish_revision(request("aaaaaaaa", b"one"))
            .unwrap();
        assert!(matches!(
            archive.publish_revision(request("aaaaaaaa", b"two")),
            Err(ArchiveError::AlreadyPublished(_))
        ));
        let path = archive.revision_path("org/model", "aaaaaaaa").unwrap();
        assert_eq!(fs::read(path.join("config.json")).unwrap(), b"one");
    }

    #[test]
    fn ref_update_keeps_old_revision() {
        let (archive, _) = archive();
        archive
            .publish_revision(request("aaaaaaaa", b"one"))
            .unwrap();
        archive
            .publish_revision(request("bbbbbbbb", b"two"))
            .unwrap();
        archive.update_ref("org/model", "main", "aaaaaaaa").unwrap();
        archive.update_ref("org/model", "main", "bbbbbbbb").unwrap();
        let root = archive.revision_path("org/model", "aaaaaaaa").unwrap();
        assert!(root.is_dir());
        let reference = root.parent().unwrap().parent().unwrap().join("refs/main");
        assert_eq!(fs::read_to_string(reference).unwrap(), "bbbbbbbb");
    }

    #[test]
    fn rejects_unsafe_paths() {
        let (archive, _) = archive();
        let mut bad = request("aaaaaaaa", b"bad");
        bad.repo_id = "../escape/model".into();
        assert!(matches!(
            archive.publish_revision(bad),
            Err(ArchiveError::InvalidPath(_))
        ));

        let mut bad = request("bbbbbbbb", b"bad");
        bad.files[0].path = "../escape".into();
        assert!(matches!(
            archive.publish_revision(bad),
            Err(ArchiveError::InvalidPath(_))
        ));
    }

    #[test]
    fn resolves_only_files_inside_published_revision() {
        let (archive, _) = archive();
        archive
            .publish_revision(request("aaaaaaaa", b"{}"))
            .unwrap();
        let resolved = archive
            .resolve_file("org/model", "aaaaaaaa", "config.json")
            .unwrap();
        assert_eq!(resolved.size, 2);
        assert!(resolved.path.ends_with("config.json"));
        assert!(archive
            .resolve_file("org/model", "aaaaaaaa", "missing.json")
            .is_err());
    }

    #[test]
    fn parses_single_byte_ranges() {
        assert_eq!(
            parse_range("bytes=0-9", 100),
            Ok(Some(ByteRange { start: 0, end: 9 }))
        );
        assert_eq!(
            parse_range("bytes=90-", 100),
            Ok(Some(ByteRange { start: 90, end: 99 }))
        );
        assert_eq!(
            parse_range("bytes=-10", 100),
            Ok(Some(ByteRange { start: 90, end: 99 }))
        );
        assert_eq!(
            parse_range("bytes=90-120", 100),
            Ok(Some(ByteRange { start: 90, end: 99 }))
        );
        assert_eq!(
            parse_range("bytes=100-", 100),
            Err(RangeError::Unsatisfiable)
        );
        assert_eq!(parse_range("bytes=0-1,4-5", 100), Err(RangeError::Invalid));
    }

    #[test]
    fn failed_publish_leaves_no_staging_directory() {
        let (archive, directory) = archive();
        let mut bad = request("aaaaaaaa", b"bad");
        bad.files[0].path = "../escape".into();
        assert!(archive.publish_revision(bad).is_err());
        let entries: Vec<_> = fs::read_dir(directory.path().join("tmp"))
            .unwrap()
            .collect();
        assert!(entries.is_empty());
    }
}
