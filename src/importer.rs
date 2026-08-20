use std::fs;
use std::path::Path;

use crate::{Archive, ArchiveError, ArchiveResult, SourceFile, SourcePublishRequest};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportReport {
    pub revisions: usize,
    pub refs: usize,
}

pub fn import_hf_cache(archive: &Archive, cache_root: &Path) -> ArchiveResult<ImportReport> {
    let cache_root = fs::canonicalize(cache_root)?;
    let mut report = ImportReport {
        revisions: 0,
        refs: 0,
    };
    for entry in fs::read_dir(&cache_root)? {
        let entry = entry?;
        let repository = entry.path();
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(repo_id) = decode_repository_name(entry.file_name().to_string_lossy().as_ref())
        else {
            continue;
        };
        let snapshots = repository.join("snapshots");
        if !snapshots.is_dir() {
            continue;
        }
        for snapshot in fs::read_dir(&snapshots)? {
            let snapshot = snapshot?;
            if !snapshot.file_type()?.is_dir() {
                continue;
            }
            let commit = snapshot.file_name().to_string_lossy().into_owned();
            let files = collect_files(&repository, &snapshot.path(), &snapshot.path())?;
            if files.is_empty() {
                continue;
            }
            let request = SourcePublishRequest {
                repo_id: repo_id.clone(),
                requested_revision: commit.clone(),
                commit: commit.clone(),
                source_root: repository.clone(),
                files,
            };
            match archive.publish_revision_from_directory(request) {
                Ok(_) => report.revisions += 1,
                Err(ArchiveError::AlreadyPublished(_)) => {}
                Err(error) => return Err(error),
            }
        }
        let refs = repository.join("refs");
        if refs.is_dir() {
            for reference in fs::read_dir(refs)? {
                let reference = reference?;
                if !reference.file_type()?.is_file() {
                    continue;
                }
                let name = reference.file_name().to_string_lossy().into_owned();
                let commit = fs::read_to_string(reference.path())?.trim().to_string();
                if commit.is_empty() {
                    continue;
                }
                archive.update_ref(&repo_id, &name, &commit)?;
                report.refs += 1;
            }
        }
    }
    Ok(report)
}

fn decode_repository_name(name: &str) -> Option<String> {
    let encoded = name.strip_prefix("models--")?;
    let mut parts = encoded.split("--");
    let namespace = parts.next()?;
    let repository = parts.next()?;
    if parts.next().is_some() || namespace.is_empty() || repository.is_empty() {
        return None;
    }
    Some(format!("{namespace}/{repository}"))
}

fn collect_files(
    source_root: &Path,
    directory: &Path,
    snapshot: &Path,
) -> ArchiveResult<Vec<SourceFile>> {
    let mut files = Vec::new();
    collect_files_recursive(source_root, directory, snapshot, &mut files)?;
    Ok(files)
}

fn collect_files_recursive(
    source_root: &Path,
    current: &Path,
    snapshot: &Path,
    files: &mut Vec<SourceFile>,
) -> ArchiveResult<()> {
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_files_recursive(source_root, &path, snapshot, files)?;
        } else if file_type.is_file() || file_type.is_symlink() {
            let relative = path
                .strip_prefix(snapshot)
                .map_err(|_| ArchiveError::InvalidPath(path.display().to_string()))?
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/");
            files.push(SourceFile {
                path: relative,
                source: path,
            });
        }
    }
    let _ = source_root;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn imports_snapshot_and_ref() {
        let cache = tempfile::tempdir().unwrap();
        let repository = cache.path().join("models--org--model");
        let snapshot = repository.join("snapshots/aaaaaaaa");
        let blob = repository.join("blobs/blob1");
        fs::create_dir_all(&snapshot).unwrap();
        fs::create_dir_all(blob.parent().unwrap()).unwrap();
        fs::write(&blob, b"from cache").unwrap();
        symlink(&blob, snapshot.join("config.json")).unwrap();
        fs::create_dir_all(repository.join("refs")).unwrap();
        fs::write(
            repository.join("refs/main"),
            "aaaaaaaa
",
        )
        .unwrap();

        let archive_dir = tempfile::tempdir().unwrap();
        let archive = Archive::new(archive_dir.path()).unwrap();
        let report = import_hf_cache(&archive, cache.path()).unwrap();
        assert_eq!(report.revisions, 1);
        assert_eq!(report.refs, 1);
        let path = archive.revision_path("org/model", "aaaaaaaa").unwrap();
        assert_eq!(fs::read(path.join("config.json")).unwrap(), b"from cache");
        assert_eq!(
            fs::read_to_string(path.parent().unwrap().parent().unwrap().join("refs/main")).unwrap(),
            "aaaaaaaa"
        );
    }
}
