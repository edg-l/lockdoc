use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use cargo_metadata::PackageId;

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
/// build lock are never touched, and `--frozen` so `Cargo.lock` is never rewritten.
pub struct Builder {
    cache: PathBuf,
    /// Path dependencies have no version to key a cache entry on, so they are rebuilt once per
    /// process instead.
    built_path_deps: HashSet<PathBuf>,
    toolchain_checked: bool,
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
            built_path_deps: HashSet::new(),
            toolchain_checked: false,
        })
    }

    /// Where the JSON for `id` lives once built. Keyed on everything that changes the output:
    /// source (registry, git commit), version, enabled features and toolchain.
    pub fn json_path(&self, ws: &Workspace, id: &PackageId) -> PathBuf {
        let p = ws.package(id);
        let source = p
            .source
            .as_ref()
            .map_or_else(|| p.manifest_path.as_str().to_string(), |s| s.repr.clone());
        let key = fnv1a(&[&source, &ws.features(id).join(","), TOOLCHAIN, RUSTDOCFLAGS].join("\n"));
        self.cache
            .join("json")
            .join(format!("{}-{}-{key:016x}.json", p.name, p.version))
    }

    fn is_fresh(&self, ws: &Workspace, id: &PackageId) -> bool {
        let path = self.json_path(ws, id);
        if ws.package(id).source.is_none() {
            return self.built_path_deps.contains(&path);
        }
        path.exists()
    }

    /// Ensures JSON exists for every package in `ids`, building the missing ones in as few
    /// cargo runs as possible. Packages that fail are reported in the note, not as an error,
    /// so one crate that needs a system library does not hide its siblings.
    pub fn ensure(&mut self, ws: &Workspace, ids: &[PackageId]) -> Result<BuildNote> {
        let mut missing: Vec<PackageId> = Vec::new();
        for id in ids {
            if !self.is_fresh(ws, id) && !missing.contains(id) && ws.lib_name(id).is_some() {
                missing.push(id.clone());
            }
        }
        let mut note = BuildNote::default();
        if missing.is_empty() {
            return Ok(note);
        }
        self.check_toolchain()?;
        let start = Instant::now();
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
            .args(["doc", "--frozen", "--no-deps", "--quiet"])
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
            let stderr = String::from_utf8_lossy(&out.stderr);
            let tail: Vec<&str> = stderr.lines().rev().take(15).collect();
            bail!(
                "cargo doc failed:\n{}",
                tail.into_iter().rev().collect::<Vec<_>>().join("\n")
            );
        }
        Ok(())
    }

    fn collect(&mut self, ws: &Workspace, batch: &[PackageId], note: &mut BuildNote) -> Result<()> {
        let doc_dir = self.target_dir(ws).join("doc");
        for id in batch {
            let lib = ws.lib_name(id).expect("batched packages have a library");
            let dest = self.json_path(ws, id);
            std::fs::create_dir_all(dest.parent().expect("cache path has a parent"))?;
            copy(&doc_dir.join(format!("{lib}.json")), &dest)?;
            if ws.package(id).source.is_none() {
                self.built_path_deps.insert(dest);
            }
            note.built.push(ws.spec(id));
        }
        Ok(())
    }

    fn check_toolchain(&mut self) -> Result<()> {
        if self.toolchain_checked {
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
        self.toolchain_checked = true;
        Ok(())
    }
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

fn copy(from: &Path, to: &Path) -> Result<()> {
    // Copy then rename, so a reader never sees a half-written file. The pid keeps two lockdoc
    // processes on one project from sharing a temporary file.
    let tmp = to.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::copy(from, &tmp).with_context(|| format!("copying {}", from.display()))?;
    std::fs::rename(&tmp, to)?;
    Ok(())
}

/// FNV-1a, for cache keys that must stay the same across Rust releases (`DefaultHasher` does not
/// promise that).
fn fnv1a(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}
