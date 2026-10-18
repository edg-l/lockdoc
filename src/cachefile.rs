//! The on-disk form of one crate's docs: the rustdoc JSON split into one blob per item behind
//! sorted lookup tables. The file is memory-mapped and searched in place, so opening a crate
//! parses nothing and only the items a query touches are deserialized.
//!
//! Layout, integers little-endian:
//!
//! ```text
//! MAGIC
//! u64 header length, header (JSON `Header`)
//! 4 tables, each: u64 record count, records of 16 bytes
//!   items    (u32 id, u32 len, u64 offset)   sorted by id; value is the item's JSON
//!   paths    (u32 id, u32 len, u64 offset)   sorted by id; value is its `ItemSummary` JSON
//!   by_path  (u32 id, u32 len, u64 offset)   sorted by key; key is the defining path, `a::b::C`
//!   by_name  (u32 id, u32 len, u64 offset)   sorted by key; key is the item name
//! blob of values and keys
//! ```

use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::path::Path;

use anyhow::{Context, Result, bail};
use memmap2::Mmap;
use rustdoc_types::{Crate, ExternalCrate, Id, Item, ItemEnum, ItemSummary};
use serde::{Deserialize, Serialize};

/// Bumped whenever the layout or `Header` changes; part of the cache key.
pub const VERSION: u32 = 1;
const MAGIC: &[u8; 8] = b"LOCKDOC\x01";
const RECORD: usize = 16;

#[derive(Serialize, Deserialize)]
struct Header {
    root: Id,
    external_crates: HashMap<u32, ExternalCrate>,
    /// Names of the crates this one re-exports from, so a search can build them in one batch
    /// without scanning every item.
    reexported_crates: Vec<String>,
}

#[derive(Clone, Copy)]
struct Table {
    start: usize,
    count: usize,
}

pub struct CacheFile {
    map: Mmap,
    header: Header,
    items: Table,
    paths: Table,
    by_path: Table,
    by_name: Table,
}

/// Writes `krate` to `to` in the cache format, through a temporary file and a rename.
pub fn write(krate: &Crate, to: &Path) -> Result<()> {
    let mut blob: Vec<u8> = Vec::new();
    let mut put = |bytes: &[u8]| -> (u32, u64) {
        let off = blob.len() as u64;
        blob.extend_from_slice(bytes);
        (bytes.len() as u32, off)
    };

    let mut items: Vec<(u32, u32, u64)> = Vec::with_capacity(krate.index.len());
    let mut by_name: Vec<(Vec<u8>, u32)> = Vec::new();
    let mut reexported = BTreeSet::new();
    for (id, item) in &krate.index {
        let (len, off) = put(&serde_json::to_vec(item)?);
        items.push((id.0, len, off));
        if item.crate_id == 0
            && let Some(name) = &item.name
        {
            by_name.push((name.as_bytes().to_vec(), id.0));
        }
        if let ItemEnum::Use(u) = &item.inner
            && let Some(summary) = u.id.and_then(|t| krate.paths.get(&t))
            && let Some(ext) = krate.external_crates.get(&summary.crate_id)
        {
            reexported.insert(ext.name.clone());
        }
    }
    let mut paths: Vec<(u32, u32, u64)> = Vec::with_capacity(krate.paths.len());
    let mut by_path: Vec<(Vec<u8>, u32)> = Vec::new();
    for (id, summary) in &krate.paths {
        let (len, off) = put(&serde_json::to_vec(summary)?);
        paths.push((id.0, len, off));
        if summary.crate_id == 0 {
            by_path.push((summary.path.join("::").into_bytes(), id.0));
        }
    }
    items.sort_unstable_by_key(|r| r.0);
    paths.sort_unstable_by_key(|r| r.0);
    by_path.sort_unstable();
    by_name.sort_unstable();
    let keyed = |entries: Vec<(Vec<u8>, u32)>, blob: &mut Vec<u8>| -> Vec<(u32, u32, u64)> {
        entries
            .into_iter()
            .map(|(key, id)| {
                let off = blob.len() as u64;
                blob.extend_from_slice(&key);
                (id, key.len() as u32, off)
            })
            .collect()
    };
    let by_path = keyed(by_path, &mut blob);
    let by_name = keyed(by_name, &mut blob);

    let header = serde_json::to_vec(&Header {
        root: krate.root,
        external_crates: krate.external_crates.clone(),
        reexported_crates: reexported.into_iter().collect(),
    })?;
    let tables = [&items, &paths, &by_path, &by_name];
    let table_bytes: usize = tables.iter().map(|t| 8 + t.len() * RECORD).sum();
    let blob_start = (MAGIC.len() + 8 + header.len() + table_bytes) as u64;

    let mut out = Vec::with_capacity(blob_start as usize + blob.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(header.len() as u64).to_le_bytes());
    out.extend_from_slice(&header);
    for table in tables {
        out.extend_from_slice(&(table.len() as u64).to_le_bytes());
        for &(id, len, off) in table {
            out.extend_from_slice(&id.to_le_bytes());
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&(blob_start + off).to_le_bytes());
        }
    }
    out.extend_from_slice(&blob);

    // The pid keeps two lockdoc processes on one project from sharing a temporary file, and the
    // rename means a reader maps either the old file or the new one, never a partial write.
    let tmp = to.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&tmp, &out).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, to)?;
    Ok(())
}

