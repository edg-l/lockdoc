//! The three queries lockdoc answers, shared by the MCP server and the CLI.

use std::fmt::Write as _;

use anyhow::{Result, anyhow};

use crate::describe;
use crate::store::{Session, Store};

/// Prepends what this query had to build, so a slow answer explains itself.
fn with_note(session: &mut Session<'_>, body: Result<String>) -> Result<String> {
    let note = session.take_note();
    match (note, body) {
        (None, body) => body,
        (Some(n), Ok(body)) => Ok(format!("{n}\n\n{body}")),
        (Some(n), Err(e)) => Err(anyhow!("{e:#}\n\n{n}")),
    }
}

pub fn deps(store: &Store) -> Result<String> {
    let ws = store.session().ws()?;
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

/// `path` as `item` resolves it: relative to `krate` when one is given, unless `path` already
/// starts with that crate.
fn qualify(krate: Option<&str>, path: &str) -> String {
    let Some(krate) = krate else {
        return path.to_string();
    };
    let code = |s: &str| s.replace('-', "_");
    let crate_root = krate.split("::").next().unwrap_or(krate);
    let path_root = path.split("::").next().unwrap_or(path);
    if code(path_root) == code(crate_root) {
        path.to_string()
    } else {
        format!("{krate}::{path}")
    }
}

pub fn item(store: &Store, krate: Option<&str>, path: &str) -> Result<String> {
    let path = qualify(krate, path);
    let mut session = store.session();
    let body = (|| {
        let (refs, public) = session.resolve(&path)?;
        let pages: Vec<String> = refs
            .iter()
            .map(|r| describe::page(&mut session, r, &public))
            .collect();
        Ok(pages.join("\n---\n\n"))
    })();
    with_note(&mut session, body)
}

pub fn search(store: &Store, krate: Option<&str>, query: &str, limit: usize) -> Result<String> {
    let mut session = store.session();
    let body = (|| {
        let hits = session.search(krate, query, limit)?;
        if hits.is_empty() {
            return Ok(match krate {
                Some(k) => format!("No public item in `{k}` matches `{query}`."),
                None => format!("No public item in any dependency matches `{query}`."),
            });
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
    with_note(&mut session, body)
}
