//! Match exhaustiveness by the usefulness algorithm (Maranget, "Warnings for
//! pattern matching"; the same scheme rustc uses). Arm patterns become rows
//! of a matrix over typed columns. A column is split by the constructors of
//! its type -- enum variants, `true`/`false`, `null`/non-null, the single
//! constructor of a tuple or struct, or disjoint `int` segments cut at the
//! arms' range bounds -- and each constructor specializes the matrix into its
//! fields. A value no row matches comes back as a witness pattern.
//!
//! Strings, floats and every other type without an enumerable constructor
//! set are only covered by a wildcard or binding. A literal or range bound
//! that is not an integer literal covers nothing, so coverage is only ever
//! under-approximated and a non-exhaustive match is never accepted.

use super::*;
use crate::hir::RecordPatField;
use std::rc::Rc;

/// Missing patterns listed in one diagnostic; the search stops past this.
const MAX_REPORTED: usize = 10;

const INT_MIN: i128 = i64::MIN as i128;
const INT_MAX: i128 = i64::MAX as i128;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ctor {
    /// Index into the enum's variants.
    Variant(usize),
    /// The only constructor of a tuple, struct or unit.
    Single,
    Bool(bool),
    /// Inclusive `int` interval.
    Int(i128, i128),
    Null,
    NonNull,
    /// A literal the checker cannot reason about (string, float, a named
    /// constant): it matches no constructor, so it covers nothing.
    Unknown,
}

#[derive(Clone, Debug)]
enum Pat {
    Wild,
    Ctor(Ctor, Vec<Pat>),
    Or(Vec<Pat>),
}

struct VariantShape {
    name: Ustr,
    fields: Vec<TyId>,
    /// Field names of a record variant, in declaration order.
    field_names: Option<Vec<Ustr>>,
}

enum Shape {
    Int,
    Bool,
    Unit,
    Nullable(TyId),
    Tuple(Vec<TyId>),
    Struct {
        name: Ustr,
        fields: Vec<(Ustr, TyId)>,
    },
    Enum {
        name: Ustr,
        variants: Vec<VariantShape>,
    },
    /// No enumerable constructors: only a wildcard covers it.
    Opaque,
}

/// A value no arm matches, as a pattern tree over the column types.
#[derive(Clone)]
enum Wit {
    Wild,
    Ctor(Ctor, TyId, Vec<Wit>),
}

/// One unguarded arm: its pattern and, when the whole pattern is a bare
/// variant name, the variant it tag-tests (see `unqualified_arm_variant`).
pub(crate) struct ArmPattern {
    pub pattern: Idx<Pattern>,
    pub tag: Option<Ustr>,
}

/// The missing patterns of a `match` over `scrut_ty` whose unguarded arms are
/// `arms`, rendered for the diagnostic; empty when the match is exhaustive.
pub(crate) fn missing_match_patterns(
    c: BodyCtx,
    gamma: &mut Gamma,
    scrut_ty: TyId,
    arms: &[ArmPattern],
    span: Span,
) -> Vec<String> {
    let mut cx = Checker {
        c,
        gamma,
        span,
        shapes: FxHashMap::default(),
        poisoned: false,
    };
    let scrut_ty = resolve(cx.gamma, scrut_ty, span);

    let rows: Vec<Vec<Pat>> = arms
        .iter()
        .map(|arm| vec![cx.lower_arm(arm, scrut_ty)])
        .collect();
    let witnesses = cx.missing(rows, &[scrut_ty], true, MAX_REPORTED + 1);

    // A poisoned type already carries its own diagnostic.
    if cx.poisoned {
        return Vec::new();
    }

    let mut out: Vec<String> = witnesses
        .iter()
        .take(MAX_REPORTED)
        .map(|w| cx.render_top(&w[0]))
        .collect();
    if witnesses.len() > MAX_REPORTED
        && let Some(last) = out.last_mut()
    {
        last.push_str(" and more");
    }
    out
}

