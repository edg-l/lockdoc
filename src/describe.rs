//! Formats items as compact text pages for an agent to read.

use std::fmt::Write as _;

use rustdoc_types::{Attribute, Item, ItemEnum, MacroKind, StructKind, Type, VariantKind};

use crate::render;
use crate::store::{Ref, Store, Target};

const DOCS_CAP: usize = 4000;
const MODULE_DOCS_CAP: usize = 1200;
const LIST_CAP: usize = 80;

pub fn kind(item: &Item) -> &'static str {
    match &item.inner {
        ItemEnum::Module(_) => "mod",
        ItemEnum::ExternCrate { .. } => "extern crate",
        ItemEnum::Use(_) => "use",
        ItemEnum::Union(_) => "union",
        ItemEnum::Struct(_) => "struct",
        ItemEnum::StructField(_) => "field",
        ItemEnum::Enum(_) => "enum",
        ItemEnum::Variant(_) => "variant",
        ItemEnum::Function(_) => "fn",
        ItemEnum::Trait(_) => "trait",
        ItemEnum::TraitAlias(_) => "trait alias",
        ItemEnum::Impl(_) => "impl",
        ItemEnum::TypeAlias(_) => "type",
        ItemEnum::Constant { .. } => "const",
        ItemEnum::Static(_) => "static",
        ItemEnum::ExternType => "extern type",
        ItemEnum::Macro(_) => "macro",
        ItemEnum::ProcMacro(p) => match p.kind {
            MacroKind::Bang => "macro",
            MacroKind::Attr => "attribute macro",
            MacroKind::Derive => "derive macro",
        },
        ItemEnum::Primitive(_) => "primitive",
        ItemEnum::AssocConst { .. } => "assoc const",
        ItemEnum::AssocType { .. } => "assoc type",
    }
}

/// The first sentence of an item's docs, for one-line listings.
pub fn summary(item: &Item) -> String {
    let docs = item.docs.as_deref().unwrap_or("");
    let para = docs.split("\n\n").next().unwrap_or("").replace('\n', " ");
    let sentence = match para.find(". ") {
        Some(i) => &para[..=i],
        None => para.as_str(),
    };
    let s = sentence.trim();
    if s.chars().count() > 120 {
        format!("{}…", s.chars().take(119).collect::<String>())
    } else {
        s.to_string()
    }
}

