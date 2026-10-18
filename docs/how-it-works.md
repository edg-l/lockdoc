# How lockdoc works

## Where the docs come from

Rustdoc can emit JSON instead of HTML (`--output-format json`, nightly only). The JSON holds
every item of a crate with its signature, docs, impls and re-exports as structured data,
which is what lockdoc reads.

lockdoc builds that JSON locally, inside your workspace's dependency resolution:

```sh
cargo +nightly-2026-10-07 doc --frozen --no-deps -p tokio@1.53.3 -p serde@1.0.229 …
# with CARGO_TARGET_DIR=~/.cache/lockdoc/target/<workspace hash>
#  and RUSTDOCFLAGS="-Zunstable-options --output-format json --document-hidden-items"
```

Running in the workspace is what makes the answer match your build. Cargo picks the version
from `Cargo.lock` and turns on the features your crates enable, so an item behind a feature
you use is there and one behind a feature you don't use is not.

docs.rs also serves rustdoc JSON, and lockdoc does not use it. docs.rs builds each crate with
its default features rather than yours. Its builds also span many format versions, while one
parser reads one format, and versions published before docs.rs started producing JSON have
none. A local build gives one format, your features and no network.

### Features of workspace members

Cargo unifies features across the workspace for a dependency selected with `-p`, but not for
a workspace member: `cargo doc -p member` builds the member with its own default features.
For members, lockdoc passes the features `cargo metadata` resolved, as
`--features member/feat`. Cargo refuses `--features` for packages outside the workspace, and
does not need it for them.

### The pinned nightly

The JSON format changes between nightlies, and each format version has a matching release of
the `rustdoc-types` crate. lockdoc pins both: `docbuild::TOOLCHAIN` names the nightly and
`Cargo.toml` names the `rustdoc-types` release whose `FORMAT_VERSION` that nightly emits.
Moving to a newer nightly means bumping both together.

### Hidden items

By default rustdoc leaves `#[doc(hidden)]` items out of the JSON. Some crates hide re-exports
that are public API: bevy's prelude is a list of `#[doc(hidden)] pub use …::prelude::*`. lockdoc
builds with `--document-hidden-items` so those re-exports exist, then filters hidden items
(`__private` modules, macro support code) out of listings and search. A hidden `use` is still
followed, because what it points to is public.

## The cache

```text
~/.cache/lockdoc/
├── docs/<package>-<version>-<key>.ldoc   one file per built crate
├── metadata/<dir hash>.json              cargo metadata output, per project directory
├── search/<hash>.json                    flattened public API, per search root
└── target/<workspace hash>/              cargo's target dir for that workspace
```

Everything a session computes is kept here, so a new session starts warm.

### Doc files

rustdoc's JSON for a large crate is tens of megabytes, and a lookup through bevy's prelude
reaches about 60 crates while reading a few dozen items. Parsing those 60 files whole costs
about 1.3 seconds and 1 GB of memory. lockdoc parses each crate's JSON once, when it is
built, and stores it as a `.ldoc` file: one JSON blob per item, behind four sorted tables (id
to item, id to path summary, defining path to id, name to ids). lockdoc memory-maps the file
and binary-searches the tables in place. Opening a crate parses nothing, and only the items a
query touches are deserialized. `cachefile.rs` documents the layout.

`<key>` hashes everything that changes the output: the package source (registry, or git
commit), the enabled features, the toolchain, the rustdoc flags and the `.ldoc` format
version. A different feature set in another project gets its own file. The hash is FNV-1a,
which gives the same key across Rust releases; std's `DefaultHasher` does not promise that.

Path dependencies have no version or commit to key on. They are rebuilt the first time each
server process needs them.

Files are written under a temporary name containing the process id, then renamed into place.
Two lockdoc processes working on one project never share a temporary file, and a reader maps
either the old file or the new one, never a partial write.

### Metadata

`cargo metadata` takes about 0.3 seconds, and every lookup needs the dependency graph it
returns. Its output is saved with the modification times of the files it was derived from:
`Cargo.lock`, the root `Cargo.toml` and the manifest of every path package. The saved copy is
used while all of those are unchanged. A running server checks the same times before each
call and, on a change, reloads the graph and drops what it holds in memory.

### Search index

The first search under a root walks the whole public API once and records each item's name,
shortest public path and location. Later searches scan that list and deserialize only the
hits they return. The index is saved with the cache file of every crate it was built from,
and is discarded when any of those crates resolves to a different file.

## Resolving a path

`bevy::prelude::Transform` is resolved one segment at a time, the way `use` resolves it.

1. `bevy` is matched against the dependencies of the workspace members, by the name used in
   code and then by package name.
2. Each later segment is looked up among the names visible in the current module: the items
   it defines, the names it re-exports with `pub use`, and everything brought in by glob
   re-exports.
3. A re-export can point into another crate. The JSON records such a target by its defining
   path, such as `bevy_transform::components::transform::Transform`. lockdoc finds the crate
   that provides it (the nearest package with that library name in the dependency graph, so
   the right one is picked when two versions are present), builds it if needed, and looks the
   path up there.
4. Once the current item is a type or trait, the next segment names a field, variant or
   associated item. Inherent items are checked first, then items in trait impls, then trait
   methods the type gets from a default implementation.

A crate's own JSON does not always agree with itself. Proc macros, for example, get one id in
the `paths` table and another in the `index`. When the path lookup fails, lockdoc falls back
to the one indexed item with the same name and kind.

## Search

Search walks the public module tree breadth first from the given root, following the same
re-export rules. Breadth first means the first path that reaches an item is the shortest,
and that is the one reported.

Before walking a crate, lockdoc collects every crate that crate re-exports from and builds
the missing ones in one cargo run. A facade such as `bevy` then costs a few batched builds
instead of one per subcrate. If a batch fails, its crates are retried one at a time, so a
crate that needs a missing system library does not block the others.
