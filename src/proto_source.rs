//! Shared, deterministic source snapshot compiler used by build.rs and runtime.
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, io::Read, path::Path};
#[derive(Debug)]
pub struct SourceError;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: String,
}
pub struct Compiled {
    pub version: String,
    pub encoded: Vec<u8>,
    pub sha256: String,
    pub files: usize,
}
struct Sources(BTreeMap<String, String>);
impl protox::file::FileResolver for Sources {
    fn open_file(&self, name: &str) -> Result<protox::file::File, protox::Error> {
        let source = self
            .0
            .get(name)
            .ok_or_else(|| protox::Error::file_not_found(name))?;
        protox::file::File::from_source(name, source)
    }
}
fn invalid<T>() -> Result<T, SourceError> {
    Err(SourceError)
}
fn read(path: &Path, limit: u64) -> Result<String, SourceError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| SourceError)?;
    if !metadata.is_file() || metadata.len() > limit {
        return invalid();
    }
    let mut value = String::new();
    fs::File::open(path)
        .map_err(|_| SourceError)?
        .take(limit + 1)
        .read_to_string(&mut value)
        .map_err(|_| SourceError)?;
    if value.len() as u64 > limit {
        return invalid();
    }
    Ok(value)
}
fn collect(
    root: &Path,
    path: &Path,
    depth: usize,
    files: &mut BTreeMap<String, String>,
    total: &mut usize,
) -> Result<(), SourceError> {
    if depth > 16
        || !fs::symlink_metadata(path)
            .map_err(|_| SourceError)?
            .is_dir()
    {
        return invalid();
    }
    for entry in fs::read_dir(path).map_err(|_| SourceError)? {
        let entry = entry.map_err(|_| SourceError)?;
        let path = entry.path();
        let kind = entry.file_type().map_err(|_| SourceError)?;
        if kind.is_symlink() {
            return invalid();
        }
        if kind.is_dir() {
            collect(root, &path, depth + 1, files, total)?;
        } else if path.extension().is_some_and(|x| x == "proto") {
            if files.len() >= 512 {
                return invalid();
            }
            let name = path
                .strip_prefix(root)
                .ok()
                .and_then(|p| p.to_str())
                .ok_or(SourceError)?
                .replace('\\', "/");
            let source = read(&path, 4 * 1024 * 1024)?;
            *total += source.len();
            if *total > 32 * 1024 * 1024 {
                return invalid();
            }
            files.insert(name, source);
        }
    }
    Ok(())
}
pub fn compile(directory: &Path) -> Result<Compiled, SourceError> {
    // Resolve a deployment symlink once. Deploy complete immutable directories.
    let directory = fs::canonicalize(directory).map_err(|_| SourceError)?;
    let manifest: Manifest = serde_json::from_str(&read(&directory.join("bundle.json"), 4096)?)
        .map_err(|_| SourceError)?;
    semver::Version::parse(&manifest.version).map_err(|_| SourceError)?;
    let root = directory.join("proto");
    let mut files = BTreeMap::new();
    collect(&root, &root, 0, &mut files, &mut 0)?;
    if files.is_empty() {
        return invalid();
    }
    let names = files.keys().cloned().collect::<Vec<_>>();
    let mut compiler = protox::Compiler::with_file_resolver(Sources(files));
    compiler
        .include_imports(true)
        .include_source_info(false)
        .open_files(&names)
        .map_err(|_| SourceError)?;
    let encoded = compiler.encode_file_descriptor_set();
    let mut hash = Sha256::new();
    hash.update(manifest.version.as_bytes());
    hash.update([0]);
    hash.update(&encoded);
    Ok(Compiled {
        version: manifest.version,
        encoded,
        sha256: format!("{:x}", hash.finalize()),
        files: names.len(),
    })
}
