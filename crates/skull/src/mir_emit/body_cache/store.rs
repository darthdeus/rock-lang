//! The on-disk shard: one append-only file per (project, environment).
//!
//! A record is `[u32 length][u128 key][u64 checksum][payload]`. Readers take
//! the newest record per key and stop at the first record that does not fit
//! the file, so a writer that died mid-append costs its last record and
//! nothing else. Appends hold an exclusive lock, so two compiles of the same
//! project never interleave inside a record.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use rustc_hash::FxHashMap;

const MAGIC: &[u8; 8] = b"RKBODYC\x01";
const HEADER_LEN: usize = MAGIC.len() + 16;
const RECORD_HEADER_LEN: usize = 4 + 16 + 8;

/// Shards per project kept after a write; older environments are dropped.
const KEPT_SHARDS: usize = 4;

/// A project directory nothing wrote to for this long is dropped: test
/// projects and deleted checkouts leave their shards behind otherwise.
const PROJECT_TTL: std::time::Duration = std::time::Duration::from_secs(30 * 24 * 3600);

/// A loaded shard: the file's bytes and the newest record per key.
#[derive(Default)]
pub(super) struct Shard {
    data: Vec<u8>,
    /// key -> (payload start, payload length, checksum)
    index: FxHashMap<u128, (usize, usize, u64)>,
    live_bytes: usize,
}

impl Shard {
    pub(super) fn load(path: &Path, env: u128) -> std::io::Result<Self> {
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error),
        };
        // Appends by another compile land after this read's end; the shared
        // lock only keeps a compaction from swapping the file under it.
        file.lock_shared()?;
        let mut data = Vec::new();
        file.read_to_end(&mut data)?;
        let _ = file.unlock();
        let mut shard = Self::default();
        if data.len() < HEADER_LEN
            || &data[..MAGIC.len()] != MAGIC
            || data[MAGIC.len()..HEADER_LEN] != env.to_le_bytes()
        {
            return Ok(shard);
        }
        let mut pos = HEADER_LEN;
        while pos + RECORD_HEADER_LEN <= data.len() {
            let len = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
            let key = u128::from_le_bytes(data[pos + 4..pos + 20].try_into().unwrap());
            let checksum = u64::from_le_bytes(data[pos + 20..pos + 28].try_into().unwrap());
            let start = pos + RECORD_HEADER_LEN;
            let Some(end) = start.checked_add(len) else {
                break;
            };
            if end > data.len() {
                break;
            }
            if let Some((_, old_len, _)) = shard.index.insert(key, (start, len, checksum)) {
                shard.live_bytes -= old_len;
            }
            shard.live_bytes += len;
            pos = end;
        }
        shard.data = data;
        Ok(shard)
    }

    pub(super) fn len(&self) -> usize {
        self.index.len()
    }

    pub(super) fn file_bytes(&self) -> usize {
        self.data.len()
    }

    /// The payload stored under `key`, checksum verified.
    pub(super) fn get(&self, key: u128) -> Option<&[u8]> {
        let &(start, len, checksum) = self.index.get(&key)?;
        let payload = &self.data[start..start + len];
        (xxhash_rust::xxh3::xxh3_64(payload) == checksum).then_some(payload)
    }

    /// Append `records` (key, payload) to the shard at `path`, creating it
    /// for `env`. When dead records dominate the file, it is rewritten with
    /// only the live ones instead.
    pub(super) fn append(
        &self,
        path: &Path,
        env: u128,
        records: &[(u128, Vec<u8>)],
    ) -> std::io::Result<usize> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut encoded = Vec::new();
        for (key, payload) in records {
            encoded.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            encoded.extend_from_slice(&key.to_le_bytes());
            encoded.extend_from_slice(&xxhash_rust::xxh3::xxh3_64(payload).to_le_bytes());
            encoded.extend_from_slice(payload);
        }
        let dead = self.data.len().saturating_sub(self.live_bytes);
        if !self.data.is_empty() && dead > self.live_bytes.max(16 << 20) {
            return self.rewrite(path, env, &encoded);
        }
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        file.lock()?;
        let len = file.seek(SeekFrom::End(0))?;
        let mut written = 0;
        if len == 0 {
            let mut header = Vec::with_capacity(HEADER_LEN);
            header.extend_from_slice(MAGIC);
            header.extend_from_slice(&env.to_le_bytes());
            file.write_all(&header)?;
            written += header.len();
        }
        file.write_all(&encoded)?;
        written += encoded.len();
        let _ = file.unlock();
        Ok(written)
    }

    /// Replace the file with the live records followed by `encoded`.
    fn rewrite(&self, path: &Path, env: u128, encoded: &[u8]) -> std::io::Result<usize> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.live_bytes + encoded.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&env.to_le_bytes());
        let mut live: Vec<(u128, (usize, usize, u64))> =
            self.index.iter().map(|(k, v)| (*k, *v)).collect();
        live.sort_by_key(|(_, (start, _, _))| *start);
        for (key, (start, len, checksum)) in live {
            out.extend_from_slice(&(len as u32).to_le_bytes());
            out.extend_from_slice(&key.to_le_bytes());
            out.extend_from_slice(&checksum.to_le_bytes());
            out.extend_from_slice(&self.data[start..start + len]);
        }
        out.extend_from_slice(encoded);
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        let current = OpenOptions::new().create(true).append(true).open(path)?;
        current.lock()?;
        std::fs::write(&tmp, &out)?;
        std::fs::rename(&tmp, path)?;
        let _ = current.unlock();
        Ok(out.len())
    }
}

/// Drop every shard in `dir` but the newest `KEPT_SHARDS`, never `keep`.
pub(super) fn prune(dir: &Path, keep: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut shards: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if !path.extension().is_some_and(|e| e == "bin") || path == keep {
                return None;
            }
            let modified = entry.metadata().and_then(|m| m.modified()).ok()?;
            Some((modified, path))
        })
        .collect();
    shards.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    for (_, path) in shards.into_iter().skip(KEPT_SHARDS - 1) {
        let _ = std::fs::remove_file(path);
    }
}

/// Drop every project directory under `root` whose newest write is older
/// than [`PROJECT_TTL`], never `keep`.
pub(super) fn prune_projects(root: &Path, keep: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        if path == keep || !path.is_dir() {
            continue;
        }
        let newest = std::fs::read_dir(&path)
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|shard| shard.metadata().and_then(|m| m.modified()).ok())
            .max();
        let stale = match newest {
            Some(modified) => now
                .duration_since(modified)
                .is_ok_and(|age| age > PROJECT_TTL),
            None => true,
        };
        if stale {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}
