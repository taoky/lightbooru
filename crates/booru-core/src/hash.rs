use std::collections::HashMap;
use std::path::{Path, PathBuf};

use image::imageops::FilterType;

use crate::error::BooruError;
use crate::scan::ImageItem;

#[derive(Clone, Copy, Debug)]
pub enum FuzzyHashAlgorithm {
    AHash,
    DHash,
}

#[derive(Clone, Copy, Debug)]
pub struct FuzzyHash {
    pub algo: FuzzyHashAlgorithm,
    pub bits: u64,
}

impl FuzzyHash {
    pub fn distance(&self, other: &FuzzyHash) -> u32 {
        (self.bits ^ other.bits).count_ones()
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
        FuzzyHashAlgorithm::AHash => compute_ahash(&image),
        FuzzyHashAlgorithm::DHash => compute_dhash(&image),
    };
    Ok(FuzzyHash { algo, bits })
}

pub fn find_duplicates(items: &[ImageItem], algo: FuzzyHashAlgorithm, max_distance: u32) -> DuplicateReport {
    let mut warnings = Vec::new();
    let mut hashes = Vec::new();

    for (idx, item) in items.iter().enumerate() {
        match compute_fuzzy_hash(&item.image_path, algo) {
            Ok(hash) => hashes.push((idx, hash)),
            Err(err) => warnings.push(DuplicateWarning {
                path: item.image_path.clone(),
                message: format!("{err}"),
            }),
        }
    }

    let mut uf = UnionFind::new(items.len());
    for i in 0..hashes.len() {
        for j in (i + 1)..hashes.len() {
            let (idx_i, hash_i) = hashes[i];
            let (idx_j, hash_j) = hashes[j];
            if hash_i.distance(&hash_j) <= max_distance {
                uf.union(idx_i, idx_j);
            }
        }
    }

    let mut groups_map: HashMap<usize, Vec<usize>> = HashMap::new();
    for (idx, _) in hashes {
        let root = uf.find(idx);
        groups_map.entry(root).or_default().push(idx);
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

fn compute_ahash(image: &image::DynamicImage) -> u64 {
    let gray = image.to_luma8();
    let resized = image::imageops::resize(&gray, 8, 8, FilterType::Triangle);
    let mut sum: u32 = 0;
    for pixel in resized.pixels() {
        sum += pixel[0] as u32;
    }
    let avg = sum / 64;
    let mut bits = 0u64;
    for (idx, pixel) in resized.pixels().enumerate() {
        if (pixel[0] as u32) >= avg {
            bits |= 1u64 << idx;
        }
    }
    bits
}

fn compute_dhash(image: &image::DynamicImage) -> u64 {
    let gray = image.to_luma8();
    let resized = image::imageops::resize(&gray, 9, 8, FilterType::Triangle);
    let mut bits = 0u64;
    let mut idx = 0;
    for y in 0..8 {
        for x in 0..8 {
            let left = resized.get_pixel(x, y)[0];
            let right = resized.get_pixel(x + 1, y)[0];
            if left > right {
                bits |= 1u64 << idx;
            }
            idx += 1;
        }
    }
    bits
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
