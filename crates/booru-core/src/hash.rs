use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use imagehash::{average_hash, difference_hash, perceptual_hash};
use rayon::prelude::*;
use rusqlite::{params, Connection};
use xdg::BaseDirectories;

use crate::error::BooruError;
use crate::scan::ImageItem;

#[derive(Clone, Copy, Debug)]
pub enum FuzzyHashAlgorithm {
    AHash,
    DHash,
    PHash,
}

#[derive(Clone, Debug)]
pub struct FuzzyHash {
    pub algo: FuzzyHashAlgorithm,
    pub bits: Vec<bool>,
}

impl FuzzyHash {
    pub fn distance(&self, other: &FuzzyHash) -> u32 {
        let min_len = self.bits.len().min(other.bits.len());
        let mut diff = 0u32;
        for idx in 0..min_len {
            if self.bits[idx] != other.bits[idx] {
                diff += 1;
            }
        }
        diff + (self.bits.len().max(other.bits.len()) - min_len) as u32
    }
}

#[derive(Debug)]
pub struct DuplicateGroup {
    pub items: Vec<usize>,
}

#[derive(Debug)]
pub struct DuplicateWarning {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug)]
pub struct DuplicateReport {
    pub groups: Vec<DuplicateGroup>,
    pub warnings: Vec<DuplicateWarning>,
}

pub trait ProgressObserver: Send + Sync {
    fn inc(&self, delta: u64);
}

#[derive(Clone, Debug)]
pub struct FileFingerprint {
    pub mtime: i64,
    pub size: i64,
}

impl FileFingerprint {
    pub fn from_path(path: &Path) -> Result<Self, BooruError> {
        let meta = fs::metadata(path).map_err(|source| BooruError::Io { path: path.to_path_buf(), source })?;
        let size = meta.len() as i64;
        let modified = meta
            .modified()
            .map_err(|err| BooruError::Io {
                path: path.to_path_buf(),
                source: std::io::Error::new(std::io::ErrorKind::Other, err),
            })?;
        let mtime = modified
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        Ok(Self { mtime, size })
    }
}

pub struct HashCache {
    conn: Connection,
    path: PathBuf,
}

impl HashCache {
    pub fn open_default() -> Result<Self, BooruError> {
        let base = BaseDirectories::with_prefix("lightbooru")
            .map_err(|err| BooruError::Cache { message: err.to_string() })?;
        let path = base
            .place_cache_file("hash_cache.sqlite")
            .map_err(|err| BooruError::Cache { message: err.to_string() })?;
        Self::open(&path)
    }

    pub fn open(path: &Path) -> Result<Self, BooruError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|source| BooruError::Io { path: parent.to_path_buf(), source })?;
        }
        let conn = Connection::open(path).map_err(|source| BooruError::Database { path: path.to_path_buf(), source })?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS hash_cache (
                 path TEXT NOT NULL,
                 algo INTEGER NOT NULL,
                 mtime INTEGER NOT NULL,
                 size INTEGER NOT NULL,
                 bits BLOB NOT NULL,
                 bits_len INTEGER NOT NULL,
                 PRIMARY KEY(path, algo)
             );",
        )
        .map_err(|source| BooruError::Database { path: path.to_path_buf(), source })?;
        Ok(Self { conn, path: path.to_path_buf() })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn lookup(
        &self,
        image_path: &Path,
        algo: FuzzyHashAlgorithm,
        fingerprint: &FileFingerprint,
    ) -> Result<Option<FuzzyHash>, BooruError> {
        let mut stmt = self
            .conn
            .prepare("SELECT mtime, size, bits, bits_len FROM hash_cache WHERE path = ?1 AND algo = ?2")
            .map_err(|source| BooruError::Database { path: self.path.clone(), source })?;
        let mut rows = stmt
            .query(params![image_path.to_string_lossy(), algo as i32])
            .map_err(|source| BooruError::Database { path: self.path.clone(), source })?;
        if let Some(row) = rows.next().map_err(|source| BooruError::Database { path: self.path.clone(), source })? {
            let mtime: i64 = row.get(0).map_err(|source| BooruError::Database { path: self.path.clone(), source })?;
            let size: i64 = row.get(1).map_err(|source| BooruError::Database { path: self.path.clone(), source })?;
            if mtime == fingerprint.mtime && size == fingerprint.size {
                let bits: Vec<u8> = row.get(2).map_err(|source| BooruError::Database { path: self.path.clone(), source })?;
                let bits_len: i64 = row.get(3).map_err(|source| BooruError::Database { path: self.path.clone(), source })?;
                let bits = unpack_bits(&bits, bits_len as usize);
                return Ok(Some(FuzzyHash { algo, bits }));
            }
        }
        Ok(None)
    }

    pub fn store(
        &self,
        image_path: &Path,
        algo: FuzzyHashAlgorithm,
        fingerprint: &FileFingerprint,
        hash: &FuzzyHash,
    ) -> Result<(), BooruError> {
        let bits = pack_bits(&hash.bits);
        self.conn
            .execute(
                "INSERT INTO hash_cache (path, algo, mtime, size, bits, bits_len)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(path, algo) DO UPDATE SET
                     mtime = excluded.mtime,
                     size = excluded.size,
                     bits = excluded.bits,
                     bits_len = excluded.bits_len",
                params![
                    image_path.to_string_lossy(),
                    algo as i32,
                    fingerprint.mtime,
                    fingerprint.size,
                    bits,
                    hash.bits.len() as i64
                ],
            )
            .map_err(|source| BooruError::Database { path: self.path.clone(), source })?;
        Ok(())
    }
}