/// One-line declaration of an item, as it would read in source.
pub fn signature(item: &Item) -> String {
    let name = item.name.as_deref().unwrap_or("_");
    match &item.inner {
        ItemEnum::Function(f) => render::function(name, f),
        ItemEnum::Struct(s) => {
            let g = render::generic_params(&s.generics.params);
            let w = render::where_clause(&s.generics);
            match &s.kind {
                StructKind::Unit => format!("struct {name}{g}{w};"),
                StructKind::Tuple(_) => format!("struct {name}{g}(…){w};"),
                StructKind::Plain { .. } => format!("struct {name}{g}{w} {{ … }}"),
            }
        }
        ItemEnum::Enum(e) => format!(
            "enum {name}{}{} {{ … }}",
            render::generic_params(&e.generics.params),
            render::where_clause(&e.generics)
        ),
        ItemEnum::Union(u) => format!(
            "union {name}{}{} {{ … }}",
            render::generic_params(&u.generics.params),
            render::where_clause(&u.generics)
        ),
        ItemEnum::Trait(t) => {
            let mut s = String::new();
            if t.is_unsafe {
                s += "unsafe ";
            }
            if t.is_auto {
                s += "auto ";
            }
            s += &format!("trait {name}{}", render::generic_params(&t.generics.params));
            if !t.bounds.is_empty() {
                s += &format!(": {}", render::bounds(&t.bounds));
            }
            s + &render::where_clause(&t.generics)
        }
        ItemEnum::TraitAlias(t) => format!(
            "trait {name}{} = {}{};",
            render::generic_params(&t.generics.params),
            render::bounds(&t.params),
            render::where_clause(&t.generics)
        ),
        ItemEnum::TypeAlias(t) => format!(
            "type {name}{} = {}{};",
            render::generic_params(&t.generics.params),
            render::ty(&t.type_),
            render::where_clause(&t.generics)
        ),
        ItemEnum::Constant { type_, const_ } => {
            format!(
                "const {name}: {}{};",
                render::ty(type_),
                const_value(Some(&const_.expr))
            )
        }
        ItemEnum::Static(s) => format!(
            "static {}{name}: {};",
            if s.is_mutable { "mut " } else { "" },
            render::ty(&s.type_)
        ),
        ItemEnum::StructField(t) => format!("{name}: {}", render::ty(t)),
        ItemEnum::Variant(v) => variant(name, &v.kind),
        ItemEnum::AssocConst { type_, value, .. } => format!(
            "const {name}: {}{};",
            render::ty(type_),
            const_value(value.as_deref())
        ),
        ItemEnum::AssocType {
            generics,
            bounds,
            type_,
            ..
        } => {
            let mut s = format!("type {name}{}", render::generic_params(&generics.params));
            if !bounds.is_empty() {
                s += &format!(": {}", render::bounds(bounds));
            }
            if let Some(t) = type_ {
                s += &format!(" = {}", render::ty(t));
            }
            s + ";"
        }
        ItemEnum::ProcMacro(p) => match p.kind {
            MacroKind::Derive if p.helpers.is_empty() => format!("#[derive({name})]"),
            MacroKind::Derive => {
                format!(
                    "#[derive({name})]  // helper attributes: {}",
                    p.helpers.join(", ")
                )
            }
            MacroKind::Attr => format!("#[{name}]"),
            MacroKind::Bang => format!("{name}!(…)"),
        },
        ItemEnum::Macro(_) => format!("{name}!(…)"),
        ItemEnum::Module(_) => format!("mod {name}"),
        ItemEnum::Primitive(_) => format!("primitive {name}"),
        _ => format!("{} {name}", kind(item)),
    }
}

/// ` = <expr>`, or nothing when rustdoc did not render the expression: it writes `_` for any
/// value that is not a literal.
fn const_value(expr: Option<&str>) -> String {
    match expr {
        None | Some("_") => String::new(),
        Some(e) => format!(" = {e}"),
    }
}

fn variant(name: &str, kind: &VariantKind) -> String {
    match kind {
        VariantKind::Plain => name.to_string(),
        VariantKind::Tuple(_) => format!("{name}(…)"),
        VariantKind::Struct { .. } => format!("{name} {{ … }}"),
    }
}

