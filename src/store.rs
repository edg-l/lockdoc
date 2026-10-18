use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{Context, Result, anyhow, bail};
use cargo_metadata::PackageId;
use rustdoc_types::{
    Attribute, Crate, FORMAT_VERSION, Id, Item, ItemEnum, StructKind, VariantKind,
};

use crate::docbuild::{BuildNote, Builder};
use crate::workspace::Workspace;

/// The rustdoc JSON of one package, with a reverse index from defining path to item.
pub struct Doc {
    pub pkg: PackageId,
    pub krate: Crate,
    by_path: HashMap<Vec<String>, Id>,
}

/// An item inside a specific package's docs.
#[derive(Clone)]
pub struct Ref {
    pub doc: Arc<Doc>,
    pub id: Id,
}

impl Ref {
    pub fn item(&self) -> &Item {
        &self.doc.krate.index[&self.id]
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

pub struct Store {
    dir: PathBuf,
    ws: Option<(Workspace, SystemTime)>,
    builder: Builder,
    docs: HashMap<PackageId, Arc<Doc>>,
    note: BuildNote,
}

impl Store {
    pub fn new(dir: PathBuf) -> Result<Self> {
        Ok(Self {
            dir,
            ws: None,
            builder: Builder::new()?,
            docs: HashMap::new(),
            note: BuildNote::default(),
        })
    }

    /// The workspace graph, reloaded when `Cargo.lock` changes so versions and features stay
    /// those of the lockfile on disk.
    pub fn ws(&mut self) -> Result<&Workspace> {
        let stamp = self.ws.as_ref().and_then(|(ws, _)| lock_mtime(&ws.root));
        let stale = match (&self.ws, stamp) {
            (Some((_, loaded)), Some(now)) => *loaded != now,
            _ => true,
        };
        if stale {
            let ws = Workspace::load(&self.dir)?;
            let mtime = lock_mtime(&ws.root).unwrap_or(SystemTime::UNIX_EPOCH);
            self.ws = Some((ws, mtime));
            self.docs.clear();
        }
        Ok(&self.ws.as_ref().expect("loaded above").0)
    }

    pub fn take_note(&mut self) -> Option<String> {
        std::mem::take(&mut self.note).render()
    }

    fn ensure(&mut self, ids: &[PackageId]) -> Result<()> {
        self.ws()?;
        let ws = &self.ws.as_ref().expect("loaded by ws()").0;
        let note = self.builder.ensure(ws, ids)?;
        self.note.merge(note);
        Ok(())
    }

    pub fn load(&mut self, id: &PackageId) -> Result<Arc<Doc>> {
        if let Some(doc) = self.docs.get(id) {
            return Ok(doc.clone());
        }
        self.ensure(std::slice::from_ref(id))?;
        let ws = &self.ws.as_ref().expect("loaded by ensure()").0;
        let path = self.builder.json_path(ws, id);
        if !path.exists() {
            bail!("no docs for {}", ws.spec(id));
        }
        let bytes = std::fs::read(&path)?;
        let krate: Crate = serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "{} is not rustdoc JSON format {FORMAT_VERSION}; delete it to rebuild",
                path.display()
            )
        })?;
        let by_path = krate
            .paths
            .iter()
            .filter(|(_, s)| s.crate_id == 0)
            .map(|(id, s)| (s.path.clone(), *id))
            .collect();
        let doc = Arc::new(Doc {
            pkg: id.clone(),
            krate,
            by_path,
        });
        self.docs.insert(id.clone(), doc.clone());
        Ok(doc)
    }

    /// Follows `id` as referenced from `doc` to the docs that define it.
    pub fn target(&mut self, doc: &Arc<Doc>, id: Id) -> Target {
        if let Some(item) = doc.krate.index.get(&id)
            && item.crate_id == 0
        {
            return Target::Found(Ref {
                doc: doc.clone(),
                id,
            });
        }
        let Some(summary) = doc.krate.paths.get(&id) else {
            return Target::Named("<unknown>".into());
        };
        let full = summary.path.join("::");
        let Some(ext) = doc.krate.external_crates.get(&summary.crate_id) else {
            return Target::Named(full);
        };
        let Ok(ws) = self.ws() else {
            return Target::Named(full);
        };
        let Some(pkg) = ws.find_from(&doc.pkg, &ext.name) else {
            return Target::Named(full);
        };
        let Ok(ext_doc) = self.load(&pkg) else {
            return Target::Named(full);
        };
        let found = ext_doc
            .by_path
            .get(&summary.path)
            .copied()
            .filter(|id| ext_doc.krate.index.contains_key(id))
            .or_else(|| {
                // A crate's own `paths` table does not always agree with its `index`: proc macros
                // get one id in each. Fall back to the unique indexed item with the same name
                // and kind.
                let name = summary.path.last()?;
                let mut matches = ext_doc
                    .krate
                    .index
                    .values()
                    .filter(|i| {
                        i.crate_id == 0
                            && i.name.as_ref() == Some(name)
                            && i.inner.item_kind() == summary.kind
                    })
                    .map(|i| i.id);
                let first = matches.next()?;
                matches.next().is_none().then_some(first)
            });
        match found {
            Some(id) => Target::Found(Ref { doc: ext_doc, id }),
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
            let Some(item) = r.doc.krate.index.get(&id) else {
                continue;
            };
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
                    if let Some(name) = &item.name {
                        out.push((
                            name.clone(),
                            Target::Found(Ref {
                                doc: r.doc.clone(),
                                id,
                            }),
                        ));
                    }
                }
            }
        }
    }

    /// Fields, variants and associated items of a type, or the items of a trait. Items from a
    /// type's trait impls are included only with `trait_impls`; their docs live on the trait.
    pub fn members(&self, r: &Ref, trait_impls: bool) -> Vec<(String, Ref)> {
        let k = &r.doc.krate;
        let at = |id: &Id| Ref {
            doc: r.doc.clone(),
            id: *id,
        };
        let named = |ids: &[Id]| -> Vec<(String, Ref)> {
            ids.iter()
                .filter_map(|id| Some((k.index.get(id)?.name.clone()?, at(id))))
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
                            .filter_map(|(i, f)| Some((i.to_string(), at(f.as_ref()?)))),
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
        let (inherent, traits): (Vec<&Item>, Vec<&Item>) = impls
            .iter()
            .filter_map(|id| k.index.get(id))
            .filter(|i| matches!(&i.inner, ItemEnum::Impl(im) if im.blanket_impl.is_none()))
            .partition(|i| matches!(&i.inner, ItemEnum::Impl(im) if im.trait_.is_none()));
        let traits = if trait_impls { traits } else { Vec::new() };
        for imp in inherent.into_iter().chain(traits) {
            if let ItemEnum::Impl(im) = &imp.inner {
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
        for id in impls {
            let Some(ItemEnum::Impl(im)) = r.doc.krate.index.get(&id).map(|i| &i.inner) else {
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
        let mut current = vec![Ref {
            id: doc.krate.root,
            doc,
        }];
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

    /// Walks the public API of `krate` breadth first, so each item is reported at its shortest
    /// public path, and returns the items whose name matches `query`.
    pub fn search(&mut self, krate: &str, query: &str, limit: usize) -> Result<Vec<Hit>> {
        let (roots, root_path) = self.resolve(krate)?;
        let mut hits: Vec<(u8, Hit)> = Vec::new();
        let mut seen: HashSet<(PackageId, Id)> = HashSet::new();
        let mut prefetched: HashSet<PackageId> = HashSet::new();
        let mut queue: VecDeque<(Ref, String)> =
            roots.into_iter().map(|r| (r, root_path.clone())).collect();
        while let Some((module, prefix)) = queue.pop_front() {
            if prefetched.insert(module.doc.pkg.clone()) {
                self.prefetch(&module.doc)?;
            }
            for (name, target) in self.children(&module) {
                let Target::Found(r) = target else {
                    continue;
                };
                if !seen.insert(r.key()) {
                    continue;
                }
                let path = format!("{prefix}::{name}");
                if let Some(score) = score(&name, query) {
                    hits.push((
                        score,
                        Hit {
                            path: path.clone(),
                            target: r.clone(),
                        },
                    ));
                }
                match &r.item().inner {
                    ItemEnum::Module(_) => queue.push_back((r, path)),
                    ItemEnum::Struct(_)
                    | ItemEnum::Enum(_)
                    | ItemEnum::Union(_)
                    | ItemEnum::Trait(_) => {
                        for (member, m) in self.members(&r, false) {
                            if let Some(score) = score(&member, query)
                                && seen.insert(m.key())
                            {
                                hits.push((
                                    score + 1,
                                    Hit {
                                        path: format!("{path}::{member}"),
                                        target: m,
                                    },
                                ));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        hits.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.path.len().cmp(&b.1.path.len())));
        Ok(hits.into_iter().take(limit).map(|(_, h)| h).collect())
    }

    /// Builds, in one cargo run, every crate `doc` re-exports from, so walking a facade crate
    /// such as `bevy` costs a few batched builds instead of one build per subcrate.
    fn prefetch(&mut self, doc: &Arc<Doc>) -> Result<()> {
        let names: HashSet<&str> = doc
            .krate
            .index
            .values()
            .filter_map(|i| match &i.inner {
                ItemEnum::Use(u) => u.id,
                _ => None,
            })
            .filter_map(|id| doc.krate.paths.get(&id))
            .filter(|s| s.crate_id != 0)
            .filter_map(|s| doc.krate.external_crates.get(&s.crate_id))
            .map(|e| e.name.as_str())
            .collect();
        let ws = self.ws()?;
        let pkgs: Vec<PackageId> = names
            .into_iter()
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

fn score(name: &str, query: &str) -> Option<u8> {
    let l = name.to_lowercase();
    let needle = query.to_lowercase();
    let needle = needle.as_str();
    if name == query {
        Some(0)
    } else if l == needle {
        Some(1)
    } else if l.starts_with(needle) {
        Some(2)
    } else if l.contains(needle) {
        Some(4)
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

fn lock_mtime(root: &Path) -> Option<SystemTime> {
    std::fs::metadata(root.join("Cargo.lock"))
        .and_then(|m| m.modified())
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn score_ranks_exact_then_prefix_then_substring() {
        assert_eq!(score("Commands", "Commands"), Some(0));
        assert_eq!(score("commands", "Commands"), Some(1));
        assert_eq!(score("CommandsExt", "commands"), Some(2));
        assert_eq!(score("EntityCommands", "commands"), Some(4));
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