struct Checker<'c, 'g, 'w> {
    c: BodyCtx<'c>,
    gamma: &'g mut Gamma<'w>,
    span: Span,
    shapes: FxHashMap<TyId, Rc<Shape>>,
    poisoned: bool,
}

impl Checker<'_, '_, '_> {
    fn shape(&mut self, ty: TyId) -> Rc<Shape> {
        let ty = resolve(self.gamma, ty, self.span);
        if let Some(s) = self.shapes.get(&ty) {
            return s.clone();
        }
        let shape = Rc::new(self.compute_shape(ty));
        self.shapes.insert(ty, shape.clone());
        shape
    }

    fn compute_shape(&mut self, ty: TyId) -> Shape {
        let span = self.span;
        match *self.gamma.get(ty) {
            TypeExpr::Builtin(BuiltinTy::Int) => Shape::Int,
            TypeExpr::Builtin(BuiltinTy::Bool) => Shape::Bool,
            TypeExpr::Builtin(BuiltinTy::Unit) => Shape::Unit,
            TypeExpr::Builtin(BuiltinTy::Error) => {
                self.poisoned = true;
                Shape::Opaque
            }
            TypeExpr::Nullable(inner) => Shape::Nullable(ty.with_idx(inner)),
            TypeExpr::Tuple { elements } => {
                Shape::Tuple(elements.into_iter().map(|e| ty.with_idx(e)).collect())
            }
            TypeExpr::App {
                ctor: TypeCtor::Ref,
                mut params,
            } => match params.next() {
                Some(inner) => {
                    let inner = resolve(self.gamma, ty.with_idx(inner), span);
                    self.compute_shape(inner)
                }
                None => Shape::Opaque,
            },
            TypeExpr::App {
                ctor: TypeCtor::Nominal(name),
                params,
            } => {
                if let Some(eid) = resolve_enum(self.gamma, name) {
                    let def = self.gamma.workspace.module(eid.module).enums[eid.def].clone();
                    let mut subs = FxHashMap::default();
                    for (g, arg) in def.generics.iter().zip(params) {
                        if let TypeExpr::Generic(var_id, _) = self.gamma.get(*g) {
                            subs.insert(*var_id, ty.with_idx(arg));
                        }
                    }
                    let mut variants = Vec::with_capacity(def.variants.len());
                    for v in &def.variants {
                        let (tys, field_names) = match &v.payload {
                            VariantPayload::Unit => (Vec::new(), None),
                            VariantPayload::Tuple(tys) => (tys.clone(), None),
                            VariantPayload::Record(fs) => (
                                fs.iter().map(|f| f.ty).collect(),
                                Some(fs.iter().map(|f| f.name).collect()),
                            ),
                        };
                        let mut fields = Vec::with_capacity(tys.len());
                        for t in tys {
                            match substitute(self.gamma, t, &subs, span) {
                                Ok(t) => fields.push(t),
                                Err(_) => {
                                    self.poisoned = true;
                                    return Shape::Opaque;
                                }
                            }
                        }
                        variants.push(VariantShape {
                            name: v.name,
                            fields,
                            field_names,
                        });
                    }
                    Shape::Enum {
                        name: def.name,
                        variants,
                    }
                } else if let Some(sid) = resolve_struct(self.gamma, name) {
                    let def = self.gamma.workspace.module(sid.module).structs[sid.def].clone();
                    let subs = struct_arg_subs(self.gamma, &def, ty, params);
                    let mut fields = Vec::with_capacity(def.fields.len());
                    for f in &def.fields {
                        match substitute(self.gamma, f.ty, &subs, span) {
                            Ok(t) => fields.push((f.name, t)),
                            Err(_) => {
                                self.poisoned = true;
                                return Shape::Opaque;
                            }
                        }
                    }
                    Shape::Struct {
                        name: def.name,
                        fields,
                    }
                } else {
                    Shape::Opaque
                }
            }
            _ => Shape::Opaque,
        }
    }

    /// Field types of `ctor` on a value of type `ty`.
    fn ctor_fields(&mut self, ty: TyId, ctor: Ctor) -> Vec<TyId> {
        let shape = self.shape(ty);
        match (&*shape, ctor) {
            (Shape::Tuple(tys), Ctor::Single) => tys.clone(),
            (Shape::Struct { fields, .. }, Ctor::Single) => {
                fields.iter().map(|(_, t)| *t).collect()
            }
            (Shape::Enum { variants, .. }, Ctor::Variant(i)) => variants[i].fields.clone(),
            (Shape::Nullable(inner), Ctor::NonNull) => vec![*inner],
            _ => Vec::new(),
        }
    }

    fn lower_arm(&mut self, arm: &ArmPattern, ty: TyId) -> Pat {
        if let Some(tag) = arm.tag
            && let Shape::Enum { variants, .. } = &*self.shape(ty)
            && let Some(i) = variants.iter().position(|v| v.name == tag)
        {
            let n = variants[i].fields.len();
            return Pat::Ctor(Ctor::Variant(i), vec![Pat::Wild; n]);
        }
        self.lower(arm.pattern, ty)
    }

    fn int_lit(&self, e: Idx<Expr>) -> Option<i128> {
        let body = self.c.body;
        match body.expr_arena[e] {
            Expr::Int(n) => Some(n as i128),
            Expr::UnaryMinus(inner, _) => match body.expr_arena[inner] {
                Expr::Int(n) => Some(-(n as i128)),
                _ => None,
            },
            _ => None,
        }
    }

    /// The matrix form of `pat` against a value of type `ty`. Nested bare
    /// names are bindings: only a whole-arm name can be a variant tag.
    fn lower(&mut self, pat: Idx<Pattern>, ty: TyId) -> Pat {
        let body = self.c.body;
        let shape = self.shape(ty);
        let pattern = body.pattern_arena[pat];

        match pattern {
            Pattern::Wildcard | Pattern::Bind(_) => return Pat::Wild,
            Pattern::Or { alternatives } => {
                return Pat::Or(alternatives.map(|a| self.lower(a, ty)).collect());
            }
            _ => {}
        }

        if let Shape::Nullable(inner) = *shape {
            if let Pattern::Literal(e) = pattern
                && matches!(body.expr_arena[e], Expr::Null)
            {
                return Pat::Ctor(Ctor::Null, Vec::new());
            }
            return Pat::Ctor(Ctor::NonNull, vec![self.lower(pat, inner)]);
        }

        match (pattern, &*shape) {
            (Pattern::Literal(e), Shape::Int) => match self.int_lit(e) {
                Some(n) => Pat::Ctor(Ctor::Int(n, n), Vec::new()),
                None => Pat::Ctor(Ctor::Unknown, Vec::new()),
            },
            (Pattern::Literal(e), Shape::Bool) => match body.expr_arena[e] {
                Expr::Bool(b) => Pat::Ctor(Ctor::Bool(b), Vec::new()),
                _ => Pat::Ctor(Ctor::Unknown, Vec::new()),
            },
            (
                Pattern::Range {
                    start,
                    end,
                    inclusive,
                },
                Shape::Int,
            ) => {
                let lo = match start {
                    None => Some(INT_MIN),
                    Some(e) => self.int_lit(e),
                };
                let hi = match end {
                    None => Some(INT_MAX),
                    Some(e) => self.int_lit(e).map(|n| if inclusive { n } else { n - 1 }),
                };
                match (lo, hi) {
                    (Some(lo), Some(hi)) if lo.max(INT_MIN) <= hi.min(INT_MAX) => {
                        Pat::Ctor(Ctor::Int(lo.max(INT_MIN), hi.min(INT_MAX)), Vec::new())
                    }
                    _ => Pat::Ctor(Ctor::Unknown, Vec::new()),
                }
            }
            (Pattern::Tuple { elements }, Shape::Tuple(tys)) => {
                let tys = tys.clone();
                let fields = elements.zip(tys).map(|(p, t)| self.lower(p, t)).collect();
                Pat::Ctor(Ctor::Single, fields)
            }
            (Pattern::EnumVariant { variant, sub, .. }, Shape::Enum { variants, .. }) => {
                let Some(i) = variants.iter().position(|v| v.name == variant) else {
                    return Pat::Ctor(Ctor::Unknown, Vec::new());
                };
                let tys = variants[i].fields.clone();
                let mut fields: Vec<Pat> = sub
                    .zip(tys.iter())
                    .map(|(p, t)| self.lower(p, *t))
                    .collect();
                fields.resize(tys.len(), Pat::Wild);
                Pat::Ctor(Ctor::Variant(i), fields)
            }
            (
                Pattern::Record {
                    name: Some(vname),
                    fields,
                    ..
                },
                Shape::Enum { variants, .. },
            ) => {
                let Some(i) = variants.iter().position(|v| v.name == vname) else {
                    return Pat::Ctor(Ctor::Unknown, Vec::new());
                };
                let names = variants[i].field_names.clone().unwrap_or_default();
                let tys = variants[i].fields.clone();
                let out = self.lower_record_fields(fields, &names, &tys);
                Pat::Ctor(Ctor::Variant(i), out)
            }
            (Pattern::Record { fields, .. }, Shape::Struct { fields: defs, .. }) => {
                let names: Vec<Ustr> = defs.iter().map(|(n, _)| *n).collect();
                let tys: Vec<TyId> = defs.iter().map(|(_, t)| *t).collect();
                let out = self.lower_record_fields(fields, &names, &tys);
                Pat::Ctor(Ctor::Single, out)
            }
            _ => Pat::Ctor(Ctor::Unknown, Vec::new()),
        }
    }

    /// Record-pattern fields in declaration order; an omitted field is a
    /// wildcard.
    fn lower_record_fields(
        &mut self,
        fields: IdxRange<RecordPatField>,
        names: &[Ustr],
        tys: &[TyId],
    ) -> Vec<Pat> {
        let mut out = vec![Pat::Wild; tys.len()];
        for f in fields {
            let pf = self.c.body.record_pat_field_arena[f];
            if let Some(i) = names.iter().position(|n| *n == pf.name) {
                out[i] = self.lower(pf.pat, tys[i]);
            }
        }
        out
    }

    /// Up to `limit` value vectors (one pattern per column of `tys`) that no
    /// row of `rows` matches.
    fn missing(
        &mut self,
        rows: Vec<Vec<Pat>>,
        tys: &[TyId],
        top: bool,
        limit: usize,
    ) -> Vec<Vec<Wit>> {
        if limit == 0 {
            return Vec::new();
        }
        let Some((&ty, rest_tys)) = tys.split_first() else {
            return if rows.is_empty() {
                vec![Vec::new()]
            } else {
                Vec::new()
            };
        };

        let rows = expand_or_heads(rows);
        let mut heads: Vec<Ctor> = rows
            .iter()
            .filter_map(|r| match &r[0] {
                Pat::Ctor(c, _) if *c != Ctor::Unknown => Some(*c),
                _ => None,
            })
            .collect();

        let shape = self.shape(ty);
        let all = all_ctors(&shape, &heads);
        if all.is_empty() {
            // No enumerable constructors: only wildcard rows cover the column.
            heads.clear();
        }

        let mut out = Vec::new();
        if heads.is_empty() {
            // Every row has a wildcard head (or one that covers nothing), so
            // the column is decided by the remaining columns. At the root a
            // finite type lists its constructors; elsewhere `_` reads better.
            let rest = self.missing(default_matrix(&rows), rest_tys, false, limit);
            if rest.is_empty() {
                return out;
            }
            let listed = match &*shape {
                Shape::Enum { .. } | Shape::Bool if top => all,
                _ => Vec::new(),
            };
            if listed.is_empty() {
                for w in rest {
                    out.push(prepend(Wit::Wild, w));
                }
                return out;
            }
            for ctor in listed {
                for w in &rest {
                    if out.len() >= limit {
                        return out;
                    }
                    let wit = self.wild_ctor(ty, ctor);
                    out.push(prepend(wit, w.clone()));
                }
            }
            return out;
        }

        let mut default_wits: Option<Vec<Vec<Wit>>> = None;
        for ctor in all {
            if out.len() >= limit {
                break;
            }
            let present = heads.iter().any(|h| covers(*h, ctor));
            if present {
                let fields = self.ctor_fields(ty, ctor);
                let arity = fields.len();
                let mut sub_tys = fields;
                sub_tys.extend_from_slice(rest_tys);
                let spec = specialize(&rows, ctor, arity);
                for mut w in self.missing(spec, &sub_tys, false, limit - out.len()) {
                    let rest = w.split_off(arity);
                    out.push(prepend(Wit::Ctor(ctor, ty, w), rest));
                }
            } else {
                if default_wits.is_none() {
                    default_wits =
                        Some(self.missing(default_matrix(&rows), rest_tys, false, limit));
                }
                for w in default_wits.as_ref().into_iter().flatten() {
                    if out.len() >= limit {
                        break;
                    }
                    let wit = self.wild_ctor(ty, ctor);
                    out.push(prepend(wit, w.clone()));
                }
            }
        }
        out
    }

    fn wild_ctor(&mut self, ty: TyId, ctor: Ctor) -> Wit {
        let n = self.ctor_fields(ty, ctor).len();
        Wit::Ctor(ctor, ty, (0..n).map(|_| Wit::Wild).collect())
    }

    /// The root witness. A bare variant with no constrained payload keeps the
    /// plain variant-name form the enum check has always reported.
    fn render_top(&mut self, w: &Wit) -> String {
        if let Wit::Ctor(Ctor::Variant(i), ty, fields) = w
            && fields.iter().all(|f| matches!(f, Wit::Wild))
            && let Shape::Enum { variants, .. } = &*self.shape(*ty)
        {
            return variants[*i].name.to_string();
        }
        format!("`{}`", self.render(w))
    }

    fn render(&mut self, w: &Wit) -> String {
        let (ctor, ty, fields) = match w {
            Wit::Wild => return "_".to_string(),
            Wit::Ctor(c, t, f) => (*c, *t, f),
        };
        let shape = self.shape(ty);
        let parts: Vec<String> = fields.iter().map(|f| self.render(f)).collect();
        match (&*shape, ctor) {
            (_, Ctor::Int(lo, hi)) => render_int_range(lo, hi),
            (_, Ctor::Bool(b)) => b.to_string(),
            (_, Ctor::Null) => "null".to_string(),
            (_, Ctor::NonNull) => parts.into_iter().next().unwrap_or_else(|| "_".into()),
            (Shape::Unit, _) => "()".to_string(),
            (Shape::Tuple(_), _) => format!("({})", parts.join(", ")),
            (Shape::Struct { name, fields: defs }, _) => {
                let names: Vec<Ustr> = defs.iter().map(|(n, _)| *n).collect();
                render_record(name.as_str(), &names, fields, &parts)
            }
            (Shape::Enum { name, variants }, Ctor::Variant(i)) => {
                let v = &variants[i];
                let qual = format!("{name}::{}", v.name);
                match &v.field_names {
                    Some(names) => render_record(&qual, names, fields, &parts),
                    None if parts.is_empty() => qual,
                    None => format!("{qual}({})", parts.join(", ")),
                }
            }
            _ => "_".to_string(),
        }
    }
}

