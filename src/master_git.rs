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
    #[error("invalid Master Git layout or branch")]
    LayoutConfig,
    #[error("Master snapshot has no recorded asset version")]
    AssetVersion,
    #[error("Master Git remote branch is absent or has no recognizable publication for this scope and layout")]
    NotAdoptable,
    #[error("invalid Master Git time budget")]
    TimeoutConfig,
}
/// Repository tree layout of a publication commit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layout {
    /// Exact plaintext table bytes plus `sirius-publication.json` (1.2.0 layout).
    #[default]
    Native,
    /// Every table re-indented (whitespace only) at the root plus `version.json`.
    IndentedRoot,
}
pub const DEFAULT_BRANCH: &str = "master-data";
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 120;
const TIMEOUT_SECONDS: std::ops::RangeInclusive<u64> = 10..=600;
/// Publication layout, target branch (`refs/heads/<branch>` locally and remotely) and time
/// budget.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    pub layout: Layout,
    pub branch: String,
    /// Budget in seconds for all Git commands of one publication or adoption attempt after
    /// local preparation (10..=600).
    pub timeout_seconds: u64,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            layout: Layout::Native,
            branch: DEFAULT_BRANCH.into(),
            timeout_seconds: DEFAULT_TIMEOUT_SECONDS,
        }
    }
}
impl Options {
    pub fn validate(&self) -> Result<(), Error> {
        if !valid_branch(&self.branch) {
            Err(Error::LayoutConfig)
        } else if !TIMEOUT_SECONDS.contains(&self.timeout_seconds) {
            Err(Error::TimeoutConfig)
        } else {
            Ok(())
        }
    }
    /// Apply an explicit override such as `SIRIUS_MASTER_GIT_TIMEOUT_SECONDS`: `None` keeps
    /// the current budget; otherwise 1-3 ASCII digits without sign, padding or unit, in
    /// 10..=600. A rejected value leaves the budget unchanged and is never echoed.
    pub fn apply_timeout_override(&mut self, value: Option<&std::ffi::OsStr>) -> Result<(), Error> {
        let Some(value) = value else {
            return Ok(());
        };
        let seconds = value
            .to_str()
            .filter(|v| {
                (1..=3).contains(&v.len())
                    && !v.starts_with('0')
                    && v.bytes().all(|b| b.is_ascii_digit())
            })
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|s| TIMEOUT_SECONDS.contains(s))
            .ok_or(Error::TimeoutConfig)?;
        self.timeout_seconds = seconds;
        Ok(())
    }
    fn deadline(&self) -> tokio::time::Instant {
        tokio::time::Instant::now() + Duration::from_secs(self.timeout_seconds)
    }
    fn reference(&self) -> String {
        format!("refs/heads/{}", self.branch)
    }
}
/// A conservative subset of Git branch names: slash-separated components of ASCII letters,
/// digits, `.`, `_` and `-`, none empty, starting with `.`/`-` or ending with `.`/`.lock`.
pub fn valid_branch(branch: &str) -> bool {
    !branch.is_empty()
        && branch.len() <= 128
        && branch != "HEAD"
        && branch.split('/').all(|part| {
            !part.is_empty()
                && !part.starts_with(['.', '-'])
                && !part.ends_with('.')
                && !part.ends_with(".lock")
                && !part.contains("..")
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        })
}
/// Re-indent JSON with two spaces and a trailing newline, changing insignificant whitespace
/// only. Tokens (key order, duplicate keys, number spellings, string escapes) are copied from
/// the original bytes. Input must already be valid JSON; unbalanced input returns `None`.
pub fn reindent(input: &[u8]) -> Option<Vec<u8>> {
    fn newline(out: &mut Vec<u8>, depth: usize) {
        out.push(b'\n');
        out.resize(out.len() + 2 * depth, b' ');
    }
    let whitespace = |b: u8| matches!(b, b' ' | b'\t' | b'\n' | b'\r');
    let mut out = Vec::with_capacity(input.len() + input.len() / 2 + 1);
    let mut depth = 0usize;
    let mut i = 0;
    while let Some(&byte) = input.get(i) {
        match byte {
            b if whitespace(b) => i += 1,
            b'"' => {
                let start = i;
                i += 1;
                loop {
                    match *input.get(i)? {
                        b'\\' => i += 2,
                        b'"' => break,
                        _ => i += 1,
                    }
                }
                i += 1;
                out.extend_from_slice(&input[start..i]);
            }
            b'{' | b'[' => {
                let close = if byte == b'{' { b'}' } else { b']' };
                let mut next = i + 1;
                while input.get(next).is_some_and(|b| whitespace(*b)) {
                    next += 1;
                }
                out.push(byte);
                if input.get(next) == Some(&close) {
                    out.push(close);
                    i = next + 1;
                } else {
                    depth += 1;
                    newline(&mut out, depth);
                    i += 1;
                }
            }
            b'}' | b']' => {
                depth = depth.checked_sub(1)?;
                newline(&mut out, depth);
                out.push(byte);
                i += 1;
            }
            b',' => {
                out.push(b',');
                newline(&mut out, depth);
                i += 1;
            }
            b':' => {
                out.extend_from_slice(b": ");
                i += 1;
            }
            _ => {
                out.push(byte);
                i += 1;
            }
        }
    }
    if depth != 0 || out.is_empty() {
        return None;
    }
    out.push(b'\n');
    Some(out)
}
/// Trailer linking a new commit to its scoped content identity. Informational only: written
/// on newly created commits and never read back by Sirius.
const CONTENT_TRAILER: &str = "Sirius-Content-SHA256";
/// Root `version.json` of the indented layout.
pub fn version_document(data_version: &str, asset_version: &str) -> Vec<u8> {
    format!(
        "{{\n  \"dataVersion\": {},\n  \"assetVersion\": {}\n}}\n",
        serde_json::Value::from(data_version),
        serde_json::Value::from(asset_version)
    )
    .into_bytes()
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
/// An owned, locked state directory; the lock is held until this value is dropped.
struct Owned {
    _owner: crate::file_lock::Exclusive,
    directory: PathBuf,
    reference: String,
    policy: CommitPolicy,
}
struct Prepared {
    owned: Owned,
    staging: tempfile::TempDir,
    manifest: PublishedManifest,
    names: Vec<String>,
}
/// Windows canonical paths use the `\\?\` verbatim form, which Git for Windows rejects
/// (`cannot mkdir ...: Invalid argument`). Convert drive and UNC forms back to ordinary
/// absolute paths; other platforms and other verbatim forms are returned unchanged.
fn without_verbatim_prefix(path: PathBuf) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path;
    };
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    match text.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => path,
    }
}
#[cfg(test)]
#[test]
fn verbatim_windows_paths_become_plain_absolute_paths() {
    for (input, expected) in [
        (r"\\?\C:\Temp\git-state", r"C:\Temp\git-state"),
        (r"\\?\UNC\server\share\git", r"\\server\share\git"),
        (r"\\?\Volume{0}\git", r"\\?\Volume{0}\git"),
        ("/var/lib/sirius/git", "/var/lib/sirius/git"),
    ] {
        assert_eq!(
            without_verbatim_prefix(PathBuf::from(input)),
            PathBuf::from(expected)
        );
    }
}
fn write_marker(
    directory: &Path,
    marker: &Path,
    expected: &serde_json::Value,
) -> Result<(), Error> {
    let mut file = tempfile::NamedTempFile::new_in(directory).map_err(|_| Error::Snapshot)?;
    use std::io::Write;
    file.write_all(&serde_json::to_vec(expected).map_err(|_| Error::Snapshot)?)
        .map_err(|_| Error::Snapshot)?;
    file.as_file().sync_all().map_err(|_| Error::Snapshot)?;
    file.persist(marker).map_err(|_| Error::Snapshot)?;
    Ok(())
}
/// Ownership, lock and marker checks shared by publication and adoption.
fn own(destination: &Path, scope: &Scope, options: &Options) -> Result<Owned, Error> {
    if let Ok(meta) = fs::symlink_metadata(destination) {
        if !meta.is_dir() || meta.file_type().is_symlink() {
            return Err(Error::Ownership);
        }
    }
    fs::create_dir_all(destination).map_err(|_| Error::Snapshot)?;
    let directory =
        without_verbatim_prefix(fs::canonicalize(destination).map_err(|_| Error::Snapshot)?);
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
    let owner = crate::file_lock::Exclusive::acquire(owner).map_err(|_| Error::Locked)?;
    let marker = directory.join("sirius-git.json");
    let expected = serde_json::json!({"schema_version":1,"scope":scope});
    if marker.exists() {
        let meta = fs::symlink_metadata(&marker).map_err(|_| Error::Ownership)?;
        if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > 4096 {
            return Err(Error::Ownership);
        }
        let mut existing: serde_json::Value =
            serde_json::from_slice(&fs::read(&marker).map_err(|_| Error::Ownership)?)
                .map_err(|_| Error::Ownership)?;
        // State written before 1.2.1 may record the deprecated alias of `hk`: read it as
        // `hk` and rewrite the marker so the alias is never kept or written.
        let legacy = existing["scope"]["region"]
            .as_str()
            .filter(|name| crate::region::is_deprecated_alias(name))
            .and_then(crate::region::Region::from_recorded_name);
        if let Some(region) = legacy {
            existing["scope"]["region"] = serde_json::json!(region);
        }
        if existing != expected {
            return Err(Error::Ownership);
        }
        if legacy.is_some() {
            write_marker(&directory, &marker, &expected)?;
        }
    } else {
        if fs::read_dir(&directory)
            .map_err(|_| Error::Ownership)?
            .any(|e| e.is_err() || e.unwrap().file_name() != "owner.lock")
        {
            return Err(Error::Ownership);
        }
        write_marker(&directory, &marker, &expected)?;
    }
    let repository = directory.join("repository.git");
    if fs::symlink_metadata(&repository).is_ok_and(|m| !m.is_dir() || m.file_type().is_symlink()) {
        return Err(Error::Ownership);
    }
    Ok(Owned {
        _owner: owner,
        directory,
        reference: options.reference(),
        policy: CommitPolicy::default(),
    })
}
fn prepare(
    source: &Path,
    destination: &Path,
    scope: Scope,
    options: &Options,
) -> Result<Prepared, Error> {
    let owned = own(destination, &scope, options)?;
    let document = master_registry::manifest(source, None, scope).map_err(|_| Error::Snapshot)?;
    let manifest: PublishedManifest =
        serde_json::from_slice(&document.bytes).map_err(|_| Error::Snapshot)?;
    // The hash becomes a commit trailer; lowercase hex cannot inject lines into the message.
    if !master_registry::hash_valid(&manifest.content_sha256) {
        return Err(Error::Snapshot);
    }
    // Fail before any Git command: an indented publication never carries a synthesized value.
    let asset_version = match options.layout {
        Layout::IndentedRoot => Some(
            manifest
                .resource_version
                .clone()
                .ok_or(Error::AssetVersion)?,
        ),
        Layout::Native => None,
    };
    let staging = tempfile::tempdir().map_err(|_| Error::Snapshot)?;
    let mut names = Vec::new();
    for file in &manifest.files {
        if matches!(
            file.name.as_str(),
            "sirius-publication.json" | "version.json"
        ) {
            return Err(Error::Snapshot);
        }
        let table = file.name.strip_suffix(".json").ok_or(Error::Snapshot)?;
        let document = master_registry::table(
            source,
            manifest.scope.region,
            &manifest.snapshot,
            table,
            &file.sha256,
        )
        .map_err(|_| Error::Snapshot)?;
        if document.bytes.len() as u64 != file.size {
            return Err(Error::Snapshot);
        }
        let bytes = match options.layout {
            Layout::Native => document.bytes,
            Layout::IndentedRoot => reindent(&document.bytes).ok_or(Error::Snapshot)?,
        };
        fs::write(staging.path().join(&file.name), bytes).map_err(|_| Error::Snapshot)?;
        names.push(file.name.clone());
    }
    if let Some(asset_version) = asset_version {
        fs::write(
            staging.path().join("version.json"),
            version_document(&manifest.version, &asset_version),
        )
        .map_err(|_| Error::Snapshot)?;
        names.push("version.json".into());
        names.sort();
        return Ok(Prepared {
            owned,
            staging,
            manifest,
            names,
        });
    }
    // Node-local UUIDs are excluded, so an identical import produces the same Git tree.
    // Asset provenance is excluded too, keeping the 1.2.0 native tree a function of content.
    let mut metadata = serde_json::to_value(&manifest).map_err(|_| Error::Snapshot)?;
    let object = metadata.as_object_mut().ok_or(Error::Snapshot)?;
    object.remove("snapshot");
    object.remove("resource_version");
    fs::write(
        staging.path().join("sirius-publication.json"),
        serde_json::to_vec_pretty(&metadata).map_err(|_| Error::Snapshot)?,
    )
    .map_err(|_| Error::Snapshot)?;
    names.push("sirius-publication.json".into());
    names.sort();
    Ok(Prepared {
        owned,
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
    if !is_oid(&value) {
        return Err(Error::Git);
    }
    Ok(value)
}
async fn command(
    owned: &Owned,
    args: Vec<OsString>,
    input: &[u8],
    deadline: tokio::time::Instant,
) -> Result<Vec<u8>, Error> {
    command_limited(owned, args, input, deadline, 1024 * 1024).await
}
async fn command_limited(
    owned: &Owned,
    args: Vec<OsString>,
    input: &[u8],
    deadline: tokio::time::Instant,
    output_limit: usize,
) -> Result<Vec<u8>, Error> {
    let remaining = deadline
        .checked_duration_since(tokio::time::Instant::now())
        .ok_or(Error::Git)?;
    let mut all = vec![
        OsString::from("--no-replace-objects"),
        OsString::from("--git-dir"),
        owned.directory.join("repository.git").into_os_string(),
        "-c".into(),
        "gc.auto=0".into(),
        "-c".into(),
        "commit.gpgsign=false".into(),
        "-c".into(),
        "user.name=Sirius Master Publisher".into(),
        "-c".into(),
        "user.email=sirius-master@localhost".into(),
    ];
    all.extend(owned.policy.arguments());
    all.extend(args);
    git_process::run_with_input(
        Path::new("git"),
        &owned.directory,
        &all,
        remaining,
        output_limit,
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
    commit_with_options(source, destination, scope, policy, &Options::default()).await
}
pub async fn commit_with_options(
    source: &Path,
    destination: &Path,
    scope: Scope,
    policy: &CommitPolicy,
    options: &Options,
) -> Result<Receipt, Error> {
    commit_internal(source, destination, scope, None, policy, options).await
}
pub async fn publish_with_policy(
    source: &Path,
    destination: &Path,
    scope: Scope,
    remote: &Remote,
    policy: &CommitPolicy,
) -> Result<Receipt, Error> {
    publish_with_options(
        source,
        destination,
        scope,
        remote,
        policy,
        &Options::default(),
    )
    .await
}
pub async fn publish_with_options(
    source: &Path,
    destination: &Path,
    scope: Scope,
    remote: &Remote,
    policy: &CommitPolicy,
    options: &Options,
) -> Result<Receipt, Error> {
    remote.validate()?;
    commit_internal(source, destination, scope, Some(remote), policy, options).await
}
async fn commit_internal(
    source: &Path,
    destination: &Path,
    scope: Scope,
    remote: Option<&Remote>,
    policy: &CommitPolicy,
    options: &Options,
) -> Result<Receipt, Error> {
    policy.validate()?;
    options.validate()?;
    if !cfg!(any(unix, windows)) {
        return Err(Error::Git);
    }
    let source = source.to_owned();
    let destination = destination.to_owned();
    let owned = options.clone();
    let mut prepared =
        tokio::task::spawn_blocking(move || prepare(&source, &destination, scope, &owned))
            .await
            .map_err(|_| Error::Snapshot)??;
    prepared.owned.policy = policy.clone();
    let deadline = options.deadline();
    let parent = initialize(&prepared.owned, deadline).await?;
    if let Some(remote) = remote {
        check_remote(&prepared.owned, remote, parent.as_deref(), deadline).await?;
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
        let hashes = command(&prepared.owned, args, &[], deadline).await?;
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
        &prepared.owned,
        vec!["mktree".into()],
        tree_input.as_bytes(),
        deadline,
    )
    .await?)?;
    if let Some(parent) = &parent {
        let old_tree = oid(command(
            &prepared.owned,
            vec!["rev-parse".into(), format!("{parent}^{{tree}}").into()],
            &[],
            deadline,
        )
        .await?)?;
        if old_tree == tree {
            return finish(
                &prepared.owned,
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
        // A second `-m` is a separate paragraph, so the subject stays unchanged.
        "-m".into(),
        format!("{CONTENT_TRAILER}: {}", prepared.manifest.content_sha256).into(),
    ];
    if let Some(parent) = &parent {
        args.extend(["-p".into(), parent.into()]);
    }
    if policy.signing.is_some() {
        args.push("-S".into());
    }
    let commit = oid(command(&prepared.owned, args, &[], deadline).await?)?;
    advance(&prepared.owned, &commit, parent.as_deref(), deadline).await?;
    finish(
        &prepared.owned,
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
/// Create the bare repository if needed and return the local branch commit.
async fn initialize(
    owned: &Owned,
    deadline: tokio::time::Instant,
) -> Result<Option<String>, Error> {
    command(
        owned,
        vec![
            "init".into(),
            "--bare".into(),
            "--quiet".into(),
            "--object-format=sha1".into(),
            owned.directory.join("repository.git").into_os_string(),
        ],
        &[],
        deadline,
    )
    .await?;
    command(
        owned,
        vec![
            "symbolic-ref".into(),
            "HEAD".into(),
            owned.reference.clone().into(),
        ],
        &[],
        deadline,
    )
    .await?;
    let refs = command(
        owned,
        vec![
            "for-each-ref".into(),
            "--format=%(objectname)".into(),
            owned.reference.clone().into(),
        ],
        &[],
        deadline,
    )
    .await?;
    if refs.is_empty() {
        Ok(None)
    } else {
        Ok(Some(oid(refs)?))
    }
}
/// Compare-and-swap the local branch; a missing branch is expected as the zero oid.
async fn advance(
    owned: &Owned,
    new: &str,
    old: Option<&str>,
    deadline: tokio::time::Instant,
) -> Result<(), Error> {
    command(
        owned,
        vec![
            "update-ref".into(),
            owned.reference.clone().into(),
            new.into(),
            old.map_or_else(|| "0".repeat(40), str::to_owned).into(),
        ],
        &[],
        deadline,
    )
    .await?;
    Ok(())
}

#[derive(Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Remote {
    pub url: String,
    pub authorization_env: Option<String>,
    pub proxy_url_env: Option<String>,
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
                && (url.to_file_path().is_err()
                    || self.authorization_env.is_some()
                    || self.proxy_url_env.is_some()))
        {
            return Err(Error::RemoteConfig);
        }
        if let Some(name) = &self.proxy_url_env {
            if name.is_empty()
                || name.len() > 256
                || name.starts_with("GIT_")
                || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                || ["http_proxy", "https_proxy", "all_proxy", "no_proxy"]
                    .contains(&name.to_ascii_lowercase().as_str())
            {
                return Err(Error::RemoteConfig);
            }
            let value = crate::config::secret(name).map_err(|_| Error::RemoteConfig)?;
            let proxy = url::Url::parse(&value).map_err(|_| Error::RemoteConfig)?;
            if value.len() > 4096
                || value.chars().any(|c| c.is_control() || c.is_whitespace())
                || value.contains('\\')
                || !matches!(proxy.scheme(), "http" | "https" | "socks5h")
                || proxy.host_str().is_none()
                || !matches!(proxy.path(), "" | "/")
                || proxy.query().is_some()
                || proxy.fragment().is_some()
                || proxy.password() == Some("")
                || (proxy.username().is_empty() != proxy.password().is_none())
            {
                return Err(Error::RemoteConfig);
            }
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
            "http.proxySSLVerify=true",
            "http.proxy=",
            "credential.helper=",
            "http.extraHeader=",
            "core.hooksPath=/dev/null",
            // Abort an HTTP(S) transfer stalled below 1000 B/s for 30 s inside the budget.
            "http.lowSpeedLimit=1000",
            "http.lowSpeedTime=30",
        ] {
            args.extend(["-c".into(), option.into()]);
        }
        if let Some(name) = &self.proxy_url_env {
            args.push(format!("--config-env=http.proxy={name}").into());
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
    #[cfg(test)]
    pub(crate) fn options_for_test(&self) -> Vec<OsString> {
        self.options()
    }
}
async fn network(
    owned: &Owned,
    remote: &Remote,
    args: Vec<OsString>,
    deadline: tokio::time::Instant,
) -> Result<Vec<u8>, Error> {
    let mut options = remote.options();
    options.extend(args);
    command(owned, options, &[], deadline).await
}
async fn remote_head(
    owned: &Owned,
    remote: &Remote,
    deadline: tokio::time::Instant,
) -> Result<Option<String>, Error> {
    let bytes = network(
        owned,
        remote,
        vec![
            "ls-remote".into(),
            "--refs".into(),
            remote.url.clone().into(),
            owned.reference.clone().into(),
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
    if reference != owned.reference {
        return Err(Error::Git);
    }
    Ok(Some(oid(hash.as_bytes().to_vec())?))
}
async fn check_remote(
    owned: &Owned,
    remote: &Remote,
    parent: Option<&str>,
    deadline: tokio::time::Instant,
) -> Result<(), Error> {
    let Some(head) = remote_head(owned, remote, deadline).await? else {
        return Ok(());
    };
    let parent = parent.ok_or(Error::RemoteChanged)?;
    if head == parent {
        return Ok(());
    }
    fetch_remote_check(owned, remote, &head, deadline).await?;
    command(
        owned,
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
/// Fetch the remote branch into `refs/sirius/remote-check` and require it to be `head`.
async fn fetch_remote_check(
    owned: &Owned,
    remote: &Remote,
    head: &str,
    deadline: tokio::time::Instant,
) -> Result<(), Error> {
    network(
        owned,
        remote,
        vec![
            "fetch".into(),
            "--no-tags".into(),
            "--no-write-fetch-head".into(),
            remote.url.clone().into(),
            format!("+{}:refs/sirius/remote-check", owned.reference).into(),
        ],
        deadline,
    )
    .await?;
    let fetched = oid(command(
        owned,
        vec!["rev-parse".into(), "refs/sirius/remote-check".into()],
        &[],
        deadline,
    )
    .await?)?;
    if fetched != head {
        return Err(Error::RemoteChanged);
    }
    Ok(())
}
async fn finish(
    owned: &Owned,
    remote: Option<&Remote>,
    mut receipt: Receipt,
    deadline: tokio::time::Instant,
) -> Result<Receipt, Error> {
    if let Some(remote) = remote {
        network(
            owned,
            remote,
            vec![
                "push".into(),
                "--porcelain".into(),
                remote.url.clone().into(),
                format!("{}:{}", receipt.commit, owned.reference).into(),
            ],
            deadline,
        )
        .await?;
        if remote_head(owned, remote, deadline).await?.as_deref() != Some(receipt.commit.as_str()) {
            return Err(Error::RemoteChanged);
        }
        receipt.remote_verified = true;
    }
    Ok(receipt)
}
/// Outcome of [`adopt`]: commit ids and a Master version only, never a path or URL.
#[derive(Clone, Serialize)]
pub struct Adoption {
    /// Local branch commit after the call.
    pub commit: String,
    pub previous: Option<String>,
    pub adopted: bool,
    /// Newest recognized Sirius publication in the adopted first-parent history.
    pub publication: Option<String>,
    pub version: Option<String>,
}
/// First-parent commits inspected for a Sirius publication before refusing.
const ADOPT_DEPTH: usize = 64;
/// The manifest table limit plus one metadata file.
const ADOPT_TREE_ENTRIES: usize = 4096 + 1;
fn is_oid(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
/// Adopt the remote branch as local managed state, for a lost state directory or a remote
/// that gained non-Sirius commits. The local branch is fast-forwarded (compare-and-swap)
/// only when it is missing or an ancestor of the remote, and only when the newest
/// `Sirius Master` commit within [`ADOPT_DEPTH`] first-parent commits is a publication of
/// this scope in the configured layout. Never pushes, never reads a Master directory and
/// never rewrites a commit; the next publication fast-forwards from the adopted head.
pub async fn adopt(destination: &Path, scope: Scope, remote: &Remote) -> Result<Adoption, Error> {
    adopt_with_options(destination, scope, remote, &Options::default()).await
}
pub async fn adopt_with_options(
    destination: &Path,
    scope: Scope,
    remote: &Remote,
    options: &Options,
) -> Result<Adoption, Error> {
    remote.validate()?;
    options.validate()?;
    if !cfg!(any(unix, windows)) {
        return Err(Error::Git);
    }
    let owned = {
        let (destination, scope, options) =
            (destination.to_owned(), scope.clone(), options.clone());
        tokio::task::spawn_blocking(move || own(&destination, &scope, &options))
            .await
            .map_err(|_| Error::Snapshot)??
    };
    let deadline = options.deadline();
    let local = initialize(&owned, deadline).await?;
    let head = remote_head(&owned, remote, deadline)
        .await?
        .ok_or(Error::NotAdoptable)?;
    let unchanged = |commit: String| Adoption {
        commit,
        previous: local.clone(),
        adopted: false,
        publication: None,
        version: None,
    };
    if local.as_deref() == Some(head.as_str()) {
        return Ok(unchanged(head));
    }
    fetch_remote_check(&owned, remote, &head, deadline).await?;
    if let Some(local) = &local {
        // A remote behind the local branch is advanced by the next ordinary push.
        if ancestor(&owned, &head, local, deadline).await {
            return Ok(unchanged(local.clone()));
        }
        if !ancestor(&owned, local, &head, deadline).await {
            return Err(Error::RemoteChanged);
        }
    }
    let (publication, version) = recognize(&owned, &scope, options.layout, &head, deadline).await?;
    advance(&owned, &head, local.as_deref(), deadline).await?;
    Ok(Adoption {
        commit: head,
        previous: local,
        adopted: true,
        publication: Some(publication),
        version: Some(version),
    })
}
async fn ancestor(owned: &Owned, older: &str, newer: &str, deadline: tokio::time::Instant) -> bool {
    command(
        owned,
        vec![
            "merge-base".into(),
            "--is-ancestor".into(),
            older.into(),
            newer.into(),
        ],
        &[],
        deadline,
    )
    .await
    .is_ok()
}
/// Find the newest `Sirius Master <region> <version>` first-parent commit of `head` and
/// require its tree to be exactly a publication of `scope` in `layout`. Older commits are
/// never consulted once a Sirius subject is found.
async fn recognize(
    owned: &Owned,
    scope: &Scope,
    layout: Layout,
    head: &str,
    deadline: tokio::time::Instant,
) -> Result<(String, String), Error> {
    let log = command(
        owned,
        vec![
            "rev-list".into(),
            "--first-parent".into(),
            format!("--max-count={ADOPT_DEPTH}").into(),
            "--format=%H%x09%s".into(),
            head.into(),
        ],
        &[],
        deadline,
    )
    .await?;
    let log = String::from_utf8_lossy(&log);
    let mut decided = None;
    for line in log.lines() {
        // Without --no-commit-header (Git 2.33+) each entry is preceded by `commit <oid>`.
        if line.strip_prefix("commit ").is_some_and(is_oid) {
            continue;
        }
        let (commit, subject) = line.split_once('\t').ok_or(Error::Git)?;
        if !is_oid(commit) {
            return Err(Error::Git);
        }
        if let Some(rest) = subject.strip_prefix("Sirius Master ") {
            decided = Some((commit.to_owned(), rest.to_owned()));
            break;
        }
    }
    let (commit, rest) = decided.ok_or(Error::NotAdoptable)?;
    let (region, version) = rest.split_once(' ').ok_or(Error::NotAdoptable)?;
    if crate::region::Region::from_recorded_name(region) != Some(scope.region)
        || !crate::master::safe_version(version)
    {
        return Err(Error::NotAdoptable);
    }
    let listing = command(
        owned,
        vec!["ls-tree".into(), "-z".into(), commit.clone().into()],
        &[],
        deadline,
    )
    .await?;
    let mut entries = std::collections::BTreeMap::new();
    for entry in listing.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        let entry = std::str::from_utf8(entry).map_err(|_| Error::NotAdoptable)?;
        let (header, name) = entry.split_once('\t').ok_or(Error::NotAdoptable)?;
        let blob = header
            .strip_prefix("100644 blob ")
            .filter(|blob| is_oid(blob))
            .ok_or(Error::NotAdoptable)?;
        if entries.len() >= ADOPT_TREE_ENTRIES
            || name.len() <= ".json".len()
            || !name.ends_with(".json")
            || name.starts_with('.')
            || name
                .chars()
                .any(|c| c == '/' || c == '\\' || c.is_control())
        {
            return Err(Error::NotAdoptable);
        }
        entries.insert(name.to_owned(), blob.to_owned());
    }
    match layout {
        Layout::Native => {
            #[derive(serde::Deserialize)]
            struct Recorded {
                scope: serde_json::Value,
                version: String,
                files: Vec<RecordedFile>,
            }
            #[derive(serde::Deserialize)]
            struct RecordedFile {
                name: String,
            }
            if entries.contains_key("version.json") {
                return Err(Error::NotAdoptable);
            }
            let blob = entries
                .remove("sirius-publication.json")
                .ok_or(Error::NotAdoptable)?;
            // Metadata lists up to 4096 tables twice (files and source manifest).
            let bytes = command_limited(
                owned,
                vec!["cat-file".into(), "blob".into(), blob.into()],
                &[],
                deadline,
                4 * 1024 * 1024,
            )
            .await?;
            let mut recorded: Recorded =
                serde_json::from_slice(&bytes).map_err(|_| Error::NotAdoptable)?;
            // Publications before 1.2.1 may record the deprecated alias of `hk`.
            if let Some(region) = recorded.scope.pointer_mut("/region") {
                let legacy = region
                    .as_str()
                    .filter(|name| crate::region::is_deprecated_alias(name))
                    .and_then(crate::region::Region::from_recorded_name);
                if let Some(legacy) = legacy {
                    *region = serde_json::json!(legacy);
                }
            }
            let recorded_scope: Scope =
                serde_json::from_value(recorded.scope).map_err(|_| Error::NotAdoptable)?;
            let mut names: Vec<&str> = recorded.files.iter().map(|f| f.name.as_str()).collect();
            names.sort_unstable();
            if recorded_scope != *scope
                || recorded.version != version
                || !names.iter().copied().eq(entries.keys().map(String::as_str))
            {
                return Err(Error::NotAdoptable);
            }
        }
        Layout::IndentedRoot => {
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields, rename_all = "camelCase")]
            struct VersionDocument {
                data_version: String,
                asset_version: String,
            }
            if entries.contains_key("sirius-publication.json") {
                return Err(Error::NotAdoptable);
            }
            let blob = entries.get("version.json").ok_or(Error::NotAdoptable)?;
            let bytes = command(
                owned,
                vec!["cat-file".into(), "blob".into(), blob.into()],
                &[],
                deadline,
            )
            .await?;
            let document: VersionDocument =
                serde_json::from_slice(&bytes).map_err(|_| Error::NotAdoptable)?;
            if document.data_version != version
                || bytes != version_document(&document.data_version, &document.asset_version)
            {
                return Err(Error::NotAdoptable);
            }
        }
    }
    Ok((commit, version.to_owned()))
}
#[cfg(test)]
pub(crate) async fn advance_for_test(
    destination: &Path,
    scope: Scope,
    options: &Options,
    new: &str,
    old: Option<&str>,
) -> Result<(), Error> {
    let owned = own(destination, &scope, options)?;
    advance(&owned, new, old, options.deadline()).await
}
