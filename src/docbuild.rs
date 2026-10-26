use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use cargo_metadata::PackageId;

use rustdoc_types::{Crate, FORMAT_VERSION};

use crate::cachefile;
use crate::workspace::Workspace;

/// The nightly whose rustdoc emits `rustdoc_types::FORMAT_VERSION`. Rustdoc JSON is nightly-only
/// and its format changes between nightlies, so the toolchain and the `rustdoc-types` version
/// move together.
pub const TOOLCHAIN: &str = "nightly-2026-10-07";

/// `--document-hidden-items` because crates hide re-exports that are public API: bevy's prelude
/// is a list of `#[doc(hidden)] pub use …::prelude::*`.
const RUSTDOCFLAGS: &str = "-Zunstable-options --output-format json --document-hidden-items";

/// Builds rustdoc JSON for dependencies and caches it under the user cache directory.
///
/// Every cargo run uses a target directory owned by lockdoc, so the project's `target/` and its
/// build lock are never touched, and `--locked` so `Cargo.lock` is never rewritten.
///
/// Shared by concurrent queries. Only `build_lock` is held while cargo runs, so a query whose
/// docs are already cached never waits behind a build.
pub struct Builder {
    cache: PathBuf,
    /// Path dependencies have no version to key a cache entry on, so they are rebuilt once per
    /// process instead.
    built_path_deps: Mutex<HashSet<PathBuf>>,
    toolchain_checked: AtomicBool,
    build_lock: Mutex<()>,
}

/// What a call had to build, reported back to the agent so a slow call is explained.
#[derive(Default)]
pub struct BuildNote {
    pub built: Vec<String>,
    pub failed: Vec<(String, String)>,
    pub elapsed: Duration,
}

impl BuildNote {
    pub fn merge(&mut self, other: BuildNote) {
        self.built.extend(other.built);
        self.failed.extend(other.failed);
        self.elapsed += other.elapsed;
    }

    pub fn render(&self) -> Option<String> {
        if self.built.is_empty() && self.failed.is_empty() {
            return None;
        }
        let mut s = String::new();
        if !self.built.is_empty() {
            let _ = write!(
                s,
                "[lockdoc built docs for {} in {:.0}s, in its own target dir; the project's target/ and Cargo.lock were not touched]",
                list(&self.built),
                self.elapsed.as_secs_f64()
            );
        }
        for (spec, err) in &self.failed {
            let _ = write!(s, "\n[lockdoc could not build docs for {spec}: {err}]");
        }
        Some(s)
    }
}

impl Builder {
    pub fn new() -> Result<Self> {
        let cache = dirs::cache_dir()
            .context("no user cache directory")?
            .join("lockdoc");
        Ok(Self {
            cache,
            built_path_deps: Mutex::default(),
            toolchain_checked: AtomicBool::new(false),
            build_lock: Mutex::new(()),
        })
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache
    }

    /// Where the docs for `id` live once built. Keyed on everything that changes the output:
    /// source (registry, git commit), version, enabled features, toolchain and cache format.
    pub fn doc_path(&self, ws: &Workspace, id: &PackageId) -> PathBuf {
        let p = ws.package(id);
        let source = p
            .source
            .as_ref()
            .map_or_else(|| p.manifest_path.as_str().to_string(), |s| s.repr.clone());
        let key = fnv1a(
            &[
                &source,
                &ws.features(id).join(","),
                TOOLCHAIN,
                RUSTDOCFLAGS,
                &cachefile::VERSION.to_string(),
            ]
            .join("\n"),
        );
        self.cache
            .join("docs")
            .join(format!("{}-{}-{key:016x}.ldoc", p.name, p.version))
    }

    fn is_fresh(&self, ws: &Workspace, id: &PackageId) -> bool {
        let path = self.doc_path(ws, id);
        if ws.package(id).source.is_none() {
            return lock(&self.built_path_deps).contains(&path);
        }
        path.exists()
    }

    /// Ensures JSON exists for every package in `ids`, building the missing ones in as few
    /// cargo runs as possible. Packages that fail are reported in the note, not as an error,
    /// so one crate that needs a system library does not hide its siblings.
    pub fn ensure(&self, ws: &Workspace, ids: &[PackageId]) -> Result<BuildNote> {
        let missing = || -> Vec<PackageId> {
            let mut out: Vec<PackageId> = Vec::new();
            for id in ids {
                if !self.is_fresh(ws, id) && !out.contains(id) && ws.lib_name(id).is_some() {
                    out.push(id.clone());
                }
            }
            out
        };
        let mut note = BuildNote::default();
        if missing().is_empty() {
            return Ok(note);
        }
        self.check_toolchain()?;
        let start = Instant::now();
        let _building = lock(&self.build_lock);
        // Another query may have built some of these while this one waited for the lock.
        let missing = missing();
        if missing.is_empty() {
            return Ok(note);
        }
        // Rustdoc writes `<lib name>.json`, so two versions of one crate need separate runs.
        let mut batches: Vec<Vec<PackageId>> = Vec::new();
        for id in missing {
            let lib = ws.lib_name(&id);
            match batches
                .iter_mut()
                .find(|b| b.iter().all(|other| ws.lib_name(other) != lib))
            {
                Some(batch) => batch.push(id),
                None => batches.push(vec![id]),
            }
        }
        for batch in batches {
            if self.run(ws, &batch).is_ok() {
                self.collect(ws, &batch, &mut note)?;
                continue;
            }
            // Isolate the failing crate so the rest of the batch still gets built.
            for id in &batch {
                match self.run(ws, std::slice::from_ref(id)) {
                    Ok(()) => self.collect(ws, std::slice::from_ref(id), &mut note)?,
                    Err(e) => note.failed.push((ws.spec(id), format!("{e:#}"))),
                }
            }
        }
        note.elapsed = start.elapsed();
        Ok(note)
    }

