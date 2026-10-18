use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use cargo_metadata::{Metadata, MetadataCommand, Package, PackageId, TargetKind};

/// The resolved dependency graph of one cargo workspace, read from `cargo metadata`.
pub struct Workspace {
    pub root: PathBuf,
    meta: Metadata,
    packages: HashMap<PackageId, usize>,
    /// Edges of the resolve graph: package → (name the dependent uses in code, dependency).
    deps: HashMap<PackageId, Vec<(String, PackageId)>>,
    features: HashMap<PackageId, Vec<String>>,
}

impl Workspace {
    pub fn load(dir: &Path) -> Result<Self> {
        // `--frozen` keeps cargo from rewriting Cargo.lock or touching the network; a lockfile
        // that is out of date becomes an error the caller sees instead of a silent update.
        let meta = MetadataCommand::new()
            .current_dir(dir)
            .other_options(vec!["--frozen".to_string()])
            .exec()
            .with_context(|| format!("cargo metadata failed in {}", dir.display()))?;
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
        })
    }

    pub fn package(&self, id: &PackageId) -> &Package {
        &self.meta.packages[self.packages[id]]
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