pub struct HashComputation {
    pub hashes: Vec<(usize, FuzzyHash)>,
    pub warnings: Vec<DuplicateWarning>,
}

pub fn compute_fuzzy_hash(path: &Path, algo: FuzzyHashAlgorithm) -> Result<FuzzyHash, BooruError> {
    let image = image::open(path).map_err(|source| BooruError::Image { path: path.to_path_buf(), source })?;
    let bits = match algo {
        FuzzyHashAlgorithm::AHash => average_hash(&image).bits,
        FuzzyHashAlgorithm::DHash => difference_hash(&image).bits,
        FuzzyHashAlgorithm::PHash => perceptual_hash(&image).bits,
    };
    Ok(FuzzyHash { algo, bits })
}

pub fn compute_hashes_with_cache(
    items: &[ImageItem],
    algo: FuzzyHashAlgorithm,
    mut cache: Option<&mut HashCache>,
    progress: Option<&dyn ProgressObserver>,
) -> HashComputation {
    let mut warnings = Vec::new();
    let mut hashes = Vec::new();
    let mut pending: Vec<(usize, PathBuf, Option<FileFingerprint>)> = Vec::new();

    for (idx, item) in items.iter().enumerate() {
        let fingerprint = if cache.is_some() {
            FileFingerprint::from_path(&item.image_path)
                .map_err(|err| {
                    warnings.push(DuplicateWarning { path: item.image_path.clone(), message: format!("{err}") });
                    err
                })
                .ok()
        } else {
            None
        };

        if let (Some(cache), Some(fingerprint)) = (cache.as_deref_mut(), fingerprint.as_ref()) {
            match cache.lookup(&item.image_path, algo, fingerprint) {
                Ok(Some(hash)) => {
                    hashes.push((idx, hash));
                    if let Some(observer) = progress {
                        observer.inc(1);
                    }
                    continue;
                }
                Ok(None) => {}
                Err(err) => warnings.push(DuplicateWarning { path: item.image_path.clone(), message: format!("{err}") }),
            }
        }
        pending.push((idx, item.image_path.clone(), fingerprint));
    }

    let observer = progress;
    let results: Vec<(usize, Result<FuzzyHash, BooruError>, Option<FileFingerprint>, PathBuf)> = pending
        .par_iter()
        .map(|(idx, path, fingerprint)| {
            let result = compute_fuzzy_hash(path, algo);
            if let Some(observer) = observer {
                observer.inc(1);
            }
            (*idx, result, fingerprint.clone(), path.clone())
        })
        .collect();

    for (idx, result, fingerprint, path) in results {
        match result {
            Ok(hash) => {
                if let (Some(cache), Some(fingerprint)) = (cache.as_deref_mut(), fingerprint.as_ref()) {
                    if let Err(err) = cache.store(&path, algo, fingerprint, &hash) {
                        warnings.push(DuplicateWarning { path: path.clone(), message: format!("{err}") });
                    }
                }
                hashes.push((idx, hash));
            }
            Err(err) => warnings.push(DuplicateWarning { path, message: format!("{err}") }),
        }
    }

    HashComputation { hashes, warnings }
}

