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
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub mod admin;
mod admin_ui;
pub mod http;
pub mod importer;
pub mod pullthrough;
pub mod singleflight;
pub mod upstream;

static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const STAGING_LEASE_FILE: &str = ".modelkeep-staging-lease";
pub(crate) const FETCH_STAGING_FILE: &str = ".modelkeep-fetch.json";
const STAGING_LEASE_SECONDS: u64 = 120;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FetchStagingMetadata {
    version: u32,
    #[serde(default)]
    pub(crate) repo_type: RepositoryType,
    pub(crate) repo_id: String,
    pub(crate) requested_revision: String,
    pub(crate) files: Vec<String>,
    pub(crate) resolved_commit: Option<String>,
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryType {
    #[default]
    Model,
    Dataset,
}

impl RepositoryType {
    pub const ALL: [Self; 2] = [Self::Model, Self::Dataset];

    pub const fn archive_directory(self) -> &'static str {
        match self {
            Self::Model => "models",
            Self::Dataset => "datasets",
        }
    }
}

impl fmt::Display for RepositoryType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Model => "model",
            Self::Dataset => "dataset",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FetchStaging {
    pub(crate) path: PathBuf,
    pub(crate) resumed: bool,
    pub(crate) resolved_commit: Option<String>,
}

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
    /// The content digest the manifest recorded for this file when the revision
    /// was published.
    ///
    /// Carried here so a serving route can advertise a validator that *is* the
    /// fingerprint of the bytes (Issue 0078). The manifest requires the digest
    /// for every entry, so a revision that lacks one cannot be read at all and
    /// can never reach a response with a synthesised validator instead.
    pub sha256: String,
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
    #[serde(default)]
    repo_type: RepositoryType,
    #[serde(default)]
    repo_id: String,
    #[serde(default)]
    requested_revision: String,
    files: Vec<ManifestFile>,
}

#[derive(Debug, Deserialize)]
struct ManifestFile {
    path: String,
    size: u64,
    sha256: String,
}

/// The internal file recording a commit's upstream file list.
///
/// It lives inside the revision directory and is named with the
/// `.modelkeep-` prefix, so [`is_internal_archive_path`] excludes it from the
/// manifest, from every file listing, and from serving: an older ModelKeep
/// binary reading this archive cannot see it, and no client can request it.
pub(crate) const UPSTREAM_FILES_FILE: &str = ".modelkeep-upstream-files.json";

/// One file as upstream reported it at an immutable commit.
///
/// A commit is immutable, so this is a fact that does not go stale. It is
/// recorded verbatim and is never presented as a statement about bytes
/// ModelKeep holds or has verified: `blob_id` is upstream's git object id and
/// `lfs_sha256` its LFS object digest, both upstream's values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpstreamFile {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lfs_sha256: Option<String>,
}

impl UpstreamFile {
    /// This entry with every untrusted field either accepted or dropped.
    ///
    /// Upstream metadata reaches ModelKeep through a helper's stdout and is
    /// served back to clients, so it is untrusted input: a path that is not a
    /// safe relative archive path rejects the whole entry, and an object id
    /// that is not a plain hexadecimal digest is dropped rather than echoed.
    pub fn sanitized(self) -> Option<Self> {
        validate_relative_file_path(&self.path).ok()?;
        Some(Self {
            path: self.path,
            size: self.size,
            blob_id: self.blob_id.filter(|value| is_hexadecimal_object_id(value)),
            lfs_sha256: self
                .lfs_sha256
                .filter(|value| is_hexadecimal_object_id(value)),
        })
    }
}

/// A plausible upstream object id: hexadecimal, and no longer than a sha512.
fn is_hexadecimal_object_id(value: &str) -> bool {
    (1..=128).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// The recorded upstream file list of one commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct UpstreamFileList {
    version: u32,
    repo_type: RepositoryType,
    repo_id: String,
    commit: String,
    files: Vec<UpstreamFile>,
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

/// Request to add absent files to an already published revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionExtensionRequest {
    pub repo_id: String,
    pub commit: String,
    pub source_root: PathBuf,
    pub files: Vec<SourceFile>,
}

/// Outcome of one monotonic revision extension.
///
/// `added` lists the paths this extension published; `skipped` lists the
/// requested paths the live manifest already held, which are never rewritten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionExtension {
    pub commit: String,
    pub path: PathBuf,
    pub added: Vec<String>,
    pub skipped: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionRemoval {
    pub commit: String,
    pub references: Vec<String>,
    pub removed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditFailure {
    pub repo_type: RepositoryType,
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
    pub repo_type: RepositoryType,
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
    pub repo_type: RepositoryType,
    pub repo_id: String,
    pub refs: BTreeMap<String, String>,
    pub revisions: Vec<RevisionSummary>,
}

/// Class of durable inconsistency reported by the startup self-check.
///
/// The set is closed so operational automation can select on it. It never
/// carries a repair action: the self-check reports and changes nothing
/// (core invariant 4, ADR-0007).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SelfCheckFindingKind {
    /// The revision manifest is absent, unparseable, incomplete, or declares a
    /// repository type that disagrees with its location in the archive.
    InvalidManifest,
    /// A path the manifest lists is not present in the revision directory.
    MissingFile,
    /// A path the manifest lists exists with a size other than the recorded one.
    SizeMismatch,
    /// An archive component or manifest path is unsafe or leaves the archive root.
    UnsafePath,
    /// A mutable ref names a revision that is absent or not servable.
    DanglingRef,
    /// Fetch staging survived recovery and still occupies the temporary area.
    OrphanedStaging,
    /// A part of the archive could not be read at all.
    UnreadableArchive,
}

impl SelfCheckFindingKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidManifest => "invalid_manifest",
            Self::MissingFile => "missing_file",
            Self::SizeMismatch => "size_mismatch",
            Self::UnsafePath => "unsafe_path",
            Self::DanglingRef => "dangling_ref",
            Self::OrphanedStaging => "orphaned_staging",
            Self::UnreadableArchive => "unreadable_archive",
        }
    }
}

/// One finding of the startup self-check.
///
/// `detail` is ModelKeep's own description of the inconsistency and of archive
/// state only. It never carries request headers, tokens, or upstream output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SelfCheckFinding {
    pub finding: SelfCheckFindingKind,
    pub repo_type: Option<RepositoryType>,
    pub repo_id: Option<String>,
    pub commit: Option<String>,
    pub path: Option<String>,
    pub reference: Option<String>,
    pub age_seconds: Option<u64>,
    pub detail: String,
}

impl SelfCheckFinding {
    fn new(finding: SelfCheckFindingKind, detail: impl Into<String>) -> Self {
        Self {
            finding,
            repo_type: None,
            repo_id: None,
            commit: None,
            path: None,
            reference: None,
            age_seconds: None,
            detail: detail.into(),
        }
    }

    fn in_repository(mut self, repo_type: RepositoryType, repo_id: &str) -> Self {
        self.repo_type = Some(repo_type);
        self.repo_id = Some(repo_id.to_string());
        self
    }

    fn at_commit(mut self, commit: &str) -> Self {
        self.commit = Some(commit.to_string());
        self
    }

    fn at_path(mut self, path: &str) -> Self {
        self.path = Some(path.to_string());
        self
    }

    fn at_ref(mut self, reference: &str) -> Self {
        self.reference = Some(reference.to_string());
        self
    }

    fn aged(mut self, age_seconds: u64) -> Self {
        self.age_seconds = Some(age_seconds);
        self
    }
}

/// Result of one archive self-consistency check.
///
/// A report with an empty `findings` list is a positive statement that the
/// archive was walked and nothing was found, which is what distinguishes
/// "checked and clean" from "never checked".
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SelfCheckReport {
    pub completed_at: u64,
    pub duration_ms: u64,
    pub repositories_checked: usize,
    pub revisions_checked: usize,
    pub files_checked: usize,
    pub refs_checked: usize,
    pub staging_directories: usize,
    pub orphaned_staging_directories: usize,
    pub oldest_orphaned_staging_age_seconds: u64,
    /// Manifest entries naming ModelKeep's own internal archive paths.
    ///
    /// An old writer could list transient downloader metadata such as
    /// `.cache/huggingface/download.json`. Serving already filters those paths,
    /// so a client can neither see nor request one, which makes them a counted
    /// observation rather than a finding an operator has to act on.
    pub filtered_internal_paths: usize,
    pub findings: Vec<SelfCheckFinding>,
}

impl SelfCheckReport {
    fn empty() -> Self {
        Self {
            completed_at: 0,
            duration_ms: 0,
            repositories_checked: 0,
            revisions_checked: 0,
            files_checked: 0,
            refs_checked: 0,
            staging_directories: 0,
            orphaned_staging_directories: 0,
            oldest_orphaned_staging_age_seconds: 0,
            filtered_internal_paths: 0,
            findings: Vec::new(),
        }
    }

    pub fn status(&self) -> &'static str {
        if self.findings.is_empty() {
            "clean"
        } else {
            "findings"
        }
    }

    /// Findings grouped by class, for a bounded operational summary.
    pub fn findings_by_kind(&self) -> BTreeMap<&'static str, usize> {
        let mut counts = BTreeMap::new();
        for finding in &self.findings {
            *counts.entry(finding.finding.as_str()).or_insert(0) += 1;
        }
        counts
    }
}

/// Whether the archive self-check has run in this process, and its last result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelfCheckState {
    NeverRun,
    Running,
    Completed(Box<SelfCheckReport>),
}

#[derive(Debug, Clone)]
pub struct Archive {
    pub(crate) root: PathBuf,
    readiness: Arc<Mutex<Option<bool>>>,
    self_check: Arc<Mutex<SelfCheckState>>,
}

impl Archive {
    pub fn new(root: impl Into<PathBuf>) -> ArchiveResult<Self> {
        let root = root.into();
        fs::create_dir_all(root.join("models"))?;
        fs::create_dir_all(root.join("datasets"))?;
        fs::create_dir_all(root.join("tmp"))?;
        Ok(Self {
            root,
            readiness: Arc::new(Mutex::new(None)),
            self_check: Arc::new(Mutex::new(SelfCheckState::NeverRun)),
        })
    }

