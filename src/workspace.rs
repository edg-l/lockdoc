use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result, bail};
use cargo_metadata::{Metadata, MetadataCommand, Package, PackageId, TargetKind};
use serde::{Deserialize, Serialize};

use crate::docbuild::fnv1a;

/// `cargo metadata` output saved on disk with the modification times of every file it was
/// derived from, so a new session skips the ~0.3s cargo run while nothing changed.
#[derive(Serialize, Deserialize)]
struct SavedMetadata {
    stamps: Vec<(PathBuf, u128)>,
    meta: Metadata,
}

fn mtime_ns(path: &Path) -> u128 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos())
}

/// The files `cargo metadata` reads that a user edits: the lockfile, the root manifest and
/// the manifest of every path package. Registry and git packages are fixed by the lockfile.
fn stamps(meta: &Metadata) -> Vec<(PathBuf, u128)> {
    let root = meta.workspace_root.as_std_path();
    let mut files = vec![root.join("Cargo.lock"), root.join("Cargo.toml")];
    files.extend(
        meta.packages
            .iter()
            .filter(|p| p.source.is_none())
            .map(|p| p.manifest_path.clone().into_std_path_buf()),
    );
    files
        .into_iter()
        .map(|f| {
            let t = mtime_ns(&f);
            (f, t)
        })
        .collect()
}

/// The triple of the machine lockdoc runs on, from `rustc -vV`.
fn host_triple() -> Result<&'static str> {
    static HOST: OnceLock<String> = OnceLock::new();
    if let Some(host) = HOST.get() {
        return Ok(host);
    }
    let out = Command::new("rustc")
        .arg("-vV")
        .output()
        .context("running rustc -vV")?;
    let host = String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("host: "))
        .context("rustc -vV printed no host triple")?
        .to_string();
    Ok(HOST.get_or_init(|| host))
}

fn metadata(dir: &Path, cache: &Path) -> Result<SavedMetadata> {
    // The `v2` names the cargo options below; output saved with other options is not reused.
    let saved_at = cache
        .join("metadata")
        .join(format!("{:016x}-v2.json", fnv1a(&dir.to_string_lossy())));
    if let Ok(bytes) = std::fs::read(&saved_at)
        && let Ok(saved) = serde_json::from_slice::<SavedMetadata>(&bytes)
        && saved.stamps.iter().all(|(f, t)| mtime_ns(f) == *t)
    {
        return Ok(saved);
    }
    // `--locked` keeps cargo from rewriting Cargo.lock: a lockfile that is out of date becomes
    // an error the caller sees instead of a silent update. Without `--filter-platform`, cargo
    // resolves every target platform and needs the sources of crates only another platform
    // uses, which a machine that never built for it does not have.
    let meta = MetadataCommand::new()
        .current_dir(dir)
        .other_options(vec![
            "--locked".to_string(),
            "--filter-platform".to_string(),
            host_triple()?.to_string(),
        ])
        .exec()
        .with_context(|| format!("cargo metadata failed in {}", dir.display()))?;
    let saved = SavedMetadata {
        stamps: stamps(&meta),
        meta,
    };
    // Saving is an optimization; a read-only cache directory only costs the next session time.
    if std::fs::create_dir_all(saved_at.parent().expect("has a parent")).is_ok()
        && let Ok(bytes) = serde_json::to_vec(&saved)
    {
        let tmp = saved_at.with_extension(format!("{}.tmp", std::process::id()));
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &saved_at);
        }
    }
    Ok(saved)
}

/// The resolved dependency graph of one cargo workspace, read from `cargo metadata`.
pub struct Workspace {
    pub root: PathBuf,
    meta: Metadata,
    packages: HashMap<PackageId, usize>,
    /// Edges of the resolve graph: package → (name the dependent uses in code, dependency).
    deps: HashMap<PackageId, Vec<(String, PackageId)>>,
    features: HashMap<PackageId, Vec<String>>,
    stamps: Vec<(PathBuf, u128)>,
}

