//! Host `Storage`: an opaque-blob key/value store backed by a single JSON
//! file on disk (a JSON file under a data directory, `create_dir_all` on
//! first write, `serde_json` for the on-disk format). The core `Storage`
//! trait is `get(key) -> Option<Vec<u8>>` / `set(key, value)` — an opaque
//! byte-blob KV store with no domain-specific shape — so any caller (e.g.
//! `pico_link_core::power::IdlePowerSetting`) can persist its own small values
//! under its own key without this type knowing anything about them.
//! Defaults to `./data/kv_store.json`.
//!
//! Known limitation (deferred, not a correctness issue): `Vec<u8>` values
//! serialize through `serde_json` as JSON arrays of numbers, not a compact
//! binary encoding. That is fine for the small blobs this project persists
//! today; if that changes, swapping the on-disk codec (e.g. to a binary
//! CBOR/bincode encoding) is a change local to `persist`/`load` below, not
//! to the `Storage` trait or its callers.

use pico_link_core::platform::Storage;
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;

#[derive(Debug)]
pub enum FileStorageError {
    Io(io::Error),
    Serde(serde_json::Error),
}

impl fmt::Display for FileStorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FileStorageError::Io(e) => write!(f, "storage I/O error: {e}"),
            FileStorageError::Serde(e) => write!(f, "storage (de)serialization error: {e}"),
        }
    }
}

impl std::error::Error for FileStorageError {}

impl From<io::Error> for FileStorageError {
    fn from(e: io::Error) -> Self {
        FileStorageError::Io(e)
    }
}

impl From<serde_json::Error> for FileStorageError {
    fn from(e: serde_json::Error) -> Self {
        FileStorageError::Serde(e)
    }
}

/// File-backed opaque-blob key/value store: the host `pico_link_core::platform::
/// Storage` implementation. Loads eagerly at construction, keeps an
/// in-memory `HashMap` as the read path (`get` never touches disk), and
/// persists the whole map back to disk on every `set` (fine for this
/// project's write frequency and data size; not designed for high-frequency
/// writes or large blobs).
pub struct FileStorage {
    file_path: PathBuf,
    entries: HashMap<String, Vec<u8>>,
}

impl FileStorage {
    /// Opens (or creates) the KV store backed by `file_path`. Creates the
    /// parent directory if it doesn't exist. If `file_path` doesn't exist
    /// yet, starts from an empty map.
    ///
    /// # Errors
    ///
    /// Returns `FileStorageError::Io` if the parent directory couldn't be
    /// created or the existing file couldn't be read, or
    /// `FileStorageError::Serde` if the existing file's contents aren't
    /// valid JSON for the expected shape.
    pub fn new(file_path: impl Into<PathBuf>) -> Result<Self, FileStorageError> {
        let file_path = file_path.into();

        if let Some(parent) = file_path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }

        let entries = if file_path.exists() {
            let contents = fs::read_to_string(&file_path)?;
            serde_json::from_str(&contents)?
        } else {
            HashMap::new()
        };

        Ok(Self { file_path, entries })
    }

    /// Default on-disk location for the host emulator:
    /// `./data/kv_store.json`.
    ///
    /// # Errors
    ///
    /// See [`FileStorage::new`].
    pub fn new_default() -> Result<Self, FileStorageError> {
        Self::new("./data/kv_store.json")
    }

    fn persist(&self) -> Result<(), FileStorageError> {
        let json = serde_json::to_string_pretty(&self.entries)?;
        fs::write(&self.file_path, json)?;
        Ok(())
    }
}

impl Storage for FileStorage {
    type Error = FileStorageError;

    fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.entries.get(key).cloned()
    }

    fn set(&mut self, key: &str, value: Vec<u8>) -> Result<(), Self::Error> {
        self.entries.insert(key.to_string(), value);
        self.persist()
    }

    /// Overrides `Storage::remove`'s default ("overwrite with an empty
    /// blob") to actually drop the map entry, so a caller that wants a key
    /// gone doesn't leave a phantom empty-array entry in `kv_store.json`
    /// forever.
    fn remove(&mut self, key: &str) -> Result<(), Self::Error> {
        self.entries.remove(key);
        self.persist()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "pico-link-emulator-file-storage-test-{name}-{}.json",
            uuid::Uuid::new_v4()
        ));
        path
    }

    #[test]
    fn get_on_missing_key_returns_none() {
        let path = temp_path("missing-key");
        let storage = FileStorage::new(&path).unwrap();
        assert_eq!(storage.get("nope"), None);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn set_then_get_round_trips_the_exact_bytes() {
        let path = temp_path("round-trip");
        let mut storage = FileStorage::new(&path).unwrap();
        storage.set("token", vec![1, 2, 3, 255, 0]).unwrap();
        assert_eq!(storage.get("token"), Some(vec![1, 2, 3, 255, 0]));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn a_second_instance_opened_on_the_same_path_sees_persisted_data() {
        let path = temp_path("persistence");
        {
            let mut storage = FileStorage::new(&path).unwrap();
            storage.set("greeting", b"hello".to_vec()).unwrap();
        }
        let reopened = FileStorage::new(&path).unwrap();
        assert_eq!(reopened.get("greeting"), Some(b"hello".to_vec()));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn set_overwrites_an_existing_key() {
        let path = temp_path("overwrite");
        let mut storage = FileStorage::new(&path).unwrap();
        storage.set("k", vec![1]).unwrap();
        storage.set("k", vec![2, 2]).unwrap();
        assert_eq!(storage.get("k"), Some(vec![2, 2]));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn remove_drops_the_key_entirely_not_just_its_value() {
        let path = temp_path("remove");
        let mut storage = FileStorage::new(&path).unwrap();
        storage.set("k", vec![1, 2, 3]).unwrap();

        storage.remove("k").unwrap();

        assert_eq!(storage.get("k"), None);
        // And it stays gone across a reopen, not just in the in-memory cache.
        let reopened = FileStorage::new(&path).unwrap();
        assert_eq!(reopened.get("k"), None);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn remove_on_a_missing_key_is_a_harmless_no_op() {
        let path = temp_path("remove-missing");
        let mut storage = FileStorage::new(&path).unwrap();

        storage.remove("nope").unwrap();

        assert_eq!(storage.get("nope"), None);
        fs::remove_file(&path).ok();
    }
}