    /// Opens an existing archive without creating or modifying durable state.
    pub fn open_read_only(root: impl Into<PathBuf>) -> ArchiveResult<Self> {
        let root = root.into();
        if !root.is_dir() || !root.join("models").is_dir() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "archive root not found").into());
        }
        Ok(Self {
            root,
            readiness: Arc::new(Mutex::new(None)),
            self_check: Arc::new(Mutex::new(SelfCheckState::NeverRun)),
        })
    }

    /// Publishes one complete immutable revision.
    pub fn publish_revision(&self, request: PublishRequest) -> ArchiveResult<PathBuf> {
        self.publish_revision_for_type(RepositoryType::Model, request)
    }

    pub fn publish_revision_for_type(
        &self,
        repo_type: RepositoryType,
        request: PublishRequest,
    ) -> ArchiveResult<PathBuf> {
        let (namespace, name) = validate_repo_id(&request.repo_id)?;
        validate_component(&request.requested_revision)?;
        validate_revision(&request.commit)?;
        if request.files.is_empty() {
            return Err(ArchiveError::InvalidPath("revision has no files".into()));
        }

        let revisions = self
            .root
            .join(repo_type.archive_directory())
            .join(namespace)
            .join(name)
            .join("revisions");
        fs::create_dir_all(&revisions)?;
        let published = revisions.join(&request.commit);
        if published.exists() {
            return Err(ArchiveError::AlreadyPublished(published));
        }

        let staging = self.create_staging("revision")?;
        let result = self.write_revision(&staging, repo_type, &request);
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
        self.update_ref_for_type(RepositoryType::Model, repo_id, reference, commit)
    }

    pub fn update_ref_for_type(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        reference: &str,
        commit: &str,
    ) -> ArchiveResult<()> {
        let (namespace, name) = validate_repo_id(repo_id)?;
        validate_component(reference)?;
        validate_revision(commit)?;
        let revision = self
            .root
            .join(repo_type.archive_directory())
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
            let name = entry.file_name().to_string_lossy().into_owned();
            let fetch_identity =
                if name.starts_with("fetch-abandoned-") || name.starts_with(".fetch-active-") {
                    read_fetch_staging_metadata(&path).ok()
                } else {
                    None
                };
            if name.starts_with("fetch-abandoned-") {
                if fetch_identity
                    .as_ref()
                    .is_some_and(|metadata| metadata.resolved_commit.is_some())
                {
                    continue;
                }
            } else if name.starts_with(".fetch-active-")
                && fetch_identity
                    .as_ref()
                    .is_some_and(|metadata| metadata.resolved_commit.is_some())
            {
                let abandoned = self
                    .root
                    .join("tmp")
                    .join(format!("fetch-abandoned-{}", operation_id()));
                fs::rename(&path, abandoned)?;
                sync_directory(&self.root.join("tmp"))?;
                if let Some(identity) = &fetch_identity {
                    tracing::info!(
                        event = "incomplete_fetch_recovered",
                        repo_type = %identity.repo_type,
                        repo_id = %identity.repo_id,
                        requested_revision = %identity.requested_revision,
                        commit = identity.resolved_commit.as_deref().unwrap_or(""),
                        recovery_action = "preserved_for_resume",
                        "recovered incomplete fetch staging"
                    );
                }
                continue;
            }
            fs::remove_dir_all(path)?;
            recovered += 1;
            if let Some(identity) = &fetch_identity {
                tracing::info!(
                    event = "incomplete_fetch_recovered",
                    repo_type = %identity.repo_type,
                    repo_id = %identity.repo_id,
                    requested_revision = %identity.requested_revision,
                    recovery_action = "discarded",
                    "recovered incomplete fetch staging"
                );
            }
        }
        Ok(recovered)
    }

    pub fn list_revisions(&self, repo_id: &str) -> ArchiveResult<Vec<String>> {
        self.list_revisions_for_type(RepositoryType::Model, repo_id)
    }

    pub fn list_revisions_for_type(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
    ) -> ArchiveResult<Vec<String>> {
        let (namespace, name) = validate_repo_id(repo_id)?;
        let revisions = self
            .root
            .join(repo_type.archive_directory())
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
        for repo_type in RepositoryType::ALL {
            let root = self.root.join(repo_type.archive_directory());
            if !root.is_dir() {
                continue;
            }
            for namespace in fs::read_dir(root)? {
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
                    let inventory = self.repository_inventory_for_type(repo_type, &repo_id)?;
                    repositories.push(RepositorySummary {
                        repo_type,
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
        }
        repositories.sort_by(|left, right| {
            (left.repo_type, &left.repo_id).cmp(&(right.repo_type, &right.repo_id))
        });
        Ok(repositories)
    }

    pub fn repository_inventory(&self, repo_id: &str) -> ArchiveResult<RepositoryInventory> {
        self.repository_inventory_for_type(RepositoryType::Model, repo_id)
    }

    pub fn repository_inventory_for_type(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
    ) -> ArchiveResult<RepositoryInventory> {
        let (namespace, name) = validate_repo_id(repo_id)?;
        let repository = self
            .root
            .join(repo_type.archive_directory())
            .join(namespace)
            .join(name);
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
        for commit in self.list_revisions_for_type(repo_type, repo_id)? {
            validate_revision(&commit)?;
            let manifest = self.read_manifest_for(repo_type, repo_id, &commit)?;
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
            repo_type,
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
        self.remove_revision_for_type(RepositoryType::Model, repo_id, commit, dry_run)
    }

    pub fn remove_revision_for_type(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        commit: &str,
        dry_run: bool,
    ) -> ArchiveResult<RevisionRemoval> {
        let revision = self.revision_path_for_type(repo_type, repo_id, commit)?;
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
        self.manifest_for_type(RepositoryType::Model, repo_id, commit)
    }

    pub fn manifest_for_type(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        commit: &str,
    ) -> ArchiveResult<String> {
        Ok(fs::read_to_string(
            self.revision_path_for_type(repo_type, repo_id, commit)?
                .join(".modelkeep-manifest.json"),
        )?)
    }

    /// Records the upstream file list of a published commit.
    ///
    /// The record is written outside the revision directory and renamed into
    /// place, so a crash leaves either no record or a whole one. It adds no
    /// manifest entry and changes no published byte: a revision that already
    /// has a record keeps it, because the list of an immutable commit cannot
    /// legitimately change.
    pub fn record_upstream_files_for_type(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        commit: &str,
        files: &[UpstreamFile],
    ) -> ArchiveResult<bool> {
        let revision = self.revision_path_for_type(repo_type, repo_id, commit)?;
        if !revision.is_dir() {
            return Err(ArchiveError::Io(io::Error::from(io::ErrorKind::NotFound)));
        }
        if revision.join(UPSTREAM_FILES_FILE).exists() {
            return Ok(false);
        }
        let record = UpstreamFileList {
            version: 1,
            repo_type,
            repo_id: repo_id.to_string(),
            commit: commit.to_string(),
            files: files
                .iter()
                .cloned()
                .filter_map(UpstreamFile::sanitized)
                .collect(),
        };
        let serialized = serde_json::to_vec(&record)
            .map_err(|error| ArchiveError::IntegrityMismatch(error.to_string()))?;
        let revisions = revision
            .parent()
            .ok_or_else(|| ArchiveError::InvalidPath(revision.display().to_string()))?
            .to_path_buf();
        let temporary = revisions.join(format!(
            ".modelkeep-upstream-files-{commit}-{}.part",
            operation_id()
        ));
        let result = (|| -> io::Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(&serialized)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            fs::rename(&temporary, revision.join(UPSTREAM_FILES_FILE))?;
            sync_directory(&revision)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result.map_err(ArchiveError::from)?;
        Ok(true)
    }

    /// The upstream file list recorded for a commit, if the archive has one.
    ///
    /// `Ok(None)` means this revision does not know its upstream file list: it
    /// was published before the list was recorded, or imported from a client
    /// cache. Nothing is migrated on its behalf, and the absence is never
    /// reported as an empty repository.
    pub fn upstream_files_for_type(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        commit: &str,
    ) -> ArchiveResult<Option<Vec<UpstreamFile>>> {
        let path = self
            .revision_path_for_type(repo_type, repo_id, commit)?
            .join(UPSTREAM_FILES_FILE);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let record: UpstreamFileList = serde_json::from_slice(&bytes)
            .map_err(|error| ArchiveError::IntegrityMismatch(error.to_string()))?;
        if record.repo_type != repo_type || record.commit != commit {
            return Err(ArchiveError::IntegrityMismatch(
                "recorded upstream file list does not match its location".into(),
            ));
        }
        Ok(Some(
            record
                .files
                .into_iter()
                .filter_map(UpstreamFile::sanitized)
                .collect(),
        ))
    }

    pub fn resolve_ref(&self, repo_id: &str, reference: &str) -> ArchiveResult<String> {
        self.resolve_ref_for_type(RepositoryType::Model, repo_id, reference)
    }

    pub fn resolve_ref_for_type(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        reference: &str,
    ) -> ArchiveResult<String> {
        let (namespace, name) = validate_repo_id(repo_id)?;
        validate_component(reference)?;
        let path = self
            .root
            .join(repo_type.archive_directory())
            .join(namespace)
            .join(name)
            .join("refs")
            .join(reference);
        let commit = fs::read_to_string(path)?.trim().to_string();
        validate_revision(&commit)?;
        Ok(commit)
    }

    pub fn check_readiness(&self) -> ArchiveResult<()> {
        let result = (|| {
            for directory in [
                self.root.join("models"),
                self.root.join("datasets"),
                self.root.join("tmp"),
            ] {
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
        })();
        if let Ok(mut readiness) = self.readiness.lock() {
            *readiness = Some(result.is_ok());
        }
        result
    }

    /// Returns the last active readiness-probe result without touching durable state.
    pub fn last_readiness(&self) -> Option<bool> {
        self.readiness.lock().ok().and_then(|value| *value)
    }

    /// Returns whether the self-check has run in this process and its last report.
    pub fn self_check_state(&self) -> SelfCheckState {
        self.self_check
            .lock()
            .map(|state| state.clone())
            .unwrap_or(SelfCheckState::NeverRun)
    }

    /// Walks the archive for inconsistencies it can answer about itself.
    ///
    /// The check is read-only and offline by construction: it opens manifests
    /// and file metadata, never file contents, never upstream, and never
    /// repairs, deletes, or re-acquires anything it finds. Digest verification
    /// stays in the `verify` and `audit` management jobs, which is why this can
    /// run at every start on a multi-terabyte archive.
    ///
    /// It never fails: an unreadable part of the archive is itself a finding,
    /// so the walk always reaches a reported outcome.
    pub fn self_check(&self) -> SelfCheckReport {
        // The start is a DEBUG event on purpose. One completed check must cost
        // a bounded and very small number of log lines at the default level,
        // because every restart pays it; "a check is in flight" is answered by
        // the Admin status route, which reports `running` without any logging.
        tracing::debug!(
            event = "archive_self_check_started",
            archive_root = %self.root.display(),
            "archive self-check started"
        );
        if let Ok(mut state) = self.self_check.lock() {
            *state = SelfCheckState::Running;
        }
        let started = Instant::now();
        let mut report = SelfCheckReport::empty();
        for repo_type in RepositoryType::ALL {
            self.self_check_repository_type(repo_type, &mut report);
        }
        self.self_check_staging(&mut report);
        report.duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        report.completed_at = unix_timestamp();
        for finding in &report.findings {
            let repo_type = finding
                .repo_type
                .map(|repo_type| repo_type.to_string())
                .unwrap_or_default();
            tracing::warn!(
                event = "archive_self_check_finding",
                finding = finding.finding.as_str(),
                repo_type = %repo_type,
                repo_id = finding.repo_id.as_deref().unwrap_or(""),
                commit = finding.commit.as_deref().unwrap_or(""),
                path = finding.path.as_deref().unwrap_or(""),
                reference = finding.reference.as_deref().unwrap_or(""),
                age_seconds = finding.age_seconds.unwrap_or(0),
                detail = %finding.detail,
                "archive self-check finding"
            );
        }
        // Every restart pays for this line, so it carries the result and the
        // measurement and nothing else. The full counts are on the Admin status
        // route and in `modelkeep self-check`, which no restart writes to a log.
        tracing::info!(
            event = "archive_self_check_completed",
            status = report.status(),
            finding_count = report.findings.len(),
            revisions_checked = report.revisions_checked,
            duration_ms = report.duration_ms,
            "archive self-check completed"
        );
        if let Ok(mut state) = self.self_check.lock() {
            *state = SelfCheckState::Completed(Box::new(report.clone()));
        }
        report
    }

    fn self_check_repository_type(&self, repo_type: RepositoryType, report: &mut SelfCheckReport) {
        let root = self.root.join(repo_type.archive_directory());
        if !root.is_dir() {
            return;
        }
        let namespaces = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) => {
                report.findings.push(SelfCheckFinding::new(
                    SelfCheckFindingKind::UnreadableArchive,
                    format!("cannot read {}: {}", repo_type.archive_directory(), error),
                ));
                return;
            }
        };
        for namespace in namespaces {
            let Some(namespace) = self.self_check_directory(namespace, repo_type, report) else {
                continue;
            };
            let namespace_name = namespace.file_name().to_string_lossy().into_owned();
            if validate_component(&namespace_name).is_err() {
                report.findings.push(
                    SelfCheckFinding::new(
                        SelfCheckFindingKind::UnsafePath,
                        "namespace directory name is not a safe archive component",
                    )
                    .in_repository(repo_type, &namespace_name),
                );
                continue;
            }
            let repositories = match fs::read_dir(namespace.path()) {
                Ok(entries) => entries,
                Err(error) => {
                    report.findings.push(
                        SelfCheckFinding::new(
                            SelfCheckFindingKind::UnreadableArchive,
                            format!("cannot read namespace directory: {error}"),
                        )
                        .in_repository(repo_type, &namespace_name),
                    );
                    continue;
                }
            };
            for repository in repositories {
                let Some(repository) = self.self_check_directory(repository, repo_type, report)
                else {
                    continue;
                };
                let repository_name = repository.file_name().to_string_lossy().into_owned();
                let repo_id = format!("{namespace_name}/{repository_name}");
                if validate_repo_id(&repo_id).is_err() {
                    report.findings.push(
                        SelfCheckFinding::new(
                            SelfCheckFindingKind::UnsafePath,
                            "repository directory name is not a safe archive component",
                        )
                        .in_repository(repo_type, &repo_id),
                    );
                    continue;
                }
                report.repositories_checked += 1;
                let servable = self.self_check_revisions(
                    repo_type,
                    &repo_id,
                    &repository.path().join("revisions"),
                    report,
                );
                self.self_check_refs(
                    repo_type,
                    &repo_id,
                    &repository.path().join("refs"),
                    &servable,
                    report,
                );
            }
        }
    }

    /// Keeps only directory entries, recording anything that cannot be classified.
    fn self_check_directory(
        &self,
        entry: io::Result<fs::DirEntry>,
        repo_type: RepositoryType,
        report: &mut SelfCheckReport,
    ) -> Option<fs::DirEntry> {
        match entry {
            Ok(entry) => match entry.file_type() {
                Ok(kind) if kind.is_dir() => Some(entry),
                Ok(_) => None,
                Err(error) => {
                    report.findings.push(
                        SelfCheckFinding::new(
                            SelfCheckFindingKind::UnreadableArchive,
                            format!("cannot classify archive entry: {error}"),
                        )
                        .in_repository(repo_type, &entry.file_name().to_string_lossy()),
                    );
                    None
                }
            },
            Err(error) => {
                report.findings.push(SelfCheckFinding::new(
                    SelfCheckFindingKind::UnreadableArchive,
                    format!("cannot read archive entry: {error}"),
                ));
                None
            }
        }
    }

    /// Returns the commits a ref may resolve to.
    ///
    /// Membership means the revision exists and its manifest is usable, which
    /// is what a ref resolution needs. A per-file inconsistency inside the
    /// revision is reported as its own finding and is deliberately not
    /// cascaded into every ref that names the revision.
    fn self_check_revisions(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        revisions: &Path,
        report: &mut SelfCheckReport,
    ) -> BTreeSet<String> {
        let mut servable = BTreeSet::new();
        if !revisions.is_dir() {
            return servable;
        }
        let entries = match fs::read_dir(revisions) {
            Ok(entries) => entries,
            Err(error) => {
                report.findings.push(
                    SelfCheckFinding::new(
                        SelfCheckFindingKind::UnreadableArchive,
                        format!("cannot read revisions directory: {error}"),
                    )
                    .in_repository(repo_type, repo_id),
                );
                return servable;
            }
        };
        for entry in entries {
            let Some(entry) = self.self_check_directory(entry, repo_type, report) else {
                continue;
            };
            let commit = entry.file_name().to_string_lossy().into_owned();
            if validate_revision(&commit).is_err() {
                report.findings.push(
                    SelfCheckFinding::new(
                        SelfCheckFindingKind::UnsafePath,
                        "revision directory name is not a commit id",
                    )
                    .in_repository(repo_type, repo_id)
                    .at_commit(&commit),
                );
                continue;
            }
            report.revisions_checked += 1;
            if self.self_check_revision(repo_type, repo_id, &commit, &entry.path(), report) {
                servable.insert(commit);
            }
        }
        servable
    }

    /// Checks one revision's manifest and the metadata of every path it lists.
    ///
    /// Returns whether a ref may resolve to this revision, which depends on the
    /// manifest alone. File contents are never read: that is what keeps the
    /// check affordable at every start, and digest verification stays in the
    /// `verify` and `audit` jobs.
    fn self_check_revision(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        commit: &str,
        revision: &Path,
        report: &mut SelfCheckReport,
    ) -> bool {
        let finding = |kind: SelfCheckFindingKind, detail: String| {
            SelfCheckFinding::new(kind, detail)
                .in_repository(repo_type, repo_id)
                .at_commit(commit)
        };
        let manifest = match fs::read(revision.join(".modelkeep-manifest.json")) {
            Ok(bytes) => bytes,
            Err(error) => {
                report.findings.push(finding(
                    SelfCheckFindingKind::InvalidManifest,
                    format!("cannot read revision manifest: {error}"),
                ));
                return false;
            }
        };
        let manifest: Manifest = match serde_json::from_slice(&manifest) {
            Ok(manifest) => manifest,
            Err(error) => {
                report.findings.push(finding(
                    SelfCheckFindingKind::InvalidManifest,
                    format!("revision manifest does not parse: {error}"),
                ));
                return false;
            }
        };
        if manifest.repo_type != repo_type {
            report.findings.push(finding(
                SelfCheckFindingKind::InvalidManifest,
                format!(
                    "manifest repository type {} does not match archive root {repo_type}",
                    manifest.repo_type
                ),
            ));
            return false;
        }
        if !manifest.complete {
            report.findings.push(finding(
                SelfCheckFindingKind::InvalidManifest,
                "revision manifest is not marked complete".into(),
            ));
            return false;
        }
        let revision_root = fs::canonicalize(revision).ok();
        for (index, entry) in manifest.files.iter().enumerate() {
            report.files_checked += 1;
            let Ok(relative) = validate_relative_file_path(&entry.path) else {
                if is_internal_archive_path(&entry.path) {
                    // Serving filters ModelKeep's own internal paths, so a
                    // manifest that lists one is a counted observation, not an
                    // inconsistency a client can ever run into.
                    report.filtered_internal_paths += 1;
                    continue;
                }
                // A path that failed validation is untrusted input ModelKeep
                // refuses everywhere else, so it is never echoed back into a
                // report. The manifest entry is identified by position, which
                // locates it without repeating it.
                report.findings.push(finding(
                    SelfCheckFindingKind::UnsafePath,
                    format!("manifest entry {index} is not a safe archive path"),
                ));
                continue;
            };
            let candidate = revision.join(relative);
            let metadata = match fs::symlink_metadata(&candidate) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    report.findings.push(
                        finding(
                            SelfCheckFindingKind::MissingFile,
                            "manifest lists a path that is absent from the revision".into(),
                        )
                        .at_path(&entry.path),
                    );
                    continue;
                }
                Err(error) => {
                    report.findings.push(
                        finding(
                            SelfCheckFindingKind::UnreadableArchive,
                            format!("cannot read archived file metadata: {error}"),
                        )
                        .at_path(&entry.path),
                    );
                    continue;
                }
            };
            // A symbolic link is the one way a manifest path with safe syntax
            // can still leave the revision, so its target is resolved and
            // confined here rather than trusted.
            if metadata.is_symlink() {
                let escapes = match (&revision_root, fs::canonicalize(&candidate)) {
                    (Some(root), Ok(target)) => !target.starts_with(root),
                    _ => true,
                };
                if escapes {
                    report.findings.push(
                        finding(
                            SelfCheckFindingKind::UnsafePath,
                            "archived path resolves outside the revision directory".into(),
                        )
                        .at_path(&entry.path),
                    );
                    continue;
                }
            }
            let size = match fs::metadata(&candidate) {
                Ok(metadata) if metadata.is_file() => metadata.len(),
                Ok(_) => {
                    report.findings.push(
                        finding(
                            SelfCheckFindingKind::MissingFile,
                            "archived path is not a regular file".into(),
                        )
                        .at_path(&entry.path),
                    );
                    continue;
                }
                Err(error) => {
                    report.findings.push(
                        finding(
                            SelfCheckFindingKind::MissingFile,
                            format!("archived path cannot be resolved for serving: {error}"),
                        )
                        .at_path(&entry.path),
                    );
                    continue;
                }
            };
            if size != entry.size {
                report.findings.push(
                    finding(
                        SelfCheckFindingKind::SizeMismatch,
                        format!(
                            "manifest records {} bytes, archive holds {size}",
                            entry.size
                        ),
                    )
                    .at_path(&entry.path),
                );
            }
        }
        true
    }

    fn self_check_refs(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        refs: &Path,
        servable: &BTreeSet<String>,
        report: &mut SelfCheckReport,
    ) {
        if !refs.is_dir() {
            return;
        }
        let entries = match fs::read_dir(refs) {
            Ok(entries) => entries,
            Err(error) => {
                report.findings.push(
                    SelfCheckFinding::new(
                        SelfCheckFindingKind::UnreadableArchive,
                        format!("cannot read refs directory: {error}"),
                    )
                    .in_repository(repo_type, repo_id),
                );
                return;
            }
        };
        for entry in entries {
            let Ok(entry) = entry else {
                report.findings.push(
                    SelfCheckFinding::new(
                        SelfCheckFindingKind::UnreadableArchive,
                        "cannot read a ref entry",
                    )
                    .in_repository(repo_type, repo_id),
                );
                continue;
            };
            if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
                continue;
            }
            let reference = entry.file_name().to_string_lossy().into_owned();
            if reference.starts_with('.') {
                // An interrupted ref update leaves `.<ref>.<id>.part` behind;
                // it is never resolved and is not a ref of its own.
                continue;
            }
            report.refs_checked += 1;
            let finding = |kind: SelfCheckFindingKind, detail: String| {
                SelfCheckFinding::new(kind, detail)
                    .in_repository(repo_type, repo_id)
                    .at_ref(&reference)
            };
            if validate_component(&reference).is_err() {
                report.findings.push(finding(
                    SelfCheckFindingKind::UnsafePath,
                    "ref name is not a safe archive component".into(),
                ));
                continue;
            }
            let commit = match fs::read_to_string(entry.path()) {
                Ok(commit) => commit.trim().to_string(),
                Err(error) => {
                    report.findings.push(finding(
                        SelfCheckFindingKind::DanglingRef,
                        format!("cannot read ref: {error}"),
                    ));
                    continue;
                }
            };
            if validate_revision(&commit).is_err() {
                report.findings.push(finding(
                    SelfCheckFindingKind::DanglingRef,
                    "ref does not hold a commit id".into(),
                ));
                continue;
            }
            if !servable.contains(&commit) {
                report.findings.push(
                    finding(
                        SelfCheckFindingKind::DanglingRef,
                        "ref resolves to a revision that is absent or not servable".into(),
                    )
                    .at_commit(&commit),
                );
            }
        }
    }

    fn self_check_staging(&self, report: &mut SelfCheckReport) {
        let tmp = self.root.join("tmp");
        let entries = match fs::read_dir(&tmp) {
            Ok(entries) => entries,
            Err(error) => {
                report.findings.push(SelfCheckFinding::new(
                    SelfCheckFindingKind::UnreadableArchive,
                    format!("cannot read temporary staging area: {error}"),
                ));
                return;
            }
        };
        let now = unix_timestamp();
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            report.staging_directories += 1;
            // A staging directory whose lease is still in the future belongs to
            // a live operation, so only an expired or absent lease is left-behind
            // state. Nothing here reclaims it; recovery owns that decision.
            let expired = match read_lease_expiry(&entry.path()) {
                Ok(expires_at) => expires_at <= now,
                Err(_) => true,
            };
            if !expired {
                continue;
            }
            let age_seconds = entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                .map_or(0, |age| age.as_secs());
            report.orphaned_staging_directories += 1;
            report.oldest_orphaned_staging_age_seconds =
                report.oldest_orphaned_staging_age_seconds.max(age_seconds);
            let identity = read_fetch_staging_metadata(&entry.path()).ok();
            let mut finding = SelfCheckFinding::new(
                SelfCheckFindingKind::OrphanedStaging,
                "fetch staging is left behind in the temporary area".to_string(),
            )
            .at_path(&entry.file_name().to_string_lossy())
            .aged(age_seconds);
            if let Some(identity) = identity {
                finding = finding.in_repository(identity.repo_type, &identity.repo_id);
                if let Some(commit) = identity.resolved_commit.as_deref() {
                    finding = finding.at_commit(commit);
                }
            }
            report.findings.push(finding);
        }
    }

    pub fn create_fetch_staging(&self) -> ArchiveResult<PathBuf> {
        self.create_staging("fetch")
    }

    #[allow(dead_code)] // Model-default compatibility for internal callers and older tests.
    pub(crate) fn acquire_fetch_staging(
        &self,
        repo_id: &str,
        requested_revision: &str,
        files: &[String],
    ) -> ArchiveResult<FetchStaging> {
        self.acquire_fetch_staging_for_type(
            RepositoryType::Model,
            repo_id,
            requested_revision,
            files,
        )
    }

    pub(crate) fn acquire_fetch_staging_for_type(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        requested_revision: &str,
        files: &[String],
    ) -> ArchiveResult<FetchStaging> {
        validate_repo_id(repo_id)?;
        validate_component(requested_revision)?;
        for file in files {
            validate_relative_file_path(file)?;
        }
        let now = unix_timestamp();
        for entry in fs::read_dir(self.root.join("tmp"))? {
            let entry = entry?;
            if !entry.file_type()?.is_dir()
                || !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("fetch-abandoned-")
            {
                continue;
            }
            let old = entry.path();
            let Ok(metadata) = read_fetch_staging_metadata(&old) else {
                continue;
            };
            if metadata.repo_type != repo_type
                || metadata.repo_id != repo_id
                || metadata.requested_revision != requested_revision
                || !staging_selection_is_adoptable(&metadata.files, files)
                || metadata.resolved_commit.is_none()
            {
                continue;
            }
            let expires_at = match read_lease_expiry(&old) {
                Ok(expires_at) => expires_at,
                Err(ArchiveError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                    continue;
                }
                Err(error) => return Err(error),
            };
            if expires_at > now {
                return Err(ArchiveError::AlreadyPublished(old));
            }
            let operation = operation_id();
            let claimed = self
                .root
                .join("tmp")
                .join(format!(".fetch-active-{operation}"));
            match fs::rename(&old, &claimed) {
                Ok(()) => {
                    write_staging_lease(&claimed, &operation)?;
                    let resolved_commit = metadata.resolved_commit.clone();
                    if metadata.files != files {
                        // An unrestricted staging adopted by a narrower request
                        // is driven by that request from here on, so the
                        // identity the acquisition records its resolved commit
                        // under is the requesting one.
                        write_fetch_staging_metadata(
                            &claimed,
                            &FetchStagingMetadata {
                                files: files.to_vec(),
                                ..metadata
                            },
                        )?;
                    }
                    sync_directory(&self.root.join("tmp"))?;
                    spawn_lease_heartbeat(claimed.clone());
                    return Ok(FetchStaging {
                        path: claimed,
                        resumed: true,
                        resolved_commit,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            }
        }
        for entry in fs::read_dir(self.root.join("tmp"))? {
            let entry = entry?;
            if !entry.file_type()?.is_dir()
                || !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".fetch-active-")
            {
                continue;
            }
            let path = entry.path();
            if read_fetch_staging_metadata(&path).is_ok_and(|metadata| {
                metadata.repo_type == repo_type
                    && metadata.repo_id == repo_id
                    && metadata.requested_revision == requested_revision
                    && metadata.files == files
            }) {
                return Err(ArchiveError::AlreadyPublished(path));
            }
        }
        let path = self.create_staging(".fetch-active")?;
        write_fetch_staging_metadata(
            &path,
            &FetchStagingMetadata {
                version: 1,
                repo_type,
                repo_id: repo_id.into(),
                requested_revision: requested_revision.into(),
                files: files.to_vec(),
                resolved_commit: None,
            },
        )?;
        Ok(FetchStaging {
            path,
            resumed: false,
            resolved_commit: None,
        })
    }

    pub(crate) fn preserve_fetch_staging(&self, staging: &Path) -> ArchiveResult<bool> {
        let metadata = read_fetch_staging_metadata(staging)?;
        if metadata.resolved_commit.is_none() {
            fs::remove_dir_all(staging)?;
            return Ok(false);
        }
        let abandoned = self
            .root
            .join("tmp")
            .join(format!("fetch-abandoned-{}", operation_id()));
        fs::rename(staging, &abandoned)?;
        write_staging_lease_with_expiry(&abandoned, "abandoned", 0)?;
        sync_directory(&self.root.join("tmp"))?;
        Ok(true)
    }

    pub fn revision_path(&self, repo_id: &str, commit: &str) -> ArchiveResult<PathBuf> {
        self.revision_path_for_type(RepositoryType::Model, repo_id, commit)
    }

    pub fn revision_path_for_type(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        commit: &str,
    ) -> ArchiveResult<PathBuf> {
        let (namespace, name) = validate_repo_id(repo_id)?;
        validate_revision(commit)?;
        Ok(self
            .root
            .join(repo_type.archive_directory())
            .join(namespace)
            .join(name)
            .join("revisions")
            .join(commit))
    }

    pub fn is_complete_revision(&self, repo_id: &str, commit: &str) -> ArchiveResult<bool> {
        self.is_complete_revision_for_type(RepositoryType::Model, repo_id, commit)
    }

    pub fn is_complete_revision_for_type(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        commit: &str,
    ) -> ArchiveResult<bool> {
        let manifest = self.read_manifest_for(repo_type, repo_id, commit)?;
        Ok(manifest.complete)
    }

    pub fn resolve_file(
        &self,
        repo_id: &str,
        commit: &str,
        relative_path: &str,
    ) -> ArchiveResult<ResolvedFile> {
        self.resolve_file_for_type(RepositoryType::Model, repo_id, commit, relative_path)
    }

    pub fn resolve_file_for_type(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        commit: &str,
        relative_path: &str,
    ) -> ArchiveResult<ResolvedFile> {
        let relative = validate_relative_file_path(relative_path)?;
        let revision = self.revision_path_for_type(repo_type, repo_id, commit)?;
        let manifest = self.read_manifest_for(repo_type, repo_id, commit)?;
        if !manifest.complete {
            return Err(ArchiveError::IntegrityMismatch(
                "revision is not complete".into(),
            ));
        }
        let Some(entry) = manifest
            .files
            .iter()
            .find(|entry| entry.path == relative_path)
        else {
            return Err(io::Error::new(io::ErrorKind::NotFound, "archive file not found").into());
        };
        let revision_root = fs::canonicalize(&revision)?;
        let candidate = fs::canonicalize(revision.join(relative))?;
        if !candidate.starts_with(&revision_root) || !candidate.is_file() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "archive file not found").into());
        }
        Ok(ResolvedFile {
            size: fs::metadata(&candidate)?.len(),
            path: candidate,
            sha256: entry.sha256.clone(),
        })
    }

    pub fn verify_revision(&self, repo_id: &str, commit: &str) -> ArchiveResult<usize> {
        self.verify_revision_for_type(RepositoryType::Model, repo_id, commit)
    }

    pub fn verify_revision_for_type(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        commit: &str,
    ) -> ArchiveResult<usize> {
        let result = self.verify_revision_inner(repo_type, repo_id, commit);
        if let Err(error) = &result {
            tracing::warn!(
                event = "archive_verification_failed",
                repo_type = %repo_type,
                repo_id,
                commit,
                error_class = match error {
                    ArchiveError::IntegrityMismatch(_) => "integrity",
                    ArchiveError::Io(io_error) if io_error.kind() == io::ErrorKind::NotFound =>
                        "not_found",
                    ArchiveError::Io(_) => "storage",
                    ArchiveError::InvalidPath(_) => "unsafe_path",
                    ArchiveError::AlreadyPublished(_) => "conflict",
                    ArchiveError::ReferencedRevision(_) => "referenced_revision",
                },
                "archive verification failed"
            );
        }
        result
    }

    fn verify_revision_inner(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        commit: &str,
    ) -> ArchiveResult<usize> {
        let revision = self.revision_path_for_type(repo_type, repo_id, commit)?;
        let manifest_path = revision.join(".modelkeep-manifest.json");
        let manifest: Manifest = serde_json::from_slice(&fs::read(&manifest_path)?)
            .map_err(|error| ArchiveError::IntegrityMismatch(error.to_string()))?;
        self.ensure_manifest_type(repo_type, &manifest)?;
        if !manifest.complete {
            return Err(ArchiveError::IntegrityMismatch(
                "revision is not complete".into(),
            ));
        }
        let mut verified = 0;
        for entry in &manifest.files {
            let resolved = self.resolve_file_for_type(repo_type, repo_id, commit, &entry.path)?;
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

    fn read_manifest_for(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        commit: &str,
    ) -> ArchiveResult<Manifest> {
        let manifest: Manifest =
            serde_json::from_str(&self.manifest_for_type(repo_type, repo_id, commit)?)
                .map_err(|error| ArchiveError::IntegrityMismatch(error.to_string()))?;
        self.ensure_manifest_type(repo_type, &manifest)?;
        Ok(manifest)
    }

    fn ensure_manifest_type(
        &self,
        repo_type: RepositoryType,
        manifest: &Manifest,
    ) -> ArchiveResult<()> {
        if manifest.repo_type != repo_type {
            return Err(ArchiveError::IntegrityMismatch(format!(
                "manifest repository type {} does not match archive root {repo_type}",
                manifest.repo_type
            )));
        }
        Ok(())
    }

    pub fn audit(&self) -> ArchiveResult<AuditReport> {
        let mut report = AuditReport {
            checked: 0,
            failures: Vec::new(),
        };
        for repo_type in RepositoryType::ALL {
            let root = self.root.join(repo_type.archive_directory());
            if !root.is_dir() {
                continue;
            }
            for namespace in fs::read_dir(root)? {
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
                        tracing::info!(repo_type = %repo_type, repo_id = %repo_id, commit = %commit, "archive audit revision");
                        if let Err(error) =
                            self.verify_revision_for_type(repo_type, &repo_id, &commit)
                        {
                            report.failures.push(AuditFailure {
                                repo_type,
                                repo_id: repo_id.clone(),
                                commit,
                                error: error.to_string(),
                            });
                        }
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
        self.publish_revision_from_directory_for_type(RepositoryType::Model, request)
    }

    pub fn publish_revision_from_directory_for_type(
        &self,
        repo_type: RepositoryType,
        request: SourcePublishRequest,
    ) -> ArchiveResult<PathBuf> {
        self.publish_revision_from_directory_with_progress_for_type(repo_type, request, &|_| {})
    }

    pub fn publish_revision_from_directory_with_progress(
        &self,
        request: SourcePublishRequest,
        progress: &(dyn Fn(&str) + Send + Sync),
    ) -> ArchiveResult<PathBuf> {
        self.publish_revision_from_directory_with_progress_for_type(
            RepositoryType::Model,
            request,
            progress,
        )
    }

    pub fn publish_revision_from_directory_with_progress_for_type(
        &self,
        repo_type: RepositoryType,
        request: SourcePublishRequest,
        progress: &(dyn Fn(&str) + Send + Sync),
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
            .join(repo_type.archive_directory())
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
        progress("validating_revision");
        let result = if reuse_staging {
            Self::write_source_manifest(&staging, &source_root, repo_type, &request, progress)
        } else {
            self.write_source_revision(&staging, &source_root, repo_type, &request)
        };
        if let Err(error) = result {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
        progress("publishing_revision");
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

    /// Adds absent files to an already published model revision.
    pub fn extend_revision_from_directory(
        &self,
        request: RevisionExtensionRequest,
    ) -> ArchiveResult<RevisionExtension> {
        self.extend_revision_from_directory_for_type(RepositoryType::Model, request)
    }

    /// Extends a published revision with files its manifest does not list.
    ///
    /// Extension is additive. A requested path the live manifest already lists is
    /// reported as skipped, and neither its bytes nor its recorded size and digest
    /// are touched; recorded entries are carried over as they stand rather than
    /// rehashed. New files are staged, validated, and made durable inside the
    /// revision directory before the manifest is atomically replaced with a
    /// superset, so an interruption leaves either the old or the new manifest live
    /// and a file the live manifest does not list is never served.
    pub fn extend_revision_from_directory_for_type(
        &self,
        repo_type: RepositoryType,
        request: RevisionExtensionRequest,
    ) -> ArchiveResult<RevisionExtension> {
        self.extend_revision_from_directory_inner(repo_type, &request, &|| Ok(()))
    }

    fn extend_revision_from_directory_inner(
        &self,
        repo_type: RepositoryType,
        request: &RevisionExtensionRequest,
        before_manifest_swap: &(dyn Fn() -> ArchiveResult<()> + Sync),
    ) -> ArchiveResult<RevisionExtension> {
        let (namespace, name) = validate_repo_id(&request.repo_id)?;
        validate_revision(&request.commit)?;
        let revisions = self
            .root
            .join(repo_type.archive_directory())
            .join(namespace)
            .join(name)
            .join("revisions");
        let revision = revisions.join(&request.commit);
        if !revision.is_dir() {
            return Err(
                io::Error::new(io::ErrorKind::NotFound, "revision is not published").into(),
            );
        }

        let mut skipped = BTreeSet::new();
        let mut requested = BTreeSet::new();
        let mut candidates = Vec::new();
        let listed = self
            .read_extendable_manifest(repo_type, request)?
            .files
            .into_iter()
            .map(|file| file.path)
            .collect::<BTreeSet<_>>();
        for source_file in &request.files {
            validate_relative_file_path(&source_file.path)?;
            if !requested.insert(source_file.path.clone()) {
                return Err(ArchiveError::IntegrityMismatch(format!(
                    "duplicate archive path: {}",
                    source_file.path
                )));
            }
            if listed.contains(&source_file.path) {
                skipped.insert(source_file.path.clone());
            } else {
                candidates.push(source_file);
            }
        }
        if candidates.is_empty() {
            return Ok(RevisionExtension {
                commit: request.commit.clone(),
                path: revision,
                added: Vec::new(),
                skipped: skipped.into_iter().collect(),
            });
        }

        let source_root = fs::canonicalize(&request.source_root)?;
        if !source_root.is_dir() {
            return Err(ArchiveError::InvalidPath(
                request.source_root.display().to_string(),
            ));
        }
        let staging = StagingGuard(self.create_staging("extend")?);
        let staged = Self::stage_extension_files(&staging.0, &source_root, &candidates)?;

        // The manifest is re-read under the lock: a concurrent extension may have
        // published one of these paths since the candidate set was computed.
        let _lock = lock_revision_extension(&revisions, &request.commit)?;
        let manifest = self.read_extendable_manifest(repo_type, request)?;
        let published = manifest
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<BTreeSet<_>>();
        let mut entries = manifest
            .files
            .iter()
            .map(|file| (file.path.as_str(), file.size, file.sha256.clone()))
            .collect::<Vec<_>>();
        let mut added = Vec::new();
        let mut directories = BTreeSet::new();
        for (path, size, digest) in &staged {
            if published.contains(path.as_str()) {
                skipped.insert(path.clone());
                continue;
            }
            let relative = validate_relative_file_path(path)?;
            let destination = revision.join(relative);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
                directories.insert(parent.to_path_buf());
            }
            fs::rename(staging.0.join(relative), &destination)?;
            added.push(path.clone());
            entries.push((path.as_str(), *size, digest.clone()));
        }
        added.sort();

        if !added.is_empty() {
            for directory in &directories {
                sync_directory(directory)?;
            }
            sync_directory(&revision)?;
            before_manifest_swap()?;
            replace_revision_manifest(
                &revisions, &revision, repo_type, request, &manifest, &entries,
            )?;
        }
        Ok(RevisionExtension {
            commit: request.commit.clone(),
            path: revision,
            added,
            skipped: skipped.into_iter().collect(),
        })
    }

    /// Reads the manifest of a revision an extension is about to grow.
    fn read_extendable_manifest(
        &self,
        repo_type: RepositoryType,
        request: &RevisionExtensionRequest,
    ) -> ArchiveResult<Manifest> {
        let manifest = self.read_manifest_for(repo_type, &request.repo_id, &request.commit)?;
        if !manifest.complete {
            return Err(ArchiveError::IntegrityMismatch(
                "revision is not complete".into(),
            ));
        }
        if !manifest.repo_id.is_empty() && manifest.repo_id != request.repo_id {
            return Err(ArchiveError::IntegrityMismatch(format!(
                "manifest repository {} does not match {}",
                manifest.repo_id, request.repo_id
            )));
        }
        Ok(manifest)
    }

    /// Copies extension sources into staging and records their size and digest.
    fn stage_extension_files(
        staging: &Path,
        source_root: &Path,
        candidates: &[&SourceFile],
    ) -> ArchiveResult<Vec<(String, u64, String)>> {
        let mut staged = Vec::with_capacity(candidates.len());
        for source_file in candidates {
            let relative = validate_relative_file_path(&source_file.path)?;
            if !fs::symlink_metadata(&source_file.source)?
                .file_type()
                .is_file()
            {
                return Err(ArchiveError::InvalidPath(source_file.path.clone()));
            }
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
            staged.push((
                source_file.path.clone(),
                size,
                hex_digest(hasher.finalize().as_slice()),
            ));
        }
        Ok(staged)
    }

    fn create_staging(&self, prefix: &str) -> ArchiveResult<PathBuf> {
        for _ in 0..16 {
            let operation = operation_id();
            let staging = self.root.join("tmp").join(format!("{prefix}-{operation}"));
            match fs::create_dir(&staging) {
                Ok(()) => {
                    write_staging_lease(&staging, &operation)?;
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

    fn write_revision(
        &self,
        staging: &Path,
        repo_type: RepositoryType,
        request: &PublishRequest,
    ) -> ArchiveResult<()> {
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
            repo_type,
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
        repo_type: RepositoryType,
        request: &SourcePublishRequest,
        progress: &(dyn Fn(&str) + Send + Sync),
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
            repo_type,
            &request.repo_id,
            &request.requested_revision,
            &request.commit,
            &entries,
        )?;
        progress("syncing_revision");
        file.sync_all()?;
        sync_directory(staging)?;
        Ok(())
    }

    fn write_source_revision(
        &self,
        staging: &Path,
        source_root: &Path,
        repo_type: RepositoryType,
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
            repo_type,
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

#[allow(dead_code)] // Model-default compatibility for internal callers and older tests.
pub(crate) fn record_fetch_resolved_commit(
    staging: &Path,
    request_repo_id: &str,
    request_revision: &str,
    files: &[String],
    commit: &str,
) -> ArchiveResult<()> {
    record_fetch_resolved_commit_for_type(
        staging,
        RepositoryType::Model,
        request_repo_id,
        request_revision,
        files,
        commit,
    )
}

/// Records a commit's upstream file list inside the acquisition's staging
/// directory.
///
/// The fetch helper learns the list while resolving the revision, long before
/// the revision can be published, so it is parked here and installed beside the
/// revision once one exists. The name carries the `.modelkeep-` prefix, so it is
/// never a manifest entry, never served, and never mistaken for payload.
pub(crate) fn write_staged_upstream_files(
    staging: &Path,
    repo_type: RepositoryType,
    repo_id: &str,
    commit: &str,
    files: &[UpstreamFile],
) -> ArchiveResult<()> {
    validate_revision(commit)?;
    validate_repo_id(repo_id)?;
    if files.is_empty() {
        return Ok(());
    }
    let record = UpstreamFileList {
        version: 1,
        repo_type,
        repo_id: repo_id.to_string(),
        commit: commit.to_string(),
        files: files
            .iter()
            .cloned()
            .filter_map(UpstreamFile::sanitized)
            .collect(),
    };
    let temporary = staging.join(format!(".modelkeep-upstream-files-{}.part", operation_id()));
    let result = (|| -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(
            &serde_json::to_vec(&record)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
        )?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, staging.join(UPSTREAM_FILES_FILE))?;
        sync_directory(staging)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(ArchiveError::from)
}

/// The upstream file list an acquisition parked in its staging directory.
///
/// Empty covers every way the list can be unavailable — no record, an
/// unreadable one, or one describing a different commit or repository type —
/// because the caller's only use for it is recording what it knows. A revision
/// whose list is unknown is served from what the archive holds, which is what
/// ModelKeep did before any list was recorded.
pub(crate) fn staged_upstream_files(
    staging: &Path,
    repo_type: RepositoryType,
    commit: &str,
) -> Vec<UpstreamFile> {
    let Ok(bytes) = fs::read(staging.join(UPSTREAM_FILES_FILE)) else {
        return Vec::new();
    };
    let Ok(record) = serde_json::from_slice::<UpstreamFileList>(&bytes) else {
        return Vec::new();
    };
    if record.version != 1 || record.repo_type != repo_type || record.commit != commit {
        return Vec::new();
    }
    record
        .files
        .into_iter()
        .filter_map(UpstreamFile::sanitized)
        .collect()
}

pub(crate) fn record_fetch_resolved_commit_for_type(
    staging: &Path,
    repo_type: RepositoryType,
    request_repo_id: &str,
    request_revision: &str,
    files: &[String],
    commit: &str,
) -> ArchiveResult<()> {
    validate_revision(commit)?;
    let mut metadata = read_fetch_staging_metadata(staging)?;
    if metadata.repo_type != repo_type
        || metadata.repo_id != request_repo_id
        || metadata.requested_revision != request_revision
        || metadata.files != files
    {
        return Err(ArchiveError::IntegrityMismatch(
            "fetch staging identity changed".into(),
        ));
    }
    if metadata
        .resolved_commit
        .as_deref()
        .is_some_and(|resolved| resolved != commit)
    {
        return Err(ArchiveError::IntegrityMismatch(
            "resolved fetch commit changed".into(),
        ));
    }
    metadata.resolved_commit = Some(commit.into());
    write_fetch_staging_metadata(staging, &metadata)
}

/// Whether staging recorded under `recorded` may be adopted by a request whose
/// selection identity is `requested`.
///
/// An unrestricted acquisition already covered every path a narrower request
/// asks for, so the partial bytes it left are bytes of those same paths at the
/// same commit; adopting them is what keeps an interrupted large fetch from
/// starting over (ADR-0017). Two different restricted selections still never
/// share staging. Adoption never decides what gets published: the acquisition
/// runs under the requesting selection and only helper-reported files reach the
/// manifest.
fn staging_selection_is_adoptable(recorded: &[String], requested: &[String]) -> bool {
    recorded.is_empty() || recorded == requested
}

fn read_fetch_staging_metadata(staging: &Path) -> ArchiveResult<FetchStagingMetadata> {
    let metadata: FetchStagingMetadata =
        serde_json::from_slice(&fs::read(staging.join(FETCH_STAGING_FILE))?)
            .map_err(|error| ArchiveError::IntegrityMismatch(error.to_string()))?;
    if metadata.version != 1
        || metadata
            .resolved_commit
            .as_deref()
            .is_some_and(|value| !is_hf_commit(value))
    {
        return Err(ArchiveError::IntegrityMismatch(
            "invalid fetch staging metadata".into(),
        ));
    }
    validate_repo_id(&metadata.repo_id)?;
    validate_component(&metadata.requested_revision)?;
    for file in &metadata.files {
        validate_relative_file_path(file)?;
    }
    Ok(metadata)
}

fn write_fetch_staging_metadata(
    staging: &Path,
    metadata: &FetchStagingMetadata,
) -> ArchiveResult<()> {
    let temporary = staging.join(format!(".modelkeep-fetch-{}.part", operation_id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    serde_json::to_writer(&mut file, metadata)
        .map_err(|error| ArchiveError::IntegrityMismatch(error.to_string()))?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(temporary, staging.join(FETCH_STAGING_FILE))?;
    sync_directory(staging)?;
    Ok(())
}

fn read_lease_expiry(staging: &Path) -> ArchiveResult<u64> {
    fs::read_to_string(staging.join(STAGING_LEASE_FILE))?
        .lines()
        .find_map(|line| line.strip_prefix("expires_at=")?.parse().ok())
        .ok_or_else(|| ArchiveError::IntegrityMismatch("staging lease has no expiry".into()))
}

fn write_staging_lease(staging: &Path, nonce: &str) -> ArchiveResult<()> {
    write_staging_lease_with_expiry(staging, nonce, unix_timestamp() + STAGING_LEASE_SECONDS)
}

fn write_staging_lease_with_expiry(
    staging: &Path,
    nonce: &str,
    expires_at: u64,
) -> ArchiveResult<()> {
    let temporary = staging.join(".modelkeep-staging-lease.part");
    let mut lease = File::create(&temporary)?;
    writeln!(lease, "nonce={nonce}")?;
    writeln!(lease, "pid={}", process::id())?;
    writeln!(lease, "expires_at={}", expires_at)?;
    lease.sync_all()?;
    fs::rename(temporary, staging.join(STAGING_LEASE_FILE))?;
    Ok(())
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

/// Removes an extension staging directory once the operation leaves scope.
struct StagingGuard(PathBuf);

impl Drop for StagingGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Serializes manifest replacement for one published revision.
///
/// The lock file lives beside the revision directories rather than inside a
/// revision, so an extension never adds an internal file to a published
/// revision. `flock` is released when the descriptor closes, including on
/// process death, so an interrupted extension leaves no lock to break.
fn lock_revision_extension(revisions: &Path, commit: &str) -> ArchiveResult<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(revisions.join(format!(".modelkeep-extend-{commit}.lock")))?;
    rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive)
        .map_err(|error| ArchiveError::Io(io::Error::from(error)))?;
    Ok(file)
}

/// Atomically replaces a published revision's manifest with a superset.
///
/// The replacement is written outside the revision directory and renamed into
/// place, so a crash leaves either the old or the new manifest live and never a
/// partially written one.
fn replace_revision_manifest(
    revisions: &Path,
    revision: &Path,
    repo_type: RepositoryType,
    request: &RevisionExtensionRequest,
    manifest: &Manifest,
    entries: &[(&str, u64, String)],
) -> ArchiveResult<()> {
    let repo_id = if manifest.repo_id.is_empty() {
        request.repo_id.as_str()
    } else {
        manifest.repo_id.as_str()
    };
    let requested_revision = if manifest.requested_revision.is_empty() {
        request.commit.as_str()
    } else {
        manifest.requested_revision.as_str()
    };
    let temporary = revisions.join(format!(
        ".modelkeep-manifest-{}-{}.part",
        request.commit,
        operation_id()
    ));
    let result = (|| -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        write_manifest(
            &mut file,
            repo_type,
            repo_id,
            requested_revision,
            &request.commit,
            entries,
        )?;
        file.sync_all()?;
        fs::rename(&temporary, revision.join(".modelkeep-manifest.json"))?;
        sync_directory(revision)?;
        sync_directory(revisions)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(ArchiveError::from)
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
        || is_internal_archive_path(value)
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

pub(crate) fn is_internal_archive_path(value: &str) -> bool {
    let mut components = value.split('/');
    let first = components.next().unwrap_or("");
    first.starts_with(".modelkeep-")
        || first == ".cache"
        || components.any(|component| component == ".cache")
}

fn write_manifest(
    output: &mut File,
    repo_type: RepositoryType,
    repo_id: &str,
    requested_revision: &str,
    commit: &str,
    entries: &[(&str, u64, String)],
) -> io::Result<()> {
    write!(
        output,
        "{{\"version\":1,\"complete\":true,\"repo_type\":\"{}\",\"repo_id\":\"{}\",\"requested_revision\":\"{}\",\"commit\":\"{}\",\"archived_at\":{},\"files\":[",
        repo_type,
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
        // ModelKeep's own internal state inside a revision directory — the
        // manifest and the recorded upstream file list — is not archive
        // content: it carries no manifest entry, is never served, and so is
        // never part of the file set a manifest is compared against.
        if relative
            .to_str()
            .is_some_and(crate::is_internal_archive_path)
        {
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
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::EnvFilter;

    #[derive(Clone, Default)]
    struct LogWriter(Arc<Mutex<Vec<u8>>>);

    impl io::Write for LogWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogWriter {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    impl LogWriter {
        fn output(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    fn capture_logs() -> (LogWriter, tracing::subscriber::DefaultGuard) {
        let writer = LogWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_env_filter(EnvFilter::new("info"))
            .with_writer(writer.clone())
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        (writer, guard)
    }

    fn archive() -> (Archive, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        (archive, directory)
    }

    #[test]
    fn model_and_dataset_with_same_id_are_isolated() {
        let (archive, directory) = archive();
        let commit = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        for (repo_type, contents) in [
            (RepositoryType::Model, b"model".as_slice()),
            (RepositoryType::Dataset, b"dataset".as_slice()),
        ] {
            archive
                .publish_revision_for_type(
                    repo_type,
                    PublishRequest {
                        repo_id: "org/shared".into(),
                        requested_revision: "main".into(),
                        commit: commit.into(),
                        files: vec![ArchiveFile {
                            path: "data.bin".into(),
                            bytes: contents.to_vec(),
                        }],
                    },
                )
                .unwrap();
            archive
                .update_ref_for_type(repo_type, "org/shared", "main", commit)
                .unwrap();
        }

        assert_eq!(
            fs::read(
                archive
                    .resolve_file_for_type(RepositoryType::Model, "org/shared", commit, "data.bin",)
                    .unwrap()
                    .path,
            )
            .unwrap(),
            b"model"
        );
        assert_eq!(
            fs::read(
                archive
                    .resolve_file_for_type(
                        RepositoryType::Dataset,
                        "org/shared",
                        commit,
                        "data.bin",
                    )
                    .unwrap()
                    .path,
            )
            .unwrap(),
            b"dataset"
        );
        assert!(directory.path().join("models/org/shared").is_dir());
        assert!(directory.path().join("datasets/org/shared").is_dir());
        let repositories = archive.list_repositories().unwrap();
        assert_eq!(repositories.len(), 2);
        assert_eq!(repositories[0].repo_type, RepositoryType::Model);
        assert_eq!(repositories[1].repo_type, RepositoryType::Dataset);
    }

    #[test]
    fn manifest_repository_type_must_match_archive_root() {
        let (archive, _directory) = archive();
        let commit = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        archive
            .publish_revision_for_type(
                RepositoryType::Dataset,
                PublishRequest {
                    repo_id: "org/data".into(),
                    requested_revision: "main".into(),
                    commit: commit.into(),
                    files: vec![ArchiveFile {
                        path: "data.json".into(),
                        bytes: b"{}".to_vec(),
                    }],
                },
            )
            .unwrap();
        let manifest = archive
            .revision_path_for_type(RepositoryType::Dataset, "org/data", commit)
            .unwrap()
            .join(".modelkeep-manifest.json");
        let contents = fs::read_to_string(&manifest)
            .unwrap()
            .replace("\"repo_type\":\"dataset\"", "\"repo_type\":\"model\"");
        fs::write(manifest, contents).unwrap();

        assert!(matches!(
            archive.verify_revision_for_type(RepositoryType::Dataset, "org/data", commit),
            Err(ArchiveError::IntegrityMismatch(_))
        ));
        let report = archive.audit().unwrap();
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].repo_type, RepositoryType::Dataset);
    }

    #[test]
    fn fetch_staging_identity_includes_repository_type() {
        let (archive, _directory) = archive();
        let model = archive
            .acquire_fetch_staging_for_type(RepositoryType::Model, "org/shared", "main", &[])
            .unwrap();
        let dataset = archive
            .acquire_fetch_staging_for_type(RepositoryType::Dataset, "org/shared", "main", &[])
            .unwrap();
        assert_ne!(model.path, dataset.path);
        assert!(record_fetch_resolved_commit_for_type(
            &model.path,
            RepositoryType::Dataset,
            "org/shared",
            "main",
            &[],
            &"c".repeat(40),
        )
        .is_err());
    }

    #[test]
    fn old_model_only_archive_opens_read_only_without_dataset_directory() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("models")).unwrap();
        fs::create_dir(directory.path().join("tmp")).unwrap();

        let archive = Archive::open_read_only(directory.path()).unwrap();
        assert!(archive.list_repositories().unwrap().is_empty());
        assert_eq!(archive.audit().unwrap().checked, 0);
        assert!(!directory.path().join("datasets").exists());
    }

    #[test]
    fn writable_upgrade_only_adds_empty_dataset_namespace() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("models")).unwrap();
        fs::create_dir(directory.path().join("tmp")).unwrap();
        let sentinel = directory.path().join("models/operator-sentinel");
        fs::write(&sentinel, b"unchanged").unwrap();

        let _archive = Archive::new(directory.path()).unwrap();

        assert_eq!(fs::read(sentinel).unwrap(), b"unchanged");
        let datasets = directory.path().join("datasets");
        assert!(datasets.is_dir());
        assert_eq!(fs::read_dir(datasets).unwrap().count(), 0);
    }

    #[test]
    fn typed_removal_does_not_remove_same_id_sibling_type() {
        let (archive, _directory) = archive();
        let commit = "dddddddddddddddddddddddddddddddddddddddd";
        for repo_type in RepositoryType::ALL {
            archive
                .publish_revision_for_type(
                    repo_type,
                    PublishRequest {
                        repo_id: "org/shared-removal".into(),
                        requested_revision: commit.into(),
                        commit: commit.into(),
                        files: vec![ArchiveFile {
                            path: "payload.bin".into(),
                            bytes: repo_type.to_string().into_bytes(),
                        }],
                    },
                )
                .unwrap();
        }

        archive
            .remove_revision_for_type(RepositoryType::Dataset, "org/shared-removal", commit, false)
            .unwrap();

        assert!(!archive
            .revision_path_for_type(RepositoryType::Dataset, "org/shared-removal", commit)
            .unwrap()
            .exists());
        let model = archive
            .resolve_file_for_type(
                RepositoryType::Model,
                "org/shared-removal",
                commit,
                "payload.bin",
            )
            .unwrap();
        assert_eq!(fs::read(model.path).unwrap(), b"model");
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

        for path in [
            ".modelkeep-staging-lease",
            ".modelkeep-fetch.json",
            ".cache/huggingface/download.json",
            "nested/.cache/download.json",
        ] {
            let mut bad = request("cccccccc", b"internal");
            bad.files[0].path = path.into();
            assert!(matches!(
                archive.publish_revision(bad),
                Err(ArchiveError::InvalidPath(_))
            ));
        }
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
    fn resumes_matching_expired_fetch_staging_exclusively() {
        let (writer, _guard) = capture_logs();
        let (archive, _directory) = archive();
        let original = archive
            .acquire_fetch_staging("org/model", "main", &[])
            .unwrap();
        fs::write(original.path.join("partial.bin"), b"partial").unwrap();
        record_fetch_resolved_commit(&original.path, "org/model", "main", &[], &"a".repeat(40))
            .unwrap();
        fs::write(
            original.path.join(STAGING_LEASE_FILE),
            "nonce=expired\npid=1\nexpires_at=0\n",
        )
        .unwrap();

        assert_eq!(archive.recover_incomplete().unwrap(), 0);
        let resumed = archive
            .acquire_fetch_staging("org/model", "main", &[])
            .unwrap();
        assert!(resumed.resumed);
        assert_eq!(resumed.resolved_commit, Some("a".repeat(40)));
        assert_eq!(
            fs::read(resumed.path.join("partial.bin")).unwrap(),
            b"partial"
        );
        assert!(!original.path.exists());
        assert!(matches!(
            archive.acquire_fetch_staging("org/model", "main", &[]),
            Err(ArchiveError::AlreadyPublished(_))
        ));
        let output = writer.output();
        assert!(output.contains("incomplete_fetch_recovered"));
        assert!(output.contains("preserved_for_resume"));
        assert!(output.contains("org/model"));
        assert!(output.contains("main"));
        assert!(output.contains(&"a".repeat(40)));
    }

    /// ADR-0017: an interrupted unrestricted acquisition already covered every
    /// path a narrower request asks for, so its partial bytes are resumable.
    #[test]
    fn unrestricted_fetch_staging_is_adopted_by_a_narrower_request() {
        let (archive, _directory) = archive();
        let commit = "a".repeat(40);
        let original = archive
            .acquire_fetch_staging("org/model", "main", &[])
            .unwrap();
        fs::write(original.path.join("partial.bin"), b"partial").unwrap();
        record_fetch_resolved_commit(&original.path, "org/model", "main", &[], &commit).unwrap();
        assert!(archive.preserve_fetch_staging(&original.path).unwrap());

        let wanted = vec!["partial.bin".to_string()];
        let resumed = archive
            .acquire_fetch_staging("org/model", "main", &wanted)
            .unwrap();
        assert!(resumed.resumed);
        assert_eq!(resumed.resolved_commit, Some(commit.clone()));
        assert_eq!(
            fs::read(resumed.path.join("partial.bin")).unwrap(),
            b"partial"
        );
        // The adopted staging carries the requesting identity from here on.
        record_fetch_resolved_commit(&resumed.path, "org/model", "main", &wanted, &commit).unwrap();
        // A different resolved commit is still refused.
        assert!(matches!(
            record_fetch_resolved_commit(
                &resumed.path,
                "org/model",
                "main",
                &wanted,
                &"b".repeat(40)
            ),
            Err(ArchiveError::IntegrityMismatch(_))
        ));
    }

    #[test]
    fn unrestricted_fetch_staging_for_another_identity_is_not_adopted() {
        let (archive, _directory) = archive();
        let commit = "a".repeat(40);
        for (repo_type, repo_id, revision) in [
            (RepositoryType::Model, "org/other", "main"),
            (RepositoryType::Model, "org/model", "dev"),
            (RepositoryType::Dataset, "org/model", "main"),
        ] {
            let staging = archive
                .acquire_fetch_staging_for_type(repo_type, repo_id, revision, &[])
                .unwrap();
            record_fetch_resolved_commit_for_type(
                &staging.path,
                repo_type,
                repo_id,
                revision,
                &[],
                &commit,
            )
            .unwrap();
            assert!(archive.preserve_fetch_staging(&staging.path).unwrap());
        }

        let fresh = archive
            .acquire_fetch_staging("org/model", "main", &["partial.bin".to_string()])
            .unwrap();
        assert!(!fresh.resumed);
        assert_eq!(fresh.resolved_commit, None);
    }

    #[test]
    fn recovery_discards_fetch_staging_without_a_resolved_commit() {
        let (writer, _guard) = capture_logs();
        let (archive, _directory) = archive();
        let staging = archive
            .acquire_fetch_staging("org/model", "main", &[])
            .unwrap();
        fs::write(
            staging.path.join(STAGING_LEASE_FILE),
            "nonce=expired\npid=1\nexpires_at=0\n",
        )
        .unwrap();
        assert_eq!(archive.recover_incomplete().unwrap(), 1);
        assert!(!staging.path.exists());
        let output = writer.output();
        assert!(output.contains("incomplete_fetch_recovered"));
        assert!(output.contains("discarded"));
        assert!(output.contains("org/model"));
        assert!(output.contains("main"));
        assert!(!output.contains(STAGING_LEASE_FILE));
    }

    #[test]
    fn verification_failure_event_is_correlated_and_credential_safe() {
        let (archive, _directory) = archive();
        archive
            .publish_revision(PublishRequest {
                repo_id: "org/model".into(),
                requested_revision: "main".into(),
                commit: "ffffffffffffffffffffffffffffffffffffffff".into(),
                files: vec![ArchiveFile {
                    path: "config.json".into(),
                    bytes: b"safe".to_vec(),
                }],
            })
            .unwrap();
        fs::write(
            archive
                .revision_path("org/model", "ffffffffffffffffffffffffffffffffffffffff")
                .unwrap()
                .join("config.json"),
            b"Bearer credential-must-not-appear",
        )
        .unwrap();
        let (writer, _guard) = capture_logs();

        assert!(matches!(
            archive.verify_revision("org/model", "ffffffffffffffffffffffffffffffffffffffff"),
            Err(ArchiveError::IntegrityMismatch(_))
        ));

        let output = writer.output();
        assert!(output.contains("archive_verification_failed"));
        assert!(output.contains("org/model"));
        assert!(output.contains("ffffffffffffffffffffffffffffffffffffffff"));
        assert!(output.contains("integrity"));
        assert!(!output.contains("credential-must-not-appear"));
    }

    #[test]
    fn resolved_fetch_commit_cannot_change() {
        let (archive, _directory) = archive();
        let staging = archive
            .acquire_fetch_staging("org/model", "main", &[])
            .unwrap();
        record_fetch_resolved_commit(&staging.path, "org/model", "main", &[], &"a".repeat(40))
            .unwrap();
        assert!(matches!(
            record_fetch_resolved_commit(&staging.path, "org/model", "main", &[], &"b".repeat(40)),
            Err(ArchiveError::IntegrityMismatch(_))
        ));
    }

    #[test]
    fn concurrent_retries_claim_one_abandoned_fetch() {
        let (archive, _directory) = archive();
        let staging = archive
            .acquire_fetch_staging("org/model", "main", &[])
            .unwrap();
        record_fetch_resolved_commit(&staging.path, "org/model", "main", &[], &"a".repeat(40))
            .unwrap();
        fs::write(
            staging.path.join(STAGING_LEASE_FILE),
            "nonce=expired\npid=1\nexpires_at=0\n",
        )
        .unwrap();
        archive.recover_incomplete().unwrap();

        let barrier = Arc::new(std::sync::Barrier::new(3));
        let mut handles = Vec::new();
        for _ in 0..2 {
            let archive = archive.clone();
            let barrier = barrier.clone();
            handles.push(thread::spawn(move || {
                barrier.wait();
                archive.acquire_fetch_staging("org/model", "main", &[])
            }));
        }
        barrier.wait();
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert_eq!(
            results
                .iter()
                .filter(|result| result.as_ref().is_ok_and(|staging| staging.resumed))
                .count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(ArchiveError::AlreadyPublished(_))))
                .count(),
            1
        );
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
    fn a_recorded_upstream_file_list_is_treated_as_untrusted_input() {
        let (archive, _directory) = archive();
        let commit = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        archive
            .publish_revision(PublishRequest {
                repo_id: "org/model".into(),
                requested_revision: "main".into(),
                commit: commit.into(),
                files: vec![ArchiveFile {
                    path: "config.json".into(),
                    bytes: b"valid".to_vec(),
                }],
            })
            .unwrap();
        let entry = |path: &str, blob_id: Option<&str>| UpstreamFile {
            path: path.into(),
            size: Some(1),
            blob_id: blob_id.map(str::to_string),
            lfs_sha256: None,
        };
        assert!(archive
            .record_upstream_files_for_type(
                RepositoryType::Model,
                "org/model",
                commit,
                &[
                    // Upstream metadata reaches ModelKeep through a helper's
                    // stdout and is served back to clients, so a path that
                    // escapes the revision, names ModelKeep's own internal
                    // state, or is absolute is dropped rather than recorded.
                    entry("../escape", None),
                    entry("/absolute", None),
                    entry(".modelkeep-manifest.json", None),
                    entry(".cache/huggingface/download.json", None),
                    // An object id that is not a plain hexadecimal digest is
                    // never echoed back into a response.
                    entry("weights/a.bin", Some("../../etc/passwd")),
                    entry("config.json", Some(&"b".repeat(40))),
                ],
            )
            .unwrap());

        let recorded = archive
            .upstream_files_for_type(RepositoryType::Model, "org/model", commit)
            .unwrap()
            .unwrap();
        assert_eq!(
            recorded,
            vec![
                UpstreamFile {
                    path: "weights/a.bin".into(),
                    size: Some(1),
                    blob_id: None,
                    lfs_sha256: None,
                },
                UpstreamFile {
                    path: "config.json".into(),
                    size: Some(1),
                    blob_id: Some("b".repeat(40)),
                    lfs_sha256: None,
                },
            ]
        );

        // The record is ModelKeep's own state: it is never resolvable as a file,
        // whatever a client asks for.
        assert!(archive
            .resolve_file("org/model", commit, UPSTREAM_FILES_FILE)
            .is_err());
        // Recording is not a way to rewrite a published revision.
        assert!(!archive
            .record_upstream_files_for_type(
                RepositoryType::Model,
                "org/model",
                commit,
                &[entry("other.json", None)],
            )
            .unwrap());
        assert_eq!(
            archive
                .upstream_files_for_type(RepositoryType::Model, "org/model", commit)
                .unwrap()
                .unwrap(),
            recorded
        );
        // An absent revision cannot be given a file list at all.
        assert!(archive
            .record_upstream_files_for_type(
                RepositoryType::Model,
                "org/model",
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                &[entry("config.json", None)],
            )
            .is_err());
    }

    #[test]
    fn read_only_open_does_not_create_a_missing_archive() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing");
        assert!(Archive::open_read_only(&missing).is_err());
        assert!(!missing.exists());
    }

    fn published_revision_for_extension(
        archive: &Archive,
        directory: &Path,
        commit: &str,
    ) -> PathBuf {
        let source = directory.join(format!("published-{commit}"));
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::write(source.join("config.json"), b"{\"base\":true}").unwrap();
        fs::write(source.join("nested/first.bin"), b"first").unwrap();
        archive
            .publish_revision_from_directory(SourcePublishRequest {
                repo_id: "org/model".into(),
                requested_revision: "main".into(),
                commit: commit.into(),
                source_root: source.clone(),
                files: vec![
                    SourceFile {
                        path: "config.json".into(),
                        source: source.join("config.json"),
                    },
                    SourceFile {
                        path: "nested/first.bin".into(),
                        source: source.join("nested/first.bin"),
                    },
                ],
            })
            .unwrap()
    }

    fn extension_source(directory: &Path, name: &str, files: &[(&str, &[u8])]) -> PathBuf {
        let root = directory.join(name);
        fs::create_dir_all(&root).unwrap();
        for (path, bytes) in files {
            let destination = root.join(path);
            fs::create_dir_all(destination.parent().unwrap()).unwrap();
            fs::write(destination, bytes).unwrap();
        }
        root
    }

    fn extension_request(commit: &str, root: &Path, paths: &[&str]) -> RevisionExtensionRequest {
        RevisionExtensionRequest {
            repo_id: "org/model".into(),
            commit: commit.into(),
            source_root: root.to_path_buf(),
            files: paths
                .iter()
                .map(|path| SourceFile {
                    path: (*path).into(),
                    source: root.join(path),
                })
                .collect(),
        }
    }

    fn manifest_files(archive: &Archive, commit: &str) -> serde_json::Value {
        let manifest: serde_json::Value =
            serde_json::from_str(&archive.manifest("org/model", commit).unwrap()).unwrap();
        manifest["files"].clone()
    }

    fn manifest_paths(archive: &Archive, commit: &str) -> BTreeSet<String> {
        manifest_files(archive, commit)
            .as_array()
            .unwrap()
            .iter()
            .map(|file| file["path"].as_str().unwrap().to_string())
            .collect()
    }

    fn manifest_entry(archive: &Archive, commit: &str, path: &str) -> serde_json::Value {
        manifest_files(archive, commit)
            .as_array()
            .unwrap()
            .iter()
            .find(|file| file["path"] == path)
            .cloned()
            .unwrap()
    }

    #[test]
    fn extension_publishes_a_manifest_superset() {
        let (archive, directory) = archive();
        let commit = "a".repeat(40);
        published_revision_for_extension(&archive, directory.path(), &commit);
        let before = manifest_paths(&archive, &commit);

        let source = extension_source(
            directory.path(),
            "extend-superset",
            &[
                ("nested/second.bin", b"second".as_slice()),
                ("extra.txt", b"extra".as_slice()),
            ],
        );
        let outcome = archive
            .extend_revision_from_directory(extension_request(
                &commit,
                &source,
                &["nested/second.bin", "extra.txt"],
            ))
            .unwrap();

        assert_eq!(outcome.added, vec!["extra.txt", "nested/second.bin"]);
        assert!(outcome.skipped.is_empty());
        let after = manifest_paths(&archive, &commit);
        assert!(before.is_subset(&after));
        assert_eq!(after.len(), before.len() + 2);
        assert_eq!(
            manifest_entry(&archive, &commit, "extra.txt")["size"]
                .as_u64()
                .unwrap(),
            5
        );
        // The manifest and the revision directory still agree exactly, so the
        // extension left no internal file inside the published revision.
        assert_eq!(archive.verify_revision("org/model", &commit).unwrap(), 4);
        assert_eq!(archive.list_revisions("org/model").unwrap(), vec![commit]);
    }

    #[test]
    fn extension_resolves_added_paths_for_serving() {
        let (archive, directory) = archive();
        let commit = "7".repeat(40);
        published_revision_for_extension(&archive, directory.path(), &commit);
        let source = extension_source(
            directory.path(),
            "extend-serving",
            &[("quant/model.gguf", b"quantised".as_slice())],
        );
        assert!(matches!(
            archive.resolve_file("org/model", &commit, "quant/model.gguf"),
            Err(ArchiveError::Io(error)) if error.kind() == io::ErrorKind::NotFound
        ));

        archive
            .extend_revision_from_directory(extension_request(
                &commit,
                &source,
                &["quant/model.gguf"],
            ))
            .unwrap();

        let resolved = archive
            .resolve_file("org/model", &commit, "quant/model.gguf")
            .unwrap();
        assert_eq!(resolved.size, 9);
        assert_eq!(fs::read(resolved.path).unwrap(), b"quantised");
        let inventory = archive.repository_inventory("org/model").unwrap();
        assert_eq!(inventory.revisions[0].file_count, 3);
    }

    #[test]
    fn extension_does_not_rewrite_published_files() {
        let (archive, directory) = archive();
        let commit = "1".repeat(40);
        let revision = published_revision_for_extension(&archive, directory.path(), &commit);
        let published_bytes = [
            fs::read(revision.join("config.json")).unwrap(),
            fs::read(revision.join("nested/first.bin")).unwrap(),
        ];
        let published_entries = [
            manifest_entry(&archive, &commit, "config.json"),
            manifest_entry(&archive, &commit, "nested/first.bin"),
        ];

        let source = extension_source(
            directory.path(),
            "extend-untouched",
            &[("added.bin", b"added".as_slice())],
        );
        archive
            .extend_revision_from_directory(extension_request(&commit, &source, &["added.bin"]))
            .unwrap();

        assert_eq!(
            [
                fs::read(revision.join("config.json")).unwrap(),
                fs::read(revision.join("nested/first.bin")).unwrap(),
            ],
            published_bytes
        );
        assert_eq!(
            [
                manifest_entry(&archive, &commit, "config.json"),
                manifest_entry(&archive, &commit, "nested/first.bin"),
            ],
            published_entries
        );
    }

    #[test]
    fn extension_skips_paths_the_manifest_already_lists() {
        let (archive, directory) = archive();
        let commit = "2".repeat(40);
        let revision = published_revision_for_extension(&archive, directory.path(), &commit);
        let source = extension_source(
            directory.path(),
            "extend-collision",
            &[
                ("config.json", b"REPLACED".as_slice()),
                ("fresh.bin", b"fresh".as_slice()),
            ],
        );

        let outcome = archive
            .extend_revision_from_directory(extension_request(
                &commit,
                &source,
                &["config.json", "fresh.bin"],
            ))
            .unwrap();

        assert_eq!(outcome.skipped, vec!["config.json"]);
        assert_eq!(outcome.added, vec!["fresh.bin"]);
        assert_eq!(
            fs::read(revision.join("config.json")).unwrap(),
            b"{\"base\":true}"
        );
        assert_eq!(manifest_paths(&archive, &commit).len(), 3);
        assert_eq!(archive.verify_revision("org/model", &commit).unwrap(), 3);
    }

    #[test]
    fn interrupted_extension_keeps_the_old_manifest_live() {
        let (archive, directory) = archive();
        let commit = "d".repeat(40);
        let revision = published_revision_for_extension(&archive, directory.path(), &commit);
        let source = extension_source(
            directory.path(),
            "extend-interrupted",
            &[("late.bin", b"late".as_slice())],
        );
        let request = extension_request(&commit, &source, &["late.bin"]);

        let error = archive
            .extend_revision_from_directory_inner(RepositoryType::Model, &request, &|| {
                Err(ArchiveError::Io(io::Error::other(
                    "interrupted before manifest swap",
                )))
            })
            .unwrap_err();

        assert!(matches!(error, ArchiveError::Io(_)));
        // The new file is already durable, and the live manifest is still the old one.
        assert_eq!(fs::read(revision.join("late.bin")).unwrap(), b"late");
        assert!(!manifest_paths(&archive, &commit).contains("late.bin"));
        assert!(matches!(
            archive.resolve_file("org/model", &commit, "late.bin"),
            Err(ArchiveError::Io(error)) if error.kind() == io::ErrorKind::NotFound
        ));

        let outcome = archive.extend_revision_from_directory(request).unwrap();
        assert_eq!(outcome.added, vec!["late.bin"]);
        assert_eq!(
            fs::read(
                archive
                    .resolve_file("org/model", &commit, "late.bin")
                    .unwrap()
                    .path
            )
            .unwrap(),
            b"late"
        );
        assert_eq!(archive.verify_revision("org/model", &commit).unwrap(), 3);
    }

    #[test]
    fn concurrent_extensions_keep_every_entry() {
        let (archive, directory) = archive();
        let commit = "c".repeat(40);
        published_revision_for_extension(&archive, directory.path(), &commit);
        let sources = [
            extension_source(
                directory.path(),
                "extend-parallel-a",
                &[("parallel/a.bin", b"aaa".as_slice())],
            ),
            extension_source(
                directory.path(),
                "extend-parallel-b",
                &[("parallel/b.bin", b"bbbb".as_slice())],
            ),
        ];
        let barrier = Arc::new(std::sync::Barrier::new(sources.len()));

        let threads = sources
            .iter()
            .zip(["parallel/a.bin", "parallel/b.bin"])
            .map(|(root, path)| {
                let archive = archive.clone();
                let commit = commit.clone();
                let root = root.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    archive.extend_revision_from_directory(extension_request(
                        &commit,
                        &root,
                        &[path],
                    ))
                })
            })
            .collect::<Vec<_>>();
        for thread in threads {
            let outcome = thread.join().unwrap().unwrap();
            assert_eq!(outcome.added.len(), 1);
        }

        let paths = manifest_paths(&archive, &commit);
        assert!(paths.contains("parallel/a.bin"), "lost entry: {paths:?}");
        assert!(paths.contains("parallel/b.bin"), "lost entry: {paths:?}");
        assert_eq!(paths.len(), 4);
        assert_eq!(archive.verify_revision("org/model", &commit).unwrap(), 4);
    }

    #[test]
    fn extension_rejects_paths_and_sources_outside_the_archive() {
        let (archive, directory) = archive();
        let commit = "e".repeat(40);
        let revision = published_revision_for_extension(&archive, directory.path(), &commit);
        let source = extension_source(
            directory.path(),
            "extend-unsafe",
            &[("escape.bin", b"escape".as_slice())],
        );

        for path in [
            "../escape.bin",
            "nested/../../escape.bin",
            "/etc/passwd",
            ".modelkeep-manifest.json",
            ".cache/huggingface/download.json",
        ] {
            let mut request = extension_request(&commit, &source, &["escape.bin"]);
            request.files[0].path = path.into();
            assert!(
                matches!(
                    archive.extend_revision_from_directory(request),
                    Err(ArchiveError::InvalidPath(_))
                ),
                "unsafe path must be rejected: {path}"
            );
        }

        let outside = directory.path().join("outside.bin");
        fs::write(&outside, b"outside").unwrap();
        let mut request = extension_request(&commit, &source, &["escape.bin"]);
        request.files[0].source = outside;
        assert!(matches!(
            archive.extend_revision_from_directory(request),
            Err(ArchiveError::InvalidPath(_))
        ));

        assert_eq!(manifest_paths(&archive, &commit).len(), 2);
        assert!(!revision.join("escape.bin").exists());
        assert!(!directory.path().join("escape.bin").exists());
        assert_eq!(archive.verify_revision("org/model", &commit).unwrap(), 2);
    }

    #[test]
    fn extending_an_absent_revision_is_not_a_cache_miss() {
        let (archive, directory) = archive();
        let commit = "f".repeat(40);
        published_revision_for_extension(&archive, directory.path(), &commit);
        let source = extension_source(
            directory.path(),
            "extend-absent",
            &[("late.bin", b"late".as_slice())],
        );

        let absent = "b".repeat(40);
        assert!(matches!(
            archive.extend_revision_from_directory(extension_request(&absent, &source, &["late.bin"])),
            Err(ArchiveError::Io(error)) if error.kind() == io::ErrorKind::NotFound
        ));
        assert!(matches!(
            archive.extend_revision_from_directory(extension_request(
                "../escape",
                &source,
                &["late.bin"]
            )),
            Err(ArchiveError::InvalidPath(_))
        ));

        let mut request = extension_request(&commit, &source, &["late.bin"]);
        request.repo_id = "org/absent".into();
        assert!(matches!(
            archive.extend_revision_from_directory(request),
            Err(ArchiveError::Io(error)) if error.kind() == io::ErrorKind::NotFound
        ));

        // A revision whose manifest cannot be parsed is an integrity failure,
        // not an absent path.
        let revision = archive.revision_path("org/model", &commit).unwrap();
        fs::write(revision.join(".modelkeep-manifest.json"), b"{not json").unwrap();
        assert!(matches!(
            archive.extend_revision_from_directory(extension_request(
                &commit,
                &source,
                &["late.bin"]
            )),
            Err(ArchiveError::IntegrityMismatch(_))
        ));
    }
}

#[cfg(test)]
mod self_check_tests {
    use super::*;

    const COMMIT: &str = "1111111111111111111111111111111111111111";

    fn healthy_archive() -> (Archive, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        archive
            .publish_revision(PublishRequest {
                repo_id: "org/model".into(),
                requested_revision: "main".into(),
                commit: COMMIT.into(),
                files: vec![
                    ArchiveFile {
                        path: "config.json".into(),
                        bytes: b"{}".to_vec(),
                    },
                    ArchiveFile {
                        path: "weights/model.bin".into(),
                        bytes: b"payload".to_vec(),
                    },
                ],
            })
            .unwrap();
        archive.update_ref("org/model", "main", COMMIT).unwrap();
        (archive, directory)
    }

    fn kinds(report: &SelfCheckReport) -> Vec<SelfCheckFindingKind> {
        report
            .findings
            .iter()
            .map(|finding| finding.finding)
            .collect()
    }

    /// Content-addressed snapshot of the whole archive tree, used to prove the
    /// check repairs nothing and publishes nothing.
    fn tree_digest(root: &Path) -> String {
        fn walk(root: &Path, directory: &Path, entries: &mut BTreeMap<String, String>) {
            let mut listing = fs::read_dir(directory)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect::<Vec<_>>();
            listing.sort();
            for path in listing {
                let relative = path.strip_prefix(root).unwrap().display().to_string();
                let metadata = fs::symlink_metadata(&path).unwrap();
                if metadata.is_symlink() {
                    let target = fs::read_link(&path).unwrap();
                    entries.insert(relative, format!("link:{}", target.display()));
                } else if metadata.is_dir() {
                    entries.insert(relative, "dir".into());
                    walk(root, &path, entries);
                } else {
                    entries.insert(relative, sha256(&fs::read(&path).unwrap()));
                }
            }
        }
        let mut entries = BTreeMap::new();
        walk(root, root, &mut entries);
        let rendered = entries
            .iter()
            .map(|(path, digest)| format!("{path}\u{0}{digest}"))
            .collect::<Vec<_>>()
            .join("\n");
        sha256(rendered.as_bytes())
    }

    fn manifest_path(archive: &Archive) -> PathBuf {
        archive
            .revision_path("org/model", COMMIT)
            .unwrap()
            .join(".modelkeep-manifest.json")
    }

    fn rewrite_manifest(archive: &Archive, from: &str, to: &str) {
        let path = manifest_path(archive);
        let contents = fs::read_to_string(&path).unwrap().replace(from, to);
        fs::write(path, contents).unwrap();
    }

    fn leave_staging_behind(root: &Path, name: &str) {
        let staging = root.join("tmp").join(name);
        fs::create_dir_all(&staging).unwrap();
        fs::write(
            staging.join(FETCH_STAGING_FILE),
            serde_json::to_vec(&FetchStagingMetadata {
                version: 1,
                repo_type: RepositoryType::Model,
                repo_id: "org/model".into(),
                requested_revision: "main".into(),
                files: Vec::new(),
                resolved_commit: Some(COMMIT.into()),
            })
            .unwrap(),
        )
        .unwrap();
        write_staging_lease_with_expiry(&staging, "abandoned", 0).unwrap();
    }

    #[test]
    fn healthy_archive_reports_a_zero_finding_result() {
        let (archive, _directory) = healthy_archive();
        assert_eq!(archive.self_check_state(), SelfCheckState::NeverRun);

        let report = archive.self_check();

        assert_eq!(report.findings, Vec::new());
        assert_eq!(report.status(), "clean");
        assert_eq!(report.repositories_checked, 1);
        assert_eq!(report.revisions_checked, 1);
        assert_eq!(report.files_checked, 2);
        assert_eq!(report.refs_checked, 1);
        assert_eq!(report.orphaned_staging_directories, 0);
        assert!(report.completed_at > 0);
        // "Checked and clean" has to be distinguishable from "never checked".
        assert_eq!(
            archive.self_check_state(),
            SelfCheckState::Completed(Box::new(report))
        );
    }

    #[test]
    fn detects_a_manifest_path_absent_from_the_revision() {
        let (archive, _directory) = healthy_archive();
        fs::remove_file(
            archive
                .revision_path("org/model", COMMIT)
                .unwrap()
                .join("weights/model.bin"),
        )
        .unwrap();

        let report = archive.self_check();

        assert_eq!(kinds(&report), vec![SelfCheckFindingKind::MissingFile]);
        assert_eq!(
            report.findings[0].path.as_deref(),
            Some("weights/model.bin")
        );
        assert_eq!(report.findings[0].repo_id.as_deref(), Some("org/model"));
        assert_eq!(report.findings[0].commit.as_deref(), Some(COMMIT));
    }

    #[test]
    fn detects_a_size_that_no_longer_matches_the_manifest() {
        let (archive, _directory) = healthy_archive();
        fs::write(
            archive
                .revision_path("org/model", COMMIT)
                .unwrap()
                .join("config.json"),
            b"{\"truncated\":true}",
        )
        .unwrap();

        let report = archive.self_check();

        assert_eq!(kinds(&report), vec![SelfCheckFindingKind::SizeMismatch]);
        assert_eq!(report.findings[0].path.as_deref(), Some("config.json"));
    }

    #[test]
    fn detects_a_ref_without_a_servable_revision() {
        let (archive, directory) = healthy_archive();
        let dangling = "2222222222222222222222222222222222222222";
        fs::write(
            directory.path().join("models/org/model/refs/main"),
            dangling,
        )
        .unwrap();

        let report = archive.self_check();

        assert_eq!(kinds(&report), vec![SelfCheckFindingKind::DanglingRef]);
        assert_eq!(report.findings[0].reference.as_deref(), Some("main"));
        assert_eq!(report.findings[0].commit.as_deref(), Some(dangling));
    }

    #[test]
    fn detects_a_manifest_path_that_leaves_the_revision() {
        let (archive, _directory) = healthy_archive();
        rewrite_manifest(
            &archive,
            "\"path\":\"config.json\"",
            "\"path\":\"../escape.json\"",
        );

        let report = archive.self_check();

        assert_eq!(kinds(&report), vec![SelfCheckFindingKind::UnsafePath]);
        // The rejected path is identified by manifest position and never
        // echoed back, so a report can carry no path ModelKeep refuses to use.
        assert_eq!(report.findings[0].path, None);
        assert!(!format!("{:?}", report.findings[0]).contains("escape.json"));
        assert!(report.findings[0].detail.contains("manifest entry 0"));
    }

    #[test]
    fn an_internal_archive_path_in_a_manifest_is_counted_and_never_repeated() {
        let (archive, _directory) = healthy_archive();
        rewrite_manifest(
            &archive,
            "\"path\":\"config.json\"",
            "\"path\":\".cache/huggingface/download.json\"",
        );

        let report = archive.self_check();

        // Serving filters an internal archive path, so a manifest that lists
        // one is a counted observation and not a finding, and the path itself
        // never reaches a report or a log line.
        assert_eq!(report.findings, Vec::new());
        assert_eq!(report.status(), "clean");
        assert_eq!(report.filtered_internal_paths, 1);
        assert!(!format!("{report:?}").contains(".cache/huggingface"));
    }

    #[test]
    fn detects_a_symbolic_link_that_resolves_outside_the_revision() {
        let (archive, directory) = healthy_archive();
        let outside = directory.path().join("outside.json");
        fs::write(&outside, b"{}").unwrap();
        let archived = archive
            .revision_path("org/model", COMMIT)
            .unwrap()
            .join("config.json");
        fs::remove_file(&archived).unwrap();
        std::os::unix::fs::symlink(&outside, &archived).unwrap();

        let report = archive.self_check();

        assert_eq!(kinds(&report), vec![SelfCheckFindingKind::UnsafePath]);
        assert_eq!(report.findings[0].path.as_deref(), Some("config.json"));
    }

    #[test]
    fn detects_a_manifest_that_does_not_parse_or_match_its_location() {
        let (archive, _directory) = healthy_archive();
        rewrite_manifest(
            &archive,
            "\"repo_type\":\"model\"",
            "\"repo_type\":\"dataset\"",
        );
        assert_eq!(
            kinds(&archive.self_check()),
            vec![
                SelfCheckFindingKind::InvalidManifest,
                // The revision stops being servable, so its ref dangles too.
                SelfCheckFindingKind::DanglingRef,
            ]
        );

        fs::write(manifest_path(&archive), b"{not json").unwrap();
        assert_eq!(
            kinds(&archive.self_check()),
            vec![
                SelfCheckFindingKind::InvalidManifest,
                SelfCheckFindingKind::DanglingRef,
            ]
        );
    }

    #[test]
    fn detects_fetch_staging_left_behind_with_its_age() {
        let (archive, directory) = healthy_archive();
        leave_staging_behind(directory.path(), "fetch-abandoned-left-over");

        let report = archive.self_check();

        assert_eq!(kinds(&report), vec![SelfCheckFindingKind::OrphanedStaging]);
        assert_eq!(report.staging_directories, 1);
        assert_eq!(report.orphaned_staging_directories, 1);
        assert_eq!(report.findings[0].repo_id.as_deref(), Some("org/model"));
        assert!(report.findings[0].age_seconds.is_some());
    }

    #[test]
    fn live_staging_is_counted_but_is_not_a_finding() {
        let (archive, directory) = healthy_archive();
        let staging = directory.path().join("tmp/.fetch-active-live");
        fs::create_dir_all(&staging).unwrap();
        write_staging_lease_with_expiry(&staging, "live", unix_timestamp() + 600).unwrap();

        let report = archive.self_check();

        assert_eq!(report.findings, Vec::new());
        assert_eq!(report.staging_directories, 1);
        assert_eq!(report.orphaned_staging_directories, 0);
    }

    #[test]
    fn check_repairs_nothing_and_leaves_the_archive_untouched() {
        let (archive, directory) = healthy_archive();
        fs::remove_file(
            archive
                .revision_path("org/model", COMMIT)
                .unwrap()
                .join("config.json"),
        )
        .unwrap();
        leave_staging_behind(directory.path(), "fetch-abandoned-kept");
        let before = tree_digest(directory.path());

        let report = archive.self_check();
        assert_eq!(
            kinds(&report),
            vec![
                SelfCheckFindingKind::MissingFile,
                SelfCheckFindingKind::OrphanedStaging,
            ]
        );

        // Nothing was repaired, deleted, or re-acquired: the damaged archive is
        // byte-for-byte what it was, staging included, and a second run says
        // exactly the same thing.
        assert_eq!(tree_digest(directory.path()), before);
        assert_eq!(kinds(&archive.self_check()), kinds(&report));
        assert_eq!(tree_digest(directory.path()), before);
    }

    #[test]
    fn reports_a_measured_duration_against_the_revision_count() {
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        for index in 0..32u32 {
            let commit = format!("{index:040x}");
            archive
                .publish_revision(PublishRequest {
                    repo_id: "org/many".into(),
                    requested_revision: "main".into(),
                    commit: commit.clone(),
                    files: vec![ArchiveFile {
                        path: "config.json".into(),
                        bytes: commit.clone().into_bytes(),
                    }],
                })
                .unwrap();
        }

        let report = archive.self_check();

        assert_eq!(report.findings, Vec::new());
        assert_eq!(report.revisions_checked, 32);
        assert_eq!(report.files_checked, 32);
        // The duration is reported beside the revision count it was measured
        // over, so startup cost stays observable as the archive grows.
        assert!(report.duration_ms < 60_000);
    }
}
