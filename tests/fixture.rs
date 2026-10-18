//! Runs the CLI against `tests/fixture`, a two-crate workspace, with a private cache directory.
//! Needs the toolchain pinned in `docbuild::TOOLCHAIN`.

use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixture")
}

fn lockdoc(args: &[&str]) -> (bool, String) {
    let cache = Path::new(env!("CARGO_TARGET_TMPDIR")).join("lockdoc-cache");
    let out = Command::new(env!("CARGO_BIN_EXE_lockdoc"))
        .arg("-C")
        .arg(fixture())
        .args(args)
        .env("XDG_CACHE_HOME", cache)
        .output()
        .expect("run lockdoc");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

fn ok(args: &[&str]) -> String {
    let (success, text) = lockdoc(args);
    assert!(success, "lockdoc {args:?} failed:\n{text}");
    text
}

#[test]
fn item_shows_declaration_docs_and_methods() {
    let out = ok(&["item", "shapes::Circle"]);
    assert!(
        out.contains("struct shapes::Circle  (fixture-shapes 0.3.1)"),
        "{out}"
    );
    assert!(
        out.contains("defined at fixture_shapes::geometry::Circle"),
        "{out}"
    );
    assert!(out.contains("pub radius: f32,"), "{out}");
    assert!(out.contains("/* private fields */"), "{out}");
    assert!(
        out.contains("fn new(radius: f32) -> Self  // Makes a circle."),
        "{out}"
    );
    assert!(out.contains("Trait impls: Area, Clone, Debug"), "{out}");
}

#[test]
fn hidden_glob_reexport_is_followed() {
    let out = ok(&["item", "shapes::prelude::Area"]);
    assert!(out.contains("trait shapes::prelude::Area"), "{out}");
    assert!(
        out.contains("Provided:\n  fn double(&self) -> f32"),
        "{out}"
    );
}

#[test]
fn provided_trait_method_resolves_through_type() {
    let out = ok(&["item", "shapes::Circle::double"]);
    assert!(out.contains("pub fn double(&self) -> f32"), "{out}");
    assert!(out.contains("Twice the area."), "{out}");
}

#[test]
fn features_enabled_by_dependent_apply_to_member() {
    let out = ok(&["item", "shapes::extra::sparkle"]);
    assert!(
        out.contains("Only present with the `extra` feature."),
        "{out}"
    );
}

#[test]
fn hidden_example_lines_are_stripped() {
    let out = ok(&["item", "shapes::Circle::area"]);
    assert!(out.contains("let a = Circle::new(1.0).area();"), "{out}");
    assert!(!out.contains("# use"), "{out}");
}

#[test]
fn search_skips_hidden_items_and_trait_impl_duplicates() {
    let out = ok(&["search", "shapes", "area"]);
    assert_eq!(out.matches("fn shapes::Circle::area").count(), 1, "{out}");
    assert!(out.contains("fn shapes::geometry::Area::area"), "{out}");
    let out = ok(&["search", "shapes", "internal"]);
    assert!(out.contains("No public item"), "{out}");
}

#[test]
fn typo_suggests_near_names() {
    let (success, out) = lockdoc(&["item", "shapes::Circel"]);
    assert!(!success);
    assert!(out.contains("similar names there: Circle"), "{out}");
}

#[test]
fn project_files_are_left_alone() {
    let lock = fixture().join("Cargo.lock");
    let before = std::fs::read(&lock).unwrap();
    ok(&["item", "shapes::Circle"]);
    assert_eq!(std::fs::read(&lock).unwrap(), before);
    assert!(
        !fixture().join("target").exists(),
        "lockdoc created the project's target/"
    );
}