fn render_record(name: &str, names: &[Ustr], fields: &[Wit], parts: &[String]) -> String {
    let shown: Vec<String> = names
        .iter()
        .zip(fields)
        .zip(parts)
        .filter(|((_, w), _)| !matches!(w, Wit::Wild))
        .map(|((n, _), p)| format!("{n}: {p}"))
        .collect();
    if shown.is_empty() {
        format!("{name} {{ .. }}")
    } else if shown.len() < names.len() {
        format!("{name} {{ {}, .. }}", shown.join(", "))
    } else {
        format!("{name} {{ {} }}", shown.join(", "))
    }
}

fn render_int_range(lo: i128, hi: i128) -> String {
    match (lo == INT_MIN, hi == INT_MAX) {
        (true, true) => "_".to_string(),
        (true, false) => format!("..={hi}"),
        (false, true) => format!("{lo}.."),
        (false, false) if lo == hi => format!("{lo}"),
        (false, false) => format!("{lo}..={hi}"),
    }
}

/// The constructors a column of this shape splits into. `int` is cut into
/// disjoint segments at the bounds of the head ranges, so every segment lies
/// wholly inside or outside each range.
fn all_ctors(shape: &Shape, heads: &[Ctor]) -> Vec<Ctor> {
    match shape {
        Shape::Bool => vec![Ctor::Bool(false), Ctor::Bool(true)],
        Shape::Nullable(_) => vec![Ctor::Null, Ctor::NonNull],
        Shape::Unit | Shape::Tuple(_) | Shape::Struct { .. } => vec![Ctor::Single],
        Shape::Enum { variants, .. } => (0..variants.len()).map(Ctor::Variant).collect(),
        Shape::Int => {
            let mut cuts = vec![INT_MIN];
            for h in heads {
                if let Ctor::Int(lo, hi) = *h {
                    cuts.push(lo);
                    if hi < INT_MAX {
                        cuts.push(hi + 1);
                    }
                }
            }
            cuts.sort_unstable();
            cuts.dedup();
            let mut segs = Vec::with_capacity(cuts.len());
            for (i, &lo) in cuts.iter().enumerate() {
                let hi = cuts.get(i + 1).map_or(INT_MAX, |next| next - 1);
                segs.push(Ctor::Int(lo, hi));
            }
            segs
        }
        Shape::Opaque => Vec::new(),
    }
}

