# Tools

Every tool returns plain text. Every tool takes an optional `project`: a directory inside the
cargo workspace to answer for. Without it, the server's working directory is used.

When a call had to build documentation, the text starts with a line in brackets naming the
crates built and the time taken:

```text
[lockdoc built docs for tokio@1.53.3, tokio-macros@2.7.2 in 2s, in its own target dir; the project's target/ and Cargo.lock were not touched]
```

A crate whose docs failed to build is reported on its own bracketed line with cargo's error.
The rest of the answer still uses every crate that did build.

## `item`

Shows one item.

| Parameter | Type | Required | Meaning |
|---|---|---|---|
| `path` | string | yes | Path as written in code. The first segment is the crate name as used in code (`serde_json`), or the package name (`serde-json`). |
| `project` | string | no | Workspace directory. |

The path resolves the way a `use` statement does: through `pub use` re-exports, glob
re-exports and re-exported crates. The last segment can also name an associated item:
`Type::method`, `Trait::method`, `Enum::Variant`, `Struct::field`. A method a type gets from a
trait's default implementation resolves to the trait's method.

Generic arguments in the path are ignored, so `Vec<T>::push` reads as `Vec::push`.

The page contains, in order:

1. `<kind> <path you asked for>  (<package> <version>)`.
2. `defined at <path>` when the item is defined somewhere other than the path asked for.
3. `DEPRECATED`, `#[must_use]`, `#[non_exhaustive]` and `cfg` attributes, when present.
4. The declaration. Structs and enums show every public field and variant; private fields
   show as `/* private fields */`.
5. The docs. Lines rustdoc hides in examples (`# use …`) are removed. Docs over 4,000 bytes
   are cut, 1,200 bytes for modules.
6. Depending on the kind:
   - Struct, enum, union: inherent methods and associated items with the first sentence of
     their docs, then trait impls, then auto traits.
   - Trait: required and provided items, then the types it is implemented for in its own
     crate and that crate's dependencies.
   - Module: its public items, following re-exports.

Lists stop at 80 entries and say how many were left out.

When one path names several items, each gets a page, separated by `---`. `serde::Serialize`
returns the trait and the derive macro.

Errors:

| Condition | Message |
|---|---|
| First segment is not a dependency | `` `x` is not a dependency of this workspace; the `deps` tool lists them `` |
| A later segment does not exist | `` `Seg` not found in `crate::path`; similar names there: … `` |
| Path starts with `std`, `core` or `alloc` | `lockdoc covers the project's dependencies, not the standard library` |
| `Cargo.lock` is out of date | cargo's `--frozen` error |
| Pinned nightly missing | `toolchain nightly-… is not installed; run rustup toolchain install nightly-…` |

## `search`

Finds public items by name.

| Parameter | Type | Required | Meaning |
|---|---|---|---|
| `crate` | string | yes | Crate or module to search under: `bevy`, `bevy::ecs`. |
| `query` | string | yes | Name or part of a name. |
| `limit` | integer | no | Maximum results. Default 20. |
| `project` | string | no | Workspace directory. |

The search walks the public API breadth first, following re-exports into other crates, so
each item is reported once, at its shortest public path. It matches item names and the names
of inherent associated items (`World::commands`). Items marked `#[doc(hidden)]` are skipped,
but hidden re-exports are followed.

Results are ordered by match quality, then by path length:

1. exact name, same case;
2. exact name, any case;
3. name starts with the query;
4. name contains the query.

An associated item ranks just below a module-level item with the same quality of match, and
above any item with a worse match, so `despawn` lists the `despawn` methods before the
`Despawn` struct.

Each result is three lines: kind and path, the one-line signature, and the first sentence of
the docs.

Searching a facade crate builds every crate it re-exports. For `bevy` that is the whole
engine; see the cost notes in the README.

## `deps`

Lists the direct dependencies of every workspace member.

| Parameter | Type | Required | Meaning |
|---|---|---|---|
| `project` | string | no | Workspace directory. |

One line per dependency: the name used in code, the package name and locked version, a
`(workspace member)` marker for path crates inside the workspace, and the enabled features in
brackets. `deps` reads `cargo metadata` only and never builds anything.

## Command line

The binary answers the same queries without an MCP client.

```text
lockdoc [-C <dir>] serve                         # MCP server on stdio; the default
lockdoc [-C <dir>] deps
lockdoc [-C <dir>] item <path>
lockdoc [-C <dir>] search <crate> <query> [--limit N]
```

`-C` sets the workspace directory, default `.`. Build notes and results print to stdout,
errors to stderr with exit status 1.
