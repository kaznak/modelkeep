//! Durable archive primitives for ModelKeep.
//!
//! The archive stores materialized files under an immutable commit directory.
//! A revision becomes visible only after all files and its manifest have been
//! written and synchronized to disk.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub mod admin;
pub mod http;
pub mod importer;
pub mod pullthrough;
pub mod singleflight;
pub mod upstream;

static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const STAGING_LEASE_FILE: &str = ".modelkeep-staging-lease";
const STAGING_LEASE_SECONDS: u64 = 3600;

#[derive(Debug)]
pub enum ArchiveError {
    Io(io::Error),
    InvalidPath(String),
    AlreadyPublished(PathBuf),
    IntegrityMismatch(String),
    ReferencedRevision(Vec<String>),
}

impl fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "archive I/O error: {error}"),
            Self::InvalidPath(path) => write!(f, "unsafe archive path: {path}"),
            Self::IntegrityMismatch(message) => write!(f, "archive integrity mismatch: {message}"),
            Self::ReferencedRevision(references) => write!(
                f,
                "revision is referenced by mutable refs: {}",
                references.join(", ")
            ),
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
    #[serde(default)]
    complete: bool,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionRemoval {
    pub commit: String,
    pub references: Vec<String>,
    pub removed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditFailure {
    pub repo_id: String,
    pub commit: String,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditReport {
    pub checked: usize,
    pub failures: Vec<AuditFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepositorySummary {
    pub repo_id: String,
    pub revision_count: usize,
    pub ref_count: usize,
    pub logical_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RevisionSummary {
    pub commit: String,
    pub file_count: usize,
    pub logical_bytes: u64,
    pub references: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepositoryInventory {
    pub repo_id: String,
    pub refs: BTreeMap<String, String>,
    pub revisions: Vec<RevisionSummary>,
}

#[derive(Debug, Clone)]
pub struct Archive {
    pub(crate) root: PathBuf,
}

impl Archive {
    pub fn new(root: impl Into<PathBuf>) -> ArchiveResult<Self> {
        let root = root.into();
        fs::create_dir_all(root.join("models"))?;
        fs::create_dir_all(root.join("tmp"))?;
        Ok(Self { root })
    }

    /// Opens an existing archive without creating or modifying durable state.
    pub fn open_read_only(root: impl Into<PathBuf>) -> ArchiveResult<Self> {
        let root = root.into();
        if !root.is_dir() || !root.join("models").is_dir() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "archive root not found").into());
        }
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

        let staging = self.create_staging("revision")?;
        let result = self.write_revision(&staging, &request);
        if let Err(error) = result {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }

        remove_staging_lease(&staging)?;
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
        let temporary = refs.join(format!(".{reference}.{}.part", operation_id()));
        let result = (|| -> io::Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(commit.as_bytes())?;
            file.sync_all()?;
            fs::rename(&temporary, refs.join(reference))?;
            sync_directory(&refs)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result.map_err(ArchiveError::from)
    }

    pub fn recover_incomplete(&self) -> ArchiveResult<usize> {
        let mut recovered = 0;
        for entry in fs::read_dir(self.root.join("tmp"))? {
            let entry = entry?;
            let path = entry.path();
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let lease = path.join(STAGING_LEASE_FILE);
            let Ok(metadata) = fs::read_to_string(&lease) else {
                continue;
            };
            let Some(expires_at) = metadata
                .lines()
                .find_map(|line| line.strip_prefix("expires_at=")?.parse::<u64>().ok())
            else {
                continue;
            };
            if expires_at > unix_timestamp() {
                continue;
            }
            fs::remove_dir_all(path)?;
            recovered += 1;
        }
        Ok(recovered)
    }

    pub fn list_revisions(&self, repo_id: &str) -> ArchiveResult<Vec<String>> {
        let (namespace, name) = validate_repo_id(repo_id)?;
        let revisions = self
            .root
            .join("models")
            .join(namespace)
            .join(name)
            .join("revisions");
        let mut commits = Vec::new();
        if !revisions.is_dir() {
            return Ok(commits);
        }
        for entry in fs::read_dir(revisions)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                commits.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        commits.sort();
        Ok(commits)
    }

    pub fn list_repositories(&self) -> ArchiveResult<Vec<RepositorySummary>> {
        let mut repositories = Vec::new();
        for namespace in fs::read_dir(self.root.join("models"))? {
            let namespace = namespace?;
            if !namespace.file_type()?.is_dir() {
                continue;
            }
            let namespace_name = namespace.file_name().to_string_lossy().into_owned();
            validate_component(&namespace_name)?;
            for repository in fs::read_dir(namespace.path())? {
                let repository = repository?;
                if !repository.file_type()?.is_dir() {
                    continue;
                }
                let repository_name = repository.file_name().to_string_lossy().into_owned();
                validate_component(&repository_name)?;
                let repo_id = format!("{namespace_name}/{repository_name}");
                let inventory = self.repository_inventory(&repo_id)?;
                repositories.push(RepositorySummary {
                    repo_id,
                    revision_count: inventory.revisions.len(),
                    ref_count: inventory.refs.len(),
                    logical_bytes: inventory
                        .revisions
                        .iter()
                        .map(|revision| revision.logical_bytes)
                        .sum(),
                });
            }
        }
        repositories.sort_by(|left, right| left.repo_id.cmp(&right.repo_id));
        Ok(repositories)
    }

    pub fn repository_inventory(&self, repo_id: &str) -> ArchiveResult<RepositoryInventory> {
        let (namespace, name) = validate_repo_id(repo_id)?;
        let repository = self.root.join("models").join(namespace).join(name);
        if !repository.is_dir() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "repository not found").into());
        }
        let mut refs = BTreeMap::new();
        let refs_path = repository.join("refs");
        if refs_path.is_dir() {
            for entry in fs::read_dir(refs_path)? {
                let entry = entry?;
                if !entry.file_type()?.is_file() {
                    continue;
                }
                let reference = entry.file_name().to_string_lossy().into_owned();
                validate_component(&reference)?;
                let commit = fs::read_to_string(entry.path())?.trim().to_string();
                validate_revision(&commit)?;
                refs.insert(reference, commit);
            }
        }
        let mut revisions = Vec::new();
        for commit in self.list_revisions(repo_id)? {
            validate_revision(&commit)?;
            let manifest: Manifest = serde_json::from_str(&self.manifest(repo_id, &commit)?)
                .map_err(|error| ArchiveError::IntegrityMismatch(error.to_string()))?;
            if !manifest.complete {
                return Err(ArchiveError::IntegrityMismatch(format!(
                    "revision {commit} is not complete"
                )));
            }
            revisions.push(RevisionSummary {
                references: refs
                    .iter()
                    .filter_map(|(reference, target)| {
                        (target == &commit).then_some(reference.clone())
                    })
                    .collect(),
                commit,
                file_count: manifest.files.len(),
                logical_bytes: manifest.files.iter().map(|file| file.size).sum(),
            });
        }
        revisions.sort_by(|left, right| left.commit.cmp(&right.commit));
        Ok(RepositoryInventory {
            repo_id: repo_id.to_string(),
            refs,
            revisions,
        })
    }

    pub fn remove_revision(
        &self,
        repo_id: &str,
        commit: &str,
        dry_run: bool,
    ) -> ArchiveResult<RevisionRemoval> {
        let revision = self.revision_path(repo_id, commit)?;
        if !revision.is_dir() {
            return Err(
                io::Error::new(io::ErrorKind::NotFound, "revision is not published").into(),
            );
        }
        let refs_path = revision.parent().unwrap().parent().unwrap().join("refs");
        let mut references = Vec::new();
        if refs_path.is_dir() {
            for entry in fs::read_dir(&refs_path)? {
                let entry = entry?;
                if !entry.file_type()?.is_file() {
                    continue;
                }
                if fs::read_to_string(entry.path())?.trim() == commit {
                    references.push(entry.file_name().to_string_lossy().into_owned());
                }
            }
        }
        references.sort();
        if !references.is_empty() {
            return Err(ArchiveError::ReferencedRevision(references));
        }
        if !dry_run {
            fs::remove_dir_all(&revision)?;
            sync_directory(revision.parent().unwrap())?;
        }
        Ok(RevisionRemoval {
            commit: commit.to_string(),
            references,
            removed: !dry_run,
        })
    }

    pub fn manifest(&self, repo_id: &str, commit: &str) -> ArchiveResult<String> {
        Ok(fs::read_to_string(
            self.revision_path(repo_id, commit)?
                .join(".modelkeep-manifest.json"),
        )?)
    }

    pub fn resolve_ref(&self, repo_id: &str, reference: &str) -> ArchiveResult<String> {
        let (namespace, name) = validate_repo_id(repo_id)?;
        validate_component(reference)?;
        let path = self
            .root
            .join("models")
            .join(namespace)
            .join(name)
            .join("refs")
            .join(reference);
        let commit = fs::read_to_string(path)?.trim().to_string();
        validate_revision(&commit)?;
        Ok(commit)
    }

    pub fn check_readiness(&self) -> ArchiveResult<()> {
        for directory in [self.root.join("models"), self.root.join("tmp")] {
            if !directory.is_dir() {
                return Err(ArchiveError::Io(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "required archive directory is unavailable: {}",
                        directory.display()
                    ),
                )));
            }
        }
        let probe = self
            .root
            .join("tmp")
            .join(format!("readiness-{}", operation_id()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)?;
        file.write_all(b"modelkeep-readiness")?;
        file.sync_all()?;
        fs::remove_file(&probe)?;
        sync_directory(&self.root.join("tmp"))?;
        Ok(())
    }

    pub fn create_fetch_staging(&self) -> ArchiveResult<PathBuf> {
        self.create_staging("fetch")
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

    pub fn is_complete_revision(&self, repo_id: &str, commit: &str) -> ArchiveResult<bool> {
        let manifest: Manifest = serde_json::from_str(&self.manifest(repo_id, commit)?)
            .map_err(|error| ArchiveError::IntegrityMismatch(error.to_string()))?;
        Ok(manifest.complete)
    }

    pub fn resolve_file(
        &self,
        repo_id: &str,
        commit: &str,
        relative_path: &str,
    ) -> ArchiveResult<ResolvedFile> {
        let relative = validate_relative_file_path(relative_path)?;
        let revision = self.revision_path(repo_id, commit)?;
        let manifest: Manifest = serde_json::from_str(&self.manifest(repo_id, commit)?)
            .map_err(|error| ArchiveError::IntegrityMismatch(error.to_string()))?;
        if !manifest.complete {
            return Err(ArchiveError::IntegrityMismatch(
                "revision is not complete".into(),
            ));
        }
        if !manifest
            .files
            .iter()
            .any(|entry| entry.path == relative_path)
        {
            return Err(io::Error::new(io::ErrorKind::NotFound, "archive file not found").into());
        }
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
        if !manifest.complete {
            return Err(ArchiveError::IntegrityMismatch(
                "revision is not complete".into(),
            ));
        }
        let mut verified = 0;
        for entry in &manifest.files {
            let resolved = self.resolve_file(repo_id, commit, &entry.path)?;
            if resolved.size != entry.size {
                return Err(ArchiveError::IntegrityMismatch(entry.path.clone()));
            }
            if sha256_file(&resolved.path)? != entry.sha256 {
                return Err(ArchiveError::IntegrityMismatch(entry.path.clone()));
            }
            verified += 1;
        }
        let expected = manifest
            .files
            .iter()
            .map(|entry| entry.path.clone())
            .collect::<BTreeSet<_>>();
        let mut actual = BTreeSet::new();
        collect_revision_files(&revision, &revision, &mut actual)?;
        if actual != expected {
            return Err(ArchiveError::IntegrityMismatch(format!(
                "manifest file set differs: expected {expected:?}, actual {actual:?}"
            )));
        }
        Ok(verified)
    }

    pub fn audit(&self) -> ArchiveResult<AuditReport> {
        let mut report = AuditReport {
            checked: 0,
            failures: Vec::new(),
        };
        for namespace in fs::read_dir(self.root.join("models"))? {
            let namespace = namespace?;
            if !namespace.file_type()?.is_dir() {
                continue;
            }
            for repository in fs::read_dir(namespace.path())? {
                let repository = repository?;
                if !repository.file_type()?.is_dir() {
                    continue;
                }
                let repo_id = format!(
                    "{}/{}",
                    namespace.file_name().to_string_lossy(),
                    repository.file_name().to_string_lossy()
                );
                let revisions = repository.path().join("revisions");
                if !revisions.is_dir() {
                    continue;
                }
                for revision in fs::read_dir(revisions)? {
                    let revision = revision?;
                    if !revision.file_type()?.is_dir() {
                        continue;
                    }
                    let commit = revision.file_name().to_string_lossy().into_owned();
                    report.checked += 1;
                    tracing::info!(repo_id = %repo_id, commit = %commit, "archive audit revision");
                    if let Err(error) = self.verify_revision(&repo_id, &commit) {
                        report.failures.push(AuditFailure {
                            repo_id: repo_id.clone(),
                            commit,
                            error: error.to_string(),
                        });
                    }
                }
            }
        }
        Ok(report)
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
        let archive_tmp = fs::canonicalize(self.root.join("tmp"))?;
        let reuse_staging = source_root.starts_with(&archive_tmp);
        let staging = if reuse_staging {
            source_root.clone()
        } else {
            self.create_staging("revision")?
        };
        let result = if reuse_staging {
            Self::write_source_manifest(&staging, &source_root, &request)
        } else {
            self.write_source_revision(&staging, &source_root, &request)
        };
        if let Err(error) = result {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
        remove_staging_lease(&staging)?;
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

    fn create_staging(&self, prefix: &str) -> ArchiveResult<PathBuf> {
        for _ in 0..16 {
            let operation = operation_id();
            let staging = self.root.join("tmp").join(format!("{prefix}-{operation}"));
            match fs::create_dir(&staging) {
                Ok(()) => {
                    let mut lease = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(staging.join(STAGING_LEASE_FILE))?;
                    writeln!(lease, "nonce={operation}")?;
                    writeln!(lease, "pid={}", process::id())?;
                    writeln!(
                        lease,
                        "expires_at={}",
                        unix_timestamp() + STAGING_LEASE_SECONDS
                    )?;
                    lease.sync_all()?;
                    sync_directory(&staging)?;
                    spawn_lease_heartbeat(staging.clone());
                    return Ok(staging);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(ArchiveError::Io(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "unable to allocate unique staging directory",
        )))
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
    fn write_source_manifest(
        staging: &Path,
        source_root: &Path,
        request: &SourcePublishRequest,
    ) -> ArchiveResult<()> {
        let mut entries = Vec::with_capacity(request.files.len());
        let mut archived_paths = BTreeSet::new();
        for source_file in &request.files {
            let relative = validate_relative_file_path(&source_file.path)?;
            if !archived_paths.insert(source_file.path.clone()) {
                return Err(ArchiveError::IntegrityMismatch(format!(
                    "duplicate archive path: {}",
                    source_file.path
                )));
            }
            let source_metadata = fs::symlink_metadata(&source_file.source)?;
            if !source_metadata.file_type().is_file() {
                return Err(ArchiveError::InvalidPath(source_file.path.clone()));
            }
            let source = fs::canonicalize(&source_file.source)?;
            let archived = fs::canonicalize(staging.join(relative))?;
            if !source.starts_with(source_root) || source != archived {
                return Err(ArchiveError::InvalidPath(source_file.path.clone()));
            }
            let mut input = File::open(source)?;
            let mut hasher = Sha256::new();
            let mut buffer = [0u8; 1024 * 1024];
            let mut size = 0u64;
            loop {
                let read = input.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
                size += read as u64;
            }
            entries.push((
                source_file.path.as_str(),
                size,
                hex_digest(hasher.finalize().as_slice()),
            ));
        }
        remove_unlisted_staging_entries(staging, staging, &archived_paths)?;
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

fn remove_unlisted_staging_entries(
    root: &Path,
    directory: &Path,
    archived_paths: &BTreeSet<String>,
) -> ArchiveResult<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .map_err(|_| ArchiveError::InvalidPath(path.display().to_string()))?;
        if relative == Path::new(STAGING_LEASE_FILE)
            || relative == Path::new(".modelkeep-manifest.json")
        {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_dir() {
            remove_unlisted_staging_entries(root, &path, archived_paths)?;
            if fs::read_dir(&path)?.next().is_none() {
                fs::remove_dir(&path)?;
            }
        } else {
            let value = relative
                .to_str()
                .ok_or_else(|| ArchiveError::InvalidPath(relative.display().to_string()))?;
            if !kind.is_file() || !archived_paths.contains(value) {
                fs::remove_file(&path)?;
            }
        }
    }
    Ok(())
}

fn operation_id() -> String {
    let mut bytes = [0u8; 16];
    if let Ok(mut random) = File::open("/dev/urandom") {
        if random.read_exact(&mut bytes).is_ok() {
            return hex_digest(&bytes);
        }
    }
    format!(
        "{}-{}-{}",
        unix_timestamp(),
        process::id(),
        STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn spawn_lease_heartbeat(staging: PathBuf) {
    thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(30));
        if !staging.is_dir() || refresh_staging_lease(&staging).is_err() {
            break;
        }
    });
}

fn refresh_staging_lease(staging: &Path) -> ArchiveResult<()> {
    let metadata = fs::read_to_string(staging.join(STAGING_LEASE_FILE))?;
    let nonce = metadata
        .lines()
        .find_map(|line| line.strip_prefix("nonce="))
        .ok_or_else(|| ArchiveError::IntegrityMismatch("staging lease has no nonce".into()))?;
    let temporary = staging.join(".modelkeep-staging-lease.part");
    let mut lease = File::create(&temporary)?;
    writeln!(lease, "nonce={nonce}")?;
    writeln!(lease, "pid={}", process::id())?;
    writeln!(
        lease,
        "expires_at={}",
        unix_timestamp() + STAGING_LEASE_SECONDS
    )?;
    lease.sync_all()?;
    fs::rename(temporary, staging.join(STAGING_LEASE_FILE))?;
    sync_directory(staging)?;
    Ok(())
}

fn remove_staging_lease(staging: &Path) -> ArchiveResult<()> {
    match fs::remove_file(staging.join(STAGING_LEASE_FILE)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
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

pub(crate) fn validate_repository_id(repo_id: &str) -> ArchiveResult<()> {
    validate_repo_id(repo_id).map(|_| ())
}

pub(crate) fn validate_revision_ref(revision: &str) -> ArchiveResult<()> {
    validate_component(revision)
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
        "{{\"version\":1,\"complete\":true,\"repo_type\":\"model\",\"repo_id\":\"{}\",\"requested_revision\":\"{}\",\"commit\":\"{}\",\"archived_at\":{},\"files\":[",
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

fn collect_revision_files(
    root: &Path,
    directory: &Path,
    files: &mut BTreeSet<String>,
) -> ArchiveResult<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .map_err(|_| ArchiveError::InvalidPath(path.display().to_string()))?;
        if relative == Path::new(".modelkeep-manifest.json") {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_dir() {
            collect_revision_files(root, &path, files)?;
        } else if kind.is_file() {
            let value = relative
                .to_str()
                .ok_or_else(|| ArchiveError::InvalidPath(relative.display().to_string()))?;
            validate_relative_file_path(value)?;
            files.insert(value.to_string());
        } else {
            return Err(ArchiveError::IntegrityMismatch(
                relative.display().to_string(),
            ));
        }
    }
    Ok(())
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

pub(crate) fn is_hf_commit(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex_digest(hasher.finalize().as_slice()))
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
        let (archive, _directory) = archive();
        let path = archive
            .publish_revision(request("aaaaaaaa", b"{}"))
            .unwrap();
        assert_eq!(fs::read(path.join("config.json")).unwrap(), b"{}");
        let manifest = fs::read_to_string(path.join(".modelkeep-manifest.json")).unwrap();
        assert!(manifest.contains("\"commit\":\"aaaaaaaa\""));
        assert!(manifest.contains("\"size\":2"));
        assert_eq!(
            archive.list_revisions("org/model").unwrap(),
            vec!["aaaaaaaa"]
        );
        assert!(archive
            .manifest("org/model", "aaaaaaaa")
            .unwrap()
            .contains("repo_id"));
    }

    #[test]
    fn inventory_is_reconstructed_from_manifests_and_refs() {
        let (archive, _directory) = archive();
        let first = "a".repeat(40);
        let second = "b".repeat(40);
        archive.publish_revision(request(&first, b"one")).unwrap();
        archive
            .publish_revision(request(&second, b"second"))
            .unwrap();
        archive.update_ref("org/model", "main", &second).unwrap();

        let repositories = archive.list_repositories().unwrap();
        assert_eq!(repositories.len(), 1);
        assert_eq!(repositories[0].repo_id, "org/model");
        assert_eq!(repositories[0].revision_count, 2);
        assert_eq!(repositories[0].ref_count, 1);
        assert_eq!(repositories[0].logical_bytes, 9);

        let inventory = archive.repository_inventory("org/model").unwrap();
        assert_eq!(inventory.refs["main"], second);
        assert!(inventory.revisions[0].references.is_empty());
        assert_eq!(inventory.revisions[1].references, vec!["main"]);
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
        let (archive, _directory) = archive();
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
        let (archive, _directory) = archive();
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
    fn concurrent_ref_updates_publish_only_complete_values() {
        let (archive, _directory) = archive();
        let commits = ["aaaaaaaa", "bbbbbbbb"];
        for commit in commits {
            archive
                .publish_revision(request(commit, commit.as_bytes()))
                .unwrap();
        }
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
        let threads = (0..16)
            .map(|index| {
                let archive = archive.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    archive.update_ref("org/model", "main", commits[index % 2])
                })
            })
            .collect::<Vec<_>>();
        for thread in threads {
            thread.join().unwrap().unwrap();
        }
        let published = archive.resolve_ref("org/model", "main").unwrap();
        assert!(commits.contains(&published.as_str()));
        let revision = archive.revision_path("org/model", commits[0]).unwrap();
        let refs = revision.parent().unwrap().parent().unwrap().join("refs");
        assert_eq!(fs::read_dir(refs).unwrap().count(), 1);
    }

    #[test]
    fn dry_run_preserves_unreferenced_revision() {
        let (archive, _directory) = archive();
        archive
            .publish_revision(request("aaaaaaaa", b"one"))
            .unwrap();
        let result = archive
            .remove_revision("org/model", "aaaaaaaa", true)
            .unwrap();
        assert!(!result.removed);
        assert!(archive
            .revision_path("org/model", "aaaaaaaa")
            .unwrap()
            .is_dir());
    }

    #[test]
    fn removes_unreferenced_revision() {
        let (archive, _directory) = archive();
        archive
            .publish_revision(request("aaaaaaaa", b"one"))
            .unwrap();
        let result = archive
            .remove_revision("org/model", "aaaaaaaa", false)
            .unwrap();
        assert!(result.removed);
        assert!(!archive
            .revision_path("org/model", "aaaaaaaa")
            .unwrap()
            .exists());
    }

    #[test]
    fn refuses_to_remove_referenced_revision() {
        let (archive, _directory) = archive();
        archive
            .publish_revision(request("aaaaaaaa", b"one"))
            .unwrap();
        archive.update_ref("org/model", "main", "aaaaaaaa").unwrap();
        assert!(matches!(
            archive.remove_revision("org/model", "aaaaaaaa", false),
            Err(ArchiveError::ReferencedRevision(refs)) if refs == vec![String::from("main")]
        ));
        assert!(archive
            .revision_path("org/model", "aaaaaaaa")
            .unwrap()
            .is_dir());
    }

    #[test]
    fn rejects_unsafe_paths() {
        let (archive, _directory) = archive();
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
        let (archive, _directory) = archive();
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
        let revision = archive.revision_path("org/model", "aaaaaaaa").unwrap();
        fs::write(revision.join("unlisted.json"), b"not in manifest").unwrap();
        assert!(matches!(
            archive.resolve_file("org/model", "aaaaaaaa", "unlisted.json"),
            Err(ArchiveError::Io(error)) if error.kind() == io::ErrorKind::NotFound
        ));
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
    fn recognizes_only_full_hugging_face_commit_ids() {
        assert!(is_hf_commit(&"a".repeat(40)));
        assert!(is_hf_commit(&"A".repeat(40)));
        assert!(!is_hf_commit("deadbeef"));
        assert!(!is_hf_commit(&"g".repeat(40)));
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
    #[test]
    fn staging_ids_are_unique_across_operations() {
        let (archive, _directory) = archive();
        let mut paths = std::collections::HashSet::new();
        for _ in 0..64 {
            let path = archive.create_fetch_staging().unwrap();
            assert!(paths.insert(path));
        }
        assert_eq!(paths.len(), 64);
    }

    #[test]
    fn recovery_preserves_active_staging() {
        let (archive, _directory) = archive();
        let staging = archive.create_fetch_staging().unwrap();
        fs::write(
            staging.join(STAGING_LEASE_FILE),
            format!("nonce=test\npid=1\nexpires_at={}\n", unix_timestamp() + 60),
        )
        .unwrap();
        assert_eq!(archive.recover_incomplete().unwrap(), 0);
        assert!(staging.exists());
    }

    #[test]
    fn recovery_removes_unpublished_staging_only() {
        let (archive, directory) = archive();
        let staging = archive.create_fetch_staging().unwrap();
        fs::write(staging.join("partial.bin"), b"partial").unwrap();
        fs::write(
            staging.join(STAGING_LEASE_FILE),
            "nonce=test\npid=1\nexpires_at=0\n",
        )
        .unwrap();
        assert_eq!(archive.recover_incomplete().unwrap(), 1);
        assert!(!staging.exists());
        assert!(!directory.path().join("models/org/model/revisions").exists());
    }
    #[test]
    fn reuses_fetch_staging_for_publication() {
        let (archive, _directory) = archive();
        let staging = archive.create_fetch_staging().unwrap();
        fs::write(staging.join("model.bin"), vec![3u8; 1024]).unwrap();
        fs::create_dir_all(staging.join(".cache/huggingface")).unwrap();
        fs::write(
            staging.join(".cache/huggingface/download.json"),
            b"metadata",
        )
        .unwrap();
        let published = archive
            .publish_revision_from_directory(SourcePublishRequest {
                repo_id: "org/model".into(),
                requested_revision: "main".into(),
                commit: "dddddddd".into(),
                source_root: staging.clone(),
                files: vec![SourceFile {
                    path: "model.bin".into(),
                    source: staging.join("model.bin"),
                }],
            })
            .unwrap();
        assert!(!staging.exists());
        assert_eq!(
            fs::metadata(published.join("model.bin")).unwrap().len(),
            1024
        );
        assert!(!published.join(".cache").exists());
        assert_eq!(archive.verify_revision("org/model", "dddddddd").unwrap(), 1);
    }

    #[test]
    fn audit_reports_corrupt_missing_malformed_and_unexpected_files() {
        let (archive, _directory) = archive();
        for (name, commit) in [
            ("healthy", "aaaaaaaa"),
            ("corrupt", "bbbbbbbb"),
            ("missing", "cccccccc"),
            ("malformed", "dddddddd"),
            ("unexpected", "eeeeeeee"),
            ("unsafe", "ffffffff"),
        ] {
            archive
                .publish_revision(PublishRequest {
                    repo_id: format!("org/{name}"),
                    requested_revision: "main".into(),
                    commit: commit.into(),
                    files: vec![ArchiveFile {
                        path: "config.json".into(),
                        bytes: b"valid".to_vec(),
                    }],
                })
                .unwrap();
        }
        fs::write(
            archive
                .revision_path("org/corrupt", "bbbbbbbb")
                .unwrap()
                .join("config.json"),
            b"wrong",
        )
        .unwrap();
        fs::remove_file(
            archive
                .revision_path("org/missing", "cccccccc")
                .unwrap()
                .join("config.json"),
        )
        .unwrap();
        fs::write(
            archive
                .revision_path("org/malformed", "dddddddd")
                .unwrap()
                .join(".modelkeep-manifest.json"),
            b"not-json",
        )
        .unwrap();
        fs::write(
            archive
                .revision_path("org/unexpected", "eeeeeeee")
                .unwrap()
                .join("extra.bin"),
            b"extra",
        )
        .unwrap();
        let unsafe_manifest = archive
            .revision_path("org/unsafe", "ffffffff")
            .unwrap()
            .join(".modelkeep-manifest.json");
        let unsafe_contents = fs::read_to_string(&unsafe_manifest)
            .unwrap()
            .replace("config.json", "../escape");
        fs::write(unsafe_manifest, unsafe_contents).unwrap();

        let report = archive.audit().unwrap();
        assert_eq!(report.checked, 6);
        assert_eq!(report.failures.len(), 5);
        assert!(!report
            .failures
            .iter()
            .any(|failure| failure.repo_id == "org/healthy"));
        for repo in [
            "org/corrupt",
            "org/missing",
            "org/malformed",
            "org/unexpected",
            "org/unsafe",
        ] {
            assert!(report
                .failures
                .iter()
                .any(|failure| failure.repo_id == repo));
        }
    }

    #[test]
    fn read_only_open_does_not_create_a_missing_archive() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing");
        assert!(Archive::open_read_only(&missing).is_err());
        assert!(!missing.exists());
    }
}