impl Workspace {
    pub fn load(dir: &Path, cache: &Path) -> Result<Self> {
        let SavedMetadata { stamps, meta } = metadata(dir, cache)?;
        let Some(resolve) = &meta.resolve else {
            bail!("cargo metadata returned no resolve graph");
        };
        let packages = meta
            .packages
            .iter()
            .enumerate()
            .map(|(i, p)| (p.id.clone(), i))
            .collect();
        let mut deps = HashMap::new();
        let mut features = HashMap::new();
        for node in &resolve.nodes {
            deps.insert(
                node.id.clone(),
                node.deps
                    .iter()
                    .map(|d| (d.name.clone(), d.pkg.clone()))
                    .collect(),
            );
            let mut f: Vec<String> = node.features.iter().map(|f| f.to_string()).collect();
            f.sort();
            features.insert(node.id.clone(), f);
        }
        Ok(Self {
            root: meta.workspace_root.clone().into_std_path_buf(),
            meta,
            packages,
            deps,
            features,
            stamps,
        })
    }

    /// Whether the lockfile and manifests are unchanged since this graph was read.
    pub fn is_current(&self) -> bool {
        self.stamps.iter().all(|(f, t)| mtime_ns(f) == *t)
    }

    pub fn package(&self, id: &PackageId) -> &Package {
        &self.meta.packages[self.packages[id]]
    }

    pub fn has(&self, id: &PackageId) -> bool {
        self.packages.contains_key(id)
    }

    pub fn features(&self, id: &PackageId) -> &[String] {
        self.features.get(id).map_or(&[], Vec::as_slice)
    }

    pub fn members(&self) -> &[PackageId] {
        &self.meta.workspace_members
    }

    /// Name of the package's library target as code refers to it (`regex_syntax`, not
    /// `regex-syntax`), or `None` when the package has no library.
    pub fn lib_name(&self, id: &PackageId) -> Option<String> {
        self.package(id)
            .targets
            .iter()
            .find(|t| {
                t.is_kind(TargetKind::Lib)
                    || t.is_kind(TargetKind::ProcMacro)
                    || t.is_kind(TargetKind::RLib)
            })
            .map(|t| t.name.replace('-', "_"))
    }

    pub fn direct_deps(&self, id: &PackageId) -> &[(String, PackageId)] {
        self.deps.get(id).map_or(&[], Vec::as_slice)
    }

    /// Finds the package that `name` refers to as seen from `from`: a direct dependency
    /// (matched by the name used in code, then by library name), else the nearest package in
    /// `from`'s dependency closure with that library name. Nearest-first matters when two
    /// versions of a crate are in the graph.
    pub fn find_from(&self, from: &PackageId, name: &str) -> Option<PackageId> {
        let name = name.replace('-', "_");
        if let Some((_, id)) = self.direct_deps(from).iter().find(|(n, _)| *n == name) {
            return Some(id.clone());
        }
        let mut seen = HashSet::from([from.clone()]);
        let mut queue = VecDeque::from([from.clone()]);
        while let Some(cur) = queue.pop_front() {
            for (_, dep) in self.direct_deps(&cur) {
                if !seen.insert(dep.clone()) {
                    continue;
                }
                if self.lib_name(dep).as_deref() == Some(name.as_str()) {
                    return Some(dep.clone());
                }
                queue.push_back(dep.clone());
            }
        }
        None
    }

    /// Resolves the first segment of a user-supplied path: a dependency of any workspace
    /// member (or a member itself), by code name or package name.
    pub fn find_root(&self, name: &str) -> Option<PackageId> {
        let code = name.replace('-', "_");
        for m in self.members() {
            if self.lib_name(m).as_deref() == Some(code.as_str()) {
                return Some(m.clone());
            }
        }
        for m in self.members() {
            if let Some(id) = self.find_from(m, &code) {
                return Some(id);
            }
        }
        self.meta
            .packages
            .iter()
            .find(|p| p.name.as_str() == name)
            .map(|p| p.id.clone())
    }

    /// Direct dependencies of every workspace member, deduplicated, as (code name, package).
    pub fn member_deps(&self) -> Vec<(String, PackageId)> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for m in self.members() {
            for (name, id) in self.direct_deps(m) {
                if seen.insert(id.clone()) {
                    out.push((name.clone(), id.clone()));
                }
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    pub fn is_member(&self, id: &PackageId) -> bool {
        self.members().contains(id)
    }

    /// `name@version`, the package spec `cargo doc -p` accepts unambiguously.
    pub fn spec(&self, id: &PackageId) -> String {
        let p = self.package(id);
        format!("{}@{}", p.name, p.version)
    }
}
