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
    assert!(out.contains("\nfn double(&self) -> f32"), "{out}");
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

#[test]
fn manifest_edit_invalidates_saved_metadata() {
    let copy = Path::new(env!("CARGO_TARGET_TMPDIR")).join("fixture-copy");
    let _ = std::fs::remove_dir_all(&copy);
    for rel in [
        "Cargo.toml",
        "Cargo.lock",
        "app/src/lib.rs",
        "dep/Cargo.toml",
        "dep/src/lib.rs",
    ] {
        let to = copy.join(rel);
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::copy(fixture().join(rel), to).unwrap();
    }
    let manifest = copy.join("app/Cargo.toml");
    let with_extra = std::fs::read_to_string(fixture().join("app/Cargo.toml")).unwrap();
    std::fs::write(&manifest, &with_extra).unwrap();
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_lockdoc"))
            .arg("-C")
            .arg(&copy)
            .args(args)
            .env(
                "XDG_CACHE_HOME",
                Path::new(env!("CARGO_TARGET_TMPDIR")).join("lockdoc-cache"),
            )
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    assert!(run(&["deps"]).contains("[extra]"));
    std::fs::write(
        &manifest,
        with_extra.replace(", features = [\"extra\"]", ""),
    )
    .unwrap();
    assert!(!run(&["deps"]).contains("[extra]"));
}

#[test]
fn exact_case_member_outranks_case_insensitive_item() {
    let out = ok(&["search", "shapes", "area"]);
    let method = out.find("fn shapes::Circle::area").expect(&out);
    let trait_ = out.find("trait shapes::geometry::Area").expect(&out);
    assert!(method < trait_, "{out}");
}

#[test]
fn unrendered_const_value_is_left_out() {
    let out = ok(&["item", "shapes::MAX_RADIUS"]);
    assert!(out.contains("const MAX_RADIUS: f32"), "{out}");
    assert!(!out.contains("= _"), "{out}");
}

/// Drives `lockdoc serve` over stdio with three concurrent calls while `cargo doc` is slowed by
/// a wrapper on PATH: a call that needs no build answers during the build, and a second call
/// needing the same crate waits for it instead of building again.
#[test]
fn queries_run_while_a_build_is_in_progress() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let tmp = Path::new(env!("CARGO_TARGET_TMPDIR")).join("slow-cargo");
    std::fs::create_dir_all(&tmp).unwrap();
    // The rustup proxy on PATH, not `$CARGO`: only the proxy understands `+toolchain`.
    let real_cargo = String::from_utf8(
        Command::new("sh")
            .args(["-c", "command -v cargo"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    let wrapper = tmp.join("cargo");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\ncase \" $* \" in *\" doc \"*) sleep 3;; esac\nexec \"{real_cargo}\" \"$@\"\n"
        ),
    )
    .unwrap();
    let mut perms = std::fs::metadata(&wrapper).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&wrapper, perms).unwrap();
    let path = format!("{}:{}", tmp.display(), std::env::var("PATH").unwrap());

    let mut child = Command::new(env!("CARGO_BIN_EXE_lockdoc"))
        .arg("-C")
        .arg(fixture())
        .arg("serve")
        .env("PATH", path)
        .env("XDG_CACHE_HOME", tmp.join("cache"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut send = |v: serde_json::Value| writeln!(stdin, "{v}").unwrap();
    let call = |id: u32, name: &str, args: serde_json::Value| {
        serde_json::json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": name, "arguments": args}})
    };

    send(
        serde_json::json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": "test", "version": "0"}}}),
    );
    lines.next().unwrap().unwrap();
    send(serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));

    let start = Instant::now();
    send(call(
        1,
        "item",
        serde_json::json!({"path": "shapes::Circle"}),
    ));
    std::thread::sleep(Duration::from_millis(300));
    send(call(2, "deps", serde_json::json!({})));
    send(call(
        3,
        "item",
        serde_json::json!({"path": "shapes::geometry::Kind"}),
    ));

    let mut answers = Vec::new();
    while answers.len() < 3 {
        let msg: serde_json::Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
        if let Some(id) = msg["id"].as_u64() {
            let text = msg["result"]["content"][0]["text"]
                .as_str()
                .unwrap_or("")
                .to_string();
            answers.push((id, start.elapsed(), text));
        }
    }
    child.kill().unwrap();
    child.wait().unwrap();

    let (_, deps_at, deps) = answers.iter().find(|a| a.0 == 2).unwrap();
    assert!(deps.contains("fixture-shapes"), "{deps}");
    assert!(
        *deps_at < Duration::from_secs(2),
        "deps waited for the build: answered after {deps_at:?}"
    );
    let built = answers
        .iter()
        .filter(|a| a.2.contains("lockdoc built docs for fixture-shapes"))
        .count();
    assert_eq!(built, 1, "expected exactly one build: {answers:#?}");
    let (_, _, kind) = answers.iter().find(|a| a.0 == 3).unwrap();
    assert!(kind.contains("enum shapes::geometry::Kind"), "{kind}");
}