pub fn group_duplicates(
    items: &[ImageItem],
    hashes: &[(usize, FuzzyHash)],
    max_distance: u32,
    skip_same_dir: bool,
) -> Vec<DuplicateGroup> {
    let mut uf = UnionFind::new(items.len());
    let pairs: Vec<(usize, usize)> = (0..hashes.len())
        .into_par_iter()
        .flat_map(|i| {
            let mut local = Vec::new();
            for j in (i + 1)..hashes.len() {
                let (idx_i, hash_i) = &hashes[i];
                let (idx_j, hash_j) = &hashes[j];
                if skip_same_dir && same_parent(&items[*idx_i].image_path, &items[*idx_j].image_path) {
                    continue;
                }
                if hash_i.distance(hash_j) <= max_distance {
                    local.push((*idx_i, *idx_j));
                }
            }
            local
        })
        .collect();

    for (a, b) in pairs {
        uf.union(a, b);
    }

    let mut groups_map: HashMap<usize, Vec<usize>> = HashMap::new();
    for (idx, _) in hashes {
        let root = uf.find(*idx);
        groups_map.entry(root).or_default().push(*idx);
    }

    let mut groups: Vec<DuplicateGroup> = groups_map
        .into_values()
        .filter(|items| items.len() > 1)
        .map(|items| DuplicateGroup { items })
        .collect();

    groups.sort_by_key(|group| group.items.len());
    groups.reverse();

    groups
}

pub fn find_duplicates_with_cache(
    items: &[ImageItem],
    algo: FuzzyHashAlgorithm,
    max_distance: u32,
    skip_same_dir: bool,
    cache: Option<&mut HashCache>,
    progress: Option<&dyn ProgressObserver>,
) -> DuplicateReport {
    let computation = compute_hashes_with_cache(items, algo, cache, progress);
    let groups = group_duplicates(items, &computation.hashes, max_distance, skip_same_dir);
    DuplicateReport { groups, warnings: computation.warnings }
}

pub fn find_duplicates(items: &[ImageItem], algo: FuzzyHashAlgorithm, max_distance: u32) -> DuplicateReport {
    find_duplicates_with_cache(items, algo, max_distance, true, None, None)
}

// Hash implementations come from the imagehash crate.

fn pack_bits(bits: &[bool]) -> Vec<u8> {
    let mut out = Vec::with_capacity((bits.len() + 7) / 8);
    let mut byte = 0u8;
    for (idx, bit) in bits.iter().enumerate() {
        if *bit {
            byte |= 1u8 << (idx % 8);
        }
        if idx % 8 == 7 {
            out.push(byte);
            byte = 0;
        }
    }
    if bits.len() % 8 != 0 {
        out.push(byte);
    }
    out
}

fn unpack_bits(bytes: &[u8], len: usize) -> Vec<bool> {
    let mut out = Vec::with_capacity(len);
    for idx in 0..len {
        let byte = bytes.get(idx / 8).copied().unwrap_or(0);
        out.push(((byte >> (idx % 8)) & 1) == 1);
    }
    out
}

fn same_parent(a: &Path, b: &Path) -> bool {
    let a_parent = a.parent();
    let b_parent = b.parent();
    match (a_parent, b_parent) {
        (Some(a_parent), Some(b_parent)) => a_parent == b_parent,
        _ => false,
    }
}

struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<usize>,
}

impl UnionFind {
    fn new(size: usize) -> Self {
        Self { parent: (0..size).collect(), rank: vec![0; size] }
    }

    fn find(&mut self, x: usize) -> usize {
        if self.parent[x] != x {
            let root = self.find(self.parent[x]);
            self.parent[x] = root;
        }
        self.parent[x]
    }

    fn union(&mut self, a: usize, b: usize) {
        let mut root_a = self.find(a);
        let mut root_b = self.find(b);
        if root_a == root_b {
            return;
        }
        if self.rank[root_a] < self.rank[root_b] {
            std::mem::swap(&mut root_a, &mut root_b);
        }
        self.parent[root_b] = root_a;
        if self.rank[root_a] == self.rank[root_b] {
            self.rank[root_a] += 1;
        }
    }
}
