//! Content-verified plaintext manifests over immutable Sirius Master snapshots.
use crate::{
    master::{self, Manifest, MasterError},
    region::{Platform, Region},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
};
const MAX_JSON: u64 = 64 * 1024 * 1024;
const MAX_TOTAL: u64 = 512 * 1024 * 1024;
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
impl PublishedManifest {
    pub fn validate(&self, scope: &Scope) -> Result<(), MasterError> {
        if self.schema_version != 1
            || &self.scope != scope
            || scope.region != Region::Jp
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
fn source(directory: &Path) -> Result<Manifest, MasterError> {
    source_with_provenance(directory).map(|(source, _)| source)
}
fn source_with_provenance(directory: &Path) -> Result<(Manifest, Option<String>), MasterError> {
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
    Ok((source, resource_version))
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
                serde_json::from_slice::<serde_json::Value>(&bytes)
                    .map_err(|_| MasterError::Format)?;
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
    if scope.region != Region::Jp
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
    let (mut source, resource_version) = source_with_provenance(&directory)?;
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
    snapshot: &str,
    table: &str,
    expected_hash: &str,
) -> Result<Document, MasterError> {
    if !master::safe_component(table) || !hash_valid(expected_hash) {
        return Err(MasterError::NotFound);
    }
    let directory = snapshot_directory(root, snapshot)?;
    let source = source(&directory)?;
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
    verify_indexed(&directory, &source, table, &bytes)?;
    serde_json::from_slice::<serde_json::Value>(&bytes).map_err(|_| MasterError::Format)?;
    Ok(Document {
        etag: format!("\"{expected_hash}\""),
        version: source.version,
        bytes,
    })
}
/// Strengthens ordinary table reads as well; absent indexes preserve 1.1 compatibility.
pub(crate) fn verify_indexed(
    directory: &Path,
    source: &Manifest,
    table: &str,
    bytes: &[u8],
) -> Result<(), MasterError> {
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
            if entry.size != bytes.len() as u64 || entry.sha256 != digest(bytes) {
                return Err(MasterError::Integrity);
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
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
    source(&directory)?;
    Ok(Some(snapshot))
}

#[derive(Serialize)]
pub struct HistoryEntry {
    pub snapshot: String,
    pub version: String,
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
    if before.is_some_and(|v| !valid_history_cursor(v)) {
        return Err(MasterError::Format);
    }
    let head = predecessor(root)?.ok_or(MasterError::NotFound)?;
    let mut cursor_found = before.is_none();
    let mut next = Some(head.clone());
    let mut visited = std::collections::BTreeSet::new();
    let mut entries = Vec::new();
    let mut legacy_boundary = false;
    while let Some(snapshot) = next.take() {
        if visited.len() == 10_000 {
            return Err(MasterError::Limit);
        }
        if !visited.insert(snapshot.clone()) {
            return Err(MasterError::Format);
        }
        let directory = snapshot_directory(root, &snapshot)?;
        let doc = manifest(root, Some(&snapshot), scope.clone())?;
        let value: PublishedManifest =
            serde_json::from_slice(&doc.bytes).map_err(|_| MasterError::Format)?;
        let publication = publication(&directory, &snapshot, &visited)?;
        if publication.is_none() {
            legacy_boundary = true;
        }
        let published_at = publication.as_ref().map(|p| p.published_at);
        next = publication.and_then(|p| p.previous_snapshot);
        if !cursor_found {
            cursor_found = before == Some(snapshot.as_str());
            continue;
        }
        entries.push(HistoryEntry {
            snapshot,
            version: value.version,
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
    })
}

fn publication(
    directory: &Path,
    snapshot: &str,
    visited: &std::collections::BTreeSet<String>,
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
pub fn manifest_by_hash(root: &Path, scope: Scope, hash: &str) -> Result<Document, MasterError> {
    if !hash_valid(hash) {
        return Err(MasterError::Format);
    }
    let mut next = predecessor(root)?;
    let mut visited = std::collections::BTreeSet::new();
    while let Some(snapshot) = next.take() {
        if visited.len() == 10_000 {
            return Err(MasterError::Limit);
        }
        if !visited.insert(snapshot.clone()) {
            return Err(MasterError::Format);
        }
        let directory = snapshot_directory(root, &snapshot)?;
        let record = publication(&directory, &snapshot, &visited)?;
        let document = manifest(root, Some(&snapshot), scope.clone())?;
        let value: PublishedManifest =
            serde_json::from_slice(&document.bytes).map_err(|_| MasterError::Format)?;
        if value.content_sha256 == hash {
            return Ok(document);
        }
        next = record.and_then(|r| r.previous_snapshot);
    }
    Err(MasterError::NotFound)
}