/// Whether a head constructor matches every value of `ctor`.
fn covers(head: Ctor, ctor: Ctor) -> bool {
    match (head, ctor) {
        (Ctor::Int(hlo, hhi), Ctor::Int(lo, hi)) => hlo <= lo && hi <= hhi,
        (Ctor::Unknown, _) => false,
        _ => head == ctor,
    }
}

/// Replace each row whose head is an or-pattern by one row per alternative.
fn expand_or_heads(rows: Vec<Vec<Pat>>) -> Vec<Vec<Pat>> {
    if !rows.iter().any(|r| matches!(r[0], Pat::Or(_))) {
        return rows;
    }
    let mut out = Vec::with_capacity(rows.len());
    let mut stack: Vec<Vec<Pat>> = rows.into_iter().rev().collect();
    while let Some(mut row) = stack.pop() {
        if let Pat::Or(alts) = &mut row[0] {
            let alts = std::mem::take(alts);
            for alt in alts.into_iter().rev() {
                let mut r = row.clone();
                r[0] = alt;
                stack.push(r);
            }
        } else {
            out.push(row);
        }
    }
    out
}

/// Rows that match a value built by `ctor`, with the head replaced by its
/// `arity` fields.
fn specialize(rows: &[Vec<Pat>], ctor: Ctor, arity: usize) -> Vec<Vec<Pat>> {
    let mut out = Vec::new();
    for row in rows {
        let mut new_row = match &row[0] {
            Pat::Wild => vec![Pat::Wild; arity],
            Pat::Ctor(h, fields) if covers(*h, ctor) => fields.clone(),
            _ => continue,
        };
        new_row.extend_from_slice(&row[1..]);
        out.push(new_row);
    }
    out
}

/// Rows whose head matches anything, without the head.
fn default_matrix(rows: &[Vec<Pat>]) -> Vec<Vec<Pat>> {
    rows.iter()
        .filter(|r| matches!(r[0], Pat::Wild))
        .map(|r| r[1..].to_vec())
        .collect()
}

fn prepend(head: Wit, mut rest: Vec<Wit>) -> Vec<Wit> {
    rest.insert(0, head);
    rest
}
