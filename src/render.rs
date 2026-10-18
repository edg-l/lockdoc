//! Prints rustdoc JSON types and signatures back as Rust source.

use rustdoc_types::{
    Abi, AssocItemConstraint, AssocItemConstraintKind, Function, FunctionHeader, GenericArg,
    GenericArgs, GenericBound, GenericParamDef, GenericParamDefKind, Generics, Path,
    PreciseCapturingArg, Term, TraitBoundModifier, Type, WherePredicate,
};

pub fn ty(t: &Type) -> String {
    match t {
        Type::ResolvedPath(p) => path(p),
        Type::DynTrait(d) => {
            let mut parts: Vec<String> = d
                .traits
                .iter()
                .map(|pt| format!("{}{}", hrtb(&pt.generic_params), path(&pt.trait_)))
                .collect();
            if let Some(l) = &d.lifetime {
                parts.push(l.clone());
            }
            format!("dyn {}", parts.join(" + "))
        }
        Type::Generic(s) | Type::Primitive(s) => s.clone(),
        Type::FunctionPointer(f) => format!(
            "{}{}fn({}){}",
            hrtb(&f.generic_params),
            header(&f.header),
            f.sig
                .inputs
                .iter()
                .map(|(_, t)| ty(t))
                .collect::<Vec<_>>()
                .join(", "),
            ret(f.sig.output.as_ref())
        ),
        Type::Tuple(ts) if ts.len() == 1 => format!("({},)", ty(&ts[0])),
        Type::Tuple(ts) => format!("({})", ts.iter().map(ty).collect::<Vec<_>>().join(", ")),
        Type::Slice(t) => format!("[{}]", ty(t)),
        Type::Array { type_, len } => format!("[{}; {len}]", ty(type_)),
        Type::Pat { type_, .. } => ty(type_),
        Type::ImplTrait(b) => format!("impl {}", bounds(b)),
        Type::Infer => "_".into(),
        Type::RawPointer { is_mutable, type_ } => {
            format!(
                "*{} {}",
                if *is_mutable { "mut" } else { "const" },
                ty(type_)
            )
        }
        Type::BorrowedRef {
            lifetime,
            is_mutable,
            type_,
        } => format!(
            "&{}{}{}",
            lifetime.as_ref().map_or(String::new(), |l| format!("{l} ")),
            if *is_mutable { "mut " } else { "" },
            ty(type_)
        ),
        Type::QualifiedPath {
            name,
            args,
            self_type,
            trait_,
        } => {
            let args = args.as_deref().map_or(String::new(), generic_args);
            match (self_type.as_ref(), trait_) {
                (Type::Generic(s), _) if s == "Self" => format!("Self::{name}{args}"),
                (self_ty, Some(tr)) if !tr.path.is_empty() => {
                    format!("<{} as {}>::{name}{args}", ty(self_ty), path(tr))
                }
                (self_ty, _) => format!("{}::{name}{args}", ty(self_ty)),
            }
        }
    }
}

pub fn path(p: &Path) -> String {
    format!(
        "{}{}",
        p.path,
        p.args.as_deref().map_or(String::new(), generic_args)
    )
}

pub fn generic_args(a: &GenericArgs) -> String {
    match a {
        GenericArgs::AngleBracketed { args, constraints } => {
            let mut parts: Vec<String> = args.iter().map(generic_arg).collect();
            parts.extend(constraints.iter().map(constraint));
            if parts.is_empty() {
                String::new()
            } else {
                format!("<{}>", parts.join(", "))
            }
        }
        GenericArgs::Parenthesized { inputs, output } => format!(
            "({}){}",
            inputs.iter().map(ty).collect::<Vec<_>>().join(", "),
            ret(output.as_ref())
        ),
        GenericArgs::ReturnTypeNotation => "(..)".into(),
    }
}

fn generic_arg(a: &GenericArg) -> String {
    match a {
        GenericArg::Lifetime(l) => l.clone(),
        GenericArg::Type(t) => ty(t),
        GenericArg::Const(c) => c.expr.clone(),
        GenericArg::Infer => "_".into(),
    }
}

fn constraint(c: &AssocItemConstraint) -> String {
    let args = c.args.as_deref().map_or(String::new(), generic_args);
    match &c.binding {
        AssocItemConstraintKind::Equality(t) => format!("{}{args} = {}", c.name, term(t)),
        AssocItemConstraintKind::Constraint(b) => format!("{}{args}: {}", c.name, bounds(b)),
    }
}

fn term(t: &Term) -> String {
    match t {
        Term::Type(t) => ty(t),
        Term::Constant(c) => c.expr.clone(),
    }
}

pub fn bounds(b: &[GenericBound]) -> String {
    b.iter().map(bound).collect::<Vec<_>>().join(" + ")
}