    fn target_dir(&self, ws: &Workspace) -> PathBuf {
        let key = fnv1a(&ws.root.to_string_lossy());
        self.cache.join("target").join(format!("{key:016x}"))
    }

    fn run(&self, ws: &Workspace, batch: &[PackageId]) -> Result<()> {
        let mut cmd = Command::new("cargo");
        cmd.arg(format!("+{TOOLCHAIN}"))
            .args(["doc", "--locked", "--no-deps", "--quiet"])
            .current_dir(&ws.root)
            .env("CARGO_TARGET_DIR", self.target_dir(ws))
            .env("RUSTDOCFLAGS", RUSTDOCFLAGS)
            // The project's pinned toolchain must not override the one the format needs.
            .env_remove("RUSTUP_TOOLCHAIN");
        for id in batch {
            cmd.args(["-p", &ws.spec(id)]);
            // A selected workspace member builds with its own default features rather than the
            // ones its dependents enable; non-members get the unified set without help, and
            // cargo rejects `--features` for them.
            if ws.is_member(id) {
                let name = &ws.package(id).name;
                for f in ws.features(id) {
                    cmd.args(["--features", &format!("{name}/{f}")]);
                }
            }
        }
        let out = cmd.output().context("running cargo doc")?;
        if !out.status.success() {
            bail!(
                "cargo doc failed:\n{}",
                excerpt(&String::from_utf8_lossy(&out.stderr))
            );
        }
        Ok(())
    }

    fn collect(&self, ws: &Workspace, batch: &[PackageId], note: &mut BuildNote) -> Result<()> {
        let doc_dir = self.target_dir(ws).join("doc");
        for id in batch {
            let lib = ws.lib_name(id).expect("batched packages have a library");
            let dest = self.doc_path(ws, id);
            std::fs::create_dir_all(dest.parent().expect("cache path has a parent"))?;
            convert(&doc_dir.join(format!("{lib}.json")), &dest)?;
            if ws.package(id).source.is_none() {
                lock(&self.built_path_deps).insert(dest);
            }
            note.built.push(ws.spec(id));
        }
        Ok(())
    }

    fn check_toolchain(&self) -> Result<()> {
        if self.toolchain_checked.load(Ordering::Relaxed) {
            return Ok(());
        }
        let ok = Command::new("rustup")
            .args(["run", TOOLCHAIN, "rustdoc", "--version"])
            .output()
            .is_ok_and(|o| o.status.success());
        if !ok {
            bail!(
                "toolchain {TOOLCHAIN} is not installed; run `rustup toolchain install {TOOLCHAIN}`"
            );
        }
        self.toolchain_checked.store(true, Ordering::Relaxed);
        Ok(())
    }
}

/// The part of cargo's stderr that says what went wrong: 15 lines from the first error or panic,
/// else the last 15. A panic is followed by a long debug dump, so the tail alone loses the cause.
fn excerpt(stderr: &str) -> String {
    const LINES: usize = 15;
    let lines: Vec<&str> = stderr.lines().collect();
    let start = lines
        .iter()
        .position(|l| l.trim_start().starts_with("error") || l.contains("panicked at"))
        .unwrap_or(lines.len().saturating_sub(LINES));
    let end = lines.len().min(start + LINES);
    let mut out = lines[start..end].join("\n");
    if end < lines.len() {
        out.push_str("\n…");
    }
    out
}

/// Names the first few crates; a facade such as bevy builds dozens at once.
fn list(specs: &[String]) -> String {
    const SHOWN: usize = 5;
    if specs.len() <= SHOWN {
        specs.join(", ")
    } else {
        format!(
            "{} and {} more",
            specs[..SHOWN].join(", "),
            specs.len() - SHOWN
        )
    }
}

/// Parses rustdoc's JSON once and stores it in the cache format.
fn convert(json: &Path, to: &Path) -> Result<()> {
    let bytes = std::fs::read(json).with_context(|| format!("reading {}", json.display()))?;
    let krate: Crate = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "{} is not rustdoc JSON format {FORMAT_VERSION}; is {TOOLCHAIN} the toolchain rustdoc-types expects?",
            json.display()
        )
    })?;
    cachefile::write(&krate, to)
}

/// FNV-1a, for cache keys that must stay the same across Rust releases (`DefaultHasher` does not
/// promise that).
/// Locks `m`, carrying on past a poisoned lock: every value guarded in lockdoc is a cache that
/// stays valid even if a query panicked while holding it.
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

pub fn fnv1a(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}
