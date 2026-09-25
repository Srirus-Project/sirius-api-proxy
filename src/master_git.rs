//! Managed Git publication over verified immutable Master snapshots.
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
    #[error("invalid Master Git commit policy")]
    CommitConfig,
    #[error("invalid Master Git remote configuration")]
    RemoteConfig,
    #[error("Master Git remote history is incompatible or could not be verified")]
    RemoteChanged,
    #[error("Master Git repository is already owned")]
    Locked,
    #[error("Master Git snapshot verification or storage failed")]
    Snapshot,
    #[error("Master Git publication failed")]
    Git,
}
#[derive(Clone, Serialize)]
pub struct Receipt {
    pub commit: String,
    pub content_sha256: String,
    pub changed: bool,
    pub remote_verified: bool,
}
#[derive(Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub name: String,
    pub email: String,
}
impl Default for Identity {
    fn default() -> Self {
        Self {
            name: "Sirius Master Publisher".into(),
            email: "sirius-master@localhost".into(),
        }
    }
}
#[derive(Clone, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SigningFormat {
    #[serde(alias = "gpg")]
    Openpgp,
    Ssh,
}
#[derive(Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Signing {
    pub format: SigningFormat,
    /// OpenPGP fingerprint or absolute SSH key path, never private key material.
    pub key: String,
    pub program: Option<PathBuf>,
}
#[derive(Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitPolicy {
    #[serde(default)]
    pub author: Identity,
    pub committer: Option<Identity>,
    pub signing: Option<Signing>,
}
impl CommitPolicy {
    pub fn validate(&self) -> Result<(), Error> {
        for identity in [
            &self.author,
            self.committer.as_ref().unwrap_or(&self.author),
        ] {
            if identity.name.trim().is_empty()
                || identity.name.len() > 256
                || identity.email.is_empty()
                || identity.email.len() > 320
                || !identity.email.contains('@')
                || identity.email.chars().any(char::is_whitespace)
                || [&identity.name, &identity.email]
                    .iter()
                    .any(|v| v.chars().any(|c| c.is_control() || matches!(c, '<' | '>')))
            {
                return Err(Error::CommitConfig);
            }
        }
        if let Some(signing) = &self.signing {
            if signing.key.is_empty()
                || signing.key.len() > 1024
                || signing.key.chars().any(char::is_control)
            {
                return Err(Error::CommitConfig);
            }
            match signing.format {
                SigningFormat::Openpgp
                    if !(16..=64).contains(&signing.key.len())
                        || !signing.key.bytes().all(|b| b.is_ascii_hexdigit()) =>
                {
                    return Err(Error::CommitConfig)
                }
                SigningFormat::Ssh if !Path::new(&signing.key).is_absolute() => {
                    return Err(Error::CommitConfig)
                }
                _ => {}
            }
            if let Some(program) = &signing.program {
                // Git may shell-interpret a custom signing program; permit one executable path only.
                if !program.is_absolute()
                    || program.to_str().is_none_or(|p| {
                        p.len() > 1024
                            || !p
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b))
                    })
                {
                    return Err(Error::CommitConfig);
                }
            }
        }
        Ok(())
    }
    fn arguments(&self) -> Vec<OsString> {
        let committer = self.committer.as_ref().unwrap_or(&self.author);
        let mut values = vec![
            format!("author.name={}", self.author.name),
            format!("author.email={}", self.author.email),
            format!("committer.name={}", committer.name),
            format!("committer.email={}", committer.email),
        ];
        if let Some(signing) = &self.signing {
            let (format, program_key) = match signing.format {
                SigningFormat::Openpgp => ("openpgp", "gpg.openpgp.program"),
                SigningFormat::Ssh => ("ssh", "gpg.ssh.program"),
            };
            values.push(format!("gpg.format={format}"));
            values.push(format!("user.signingkey={}", signing.key));
            if let Some(program) = &signing.program {
                values.push(format!("{program_key}={}", program.display()));
            }
        }
        values
            .into_iter()
            .flat_map(|v| [OsString::from("-c"), v.into()])
            .collect()
    }
}
struct Prepared {
    policy: CommitPolicy,
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
        policy: CommitPolicy::default(),
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
    all.extend(prepared.policy.arguments());
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
    commit_with_policy(source, destination, scope, &CommitPolicy::default()).await
}
pub async fn publish(
    source: &Path,
    destination: &Path,
    scope: Scope,
    remote: &Remote,
) -> Result<Receipt, Error> {
    publish_with_policy(source, destination, scope, remote, &CommitPolicy::default()).await
}
pub async fn commit_with_policy(
    source: &Path,
    destination: &Path,
    scope: Scope,
    policy: &CommitPolicy,
) -> Result<Receipt, Error> {
    commit_internal(source, destination, scope, None, policy).await
}
pub async fn publish_with_policy(
    source: &Path,
    destination: &Path,
    scope: Scope,
    remote: &Remote,
    policy: &CommitPolicy,
) -> Result<Receipt, Error> {
    remote.validate()?;
    commit_internal(source, destination, scope, Some(remote), policy).await
}
async fn commit_internal(
    source: &Path,
    destination: &Path,
    scope: Scope,
    remote: Option<&Remote>,
    policy: &CommitPolicy,
) -> Result<Receipt, Error> {
    policy.validate()?;
    if !cfg!(unix) {
        return Err(Error::Git);
    }
    let source = source.to_owned();
    let destination = destination.to_owned();
    let mut prepared = tokio::task::spawn_blocking(move || prepare(&source, &destination, scope))
        .await
        .map_err(|_| Error::Snapshot)??;
    prepared.policy = policy.clone();
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
    if let Some(remote) = remote {
        check_remote(&prepared, remote, parent.as_deref(), deadline).await?;
    }
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
            return finish(
                &prepared,
                remote,
                Receipt {
                    commit: parent.clone(),
                    content_sha256: prepared.manifest.content_sha256.clone(),
                    changed: false,
                    remote_verified: false,
                },
                deadline,
            )
            .await;
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
    if policy.signing.is_some() {
        args.push("-S".into());
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
    finish(
        &prepared,
        remote,
        Receipt {
            commit,
            content_sha256: prepared.manifest.content_sha256.clone(),
            changed: true,
            remote_verified: false,
        },
        deadline,
    )
    .await
}

#[derive(Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Remote {
    pub url: String,
    pub authorization_env: Option<String>,
    #[serde(default)]
    pub allow_http: bool,
    #[serde(default)]
    pub allow_file: bool,
}
impl Remote {
    pub fn validate(&self) -> Result<(), Error> {
        let url = url::Url::parse(&self.url).map_err(|_| Error::RemoteConfig)?;
        if self.url.len() > 2048
            || self.url.bytes().any(|b| b.is_ascii_whitespace())
            || self.url.contains('\\')
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !(url.scheme() == "https"
                || (self.allow_http && url.scheme() == "http")
                || (self.allow_file && url.scheme() == "file"))
            || (url.scheme() != "file" && url.host_str().is_none())
            || (url.scheme() == "file"
                && (url.to_file_path().is_err() || self.authorization_env.is_some()))
        {
            return Err(Error::RemoteConfig);
        }
        if let Some(name) = &self.authorization_env {
            if name.is_empty()
                || name.len() > 256
                || name.starts_with("GIT_")
                || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            {
                return Err(Error::RemoteConfig);
            }
            let value = crate::config::secret(name).map_err(|_| Error::RemoteConfig)?;
            if value.len() > 8192
                || !value.bytes().all(|b| (32..=126).contains(&b))
                || value
                    .strip_prefix("Authorization: Basic ")
                    .or_else(|| value.strip_prefix("Authorization: Bearer "))
                    .is_none_or(|token| {
                        token.is_empty() || token.bytes().any(|b| b.is_ascii_whitespace())
                    })
            {
                return Err(Error::RemoteConfig);
            }
        }
        Ok(())
    }
    fn options(&self) -> Vec<OsString> {
        let mut args = Vec::new();
        for option in [
            "protocol.allow=never",
            "protocol.https.allow=always",
            "http.followRedirects=false",
            "http.sslVerify=true",
            "http.proxy=",
            "credential.helper=",
            "http.extraHeader=",
            "core.hooksPath=/dev/null",
        ] {
            args.extend(["-c".into(), option.into()]);
        }
        if self.allow_http {
            args.extend(["-c".into(), "protocol.http.allow=always".into()]);
        }
        if self.allow_file {
            args.extend(["-c".into(), "protocol.file.allow=always".into()]);
        }
        if let Some(name) = &self.authorization_env {
            args.push(format!("--config-env=http.{}.extraHeader={name}", self.url).into());
        }
        args
    }
}
async fn network(
    prepared: &Prepared,
    remote: &Remote,
    args: Vec<OsString>,
    deadline: tokio::time::Instant,
) -> Result<Vec<u8>, Error> {
    let mut options = remote.options();
    options.extend(args);
    command(prepared, options, &[], deadline).await
}
async fn remote_head(
    prepared: &Prepared,
    remote: &Remote,
    deadline: tokio::time::Instant,
) -> Result<Option<String>, Error> {
    let bytes = network(
        prepared,
        remote,
        vec![
            "ls-remote".into(),
            "--refs".into(),
            remote.url.clone().into(),
            "refs/heads/master-data".into(),
        ],
        deadline,
    )
    .await?;
    let text = String::from_utf8(bytes).map_err(|_| Error::Git)?;
    if text.is_empty() {
        return Ok(None);
    }
    let lines: Vec<_> = text.lines().collect();
    if lines.len() != 1 {
        return Err(Error::Git);
    }
    let (hash, reference) = lines[0].split_once('\t').ok_or(Error::Git)?;
    if reference != "refs/heads/master-data" {
        return Err(Error::Git);
    }
    Ok(Some(oid(hash.as_bytes().to_vec())?))
}
async fn check_remote(
    prepared: &Prepared,
    remote: &Remote,
    parent: Option<&str>,
    deadline: tokio::time::Instant,
) -> Result<(), Error> {
    let Some(head) = remote_head(prepared, remote, deadline).await? else {
        return Ok(());
    };
    let parent = parent.ok_or(Error::RemoteChanged)?;
    if head == parent {
        return Ok(());
    }
    network(
        prepared,
        remote,
        vec![
            "fetch".into(),
            "--no-tags".into(),
            "--no-write-fetch-head".into(),
            remote.url.clone().into(),
            "+refs/heads/master-data:refs/sirius/remote-check".into(),
        ],
        deadline,
    )
    .await?;
    let fetched = oid(command(
        prepared,
        vec!["rev-parse".into(), "refs/sirius/remote-check".into()],
        &[],
        deadline,
    )
    .await?)?;
    if fetched != head {
        return Err(Error::RemoteChanged);
    }
    command(
        prepared,
        vec![
            "merge-base".into(),
            "--is-ancestor".into(),
            head.into(),
            parent.into(),
        ],
        &[],
        deadline,
    )
    .await
    .map_err(|_| Error::RemoteChanged)?;
    Ok(())
}
async fn finish(
    prepared: &Prepared,
    remote: Option<&Remote>,
    mut receipt: Receipt,
    deadline: tokio::time::Instant,
) -> Result<Receipt, Error> {
    if let Some(remote) = remote {
        network(
            prepared,
            remote,
            vec![
                "push".into(),
                "--porcelain".into(),
                remote.url.clone().into(),
                format!("{}:refs/heads/master-data", receipt.commit).into(),
            ],
            deadline,
        )
        .await?;
        if remote_head(prepared, remote, deadline).await?.as_deref()
            != Some(receipt.commit.as_str())
        {
            return Err(Error::RemoteChanged);
        }
        receipt.remote_verified = true;
    }
    Ok(receipt)
}
