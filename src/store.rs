use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow, bail};
use cargo_metadata::PackageId;
use rustdoc_types::{Attribute, Id, Item, ItemEnum, StructKind, VariantKind};
use serde::{Deserialize, Serialize};

use crate::cachefile::CacheFile;
use crate::docbuild::{BuildNote, Builder, fnv1a, lock};
use crate::workspace::Workspace;

/// The docs of one package.
pub struct Doc {
    pub pkg: PackageId,
    pub file: CacheFile,
}

/// An item inside a specific package's docs, deserialized.
#[derive(Clone)]
pub struct Ref {
    pub doc: Arc<Doc>,
    pub id: Id,
    item: Arc<Item>,
}

impl Ref {
    pub fn new(doc: &Arc<Doc>, id: Id) -> Option<Self> {
        Some(Self {
            item: Arc::new(doc.file.item(id)?),
            doc: doc.clone(),
            id,
        })
    }

    pub fn item(&self) -> &Item {
        &self.item
    }

    fn key(&self) -> (PackageId, Id) {
        (self.doc.pkg.clone(), self.id)
    }
}

/// Where a name leads: an item lockdoc has docs for, or something it can only name (std, or a
/// crate whose docs failed to build).
pub enum Target {
    Found(Ref),
    Named(String),
}

pub struct Hit {
    pub path: String,
    pub target: Ref,
}

/// The flattened public API under one path, for search. Saved on disk with the cache files it
/// was built from.
#[derive(Serialize, Deserialize)]
struct SearchIndex {
    docs: Vec<(PackageId, PathBuf)>,
    entries: Vec<IndexEntry>,
}

#[derive(Serialize, Deserialize)]
struct IndexEntry {
    name: String,
    path: String,
    pkg: PackageId,
    id: u32,
    /// An associated item rather than a module-level one; ranks one step lower.
    member: bool,
}

/// Everything known about one project, shared by every query against it. Each field has its
/// own short-lived lock, and building docs takes the builder's separate lock, so concurrent
/// queries only wait on each other when one needs a build another is running.
pub struct Store {
    dir: PathBuf,
    builder: Builder,
    ws: Mutex<Option<Arc<Workspace>>>,
    docs: Mutex<HashMap<PackageId, Arc<Doc>>>,
    indexes: Mutex<HashMap<String, Arc<SearchIndex>>>,
}

impl Store {
    pub fn new(dir: PathBuf) -> Result<Self> {
        Ok(Self {
            dir,
            builder: Builder::new()?,
            ws: Mutex::default(),
            docs: Mutex::default(),
            indexes: Mutex::default(),
        })
    }

    /// Starts one query. Its build note collects what this query alone had to build.
    pub fn session(&self) -> Session<'_> {
        Session {
            store: self,
            note: BuildNote::default(),
        }
    }
}

/// One query against a `Store`.
pub struct Session<'a> {
    store: &'a Store,
    note: BuildNote,
}

