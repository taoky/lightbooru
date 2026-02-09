use std::collections::HashMap;
use std::path::{Path, PathBuf};

use imagehash::{average_hash, difference_hash, perceptual_hash};
use rayon::prelude::*;

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

pub fn compute_fuzzy_hash(path: &Path, algo: FuzzyHashAlgorithm) -> Result<FuzzyHash, BooruError> {
    let image = image::open(path).map_err(|source| BooruError::Image { path: path.to_path_buf(), source })?;
    let bits = match algo {
        FuzzyHashAlgorithm::AHash => average_hash(&image).bits,
        FuzzyHashAlgorithm::DHash => difference_hash(&image).bits,
        FuzzyHashAlgorithm::PHash => perceptual_hash(&image).bits,
    };
    Ok(FuzzyHash { algo, bits })
}

pub fn find_duplicates(items: &[ImageItem], algo: FuzzyHashAlgorithm, max_distance: u32) -> DuplicateReport {
    let results: Vec<(usize, Result<FuzzyHash, BooruError>)> = items
        .par_iter()
        .enumerate()
        .map(|(idx, item)| (idx, compute_fuzzy_hash(&item.image_path, algo)))
        .collect();

    let mut warnings = Vec::new();
    let mut hashes = Vec::new();
    for (idx, result) in results {
        match result {
            Ok(hash) => hashes.push((idx, hash)),
            Err(err) => warnings.push(DuplicateWarning {
                path: items[idx].image_path.clone(),
                message: format!("{err}"),
            }),
        }
    }

    let mut uf = UnionFind::new(items.len());
    let pairs: Vec<(usize, usize)> = (0..hashes.len())
        .into_par_iter()
        .flat_map(|i| {
            let mut local = Vec::new();
            for j in (i + 1)..hashes.len() {
                let (idx_i, hash_i) = &hashes[i];
                let (idx_j, hash_j) = &hashes[j];
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
    for (idx, _) in &hashes {
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

    DuplicateReport { groups, warnings }
}

// Hash implementations come from the imagehash crate.

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
