//! Single-owner durable dispatch identities. Network work must follow successful persistence.
use crate::{asset_jobs::Request, region::Region};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid asset dispatch identity or transition")]
    Invalid,
    #[error("asset dispatch state is already owned")]
    Locked,
    #[error("asset dispatch state could not be persisted or read")]
    Storage,
    #[error("asset dispatch history is full")]
    Full,
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    /// SHA-256 of the normalized configured destination; no credentials.
    pub destination_sha256: String,
    pub request: Request,
    pub profile_revision: String,
    pub environment: String,
    pub platform: String,
    pub resource_version: String,
    pub platform_hash: String,
    pub require_full_catalog: bool,
    pub require_full_export: bool,
    pub require_publication: bool,
}
impl Identity {
    pub fn key(&self) -> Result<String, Error> {
        if !digest(&self.destination_sha256)
            || self.request.region == Region::Cn
            || self.request.profile.len() > 64
            || !component(&self.request.profile)
            || self.request.profile.contains('.')
            || [
                &self.profile_revision,
                &self.environment,
                &self.resource_version,
                &self.platform_hash,
            ]
            .iter()
            .any(|v| !component(v))
            || !matches!(self.platform.as_str(), "iOS" | "Android")
        {
            return Err(Error::Invalid);
        }
        let bytes = serde_json::to_vec(&(1u8, self)).map_err(|_| Error::Invalid)?;
        Ok(format!("sirius-{:x}", Sha256::digest(bytes)))
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum State {
    Pending,
    /// Persisted before POST. A crash here leaves acceptance ambiguous.
    Sending {
        first_attempt_at: chrono::DateTime<chrono::Utc>,
    },
    Submitted {
        job_id: String,
    },
    Completed {
        job_id: String,
        catalog_sha256: String,
        publication_id: Option<String>,
    },
    /// Keep identity reserved; do not silently resubmit lost/pruned/failed work.
    Failed {
        job_id: Option<String>,
        code: String,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub identity: Identity,
    pub state: State,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    schema_version: u8,
    #[serde(default)]
    reconciliation_cursor: Option<String>,
    entries: BTreeMap<String, Entry>,
}
pub struct Outbox {
    directory: PathBuf,
    _owner: crate::file_lock::Exclusive,
    ledger: Ledger,
    capacity: usize,
}
impl Outbox {
    pub fn open(directory: &Path, capacity: usize) -> Result<Self, Error> {
        if !(1..=100_000).contains(&capacity) {
            return Err(Error::Invalid);
        }
        fs::create_dir_all(directory).map_err(|_| Error::Storage)?;
        let owner = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(directory.join("owner.lock"))
            .map_err(|_| Error::Storage)?;
        let owner = crate::file_lock::Exclusive::acquire(owner).map_err(|_| Error::Locked)?;
        let path = directory.join("outbox.json");
        let ledger: Ledger = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|_| Error::Storage)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ledger {
                schema_version: 1,
                reconciliation_cursor: None,
                entries: BTreeMap::new(),
            },
            Err(_) => return Err(Error::Storage),
        };
        if ledger.schema_version != 1
            || ledger.entries.len() > capacity
            || ledger
                .reconciliation_cursor
                .as_ref()
                .is_some_and(|key| !ledger.entries.contains_key(key))
        {
            return Err(Error::Storage);
        }
        for (key, entry) in &ledger.entries {
            if entry.identity.key().map_err(|_| Error::Storage)? != *key
                || !valid_state(&entry.state)
            {
                return Err(Error::Storage);
            }
        }
        let mut result = Self {
            directory: directory.into(),
            _owner: owner,
            ledger,
            capacity,
        };
        result.commit(result.ledger.clone())?;
        Ok(result)
    }
    pub fn entries(&self) -> &BTreeMap<String, Entry> {
        &self.ledger.entries
    }
    /// Read one archived completion without loading unbounded history into the working ledger.
    pub fn archived(&self, key: &str) -> Result<Option<Entry>, Error> {
        if !valid_key(key) {
            return Err(Error::Invalid);
        }
        let directory = self.directory.join("completed");
        match fs::symlink_metadata(&directory) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Ok(m) if m.is_dir() && !m.file_type().is_symlink() => {}
            _ => return Err(Error::Storage),
        }
        let path = directory.join(format!("{key}.json"));
        let metadata = match fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Ok(m) if m.is_file() && !m.file_type().is_symlink() && m.len() <= 65536 => m,
            _ => return Err(Error::Storage),
        };
        let mut bytes = Vec::new();
        File::open(&path)
            .map_err(|_| Error::Storage)?
            .take(65537)
            .read_to_end(&mut bytes)
            .map_err(|_| Error::Storage)?;
        if bytes.len() as u64 != metadata.len() {
            return Err(Error::Storage);
        }
        let entry: Entry = serde_json::from_slice(&bytes).map_err(|_| Error::Storage)?;
        if entry.identity.key().map_err(|_| Error::Storage)? != key
            || !valid_state(&entry.state)
            || !matches!(entry.state, State::Completed { .. })
        {
            return Err(Error::Storage);
        }
        Ok(Some(entry))
    }
    /// Archive only a confirmed completion; its permanent identity prevents replay after compaction.
    /// The archive is durable before the working ledger forgets the entry. Retry is idempotent.
    pub fn archive_completed(&mut self, key: &str, expected_job_id: &str) -> Result<Entry, Error> {
        if !valid_key(key) || !uuid(expected_job_id) {
            return Err(Error::Invalid);
        }
        let archived = self.archived(key)?;
        let entry = self
            .ledger
            .entries
            .get(key)
            .cloned()
            .or_else(|| archived.clone())
            .ok_or(Error::Invalid)?;
        if !matches!(&entry.state, State::Completed {job_id, ..} if job_id == expected_job_id) {
            return Err(Error::Invalid);
        }
        if archived.as_ref().is_some_and(|old| old != &entry) {
            return Err(Error::Storage);
        }
        let directory = self.directory.join("completed");
        if archived.is_none() {
            fs::create_dir_all(&directory).map_err(|_| Error::Storage)?;
            let metadata = fs::symlink_metadata(&directory).map_err(|_| Error::Storage)?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(Error::Storage);
            }
            let bytes = serde_json::to_vec(&entry).map_err(|_| Error::Storage)?;
            let mut file =
                tempfile::NamedTempFile::new_in(&directory).map_err(|_| Error::Storage)?;
            file.write_all(&bytes).map_err(|_| Error::Storage)?;
            file.as_file().sync_all().map_err(|_| Error::Storage)?;
            file.persist_noclobber(directory.join(format!("{key}.json")))
                .map_err(|_| Error::Storage)?;
        }
        // Also synchronize an existing archive on retry after interrupted persistence.
        File::open(directory.join(format!("{key}.json")))
            .map_err(|_| Error::Storage)?
            .sync_all()
            .map_err(|_| Error::Storage)?;
        #[cfg(unix)]
        {
            File::open(&directory)
                .map_err(|_| Error::Storage)?
                .sync_all()
                .map_err(|_| Error::Storage)?;
            File::open(&self.directory)
                .map_err(|_| Error::Storage)?
                .sync_all()
                .map_err(|_| Error::Storage)?;
        }
        if self.ledger.entries.contains_key(key) {
            let mut ledger = self.ledger.clone();
            ledger.entries.remove(key);
            if ledger.reconciliation_cursor.as_deref() == Some(key) {
                ledger.reconciliation_cursor = None;
            }
            self.commit(ledger)?;
        }
        Ok(entry)
    }
    /// Rotate across nonterminal work. Persist selection before any network side effect so
    /// restarting cannot repeatedly favor the same blocked prefix of the ledger.
    pub fn next_batch(&mut self, limit: usize) -> Result<Vec<(String, Entry)>, Error> {
        if !(1..=256).contains(&limit) {
            return Err(Error::Invalid);
        }
        let cursor = self.ledger.reconciliation_cursor.as_ref();
        let after = self
            .ledger
            .entries
            .iter()
            .filter(|(key, _)| cursor.is_none_or(|cursor| *key > cursor));
        let before = self
            .ledger
            .entries
            .iter()
            .filter(|(key, _)| cursor.is_some_and(|cursor| *key <= cursor));
        let batch: Vec<_> = after
            .chain(before)
            .filter(|(_, entry)| {
                !matches!(entry.state, State::Completed { .. } | State::Failed { .. })
            })
            .take(limit)
            .map(|(key, entry)| (key.clone(), entry.clone()))
            .collect();
        if let Some((key, _)) = batch.last() {
            let mut ledger = self.ledger.clone();
            ledger.reconciliation_cursor = Some(key.clone());
            self.commit(ledger)?;
        }
        Ok(batch)
    }
    /// Reobserving any known identity preserves its state, including terminal failures.
    pub fn observe(&mut self, identity: Identity) -> Result<String, Error> {
        let key = identity.key()?;
        if self.ledger.entries.contains_key(&key) {
            return Ok(key);
        }
        if self.archived(&key)?.is_some() {
            return Ok(key);
        }
        if self.ledger.entries.len() >= self.capacity {
            return Err(Error::Full);
        }
        let mut ledger = self.ledger.clone();
        ledger.entries.insert(
            key.clone(),
            Entry {
                identity,
                state: State::Pending,
            },
        );
        self.commit(ledger)?;
        Ok(key)
    }
    /// Record possible network side effects before sending. Never reset this timestamp on retries.
    pub fn begin_send(&mut self, key: &str) -> Result<(), Error> {
        self.change(key, |state| match state {
            State::Pending => Ok(State::Sending {
                first_attempt_at: chrono::Utc::now(),
            }),
            State::Sending { .. } => Ok(state.clone()),
            _ => Err(Error::Invalid),
        })
    }
    pub fn acknowledge(&mut self, key: &str, id: &str) -> Result<(), Error> {
        if !uuid(id) {
            return Err(Error::Invalid);
        }
        self.change(key, |state| match state {
            State::Sending { .. } => Ok(State::Submitted { job_id: id.into() }),
            State::Submitted { job_id } if job_id == id => Ok(state.clone()),
            _ => Err(Error::Invalid),
        })
    }
    /// Operator recovery only: adopt existing work, never create or claim completion.
    pub fn adopt(&mut self, key: &str, id: &str) -> Result<(), Error> {
        if !uuid(id) {
            return Err(Error::Invalid);
        }
        self.change(key, |state| match state {
            State::Sending { .. } => Ok(State::Submitted { job_id: id.into() }),
            State::Failed { job_id: None, code }
                if matches!(
                    code.as_str(),
                    "submission_ambiguous" | "invalid_job_response"
                ) =>
            {
                Ok(State::Submitted { job_id: id.into() })
            }
            State::Submitted { job_id } if job_id == id => Ok(state.clone()),
            _ => Err(Error::Invalid),
        })
    }
    pub fn complete(
        &mut self,
        key: &str,
        id: &str,
        catalog_sha256: &str,
        publication_id: Option<String>,
    ) -> Result<(), Error> {
        if !digest(catalog_sha256) || publication_id.as_ref().is_some_and(|v| !uuid(v)) {
            return Err(Error::Invalid);
        }
        let target = State::Completed {
            job_id: id.into(),
            catalog_sha256: catalog_sha256.into(),
            publication_id,
        };
        self.change(key, |state| match state {
            State::Submitted { job_id } if job_id == id => Ok(target.clone()),
            _ if state == &target => Ok(state.clone()),
            _ => Err(Error::Invalid),
        })
    }
    pub fn fail(&mut self, key: &str, code: &str) -> Result<(), Error> {
        if code.is_empty()
            || code.len() > 64
            || !code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
        {
            return Err(Error::Invalid);
        }
        self.change(key, |state| match state {
            State::Pending | State::Sending { .. } => Ok(State::Failed {
                job_id: None,
                code: code.into(),
            }),
            State::Submitted { job_id } => Ok(State::Failed {
                job_id: Some(job_id.clone()),
                code: code.into(),
            }),
            _ => Err(Error::Invalid),
        })
    }
    fn change(
        &mut self,
        key: &str,
        change: impl FnOnce(&State) -> Result<State, Error>,
    ) -> Result<(), Error> {
        let mut ledger = self.ledger.clone();
        let entry = ledger.entries.get_mut(key).ok_or(Error::Invalid)?;
        entry.state = change(&entry.state)?;
        self.commit(ledger)
    }
    fn commit(&mut self, ledger: Ledger) -> Result<(), Error> {
        let bytes = serde_json::to_vec(&ledger).map_err(|_| Error::Storage)?;
        let mut file =
            tempfile::NamedTempFile::new_in(&self.directory).map_err(|_| Error::Storage)?;
        file.write_all(&bytes).map_err(|_| Error::Storage)?;
        file.as_file().sync_all().map_err(|_| Error::Storage)?;
        file.persist(self.directory.join("outbox.json"))
            .map_err(|_| Error::Storage)?;
        self.ledger = ledger;
        Ok(())
    }
}
fn uuid(v: &str) -> bool {
    uuid::Uuid::parse_str(v).is_ok_and(|id| id.to_string() == v)
}
fn digest(v: &str) -> bool {
    v.len() == 64
        && v.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn component(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 256
        && !matches!(v, "." | "..")
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}
fn valid_state(state: &State) -> bool {
    match state {
        State::Pending | State::Sending { .. } => true,
        State::Submitted { job_id } => uuid(job_id),
        State::Completed {
            job_id,
            catalog_sha256,
            publication_id,
        } => {
            uuid(job_id)
                && digest(catalog_sha256)
                && publication_id.as_ref().is_none_or(|v| uuid(v))
        }
        State::Failed { job_id, code } => {
            job_id.as_ref().is_none_or(|v| uuid(v))
                && !code.is_empty()
                && code.len() <= 64
                && code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
        }
    }
}

fn valid_key(key: &str) -> bool {
    key.len() == 71 && key.starts_with("sirius-") && digest(&key[7..])
}
