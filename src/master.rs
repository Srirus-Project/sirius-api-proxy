//! Validated Master files and immutable JSON snapshots. No database models.
use crate::rijndael::Rijndael256;
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Write},
    path::Path,
};

pub(crate) const MAX_MANIFEST: u64 = 1024 * 1024;
const MAX_ENCRYPTED: u64 = 32 * 1024 * 1024;
const MAX_JSON: u64 = 64 * 1024 * 1024;
const MAX_TOTAL: u64 = 512 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum MasterError {
    #[error("another Master writer is active")]
    Busy,
    #[error("Master table not found")]
    NotFound,
    #[error("Master file I/O failed")]
    Io(#[from] std::io::Error),
    #[error("invalid Master manifest or JSON")]
    Format,
    #[error("Master size limit exceeded")]
    Limit,
    #[error("Master encrypted file hash or size mismatch")]
    Integrity,
    #[error("Master decryption failed")]
    Cipher,
    #[error("Master key and IV must each be 64 hex characters")]
    Key,
}

pub(crate) struct WriterLock {
    _file: fs::File,
}
impl WriterLock {
    pub(crate) fn acquire(directory: &Path) -> Result<Self, MasterError> {
        fs::create_dir_all(directory)?;
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(directory.join(".writer.lock"))?;
        file.try_lock().map_err(|_| MasterError::Busy)?;
        Ok(Self { _file: file })
    }
}

pub struct MasterDocument {
    pub version: String,
    pub bytes: Vec<u8>,
}

/// Pin CURRENT once, so concurrent imports cannot mix two versions in one read.
pub fn read_current(directory: &Path, table: Option<&str>) -> Result<MasterDocument, MasterError> {
    let pointer = read_bounded(&directory.join("CURRENT"), 128)?;
    let snapshot = std::str::from_utf8(&pointer).map_err(|_| MasterError::Format)?;
    if !snapshot.starts_with("master-") || !safe_component(snapshot) {
        return Err(MasterError::Format);
    }
    let directory = directory.join(snapshot);
    let manifest = Manifest::parse(&read_bounded(
        &directory.join("MasterManifest.json"),
        MAX_MANIFEST,
    )?)?;
    let bytes = if let Some(table) = table {
        if !safe_component(table)
            || !manifest
                .files
                .iter()
                .any(|f| f.name == format!("{table}.bin"))
        {
            return Err(MasterError::NotFound);
        }
        let bytes = read_bounded(&directory.join(format!("{table}.json")), MAX_JSON)?;
        crate::master_registry::verify_indexed(&directory, &manifest, table, &bytes)?;
        bytes
    } else {
        let receipt: serde_json::Value =
            serde_json::from_slice(&read_bounded(&directory.join("receipt.json"), 4096)?)
                .map_err(|_| MasterError::Format)?;
        let source = receipt
            .get("source")
            .and_then(serde_json::Value::as_str)
            .filter(|source| matches!(*source, "local-import" | "remote" | "registry"))
            .ok_or(MasterError::Format)?;
        if receipt["version"] != manifest.version || receipt["snapshot"] != snapshot {
            return Err(MasterError::Format);
        }
        serde_json::to_vec(&serde_json::json!({
            "version":manifest.version, "snapshot":snapshot,
            "source":source,
            "tables":manifest.files.iter().map(|f|f.name.trim_end_matches(".bin")).collect::<Vec<_>>()
        })).map_err(|_|MasterError::Format)?
    };
    Ok(MasterDocument {
        version: manifest.version,
        bytes,
    })
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub version: String,
    pub files: Vec<Entry>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub name: String,
    pub hash: String,
    pub size: u64,
}
#[derive(Clone, Serialize)]
pub struct ImportReceipt {
    pub version: String,
    pub snapshot: String,
    pub tables: usize,
    pub json_bytes: u64,
    pub source: &'static str,
}
pub fn key_from_hex(value: &str) -> Result<[u8; 32], MasterError> {
    if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(MasterError::Key);
    }
    let mut key = [0; 32];
    for (i, b) in key.iter_mut().enumerate() {
        *b = u8::from_str_radix(&value[i * 2..i * 2 + 2], 16).map_err(|_| MasterError::Key)?;
    }
    Ok(key)
}
pub(crate) fn safe_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
// Master versions may contain a version/hash pair, but never arbitrary paths.
pub(crate) fn safe_version(value: &str) -> bool {
    let mut parts = value.split('/');
    let version = parts.next().unwrap_or_default();
    let hash = parts.next();
    !version.is_empty()
        && version.len() <= 128
        && value.len() <= 256
        && !matches!(version, "." | "..")
        && version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        && hash.is_none_or(|h| h.len() == 32 && h.bytes().all(|b| b.is_ascii_hexdigit()))
        && parts.next().is_none()
}
pub fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>, MasterError> {
    let file = fs::File::open(path)?;
    if file.metadata()?.len() > limit {
        return Err(MasterError::Limit);
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(MasterError::Limit);
    }
    Ok(bytes)
}
impl Manifest {
    pub fn parse(bytes: &[u8]) -> Result<Self, MasterError> {
        if bytes.len() as u64 > MAX_MANIFEST {
            return Err(MasterError::Limit);
        }
        let manifest: Self = serde_json::from_slice(bytes).map_err(|_| MasterError::Format)?;
        if !safe_version(&manifest.version)
            || manifest.files.is_empty()
            || manifest.files.len() > 4096
        {
            return Err(MasterError::Format);
        }
        let mut names = BTreeSet::new();
        let mut total = 0;
        for entry in &manifest.files {
            let stem = entry.name.strip_suffix(".bin").ok_or(MasterError::Format)?;
            if stem == "MasterManifest"
                || !stem.starts_with("Master")
                || !safe_component(stem)
                || !names.insert(&entry.name)
                || entry.hash.len() != 64
                || !entry.hash.bytes().all(|b| b.is_ascii_hexdigit())
                || entry.size <= 64
                || entry.size > MAX_ENCRYPTED
                || entry.size % 32 != 0
            {
                return Err(MasterError::Format);
            }
            total += entry.size;
        }
        if total > MAX_TOTAL {
            return Err(MasterError::Limit);
        }
        Ok(manifest)
    }
}
pub struct MasterDecoder {
    cipher: Rijndael256,
    iv: [u8; 32],
}
impl MasterDecoder {
    pub fn new(key: &[u8; 32], iv: [u8; 32]) -> Self {
        Self {
            cipher: Rijndael256::new(key),
            iv,
        }
    }
    pub fn decode(&self, entry: &Entry, data: &[u8]) -> Result<Vec<u8>, MasterError> {
        if data.len() as u64 > MAX_ENCRYPTED {
            return Err(MasterError::Limit);
        }
        if data.len() as u64 != entry.size
            || !format!("{:x}", Sha256::digest(data)).eq_ignore_ascii_case(&entry.hash)
        {
            return Err(MasterError::Integrity);
        }
        let ciphertext = data.get(32..).ok_or(MasterError::Cipher)?;
        let plaintext = self.cipher.decrypt(ciphertext, &self.iv)?;
        let compressed = plaintext.get(32..).ok_or(MasterError::Cipher)?;
        let mut json = Vec::new();
        GzDecoder::new(compressed)
            .take(MAX_JSON + 1)
            .read_to_end(&mut json)
            .map_err(|_| MasterError::Cipher)?;
        if json.len() as u64 > MAX_JSON {
            return Err(MasterError::Limit);
        }
        // Validate without rewriting numbers or the original table schema.
        serde_json::from_slice::<serde_json::Value>(&json).map_err(|_| MasterError::Format)?;
        Ok(json)
    }
}
fn write_synced(path: &Path, bytes: &[u8]) -> Result<(), MasterError> {
    let mut file = fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
// Unix supports fsync on directory handles. Windows File::open cannot open a
// directory as a normal file. Every data file and CURRENT is still synced
// through its writable handle before atomic publication on all platforms.
fn sync_directory(path: &Path) -> Result<(), MasterError> {
    #[cfg(unix)]
    fs::File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
/// Import the client bundled/encrypted directory. Publish only after every table
/// passes size, SHA-256, cipher padding, gzip and JSON validation.
pub fn import_directory(
    input: &Path,
    output: &Path,
    decoder: &MasterDecoder,
) -> Result<ImportReceipt, MasterError> {
    let _lock = WriterLock::acquire(output)?;
    prepare_directory(input, output, decoder, "local-import")?.publish(output)
}

pub(crate) struct PreparedImport {
    staging: tempfile::TempDir,
    receipt: ImportReceipt,
}

pub(crate) fn prepare_directory(
    input: &Path,
    output: &Path,
    decoder: &MasterDecoder,
    source: &'static str,
) -> Result<PreparedImport, MasterError> {
    let manifest_bytes = read_bounded(&input.join("MasterManifest.json"), MAX_MANIFEST)?;
    let manifest = Manifest::parse(&manifest_bytes)?;
    fs::create_dir_all(output)?;
    let staging = tempfile::Builder::new()
        .prefix(".master-stage-")
        .tempdir_in(output)?;
    let mut total = 0;
    let mut indexed_files = Vec::new();
    for entry in &manifest.files {
        let bytes = read_bounded(&input.join(&entry.name), MAX_ENCRYPTED)?;
        let json = decoder.decode(entry, &bytes)?;
        total += json.len() as u64;
        if total > MAX_TOTAL {
            return Err(MasterError::Limit);
        }
        write_synced(
            &staging.path().join(entry.name.replace(".bin", ".json")),
            &json,
        )?;
        indexed_files.push(crate::master_registry::file(
            entry.name.replace(".bin", ".json"),
            &json,
        ));
    }
    indexed_files.sort_by(|a, b| a.name.cmp(&b.name));
    let index = crate::master_registry::Inventory {
        schema_version: 1,
        version: manifest.version.clone(),
        files: indexed_files,
    };
    index.validate(&manifest)?;
    write_synced(
        &staging.path().join("tables.json"),
        &serde_json::to_vec(&index).map_err(|_| MasterError::Format)?,
    )?;
    write_synced(&staging.path().join("MasterManifest.json"), &manifest_bytes)?;
    let snapshot = format!("master-{}", uuid::Uuid::new_v4().simple());
    let receipt = ImportReceipt {
        version: manifest.version,
        snapshot: snapshot.clone(),
        tables: manifest.files.len(),
        json_bytes: total,
        source,
    };
    write_synced(
        &staging.path().join("receipt.json"),
        &serde_json::to_vec(&receipt).map_err(|_| MasterError::Format)?,
    )?;
    sync_directory(staging.path())?;
    Ok(PreparedImport { staging, receipt })
}

impl PreparedImport {
    pub(crate) fn publish(self, output: &Path) -> Result<ImportReceipt, MasterError> {
        let snapshot = &self.receipt.snapshot;
        let publication = crate::master_registry::Publication {
            schema_version: 1,
            snapshot: snapshot.clone(),
            previous_snapshot: crate::master_registry::predecessor(output)?,
            published_at: chrono::Utc::now(),
        };
        write_synced(
            &self.staging.path().join("publication.json"),
            &serde_json::to_vec(&publication).map_err(|_| MasterError::Format)?,
        )?;
        sync_directory(self.staging.path())?;
        fs::rename(self.staging.path(), output.join(snapshot))?;
        sync_directory(output)?;
        let mut pointer = tempfile::NamedTempFile::new_in(output)?;
        pointer.write_all(snapshot.as_bytes())?;
        pointer.as_file().sync_all()?;
        pointer
            .persist(output.join("CURRENT"))
            .map_err(|err| MasterError::Io(err.error))?;
        sync_directory(output)?;
        Ok(self.receipt)
    }
}

/// Validate staged plaintext from a pinned owner; never publish from the blocking worker.
pub(crate) fn prepare_registry(
    staging: tempfile::TempDir,
    manifest: &crate::master_registry::PublishedManifest,
) -> Result<PreparedImport, MasterError> {
    manifest.validate(&manifest.scope)?;
    let mut total = 0;
    for entry in &manifest.files {
        let path = staging.path().join(&entry.name);
        if !fs::symlink_metadata(&path)?.is_file() {
            return Err(MasterError::Format);
        }
        let bytes = read_bounded(&path, entry.size)?;
        if bytes.len() as u64 != entry.size
            || crate::master_registry::digest(&bytes) != entry.sha256
        {
            return Err(MasterError::Integrity);
        }
        serde_json::from_slice::<serde_json::Value>(&bytes).map_err(|_| MasterError::Format)?;
        fs::File::open(path)?.sync_all()?;
        total += entry.size;
    }
    let index = crate::master_registry::Inventory {
        schema_version: 1,
        version: manifest.version.clone(),
        files: manifest.files.clone(),
    };
    write_synced(
        &staging.path().join("tables.json"),
        &serde_json::to_vec(&index).map_err(|_| MasterError::Format)?,
    )?;
    write_synced(
        &staging.path().join("MasterManifest.json"),
        &serde_json::to_vec(&manifest.source_manifest).map_err(|_| MasterError::Format)?,
    )?;
    let receipt = ImportReceipt {
        version: manifest.version.clone(),
        snapshot: format!("master-{}", uuid::Uuid::new_v4().simple()),
        tables: manifest.files.len(),
        json_bytes: total,
        source: "registry",
    };
    write_synced(
        &staging.path().join("receipt.json"),
        &serde_json::to_vec(&receipt).map_err(|_| MasterError::Format)?,
    )?;
    sync_directory(staging.path())?;
    Ok(PreparedImport { staging, receipt })
}