impl Session<'_> {
    /// The workspace graph, reloaded when the lockfile or a manifest changes so versions and
    /// features stay those on disk.
    pub fn ws(&self) -> Result<Arc<Workspace>> {
        let mut ws = lock(&self.store.ws);
        if let Some(current) = ws.as_ref().filter(|w| w.is_current()) {
            return Ok(current.clone());
        }
        let fresh = Arc::new(Workspace::load(
            &self.store.dir,
            self.store.builder.cache_dir(),
        )?);
        *ws = Some(fresh.clone());
        lock(&self.store.docs).clear();
        lock(&self.store.indexes).clear();
        Ok(fresh)
    }

    pub fn take_note(&mut self) -> Option<String> {
        std::mem::take(&mut self.note).render()
    }

    fn ensure(&mut self, ids: &[PackageId]) -> Result<()> {
        let ws = self.ws()?;
        let note = self.store.builder.ensure(&ws, ids)?;
        self.note.merge(note);
        Ok(())
    }

    pub fn load(&mut self, id: &PackageId) -> Result<Arc<Doc>> {
        if let Some(doc) = lock(&self.store.docs).get(id) {
            return Ok(doc.clone());
        }
        self.ensure(std::slice::from_ref(id))?;
        let ws = self.ws()?;
        let path = self.store.builder.doc_path(&ws, id);
        if !path.exists() {
            bail!("no docs for {}", ws.spec(id));
        }
        let doc = Arc::new(Doc {
            pkg: id.clone(),
            file: CacheFile::open(&path)?,
        });
        lock(&self.store.docs).insert(id.clone(), doc.clone());
        Ok(doc)
    }

    /// Follows `id` as referenced from `doc` to the docs that define it.
    pub fn target(&mut self, doc: &Arc<Doc>, id: Id) -> Target {
        if let Some(r) = Ref::new(doc, id)
            && r.item().crate_id == 0
        {
            return Target::Found(r);
        }
        let Some(summary) = doc.file.summary(id) else {
            return Target::Named("<unknown>".into());
        };
        let full = summary.path.join("::");
        let Some(ext) = doc.file.external_crate(summary.crate_id) else {
            return Target::Named(full);
        };
        let Ok(ws) = self.ws() else {
            return Target::Named(full);
        };
        let Some(pkg) = ws.find_from(&doc.pkg, &ext.name.clone()) else {
            return Target::Named(full);
        };
        let Ok(ext_doc) = self.load(&pkg) else {
            return Target::Named(full);
        };
        let found = ext_doc
            .file
            .id_by_path(&summary.path)
            .and_then(|id| Ref::new(&ext_doc, id))
            .or_else(|| {
                // A crate's own `paths` table does not always agree with its `index`: proc macros
                // get one id in each. Fall back to the unique indexed item with the same name
                // and kind.
                let name = summary.path.last()?;
                let mut matches = ext_doc
                    .file
                    .ids_named(name)
                    .into_iter()
                    .filter_map(|id| Ref::new(&ext_doc, id))
                    .filter(|r| r.item().inner.item_kind() == summary.kind);
                let first = matches.next()?;
                matches.next().is_none().then_some(first)
            });
        match found {
            Some(r) => Target::Found(r),
            None => Target::Named(full),
        }
    }

    /// Names reachable from a module, following `pub use` re-exports and globs into other
    /// crates the way `use` paths resolve in code.
    pub fn children(&mut self, module: &Ref) -> Vec<(String, Target)> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        self.children_into(module, &mut out, &mut seen);
        out
    }

    fn children_into(
        &mut self,
        r: &Ref,
        out: &mut Vec<(String, Target)>,
        seen: &mut HashSet<(PackageId, Id)>,
    ) {
        if !seen.insert(r.key()) {
            return;
        }
        let ids: Vec<Id> = match &r.item().inner {
            ItemEnum::Module(m) => m.items.clone(),
            ItemEnum::Enum(e) => e.variants.clone(),
            _ => return,
        };
        for id in ids {
            let Some(child) = Ref::new(&r.doc, id) else {
                continue;
            };
            let item = child.item();
            match &item.inner {
                ItemEnum::Use(u) => {
                    let Some(target_id) = u.id else {
                        continue;
                    };
                    let target = self.target(&r.doc, target_id);
                    if u.is_glob {
                        if let Target::Found(t) = target {
                            self.children_into(&t, out, seen);
                        }
                    } else {
                        out.push((u.name.clone(), target));
                    }
                }
                ItemEnum::Impl(_) => {}
                _ if is_hidden(item) => {}
                _ => {
                    if let Some(name) = item.name.clone() {
                        out.push((name, Target::Found(child)));
                    }
                }
            }
        }
    }

    /// Fields, variants and associated items of a type, or the items of a trait. Items from a
    /// type's trait impls are included only with `trait_impls`; their docs live on the trait.
    pub fn members(&self, r: &Ref, trait_impls: bool) -> Vec<(String, Ref)> {
        let at = |id: &Id| Ref::new(&r.doc, *id);
        let named = |ids: &[Id]| -> Vec<(String, Ref)> {
            ids.iter()
                .filter_map(|id| {
                    let m = at(id)?;
                    Some((m.item().name.clone()?, m))
                })
                .collect()
        };
        let mut out = Vec::new();
        let impls: &[Id] = match &r.item().inner {
            ItemEnum::Struct(s) => {
                match &s.kind {
                    StructKind::Plain { fields, .. } => out.extend(named(fields)),
                    StructKind::Tuple(fields) => out.extend(
                        fields
                            .iter()
                            .enumerate()
                            .filter_map(|(i, f)| Some((i.to_string(), at(f.as_ref()?)?))),
                    ),
                    StructKind::Unit => {}
                }
                &s.impls
            }
            ItemEnum::Enum(e) => {
                out.extend(named(&e.variants));
                &e.impls
            }
            ItemEnum::Union(u) => {
                out.extend(named(&u.fields));
                &u.impls
            }
            ItemEnum::Primitive(p) => &p.impls,
            ItemEnum::Trait(t) => return named(&t.items),
            ItemEnum::Variant(v) => {
                if let VariantKind::Struct { fields, .. } = &v.kind {
                    out.extend(named(fields));
                }
                return out;
            }
            _ => return out,
        };
        let (inherent, traits): (Vec<Ref>, Vec<Ref>) = impls
            .iter()
            .filter_map(at)
            .filter(|i| matches!(&i.item().inner, ItemEnum::Impl(im) if im.blanket_impl.is_none()))
            .partition(|i| matches!(&i.item().inner, ItemEnum::Impl(im) if im.trait_.is_none()));
        let traits = if trait_impls { traits } else { Vec::new() };
        for imp in inherent.into_iter().chain(traits) {
            if let ItemEnum::Impl(im) = &imp.item().inner {
                out.extend(named(&im.items));
            }
        }
        out
    }

    /// Provided trait methods named `name` that a type gets from its trait impls without
    /// overriding them, so they appear on the trait and not in the impl.
    fn provided(&mut self, r: &Ref, name: &str) -> Vec<Ref> {
        let impls = match &r.item().inner {
            ItemEnum::Struct(s) => s.impls.clone(),
            ItemEnum::Enum(e) => e.impls.clone(),
            ItemEnum::Union(u) => u.impls.clone(),
            ItemEnum::Primitive(p) => p.impls.clone(),
            _ => return Vec::new(),
        };
        let mut out = Vec::new();
        for imp in impls.iter().filter_map(|id| Ref::new(&r.doc, *id)) {
            let ItemEnum::Impl(im) = &imp.item().inner else {
                continue;
            };
            let Some(trait_) = &im.trait_ else {
                continue;
            };
            if !im.provided_trait_methods.iter().any(|m| m == name) {
                continue;
            }
            if let Target::Found(t) = self.target(&r.doc, trait_.id) {
                out.extend(
                    self.members(&t, false)
                        .into_iter()
                        .filter(|(n, _)| n == name)
                        .map(|(_, m)| m),
                );
            }
        }
        out
    }

    /// Resolves `crate::path::Item` (or `Type::method`) to every item it names. A path can name
    /// several items at once, such as the `Serialize` trait and derive macro.
    pub fn resolve(&mut self, path: &str) -> Result<(Vec<Ref>, String)> {
        let segs: Vec<&str> = path
            .split("::")
            .map(|s| s.split('<').next().unwrap_or(s).trim())
            .filter(|s| !s.is_empty())
            .collect();
        let Some((first, rest)) = segs.split_first() else {
            bail!("empty path");
        };
        if matches!(*first, "std" | "core" | "alloc") {
            bail!("lockdoc covers the project's dependencies, not the standard library");
        }
        let root = {
            let ws = self.ws()?;
            ws.find_root(first).ok_or_else(|| {
                anyhow!(
                    "`{first}` is not a dependency of this workspace; the `deps` tool lists them"
                )
            })?
        };
        let doc = self.load(&root)?;
        let root_ref = Ref::new(&doc, doc.file.root())
            .ok_or_else(|| anyhow!("docs for `{first}` have no root module"))?;
        let mut current = vec![root_ref];
        let mut walked = vec![first.to_string()];
        for (i, seg) in rest.iter().enumerate() {
            let last = i + 1 == rest.len();
            let mut next = Vec::new();
            for r in &current {
                let found: Vec<Ref> = if matches!(r.item().inner, ItemEnum::Module(_)) {
                    self.children(r)
                        .into_iter()
                        .filter(|(n, _)| n == seg)
                        .filter_map(|(_, t)| match t {
                            Target::Found(r) => Some(r),
                            Target::Named(_) => None,
                        })
                        .collect()
                } else {
                    let matching = |with_traits| -> Vec<Ref> {
                        self.members(r, with_traits)
                            .into_iter()
                            .filter(|(n, _)| n == seg)
                            .map(|(_, r)| r)
                            .collect()
                    };
                    let mut found = matching(false);
                    if found.is_empty() {
                        found = matching(true);
                    }
                    if found.is_empty() {
                        found = self.provided(r, seg);
                    }
                    found
                };
                next.extend(found);
            }
            // Away from the end of the path only containers matter, which drops the function or
            // macro that shares a name with a module.
            if !last {
                next.retain(|r| {
                    matches!(
                        r.item().inner,
                        ItemEnum::Module(_)
                            | ItemEnum::Struct(_)
                            | ItemEnum::Enum(_)
                            | ItemEnum::Union(_)
                            | ItemEnum::Trait(_)
                            | ItemEnum::Primitive(_)
                            | ItemEnum::Variant(_)
                    )
                });
            }
            let mut seen = HashSet::new();
            next.retain(|r| seen.insert(r.key()));
            if next.is_empty() {
                let near = self.near_names(&current, seg);
                bail!(
                    "`{seg}` not found in `{}`{}",
                    walked.join("::"),
                    if near.is_empty() {
                        String::new()
                    } else {
                        format!("; similar names there: {}", near.join(", "))
                    }
                );
            }
            walked.push(seg.to_string());
            current = next;
        }
        Ok((current, walked.join("::")))
    }

    fn near_names(&mut self, scopes: &[Ref], seg: &str) -> Vec<String> {
        let needle = seg.to_lowercase();
        let mut names = Vec::new();
        for r in scopes {
            let all: Vec<String> = if matches!(r.item().inner, ItemEnum::Module(_)) {
                self.children(r).into_iter().map(|(n, _)| n).collect()
            } else {
                self.members(r, true).into_iter().map(|(n, _)| n).collect()
            };
            names.extend(all.into_iter().filter(|n| {
                let l = n.to_lowercase();
                l.contains(&needle) || needle.contains(&l) || edit_distance(&l, &needle) <= 2
            }));
        }
        names.sort();
        names.dedup();
        names.truncate(15);
        names
    }

    /// Public items under `krate` whose name matches `query`, best match first.
    pub fn search(&mut self, krate: &str, query: &str, limit: usize) -> Result<Vec<Hit>> {
        let (roots, root_path) = self.resolve(krate)?;
        let index = self.search_index(roots, &root_path)?;
        let mut scored: Vec<(u8, &IndexEntry)> = index
            .entries
            .iter()
            .filter_map(|e| Some((score(&e.name, query)? + u8::from(e.member), e)))
            .collect();
        scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.path.len().cmp(&b.1.path.len())));
        let mut hits = Vec::new();
        for (_, e) in scored {
            if hits.len() == limit {
                break;
            }
            let doc = self.load(&e.pkg)?;
            if let Some(target) = Ref::new(&doc, Id(e.id)) {
                hits.push(Hit {
                    path: e.path.clone(),
                    target,
                });
            }
        }
        Ok(hits)
    }

    /// Every public item under `root_path`, from memory, from disk, or by walking the docs.
    fn search_index(&mut self, roots: Vec<Ref>, root_path: &str) -> Result<Arc<SearchIndex>> {
        if let Some(index) = lock(&self.store.indexes).get(root_path) {
            return Ok(index.clone());
        }
        let ws = self.ws()?;
        let saved_at = self.store.builder.cache_dir().join("search").join(format!(
            "{:016x}.json",
            fnv1a(&format!("{}\n{root_path}", ws.root.display()))
        ));
        let saved = std::fs::read(&saved_at)
            .ok()
            .and_then(|b| serde_json::from_slice::<SearchIndex>(&b).ok())
            .filter(|index| {
                // Valid while every crate it was built from still resolves to the same cache
                // file; a lockfile or feature change moves at least one of them.
                index.docs.iter().all(|(pkg, path)| {
                    ws.has(pkg) && self.store.builder.doc_path(&ws, pkg) == *path
                })
            });
        let index = Arc::new(match saved {
            Some(index) => index,
            None => {
                let index = self.walk(roots, root_path)?;
                if std::fs::create_dir_all(saved_at.parent().expect("has a parent")).is_ok()
                    && let Ok(bytes) = serde_json::to_vec(&index)
                {
                    let tmp = saved_at.with_extension(format!("{}.tmp", std::process::id()));
                    if std::fs::write(&tmp, bytes).is_ok() {
                        let _ = std::fs::rename(&tmp, &saved_at);
                    }
                }
                index
            }
        });
        lock(&self.store.indexes).insert(root_path.to_string(), index.clone());
        Ok(index)
    }

    /// Walks the public API breadth first, so each item is recorded at its shortest public
    /// path, along with the inherent associated items of every type and trait it reaches.
    fn walk(&mut self, roots: Vec<Ref>, root_path: &str) -> Result<SearchIndex> {
        let mut entries = Vec::new();
        let mut seen: HashSet<(PackageId, Id)> = HashSet::new();
        let mut docs: HashMap<PackageId, PathBuf> = HashMap::new();
        let mut queue: VecDeque<(Ref, String)> = roots
            .into_iter()
            .map(|r| (r, root_path.to_string()))
            .collect();
        while let Some((module, prefix)) = queue.pop_front() {
            if !docs.contains_key(&module.doc.pkg) {
                self.prefetch(&module.doc)?;
                let ws = self.ws()?;
                docs.insert(
                    module.doc.pkg.clone(),
                    self.store.builder.doc_path(&ws, &module.doc.pkg),
                );
            }
            for (name, target) in self.children(&module) {
                let Target::Found(r) = target else {
                    continue;
                };
                if !seen.insert(r.key()) {
                    continue;
                }
                if !docs.contains_key(&r.doc.pkg) {
                    let ws = self.ws()?;
                    docs.insert(
                        r.doc.pkg.clone(),
                        self.store.builder.doc_path(&ws, &r.doc.pkg),
                    );
                }
                let path = format!("{prefix}::{name}");
                if matches!(
                    r.item().inner,
                    ItemEnum::Struct(_)
                        | ItemEnum::Enum(_)
                        | ItemEnum::Union(_)
                        | ItemEnum::Trait(_)
                ) {
                    for (member, m) in self.members(&r, false) {
                        if seen.insert(m.key()) {
                            entries.push(IndexEntry {
                                path: format!("{path}::{member}"),
                                name: member,
                                pkg: m.doc.pkg.clone(),
                                id: m.id.0,
                                member: true,
                            });
                        }
                    }
                }
                entries.push(IndexEntry {
                    name,
                    path: path.clone(),
                    pkg: r.doc.pkg.clone(),
                    id: r.id.0,
                    member: false,
                });
                if matches!(r.item().inner, ItemEnum::Module(_)) {
                    queue.push_back((r, path));
                }
            }
        }
        Ok(SearchIndex {
            docs: docs.into_iter().collect(),
            entries,
        })
    }

    /// Builds, in one cargo run, every crate `doc` re-exports from, so walking a facade crate
    /// such as `bevy` costs a few batched builds instead of one build per subcrate.
    fn prefetch(&mut self, doc: &Arc<Doc>) -> Result<()> {
        let ws = self.ws()?;
        let pkgs: Vec<PackageId> = doc
            .file
            .reexported_crates()
            .iter()
            .filter_map(|n| ws.find_from(&doc.pkg, n))
            .collect();
        self.ensure(&pkgs)
    }
}

