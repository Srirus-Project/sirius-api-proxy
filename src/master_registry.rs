//! Content-verified plaintext manifests over immutable Sirius Master snapshots.
use crate::{
    master::{self, Manifest, MasterError},
    region::{Platform, Region},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    io::Write,
    path::{Path, PathBuf},
};
const MAX_JSON: u64 = 64 * 1024 * 1024;
pub(crate) const MAX_TOTAL: u64 = 512 * 1024 * 1024;
const MAX_INDEX: u64 = 1024 * 1024;
#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct File {
    pub name: String,
    pub size: u64,
    pub sha256: String,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Inventory {
    pub schema_version: u32,
    pub version: String,
    pub files: Vec<File>,
}
#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    pub region: Region,
    pub environment: String,
    pub platform: Platform,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PublishedManifest {
    pub schema_version: u32,
    pub scope: Scope,
    pub snapshot: String,
    pub version: String,
    /// Asset (resource) version recorded with this installation, from the same game VERSION
    /// observation as `version` (or carried from the owner). Absent on legacy snapshots.
    /// Provenance only: excluded from `content_sha256`, which identifies table content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_version: Option<String>,
    pub content_sha256: String,
    pub files: Vec<File>,
    /// Original encrypted-file metadata, without any CDN credentials or keys.
    pub source_manifest: Manifest,
}
/// `deserialize_with` for configuration scopes (`registry-serve`, `master-db-*`): like
/// [`Scope`], but the region also accepts the deprecated alias of `hk`.
pub fn config_scope<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Scope, D::Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ConfigScope {
        #[serde(deserialize_with = "crate::region::config_region")]
        region: Region,
        environment: String,
        platform: Platform,
    }
    let scope = ConfigScope::deserialize(deserializer)?;
    Ok(Scope {
        region: scope.region,
        environment: scope.environment,
        platform: scope.platform,
    })
}
impl PublishedManifest {
    pub fn validate(&self, scope: &Scope) -> Result<(), MasterError> {
        if self.schema_version != 1
            || &self.scope != scope
            || !scope.region.master_supported()
            || !self.snapshot.starts_with("master-")
            || !master::safe_component(&self.snapshot)
            || self.version != self.source_manifest.version
            || self
                .resource_version
                .as_deref()
                .is_some_and(|v| !master::safe_version(v))
        {
            return Err(MasterError::Format);
        }
        let source = Manifest::parse(
            &serde_json::to_vec(&self.source_manifest).map_err(|_| MasterError::Format)?,
        )?;
        if source
            .files
            .windows(2)
            .any(|pair| pair[0].name >= pair[1].name)
        {
            return Err(MasterError::Format);
        }
        let inventory = Inventory {
            schema_version: 1,
            version: self.version.clone(),
            files: self.files.clone(),
        };
        inventory.validate(&source)?;
        if content_hash(scope, &source, &inventory)? != self.content_sha256 {
            return Err(MasterError::Integrity);
        }
        Ok(())
    }
}
pub fn content_hash(
    scope: &Scope,
    source: &Manifest,
    inventory: &Inventory,
) -> Result<String, MasterError> {
    let bytes=serde_json::to_vec(&serde_json::json!({"schema_version":1,"scope":scope,"source_manifest":source,"files":inventory.files})).map_err(|_|MasterError::Format)?;
    Ok(digest(&bytes))
}
pub struct Document {
    pub bytes: Vec<u8>,
    pub etag: String,
    pub version: String,
}
pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub fn file(name: String, bytes: &[u8]) -> File {
    File {
        name,
        size: bytes.len() as u64,
        sha256: digest(bytes),
    }
}
pub fn hash_valid(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
impl Inventory {
    pub fn validate(&self, source: &Manifest) -> Result<(), MasterError> {
        if self.schema_version != 1
            || self.version != source.version
            || self.files.len() != source.files.len()
        {
            return Err(MasterError::Format);
        }
        let mut expected = source
            .files
            .iter()
            .map(|e| e.name.replace(".bin", ".json"))
            .collect::<Vec<_>>();
        expected.sort();
        let mut total = 0u64;
        for (entry, name) in self.files.iter().zip(expected) {
            if entry.name != name
                || entry.size == 0
                || entry.size > MAX_JSON
                || !hash_valid(&entry.sha256)
            {
                return Err(MasterError::Format);
            }
            total = total.checked_add(entry.size).ok_or(MasterError::Limit)?;
        }
        if total > MAX_TOTAL {
            return Err(MasterError::Limit);
        }
        Ok(())
    }
}
fn regular(path: &Path, limit: u64) -> Result<Vec<u8>, MasterError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(MasterError::Format);
    }
    master::read_bounded(path, limit)
}
fn snapshot_directory(root: &Path, snapshot: &str) -> Result<PathBuf, MasterError> {
    if !snapshot.starts_with("master-") || !master::safe_component(snapshot) {
        return Err(MasterError::NotFound);
    }
    let path = root.join(snapshot);
    let metadata = fs::symlink_metadata(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            MasterError::NotFound
        } else {
            e.into()
        }
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(MasterError::Format);
    }
    Ok(path)
}
fn source(directory: &Path, region: Region) -> Result<Manifest, MasterError> {
    let (source, _, recorded) = source_with_provenance(directory)?;
    if recorded != region {
        return Err(MasterError::Format);
    }
    Ok(source)
}
/// Region recorded by a committed snapshot (legacy receipts without one are JP).
pub(crate) fn snapshot_region(root: &Path, snapshot: &str) -> Result<Region, MasterError> {
    let directory = snapshot_directory(root, snapshot)?;
    source_with_provenance(&directory).map(|(_, _, region)| region)
}
fn source_with_provenance(
    directory: &Path,
) -> Result<(Manifest, Option<String>, Region), MasterError> {
    let source = Manifest::parse(&regular(
        &directory.join("MasterManifest.json"),
        master::MAX_MANIFEST,
    )?)?;
    let receipt: serde_json::Value =
        serde_json::from_slice(&regular(&directory.join("receipt.json"), 4096)?)
            .map_err(|_| MasterError::Format)?;
    if receipt["version"] != source.version
        || receipt["snapshot"].as_str() != directory.file_name().and_then(|name| name.to_str())
        || receipt["tables"].as_u64() != Some(source.files.len() as u64)
        || !matches!(
            receipt["source"].as_str(),
            Some("local-import" | "remote" | "registry")
        )
    {
        return Err(MasterError::Format);
    }
    let resource_version = master::recorded_resource_version(&receipt)?;
    let region = master::recorded_region(&receipt)?;
    Ok((source, resource_version, region))
}
fn inventory(directory: &Path, source: &Manifest) -> Result<Inventory, MasterError> {
    let path = directory.join("tables.json");
    let value = match fs::symlink_metadata(&path) {
        Ok(_) => serde_json::from_slice::<Inventory>(&regular(&path, MAX_INDEX)?)
            .map_err(|_| MasterError::Format)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Existing 1.1 snapshots remain readable. Compute their manifest in memory;
            // pinned blob reads still verify the caller's digest, without rescanning all tables.
            let mut files = Vec::new();
            let mut total = 0u64;
            for entry in &source.files {
                let name = entry.name.replace(".bin", ".json");
                let bytes = regular(&directory.join(&name), MAX_JSON)?;
                total += bytes.len() as u64;
                if total > MAX_TOTAL {
                    return Err(MasterError::Limit);
                }
                master::validate_json(&bytes).map_err(|_| MasterError::Format)?;
                files.push(file(name, &bytes));
            }
            files.sort_by(|a, b| a.name.cmp(&b.name));
            Inventory {
                schema_version: 1,
                version: source.version.clone(),
                files,
            }
        }
        Err(e) => return Err(e.into()),
    };
    value.validate(source)?;
    Ok(value)
}
pub fn current_snapshot(root: &Path) -> Result<String, MasterError> {
    let bytes = regular(&root.join("CURRENT"), 128)?;
    let snapshot = String::from_utf8(bytes).map_err(|_| MasterError::Format)?;
    snapshot_directory(root, &snapshot)?;
    Ok(snapshot)
}
pub fn manifest(
    root: &Path,
    snapshot: Option<&str>,
    scope: Scope,
) -> Result<Document, MasterError> {
    if !scope.region.master_supported()
        || scope.environment.is_empty()
        || scope.environment.len() > 256
        || !scope
            .environment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(MasterError::Format);
    }
    let snapshot = match snapshot {
        Some(v) => v.to_owned(),
        None => current_snapshot(root)?,
    };
    let directory = snapshot_directory(root, &snapshot)?;
    let (mut source, resource_version, region) = source_with_provenance(&directory)?;
    // Content identity is scoped: a snapshot is served only under the region that made it.
    if region != scope.region {
        return Err(MasterError::Format);
    }
    source.files.sort_by(|a, b| a.name.cmp(&b.name));
    let inventory = inventory(&directory, &source)?;
    let content = content_hash(&scope, &source, &inventory)?;
    let manifest = PublishedManifest {
        schema_version: 1,
        scope,
        snapshot,
        version: source.version.clone(),
        resource_version,
        content_sha256: content,
        files: inventory.files,
        source_manifest: source,
    };
    let bytes = serde_json::to_vec(&manifest).map_err(|_| MasterError::Format)?;
    Ok(Document {
        etag: format!("\"{}\"", digest(&bytes)),
        version: manifest.version,
        bytes,
    })
}
pub fn table(
    root: &Path,
    region: Region,
    snapshot: &str,
    table: &str,
    expected_hash: &str,
) -> Result<Document, MasterError> {
    table_read(root, region, snapshot, table, expected_hash, true)
}
/// For unchanged polls only. An indexed table whose length and SHA-256 match was validated
/// as JSON before its index was written, so only legacy unindexed tables are parsed again.
pub(crate) fn table_intact(
    root: &Path,
    region: Region,
    snapshot: &str,
    table: &str,
    expected_hash: &str,
) -> bool {
    table_read(root, region, snapshot, table, expected_hash, false).is_ok()
}
fn table_read(
    root: &Path,
    region: Region,
    snapshot: &str,
    table: &str,
    expected_hash: &str,
    validate_indexed: bool,
) -> Result<Document, MasterError> {
    if !master::safe_component(table) || !hash_valid(expected_hash) {
        return Err(MasterError::NotFound);
    }
    let directory = snapshot_directory(root, snapshot)?;
    let source = source(&directory, region)?;
    if !source
        .files
        .iter()
        .any(|entry| entry.name == format!("{table}.bin"))
    {
        return Err(MasterError::NotFound);
    }
    let bytes = regular(&directory.join(format!("{table}.json")), MAX_JSON)?;
    if digest(&bytes) != expected_hash {
        return Err(MasterError::Integrity);
    }
    // The check above proved `expected_hash` is the digest of these bytes.
    let indexed = verify_indexed_digest(
        &directory,
        &source,
        table,
        bytes.len() as u64,
        expected_hash,
    )?;
    if validate_indexed || !indexed {
        master::validate_json(&bytes).map_err(|_| MasterError::Format)?;
    }
    Ok(Document {
        etag: format!("\"{expected_hash}\""),
        version: source.version,
        bytes,
    })
}
/// Strengthens ordinary table reads as well; absent indexes preserve 1.1 compatibility.
/// `size`/`sha256` describe bytes the caller already read, so they are hashed only once.
/// `Ok(true)` means the index exists and matched; `Ok(false)` is a legacy unindexed snapshot.
pub(crate) fn verify_indexed_digest(
    directory: &Path,
    source: &Manifest,
    table: &str,
    size: u64,
    sha256: &str,
) -> Result<bool, MasterError> {
    let path = directory.join("tables.json");
    match fs::symlink_metadata(&path) {
        Ok(_) => {
            let index: Inventory = serde_json::from_slice(&regular(&path, MAX_INDEX)?)
                .map_err(|_| MasterError::Format)?;
            index.validate(source)?;
            let entry = index
                .files
                .iter()
                .find(|e| e.name == format!("{table}.json"))
                .ok_or(MasterError::Format)?;
            if entry.size != size || entry.sha256 != sha256 {
                return Err(MasterError::Integrity);
            }
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Installed with the snapshot before CURRENT changes. The predecessor is the last
/// committed snapshot, never a directory discovered by scanning staging/orphans.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Publication {
    pub schema_version: u32,
    pub snapshot: String,
    pub previous_snapshot: Option<String>,
    pub published_at: chrono::DateTime<chrono::Utc>,
}

pub(crate) fn predecessor(root: &Path) -> Result<Option<String>, MasterError> {
    let pointer = match regular(&root.join("CURRENT"), 128) {
        Ok(bytes) => bytes,
        Err(MasterError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let snapshot = String::from_utf8(pointer).map_err(|_| MasterError::Format)?;
    let directory = snapshot_directory(root, &snapshot)?;
    source_with_provenance(&directory)?;
    Ok(Some(snapshot))
}

#[derive(Serialize)]
pub struct HistoryEntry {
    pub snapshot: String,
    pub version: String,
    /// The snapshot's recorded asset version, or null when none was recorded.
    pub resource_version: Option<String>,
    pub content_sha256: String,
    pub published_at: Option<chrono::DateTime<chrono::Utc>>,
    pub file_count: usize,
    pub total_size: u64,
}
#[derive(Serialize)]
pub struct History {
    pub schema_version: u32,
    pub scope: Scope,
    pub head: String,
    pub entries: Vec<HistoryEntry>,
    pub has_more: bool,
    /// Pass this snapshot as `before` to continue with older committed entries.
    pub next_before: Option<String>,
    /// Older snapshots have no predecessor record; their chronology is unknown.
    pub legacy_boundary: bool,
    /// Older committed snapshots were removed by snapshot retention. Absent means false:
    /// omitted so the serialization of directories never pruned is unchanged.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub retention_boundary: bool,
}

pub fn history(root: &Path, scope: Scope, limit: usize) -> Result<History, MasterError> {
    history_page(root, scope, limit, None)
}

pub fn valid_history_cursor(value: &str) -> bool {
    value.len() <= 128 && value.starts_with("master-") && master::safe_component(value)
}

pub fn history_page(
    root: &Path,
    scope: Scope,
    limit: usize,
    before: Option<&str>,
) -> Result<History, MasterError> {
    if !(1..=100).contains(&limit) {
        return Err(MasterError::Limit);
    }
    history_page_inner(root, scope, limit, before)
}

/// Traverse only the chain pinned by CURRENT; never include uncommitted directories.
pub(crate) fn committed_history(root: &Path, scope: Scope) -> Result<History, MasterError> {
    let history = history_page_inner(root, scope, 10_000, None)?;
    if history.has_more {
        return Err(MasterError::Limit);
    }
    Ok(history)
}

fn history_page_inner(
    root: &Path,
    scope: Scope,
    limit: usize,
    before: Option<&str>,
) -> Result<History, MasterError> {
    let boundary = retention_boundary(root)?;
    history_walk(root, scope, limit, before, boundary)
}

/// Test entry point: walk with the retention boundary a reader read before a pass moved it.
#[cfg(test)]
pub(crate) fn history_page_with_boundary(
    root: &Path,
    scope: Scope,
    limit: usize,
    before: Option<&str>,
    boundary: Option<String>,
) -> Result<History, MasterError> {
    history_walk(root, scope, limit, before, boundary)
}

fn history_walk(
    root: &Path,
    scope: Scope,
    limit: usize,
    before: Option<&str>,
    boundary: Option<String>,
) -> Result<History, MasterError> {
    if before.is_some_and(|v| !valid_history_cursor(v)) {
        return Err(MasterError::Format);
    }
    let head = predecessor(root)?.ok_or(MasterError::NotFound)?;
    let mut cursor_found = before.is_none();
    let mut next = Some(head.clone());
    let mut visited = BTreeSet::new();
    let mut entries = Vec::<HistoryEntry>::new();
    let mut legacy_boundary = false;
    let mut retained = false;
    while let Some(snapshot) = next.take() {
        if visited.len() == 10_000 {
            return Err(MasterError::Limit);
        }
        if !visited.insert(snapshot.clone()) {
            return Err(MasterError::Format);
        }
        let (doc, publication) = match chain_step(root, &scope, &snapshot, &visited) {
            Ok(step) => step,
            Err(error) => {
                let crossed = crossed_boundary(root, error, &snapshot, &visited)?;
                if before == Some(crossed.as_str()) {
                    entries.clear();
                } else if let Some(at) = entries.iter().position(|e| e.snapshot == crossed) {
                    entries.truncate(at + 1);
                } else {
                    // The cursor itself lies beyond the new boundary: it was pruned.
                    return Err(MasterError::NotFound);
                }
                retained = true;
                break;
            }
        };
        let value: PublishedManifest =
            serde_json::from_slice(&doc.bytes).map_err(|_| MasterError::Format)?;
        if publication.is_none() {
            legacy_boundary = true;
        }
        let published_at = publication.as_ref().map(|p| p.published_at);
        next = publication.and_then(|p| p.previous_snapshot);
        if next.is_some() && boundary.as_deref() == Some(snapshot.as_str()) {
            next = None;
            retained = true;
        }
        if !cursor_found {
            cursor_found = before == Some(snapshot.as_str());
            continue;
        }
        entries.push(HistoryEntry {
            snapshot,
            version: value.version,
            resource_version: value.resource_version,
            content_sha256: value.content_sha256,
            published_at,
            file_count: value.files.len(),
            total_size: value.files.iter().map(|f| f.size).sum(),
        });
        if entries.len() == limit {
            break;
        }
    }
    if !cursor_found {
        return Err(MasterError::NotFound);
    }
    let next_before = if next.is_some() {
        entries.last().map(|entry| entry.snapshot.clone())
    } else {
        None
    };
    Ok(History {
        schema_version: 1,
        scope,
        head,
        entries,
        has_more: next.is_some(),
        next_before,
        legacy_boundary,
        retention_boundary: retained,
    })
}

/// One committed chain step. A directory renamed away by a concurrent retention pass
/// surfaces as missing, never as a legacy snapshot without a publication record.
fn chain_step(
    root: &Path,
    scope: &Scope,
    snapshot: &str,
    visited: &BTreeSet<String>,
) -> Result<(Document, Option<Publication>), MasterError> {
    let directory = snapshot_directory(root, snapshot)?;
    let document = manifest(root, Some(snapshot), scope.clone())?;
    let record = publication(&directory, snapshot, visited)?;
    if record.is_none() {
        snapshot_directory(root, snapshot)?;
    }
    Ok((document, record))
}

/// A reader that read the boundary before a retention pass moved it can find an older
/// snapshot renamed away mid-walk. Only a new boundary this walk already crossed explains
/// that; any other missing directory remains an explicit error, never hidden as retention.
fn crossed_boundary(
    root: &Path,
    error: MasterError,
    failed: &str,
    visited: &BTreeSet<String>,
) -> Result<String, MasterError> {
    let missing = match &error {
        MasterError::NotFound => true,
        MasterError::Io(e) => e.kind() == std::io::ErrorKind::NotFound,
        _ => false,
    };
    if !missing {
        return Err(error);
    }
    match retention_boundary(root) {
        Ok(Some(boundary)) if boundary != failed && visited.contains(&boundary) => Ok(boundary),
        _ => Err(error),
    }
}

fn publication(
    directory: &Path,
    snapshot: &str,
    visited: &BTreeSet<String>,
) -> Result<Option<Publication>, MasterError> {
    match regular(&directory.join("publication.json"), 4096) {
        Ok(bytes) => {
            let record: Publication =
                serde_json::from_slice(&bytes).map_err(|_| MasterError::Format)?;
            if record.schema_version != 1
                || record.snapshot != snapshot
                || record.previous_snapshot.as_ref().is_some_and(|previous| {
                    !previous.starts_with("master-")
                        || !master::safe_component(previous)
                        || visited.contains(previous)
                })
            {
                return Err(MasterError::Format);
            }
            Ok(Some(record))
        }
        Err(MasterError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}
/// Find the newest committed installation with this scoped content identity.
/// UUID and response ETag may change after an identical reimport; content identity does not.
/// The walk stops at the retention boundary: pruned content identities are not found.
pub fn manifest_by_hash(root: &Path, scope: Scope, hash: &str) -> Result<Document, MasterError> {
    if !hash_valid(hash) {
        return Err(MasterError::Format);
    }
    let boundary = retention_boundary(root)?;
    by_hash_walk(root, scope, hash, boundary)
}

/// Test entry point: look up with the retention boundary a reader read before a pass.
#[cfg(test)]
pub(crate) fn manifest_by_hash_with_boundary(
    root: &Path,
    scope: Scope,
    hash: &str,
    boundary: Option<String>,
) -> Result<Document, MasterError> {
    by_hash_walk(root, scope, hash, boundary)
}

fn by_hash_walk(
    root: &Path,
    scope: Scope,
    hash: &str,
    boundary: Option<String>,
) -> Result<Document, MasterError> {
    let mut next = predecessor(root)?;
    let mut visited = BTreeSet::new();
    while let Some(snapshot) = next.take() {
        if visited.len() == 10_000 {
            return Err(MasterError::Limit);
        }
        if !visited.insert(snapshot.clone()) {
            return Err(MasterError::Format);
        }
        let (document, record) = match chain_step(root, &scope, &snapshot, &visited) {
            Ok(step) => step,
            Err(error) => {
                crossed_boundary(root, error, &snapshot, &visited)?;
                return Err(MasterError::NotFound);
            }
        };
        let value: PublishedManifest =
            serde_json::from_slice(&document.bytes).map_err(|_| MasterError::Format)?;
        if value.content_sha256 == hash {
            return Ok(document);
        }
        if boundary.as_deref() == Some(snapshot.as_str()) {
            break;
        }
        next = record.and_then(|r| r.previous_snapshot);
    }
    Err(MasterError::NotFound)
}

/// Optional file snapshot retention. The newest `keep_snapshots` installations along the
/// committed publication chain are kept (an identical reimport counts as its own
/// installation); older chain entries are removed. Absent keeps every snapshot.
#[derive(Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Retention {
    pub keep_snapshots: usize,
}
impl Retention {
    /// At least 2: the snapshot CURRENT just replaced stays readable for pinned readers.
    /// At most the committed history walk limit, so migration still reads the whole window.
    pub fn valid(&self) -> bool {
        (2..=10_000).contains(&self.keep_snapshots)
    }
}

/// Snapshots removed by one retention pass at most. A larger backlog converges over later
/// passes, oldest first, so every remaining candidate stays reachable from the boundary.
pub(crate) const MAX_PRUNED_PER_PASS: usize = 64;

/// `retention.json`: the oldest retained snapshot. Data, not configuration: readers honour
/// it even after retention is unconfigured, because older snapshots no longer exist.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RetentionRecord {
    schema_version: u32,
    boundary: String,
}

pub(crate) fn retention_boundary(root: &Path) -> Result<Option<String>, MasterError> {
    let bytes = match regular(&root.join("retention.json"), 4096) {
        Ok(bytes) => bytes,
        Err(MasterError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let record: RetentionRecord =
        serde_json::from_slice(&bytes).map_err(|_| MasterError::Format)?;
    if record.schema_version != 1
        || !record.boundary.starts_with("master-")
        || !master::safe_component(&record.boundary)
    {
        return Err(MasterError::Format);
    }
    Ok(Some(record.boundary))
}

fn write_retention(root: &Path, boundary: &str) -> Result<(), MasterError> {
    let record = RetentionRecord {
        schema_version: 1,
        boundary: boundary.to_owned(),
    };
    let bytes = serde_json::to_vec(&record).map_err(|_| MasterError::Format)?;
    let mut file = tempfile::NamedTempFile::new_in(root)?;
    file.write_all(&bytes)?;
    file.as_file().sync_all()?;
    file.persist(root.join("retention.json"))
        .map_err(|err| MasterError::Io(err.error))?;
    master::sync_directory(root)
}

/// Remove committed snapshots older than the newest `keep` along CURRENT's chain. Only the
/// writer may prune; the directory is never scanned, so staging, download, orphan and
/// legacy directories (and other chains) are left alone.
pub(crate) fn prune(
    writer: &master::WriterLock,
    root: &Path,
    keep: usize,
) -> Result<usize, MasterError> {
    prune_within(writer, root, keep, MAX_PRUNED_PER_PASS)
}

pub(crate) fn prune_within(
    _writer: &master::WriterLock,
    root: &Path,
    keep: usize,
    budget: usize,
) -> Result<usize, MasterError> {
    if !(Retention {
        keep_snapshots: keep,
    })
    .valid()
    {
        return Err(MasterError::Limit);
    }
    let Some(head) = predecessor(root)? else {
        return Ok(0);
    };
    let old = retention_boundary(root)?;
    let mut visited = BTreeSet::new();
    let mut next = Some(head);
    let mut kept = 0;
    // The boundary is the keep-th installation, or an earlier recorded one: raising keep
    // never moves the boundary back to snapshots readers were already told are gone.
    let (boundary, mut next) = loop {
        let Some(snapshot) = next.take() else {
            return Ok(0);
        };
        if !visited.insert(snapshot.clone()) {
            return Err(MasterError::Format);
        }
        let directory = snapshot_directory(root, &snapshot)?;
        // A legacy snapshot inside the window has no recorded predecessor to prune.
        let Some(record) = publication(&directory, &snapshot, &visited)? else {
            return Ok(0);
        };
        kept += 1;
        if kept == keep || old.as_deref() == Some(snapshot.as_str()) {
            break (snapshot, record.previous_snapshot);
        }
        next = record.previous_snapshot;
    };
    let mut candidates = Vec::new();
    while let Some(snapshot) = next.take() {
        if candidates.len() == 10_000 {
            return Err(MasterError::Limit);
        }
        if !visited.insert(snapshot.clone()) {
            return Err(MasterError::Format);
        }
        let directory = match snapshot_directory(root, &snapshot) {
            Ok(directory) => directory,
            // Removed by an earlier pass, which deletes oldest first.
            Err(MasterError::NotFound) => break,
            Err(e) => return Err(e),
        };
        match publication(&directory, &snapshot, &visited)? {
            Some(record) => {
                next = record.previous_snapshot;
                candidates.push(snapshot);
            }
            // Legacy snapshots have no recorded chronology and are never removed.
            None => break,
        }
    }
    if candidates.is_empty() {
        return Ok(0);
    }
    // The boundary is durable before anything disappears, so readers stop there first.
    if old.as_deref() != Some(boundary.as_str()) {
        write_retention(root, &boundary)?;
    }
    let mut pruned = 0;
    for snapshot in candidates.iter().rev().take(budget) {
        // Rename first: the snapshot leaves its canonical path atomically. A failure (for
        // example an open handle on Windows) stops the pass with the rest still contiguous.
        let trash = root.join(format!(".master-pruned-{}", uuid::Uuid::new_v4().simple()));
        fs::rename(root.join(snapshot), &trash)?;
        master::sync_directory(root)?;
        if fs::remove_dir_all(&trash).is_err() {
            tracing::warn!(
                error_code = "master_retention_cleanup_failed",
                "Pruned Master snapshot left unreachable; it is safe to delete by hand"
            );
        }
        pruned += 1;
    }
    Ok(pruned)
}

/// Apply configured retention after a pass's result is settled. Never fails that pass: a
/// busy writer skips this cycle, and a failed pass keeps what it did not remove. The
/// blocking worker owns the lock, so cancelling the caller never releases it mid-pass.
pub(crate) async fn retain(output: &Path, retention: Option<Retention>) -> Option<usize> {
    let keep = retention?.keep_snapshots;
    let writer = match master::WriterLock::acquire(output) {
        Ok(writer) => writer,
        Err(MasterError::Busy) => {
            tracing::debug!("Master snapshot retention skipped; another writer is active");
            return None;
        }
        Err(_) => {
            tracing::warn!(
                error_code = "master_retention_failed",
                "Master snapshot retention failed; snapshots retained"
            );
            return None;
        }
    };
    let output = output.to_owned();
    match tokio::task::spawn_blocking(move || prune(&writer, &output, keep)).await {
        Ok(Ok(pruned)) => Some(pruned),
        _ => {
            tracing::warn!(
                error_code = "master_retention_failed",
                "Master snapshot retention failed; snapshots retained"
            );
            None
        }
    }
}
