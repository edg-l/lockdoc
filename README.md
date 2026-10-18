<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/logo/lockdoc-dark.svg">
  <img src="assets/logo/lockdoc.svg" width="112" height="112" alt="lockdoc logo: a page of docs with a key laid across it as a bookmark">
</picture>

# lockdoc

**API docs for your dependencies, at the versions in your `Cargo.lock`.**

An MCP server for coding agents working in Rust.

[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

</div>

Coding agents write Rust against the API they remember, and they remember whichever version
was common in their training data. When your project pins bevy 0.20 and the agent learned
0.15, it writes calls that no longer exist, and the compiler is the first to notice.

lockdoc gives the agent the real signatures instead. It builds rustdoc JSON for each
dependency at the exact version and feature set your lockfile resolves, and answers lookups
from it:

```text
> item tokio::sync::Mutex

struct tokio::sync::Mutex  (tokio 1.53.3)
defined at tokio::sync::mutex::Mutex

pub struct Mutex<T: ?Sized> {
    /* private fields */
}

An asynchronous `Mutex`-like type.
…

Inherent items (10):
  fn new(t: T) -> Self where T: Sized  // Creates a new lock in an unlocked state ready for use.
  async fn lock(&self) -> MutexGuard<'_, T>  // Locks this mutex, causing the current task to yield until the lock has been acquired.
  fn try_lock(&self) -> Result<MutexGuard<'_, T>, TryLockError>  // Attempts to acquire the lock, and returns [`TryLockError`] if the lock is currently held somewhere else.
  …

Trait impls: Debug, Default, From<T>, Send, Sync
```

Paths resolve the way `use` does, through re-exports and globs, so
`bevy::prelude::Transform` finds the struct in `bevy_transform` and reports which crate and
version it came from.

## Install

lockdoc needs a nightly toolchain, because rustdoc's JSON output is nightly-only. The
nightly is pinned so its output format matches the parser lockdoc is built with.

```sh
rustup toolchain install nightly-2026-10-07
cargo install --git https://github.com/edg-l/lockdoc
```

Then register it with your agent. For Claude Code, across all projects:

```sh
claude mcp add --scope user lockdoc -- lockdoc serve
```

The server works on the cargo workspace it is started in. Claude Code starts MCP servers in
the project directory, so nothing else is needed; every tool also takes a `project` path.

## Tools

| Tool | What it returns |
|---|---|
| `item` | One item by path: declaration, docs, and its methods, fields, variants, trait impls or module contents. `Type::method` shows a single associated item. |
| `search` | Public items under a crate or module whose name matches, each at its shortest public path with its signature. |
| `deps` | The workspace's direct dependencies with locked versions and enabled features. |

The same queries run from the shell, which is handy for checking what the agent sees:

```sh
lockdoc item serde::Serialize
lockdoc search bevy Commands --limit 5
lockdoc -C ~/dev/my-game deps
```

[docs/tools.md](docs/tools.md) has every parameter and the output format.

## It stays out of your project's way

- Builds go to lockdoc's own target directory under `~/.cache/lockdoc`. Your `target/` and
  cargo's build lock on it are never touched, so `cargo build` in the project never waits on
  lockdoc.
- Every cargo command runs with `--frozen`. lockdoc cannot rewrite `Cargo.lock` or reach the
  network; an out-of-date lockfile is reported as an error.
- The toolchain is selected with `+nightly-…`, so a `rust-toolchain.toml` in the project does
  not change it.
- A lookup that had to build says so, with the crates and the time taken, so the agent knows
  why a call was slow and that nothing in the project changed.

## Costs and limits

The first lookup in a crate builds its docs. A small crate takes seconds. A facade such as
`bevy` re-exports around 60 crates, and the first lookup through it took 60 to 170 seconds on
a Ryzen 5 5600, depending on whether the dependencies were already compiled.

After that, everything comes from a cache on disk that new sessions reuse. On the same
machine, with bevy's docs built, a lookup through `bevy::prelude` takes 45 ms from the shell
and under 10 ms from a running server, and a search across all of bevy takes 2 to 12 ms. The
server held 121 MB after those calls.

| Not covered | Why |
|---|---|
| `std`, `core`, `alloc` | Only the project's dependencies are built. |
| Trait impls in downstream crates | A trait's implementors come from its own crate and that crate's dependencies. |
| Caching path dependencies across sessions | They have no version to key the cache on, so each server process rebuilds them once. |

[docs/how-it-works.md](docs/how-it-works.md) covers the build pipeline, cache layout and
re-export resolution.

## License

Apache-2.0. See [LICENSE](LICENSE).