impl CacheFile {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        // SAFETY: cache files are only ever replaced by renaming a new file over them, never
        // written in place or truncated, so the mapped bytes cannot change under the map.
        let map = unsafe { Mmap::map(&file)? };
        if map.get(..MAGIC.len()) != Some(MAGIC.as_slice()) {
            bail!("{} is not a lockdoc cache file", path.display());
        }
        let mut pos = MAGIC.len();
        let header_len = read_u64(&map, pos)? as usize;
        pos += 8;
        let header: Header = serde_json::from_slice(
            map.get(pos..pos + header_len)
                .context("truncated cache header")?,
        )?;
        pos += header_len;
        let mut table = || -> Result<Table> {
            let count = read_u64(&map, pos)? as usize;
            let start = pos + 8;
            pos = start + count * RECORD;
            if pos > map.len() {
                bail!("truncated cache table in {}", path.display());
            }
            Ok(Table { start, count })
        };
        let (items, paths, by_path, by_name) = (table()?, table()?, table()?, table()?);
        Ok(Self {
            map,
            header,
            items,
            paths,
            by_path,
            by_name,
        })
    }

    pub fn root(&self) -> Id {
        self.header.root
    }

    pub fn external_crate(&self, crate_id: u32) -> Option<&ExternalCrate> {
        self.header.external_crates.get(&crate_id)
    }

    pub fn reexported_crates(&self) -> &[String] {
        &self.header.reexported_crates
    }

    pub fn item(&self, id: Id) -> Option<Item> {
        serde_json::from_slice(self.by_id(self.items, id)?).ok()
    }

    pub fn summary(&self, id: Id) -> Option<ItemSummary> {
        serde_json::from_slice(self.by_id(self.paths, id)?).ok()
    }

    /// The local item whose defining path is `path`.
    pub fn id_by_path(&self, path: &[String]) -> Option<Id> {
        self.by_key(self.by_path, path.join("::").as_bytes())
            .first()
            .copied()
    }

    /// Every local item named `name`.
    pub fn ids_named(&self, name: &str) -> Vec<Id> {
        self.by_key(self.by_name, name.as_bytes())
    }

    fn record(&self, t: Table, i: usize) -> (u32, usize, usize) {
        let at = t.start + i * RECORD;
        let r = &self.map[at..at + RECORD];
        let id = u32::from_le_bytes(r[0..4].try_into().expect("4 bytes"));
        let len = u32::from_le_bytes(r[4..8].try_into().expect("4 bytes")) as usize;
        let off = u64::from_le_bytes(r[8..16].try_into().expect("8 bytes")) as usize;
        (id, len, off)
    }

    fn value(&self, off: usize, len: usize) -> Option<&[u8]> {
        self.map.get(off..off + len)
    }

    fn by_id(&self, t: Table, id: Id) -> Option<&[u8]> {
        let i = partition_point(t.count, |i| self.record(t, i).0 < id.0);
        let (found, len, off) = (i < t.count).then(|| self.record(t, i))?;
        (found == id.0).then(|| self.value(off, len))?
    }

    fn by_key(&self, t: Table, key: &[u8]) -> Vec<Id> {
        let key_at = |i: usize| {
            let (_, len, off) = self.record(t, i);
            self.value(off, len).unwrap_or_default()
        };
        let first = partition_point(t.count, |i| key_at(i) < key);
        (first..t.count)
            .take_while(|&i| key_at(i) == key)
            .map(|i| Id(self.record(t, i).0))
            .collect()
    }
}

/// `slice::partition_point` over indices, for records that are decoded on access.
fn partition_point(count: usize, mut before: impl FnMut(usize) -> bool) -> usize {
    let (mut lo, mut hi) = (0, count);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if before(mid) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

fn read_u64(map: &[u8], at: usize) -> Result<u64> {
    let bytes = map.get(at..at + 8).context("truncated cache file")?;
    Ok(u64::from_le_bytes(bytes.try_into().expect("8 bytes")))
}