fn bound(b: &GenericBound) -> String {
    match b {
        GenericBound::TraitBound {
            trait_,
            generic_params,
            modifier,
        } => {
            let m = match modifier {
                TraitBoundModifier::None => "",
                TraitBoundModifier::Maybe => "?",
                TraitBoundModifier::MaybeConst => "[const] ",
            };
            format!("{}{m}{}", hrtb(generic_params), path(trait_))
        }
        GenericBound::Outlives(l) => l.clone(),
        GenericBound::Use(args) => format!(
            "use<{}>",
            args.iter()
                .map(|a| match a {
                    PreciseCapturingArg::Lifetime(s) | PreciseCapturingArg::Param(s) => s.as_str(),
                })
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn hrtb(params: &[GenericParamDef]) -> String {
    if params.is_empty() {
        String::new()
    } else {
        format!("for{} ", generic_params(params))
    }
}

/// `<'a, T: Bound = Default, const N: usize>`, leaving out the synthetic parameters rustdoc
/// adds for argument-position `impl Trait`.
pub fn generic_params(params: &[GenericParamDef]) -> String {
    let parts: Vec<String> = params
        .iter()
        .filter_map(|p| match &p.kind {
            GenericParamDefKind::Lifetime { outlives } if outlives.is_empty() => {
                Some(p.name.clone())
            }
            GenericParamDefKind::Lifetime { outlives } => {
                Some(format!("{}: {}", p.name, outlives.join(" + ")))
            }
            GenericParamDefKind::Type {
                is_synthetic: true, ..
            } => None,
            GenericParamDefKind::Type {
                bounds: b, default, ..
            } => {
                let mut s = p.name.clone();
                if !b.is_empty() {
                    s += &format!(": {}", bounds(b));
                }
                if let Some(d) = default {
                    s += &format!(" = {}", ty(d));
                }
                Some(s)
            }
            GenericParamDefKind::Const { type_, default } => Some(format!(
                "const {}: {}{}",
                p.name,
                ty(type_),
                default
                    .as_ref()
                    .map_or(String::new(), |d| format!(" = {d}"))
            )),
        })
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!("<{}>", parts.join(", "))
    }
}

pub fn where_clause(g: &Generics) -> String {
    if g.where_predicates.is_empty() {
        return String::new();
    }
    let preds: Vec<String> = g
        .where_predicates
        .iter()
        .map(|w| match w {
            WherePredicate::BoundPredicate {
                type_,
                bounds: b,
                generic_params,
            } => format!("{}{}: {}", hrtb(generic_params), ty(type_), bounds(b)),
            WherePredicate::LifetimePredicate { lifetime, outlives } => {
                format!("{lifetime}: {}", outlives.join(" + "))
            }
            WherePredicate::EqPredicate { lhs, rhs } => format!("{} = {}", ty(lhs), term(rhs)),
        })
        .collect();
    format!(" where {}", preds.join(", "))
}

fn header(h: &FunctionHeader) -> String {
    let mut s = String::new();
    if h.is_const {
        s += "const ";
    }
    if h.is_async {
        s += "async ";
    }
    if h.is_unsafe {
        s += "unsafe ";
    }
    match &h.abi {
        Abi::Rust => {}
        Abi::C { .. } => s += "extern \"C\" ",
        Abi::System { .. } => s += "extern \"system\" ",
        other => s += &format!("extern \"{other:?}\" "),
    }
    s
}

fn ret(t: Option<&Type>) -> String {
    match t {
        None => String::new(),
        Some(Type::Tuple(v)) if v.is_empty() => String::new(),
        Some(t) => format!(" -> {}", ty(t)),
    }
}

/// `pub fn name<T>(&self, x: T) -> U where …`, on one line.
pub fn function(name: &str, f: &Function) -> String {
    let inputs: Vec<String> = f
        .sig
        .inputs
        .iter()
        .map(|(n, t)| match (n.as_str(), t) {
            ("self", Type::Generic(s)) if s == "Self" => "self".into(),
            (
                "self",
                Type::BorrowedRef {
                    lifetime,
                    is_mutable,
                    type_,
                },
            ) if matches!(type_.as_ref(), Type::Generic(s) if s == "Self") => {
                format!(
                    "&{}{}self",
                    lifetime.as_ref().map_or(String::new(), |l| format!("{l} ")),
                    if *is_mutable { "mut " } else { "" }
                )
            }
            (n, t) => format!("{n}: {}", ty(t)),
        })
        .collect();
    let variadic = if f.sig.is_c_variadic { ", ..." } else { "" };
    format!(
        "{}fn {name}{}({}{variadic}){}{}",
        header(&f.header),
        generic_params(&f.generics.params),
        inputs.join(", "),
        ret(f.sig.output.as_ref()),
        where_clause(&f.generics)
    )
}