/// Full declaration with fields or variants spelled out.
fn declaration(r: &Ref) -> String {
    let item = r.item();
    let k = &r.doc.file;
    let field = |id: &rustdoc_types::Id| -> Option<String> {
        let f = k.item(*id)?;
        match &f.inner {
            ItemEnum::StructField(t) => {
                Some(format!("pub {}: {}", f.name.as_deref()?, render::ty(t)))
            }
            _ => None,
        }
    };
    let tuple_field = |f: &Option<rustdoc_types::Id>| -> String {
        f.as_ref()
            .and_then(|id| k.item(*id))
            .and_then(|f| match &f.inner {
                ItemEnum::StructField(t) => Some(format!("pub {}", render::ty(t))),
                _ => None,
            })
            .unwrap_or_else(|| "_".into())
    };
    let name = item.name.as_deref().unwrap_or("_");
    match &item.inner {
        ItemEnum::Struct(s) => {
            let head = format!(
                "pub struct {name}{}",
                render::generic_params(&s.generics.params)
            );
            let w = render::where_clause(&s.generics);
            match &s.kind {
                StructKind::Unit => format!("{head}{w};"),
                StructKind::Tuple(fs) => format!(
                    "{head}({}){w};",
                    fs.iter().map(tuple_field).collect::<Vec<_>>().join(", ")
                ),
                StructKind::Plain {
                    fields,
                    has_stripped_fields,
                } => {
                    let mut body: Vec<String> = fields.iter().filter_map(field).collect();
                    if *has_stripped_fields {
                        body.push("/* private fields */".into());
                    }
                    braces(&format!("{head}{w}"), &body, ",")
                }
            }
        }
        ItemEnum::Union(u) => {
            let mut body: Vec<String> = u.fields.iter().filter_map(field).collect();
            if u.has_stripped_fields {
                body.push("/* private fields */".into());
            }
            braces(
                &format!(
                    "pub union {name}{}{}",
                    render::generic_params(&u.generics.params),
                    render::where_clause(&u.generics)
                ),
                &body,
                ",",
            )
        }
        ItemEnum::Enum(e) => {
            let mut body: Vec<String> = e
                .variants
                .iter()
                .filter_map(|id| k.item(*id))
                .map(|v| {
                    let vname = v.name.as_deref().unwrap_or("_");
                    let ItemEnum::Variant(var) = &v.inner else {
                        return vname.to_string();
                    };
                    let mut s = match &var.kind {
                        VariantKind::Plain => vname.to_string(),
                        VariantKind::Tuple(fs) => format!(
                            "{vname}({})",
                            fs.iter()
                                .map(|f| tuple_field(f).trim_start_matches("pub ").to_string())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                        VariantKind::Struct {
                            fields,
                            has_stripped_fields,
                        } => {
                            let mut fs: Vec<String> = fields
                                .iter()
                                .filter_map(field)
                                .map(|f| f.trim_start_matches("pub ").to_string())
                                .collect();
                            if *has_stripped_fields {
                                fs.push("..".into());
                            }
                            format!("{vname} {{ {} }}", fs.join(", "))
                        }
                    };
                    if let Some(d) = &var.discriminant {
                        s += &format!(" = {}", d.expr);
                    }
                    s
                })
                .collect();
            if e.has_stripped_variants {
                body.push("/* hidden variants */".into());
            }
            braces(
                &format!(
                    "pub enum {name}{}{}",
                    render::generic_params(&e.generics.params),
                    render::where_clause(&e.generics)
                ),
                &body,
                ",",
            )
        }
        ItemEnum::Macro(src) => src.clone(),
        ItemEnum::ProcMacro(_) => signature(item),
        _ => format!("pub {}", signature(item)),
    }
}

fn braces(head: &str, body: &[String], sep: &str) -> String {
    if body.is_empty() {
        return format!("{head} {{}}");
    }
    let mut s = format!("{head} {{\n");
    for line in body {
        let sep = if line.starts_with("/*") { "" } else { sep };
        let _ = writeln!(s, "    {line}{sep}");
    }
    s + "}"
}

fn attrs(item: &Item) -> Vec<String> {
    item.attrs
        .iter()
        .filter_map(|a| match a {
            Attribute::NonExhaustive => Some("#[non_exhaustive]".into()),
            Attribute::MustUse { .. } => Some("#[must_use]".into()),
            Attribute::Repr(_) => None,
            Attribute::Other(s) if s.contains("cfg") => Some(s.clone()),
            _ => None,
        })
        .collect()
}

/// The full page for an item: where it lives, its declaration, docs and members.
pub fn page(store: &mut Store, r: &Ref, public_path: &str) -> String {
    let item = r.item();
    let mut out = String::new();
    let (pkg_name, version) = match store.ws() {
        Ok(ws) => {
            let p = ws.package(&r.doc.pkg);
            (p.name.to_string(), p.version.to_string())
        }
        Err(_) => (String::new(), String::new()),
    };
    let _ = writeln!(out, "{} {public_path}  ({pkg_name} {version})", kind(item));
    if let Some(defined) = r.doc.file.summary(r.id).map(|s| s.path.join("::"))
        && defined != public_path
    {
        let _ = writeln!(out, "defined at {defined}");
    }
    if let Some(dep) = &item.deprecation {
        let _ = writeln!(
            out,
            "DEPRECATED{}{}",
            dep.since
                .as_ref()
                .map_or(String::new(), |s| format!(" since {s}")),
            dep.note
                .as_ref()
                .map_or(String::new(), |n| format!(": {n}"))
        );
    }
    for a in attrs(item) {
        let _ = writeln!(out, "{a}");
    }
    let _ = writeln!(out, "\n```rust\n{}\n```", declaration(r));
    if let Some(docs) = item.docs.as_deref().filter(|d| !d.trim().is_empty()) {
        // A module's docs are often the crate README; keep them short so its items show.
        let limit = if matches!(item.inner, ItemEnum::Module(_)) {
            MODULE_DOCS_CAP
        } else {
            DOCS_CAP
        };
        let _ = writeln!(out, "\n{}", cap(&strip_hidden_lines(docs), limit));
    }
    match &item.inner {
        ItemEnum::Module(_) => module_listing(store, r, &mut out),
        ItemEnum::Trait(t) => {
            trait_listing(store, r, &mut out);
            implementors(r, &t.implementations, &mut out);
        }
        ItemEnum::Struct(_) | ItemEnum::Enum(_) | ItemEnum::Union(_) | ItemEnum::Primitive(_) => {
            impl_listing(r, &mut out);
        }
        _ => {}
    }
    out
}

/// Drops the `# ` lines rustdoc hides in Rust code examples; they are setup boilerplate.
fn strip_hidden_lines(docs: &str) -> String {
    let mut in_rust = false;
    let mut out = Vec::new();
    for line in docs.lines() {
        let t = line.trim_start();
        if let Some(lang) = t.strip_prefix("```") {
            if in_rust {
                in_rust = false;
            } else {
                in_rust = lang.is_empty() || lang.split(',').any(|l| l.trim() == "rust");
            }
        } else if in_rust && (t == "#" || t.starts_with("# ")) {
            continue;
        }
        out.push(line);
    }
    out.join("\n")
}

fn cap(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut end = n;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n\n[docs cut at {n} bytes of {}]", &s[..end], s.len())
}

fn module_listing(store: &mut Store, r: &Ref, out: &mut String) {
    let children = store.children(r);
    let _ = writeln!(out, "\nItems ({}):", children.len());
    for (name, t) in children.iter().take(LIST_CAP) {
        match t {
            Target::Found(c) => {
                let item = c.item();
                let s = summary(item);
                let _ = writeln!(
                    out,
                    "  {} {name}{}",
                    kind(item),
                    if s.is_empty() {
                        String::new()
                    } else {
                        format!("  // {s}")
                    }
                );
            }
            Target::Named(p) => {
                let _ = writeln!(out, "  use {name}  // re-export of {p}");
            }
        }
    }
    if children.len() > LIST_CAP {
        let _ = writeln!(
            out,
            "  … {} more; use `search` to find one",
            children.len() - LIST_CAP
        );
    }
}

fn trait_listing(store: &mut Store, r: &Ref, out: &mut String) {
    let members = store.members(r, false);
    let (required, provided): (Vec<_>, Vec<_>) = members.iter().partition(|(_, m)| {
        !matches!(&m.item().inner, ItemEnum::Function(f) if f.has_body)
            && !matches!(&m.item().inner, ItemEnum::AssocType { type_: Some(_), .. })
            && !matches!(&m.item().inner, ItemEnum::AssocConst { value: Some(_), .. })
    });
    for (title, list) in [("Required", required), ("Provided", provided)] {
        if list.is_empty() {
            continue;
        }
        let _ = writeln!(out, "\n{title}:");
        for (_, m) in list {
            line(m.item(), out);
        }
    }
}

fn line(item: &Item, out: &mut String) {
    let s = summary(item);
    let _ = writeln!(
        out,
        "  {}{}",
        signature(item),
        if s.is_empty() {
            String::new()
        } else {
            format!("  // {s}")
        }
    );
}

fn impl_listing(r: &Ref, out: &mut String) {
    let k = &r.doc.file;
    let impls = match &r.item().inner {
        ItemEnum::Struct(s) => &s.impls,
        ItemEnum::Enum(e) => &e.impls,
        ItemEnum::Union(u) => &u.impls,
        ItemEnum::Primitive(p) => &p.impls,
        _ => return,
    };
    let mut inherent = Vec::new();
    let mut traits = Vec::new();
    let mut auto = Vec::new();
    let impl_items: Vec<Item> = impls.iter().filter_map(|id| k.item(*id)).collect();
    for imp in &impl_items {
        let ItemEnum::Impl(im) = &imp.inner else {
            continue;
        };
        match &im.trait_ {
            None => inherent.push(im),
            Some(_) if im.blanket_impl.is_some() => {}
            Some(t) if im.is_synthetic => auto.push(render::path(t)),
            Some(t) => traits.push(format!(
                "{}{}",
                if im.is_negative { "!" } else { "" },
                render::path(t)
            )),
        }
    }
    let methods: Vec<Item> = inherent
        .iter()
        .flat_map(|im| im.items.iter())
        .filter_map(|id| k.item(*id))
        .collect();
    if !methods.is_empty() {
        let _ = writeln!(out, "\nInherent items ({}):", methods.len());
        for m in methods.iter().take(LIST_CAP) {
            line(m, out);
        }
        if methods.len() > LIST_CAP {
            let _ = writeln!(
                out,
                "  … {} more; look one up as `Type::name`",
                methods.len() - LIST_CAP
            );
        }
    }
    if !traits.is_empty() {
        traits.sort();
        let _ = writeln!(out, "\nTrait impls: {}", traits.join(", "));
    }
    if !auto.is_empty() {
        let _ = writeln!(out, "Auto traits: {}", auto.join(", "));
    }
}

fn implementors(r: &Ref, ids: &[rustdoc_types::Id], out: &mut String) {
    let k = &r.doc.file;
    let fors: Vec<String> = ids
        .iter()
        .filter_map(|id| k.item(*id))
        .filter_map(|i| match &i.inner {
            ItemEnum::Impl(im) if !im.is_synthetic => Some(match &im.blanket_impl {
                Some(Type::Generic(g)) => {
                    format!("{g} (blanket{})", render::where_clause(&im.generics))
                }
                _ => render::ty(&im.for_),
            }),
            _ => None,
        })
        .collect();
    if fors.is_empty() {
        return;
    }
    let _ = writeln!(
        out,
        "\nImplemented in {} for ({}, downstream crates not shown):",
        k.summary(r.id)
            .and_then(|s| s.path.first().cloned())
            .unwrap_or_default(),
        fors.len()
    );
    for f in fors.iter().take(LIST_CAP) {
        let _ = writeln!(out, "  {f}");
    }
    if fors.len() > LIST_CAP {
        let _ = writeln!(out, "  … {} more", fors.len() - LIST_CAP);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_lines_go_only_inside_rust_blocks() {
        let docs = "# Heading\n```\n# use x;\nlet a = 1;\n#\n```\n```text\n# kept\n```";
        assert_eq!(
            strip_hidden_lines(docs),
            "# Heading\n```\nlet a = 1;\n```\n```text\n# kept\n```"
        );
    }

    #[test]
    fn cap_cuts_on_a_char_boundary() {
        let s = "héllo world";
        let out = cap(s, 2);
        assert!(out.starts_with("h\n\n[docs cut at 2 bytes"), "{out}");
        assert_eq!(cap(s, 100), s);
    }
}
