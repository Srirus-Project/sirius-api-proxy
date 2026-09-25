//! Local Git publication over verified immutable Master snapshots. No network commands.
use crate::{
    git_process,
    master_registry::{self, PublishedManifest, Scope},
};
use serde::Serialize;
use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid or unowned Master Git repository")]
    Ownership,
    #[error("Master Git repository is already owned")]
    Locked,
    #[error("Master Git snapshot verification or storage failed")]
    Snapshot,
    #[error("Master Git publication failed")]
    Git,
}
#[derive(Serialize)]
pub struct Receipt {
    pub commit: String,
    pub content_sha256: String,
    pub changed: bool,
}
struct Prepared {
    _owner: fs::File,
    directory: PathBuf,
    staging: tempfile::TempDir,
    manifest: PublishedManifest,
    names: Vec<String>,
}
fn prepare(source: &Path, destination: &Path, scope: Scope) -> Result<Prepared, Error> {
    if let Ok(meta) = fs::symlink_metadata(destination) {
        if !meta.is_dir() || meta.file_type().is_symlink() {
            return Err(Error::Ownership);
        }
    }
    fs::create_dir_all(destination).map_err(|_| Error::Snapshot)?;
    let directory = fs::canonicalize(destination).map_err(|_| Error::Snapshot)?;
    let lock = directory.join("owner.lock");
    if fs::symlink_metadata(&lock).is_ok_and(|m| !m.is_file() || m.file_type().is_symlink()) {
        return Err(Error::Ownership);
    }
    let owner = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock)
        .map_err(|_| Error::Snapshot)?;
    owner.try_lock().map_err(|_| Error::Locked)?;
    let marker = directory.join("sirius-git.json");
    let expected = serde_json::json!({"schema_version":1,"scope":scope});
    if marker.exists() {
        let meta = fs::symlink_metadata(&marker).map_err(|_| Error::Ownership)?;
        if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > 4096 {
            return Err(Error::Ownership);
        }
        let existing: serde_json::Value =
            serde_json::from_slice(&fs::read(&marker).map_err(|_| Error::Ownership)?)
                .map_err(|_| Error::Ownership)?;
        if existing != expected {
            return Err(Error::Ownership);
        }
    } else {
        if fs::read_dir(&directory)
            .map_err(|_| Error::Ownership)?
            .any(|e| e.is_err() || e.unwrap().file_name() != "owner.lock")
        {
            return Err(Error::Ownership);
        }
        let mut file = tempfile::NamedTempFile::new_in(&directory).map_err(|_| Error::Snapshot)?;
        use std::io::Write;
        file.write_all(&serde_json::to_vec(&expected).map_err(|_| Error::Snapshot)?)
            .map_err(|_| Error::Snapshot)?;
        file.as_file().sync_all().map_err(|_| Error::Snapshot)?;
        file.persist(&marker).map_err(|_| Error::Snapshot)?;
    }
    let repository = directory.join("repository.git");
    if fs::symlink_metadata(&repository).is_ok_and(|m| !m.is_dir() || m.file_type().is_symlink()) {
        return Err(Error::Ownership);
    }
    let document = master_registry::manifest(source, None, scope).map_err(|_| Error::Snapshot)?;
    let manifest: PublishedManifest =
        serde_json::from_slice(&document.bytes).map_err(|_| Error::Snapshot)?;
    let staging = tempfile::tempdir().map_err(|_| Error::Snapshot)?;
    let mut names = Vec::new();
    for file in &manifest.files {
        if file.name == "sirius-publication.json" {
            return Err(Error::Snapshot);
        }
        let table = file.name.strip_suffix(".json").ok_or(Error::Snapshot)?;
        let document = master_registry::table(source, &manifest.snapshot, table, &file.sha256)
            .map_err(|_| Error::Snapshot)?;
        if document.bytes.len() as u64 != file.size {
            return Err(Error::Snapshot);
        }
        fs::write(staging.path().join(&file.name), document.bytes).map_err(|_| Error::Snapshot)?;
        names.push(file.name.clone());
    }
    // Node-local UUIDs are excluded, so an identical import produces the same Git tree.
    let mut metadata = serde_json::to_value(&manifest).map_err(|_| Error::Snapshot)?;
    metadata
        .as_object_mut()
        .ok_or(Error::Snapshot)?
        .remove("snapshot");
    fs::write(
        staging.path().join("sirius-publication.json"),
        serde_json::to_vec_pretty(&metadata).map_err(|_| Error::Snapshot)?,
    )
    .map_err(|_| Error::Snapshot)?;
    names.push("sirius-publication.json".into());
    names.sort();
    Ok(Prepared {
        _owner: owner,
        directory,
        staging,
        manifest,
        names,
    })
}
fn oid(bytes: Vec<u8>) -> Result<String, Error> {
    let value = String::from_utf8(bytes)
        .map_err(|_| Error::Git)?
        .trim()
        .to_owned();
    if value.len() != 40
        || !value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(Error::Git);
    }
    Ok(value)
}
async fn command(
    prepared: &Prepared,
    args: Vec<OsString>,
    input: &[u8],
    deadline: tokio::time::Instant,
) -> Result<Vec<u8>, Error> {
    let remaining = deadline
        .checked_duration_since(tokio::time::Instant::now())
        .ok_or(Error::Git)?;
    let mut all = vec![
        OsString::from("--no-replace-objects"),
        OsString::from("--git-dir"),
        prepared.directory.join("repository.git").into_os_string(),
        "-c".into(),
        "gc.auto=0".into(),
        "-c".into(),
        "commit.gpgsign=false".into(),
        "-c".into(),
        "user.name=Sirius Master Publisher".into(),
        "-c".into(),
        "user.email=sirius-master@localhost".into(),
    ];
    all.extend(args);
    git_process::run_with_input(
        Path::new("git"),
        &prepared.directory,
        &all,
        remaining,
        1024 * 1024,
        input,
    )
    .await
    .map_err(|_| Error::Git)
}
/// Publish a local bare Git commit only; a receipt is not a remote push acknowledgement.
pub async fn commit(source: &Path, destination: &Path, scope: Scope) -> Result<Receipt, Error> {
    if !cfg!(unix) {
        return Err(Error::Git);
    }
    let source = source.to_owned();
    let destination = destination.to_owned();
    let prepared = tokio::task::spawn_blocking(move || prepare(&source, &destination, scope))
        .await
        .map_err(|_| Error::Snapshot)??;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    command(
        &prepared,
        vec![
            "init".into(),
            "--bare".into(),
            "--quiet".into(),
            "--object-format=sha1".into(),
            prepared.directory.join("repository.git").into_os_string(),
        ],
        &[],
        deadline,
    )
    .await?;
    command(
        &prepared,
        vec![
            "symbolic-ref".into(),
            "HEAD".into(),
            "refs/heads/master-data".into(),
        ],
        &[],
        deadline,
    )
    .await?;
    let refs = command(
        &prepared,
        vec![
            "for-each-ref".into(),
            "--format=%(objectname)".into(),
            "refs/heads/master-data".into(),
        ],
        &[],
        deadline,
    )
    .await?;
    let parent = if refs.is_empty() {
        None
    } else {
        Some(oid(refs)?)
    };
    let mut tree_input = String::new();
    for names in prepared.names.chunks(64) {
        let mut args = vec![
            "hash-object".into(),
            "-w".into(),
            "--no-filters".into(),
            "--".into(),
        ];
        args.extend(
            names
                .iter()
                .map(|n| prepared.staging.path().join(n).into_os_string()),
        );
        let hashes = command(&prepared, args, &[], deadline).await?;
        let hashes = String::from_utf8(hashes).map_err(|_| Error::Git)?;
        if hashes.lines().count() != names.len() {
            return Err(Error::Git);
        }
        for (name, hash) in names.iter().zip(hashes.lines()) {
            let hash = oid(hash.as_bytes().to_vec())?;
            tree_input.push_str(&format!("100644 blob {hash}\t{name}\n"));
        }
    }
    let tree = oid(command(
        &prepared,
        vec!["mktree".into()],
        tree_input.as_bytes(),
        deadline,
    )
    .await?)?;
    if let Some(parent) = &parent {
        let old_tree = oid(command(
            &prepared,
            vec!["rev-parse".into(), format!("{parent}^{{tree}}").into()],
            &[],
            deadline,
        )
        .await?)?;
        if old_tree == tree {
            return Ok(Receipt {
                commit: parent.clone(),
                content_sha256: prepared.manifest.content_sha256,
                changed: false,
            });
        }
    }
    let mut args = vec![
        "commit-tree".into(),
        tree.into(),
        "-m".into(),
        format!(
            "Sirius Master {} {}",
            prepared.manifest.scope.region.name(),
            prepared.manifest.version
        )
        .into(),
    ];
    if let Some(parent) = &parent {
        args.extend(["-p".into(), parent.into()]);
    }
    let commit = oid(command(&prepared, args, &[], deadline).await?)?;
    command(
        &prepared,
        vec![
            "update-ref".into(),
            "refs/heads/master-data".into(),
            commit.clone().into(),
            parent.unwrap_or_else(|| "0".repeat(40)).into(),
        ],
        &[],
        deadline,
    )
    .await?;
    Ok(Receipt {
        commit,
        content_sha256: prepared.manifest.content_sha256,
        changed: true,
    })
}
