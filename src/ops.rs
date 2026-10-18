//! The three queries lockdoc answers, shared by the MCP server and the CLI.

use std::fmt::Write as _;

use anyhow::{Result, anyhow};

use crate::describe;
use crate::store::Store;

/// Prepends what had to be built, so a slow answer explains itself.
fn with_note(store: &mut Store, body: Result<String>) -> Result<String> {
    let note = store.take_note();
    match (note, body) {
        (None, body) => body,
        (Some(n), Ok(body)) => Ok(format!("{n}\n\n{body}")),
        (Some(n), Err(e)) => Err(anyhow!("{e:#}\n\n{n}")),
    }
}

pub fn deps(store: &mut Store) -> Result<String> {
    let ws = store.ws()?;
    let mut out = format!("Workspace {}\n", ws.root.display());
    for (name, id) in ws.member_deps() {
        let p = ws.package(&id);
        let features = ws.features(&id);
        let _ = writeln!(
            out,
            "  {name} = {} {}{}{}",
            p.name,
            p.version,
            if ws.is_member(&id) {
                " (workspace member)"
            } else {
                ""
            },
            if features.is_empty() {
                String::new()
            } else {
                format!("  [{}]", features.join(", "))
            }
        );
    }
    Ok(out)
}

pub fn item(store: &mut Store, path: &str) -> Result<String> {
    let body = (|| {
        let (refs, public) = store.resolve(path)?;
        let pages: Vec<String> = refs
            .iter()
            .map(|r| describe::page(store, r, &public))
            .collect();
        Ok(pages.join("\n---\n\n"))
    })();
    with_note(store, body)
}

pub fn search(store: &mut Store, krate: &str, query: &str, limit: usize) -> Result<String> {
    let body = (|| {
        let hits = store.search(krate, query, limit)?;
        if hits.is_empty() {
            return Ok(format!("No public item in `{krate}` matches `{query}`."));
        }
        let mut out = String::new();
        for h in hits {
            let item = h.target.item();
            let s = describe::summary(item);
            let _ = writeln!(
                out,
                "{} {}\n    {}{}",
                describe::kind(item),
                h.path,
                describe::signature(item),
                if s.is_empty() {
                    String::new()
                } else {
                    format!("\n    // {s}")
                }
            );
        }
        Ok(out)
    })();
    with_note(store, body)
}