/// `#[doc(hidden)]` items are implementation details (`__private` modules, macro support),
/// but the `use` items crates hide are followed, since their targets are public API.
fn is_hidden(item: &Item) -> bool {
    item.attrs
        .iter()
        .any(|a| matches!(a, Attribute::Other(s) if s.contains("doc(hidden)")))
}

/// Match quality, lower is better. Levels are two apart so the one-step penalty search adds
/// for associated items stays inside a level: `World::despawn` matching `despawn` exactly
/// still beats the struct `Despawn`, which matches only without case.
fn score(name: &str, query: &str) -> Option<u8> {
    let l = name.to_lowercase();
    let needle = query.to_lowercase();
    let needle = needle.as_str();
    if name == query {
        Some(0)
    } else if l == needle {
        Some(2)
    } else if l.starts_with(needle) {
        Some(4)
    } else if l.contains(needle) {
        Some(6)
    } else {
        None
    }
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = (prev + usize::from(ca != *cb)).min(row[j] + 1).min(cur + 1);
            prev = cur;
        }
    }
    row[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn score_ranks_exact_then_prefix_then_substring() {
        assert_eq!(score("Commands", "Commands"), Some(0));
        assert_eq!(score("commands", "Commands"), Some(2));
        assert_eq!(score("CommandsExt", "commands"), Some(4));
        assert_eq!(score("EntityCommands", "commands"), Some(6));
        // An exact-case associated item, after search's one-step member penalty, still ranks
        // above a case-insensitive item.
        assert!(score("despawn", "despawn").unwrap() + 1 < score("Despawn", "despawn").unwrap());
        assert_eq!(score("World", "commands"), None);
    }

    #[test]
    fn edit_distance_counts_single_edits() {
        assert_eq!(edit_distance("query", "query"), 0);
        assert_eq!(edit_distance("querry", "query"), 1);
        assert_eq!(edit_distance("qeury", "query"), 2);
        assert_eq!(edit_distance("", "abc"), 3);
    }
}
