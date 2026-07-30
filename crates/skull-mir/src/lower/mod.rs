//! bone HIR -> skull MIR lowering (Phase 1: the straight-line spine).
//!
//! A type-directed mechanical walk over bone's already-resolved HIR (names,
//! overloads, operators, and types are all decided by `semantic_pass`). A
//! `Builder` holds a cursor on the "current block" and appends instructions;
//! `lower_expr` returns the [`Idx<Inst>`] that holds the expression's value.
//!
//! Variables (`let`/param) become mutable [`Cell`]s; a `Var` read is an
//! [`Inst::Load`], a binding's initializer an [`Inst::Store`]. Arithmetic
//! arrives as `__op_*` calls (rock's desugar) and is dispatched to **typed**
//! ops at build time (decision 8): `int`+`int` -> [`Inst::IntAdd`], `float` ->
//! [`Inst::FloatAdd`], `string`+`+` -> [`Inst::StringConcat`].
//!
//! Phase 1 is straight-line ONLY: control flow (`if`/`&&`/`??`/loops/`match`),
//! coroutines, calls-to-`FuncRef`, and aggregates are not here yet — they
//! `gap` (return a clear error) and land in later phases. The gate is the real
//! corpus, not this comment; see `docs/skull-bone-mir-faithful-plan.md`.

use std::collections::{HashMap, HashSet};

use bone_semantic::hir::{
    BinOp, BindingInfo, BindingPattern, Body, BodyOwner, BuiltinTy, Callee, Expr, Function,
    MatchArm, Pattern, Statement, TyId, TypeCtor, TypeExpr, VariantArgs, VariantPayload,
};
use ustr::Ustr;
use bone_semantic::inference::{BodyInference, Workspace};
use bone_semantic::prelude::{BoundNext, ForIterInfo, ModuleId, Span};
use comfy_la_arena::{Idx, IdxRange};
use skull_parser::CompilerError;

use crate::intrinsics::ExpandKind;
use crate::mir::*;

fn gap(msg: impl std::fmt::Display) -> CompilerError {
    CompilerError::new_no_location(format!("skull-mir: lowering gap: {msg}"))
}

// --- scalarized value ABI (rock parity) -------------------------------------
//
// A `value` struct is passed/returned as its flattened primitive LEAF scalars
// (no per-call cdata at the ABI boundary), reconstructed where a real value is
// needed. The static shape drives flatten/rebuild; the runtime carrier `ValueRepr`
// threads the unboxed form through lowering; `pack` is the single place a Group
// becomes a real `<name>_ct(...)` cdata (an escape). See the plan doc.

/// Whether a fn with backend `b` can serve codegen `target`: an `Any` (portable
/// Rock) body serves every target, else the backend must match exactly.
fn serves_target(b: bone_semantic::hir::FnBackend, target: bone_semantic::hir::FnBackend) -> bool {
    use bone_semantic::hir::FnBackend;
    b == FnBackend::Any || b == target
}

/// `Some(k)` iff `name` is a GC-free `value` struct whose every flattened
/// primitive leaf is `float` (recursively), where `k` is the leaf count; `None`
/// otherwise (not such a struct, or any int/bool/enum leaf). The JS backend uses
/// it to lower a `NativeBuffer<Vertex>` (all-float value struct, e.g. the vertex
/// pool) to one flat `Float32Array(n * k)` -- JS has no per-struct ctype.

/// Value/enum REPRESENTATION analysis over a `Workspace` -- what a type looks
/// like, as opposed to how a body lowers. Re-exported flat so the ~168 external
/// `skull_mir::lower::*` call sites keep their paths.
mod shape;
pub use shape::*;

/// The `impl Builder` walk, split by topic. Each is a plain `impl Builder<'_>`
/// block in a child module -- Rust allows the impl to be spread across the
/// crate, and a child sees this module's private items, so nothing needed
/// widening to make the split.
mod embed;
mod interp;
mod storage;

// A flattened child field's physical slots are DOTTED parent keys, the same
// encoding `embed` uses (`docs/scalarization-and-boxing-design.md` S 6). `.` is
// not an identifier character, so a slot key can never equal a legal user field
// name -- which the `__`-based encoding this replaced could: an enum field `sig`
// and a user field `sig__tag` both wrote `sig__tag`, so the table ctor emitted
// the key twice and Lua's last-write-wins clobbered the discriminant, silently
// breaking every match on that enum.
//
// The three forms are disjoint by their suffix: `#` for the discriminant (not an
// identifier character and not a digit), all-digits for a positional payload (an
// identifier cannot start with a digit), an identifier for a named payload. They
// cannot collide with an `embed` leaf key either, because both are prefixed by
// the owning parent FIELD name and a field name denotes exactly one field.

/// Re-key relative embed leaf paths under an owning field: `[("x", v)]` under
/// `p` becomes `[("p.x", v)]`. Relative paths let a leaf list be computed once
/// and attached wherever the child is spliced.
fn prefixed(field: Ustr, leaves: Vec<(Ustr, Idx<Inst>)>) -> Vec<(Ustr, Idx<Inst>)> {
    leaves
        .into_iter()
        .map(|(k, v)| (Ustr::from(&format!("{field}.{k}")), v))
        .collect()
}

/// The parent-table slot holding a flattened enum field's discriminant (variant
/// name string), e.g. field `sig` -> `sig.#`.
fn flat_tag_slot(field: Ustr) -> Ustr {
    Ustr::from(format!("{field}.#").as_str())
}

/// The parent-table slot holding a flattened enum field's `i`th positional payload,
/// e.g. field `sig` payload 0 -> `sig.0`.
fn flat_payload_slot(field: Ustr, i: usize) -> Ustr {
    Ustr::from(format!("{field}.{i}").as_str())
}

/// The parent-table slot holding a flattened enum field's named (record) payload,
/// e.g. field `shape` record field `r` -> `shape.r`.
fn flat_named_slot(field: Ustr, rec_field: Ustr) -> Ustr {
    Ustr::from(format!("{field}.{rec_field}").as_str())
}

/// Every relative slot a flattened enum field occupies: discriminant, positional
/// payloads, named payloads. A flat-enum field is MULTI-SLOT, so any site walking
/// a struct's physical leaves must expand it -- treating the field name as one
/// leaf silently splits the field across two encodings (the store writes
/// `f.#`/`f.0`, the walker touches `f`).
fn flat_slot_names(field: Ustr, shape: &EnumFlatShape) -> Vec<Ustr> {
    let mut out = vec![flat_tag_slot(field)];
    for i in 0..shape.tuple_width {
        out.push(flat_payload_slot(field, i));
    }
    for rf in &shape.record_fields {
        out.push(flat_named_slot(field, *rf));
    }
    out
}

/// A flattened enum value's runtime cells: the discriminant + every union slot
/// (positional tuple slots and named record slots), nil where unused.
#[derive(Clone)]
struct FlatCells {
    tag: Idx<Inst>,
    positional: Vec<Idx<Inst>>,
    named: Vec<(Ustr, Idx<Inst>)>,
}

/// A `match`/`if let`/`while let` scrutinee: either an ordinary lowered value, or a
/// FLATTENED enum field still living as `parent.{prefix}__tag`/`__i` slots. The
/// flat form lets the tag test + payload binds read the parent's slots directly --
/// no box -- and only materializes (`scrut_value`) on a whole-value path.
#[derive(Clone)]
enum Scrut {
    Val(Idx<Inst>),
    Flat {
        parent: Idx<Inst>,
        prefix: Ustr,
        shape: EnumFlatShape,
    },
    /// A flat enum already threaded as scalar cells (a scalarized param/return or a
    /// constructed-and-matched temporary) -- tag + payload values held directly, so
    /// the tag test + payload binds need no box and no slot read.
    Cells {
        tag: Idx<Inst>,
        positional: Vec<Idx<Inst>>,
        named: Vec<(Ustr, Idx<Inst>)>,
        shape: EnumFlatShape,
    },
}

/// Workspace-wide scalar-ABI context. Address-taken functions use the
/// type-derived fn-value ABI at their definitions and direct calls, so all three
/// call forms agree without callee-identity analysis.
pub struct ScalarCtx {
    taken_by_ref: HashSet<Ustr>,
    /// Per-body contribution: each body's set of fn-names it references by value.
    /// Keyed by `(module, owner)` so a body edit can diff its OLD vs NEW set
    /// without rescanning the whole workspace. Empty-contribution bodies are
    /// omitted (absent == empty).
    per_body: HashMap<(ModuleId, BodyOwner), HashSet<Ustr>>,
    /// Number of distinct bodies referencing each name by value.
    /// `taken_by_ref == { name : count > 0 }`. Only a membership flip (0<->1)
    /// changes scalar-ABI eligibility, so [`update_body`](Self::update_body)
    /// reports exactly those.
    ref_counts: HashMap<Ustr, u32>,
    /// When set, the scalarized value ABI is disabled workspace-wide: every fn
    /// keeps value structs as first-class single values (params/returns/args),
    /// never splatting them to leaf scalars. Used by the GLSL backend, where a
    /// `value` struct is a first-class `vecN`, not the luajit-shaped scalar leaves
    /// (`docs/shaders.md` finding 5). The toggle is contained in
    /// [`fn_scalar_abi`], which definitions and direct calls both consult.
    values_first_class: bool,
    /// The codegen backend this lowering feeds. Drives backend-variant selection
    /// at every resolved call ([`Builder::resolve_funcref`]): `Lua` rejects a
    /// call whose only binding is a JS decl; `Js` swaps a call to its JS sibling
    /// when the collapsed representative is a luajit-native variant. `Any`
    /// disables the filtering entirely (the GLSL backend + generic/test callers).
    target: bone_semantic::hir::FnBackend,
}

/// The fn-names a single body references by value (`Expr::Global(name)` whose
/// `name` is a function) — the way `let f = v2_sum` / passing `f` as an arg
/// surfaces in HIR (a direct call's callee is a `Callee`, not an `Expr`).
fn scan_body_value_refs(ws: &Workspace, body: &Body) -> HashSet<Ustr> {
    let mut set = HashSet::new();
    for (_, expr) in body.expr_arena.iter() {
        if let Expr::Global(name) = expr {
            if ws.functions_by_name.contains_key(name) {
                set.insert(*name);
            }
        }
    }
    set
}

impl ScalarCtx {
    pub fn new(ws: &Workspace) -> Self {
        let mut taken_by_ref = HashSet::new();
        let mut per_body: HashMap<(ModuleId, BodyOwner), HashSet<Ustr>> = HashMap::new();
        let mut ref_counts: HashMap<Ustr, u32> = HashMap::new();
        for module in ws.modules.iter() {
            for (owner, body) in module.bodies.iter() {
                let set = scan_body_value_refs(ws, body);
                for name in &set {
                    *ref_counts.entry(*name).or_insert(0) += 1;
                    taken_by_ref.insert(*name);
                }
                if !set.is_empty() {
                    per_body.insert((module.id, *owner), set);
                }
            }
        }
        // Default target is Lua: skull-codegen-lua is the primary consumer and its
        // full + incremental paths build a `ScalarCtx::new`. The JS driver and the
        // GLSL backend override via `with_target`.
        Self {
            taken_by_ref,
            per_body,
            ref_counts,
            values_first_class: false,
            target: bone_semantic::hir::FnBackend::Lua,
        }
    }

    /// Set the codegen backend this lowering feeds (see [`target`]).
    pub fn with_target(mut self, target: bone_semantic::hir::FnBackend) -> Self {
        self.target = target;
        self
    }

    /// Like [`new`](Self::new) but with the scalarized value ABI disabled: value
    /// structs stay first-class single values everywhere (the GLSL backend's
    /// requirement -- a `vecN` is not scalar leaves). See [`values_first_class`].
    /// The GLSL backend does not participate in Lua/JS backend-variant selection,
    /// so its target is `Any` (filtering disabled).
    pub fn new_values_first_class(ws: &Workspace) -> Self {
        let mut scx = Self::new(ws);
        scx.values_first_class = true;
        scx.target = bone_semantic::hir::FnBackend::Any;
        scx
    }

    /// Incrementally fold one re-inferred body's value-reference contribution into
    /// the cached set, and return the names whose GLOBAL taken-by-ref membership
    /// FLIPPED (entered or left the set). A flip means a fn's scalar-ABI
    /// eligibility changed, so the named fn(s) and every direct caller must
    /// re-emit -- the caller adds the flipped names to the affected set.
    /// Body edits keep the body SET fixed (add/remove fn is interface-tier ->
    /// full rebuild), so diffing only the re-inferred bodies is complete.
    pub fn update_body(&mut self, ws: &Workspace, mid: ModuleId, owner: BodyOwner) -> Vec<Ustr> {
        let key = (mid, owner);
        let old = self.per_body.remove(&key).unwrap_or_default();
        let new = match ws.module(mid).bodies.get(&owner) {
            Some(body) => scan_body_value_refs(ws, body),
            None => HashSet::new(),
        };
        let mut flipped = Vec::new();
        for name in old.difference(&new) {
            if let Some(c) = self.ref_counts.get_mut(name) {
                *c -= 1;
                if *c == 0 {
                    self.ref_counts.remove(name);
                    self.taken_by_ref.remove(name);
                    flipped.push(*name);
                }
            }
        }
        for name in new.difference(&old) {
            let c = self.ref_counts.entry(*name).or_insert(0);
            *c += 1;
            if *c == 1 {
                self.taken_by_ref.insert(*name);
                flipped.push(*name);
            }
        }
        if !new.is_empty() {
            self.per_body.insert(key, new);
        }
        flipped
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FnScalarAbi {
    param_shapes: Vec<Option<ValueShape>>,
    ret_shape: Option<ValueShape>,
}

/// The one ABI policy read by a top-level definition and every direct call.
/// Address-taken Rock bodies narrow to the `nobox` shapes an indirect caller can
/// derive from the fn type; direct-only bodies may scalarize every eligible value.
fn fn_scalar_abi(ws: &Workspace, scx: &ScalarCtx, fref: FuncRef) -> Option<FnScalarAbi> {
    fn_scalar_abi_spec(ws, scx, fref, &SpecSubst::new())
}

fn fn_nobox_abi_spec(
    ws: &Workspace,
    fref: FuncRef,
    subst: &SpecSubst,
) -> Option<FnScalarAbi> {
    let f = &ws.module(fref.module).functions[fref.func];
    let param_shapes: Vec<Option<ValueShape>> = f
        .param_decls
        .iter()
        .map(|pd| nobox_declared_shape_spec(ws, subst, pd.ty))
        .collect();
    let ret_shape =
        f.return_ty.and_then(|ty| nobox_declared_shape_spec(ws, subst, ty));
    (ret_shape.is_some() || param_shapes.iter().any(Option::is_some))
        .then_some(FnScalarAbi { param_shapes, ret_shape })
}

fn fn_scalar_abi_spec(
    ws: &Workspace,
    scx: &ScalarCtx,
    fref: FuncRef,
    subst: &SpecSubst,
) -> Option<FnScalarAbi> {
    if scx.values_first_class {
        return None;
    }
    let f = &ws.module(fref.module).functions[fref.func];
    if f.is_extern {
        return None;
    }
    let has_scalar_abi = f.annotations.iter().any(|a| a.name.name.as_str() == "scalar_abi");
    let nobox_only = if has_scalar_abi {
        false
    } else {
        let name_is_fixed = f.annotations.iter().any(|a| match a.name.name.as_str() {
            "nomangle" => subst.is_empty(),
            "test" => true,
            _ => false,
        });
        if f.inline_body.is_some() || f.name.as_str() == "main" || name_is_fixed {
            return None;
        }
        scx.taken_by_ref.contains(&f.name)
    };
    if nobox_only {
        return fn_nobox_abi_spec(ws, fref, subst);
    }
    let param_shapes: Vec<Option<ValueShape>> =
        f.param_decls.iter().map(|pd| sig_value_shape_spec(ws, subst, pd.ty)).collect();
    let ret_shape = f.return_ty.and_then(|ty| sig_value_shape_spec(ws, subst, ty));
    (ret_shape.is_some() || param_shapes.iter().any(Option::is_some))
        .then_some(FnScalarAbi { param_shapes, ret_shape })
}

/// A hoisted nested `fn`: the private top-level name it resolves to, and the
/// value ABI its definition and every call site both read (so the two can never
/// drift, the same single-gate discipline as [`fn_scalar_abi`]).
#[derive(Clone)]
struct NestedFn {
    hoisted: Ustr,
    /// Declared param names, so a call site can place named args into slots
    /// (`slotted_call_args`) exactly as a top-level callee's would be.
    param_names: Vec<Ustr>,
    /// Per-param value shape; all-`None` when the fn keeps the boxed ABI.
    param_shapes: Vec<Option<ValueShape>>,
    ret_shape: Option<ValueShape>,
}

impl NestedFn {
    fn uses_scalar_abi(&self) -> bool {
        self.ret_shape.is_some() || self.param_shapes.iter().any(|s| s.is_some())
    }
}

/// The Lua parameter-name list for a scalar-ABI `inline(lua)` fn (`@scalar_abi`):
/// each value param expands to its depth-first leaf names `<base>__<leaf>` (the
/// names the hand-written body references), non-value params keep their name.
/// `None` if the fn doesn't use the scalar ABI (the caller emits the plain list).
/// The leaf ORDER matches `repr_to_leaves`, so def params line up with the
/// callsite splat positionally.
pub fn scalar_abi_lua_param_names(
    ws: &Workspace,
    scx: &ScalarCtx,
    fref: FuncRef,
) -> Option<Vec<String>> {
    let abi = fn_scalar_abi(ws, scx, fref)?;
    let f = &ws.module(fref.module).functions[fref.func];
    let mut names = Vec::new();
    for (pd, shape) in f.param_decls.iter().zip(abi.param_shapes) {
        match shape {
            Some(shape) => leaf_param_names(pd.name.as_str(), &shape, &mut names),
            None => names.push(pd.name.as_str().to_string()),
        }
    }
    Some(names)
}

/// Per-declared-param LEAF width under the scalar ABI (a value-struct param's
/// flattened leaf count, 1 for everything else); `None` when `fref` does not
/// use the scalar ABI. Pairs with `scalar_abi_lua_param_names` -- the JS arity
/// guard needs widths per DECLARED param so a defaulted value-struct param
/// counts as its whole leaf group, not one slot.
pub fn scalar_abi_param_leaf_widths(
    ws: &Workspace,
    scx: &ScalarCtx,
    fref: FuncRef,
) -> Option<Vec<usize>> {
    let abi = fn_scalar_abi(ws, scx, fref)?;
    Some(
        abi.param_shapes
            .iter()
            .map(|s| s.as_ref().map_or(1, ValueShape::leaf_count))
            .collect(),
    )
}

/// THE invariant: a function's emitted Lua parameter list must be INJECTIVE.
///
/// The scalar-ABI leaf encoding joins nested field names with `__`, and `__` is
/// legal in a Rock identifier, so the join cannot be made injective on its own.
/// Two distinct sources can land on one emitted name:
///   - two leaves of the same value param (`value Bad { tl__x: float, tl: V2 }`
///     flattens both `tl__x` and `tl.x` to `r__tl__x`), or
///   - a leaf and a SIBLING param (`fn f(r: V2, r__x: float)` emits
///     `f(r__x, r__y, r__x)`).
/// Either way Lua binds the later one and the body silently reads the wrong
/// value. Reject rather than emit the duplicate -- same discipline as
/// [`crate::mangle::collision_errors`].
///
/// Keyed on the SIGNATURE, at the point leaves actually become parameter names --
/// not on the value declaration. A declaration key is both unsound and
/// over-broad: it misses the sibling-param collision entirely (the struct is
/// clean there), and it rejects a program whose ambiguous shape never reaches a
/// param list at all, which is legal and observably correct today.
///
/// The names come from [`scalar_abi_lua_param_names`] -- the very list the
/// emitter writes -- so the checker cannot disagree with what is emitted.
///
/// Scoped to `inline(lua)` bodies because that is the ONLY emission that names
/// leaves: an ordinary Rock fn's params are emitted as `c<N>` cells, positional
/// and injective by construction, so an ambiguous shape flowing through one is
/// harmless and must keep compiling.
pub fn scalar_abi_param_collisions(
    ws: &Workspace,
    scx: &ScalarCtx,
    fref: FuncRef,
) -> Vec<(String, CompilerError)> {
    let f = &ws.module(fref.module).functions[fref.func];
    if f.inline_body.is_none() {
        return Vec::new();
    }
    let Some(names) = scalar_abi_lua_param_names(ws, scx, fref) else {
        return Vec::new();
    };
    let origins = scalar_abi_param_origins(ws, scx, fref);
    debug_assert_eq!(names.len(), origins.len(), "one derivation, two spellings");

    let mut out = Vec::new();
    let mut seen: HashMap<&str, &str> = HashMap::new();
    let mut reported: HashSet<&str> = HashSet::new();
    for (name, origin) in names.iter().zip(origins.iter()) {
        match seen.get(name.as_str()) {
            Some(prev) if !reported.contains(name.as_str()) => {
                reported.insert(name.as_str());
                let fname = f.name;
                let msg = format!(
                    "`{fname}` would emit the Lua parameter `{name}` twice: it is both {prev} and \
                     {origin}. The scalar ABI joins nested value fields with `__`, which is legal \
                     in an identifier too, so the two are indistinguishable and the body would \
                     read whichever Lua bound last -- rename one of them"
                );
                out.push((
                    format!("{fname}.{name}"),
                    CompilerError::new_with_label(f.name_span, msg),
                ));
            }
            Some(_) => {}
            None => {
                seen.insert(name.as_str(), origin.as_str());
            }
        }
    }
    out
}

/// A human description per emitted parameter, positionally parallel to
/// [`scalar_abi_lua_param_names`]: either the declared param itself, or which
/// leaf of which value param it flattens from (dotted, so it is readable and
/// unambiguous). Built by the same walk under the `.` separator.
fn scalar_abi_param_origins(ws: &Workspace, scx: &ScalarCtx, fref: FuncRef) -> Vec<String> {
    let Some(abi) = fn_scalar_abi(ws, scx, fref) else {
        return Vec::new();
    };
    let f = &ws.module(fref.module).functions[fref.func];
    let mut out = Vec::new();
    for (pd, shape) in f.param_decls.iter().zip(abi.param_shapes) {
        let pname = pd.name;
        match shape {
            Some(shape) => out.extend(
                leaf_names(&shape, pname.as_str(), ".")
                    .into_iter()
                    .map(|p| format!("the leaf `{p}` of value param `{pname}`")),
            ),
            None => out.push(format!("the declared param `{pname}`")),
        }
    }
    out
}

/// Append `<base>__<leaf>` for every primitive leaf of `shape`, depth-first.
/// `__` rather than `.` because these are Lua identifiers -- which is exactly
/// what makes the encoding non-injective and needs
/// [`value_leaf_name_collisions`] to guard it.
fn leaf_param_names(base: &str, shape: &ValueShape, out: &mut Vec<String>) {
    out.extend(leaf_names(shape, base, LEAF_PARAM_SEP));
}

/// The Lua leaf-param separator, and the reason a `__`-bearing field name can
/// collide with a nested path.
const LEAF_PARAM_SEP: &str = "__";

/// A chunk of a `string.format` conversion string: a static literal, or a
/// runtime int expression (dynamic width/precision) stringified into the format.
enum FmtPiece {
    Lit(String),
    DynInt(Idx<Expr>),
}

/// Decompose an interpolation spec into `string.format` conversion pieces
/// (`%[flags][width][.precision]type`). All-`Lit` => a static conversion; any
/// `DynInt` => assemble at runtime. `inferred_ty` supplies the conversion letter
/// when the spec omits one. `skip_width` leaves width out (the caller pads after
/// grouping); `skip_pad_attrs` leaves alignment/fill to the pad helper. `None`
/// for specs with no printf equivalent (custom fill / center alignment) — the
/// caller gaps so we never silently mis-format. Mirrors old skull `fmt_pieces`.
fn fmt_pieces(
    spec: &bone_semantic::hir::InterpSpec,
    inferred_ty: Option<char>,
    skip_width: bool,
    skip_pad_attrs: bool,
) -> Option<Vec<FmtPiece>> {
    if spec.fill.is_some() && !skip_pad_attrs {
        return None;
    }
    let ty = spec.format_type.or(inferred_ty)?;

    let mut pieces = Vec::new();
    let mut buf = String::from("%");
    if !skip_pad_attrs {
        match spec.alignment {
            None | Some('>') => {}       // right-align is printf's default
            Some('<') => buf.push('-'),  // left-align
            _ => return None,            // center etc. — no printf equivalent
        }
    }
    match spec.sign {
        Some('+') => buf.push('+'),
        Some(' ') => buf.push(' '),
        _ => {}
    }
    if spec.alternate {
        buf.push('#');
    }
    // Zero-pad only with a STATIC width (a dynamic width can't bake `0`+width
    // into the literal; falls back to space pad).
    if spec.zero_pad && spec.width_expr.is_none() {
        buf.push('0');
    }
    if skip_width {
    } else if let Some(w) = spec.width {
        buf.push_str(&w.to_string());
    } else if let Some(we) = spec.width_expr {
        pieces.push(FmtPiece::Lit(std::mem::take(&mut buf)));
        pieces.push(FmtPiece::DynInt(we));
    }
    if spec.precision.is_some() || spec.precision_expr.is_some() {
        buf.push('.');
        if let Some(p) = spec.precision {
            buf.push_str(&p.to_string());
        } else if let Some(pe) = spec.precision_expr {
            pieces.push(FmtPiece::Lit(std::mem::take(&mut buf)));
            pieces.push(FmtPiece::DynInt(pe));
        }
    }
    buf.push(ty);
    pieces.push(FmtPiece::Lit(buf));
    Some(pieces)
}

// A short tag for an unhandled expr variant, so a lowering gap names the form it
// hit (`...: InterpolatedString`) -- the corpus coverage lens buckets on it.
fn expr_tag(e: &Expr) -> &'static str {
    match e {
        Expr::As { .. } => "As",
        Expr::IncDec { .. } => "IncDec",
        Expr::InterpolatedString(_) => "InterpolatedString",
        Expr::Lambda { .. } => "Lambda",
        Expr::Range { .. } => "Range",
        Expr::Return(_) => "Return",
        Expr::SafeNav { .. } => "SafeNav",
        Expr::SafeNavCall { .. } => "SafeNavCall",
        Expr::SafeNavLet { .. } => "SafeNavLet",
        Expr::SiblingRef { .. } => "SiblingRef",
        Expr::Spread(_) => "Spread",
        Expr::Yield(_) => "Yield",
        Expr::YieldDelegate(_) => "YieldDelegate",
        _ => "other",
    }
}

fn raw(b: Idx<BindingInfo>) -> u32 {
    b.into_raw().into()
}

/// The `NativeBuffer*` nominal type names (T-typed + phantom-size variants).
fn is_native_buffer(name: &str) -> bool {
    matches!(
        name,
        "NativeBuffer"
            | "NativeBufferU8"
            | "NativeBufferI8"
            | "NativeBufferU16"
            | "NativeBufferI16"
            | "NativeBufferU32"
            | "NativeBufferI32"
    )
}

/// The scalar class of an operand, for the native-arithmetic dispatch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ArithTy {
    Int,
    Float,
    Str,
}

/// The typed native arithmetic instruction for an `__op_*` over two scalar
/// values of the given classes (the build-time int/float/string split, D8).
fn arith_inst(op: &str, lt: ArithTy, rt: ArithTy, lv: Idx<Inst>, rv: Idx<Inst>) -> Result<Inst, CompilerError> {
    let float = lt == ArithTy::Float || rt == ArithTy::Float;
    Ok(match op {
        "__op_add" if lt == ArithTy::Str || rt == ArithTy::Str => Inst::StringConcat(lv, rv),
        "__op_add" if float => Inst::FloatAdd(lv, rv),
        "__op_add" => Inst::IntAdd(lv, rv),
        "__op_sub" if float => Inst::FloatSub(lv, rv),
        "__op_sub" => Inst::IntSub(lv, rv),
        "__op_mul" if float => Inst::FloatMul(lv, rv),
        "__op_mul" => Inst::IntMul(lv, rv),
        // `/` is always float division (Lua `/` promotes int operands);
        // integer floor division is the separate `./` -> `__op_idiv`, which
        // codegen renders `math.floor(a / b)`.
        "__op_div" => Inst::FloatDiv(lv, rv),
        "__op_idiv" => Inst::IntDiv(lv, rv),
        "__op_mod" if float => Inst::FloatMod(lv, rv),
        "__op_mod" => Inst::IntMod(lv, rv),
        _ => return Err(gap(format!("native arith op {op}"))),
    })
}

/// Lower a single function to a [`MirFunc`]. Errors (a `gap`) when the body uses
/// a construct Phase 1 doesn't lower yet.
/// Owner segment of a `static`'s cell name. Stable across body edits (depends
/// only on the fn name + rendered param types), distinct per overload. Not the
/// mangled name -- skull-mir is upstream of the mangler -- but mir_emit reads
/// the resulting cell names back verbatim from `MirFunc::statics`, so exact
/// equality with the mangled name is not needed.
fn static_owner_tag(
    ws: &Workspace,
    inf: Option<&BodyInference>,
    name: Ustr,
    params: &[bone_semantic::hir::ParamDecl],
) -> Ustr {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    name.as_str().hash(&mut h);
    for pd in params {
        ws.render(inf, pd.ty).hash(&mut h);
    }
    Ustr::from(&format!("{}_{:06x}", name, h.finish() & 0xff_ffff))
}

/// Content hash of a `static` initializer -- the reload key's init half. Hashes
/// names / literals / operator kinds and recurses, but NEVER an arena `Idx`
/// (those shift when unrelated code in the body is edited). So it changes iff
/// the initializer's meaning changes, and editing code elsewhere in the fn
/// leaves it untouched (the cell is preserved across that reload).
fn hash_init_expr(body: &Body, e: Idx<Expr>, h: &mut std::collections::hash_map::DefaultHasher) {
    use bone_semantic::hir::{Callee, Expr, InterpPart, VariantArgs};
    use std::hash::Hash;
    macro_rules! go {
        ($x:expr) => {
            hash_init_expr(body, $x, h)
        };
    }
    macro_rules! tag {
        ($n:expr) => {
            ($n as u16).hash(h)
        };
    }
    match body.expr_arena[e] {
        Expr::Unit => tag!(0),
        Expr::Null => tag!(1),
        Expr::Hole(n) => {
            tag!(2);
            n.hash(h);
        }
        Expr::Int(v) => {
            tag!(3);
            v.hash(h);
        }
        Expr::Float(v) => {
            tag!(4);
            v.hash(h);
        }
        Expr::Bool(v) => {
            tag!(5);
            v.hash(h);
        }
        Expr::String(s) => {
            tag!(6);
            s.hash(h);
        }
        Expr::Var(b) => {
            tag!(7);
            body.binding_arena[b].name.hash(h);
        }
        Expr::Global(n) => {
            tag!(8);
            n.hash(h);
        }
        Expr::SiblingRef(n) => {
            tag!(9);
            n.hash(h);
        }
        Expr::Tuple(es) | Expr::Array(es) => {
            tag!(10);
            for x in es {
                go!(x);
            }
        }
        Expr::InterpolatedString(parts) => {
            tag!(13);
            for p in parts {
                match body.interp_part_arena[p] {
                    InterpPart::Literal(s) => {
                        tag!(130);
                        s.hash(h);
                    }
                    InterpPart::Value { expr, .. } => {
                        tag!(131);
                        go!(expr);
                    }
                }
            }
        }
        Expr::Not(i)
        | Expr::BitNot(i)
        | Expr::UnaryMinus(i)
        | Expr::Spread(i)
        | Expr::YieldDelegate(i) => {
            tag!(14);
            go!(i);
        }
        Expr::Return(o) => {
            tag!(19);
            if let Some(i) = o {
                go!(i);
            }
        }
        Expr::Yield(o) => {
            tag!(20);
            if let Some(i) = o {
                go!(i);
            }
        }
        Expr::Break => tag!(21),
        Expr::Continue => tag!(22),
        Expr::As { value, target } => {
            tag!(23);
            target.hash(h);
            go!(value);
        }
        Expr::TupleIndex { tuple, index } => {
            tag!(24);
            index.hash(h);
            go!(tuple);
        }
        Expr::FieldAccess { base, field } | Expr::SafeNav { base, field } => {
            tag!(25);
            field.hash(h);
            go!(base);
        }
        Expr::IncDec { target, decrement, postfix } => {
            tag!(27);
            decrement.hash(h);
            postfix.hash(h);
            go!(target);
        }
        Expr::Index { base, index } => {
            tag!(28);
            go!(base);
            go!(index);
        }
        Expr::Range { low, high } => {
            tag!(29);
            go!(low);
            go!(high);
        }
        Expr::BinOp { op, lhs, rhs } => {
            tag!(30);
            std::mem::discriminant(&op).hash(h);
            go!(lhs);
            go!(rhs);
        }
        Expr::Assign { target, value } => {
            tag!(31);
            go!(target);
            go!(value);
        }
        Expr::CompoundAssign { target, op_fn, value } => {
            tag!(32);
            op_fn.hash(h);
            go!(target);
            go!(value);
        }
        Expr::If { cond, then_branch, else_branch } => {
            tag!(33);
            go!(cond);
            go!(then_branch);
            if let Some(x) = else_branch {
                go!(x);
            }
        }
        Expr::IfLet { value, then_branch, else_branch, .. } => {
            tag!(34);
            go!(value);
            go!(then_branch);
            if let Some(x) = else_branch {
                go!(x);
            }
        }
        Expr::Lambda { body: b, .. } => {
            tag!(35);
            go!(b);
        }
        Expr::Block { statements, tail } => {
            tag!(36);
            for s in statements {
                hash_init_stmt(body, s, h);
            }
            if let Some(t) = tail {
                go!(t);
            }
        }
        Expr::Match { scrutinee, arms } => {
            tag!(37);
            go!(scrutinee);
            for a in arms {
                let arm = body.match_arm_arena[a];
                if let Some(g) = arm.guard {
                    go!(g);
                }
                go!(arm.body);
            }
        }
        Expr::Call { callee, args } => {
            tag!(38);
            match callee {
                Callee::Unresolved { name, .. } => {
                    tag!(380);
                    name.hash(h);
                }
                Callee::Local(b) => {
                    tag!(381);
                    body.binding_arena[b].name.hash(h);
                }
                Callee::Value(v) => {
                    tag!(382);
                    go!(v);
                }
            }
            for x in args {
                go!(x);
            }
        }
        Expr::SafeNavCall { receiver, method, args } => {
            tag!(39);
            method.hash(h);
            go!(receiver);
            for x in args {
                go!(x);
            }
        }
        Expr::SafeNavLet { receiver, body: b, .. } => {
            tag!(40);
            go!(receiver);
            go!(b);
        }
        Expr::StructLiteral { name, fields, bases } => {
            tag!(41);
            name.hash(h);
            for b in bases {
                go!(b);
            }
            for fld in fields {
                let f = body.struct_lit_field_arena[fld];
                f.name.hash(h);
                go!(f.value);
            }
        }
        Expr::EnumVariant { enum_name, variant, args, .. } => {
            tag!(42);
            enum_name.hash(h);
            variant.hash(h);
            match args {
                VariantArgs::Unit => tag!(420),
                VariantArgs::Tuple(es) => {
                    tag!(421);
                    for x in es {
                        go!(x);
                    }
                }
                VariantArgs::Record(fs) => {
                    tag!(422);
                    for fld in fs {
                        let f = body.struct_lit_field_arena[fld];
                        f.name.hash(h);
                        go!(f.value);
                    }
                }
            }
        }
    }
}

fn hash_init_stmt(body: &Body, s: Idx<Statement>, h: &mut std::collections::hash_map::DefaultHasher) {
    use bone_semantic::hir::Statement;
    use std::hash::Hash;
    macro_rules! go {
        ($x:expr) => {
            hash_init_expr(body, $x, h)
        };
    }
    match body.statement_arena[s] {
        Statement::Let { value, .. }
        | Statement::LetTuple { value, .. }
        | Statement::LetNewtype { value, .. }
        | Statement::Expr(value)
        | Statement::Defer(value)
        | Statement::Return(Some(value))
        | Statement::Static { value, .. } => go!(value),
        Statement::Fn { body: b, .. } => go!(b),
        Statement::For { iter, body: b, .. } => {
            go!(iter);
            go!(b);
        }
        Statement::While { cond, body: b } => {
            go!(cond);
            go!(b);
        }
        Statement::WhileLet { value, body: b, .. } => {
            go!(value);
            go!(b);
        }
        Statement::Return(None) | Statement::Break | Statement::Continue | Statement::Comment(_) => {
            (b'x').hash(h)
        }
    }
}

pub fn lower_function(
    ws: &Workspace,
    scx: &ScalarCtx,
    mid: ModuleId,
    fid: Idx<Function>,
) -> Result<MirFunc, CompilerError> {
    lower_function_spec(ws, scx, mid, fid, &SpecSubst::new())
}

/// Lower `fref` with a per-instantiation generic substitution (`docs/flatvec-plan.md`,
/// call-site specialization). `subst` maps the callee's generic `VarId`s to concrete
/// leaves so a value-instantiated body lowers as if hand-written for that type: value
/// params explode field-wise, `buf[i]=v` scalarizes, `native_buffer_alloc<T>` resolves
/// its ctype. An EMPTY `subst` is exactly [`lower_function`] (the shared generic /
/// non-generic snapshot), byte-identical.
pub fn lower_function_spec(
    ws: &Workspace,
    scx: &ScalarCtx,
    mid: ModuleId,
    fid: Idx<Function>,
    subst: &SpecSubst,
) -> Result<MirFunc, CompilerError> {
    let module = ws.module(mid);
    let func = &module.functions[fid];
    if func.inline_body.is_some() {
        return Err(gap("inline(lua) body"));
    }
    let body = module
        .bodies
        .get(&BodyOwner::Function(fid))
        .ok_or_else(|| gap("function has no body (extern?)"))?;
    let inf = ws
        .inference(mid)
        .and_then(|ir| ir.per_fn.get(&BodyOwner::Function(fid)));

    let mut f = MirFunc::new(func.name);
    // A `co fn` lowers to an ordinary CFG with `Term::Yield`s + a resume-arg cell
    // (the value `yield e` evaluates to). `skull-transform`'s coroutine pass then
    // rewrites it into the stackless state machine.
    let resume_arg = func
        .is_coroutine
        .then(|| f.new_cell(Some(Ustr::from("__resume_arg"))));
    f.is_coroutine = func.is_coroutine;
    f.resume_arg = resume_arg;
    let cur = f.entry();
    // Scalar value ABI: when this fn participates, each value param expands to
    // leaf cells and a value return becomes a multi-value `Return`. A specialized
    // instantiation additionally treats a generic param/return bound to a value
    // leaf as that value (`sig_value_shape_spec`), so it joins the scalar ABI.
    let self_ref = FuncRef { module: mid, func: fid };
    let abi = fn_scalar_abi_spec(ws, scx, self_ref, subst);
    let param_shapes = abi.as_ref().map(|a| a.param_shapes.clone()).unwrap_or_default();
    let ret_shape = abi.and_then(|a| a.ret_shape);
    let mut b = Builder {
        ws,
        scx,
        spec_subst: subst,
        body,
        inf,
        f,
        cur,
        binds: HashMap::new(),
        loop_targets: Vec::new(),
        used_break_targets: HashSet::default(),
        defers: Vec::new(),
        in_defer_body: false,
        resume_arg,
        // A coroutine body emits via `emit_coroutine` before `inline_functions`
        // runs, so it is never spliced -- box value args (see field docs).
        in_unspliced_ctx: func.is_coroutine,
        param_shapes,
        ret_shape,
        ret_tuple: func.return_ty.and_then(|t| tuple_ret_shape(ws, subst, t)),
        ret_ty: func.return_ty,
        fn_name_span: Some(func.name_span),
        cur_span: None,
        group_binds: HashMap::new(),
        tuple_binds: HashMap::new(),
        static_cells: HashMap::new(),
        // Lazy: only pay the param-type render/hash when the body has a static or
        // a nested fn (both bake the owner into a private global name that must be
        // distinct per overload). Keeps the tag off every other fn's compile.
        static_owner: if body
            .statement_arena
            .iter()
            .any(|(_, s)| matches!(s, Statement::Static { .. } | Statement::Fn { .. }))
        {
            static_owner_tag(ws, inf, func.name, &func.param_decls)
        } else {
            func.name
        },
        static_counters: HashMap::new(),
        nested_fn_globals: HashMap::new(),
        nested_fn_counters: HashMap::new(),
        presence_only: None,
    };
    b.reject_incompatible_nobox_fn_value_abi()?;
    b.lower_params();
    b.fill_param_defaults()?;
    b.lower_root()?;
    Ok(b.f)
}

/// Lower a module-level global's initializer body into a 0-param [`MirFunc`]
/// named `name` that **returns** the init value — the codegen emits it as a
/// `function name() ... end` and a `<GLOBAL> = name()` load-time line. `Ok(None)`
/// for a global with no initializer (declared, never assigned at module scope).
pub fn lower_global(
    ws: &Workspace,
    scx: &ScalarCtx,
    mid: ModuleId,
    gid: Idx<bone_semantic::hir::GlobalDef>,
    name: Ustr,
) -> Result<Option<MirFunc>, CompilerError> {
    let module = ws.module(mid);
    let body = module
        .bodies
        .get(&BodyOwner::Global(gid))
        .ok_or_else(|| gap("global has no body"))?;
    if body.root.is_none() {
        return Ok(None);
    }
    let inf = ws
        .inference(mid)
        .and_then(|ir| ir.per_fn.get(&BodyOwner::Global(gid)));
    let f = MirFunc::new(name);
    let cur = f.entry();
    let empty_subst = SpecSubst::new();
    let mut b = Builder {
        ws,
        scx,
        spec_subst: &empty_subst,
        body,
        inf,
        f,
        cur,
        binds: HashMap::new(),
        loop_targets: Vec::new(),
        used_break_targets: HashSet::default(),
        defers: Vec::new(),
        in_defer_body: false,
        resume_arg: None,
        // Global-init bodies never run `inline_functions` -- box value args.
        in_unspliced_ctx: true,
        param_shapes: Vec::new(),
        ret_shape: None,
        ret_tuple: None,
        ret_ty: None,
        fn_name_span: None,
        cur_span: None,
        group_binds: HashMap::new(),
        tuple_binds: HashMap::new(),
        static_cells: HashMap::new(),
        static_owner: name,
        static_counters: HashMap::new(),
        nested_fn_globals: HashMap::new(),
        nested_fn_counters: HashMap::new(),
        presence_only: None,
    };
    // Same gate the fn and decorator bodies run. An initializer can name a fn
    // whose fixed ABI cannot adopt the type-derived leaf ABI, and a global is a
    // fn value's most ordinary home (a dispatch table); without this the name
    // reached codegen and the leaves splatted into a callee expecting a box.
    b.reject_incompatible_nobox_fn_value_abi()?;
    // A `nobox` global is category-2 storage: its layout comes from the declared
    // type, so the initializer RETURNS leaves and `mir_emit` binds one Lua global
    // per leaf. Without this the body packs a box the verifier then rejects.
    let g = &module.globals[gid];
    b.ret_shape = nobox_declared_shape(ws, g.ty);
    b.lower_root()?;
    Ok(Some(b.f))
}

/// Lower a `deco <kind> <name> { body }` definition into a [`MirFunc`] named
/// `name` (the codegen `__deco_<name>`) taking `(target, <declared params>)`.
/// `target` is the decorated-fn descriptor (`{ name, params, func }`) the body
/// reads via `target.name` etc.; the body's value is discarded (run at module
/// load per `@<name>` application).
pub fn lower_decorator(
    ws: &Workspace,
    scx: &ScalarCtx,
    mid: ModuleId,
    did: Idx<bone_semantic::hir::DecoratorDef>,
    name: Ustr,
) -> Result<MirFunc, CompilerError> {
    let module = ws.module(mid);
    let body = module
        .bodies
        .get(&BodyOwner::Decorator(did))
        .ok_or_else(|| gap("decorator has no body"))?;
    let inf = ws
        .inference(mid)
        .and_then(|ir| ir.per_fn.get(&BodyOwner::Decorator(did)));
    let target = body
        .deco_target
        .ok_or_else(|| gap("decorator body has no target binding"))?;
    let f = MirFunc::new(name);
    let cur = f.entry();
    let empty_subst = SpecSubst::new();
    let mut b = Builder {
        ws,
        scx,
        spec_subst: &empty_subst,
        body,
        inf,
        f,
        cur,
        binds: HashMap::new(),
        loop_targets: Vec::new(),
        used_break_targets: HashSet::default(),
        defers: Vec::new(),
        in_defer_body: false,
        resume_arg: None,
        // Decorator bodies never run `inline_functions` -- box value args.
        in_unspliced_ctx: true,
        param_shapes: Vec::new(),
        ret_shape: None,
        ret_tuple: None,
        ret_ty: None,
        fn_name_span: None,
        cur_span: None,
        group_binds: HashMap::new(),
        tuple_binds: HashMap::new(),
        static_cells: HashMap::new(),
        static_owner: name,
        static_counters: HashMap::new(),
        nested_fn_globals: HashMap::new(),
        nested_fn_counters: HashMap::new(),
        presence_only: None,
    };
    // `target` is param 0, then the decorator's declared params.
    let tcell = b.f.new_cell(Some(b.body.binding_arena[target].name));
    b.f.params.push(tcell);
    b.binds.insert(raw(target), tcell);
    b.reject_incompatible_nobox_fn_value_abi()?;
    b.lower_params();
    b.fill_param_defaults()?;
    b.lower_root()?;
    Ok(b.f)
}

struct Builder<'a> {
    ws: &'a Workspace,
    scx: &'a ScalarCtx,
    /// Per-instantiation generic substitution (`docs/flatvec-plan.md` call-site
    /// specialization). Empty for the shared generic / non-generic snapshot; a
    /// value-instantiated splice carries `callee generic VarId -> concrete type`
    /// so type resolution (`type_of`/`native_buffer_alloc_ctype`/param shapes)
    /// sees the concrete type and the body scalarizes.
    spec_subst: &'a SpecSubst,
    body: &'a Body,
    inf: Option<&'a BodyInference>,
    f: MirFunc,
    cur: Idx<Block>,
    /// raw binding index -> the variable cell it maps to.
    binds: HashMap<u32, Idx<Cell>>,
    /// Innermost-last stack of `(continue_target, break_target)` blocks.
    loop_targets: Vec<(Idx<Block>, Idx<Block>)>,
    /// Break targets some `break` actually jumped to. A `while true` whose exit
    /// was never targeted terminates the statement list (its fall-through is
    /// unreachable), which is what lets a `while true { ... return ... }` co fn
    /// body pass the non-unit fall-off rejection.
    used_break_targets: HashSet<Idx<Block>>,
    /// Innermost-last stack of pending `defer` bodies per block scope; spliced
    /// (LIFO) at block exit and before every `return`. In a co fn each entry
    /// also carries its terminal-cleanup flag cell (see `MirFunc::coro_defers`);
    /// the splice clears it so kill/error never double-run a completed defer.
    defers: Vec<Vec<(Idx<Expr>, Option<Idx<Cell>>)>>,
    /// True while lowering a `defer` body (inline splice or terminal copy) --
    /// `yield`/`await` inside a defer body is a compile error.
    in_defer_body: bool,
    /// In a `co fn`, the cell that `yield e` evaluates to (the value passed to
    /// the next `resume(arg)`). `None` outside a coroutine.
    resume_arg: Option<Idx<Cell>>,
    /// True while lowering a body whose calls this pass cannot give the
    /// specialized scalar ABI: a closure sub-CFG, a hoisted nested fn, a
    /// coroutine, a global init, or a decorator. A call here must NOT take the
    /// SPECIALIZED scalar ABI (splatting a value arg to leaves), which is only
    /// sound when the inliner replaces the call with the matching specialized
    /// body -- an unspliced residual would call the SHARED generic body (`T` = one
    /// boxed slot) with splatted args and drop every leaf but the first. Boxing
    /// the value arg matches the shared body exactly.
    in_unspliced_ctx: bool,
    /// Per-param scalarized shape: `Some` for a value param expanded to leaf
    /// cells (parallel to `body.param_bindings`). Empty when the fn doesn't use
    /// the scalar ABI (globals, decorators, non-value fns).
    param_shapes: Vec<Option<ValueShape>>,
    /// `Some` when the fn returns a value struct via the scalar ABI (N leaves).
    ret_shape: Option<ValueShape>,
    /// `Some` when the fn's return type is a tuple -- it returns its slots as
    /// separate values (multi-return ABI), not one heap table. Set for named
    /// fns; lambdas decide per-body at their own return site.
    ret_tuple: Option<TupleRetShape>,
    /// The fn's declared return type -- the expected type at a scalar `return`
    /// sink (int->float subsumption coercion, see [`Builder::coerced`]).
    ret_ty: Option<TyId>,
    /// The fn's name span, for spanned emit-time rejections that have no better
    /// anchor (the non-unit-coroutine fall-off check). `None` for non-fn bodies.
    fn_name_span: Option<Span>,
    /// The source span currently being lowered, maintained by [`Builder::lower_expr`].
    /// Instructions a post-transform check can reject stamp it into
    /// [`MirFunc::inst_spans`] so the diagnostic points at the construct rather
    /// than at the enclosing fn.
    cur_span: Option<Span>,
    /// A value PARAM binding -> (struct name, its leaf cells, shape). A read of
    /// the param reconstructs the unboxed `Group` from these; a field read
    /// projects a leaf directly (no cdata).
    group_binds: HashMap<u32, (Vec<Idx<Cell>>, ValueShape)>,
    /// The tuple twin of [`Self::group_binds`]: a NAME bound to a splatting
    /// tuple holds its slot cells, so `if let t = <nullable tuple call>` can
    /// rebuild the unboxed repr on each read of `t` instead of packing it.
    /// Keyed the same way; the shape is stored non-nullable because presence
    /// was already consumed by the branch that bound it.
    tuple_binds: HashMap<u32, (Vec<Idx<Cell>>, TupleRetShape)>,
    /// `static` locals: raw binding idx -> the private global cell name a read /
    /// write of the binding resolves to (an `Inst::Global`/`StoreGlobal`, not a
    /// local cell). Populated by `lower_static`.
    static_cells: HashMap<u32, Ustr>,
    /// Owner tag baked into a static's cell name -- stable across body edits,
    /// distinct per overload (fn name + rendered param types). NOT the mangled
    /// name (skull-mir is upstream of the mangler); it only needs uniqueness +
    /// stability, which mir_emit reads back verbatim from `MirFunc::statics`.
    static_owner: Ustr,
    /// Per static source-name counter (source order) so shadowing duplicates get
    /// distinct cells and adding an unrelated static never renames siblings.
    static_counters: HashMap<Ustr, u32>,
    /// Nested `fn` names: raw binding idx -> the hoisted `__local_*` global a
    /// call / value-ref of the binding resolves to, plus its value ABI.
    /// Inherited (cloned) by a nested fn's own builder so it can reference
    /// itself + prior siblings.
    nested_fn_globals: HashMap<u32, NestedFn>,
    /// Per nested-fn source-name counter (source order), the `static_counters`
    /// analogue for shadowing duplicates.
    nested_fn_counters: HashMap<Ustr, u32>,
    /// The one expression currently being lowered for its PRESENCE only (`x ==
    /// null`): a `V?` place read there skips its copy-out, because a nil test
    /// answers the same question without a cdata
    /// ([`Builder::nullable_place_value_of`]). Exact node, never a subtree -- a
    /// nested read that feeds a real whole-value use still copies.
    presence_only: Option<Idx<Expr>>,
}

impl Builder<'_> {
    /// [`gap`] anchored at the construct currently being lowered. Use wherever a
    /// gap names a user-visible type or expression -- an unspanned one leaves the
    /// reader with a whole project to search.
    fn gap_here(&self, msg: impl std::fmt::Display) -> CompilerError {
        match self.cur_span {
            Some(span) => CompilerError::new_with_label(
                span,
                format!("skull-mir: lowering gap: {msg}"),
            ),
            None => gap(msg),
        }
    }

    fn emit_cur(&mut self, inst: Inst) -> Idx<Inst> {
        let id = self.f.emit(self.cur, inst);
        if let Some(span) = self.cur_span {
            self.f.inst_spans.insert(id, span);
        }
        id
    }

    /// Whether the current block is still open (no terminator set yet). A
    /// diverging sub-expression (`return`/`break`/`continue`) sets a real
    /// terminator, so after lowering it `is_open()` is false and the caller
    /// must NOT append a fall-through `Store`/`Goto`.
    fn is_open(&self) -> bool {
        matches!(self.f.blocks[self.cur].term, Term::Unreachable)
    }

    fn lower_params(&mut self) {
        let pbs: Vec<Idx<BindingInfo>> = self.body.param_bindings.into_iter().collect();
        for (i, pb) in pbs.iter().enumerate() {
            let name = self.body.binding_arena[*pb].name;
            // A value param under the scalar ABI expands to N leaf cells (the Lua
            // param list); reads reconstruct the unboxed Group from them.
            match self.param_shapes.get(i).and_then(|s| s.clone()) {
                Some(shape) => {
                    let n = shape.leaf_count();
                    let mut cells = Vec::with_capacity(n);
                    for li in 0..n {
                        let cn = Ustr::from(format!("{name}__{li}").as_str());
                        let c = self.f.new_cell(Some(cn));
                        self.f.params.push(c);
                        cells.push(c);
                    }
                    self.group_binds.insert(raw(*pb), (cells, shape));
                }
                None => {
                    let cty = self.binding_ty(*pb);
                    let cell = self.f.new_cell_typed(Some(name), cty);
                    self.f.params.push(cell);
                    self.binds.insert(raw(*pb), cell);
                }
            }
        }
    }

    /// Callee-side default fill: an omitted argument arrives as Lua `nil`, so for
    /// each defaulted param emit `if cell == nil { cell = <default> }` at the top
    /// of the body (rock's convention — the default expr lives in THIS body's
    /// arena). A trailing omitted arg is simply not passed (Lua nils it); a
    /// named-arg gap is passed an explicit nil by the call site.
    fn fill_param_defaults(&mut self) -> Result<(), CompilerError> {
        let pbs: Vec<Idx<BindingInfo>> = self.body.param_bindings.into_iter().collect();
        let defaults = self.body.param_defaults.clone();
        for (i, def) in defaults.iter().enumerate() {
            let Some(def) = def else { continue };
            let Some(&pb) = pbs.get(i) else { continue };
            // A scalarized value param's omitted arg arrives as N nils — guard on
            // the FIRST leaf cell and fill all leaves from the default's leaves.
            if let Some((cells, shape)) = self.group_binds.get(&raw(pb)).cloned() {
                let cur = self.emit_cur(Inst::Load(cells[0]));
                let nil = self.emit_cur(Inst::Const(Lit::Nil));
                let is_nil = self.emit_cur(Inst::Eq(cur, nil));
                let then_bb = self.f.new_block();
                let join = self.f.new_block();
                self.f.set_term(self.cur, Term::Branch { cond: is_nil, then_bb, else_bb: join });
                self.cur = then_bb;
                let r = self.lower_expr_repr(*def)?;
                let leaves = self.repr_to_leaves(&r, &shape);
                for (c, v) in cells.iter().zip(leaves) {
                    self.emit_cur(Inst::Store { cell: *c, value: v });
                }
                if self.is_open() {
                    self.f.set_term(self.cur, Term::Goto(join));
                }
                self.cur = join;
                continue;
            }
            let Some(&cell) = self.binds.get(&raw(pb)) else { continue };
            let cur = self.emit_cur(Inst::Load(cell));
            let nil = self.emit_cur(Inst::Const(Lit::Nil));
            let is_nil = self.emit_cur(Inst::Eq(cur, nil));
            let then_bb = self.f.new_block();
            let join = self.f.new_block();
            self.f.set_term(self.cur, Term::Branch { cond: is_nil, then_bb, else_bb: join });
            self.cur = then_bb;
            let dv = self.lower_expr(*def)?;
            self.emit_cur(Inst::Store { cell, value: dv });
            self.f.set_term(self.cur, Term::Goto(join));
            self.cur = join;
        }
        Ok(())
    }

    /// Lower the body's root expression, ending the entry block in a `Return`.
    /// A top-level `Block` is special-cased so a tail-less body returns bare.
    fn lower_root(&mut self) -> Result<(), CompilerError> {
        let Some(root) = self.body.root else {
            self.f.set_term(self.cur, Term::Return(vec![]));
            return Ok(());
        };
        if let Expr::Block { statements, tail } = self.body.expr_arena[root] {
            self.defers.push(Vec::new());
            let terminated = self.lower_stmts(statements)?;
            if terminated {
                self.defers.pop();
                return Ok(());
            }
            // A coroutine result comes ONLY from explicit `return` statements
            // (bone discards a co fn's body tail without coercing it to R), so
            // ANY reachable body end -- bare fall-off or unchecked tail -- is a
            // fall-off for the non-unit-R rejection below.
            if self.f.is_coroutine {
                self.reject_nonunit_coroutine_falloff()?;
            }
            let leaves = self.return_leaves(tail)?;
            self.run_top_frame()?; // splice this scope's defers before the return
            self.defers.pop();
            self.f.set_term(self.cur, Term::Return(leaves));
            Ok(())
        } else {
            let leaves = self.return_leaves(Some(root))?;
            self.f.set_term(self.cur, Term::Return(leaves));
            Ok(())
        }
    }

    /// Reject a REACHABLE fall-off in a `co fn` whose declared result R is a
    /// concrete non-unit, non-nullable type: bone coerces only explicit
    /// `return <expr>` statements to R (a coroutine's body tail is discarded,
    /// never coerced), so a fall-off would stamp the completed terminal with
    /// `__return` nil and `result()`/`take_result` would hand out a nil typed
    /// as R. Unit R (the default), nullable R (nil is a legal value), and
    /// generic R (unknown at the shared lowering; explicit `return` sites in
    /// instantiations are still checked by bone) all stay admitted. Called only
    /// when the body's end is reachable, so `while true` bodies never trip it.
    fn reject_nonunit_coroutine_falloff(&self) -> Result<(), CompilerError> {
        let Some(ret) = self.ret_ty else { return Ok(()) };
        let Some(TypeExpr::App { ctor: TypeCtor::Nominal(n), params }) = static_ty(self.ws, ret)
        else {
            return Ok(());
        };
        if n.as_str() != "Coroutine" {
            return Ok(());
        }
        let TyId::Static(m, _) = ret else { return Ok(()) };
        // Fewer than two args means R was default-filled to `()`.
        let Some(r_idx) = params.into_iter().nth(1) else { return Ok(()) };
        let nonunit_concrete = match self.ws.module(m).ty_arena[r_idx] {
            TypeExpr::Builtin(BuiltinTy::Unit | BuiltinTy::Error) => false,
            TypeExpr::Tuple { elements } if elements.into_iter().count() == 0 => false,
            TypeExpr::Nullable(_) => false,
            TypeExpr::Generic(..) | TypeExpr::Infer(..) => false,
            _ => true,
        };
        if nonunit_concrete {
            let msg = format!(
                "co fn `{}` can fall off the end of its body, but its coroutine result type is not `()` -- \
                 a completed coroutine would hand `result()` a nil typed as the declared result; \
                 add an explicit `return <value>` on every path",
                self.f.name
            );
            return Err(match self.fn_name_span {
                Some(span) => CompilerError::new_with_label(span, msg),
                None => CompilerError::new_no_location(msg),
            });
        }
        Ok(())
    }

    /// The return-value list for `opt`: under the scalar value ABI a value return
    /// flattens to its leaf scalars (`return v0, v1, ...`); otherwise it's the
    /// single (boxed) value, or empty for a bare return.
    fn return_leaves(&mut self, opt: Option<Idx<Expr>>) -> Result<Vec<Idx<Inst>>, CompilerError> {
        let Some(e) = opt else { return Ok(vec![]) };
        if let Some(shape) = self.ret_shape.clone() {
            let r = self.lower_expr_repr(e)?;
            return Ok(self.repr_to_leaves(&r, &shape));
        }
        if let Some(shape) = self.ret_tuple.clone() {
            if shape.nullable {
                return self.nullable_tuple_return_slots(e, &shape);
            }
            if shape.splats() {
                let r = self.lower_expr_repr(e)?;
                return Ok(self.tuple_repr_to_slots(r, &shape));
            }
            let v = self.lower_expr(e)?;
            return Ok(self.tuple_return_operands(v, shape.slots.len()));
        }
        let v = self.lower_expr(e)?;
        let v = match self.ret_ty {
            Some(rt) => self.coerced(v, e, rt),
            None => v,
        };
        Ok(vec![v])
    }

    fn absent_tuple_slots(&mut self, shape: &TupleRetShape) -> Vec<Idx<Inst>> {
        let mut out = Vec::with_capacity(shape.slot_count());
        if shape.nullable {
            out.push(self.emit_cur(Inst::Const(Lit::Bool(false))));
        }
        while out.len() < shape.slot_count() {
            out.push(self.emit_cur(Inst::Const(Lit::Nil)));
        }
        out
    }

    /// The `present, <slots...>` wire for a `-> (..)?` return, the tuple analogue
    /// of the nullable arms in [`Self::repr_to_leaves`]: a literal `null` folds to
    /// `false, nil, ...` and a tuple value leads with `true`.
    ///
    /// A tuple-or-nil arriving as an already-packed SCALAR (a variable, an if-join)
    /// would need a runtime `~= nil` test and a per-slot branch. That is a real
    /// shape, just not one any in-tree caller writes, so it gaps rather than
    /// silently boxing the nobox element the wire exists to keep unboxed.
    fn nullable_tuple_return_slots(
        &mut self,
        e: Idx<Expr>,
        shape: &TupleRetShape,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        let r = self.lower_expr_repr(e)?;
        if let ValueRepr::Scalar(v) = &r {
            if !matches!(self.f.insts[*v], Inst::Const(Lit::Nil)) {
                return Err(gap(
                    "returning an already-packed nullable tuple through the splatting ABI \
                     (return the tuple literal or `null` directly)",
                ));
            }
            return Ok(self.absent_tuple_slots(shape));
        }
        let mut out = vec![self.emit_cur(Inst::Const(Lit::Bool(true)))];
        out.extend(self.tuple_repr_to_slots(r, shape));
        Ok(out)
    }

    /// The N return operands for a tuple-return `return <v>`. A fresh `MakeTuple`
    /// in return position (the common `return (a, b)` and the `return f()` forward
    /// where `f()` lowered to `MakeTuple(loads)`) is elided to its operands -- no
    /// table alloc. A tuple value from elsewhere (a var/field holding a packed
    /// table) is unpacked with `TupleIndex`.
    fn tuple_return_operands(&mut self, v: Idx<Inst>, n: usize) -> Vec<Idx<Inst>> {
        if let Inst::MakeTuple(vs) = &self.f.insts[v] {
            if vs.len() == n {
                return vs.clone();
            }
        }
        (0..n)
            .map(|i| self.emit_cur(Inst::TupleIndex { base: v, index: i }))
            .collect()
    }

    /// Flatten a tuple repr to the slot values `shape` describes -- the tuple
    /// analogue of [`Self::repr_to_leaves`]. A `Tuple` repr (a splatting call
    /// forwarded straight out, or a tuple literal) yields its parts; anything
    /// else is a packed tuple, so each element is read back with `TupleIndex`
    /// and a `Leaves` slot is field-read off the box.
    fn tuple_repr_to_slots(&mut self, r: ValueRepr, shape: &TupleRetShape) -> Vec<Idx<Inst>> {
        // A phantom tuple has no aggregate to project; every wire slot is absent.
        if let ValueRepr::Scalar(v) = &r {
            if matches!(self.f.insts[*v], Inst::Const(Lit::Nil)) {
                return self.absent_tuple_slots(shape);
            }
        }
        let parts: Vec<ValueRepr> = match r {
            ValueRepr::Tuple { parts, .. } if parts.len() == shape.slots.len() => parts,
            other => {
                let base = self.pack(other);
                (0..shape.slots.len())
                    .map(|index| ValueRepr::Scalar(self.emit_cur(Inst::TupleIndex { base, index })))
                    .collect()
            }
        };
        let mut out = Vec::with_capacity(shape.slot_count());
        for (slot, part) in shape.slots.iter().zip(parts) {
            match slot {
                RetSlot::Opaque => out.push(self.pack(part)),
                // A BOXED source at a non-nullable splatting slot is the iterator
                // protocol's "no element" placeholder (`return (false,
                // phantom())`), which is nil -- field-reading it would fault, and
                // the slot is dead anyway because the tuple's own flag guards it.
                // The nullable spread already branches on nil, so borrow it and
                // drop the presence leaf.
                RetSlot::Leaves(vs)
                    if !vs.nullable && !matches!(part, ValueRepr::Group { .. }) =>
                {
                    let mut nullable = vs.clone();
                    nullable.nullable = true;
                    let leaves = self.repr_to_leaves(&part, &nullable);
                    out.extend(leaves.into_iter().skip(1));
                }
                RetSlot::Leaves(vs) => out.extend(self.repr_to_leaves(&part, vs)),
                RetSlot::Nested(inner) => out.extend(self.tuple_repr_to_slots(part, inner)),
            }
        }
        out
    }

    /// Rebuild the tuple repr `shape` describes from a flat slot list -- the
    /// inverse of [`Self::tuple_repr_to_slots`], used at a call site to hand a
    /// destructure its elements without ever boxing a `nobox` one.
    fn tuple_from_slots(
        &mut self,
        shape: &TupleRetShape,
        slots: &[Idx<Inst>],
        idx: &mut usize,
    ) -> ValueRepr {
        // Presence leads the wire, exactly as it does one level down in
        // `rebuild_from_leaves`, so it is consumed before the slots.
        let present = shape.nullable.then(|| {
            let v = slots[*idx];
            *idx += 1;
            v
        });
        let parts = shape
            .slots
            .clone()
            .iter()
            .map(|slot| self.slot_repr(slot, slots, idx))
            .collect();
        ValueRepr::Tuple { parts, present }
    }

    /// One element of [`Self::tuple_from_slots`], consuming its slots from `*idx`.
    fn slot_repr(&mut self, slot: &RetSlot, slots: &[Idx<Inst>], idx: &mut usize) -> ValueRepr {
        match slot {
            RetSlot::Opaque => {
                let v = slots[*idx];
                *idx += 1;
                ValueRepr::Scalar(v)
            }
            RetSlot::Leaves(vs) => self.rebuild_from_leaves(vs, slots, idx),
            RetSlot::Nested(inner) => self.tuple_from_slots(inner, slots, idx),
        }
    }

    fn repr_to_slot_values(&mut self, repr: ValueRepr, slot: &RetSlot) -> Vec<Idx<Inst>> {
        match slot {
            RetSlot::Opaque => vec![self.pack(repr)],
            RetSlot::Leaves(shape) => self.repr_to_leaves(&repr, shape),
            RetSlot::Nested(shape) => self.tuple_repr_to_slots(repr, shape),
        }
    }

    /// Lower a statement sequence into the current block. Returns `true` if a
    /// `return` terminated the block (the rest is dead).
    /// `static name = init` -> a private global cell + done-flag, guarded so the
    /// initializer runs only on the first reach:
    /// `if not <done> then <cell> = <init>; <done> = true end`. Cell/done-flag
    /// names are recorded on `MirFunc::statics` for the mir_emit reload preamble;
    /// reads/writes of the binding resolve to the cell global via the
    /// `static_cells` guards in the read/assign/compound/incdec paths.
    fn lower_static(
        &mut self,
        name: Idx<BindingInfo>,
        value: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        let sname = self.body.binding_arena[name].name;
        let ctr = {
            let c = self.static_counters.entry(sname).or_insert(0);
            let v = *c;
            *c += 1;
            v
        };
        let cell = Ustr::from(format!("__static_{}_{}_{}", self.static_owner, sname, ctr).as_str());
        let done_flag = Ustr::from(format!("{cell}__done").as_str());
        self.static_cells.insert(raw(name), cell);

        let ty = self
            .inf
            .and_then(|i| i.binding_types.get(name).copied())
            .or(self.body.binding_arena[name].declared_ty);
        let type_str = ty
            .map(|t| self.ws.render(self.inf, t))
            .unwrap_or_else(|| "any".to_string());
        let init_hash = {
            use std::hash::Hasher;
            let mut hsh = std::collections::hash_map::DefaultHasher::new();
            hash_init_expr(self.body, value, &mut hsh);
            hsh.finish()
        };
        self.f.statics.push(StaticCell { cell, done_flag, type_str, init_hash });

        let done_v = self.emit_cur(Inst::Global(done_flag));
        let init_bb = self.f.new_block();
        let join = self.f.new_block();
        self.f
            .set_term(self.cur, Term::Branch { cond: done_v, then_bb: join, else_bb: init_bb });
        self.cur = init_bb;
        let v = self.lower_expr(value)?;
        self.emit_cur(Inst::StoreGlobal { name: cell, value: v });
        let t = self.emit_cur(Inst::Const(Lit::Bool(true)));
        self.emit_cur(Inst::StoreGlobal { name: done_flag, value: t });
        self.f.set_term(self.cur, Term::Goto(join));
        self.cur = join;
        Ok(())
    }

    /// The value ABI of a nested `fn`, read by both its definition and its call
    /// sites. Same rule as [`fn_scalar_abi`] for a top-level fn, minus annotation
    /// gates: a fn value narrows to `nobox` shapes; a direct-only fn may use every
    /// scalarizable value shape.
    fn nested_fn_abi(
        &self,
        name: Idx<BindingInfo>,
        params: IdxRange<BindingInfo>,
        ret_ty: Option<TyId>,
        hoisted: Ustr,
    ) -> NestedFn {
        let param_names: Vec<Ustr> =
            params.into_iter().map(|pb| self.body.binding_arena[pb].name).collect();
        let boxed =
            NestedFn { hoisted, param_names: param_names.clone(), param_shapes: Vec::new(), ret_shape: None };
        let taken_by_value = self
            .body
            .expr_arena
            .iter()
            .any(|(_, e)| matches!(e, Expr::Var(b) if *b == name));
        if self.scx.values_first_class {
            return boxed;
        }
        // TAKEN BY VALUE: the ABI narrows to what an indirect call site can
        // compute from the fn TYPE alone ([`Self::fn_value_abi`]), which is
        // `nobox` values only. Both ends must run the SAME predicate -- using
        // the wider `body_sig_value_shape` here would scalarize an ordinary
        // value param that an opaque call site still boxes, and the two would
        // disagree exactly where neither can see the other. Dropping to the
        // boxed ABI entirely, which is what this used to do, is the same
        // disagreement in the other direction: it is what made a `nobox` param
        // reject once the fn was taken by value.
        let shape_of = |s: &Self, t: TyId| {
            if taken_by_value {
                s.nobox_shape_of_ty(t)
            } else {
                s.body_sig_value_shape(t)
            }
        };
        let abi = NestedFn {
            hoisted,
            param_names,
            param_shapes: params
                .into_iter()
                .map(|pb| self.binding_ty(pb).and_then(|t| shape_of(self, t)))
                .collect(),
            ret_shape: self
                .nested_fn_ret_ty(name, ret_ty)
                .and_then(|t| shape_of(self, t)),
        };
        if abi.uses_scalar_abi() { abi } else { boxed }
    }

    /// A nested `fn`'s return type. An ANNOTATION is authoritative when present;
    /// without one the type is inferred, and it must be read off the binding's own
    /// `fn(..) -> R` type rather than defaulted away -- an inferred value return is
    /// as much a part of the signature as a written one, and treating it as absent
    /// silently drops the callee back to the boxed ABI.
    fn nested_fn_ret_ty(&self, name: Idx<BindingInfo>, ret_ty: Option<TyId>) -> Option<TyId> {
        if ret_ty.is_some() {
            return ret_ty;
        }
        let fty = self.resolve_tyid(self.binding_ty(name)?)?;
        match self.ty_node(fty)? {
            TypeExpr::Function { ret, .. } => Some(fty.with_idx(ret)),
            _ => None,
        }
    }

    /// [`sig_value_shape`] for a type that may live in the BODY's inference arena
    /// rather than the workspace one -- a nested `fn`'s params are declared inside
    /// the enclosing body, so `static_ty` cannot see them. Same shape policy
    /// (`sig_shape_of_nominal`), reached through `ty_node`, which also applies the
    /// enclosing specialization substitution.
    fn body_sig_value_shape(&self, ty: TyId) -> Option<ValueShape> {
        let ty = self.resolve_tyid(ty)?;
        match self.ty_node(ty)? {
            TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } => sig_shape_of_nominal(self.ws, n),
            TypeExpr::Nullable(inner) => {
                let inner = self.resolve_tyid(ty.with_idx(inner))?;
                match self.ty_node(inner)? {
                    TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } => {
                        sig_shape_of_nullable_nominal(self.ws, n)
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Hoist a nested `fn` to a private top-level [`MirFunc`] `__local_<owner>_
    /// <name>_<ctr>`. The name is registered in `nested_fn_globals` BEFORE the
    /// body is lowered so self-recursion resolves; the child inherits the
    /// parent's in-scope statics + sibling nested fns (decl-before-use).
    fn lower_nested_fn(
        &mut self,
        name: Idx<BindingInfo>,
        params: IdxRange<BindingInfo>,
        ret_ty: Option<TyId>,
        body: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        let sname = self.body.binding_arena[name].name;
        let ctr = {
            let c = self.nested_fn_counters.entry(sname).or_insert(0);
            let v = *c;
            *c += 1;
            v
        };
        let hoisted = Ustr::from(format!("__local_{}_{}_{}", self.static_owner, sname, ctr).as_str());
        let abi = self.nested_fn_abi(name, params, ret_ty, hoisted);
        self.nested_fn_globals.insert(raw(name), abi.clone());

        let mf = MirFunc::new(hoisted);
        let entry = mf.entry();
        let mut child = Builder {
            ws: self.ws,
            scx: self.scx,
            spec_subst: self.spec_subst,
            body: self.body,
            inf: self.inf,
            f: mf,
            cur: entry,
            binds: HashMap::new(),
            loop_targets: Vec::new(),
            used_break_targets: HashSet::default(),
            defers: Vec::new(),
            in_defer_body: false,
            resume_arg: None,
            // A hoisted nested fn is emitted via `emit_nested_fns`, which never
            // runs `inline_functions` -- box value args.
            in_unspliced_ctx: true,
            param_shapes: abi.param_shapes.clone(),
            ret_shape: abi.ret_shape.clone(),
            ret_tuple: None,
            // A nested fn is emitted as a sibling top-level Lua fn and is never a
            // GLSL callee (referenced indirectly), so a return coercion here is
            // Lua-identity only; leaving it off matches today's emitted text.
            ret_ty: None,
            fn_name_span: None,
            cur_span: None,
            group_binds: HashMap::new(),
        tuple_binds: HashMap::new(),
            // In-scope statics + sibling nested fns are module-level, so the
            // hoisted body may reference them; enclosing params/locals are NOT
            // inherited (bone rejects capturing them).
            static_cells: self.static_cells.clone(),
            static_owner: hoisted,
            static_counters: HashMap::new(),
            nested_fn_globals: self.nested_fn_globals.clone(),
            nested_fn_counters: HashMap::new(),
            presence_only: None,
        };

        for (i, pb) in params.into_iter().enumerate() {
            let pname = child.body.binding_arena[pb].name;
            match child.param_shapes.get(i).and_then(|s| s.clone()) {
                Some(shape) => {
                    let cells: Vec<Idx<Cell>> = (0..shape.leaf_count())
                        .map(|li| child.f.new_cell(Some(Ustr::from(format!("{pname}__{li}").as_str()))))
                        .collect();
                    child.f.params.extend(cells.iter().copied());
                    child.group_binds.insert(raw(pb), (cells, shape));
                }
                None => {
                    let cell = child.f.new_cell(Some(pname));
                    child.f.params.push(cell);
                    child.binds.insert(raw(pb), cell);
                }
            }
        }

        child.defers.push(Vec::new());
        if let Expr::Block { statements, tail } = child.body.expr_arena[body] {
            let terminated = child.lower_stmts(statements)?;
            if !terminated {
                let leaves = child.return_leaves(tail)?;
                child.run_top_frame()?;
                child.f.set_term(child.cur, Term::Return(leaves));
            }
        } else {
            let leaves = child.return_leaves(Some(body))?;
            child.f.set_term(child.cur, Term::Return(leaves));
        }
        child.defers.pop();

        self.f.nested_fns.push(child.f);
        Ok(())
    }

    fn lower_stmts(&mut self, stmts: IdxRange<Statement>) -> Result<bool, CompilerError> {
        for sidx in stmts {
            // Coarse anchor for anything lowered outside an expression (a `for`
            // desugaring's element bind, a scope-exit splice). `lower_expr`
            // narrows it wherever a real expression is in hand.
            self.cur_span = self.body.statement_spans.get(sidx).copied();
            match self.body.statement_arena[sidx].clone() {
                Statement::Let { name, value } => {
                    // Flat enums and guaranteed no-box values live in leaf cells,
                    // matching the scalar ABI instead of relying on later SROA.
                    let threaded_shape = self
                        .flat_enum_value_shape_of(value)
                        .or_else(|| self.nobox_binding_shape(name))
                        .or_else(|| self.nobox_value_shape_of(value));
                    if let Some(shape) = threaded_shape {
                        let r = self.lower_expr_repr(value)?;
                        let leaves = self.repr_to_leaves(&r, &shape);
                        let bname = self.body.binding_arena[name].name;
                        let cells: Vec<Idx<Cell>> = leaves
                            .into_iter()
                            .enumerate()
                            .map(|(li, lv)| {
                                let c = self.f.new_cell(Some(Ustr::from(format!("{bname}__{li}").as_str())));
                                self.emit_cur(Inst::Store { cell: c, value: lv });
                                c
                            })
                            .collect();
                        self.group_binds.insert(raw(name), (cells, shape));
                    } else {
                        let v = self.lower_expr(value)?;
                        let v = self.coerced_to_binding(v, value, name);
                        let cty = self.binding_ty(name);
                        let cell = self.f.new_cell_typed(Some(self.body.binding_arena[name].name), cty);
                        if self.binding_is_nonnull_value_struct(name) {
                            self.f.mark_nonnull_value_struct(cell);
                        }
                        self.emit_cur(Inst::Store { cell, value: v });
                        self.binds.insert(raw(name), cell);
                    }
                }
                Statement::Expr(e) => {
                    self.lower_stmt_expr(e)?;
                }
                Statement::Return(opt) => {
                    let leaves = self.return_leaves(opt)?;
                    // Run every pending defer (all enclosing block scopes, LIFO)
                    // before the actual return.
                    self.run_all_defers()?;
                    self.f.set_term(self.cur, Term::Return(leaves));
                    return Ok(true);
                }
                // Collected, not run inline; spliced at block exit / before return.
                // In a co fn, dying (kill/error) counts as exiting every open
                // scope: registration also sets a frame flag + builds a detached
                // terminal-cleanup copy (see `MirFunc::coro_defers`).
                Statement::Defer(body) => {
                    let flag = if self.resume_arg.is_some() {
                        let flag = self.f.new_cell(Some(Ustr::from("__defer")));
                        let on = self.emit_cur(Inst::Const(Lit::Bool(true)));
                        self.emit_cur(Inst::Store { cell: flag, value: on });
                        self.build_coro_defer_copy(body, flag)?;
                        Some(flag)
                    } else {
                        None
                    };
                    if let Some(frame) = self.defers.last_mut() {
                        frame.push((body, flag));
                    }
                }
                // `let (a, b) = e` — destructure via TupleIndex into a cell per
                // leaf (the same machinery `for (k,v)` uses).
                Statement::LetTuple { vars, value } => {
                    match self.expr_tuple_shape(value).filter(TupleRetShape::splats) {
                        Some(shape) => {
                            let r = self.lower_expr_repr(value)?;
                            self.bind_tuple_repr(&vars, &shape, r);
                        }
                        None => {
                            let v = self.lower_expr(value)?;
                            self.bind_for_pattern(&vars, v);
                        }
                    }
                }
                // `let Wrapper(x) = e` — a transparent newtype's runtime value IS
                // the inner, so `x` binds directly to the value.
                Statement::LetNewtype { var, value, .. } => {
                    let v = self.lower_expr(value)?;
                    let cell = self.f.new_cell(Some(self.body.binding_arena[var].name));
                    self.emit_cur(Inst::Store { cell, value: v });
                    self.binds.insert(raw(var), cell);
                }
                Statement::Static { name, value } => {
                    self.lower_static(name, value)?;
                }
                Statement::Fn { name, params, ret_ty, body } => {
                    self.lower_nested_fn(name, params, ret_ty, body)?;
                }
                Statement::Comment(_) => {}
                Statement::While { cond, body } => {
                    if self.lower_while(cond, body)? {
                        return Ok(true);
                    }
                }
                Statement::For { vars, iter, body } => {
                    self.lower_for(vars, iter, body)?;
                }
                Statement::WhileLet {
                    pattern,
                    value,
                    body,
                } => {
                    self.lower_while_let(pattern, value, body)?;
                }
                Statement::Break => {
                    let (_, brk) = self.loop_target("break")?;
                    self.used_break_targets.insert(brk);
                    self.f.set_term(self.cur, Term::Goto(brk));
                    return Ok(true);
                }
                Statement::Continue => {
                    let (cont, _) = self.loop_target("continue")?;
                    self.f.set_term(self.cur, Term::Goto(cont));
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Splice the current (innermost) block scope's pending `defer` bodies in
    /// LIFO order into the current block — the normal block-exit path.
    fn run_top_frame(&mut self) -> Result<(), CompilerError> {
        if let Some(frame) = self.defers.last() {
            for (body, flag) in frame.clone().into_iter().rev() {
                self.lower_defer_splice(body, flag)?;
            }
        }
        Ok(())
    }

    /// Splice EVERY pending defer (all enclosing block scopes, innermost first,
    /// each LIFO) — before a `return` that abandons those scopes.
    fn run_all_defers(&mut self) -> Result<(), CompilerError> {
        let frames: Vec<Vec<(Idx<Expr>, Option<Idx<Cell>>)>> = self.defers.iter().rev().cloned().collect();
        for frame in frames {
            for (body, flag) in frame.into_iter().rev() {
                self.lower_defer_splice(body, flag)?;
            }
        }
        Ok(())
    }

    /// One inline defer splice. With a co-fn terminal flag the splice is
    /// `if flag { flag = false; <body> }` — the SAME guarded shape as the
    /// detached kill copy, so kill-then-splice and splice-then-kill both run
    /// the defer exactly once (a body that kills its own handle mid-run
    /// otherwise double-fires on the trailing scope exit). Flagless (plain fn)
    /// defers splice unguarded.
    fn lower_defer_splice(&mut self, body: Idx<Expr>, flag: Option<Idx<Cell>>) -> Result<(), CompilerError> {
        if let Some(f) = flag {
            let b_body = self.f.new_block();
            let b_done = self.f.new_block();
            let fv = self.emit_cur(Inst::Load(f));
            self.f.set_term(self.cur, Term::Branch { cond: fv, then_bb: b_body, else_bb: b_done });
            self.cur = b_body;
            let off = self.emit_cur(Inst::Const(Lit::Bool(false)));
            self.emit_cur(Inst::Store { cell: f, value: off });
            let was = std::mem::replace(&mut self.in_defer_body, true);
            let res = self.lower_stmt_expr(body);
            self.in_defer_body = was;
            res?;
            self.f.set_term(self.cur, Term::Goto(b_done));
            self.cur = b_done;
        } else {
            let was = std::mem::replace(&mut self.in_defer_body, true);
            let res = self.lower_stmt_expr(body);
            self.in_defer_body = was;
            res?;
        }
        Ok(())
    }

    /// Build the detached terminal-cleanup copy of a co fn `defer` body:
    /// `if flag { flag = false; <body> }` ending in a bare return, in blocks
    /// UNREACHABLE from the fn entry (like closure bodies — the coroutine
    /// transform and DCE leave them alone). `emit_coroutine` compiles each as a
    /// standalone `<name>__defer<k>(s)` fn; the ctor stores them BY VALUE on
    /// the frame so a stale (stamp-killed) coroutine runs ITS OWN layout's
    /// cleanup. Lowered at the registration site with the same binds; the
    /// resolution maps are snapshotted so `let`s inside the copy can't leak
    /// into main-body lowering, and loop targets are cleared so a stray
    /// `break` can't jump into the main CFG.
    fn build_coro_defer_copy(&mut self, body: Idx<Expr>, flag: Idx<Cell>) -> Result<(), CompilerError> {
        let saved_cur = self.cur;
        let saved_binds = self.binds.clone();
        let saved_group = self.group_binds.clone();
        let saved_loops = std::mem::take(&mut self.loop_targets);

        let g_entry = self.f.new_block();
        let b_body = self.f.new_block();
        let b_done = self.f.new_block();
        self.cur = g_entry;
        let fv = self.emit_cur(Inst::Load(flag));
        self.f.set_term(g_entry, Term::Branch { cond: fv, then_bb: b_body, else_bb: b_done });
        self.cur = b_body;
        let off = self.emit_cur(Inst::Const(Lit::Bool(false)));
        self.emit_cur(Inst::Store { cell: flag, value: off });
        let was = std::mem::replace(&mut self.in_defer_body, true);
        let res = self.lower_stmt_expr(body);
        self.in_defer_body = was;
        res?;
        self.f.set_term(self.cur, Term::Goto(b_done));
        self.f.set_term(b_done, Term::Return(Vec::new()));
        self.f.coro_defers.push((flag, g_entry));

        self.cur = saved_cur;
        self.binds = saved_binds;
        self.group_binds = saved_group;
        self.loop_targets = saved_loops;
        Ok(())
    }

    fn lower_block_repr(
        &mut self,
        statements: comfy_la_arena::IdxRange<Statement>,
        tail: Option<Idx<Expr>>,
    ) -> Result<ValueRepr, CompilerError> {
        self.defers.push(Vec::new());
        let terminated = self.lower_stmts(statements)?;
        if terminated {
            self.defers.pop();
            return Ok(ValueRepr::Scalar(self.emit_cur(Inst::Const(Lit::Nil))));
        }
        let repr = match tail {
            Some(tail) => self.lower_expr_repr(tail)?,
            None => ValueRepr::Scalar(self.emit_cur(Inst::Const(Lit::Nil))),
        };
        self.run_top_frame()?;
        self.defers.pop();
        Ok(repr)
    }

    /// Lower an expression in statement position (value discarded). A tail-less
    /// block lowers to nothing (no dead unit); side-effecting tails still run.
    fn lower_stmt_expr(&mut self, e: Idx<Expr>) -> Result<(), CompilerError> {
        match self.body.expr_arena[e].clone() {
            Expr::Block { statements, tail } => {
                self.defers.push(Vec::new());
                let terminated = self.lower_stmts(statements)?;
                if !terminated {
                    if let Some(t) = tail {
                        // The tail is itself statement-position: a tail `if`/`match`
                        // skips its result cell too.
                        self.lower_stmt_expr(t)?;
                    }
                    self.run_top_frame()?;
                }
                self.defers.pop();
                Ok(())
            }
            // Control flow in statement position: lower WITHOUT a result cell (the
            // discarded if/match result + its store/load is the bulk of skull's
            // trace-hostile dead-cell soup). Arms recurse statement-position.
            Expr::If { cond, then_branch, else_branch } => {
                self.lower_if_stmt(cond, then_branch, else_branch)
            }
            Expr::Match { scrutinee, arms } => self.lower_match_stmt(scrutinee, arms),
            // Assignments are statements (unit value); lower the effect directly
            // so statement position leaves no dead unit.
            Expr::Assign { target, value } => self.lower_assign_effect(target, value),
            Expr::CompoundAssign {
                target,
                op_fn,
                value,
            } => self.lower_compound_assign_effect(e, target, op_fn, value),
            _ => {
                self.lower_expr(e)?;
                Ok(())
            }
        }
    }

    fn lower_expr(&mut self, e: Idx<Expr>) -> Result<Idx<Inst>, CompilerError> {
        // Source cursor for instruction-level diagnostics: whatever is lowered
        // from here down came from `e` unless a nested `lower_expr` narrows it,
        // and the restore puts the enclosing expression back on the way out.
        let outer = self.cur_span.replace(self.body.expr_spans[e]);
        let v = self.lower_expr_inner(e);
        self.cur_span = outer;
        let v = v?;
        // Implicit iterable->iterator coercion at a bounded call arg: bone
        // recorded the resolved `iter` fn; wrap the arg value in that call
        // (the call-arg analog of `lower_for_user_iter`'s `iter_fn`).
        if let Some(&(m, f)) = self.inf.and_then(|i| i.implicit_iter_args.get(&e)) {
            return Ok(self.emit_cur(Inst::Call {
                func: FuncRef { module: m, func: f },
                args: vec![v],
            }));
        }
        Ok(v)
    }

    fn lower_expr_inner(&mut self, e: Idx<Expr>) -> Result<Idx<Inst>, CompilerError> {
        match self.body.expr_arena[e].clone() {
            Expr::Unit | Expr::Null => Ok(self.emit_cur(Inst::Const(Lit::Nil))),
            Expr::Int(i) => Ok(self.emit_cur(Inst::Const(Lit::Int(i)))),
            Expr::Float(of) => Ok(self.emit_cur(Inst::Const(Lit::Float(of.0)))),
            Expr::Bool(x) => Ok(self.emit_cur(Inst::Const(Lit::Bool(x)))),
            Expr::String(s) => Ok(self.emit_cur(Inst::Const(Lit::Str(s)))),

            // A field-ref key path (`.field` resolved against `FieldRef<T, V>`)
            // lowers to its interned field name. skull emits named struct keys,
            // so `obj[name]` is identical to `obj.field`.
            Expr::SiblingRef(name) => Ok(self.emit_cur(Inst::Const(Lit::Str(name)))),

            // A scalarized value param read in a BOXED context reconstructs the
            // cdata (`pack` the rebuilt Group); a plain local is a cell load.
            Expr::Var(b) if self.group_binds.contains_key(&raw(b)) => {
                let r = self.lower_expr_repr(e)?;
                Ok(self.pack(r))
            }
            Expr::Var(b) if self.static_cells.contains_key(&raw(b)) => {
                Ok(self.emit_cur(Inst::Global(self.static_cells[&raw(b)])))
            }
            // A nested fn used as a value: reference the hoisted top-level fn by
            // name (a plain global read -- zero closure alloc).
            Expr::Var(b) if self.nested_fn_globals.contains_key(&raw(b)) => {
                Ok(self.emit_cur(Inst::Global(self.nested_fn_globals[&raw(b)].hoisted)))
            }
            // A whole-tuple use of a name bound to slot cells: rebuild and pack.
            // Packing a `nobox` element here is exactly how a whole-tuple escape
            // still reaches the guarantee's rejection.
            Expr::Var(b) if self.tuple_binds.contains_key(&raw(b)) => {
                let r = self.lower_expr_repr(e)?;
                Ok(self.pack(r))
            }
            Expr::Var(b) => {
                let cell = *self
                    .binds
                    .get(&raw(b))
                    .ok_or_else(|| gap("read of an unbound variable"))?;
                Ok(self.emit_cur(Inst::Load(cell)))
            }
            // A bare name resolving to a FUNCTION taken by value (`let f = adder`,
            // passing `adder` as an arg, a `func: adder` field) must reference the
            // fn's MANGLED emitted symbol -- every non-exempt fn mangles, so a bare
            // `Inst::Global(name)` would read a nil global. The JS backend is the
            // exception: it resolves fn-values through its own discovery keyed on
            // the SURFACE name (`discover_globals` -> `callee_js_name`), so it keeps
            // the bare name and mangles at its own emit boundary. A real global
            // variable keeps its bare name on every target.
            Expr::Global(name)
                if self.scx.target != bone_semantic::hir::FnBackend::Js
                    && self.ws.functions_by_name.contains_key(&name) =>
            {
                let sym = self.fn_value_symbol(e, name)?;
                Ok(self.emit_cur(Inst::Global(Ustr::from(sym.as_str()))))
            }
            // A global holding a value struct routes through the repr layer so a
            // whole-value read copies the box (a global is a mutable place).
            Expr::Global(name) => {
                if self.value_struct_of(e).is_some() || self.nullable_place_value_of(e).is_some() {
                    let r = self.lower_expr_repr(e)?;
                    return Ok(self.pack(r));
                }
                Ok(self.emit_cur(Inst::Global(name)))
            }

            // A reached `?` hole is a not-yet-filled marker: raise a readable
            // runtime error (naming the hole) rather than silently evaluating to
            // nil. `error` diverges, so the trailing nil is never read -- it only
            // satisfies the expression's value slot.
            Expr::Hole(name) => {
                let label = match name {
                    Some(n) => format!("reached unfilled hole ?{n}"),
                    None => "reached unfilled hole ?".to_string(),
                };
                let msg = self.emit_cur(Inst::Const(Lit::Str(Ustr::from(label.as_str()))));
                let func = self
                    .ws
                    .functions_by_name
                    .get(&Ustr::from("error"))
                    .and_then(|l| l.first())
                    .map(|&(module, func)| FuncRef { module, func })
                    .ok_or_else(|| gap("stdlib `error` missing for hole lowering"))?;
                self.emit_cur(Inst::Call { func, args: vec![msg] });
                Ok(self.emit_cur(Inst::Const(Lit::Nil)))
            }

            // Unary `!`/`~`/`-`: a struct operand is an operator overload (bone
            // records the target on `e`); a scalar uses the native op.
            Expr::Not(x) => {
                let v = self.lower_expr(x)?;
                match self.overload_target(e) {
                    Some(func) => Ok(self.emit_cur(Inst::Call { func, args: vec![v] })),
                    None => Ok(self.emit_cur(Inst::Not(v))),
                }
            }
            Expr::BitNot(x) => {
                let v = self.lower_expr(x)?;
                match self.overload_target(e) {
                    Some(func) => Ok(self.emit_cur(Inst::Call { func, args: vec![v] })),
                    None => Ok(self.emit_cur(Inst::BitNot(v))),
                }
            }
            Expr::UnaryMinus(x) => {
                let v = self.lower_expr(x)?;
                match self.overload_target(e) {
                    Some(func) => Ok(self.emit_cur(Inst::Call { func, args: vec![v] })),
                    None => Ok(self.emit_cur(Inst::Neg(v))),
                }
            }

            Expr::BinOp { op, lhs, rhs } => self.lower_binop(op, lhs, rhs),

            Expr::If {
                cond,
                then_branch,
                else_branch,
            } => self.lower_if(cond, then_branch, else_branch),

            // `break`/`continue` in expression position: emit a placeholder value
            // (never read — the block diverges) then a goto to the loop target.
            Expr::Break => {
                let (_, brk) = self.loop_target("break")?;
                self.used_break_targets.insert(brk);
                let v = self.emit_cur(Inst::Const(Lit::Nil));
                self.f.set_term(self.cur, Term::Goto(brk));
                Ok(v)
            }
            Expr::Continue => {
                let (cont, _) = self.loop_target("continue")?;
                let v = self.emit_cur(Inst::Const(Lit::Nil));
                self.f.set_term(self.cur, Term::Goto(cont));
                Ok(v)
            }

            Expr::Block { statements, tail } => {
                let repr = self.lower_block_repr(statements, tail)?;
                Ok(self.pack(repr))
            }

            Expr::Call { callee, args } => self.lower_call(e, callee, args),

            // --- aggregates + projections + container reads ---
            Expr::Tuple(elems) => {
                let vs = self.lower_exprs(elems)?;
                Ok(self.emit_cur(Inst::MakeTuple(vs)))
            }
            Expr::Array(elems) => self.lower_array_literal(e, elems),
            Expr::Index { base, index } => {
                // A whole-value read of a value-struct element COPIES (via the repr
                // layer's per-field Group -> `pack` rebuilds a fresh cdata) rather
                // than binding the interior array reference.
                //
                // Column storage routes on the CONTAINER, not on the element
                // type: inside a body specialized at `T = E?` the element type is
                // an unsubstituted generic (a nullable binding has no
                // synthesizable node), so the type predicates below see nothing
                // while the storage still has no element slot to name.
                if self.value_struct_of(e).is_some()
                    || self.nullable_place_value_of(e).is_some()
                    || self.vec_column_elem_of(base).is_some()
                {
                    let r = self.lower_expr_repr(e)?;
                    return Ok(self.pack(r));
                }
                let (b, kind) = self.lower_index_base(base)?;
                let i = self.lower_expr(index)?;
                Ok(self.emit_cur(Inst::Index {
                    base: b,
                    index: i,
                    kind,
                }))
            }
            // Route through the repr layer: a field read on a scalarized value
            // projects a leaf directly (no intermediate cdata); a boxed base
            // reads the field off the value as before.
            Expr::FieldAccess { .. } => {
                let r = self.lower_expr_repr(e)?;
                Ok(self.pack(r))
            }
            Expr::TupleIndex { tuple, .. } => {
                // `w.0` on a transparent newtype is identity -- its runtime value
                // IS the inner, not a positional table -- so `TupleIndex` (a
                // `[1]` read) would index a scalar and yield nil.
                if self.expr_is_newtype(tuple) {
                    return self.lower_expr(tuple);
                }
                // Otherwise through the repr layer, same as `FieldAccess` above:
                // a splatting base projects its part instead of packing a table
                // and reading a boxed element back out.
                let r = self.lower_expr_repr(e)?;
                Ok(self.pack(r))
            }
            // A value-struct literal is built via the repr layer (so a callsite
            // can splat it without a cdata); a plain struct is a Lua table.
            Expr::StructLiteral { name, fields, bases } if self.is_value_struct(name) => {
                let r = self.value_literal_repr(name, fields, bases)?;
                Ok(self.pack(r))
            }
            Expr::StructLiteral { name, fields, bases } => {
                self.lower_struct_literal(e, name, fields, bases)
            }
            Expr::EnumVariant {
                enum_name,
                variant,
                args,
                ..
            } => self.lower_enum_variant(enum_name, variant, args),

            Expr::Assign { target, value } => {
                self.lower_assign_effect(target, value)?;
                Ok(self.emit_cur(Inst::Const(Lit::Nil)))
            }
            Expr::CompoundAssign {
                target,
                op_fn,
                value,
            } => {
                self.lower_compound_assign_effect(e, target, op_fn, value)?;
                Ok(self.emit_cur(Inst::Const(Lit::Nil)))
            }

            Expr::InterpolatedString(parts) => self.lower_interp(parts),

            // `yield e` (or bare `yield`): end the block with `Term::Yield`, then
            // continue in the resume block; the expression evaluates to the
            // resume arg (the value of the next `resume(arg)`).
            Expr::Yield(opt) => {
                if self.in_defer_body {
                    return Err(gap("`yield` inside a `defer` body is not allowed"));
                }
                // A flat-enum yield value threads as leaves through the frame-slot
                // channel (the transform writes them to reused `s.__y*` slots, 0
                // alloc); a scalar/non-flat value stays a single packed value.
                let values: Vec<Idx<Inst>> = match opt {
                    Some(eb) => match self.flat_enum_value_shape_of(eb) {
                        Some(shape) => {
                            let r = self.lower_expr_repr(eb)?;
                            self.repr_to_leaves(&r, &shape)
                        }
                        None => vec![self.lower_expr(eb)?],
                    },
                    None => Vec::new(),
                };
                let resume = self.f.new_block();
                self.f.set_term(self.cur, Term::Yield { values, resume });
                self.cur = resume;
                let rac = self.resume_arg.ok_or_else(|| gap("yield outside a coroutine"))?;
                Ok(self.emit_cur(Inst::Load(rac)))
            }
            Expr::YieldDelegate(sub) => {
                if self.in_defer_body {
                    return Err(gap("`await` inside a `defer` body is not allowed"));
                }
                self.lower_yield_delegate(sub)
            }

            // A closure: lower its body into a sub-CFG in the shared arenas.
            // Captured outer vars are the enclosing cells (Lua upvalues); the
            // closure's params are fresh cells (its Lua arg list).
            Expr::Lambda { params, body } => {
                // A `nobox` param arrives as leaves, so the lambda binds a Group
                // over N cells instead of one -- the same shape a scalar-ABI fn
                // gives its params. Decided by the lambda's own fn TYPE, which is
                // what the call site reads too (`fn_value_abi`), so the two ends
                // agree without either seeing the other.
                let lam_abi = self.expr_fn_value_abi(e);
                // Param shapes come from the param BINDINGS, which is where a
                // lambda's param types actually land -- its own `fn(..) -> ..`
                // node can still hold unresolved vars at this point, while the
                // bindings carry what inference unified them WITH, i.e. the same
                // types the call site reads off the fn type.
                let param_shapes: Vec<Option<ValueShape>> = params
                    .into_iter()
                    .map(|pb| self.binding_ty(pb).and_then(|t| self.nobox_shape_of_ty(t)))
                    .collect();
                let mut param_cells: Vec<Idx<Cell>> = Vec::new();
                for (i, pb) in params.into_iter().enumerate() {
                    let pname = self.body.binding_arena[pb].name;
                    match param_shapes.get(i).and_then(|s| s.clone()) {
                        Some(shape) => {
                            let cells: Vec<Idx<Cell>> = (0..shape.leaf_count())
                                .map(|li| {
                                    self.f.new_cell(Some(Ustr::from(
                                        format!("{pname}__{li}").as_str(),
                                    )))
                                })
                                .collect();
                            param_cells.extend(cells.iter().copied());
                            self.group_binds.insert(raw(pb), (cells, shape));
                        }
                        None => {
                            let cell = self.f.new_cell(Some(pname));
                            self.binds.insert(raw(pb), cell);
                            param_cells.push(cell);
                        }
                    }
                }
                let entry = self.f.new_block();
                let saved = self.cur;
                self.cur = entry;
                // The lambda body returns via the LAMBDA's ABI, not the enclosing
                // fn's: a bone-desugared `() => e` body is a block with a `return e`
                // statement, so the return machinery (`return_leaves`) must see the
                // lambda's own return shape. A tuple-returning lambda splats N values
                // so an indirect call site can capture them (multi-return ABI); the
                // enclosing value-struct `ret_shape` never applies (a lambda is
                // called through a boxed fn-value). Save/restore around the body.
                let saved_ret_tuple = self.ret_tuple.take();
                let saved_ret_shape = self.ret_shape.take();
                // Isolate the defer state, like `lower_nested_fn`. A lambda body
                // is its OWN fn scope: bone desugars `() => e` to a block with
                // `return e`, so the body's return runs `run_all_defers` -- which
                // must see only the lambda's own pending defers, never the
                // enclosing fn's. Leaving the stack shared spliced every pending
                // enclosing defer into the lambda body (it then ran on every call)
                // and, when the defer body was what created the lambda, recursed
                // forever (run_all_defers re-splices the still-pending defer ->
                // re-lowers the lambda). `in_defer_body` resets to false: the
                // lambda body is not the defer body's code (a `yield`/`await`
                // there is already rejected by bone as yield-in-plain-lambda, so
                // the reset unmasks no diagnostic -- it just makes the flag honest).
                let saved_defers = std::mem::take(&mut self.defers);
                let saved_in_defer = std::mem::replace(&mut self.in_defer_body, false);
                // Box value args (see field docs). A lambda body IS spliced into
                // now -- `convert_closures` splits it out and the inliner runs
                // over the standalone body -- but that happens after lowering, so
                // this decision cannot see the specialized callee it would need.
                let saved_unspliced = std::mem::replace(&mut self.in_unspliced_ctx, true);
                self.ret_tuple = self.lambda_ret_tuple_shape(e);
                // A `nobox` return leaves as leaves, same multi-return wire the
                // tuple ABI uses and the same fn-TYPE key.
                self.ret_shape = lam_abi.as_ref().and_then(|(_, rs)| rs.clone());
                if let Some(shape) = self.ret_shape.clone() {
                    let r = self.lower_expr_repr(body)?;
                    if self.is_open() {
                        let leaves = self.repr_to_leaves(&r, &shape);
                        self.f.set_term(self.cur, Term::Return(leaves));
                    }
                    self.defers = saved_defers;
                    self.in_defer_body = saved_in_defer;
                    self.in_unspliced_ctx = saved_unspliced;
                    self.ret_tuple = saved_ret_tuple;
                    self.ret_shape = saved_ret_shape;
                    self.cur = saved;
                    return Ok(self.emit_cur(Inst::Lambda { params: param_cells, entry }));
                }
                match self.ret_tuple.clone() {
                    // A splatting tuple return needs the body as a repr, so a
                    // `nobox` element reaches the wire as leaves.
                    Some(shape) if shape.splats() => {
                        let r = self.lower_expr_repr(body)?;
                        if self.is_open() {
                            let leaves = self.tuple_repr_to_slots(r, &shape);
                            self.f.set_term(self.cur, Term::Return(leaves));
                        }
                    }
                    other => {
                        let v = self.lower_expr(body)?;
                        if self.is_open() {
                            let leaves = match other {
                                Some(shape) => self.tuple_return_operands(v, shape.slots.len()),
                                None => vec![v],
                            };
                            self.f.set_term(self.cur, Term::Return(leaves));
                        }
                    }
                }
                self.defers = saved_defers;
                self.in_defer_body = saved_in_defer;
                self.in_unspliced_ctx = saved_unspliced;
                self.ret_tuple = saved_ret_tuple;
                self.ret_shape = saved_ret_shape;
                self.cur = saved;
                Ok(self.emit_cur(Inst::Lambda { params: param_cells, entry }))
            }

            // `as` is identity at runtime EXCEPT `float as int`, which floors
            // (Lua numbers are all doubles) — mirrors rock-codegen.
            Expr::As { value, target } => {
                let v = self.lower_expr(value)?;
                let to_int = matches!(self.resolve_ty(target), Some(TypeExpr::Builtin(BuiltinTy::Int)));
                let from_float =
                    matches!(self.expr_ty(value), Some(TypeExpr::Builtin(BuiltinTy::Float)));
                if to_int && from_float {
                    Ok(self.emit_cur(Inst::FloatToInt(v)))
                } else {
                    Ok(v)
                }
            }

            // `x?.field` -> `if x == nil then nil else x.field`, joined through a
            // result cell (mirrors rock-codegen). A numeric field is a tuple read
            // (`x?.0`), a name is a struct/enum-record field.
            Expr::SafeNav { base, field } => {
                let bv = self.lower_expr(base)?;
                let nil = self.emit_cur(Inst::Const(Lit::Nil));
                let is_nil = self.emit_cur(Inst::Eq(bv, nil));
                let result = self.f.new_cell(None);
                let nil_bb = self.f.new_block();
                let val_bb = self.f.new_block();
                let join = self.f.new_block();
                self.f.set_term(self.cur, Term::Branch { cond: is_nil, then_bb: nil_bb, else_bb: val_bb });

                self.cur = nil_bb;
                let n = self.emit_cur(Inst::Const(Lit::Nil));
                self.store_and_join(result, n, join);

                self.cur = val_bb;
                let fv = match field.as_str().parse::<usize>() {
                    // `x?.0` on a transparent newtype is identity (see TupleIndex).
                    Ok(_) if self.expr_is_newtype(base) => bv,
                    Ok(index) => self.emit_cur(Inst::TupleIndex { base: bv, index }),
                    Err(_) => match self.swizzle_of(base, field) {
                        // `v?.yx`: expand on the non-null arm, packed for the join cell.
                        Some(idxs) => {
                            let g = self.swizzle_from_packed(bv, &idxs);
                            self.pack(g)
                        }
                        // An embed/nobox field has NO slot under its own name --
                        // its leaves live on the parent under dotted keys -- so
                        // the read rebuilds from them, exactly as the non-safe-nav
                        // paths do (`physical_field` at the place-base read,
                        // `embed_field_of` at the repr read). Deciding storage
                        // BEFORE emitting is the whole point: an `Inst::Field` here
                        // reads a key that does not exist, and for a NULLABLE child
                        // reads its presence tag, which the next link then indexes.
                        None if self.embed_field_of(base, field).is_some() => {
                            let cname =
                                self.embed_field_of(base, field).expect("guard matched");
                            // Compose through `physical_field`, exactly as the
                            // place-base read does -- NOT off `bv`. When the base
                            // is itself an embed read, `bv` is a re-materialized
                            // box whose fields are ordinary keys, so reading the
                            // dotted leaves off it finds nothing; composing walks
                            // up to the real physical parent instead
                            // (`T.child?.inner` -> `T["child.inner"]`).
                            let (pbase, key) = self.physical_field(base, field);
                            let parent =
                                if pbase == base { bv } else { self.lower_expr(pbase)? };
                            if self.embed_read_is_nullable(base, field) {
                                self.materialize_embed_nullable(parent, key, cname)
                            } else {
                                self.materialize_embed(parent, key, cname)
                            }
                        }
                        // `o?.p` reads a value field whole out of a mutable place, so
                        // it copies exactly as `o.p` does. The base is non-nil on
                        // this arm; the FIELD may still be absent, so a nullable one
                        // takes the presence-aware copy.
                        None => {
                            let fv = self.emit_cur(Inst::Field { base: bv, field });
                            match self
                                .value_struct_name_of(e)
                                .filter(|_| self.is_mutable_place(base))
                            {
                                Some(name) if self.embed_read_is_nullable(base, field) => {
                                    self.nullable_boxed_value_copy(fv, name)
                                }
                                Some(name) => {
                                    let copy = self.boxed_value_copy_repr(fv, name);
                                    self.pack(copy)
                                }
                                None => fv,
                            }
                        }
                    },
                };
                self.store_and_join(result, fv, join);

                self.cur = join;
                Ok(self.emit_cur(Inst::Load(result)))
            }

            Expr::Match { scrutinee, arms } => self.lower_match(scrutinee, arms),
            Expr::IfLet {
                pattern,
                value,
                then_branch,
                else_branch,
            } => self.lower_if_let(pattern, value, then_branch, else_branch),

            Expr::IncDec { target, decrement, postfix } => {
                self.lower_incdec(target, decrement, postfix)
            }

            // A range used as a VALUE (not a `for` iterable — `lower_for`
            // intercepts that) materializes to the stdlib Vec-shaped `__range`.
            Expr::Range { low, high } => {
                let lo = self.lower_expr(low)?;
                let hi = self.lower_expr(high)?;
                let func = self.resolve_global_fn("__range")?;
                Ok(self.emit_cur(Inst::Call { func, args: vec![lo, hi] }))
            }

            // `return e` in expression position (`x ?? return`, `cond ? a : return`).
            // Diverges: run defers, set the Return terminator. The block is now
            // closed, so callers skip any fall-through (via `is_open()`); the
            // placeholder value is never used.
            Expr::Return(opt) => {
                let leaves = self.return_leaves(opt)?;
                self.run_all_defers()?;
                self.f.set_term(self.cur, Term::Return(leaves));
                Ok(self.emit_cur(Inst::Const(Lit::Nil)))
            }

            Expr::SafeNavCall { receiver, method, args } => {
                self.lower_safe_nav_call(e, receiver, method, args)
            }
            Expr::SafeNavLet { receiver, binding, body } => {
                self.lower_safe_nav_let(receiver, binding, body)
            }

            other => Err(gap(format!("expression form not in Phase 1: {}", expr_tag(&other)))),
        }
    }

    /// `target = value` -> a `Store`/`StoreField`/`StoreIndex`/`StoreGlobal`
    /// (the effect; no value). The expression form wraps this with a unit value.
    /// The BASE of an assignment target, lowered as a PLACE: the slot itself,
    /// never a packed copy of it. `lower_expr` on a value-struct element/field is
    /// an RVALUE read and rebuilds a fresh box (the whole-value copy rule), so
    /// using it here would land the store on a temporary. The syntactic position
    /// ("I am an assignment target's base") is known here, so nothing needs to be
    /// plumbed down from bone. Recurses the whole chain: `a.b[i].c.x = 5` must not
    /// pack at any level.
    fn lower_place_base(&mut self, e: Idx<Expr>) -> Result<Idx<Inst>, CompilerError> {
        match self.body.expr_arena[e].clone() {
            Expr::Index { base, index } => {
                let (b, kind) = self.lower_index_base(base)?;
                let i = self.lower_expr(index)?;
                Ok(self.emit_cur(Inst::Index { base: b, index: i, kind }))
            }
            // A `nobox` global has no single slot -- `Inst::Global(name)` would
            // read a nil global and the write would land on nothing. Every
            // reachable place form (whole group, leaf field) is handled before
            // this; reject rather than emit that silent nil.
            Expr::Global(name) if self.nobox_global_place(e).is_some() => {
                Err(CompilerError::new_with_label(
                    self.body.expr_spans[e],
                    format!(
                        "`{name}` is a `nobox` global, which is stored as leaves and has no \
                         slot this assignment target can write through"
                    ),
                ))
            }
            Expr::Global(name) => Ok(self.emit_cur(Inst::Global(name))),
            // A swizzle and a flattened-enum field are not places (both
            // materialize a fresh value); leave them to the rvalue path, which
            // rejects or copies as it does today.
            Expr::FieldAccess { base, field }
                if self.swizzle_of(base, field).is_none()
                    && self.flat_field_of(base, field).is_none() =>
            {
                let (pbase, key) = self.physical_field(base, field);
                let b = self.lower_place_base(pbase)?;
                Ok(self.emit_cur(Inst::Field { base: b, field: key }))
            }
            // A call returning a VALUE is not storage: the rvalue lowering packs a
            // fresh box, so the store lands on a temporary and emits
            // `(Pt_ct(a, b)).x = v` -- not valid Lua, so the whole chunk fails to
            // parse at load. bone rejects the reachable form (`1dd209b65`), so
            // this is unreachable by construction; it rejects rather than falls
            // back so a future admission change surfaces here instead of as an
            // unparseable chunk.
            //
            // A call returning a REFERENCE struct IS storage -- it hands back the
            // shared table, so the write is real. `o.next.unwrap().value = v` is
            // the live shape (`tests/lambda_stress_tests.rock`).
            Expr::Call { .. } | Expr::SafeNavCall { .. } if self.expr_is_value_struct(e) => {
                Err(CompilerError::new_with_label(
                    self.body.expr_spans[e],
                    "assignment target's base is a `value` call result, which is not storage: \
                     the write would land on a temporary and be discarded"
                        .to_string(),
                ))
            }
            _ => self.lower_expr(e),
        }
    }

    fn lower_assign_effect(
        &mut self,
        target: Idx<Expr>,
        value: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        match self.body.expr_arena[target].clone() {
            // A threaded (group_binds) var -- a scalarized flat-enum local or value
            // param -- updates its leaf cells, not one boxed cell.
            Expr::Var(b) if self.group_binds.contains_key(&raw(b)) => {
                let (cells, shape) = self.group_binds.get(&raw(b)).cloned().unwrap();
                let r = self.lower_expr_repr(value)?;
                let leaves = self.repr_to_leaves(&r, &shape);
                for (c, lv) in cells.iter().zip(leaves) {
                    self.emit_cur(Inst::Store { cell: *c, value: lv });
                }
            }
            Expr::Var(b) if self.static_cells.contains_key(&raw(b)) => {
                let name = self.static_cells[&raw(b)];
                let v = self.lower_expr(value)?;
                self.emit_cur(Inst::StoreGlobal { name, value: v });
            }
            Expr::Var(b) => {
                let cell = *self
                    .binds
                    .get(&raw(b))
                    .ok_or_else(|| gap("assignment to an unbound variable"))?;
                let v = self.lower_expr(value)?;
                let v = self.coerced_to_binding(v, value, b);
                self.emit_cur(Inst::Store { cell, value: v });
            }
            // A leaf of a `nobox` global (`SPOT.x = v`, `SPOT.a.b = v`): the
            // group has no slot to take a place base of, so the write lands in
            // the leaf global itself.
            Expr::FieldAccess { .. } if self.nobox_global_place(target).is_some() => {
                let (leaves, shape) =
                    self.nobox_global_place(target).expect("guard matched");
                self.nobox_global_store(&leaves, shape, value)?;
            }
            Expr::FieldAccess { base, field } => {
                // `xs[i].x = v` on a column-stored Vec: there is no element slot
                // to take a place base of, so the write lands directly in the
                // leaf's own column.
                if let Some((vec, idx, elem)) = self.column_index_place(base)? {
                    return self.column_leaf_store(vec, idx, elem, field, value);
                }
                // `buf[i].pos = v` / `buf[i].id = v` / `buf[i].lo.n = v` on a
                // leaf-addressed NativeBuffer: there is no element slot to take a
                // place base of, so the write lands in the leaves under this
                // field's path. A whole nested field spans several leaves; a
                // primitive field is exactly one.
                if let Some((buf, idx, elem, path)) = self.nb_leaf_field_place(base, field)? {
                    return self.nb_leaf_field_store(buf, idx, elem, &path, value);
                }
                // A whole embedded field (`p.f = Foo { .. }`): the child has no
                // slot of its own, so write its leaves. Composition happens
                // BEFORE this, so `p.f` here is the child, not a physical key.
                if let Some(cname) = self.embed_field_of(base, field) {
                    let b = self.lower_place_base(base)?;
                    self.store_embed(b, base, field, cname, value)?;
                    return Ok(());
                }
                // An embed LEAF write composes to one dotted key on the parent
                // that holds it (`h.p.x = 9` -> `h["p.x"] = 9`).
                let (pbase, key) = self.physical_field(base, field);
                let b = self.lower_place_base(pbase)?;
                // A flattened enum field: write the discriminant + payload slots
                // directly on the parent table (no nested box).
                if let Some(shape) = self.flat_field_of(base, field) {
                    let cells = self.lower_flat_value(&shape, value)?;
                    self.store_flat_cells(b, key, cells);
                    return Ok(());
                }
                let v = self.lower_expr(value)?;
                self.emit_cur(Inst::StoreField {
                    base: b,
                    field: key,
                    value: v,
                });
            }
            Expr::Index { base, index } => {
                let (b, kind) = self.lower_index_base(base)?;
                let i = self.lower_expr(index)?;
                // Column storage has no element slot: the write goes leaf-wise
                // into K columns, so the value is consumed as a repr and never
                // boxed. Mirrors `column_element_repr` on the read side.
                if let ContainerKind::VecValueColumns(elem) = kind {
                    return self.column_element_store(b, i, value, elem);
                }
                // A column-stored Map has no slot at the key either: the write
                // goes through the spine to a slot, then leaf-wise into `data`.
                if let Some(elem) = self.map_column_value_of(base) {
                    return self.map_column_store(b, i, value, elem);
                }
                if let ContainerKind::NativeBufferValue(elem) = kind {
                    if self.is_leaf_addressed(elem) {
                        return self.nb_leaf_element_store(b, i, value, elem);
                    }
                }
                let v = self.lower_expr(value)?;
                self.emit_cur(Inst::StoreIndex {
                    base: b,
                    index: i,
                    value: v,
                    kind,
                });
            }
            Expr::Global(_) if self.nobox_global_place(target).is_some() => {
                let (leaves, shape) =
                    self.nobox_global_place(target).expect("guard matched");
                self.nobox_global_store(&leaves, shape, value)?;
            }
            Expr::Global(name) => {
                let v = self.lower_expr(value)?;
                self.emit_cur(Inst::StoreGlobal { name, value: v });
            }
            // `base?.field = value` — silently skip if any `?.` link is null (the
            // receiver `base` lowers to nil then). `value` evaluates ONLY on the
            // non-null path. Deep `a?.b?.c = v` works: `base` (`a?.b`) is itself
            // a SafeNav that lowers to nil if any inner link is null.
            Expr::SafeNav { base, field } => {
                let recv = self.lower_place_base(base)?;
                let nil = self.emit_cur(Inst::Const(Lit::Nil));
                let is_nil = self.emit_cur(Inst::Eq(recv, nil));
                let store_bb = self.f.new_block();
                let join = self.f.new_block();
                self.f.set_term(self.cur, Term::Branch { cond: is_nil, then_bb: join, else_bb: store_bb });
                self.cur = store_bb;
                let v = self.lower_expr(value)?;
                self.emit_cur(Inst::StoreField { base: recv, field, value: v });
                self.f.set_term(self.cur, Term::Goto(join));
                self.cur = join;
            }
            _ => return Err(gap("assignment to this target form (later)")),
        }
        Ok(())
    }

    /// `target <op>= value` == `target = target <op> value` (the effect), over
    /// var / global / field / index targets. The place is read and written
    /// through a SINGLE base/index evaluation (no double side effects). A scalar
    /// target uses native arith; a non-scalar target (struct/value with an
    /// operator overload) routes `op_fn(target, value)` through the resolved
    /// overload (bone records it on the CompoundAssign expr).
    fn lower_compound_assign_effect(
        &mut self,
        e: Idx<Expr>,
        target: Idx<Expr>,
        op_fn: Ustr,
        value: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        let scalar = self.arith_ty(target).zip(self.arith_ty(value));
        // Non-scalar: resolve the operator overload once (overload target on the
        // expr, else the named op fn).
        let overload = if scalar.is_none() {
            Some(match self.overload_target(e) {
                Some(f) => f,
                None => self.resolve_funcref(e, op_fn)?,
            })
        } else {
            None
        };
        match self.body.expr_arena[target].clone() {
            Expr::Var(b) if self.static_cells.contains_key(&raw(b)) => {
                let name = self.static_cells[&raw(b)];
                let cur = self.emit_cur(Inst::Global(name));
                let rv = self.lower_expr(value)?;
                let new_v = self.compound_combine(scalar, op_fn, overload, cur, rv)?;
                self.emit_cur(Inst::StoreGlobal { name, value: new_v });
            }
            Expr::Var(b) => {
                let cell = *self
                    .binds
                    .get(&raw(b))
                    .ok_or_else(|| gap("compound-assign to an unbound variable"))?;
                let cur = self.emit_cur(Inst::Load(cell));
                let rv = self.lower_expr(value)?;
                let new_v = self.compound_combine(scalar, op_fn, overload, cur, rv)?;
                self.emit_cur(Inst::Store { cell, value: new_v });
            }
            // A `nobox` global is stored as leaves, so neither the whole-group
            // nor the leaf form has a slot the arms below can read-modify-write
            // through. bone desugars `t <op>= v` to `t = t <op> v` before skull
            // sees it, so this is unreachable by construction (proved by
            // `error`-ing the arm over the full surface: zero fires). It rejects
            // rather than falling through so a future routing change surfaces
            // here instead of as a silent read of a nil global.
            _ if self.nobox_global_place(target).is_some() => {
                return Err(gap("compound assignment reached a `nobox` global place"))
            }
            Expr::Global(name) => {
                let cur = self.emit_cur(Inst::Global(name));
                let rv = self.lower_expr(value)?;
                let new_v = self.compound_combine(scalar, op_fn, overload, cur, rv)?;
                self.emit_cur(Inst::StoreGlobal { name, value: new_v });
            }
            Expr::FieldAccess { base, field } => {
                let (pbase, key) = self.physical_field(base, field);
                let b = self.lower_place_base(pbase)?;
                let cur = self.emit_cur(Inst::Field { base: b, field: key });
                let rv = self.lower_expr(value)?;
                let new_v = self.compound_combine(scalar, op_fn, overload, cur, rv)?;
                self.emit_cur(Inst::StoreField { base: b, field: key, value: new_v });
            }
            Expr::Index { base, index } => {
                let (b, kind) = self.lower_index_base(base)?;
                let i = self.lower_expr(index)?;
                let cur = self.emit_cur(Inst::Index { base: b, index: i, kind });
                let rv = self.lower_expr(value)?;
                let new_v = self.compound_combine(scalar, op_fn, overload, cur, rv)?;
                self.emit_cur(Inst::StoreIndex { base: b, index: i, value: new_v, kind });
            }
            // `base?.field <op>= value` — skip silently if any `?.` link is null.
            // The read-modify-write and `value` run only on the non-null path.
            Expr::SafeNav { base, field } => {
                let recv = self.lower_place_base(base)?;
                let nil = self.emit_cur(Inst::Const(Lit::Nil));
                let is_nil = self.emit_cur(Inst::Eq(recv, nil));
                let store_bb = self.f.new_block();
                let join = self.f.new_block();
                self.f.set_term(self.cur, Term::Branch { cond: is_nil, then_bb: join, else_bb: store_bb });
                self.cur = store_bb;
                let cur = self.emit_cur(Inst::Field { base: recv, field });
                let rv = self.lower_expr(value)?;
                let new_v = self.compound_combine(scalar, op_fn, overload, cur, rv)?;
                self.emit_cur(Inst::StoreField { base: recv, field, value: new_v });
                self.f.set_term(self.cur, Term::Goto(join));
                self.cur = join;
            }
            _ => return Err(gap("compound-assign to this target form (later)")),
        }
        Ok(())
    }

    /// `cur <op> rv`: native arith for a scalar target, else the resolved
    /// operator-overload call `op_fn(cur, rv)`.
    fn compound_combine(
        &mut self,
        scalar: Option<(ArithTy, ArithTy)>,
        op_fn: Ustr,
        overload: Option<FuncRef>,
        cur: Idx<Inst>,
        rv: Idx<Inst>,
    ) -> Result<Idx<Inst>, CompilerError> {
        match scalar {
            Some((lt, rt)) => {
                let inst = arith_inst(op_fn.as_str(), lt, rt, cur, rv)?;
                Ok(self.emit_cur(inst))
            }
            None => Ok(self.emit_cur(Inst::Call {
                func: overload.expect("non-scalar compound-assign resolved an overload"),
                args: vec![cur, rv],
            })),
        }
    }

    /// `target++` / `target--` (prefix or postfix). Reads the target, adds +/-1
    /// (typed: `IntAdd`/`FloatAdd` per the target's scalar type), writes it back,
    /// and yields the OLD value (postfix) or NEW value (prefix). Var / global /
    /// field / index targets, mirroring assignment.
    fn lower_incdec(
        &mut self,
        target: Idx<Expr>,
        decrement: bool,
        postfix: bool,
    ) -> Result<Idx<Inst>, CompilerError> {
        let ty = self
            .arith_ty(target)
            .filter(|t| matches!(t, ArithTy::Int | ArithTy::Float))
            .ok_or_else(|| gap("inc/dec of a non-int/float target"))?;
        let bump = |s: &mut Self, old: Idx<Inst>| -> Idx<Inst> {
            let delta = match (ty, decrement) {
                (ArithTy::Float, false) => Inst::Const(Lit::Float(1.0)),
                (ArithTy::Float, true) => Inst::Const(Lit::Float(-1.0)),
                (_, false) => Inst::Const(Lit::Int(1)),
                (_, true) => Inst::Const(Lit::Int(-1)),
            };
            let d = s.emit_cur(delta);
            let inst = if ty == ArithTy::Float {
                Inst::FloatAdd(old, d)
            } else {
                Inst::IntAdd(old, d)
            };
            s.emit_cur(inst)
        };
        match self.body.expr_arena[target].clone() {
            Expr::Var(b) if self.static_cells.contains_key(&raw(b)) => {
                let name = self.static_cells[&raw(b)];
                let old = self.emit_cur(Inst::Global(name));
                let new = bump(self, old);
                self.emit_cur(Inst::StoreGlobal { name, value: new });
                Ok(if postfix { old } else { new })
            }
            Expr::Var(b) => {
                let cell = *self
                    .binds
                    .get(&raw(b))
                    .ok_or_else(|| gap("inc/dec of an unbound variable"))?;
                let old = self.emit_cur(Inst::Load(cell));
                let new = bump(self, old);
                self.emit_cur(Inst::Store { cell, value: new });
                Ok(if postfix { old } else { new })
            }
            // A primitive leaf of a `nobox` global (`SPOT.x++`) is its own global.
            // `arith_ty` already restricted the target to int/float, so a whole
            // group can never reach here.
            _ if matches!(self.nobox_global_place(target), Some((_, None))) => {
                let (leaves, _) = self.nobox_global_place(target).expect("guard matched");
                let name = leaves[0];
                let old = self.emit_cur(Inst::Global(name));
                let new = bump(self, old);
                self.emit_cur(Inst::StoreGlobal { name, value: new });
                Ok(if postfix { old } else { new })
            }
            Expr::Global(name) => {
                let old = self.emit_cur(Inst::Global(name));
                let new = bump(self, old);
                self.emit_cur(Inst::StoreGlobal { name, value: new });
                Ok(if postfix { old } else { new })
            }
            Expr::FieldAccess { base, field } => {
                let (pbase, key) = self.physical_field(base, field);
                let b = self.lower_place_base(pbase)?;
                let old = self.emit_cur(Inst::Field { base: b, field: key });
                let new = bump(self, old);
                self.emit_cur(Inst::StoreField { base: b, field: key, value: new });
                Ok(if postfix { old } else { new })
            }
            Expr::Index { base, index } => {
                let (b, kind) = self.lower_index_base(base)?;
                let i = self.lower_expr(index)?;
                let old = self.emit_cur(Inst::Index { base: b, index: i, kind });
                let new = bump(self, old);
                self.emit_cur(Inst::StoreIndex { base: b, index: i, value: new, kind });
                Ok(if postfix { old } else { new })
            }
            _ => Err(gap("inc/dec of a non-assignable target")),
        }
    }

    /// `recv?.method(args)` — `nil` if `recv` is nil, else the UFCS call
    /// `method(recv, args)` (cdata-everywhere: no scalar splat). Args evaluate
    /// only on the non-null path. Result flows through a cell.
    fn lower_safe_nav_call(
        &mut self,
        e: Idx<Expr>,
        receiver: Idx<Expr>,
        method: Ustr,
        args: IdxRange<Expr>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let recv = self.lower_expr(receiver)?;
        let nil = self.emit_cur(Inst::Const(Lit::Nil));
        let is_nil = self.emit_cur(Inst::Eq(recv, nil));
        let result = self.f.new_cell(None);
        let nil_bb = self.f.new_block();
        let call_bb = self.f.new_block();
        let join = self.f.new_block();
        self.f.set_term(self.cur, Term::Branch { cond: is_nil, then_bb: nil_bb, else_bb: call_bb });

        self.cur = nil_bb;
        let n = self.emit_cur(Inst::Const(Lit::Nil));
        self.store_and_join(result, n, join);

        self.cur = call_bb;
        let cv = if self.struct_field_is_fn(receiver, method) {
            // `recv?.field(args)` on a fn-typed FIELD: nil-guarded indirect call
            // `(recv.field)(args)` — the field IS the callee, no receiver arg.
            let fieldv = self.emit_cur(Inst::Field { base: recv, field: method });
            let mut argv = Vec::new();
            for a in args {
                argv.push(self.lower_expr(a)?);
            }
            let shape = self.peeled_tuple_shape(e);
            self.emit_call_value_n(shape, fieldv, argv)
        } else {
            // `recv?.method(args)` UFCS: `method(recv, args)`, recv as arg 0.
            let func = self.resolve_funcref(e, method)?;
            let arg_idxs: Vec<Idx<Expr>> = args.into_iter().collect();
            if let Some(abi) = fn_scalar_abi(self.ws, self.scx, func) {
                // Scalar-ABI callee: splat the value receiver + value args into
                // leaves (a value return packs to the single safe-nav result).
                self.scalar_ufcs_call(func, recv, &arg_idxs, &abi)?
            } else {
                let mut argv = vec![recv];
                for a in arg_idxs {
                    argv.push(self.lower_expr(a)?);
                }
                // The instantiation key travels through `?.` like it does through a
                // plain call: a column-stored element cannot be served by the shared
                // generic body, which reads a `_data` this storage does not have.
                let key = self
                    .call_spec(e, func)
                    .filter(|(k, _)| {
                        !self.in_unspliced_ctx || spec_key_has_column_elem(self.ws, k)
                    })
                    .map(|(k, _)| k);
                // A tuple-returning method splats N values; capture + pack into
                // the single (boxed, nullable) safe-nav result.
                let ret_ty = self.ws.module(func.module).functions[func.func].return_ty;
                match ret_ty.and_then(|t| tuple_ret_shape(self.ws, self.spec_subst, t)) {
                    Some(shape) => {
                        let r = self.emit_tuple_call_tree(func, argv, &shape, None, key);
                        self.pack(r)
                    }
                    None => {
                        let call = self.emit_cur(Inst::Call { func, args: argv });
                        if let Some(k) = key {
                            self.f.spec_bindings.insert(call, k);
                        }
                        call
                    }
                }
            }
        };
        self.store_and_join(result, cv, join);

        self.cur = join;
        Ok(self.emit_cur(Inst::Load(result)))
    }

    /// `recv?.let { b -> body }` — `nil` if `recv` is nil, else `body` with `b`
    /// bound to the (non-nil) receiver. Result flows through a cell.
    fn lower_safe_nav_let(
        &mut self,
        receiver: Idx<Expr>,
        binding: Idx<bone_semantic::hir::BindingInfo>,
        body: Idx<Expr>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let recv = self.lower_expr(receiver)?;
        let nil = self.emit_cur(Inst::Const(Lit::Nil));
        let is_nil = self.emit_cur(Inst::Eq(recv, nil));
        let result = self.f.new_cell(None);
        let nil_bb = self.f.new_block();
        let let_bb = self.f.new_block();
        let join = self.f.new_block();
        self.f.set_term(self.cur, Term::Branch { cond: is_nil, then_bb: nil_bb, else_bb: let_bb });

        self.cur = nil_bb;
        let n = self.emit_cur(Inst::Const(Lit::Nil));
        self.store_and_join(result, n, join);

        self.cur = let_bb;
        let cell = self.f.new_cell(Some(self.body.binding_arena[binding].name));
        self.emit_cur(Inst::Store { cell, value: recv });
        self.binds.insert(raw(binding), cell);
        let bv = self.lower_expr(body)?;
        self.store_and_join(result, bv, join);

        self.cur = join;
        Ok(self.emit_cur(Inst::Load(result)))
    }

    /// A `[T]` literal whose element stores as columns lowers every element to
    /// its leaves, including elements supplied through a spread.
    fn column_vec_literal(
        &mut self,
        elems: &[Idx<Expr>],
        elem: ColumnElem,
    ) -> Result<Idx<Inst>, CompilerError> {
        let shape = self.column_shape(elem)?;
        let paths = column_leaf_paths(&shape);
        let mut columns: Vec<(Ustr, Vec<Idx<Inst>>)> =
            paths.iter().map(|p| (*p, Vec::with_capacity(elems.len()))).collect();
        for &e in elems {
            let repr = self.lower_expr_repr(e)?;
            let leaves = self.repr_to_leaves(&repr, &shape);
            if leaves.len() != paths.len() {
                return Err(gap(format!(
                    "column literal arity mismatch for [{}]: {} leaves vs {} columns",
                    elem.name,
                    leaves.len(),
                    paths.len()
                )));
            }
            for (col, leaf) in columns.iter_mut().zip(leaves) {
                col.1.push(leaf);
            }
        }
        let capacity = self.emit_cur(Inst::Const(Lit::Int(elems.len() as i64)));
        Ok(self.emit_cur(Inst::MakeVecColumns {
            elem: elem.name,
            columns,
            len: elems.len(),
            capacity,
        }))
    }

    fn lower_array_literal(
        &mut self,
        lit: Idx<Expr>,
        elems: IdxRange<Expr>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let idxs: Vec<Idx<Expr>> = elems.into_iter().collect();
        let has_spread = idxs
            .iter()
            .any(|&e| matches!(self.body.expr_arena[e], Expr::Spread(_)));
        if !has_spread {
            // Column storage: transpose the literal's elements into one array
            // per leaf. Each element lowers as a repr, so no element box is
            // built even for a literal with entries.
            if let Some(elem) = self.vec_column_elem_of(lit) {
                return self.column_vec_literal(&idxs, elem);
            }
            let vs: Vec<Idx<Inst>> = idxs.iter().map(|&e| self.lower_expr(e)).collect::<Result<_, _>>()?;
            return Ok(self.emit_cur(Inst::MakeVec(vs)));
        }
        if let Some(elem) = self.vec_column_elem_of(lit) {
            let capacity = self.emit_cur(Inst::Const(Lit::Int(0)));
            let arr = self.empty_column_vec(elem, capacity)?;
            for &e in &idxs {
                if let Expr::Spread(inner) = self.body.expr_arena[e] {
                    self.spread_into(arr, inner, SpreadDest::Columns(elem))?;
                } else {
                    let repr = self.lower_expr_repr(e)?;
                    self.column_append_repr(arr, &repr, elem)?;
                }
            }
            return Ok(arr);
        }
        let arr = self.emit_cur(Inst::MakeVec(vec![]));
        for &e in &idxs {
            if let Expr::Spread(inner) = self.body.expr_arena[e] {
                self.spread_into(arr, inner, SpreadDest::Container(ContainerKind::Vec))?;
            } else {
                let v = self.lower_expr(e)?;
                self.emit_cur(Inst::ArrayPush { arr, value: v, kind: ContainerKind::Vec });
            }
        }
        Ok(arr)
    }

    fn append_spread_repr(
        &mut self,
        arr: Idx<Inst>,
        repr: ValueRepr,
        dest: SpreadDest,
    ) -> Result<(), CompilerError> {
        match dest {
            SpreadDest::Container(kind) => {
                let value = self.pack(repr);
                self.emit_cur(Inst::ArrayPush { arr, value, kind });
                Ok(())
            }
            SpreadDest::Columns(elem) => self.column_append_repr(arr, &repr, elem),
        }
    }

    /// Splice `inner` into `arr`: a tuple by static arity, a struct by declared
    /// fields, or an array/Vec through one runtime loop. Only the destination
    /// append differs between ordinary and column storage.
    fn spread_into(
        &mut self,
        arr: Idx<Inst>,
        inner: Idx<Expr>,
        dest: SpreadDest,
    ) -> Result<(), CompilerError> {
        if let (SpreadDest::Columns(elem), Expr::Tuple(elements)) =
            (dest, self.body.expr_arena[inner])
        {
            for element in elements {
                let repr = self.lower_expr_repr(element)?;
                self.column_append_repr(arr, &repr, elem)?;
            }
            return Ok(());
        }
        let v = self.lower_expr(inner)?;
        match self.expr_ty(inner) {
            Some(TypeExpr::Tuple { elements }) => {
                for i in 0..elements.into_iter().count() {
                    let value = self.emit_cur(Inst::TupleIndex { base: v, index: i });
                    self.append_spread_repr(arr, ValueRepr::Scalar(value), dest)?;
                }
                Ok(())
            }
            Some(TypeExpr::App { ctor: TypeCtor::Array | TypeCtor::Map, .. }) => {
                self.spread_array_loop(arr, inner, v, dest)
            }
            Some(TypeExpr::App { ctor: TypeCtor::Nominal(name), .. }) => {
                let fields = self
                    .struct_field_names(name)
                    .ok_or_else(|| gap("spread of a non-struct nominal type"))?;
                for field in fields {
                    let value = self.emit_cur(Inst::Field { base: v, field });
                    self.append_spread_repr(arr, ValueRepr::Scalar(value), dest)?;
                }
                Ok(())
            }
            _ => Err(gap("spread of an unsupported type")),
        }
    }

    /// Runtime copy of every element of `src` (read per its `coll_kind`) into
    /// `arr` through the destination's append operation.
    fn spread_array_loop(
        &mut self,
        arr: Idx<Inst>,
        src_expr: Idx<Expr>,
        src: Idx<Inst>,
        dest: SpreadDest,
    ) -> Result<(), CompilerError> {
        let src_kind = self.coll_kind(src_expr);
        let n = self.emit_cur(Inst::Len { base: src, kind: src_kind });
        let i_cell = self.f.new_cell(None);
        let zero = self.emit_cur(Inst::Const(Lit::Int(0)));
        self.emit_cur(Inst::Store { cell: i_cell, value: zero });

        let header = self.f.new_block();
        let body = self.f.new_block();
        let exit = self.f.new_block();
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = header;
        let i = self.emit_cur(Inst::Load(i_cell));
        let cmp = self.emit_cur(Inst::IntLt(i, n));
        self.f.set_term(self.cur, Term::Branch { cond: cmp, then_bb: body, else_bb: exit });

        self.cur = body;
        let i_now = self.emit_cur(Inst::Load(i_cell));
        let repr = self.loop_element_repr_at(src, i_now, src_kind)?;
        self.append_spread_repr(arr, repr, dest)?;
        let i2 = self.emit_cur(Inst::Load(i_cell));
        let one = self.emit_cur(Inst::Const(Lit::Int(1)));
        let inc = self.emit_cur(Inst::IntAdd(i2, one));
        self.emit_cur(Inst::Store { cell: i_cell, value: inc });
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = exit;
        Ok(())
    }

    /// Lower a call to a (boxed) single value — the scalar-ABI Group result is
    /// `pack`ed to a cdata. ABI-boundary callers use [`Self::lower_call_repr`].
    fn lower_call(
        &mut self,
        e: Idx<Expr>,
        callee: Callee,
        args: IdxRange<Expr>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let r = self.lower_call_repr(e, callee, args)?;
        Ok(self.pack(r))
    }

    /// Lower a call to a [`ValueRepr`]. A scalar-ABI value-returning direct call
    /// yields an unboxed `Group` (so a consuming ABI boundary splats it without a
    /// round-trip through cdata); everything else is a `Scalar`.
    fn lower_call_repr(
        &mut self,
        e: Idx<Expr>,
        callee: Callee,
        args: IdxRange<Expr>,
    ) -> Result<ValueRepr, CompilerError> {
        let arg_idxs: Vec<Idx<Expr>> = args.into_iter().collect();
        if let Callee::Unresolved { name, .. } = callee {
            if matches!(name.as_str(), "inspect" | "inspect_formatted") {
                if let Some(&arg) = arg_idxs.first() {
                    if let Some(elem) = self.vec_column_elem_of(arg) {
                        if self.is_nobox_struct(elem.name) {
                            return Err(CompilerError::new_with_label(
                                self.body.expr_spans[e],
                                format!(
                                    "cannot inspect a column-stored nobox value `{}`: \
                                     inspection requires implicit element materialization",
                                    elem.name
                                ),
                            ));
                        }
                    }
                }
            }
        }
        // `cast<T>(v)` is identity at runtime (`return value`), so a `nobox`
        // RESULT threads the argument's repr instead of packing it for the boxed
        // `any` param and field-reading the leaves back off that box. This is
        // what lets a whole COLUMN element read widen to `T?`: every stdlib
        // entry point that widens one spells it `cast(__vec_data_get(..))`, and
        // `__vec_data_get` already lowers to a Group of column reads -- the box
        // was minted only to cross `cast`. Same identity treatment the newtype
        // ctor and the bound `iter` call below already get.
        // Keyed on the RESOLVED callee, never the spelling: a user fn named
        // `cast` returning a nobox value is a different function, and threading
        // its argument through would delete the call and emit a leaf read of
        // whatever was passed in (pinned by tests/nobox_cast_identity.rock).
        if let Callee::Unresolved { name, .. } = callee {
            if name.as_str() == "cast" && self.nobox_value_name(e).is_some() {
                if let [inner] = arg_idxs[..] {
                    if self.resolves_to_inline_builtin(e, name) {
                        return self.lower_expr_repr(inner);
                    }
                }
            }
        }
        // Scalar arithmetic: rock desugars `+ - * / %` to `__op_*` calls; turn
        // them back into native typed ops when both operands are scalars.
        // (Non-scalar `__op_*` falls through to a real overload Call below.)
        if let Callee::Unresolved { name, .. } = callee {
            let n = name.as_str();
            if matches!(
                n,
                "__op_add" | "__op_sub" | "__op_mul" | "__op_div" | "__op_mod"
            ) {
                if let [a, b] = arg_idxs[..] {
                    if let (Some(lt), Some(rt)) = (self.arith_ty(a), self.arith_ty(b)) {
                        return Ok(ValueRepr::Scalar(self.lower_native_arith(n, lt, rt, a, b)?));
                    }
                }
            }
        }
        // `native_buffer_alloc<T>(n)` intrinsic: allocate a flat ffi array of the
        // concrete element type and wrap it in a `NativeBuffer`. Resolving the
        // ctype here (not in a generic inline(lua) body) is the whole point -- it
        // gives value-struct-typed NativeBuffers a boilerplate-free ctor. Non-value
        // / GC / opaque-generic elements reject spanned.
        if let Callee::Unresolved { name, .. } = callee {
            if name.as_str() == "native_buffer_alloc" {
                let [n_arg] = arg_idxs[..] else {
                    return Err(gap("`native_buffer_alloc` takes exactly one argument (count)"));
                };
                let span = self.body.expr_spans[e];
                let ctype = self.native_buffer_alloc_ctype(e, span)?;
                let count = self.lower_expr(n_arg)?;
                let ptr = self.emit_cur(Inst::FfiNewArray { ctype, count });
                let mut fields =
                    vec![(Ustr::from("ptr"), ptr), (Ustr::from("len"), count)];
                fields.extend(self.leaf_view_fields(ctype, ptr));
                let nb = self.emit_cur(Inst::MakeStruct {
                    ty: Some(Ustr::from("NativeBuffer")),
                    fields,
                });
                return Ok(ValueRepr::Scalar(nb));
            }
        }
        // `xs.push(v)` / `vec_push(xs, v)` onto a COLUMN-stored Vec. The stdlib
        // push bodies write one element into a single `_data` backing, which
        // column storage does not have -- so the call is replaced here, where
        // the receiver's element type is concrete. (Inside `Vec<T>.push` itself
        // the element is an opaque `T`, so the kind is undecidable there.)
        if let Callee::Unresolved { name, .. } = callee {
            if matches!(name.as_str(), "push" | "vec_push") {
                if let [recv, item] = arg_idxs[..] {
                    if let Some(elem) = self.vec_column_elem_of(recv) {
                        return self.column_push(recv, item, elem);
                    }
                }
            }
            // The three raw-backing primitives ARE the representation boundary.
            // Nearly every stdlib Vec helper -- `get`, `set`, `pop`, `clear`,
            // `insert_at`, `remove_at`, `swap_remove` -- reaches storage only
            // through these, so making them column-aware here converts that whole
            // surface at once instead of intercepting each helper by name.
            //
            // This works inside a GENERIC body because `vec_column_elem_of`
            // resolves through the active `SpecSubst`: in the shared snapshot of
            // `Vec<T>.clear` the element is an opaque `T` and the boxed `_data`
            // body stands, while the snapshot specialized at `T = Vec2` sees the
            // concrete element and lowers to column operations. Static, per
            // instantiation -- never a runtime test for whether `_data` exists.
            if let Some(r) = self.column_data_primitive(name.as_str(), &arg_idxs)? {
                return Ok(r);
            }
            // `vec_new()` is the fourth boundary primitive, and the odd one out:
            // its element type comes from the CALL's own type, not a receiver, so
            // it cannot be read off an argument. It is also `inline(lua)`, so it
            // never specializes (`compute_call_spec` bails on an inline body) --
            // the opaque body would hand back a `_data` table that every later
            // column access then indexes as nil.
            if name.as_str() == "__vec_data_alloc" && arg_idxs.len() == 1 {
                if let Some(elem) = self.vec_column_elem_of(e) {
                    let capacity = self.lower_expr(arg_idxs[0])?;
                    return Ok(ValueRepr::Scalar(self.empty_column_vec(elem, capacity)?));
                }
            }
            if name.as_str() == "vec_new" && arg_idxs.is_empty() {
                if let Some(elem) = self.vec_column_elem_of(e) {
                    let capacity = self.emit_cur(Inst::Const(Lit::Int(0)));
                    return Ok(ValueRepr::Scalar(self.empty_column_vec(elem, capacity)?));
                }
            }
            // The Map surface. `get`/`table_unset` are the storage boundary the
            // generic Rock bodies reach through -- `remove`, `contains`,
            // `get_or_insert`, `update` and `upsert` are all built on them, so
            // converting these converts that surface at once. `len`/`clear` are
            // intercepted despite having working generic bodies: the generic
            // `clear` unsets while iterating the very table it walks, and `len`
            // counts a spine it can read in O(1).
            if let Some(r) = self.map_column_primitive(name.as_str(), &arg_idxs)? {
                return Ok(r);
            }
            // `map_new()` is the Map's `vec_new`: its value type comes from the
            // CALL's own type, so it cannot be read off an argument.
            if name.as_str() == "map_new" && arg_idxs.is_empty() {
                if let Some(elem) = self.map_column_value_of(e) {
                    return Ok(ValueRepr::Scalar(self.empty_column_map(elem)?));
                }
            }
            if name.as_str() == "__rock_vec_from_lua_array" {
                if let ([raw, len], Some(elem)) = (&arg_idxs[..], self.vec_column_elem_of(e)) {
                    let raw = self.lower_expr(*raw)?;
                    let len = self.lower_expr(*len)?;
                    return Ok(ValueRepr::Scalar(
                        self.column_vec_from_raw_array(raw, len, elem)?,
                    ));
                }
            }
        }
        // `on_killed(f)` / `on_resume(f)` inside a `co fn` register a lifecycle
        // hook. bone desugars `on_killed { body }` to a call of an `on_killed`
        // stub (which errors at runtime) with a `() => body` lambda. We capture
        // it as `Inst::CoroHook` so the coroutine transform relocates the lambda
        // to the constructor (a kill before the first resume must still fire).
        // Outside a co fn (`resume_arg` unset) it falls through to the stub.
        if self.resume_arg.is_some() {
            if let Callee::Unresolved { name, .. } = callee {
                let kind = match name.as_str() {
                    "on_killed" => Some(CoroHookKind::OnKilled),
                    "on_resume" => Some(CoroHookKind::OnResume),
                    "on_hot_reload" => Some(CoroHookKind::OnHotReload),
                    _ => None,
                };
                if let Some(kind) = kind {
                    if let [arg] = arg_idxs[..] {
                        // The hook is peeled to the constructor, which can only
                        // re-render a LAMBDA LITERAL with the frame context -- any
                        // other fn-typed expression (a named fn, a fn in a
                        // variable) would silently install no hook. Reject.
                        if !matches!(self.body.expr_arena[arg], Expr::Lambda { .. }) {
                            return Err(gap(format!(
                                "`{name}` argument must be a lambda literal (`() => ...`) -- the hook is relocated to construction time"
                            )));
                        }
                        let lambda = self.lower_expr(arg)?;
                        self.emit_cur(Inst::CoroHook { kind, lambda });
                        return Ok(ValueRepr::Scalar(self.emit_cur(Inst::Const(Lit::Nil))));
                    }
                }
            }
        }
        // A concrete `c.resume()` / `c.resume(arg)` whose yield type is a flat enum:
        // thread the value through the frame-slot channel instead of the type-erased
        // generic `resume` return (which always boxes). `c.__advance(c, arg)` steps
        // the coroutine (writing the leaves to reused `__y*` slots on the object,
        // which IS the frame), then we read them back as an unboxed group -- no
        // per-yield allocation.
        if let Callee::Unresolved { name, .. } = callee {
            if name.as_str() == "resume"
                && !arg_idxs.is_empty()
                && self.is_coroutine_recv(arg_idxs[0])
            {
                if let Some(shape) = self.flat_enum_shape_of_expr(e) {
                    let recv = self.lower_expr(arg_idxs[0])?;
                    let advance = self.emit_cur(Inst::Field { base: recv, field: Ustr::from("__advance") });
                    let mut cargs: Vec<Idx<Inst>> = vec![recv];
                    if let Some(&a) = arg_idxs.get(1) {
                        cargs.push(self.lower_expr(a)?);
                    }
                    self.emit_cur(Inst::CallValue { callee: advance, args: cargs });
                    return Ok(self.resume_frame_read(recv, &shape));
                }
            }
        }
        // Bound-calls inside an `Iterator`-bounded body (`next(x)` / `iter(x)`
        // where `x: I`): `iter` is IDENTITY (the arg is the iterator, no call);
        // `next` is a placeholder [`Inst::CallBoundMulti`] -- the concrete
        // callee is substituted per splice from the site's `BoundTree`.
        if let Some(inf) = self.inf {
            if inf.bound_identity_calls.contains(&e) {
                let [arg] = arg_idxs[..] else {
                    return Err(gap("bound `iter` call with non-unary args"));
                };
                return Ok(ValueRepr::Scalar(self.lower_expr(arg)?));
            }
            if let Some(&gidx) = inf.bound_calls.get(&e) {
                let [arg] = arg_idxs[..] else {
                    return Err(gap("bound `next` call with non-unary args"));
                };
                let arg_v = self.lower_expr(arg)?;
                // Bound conformance fixes the shape at exactly `(bool, E)`.
                let dests: Vec<Idx<Cell>> = (0..2).map(|_| self.f.new_cell(None)).collect();
                self.emit_cur(Inst::CallBoundMulti {
                    dests: dests.clone(),
                    generic_idx: gidx as u32,
                    args: vec![arg_v],
                });
                let loads: Vec<Idx<Inst>> =
                    dests.iter().map(|c| self.emit_cur(Inst::Load(*c))).collect();
                return Ok(ValueRepr::Scalar(self.emit_cur(Inst::MakeTuple(loads))));
            }
        }
        // A spread arg fills param slots by field name, so it takes the
        // slot-reordering path just like a named arg.
        let any_named = arg_idxs
            .iter()
            .any(|a| self.body.arg_names.contains_key(a) || self.body.spread_args.contains(a));
        match callee {
            // A transparent newtype constructor (`UserId(5)`) is identity at
            // runtime -- the wrapped value IS the inner -- so it lowers to the
            // single argument value, no call.
            Callee::Unresolved { name, .. } if self.ws.newtypes_by_name.contains_key(&name) => {
                match arg_idxs.as_slice() {
                    [inner] => Ok(ValueRepr::Scalar(self.lower_expr(*inner)?)),
                    _ => Err(gap(format!("newtype ctor `{name}` with non-unary args"))),
                }
            }
            // A statically-named function -> a `FuncRef` (mangling is codegen's
            // job, not baked into the IR). On resolution failure, recover an
            // INDIRECT call: a fn-typed struct field via UFCS (`recv.f(a)` ==
            // `f(recv, a)`, no real fn `f`) or a fn-typed global (`g(a)`).
            Callee::Unresolved { name, .. } => match self.resolve_funcref(e, name) {
                // A `@struct_expand` fn (docs/shaders.md) is replaced
                // by its literal pairs list -- recognized through the intrinsic
                // registry off the resolved FuncRef, so no user fn is hijacked by
                // name and the common non-intrinsic call pays only one field read.
                Ok(func) if matches!(self.fn_expand_kind(func), Some(ExpandKind::StructPairs)) => {
                    self.expand_struct_pairs(e, func, &arg_idxs)
                }
                Ok(func) if matches!(self.fn_expand_kind(func), Some(ExpandKind::Materialize)) => {
                    self.expand_materialize(&arg_idxs)
                }
                Ok(func)
                    if matches!(self.fn_expand_kind(func), Some(ExpandKind::ShaderPipeline)) =>
                {
                    self.expand_shader_pipeline(e, &arg_idxs)
                }
                Ok(func) => self.lower_direct_call_repr(e, func, &arg_idxs, any_named),
                Err(gap_err) => {
                    if !any_named {
                        let arg_vals: Vec<Idx<Inst>> =
                            arg_idxs.iter().map(|&a| self.lower_expr(a)).collect::<Result<_, _>>()?;
                        // `recv.field(args)` where `field` is a fn-typed field:
                        // call through `(recv.field)(args)` (recv is UFCS arg 0).
                        if let Some(&recv_expr) = arg_idxs.first() {
                            if self.struct_field_is_fn(recv_expr, name) {
                                let fieldv = self.emit_cur(Inst::Field { base: arg_vals[0], field: name });
                                let v = self.emit_call_value(e, fieldv, arg_vals[1..].to_vec());
                                return Ok(ValueRepr::Scalar(v));
                            }
                        }
                        // `g(args)` where `g` is a fn-typed module global.
                        if self.is_fn_global(name) {
                            let g = self.emit_cur(Inst::Global(name));
                            let v = self.emit_call_value(e, g, arg_vals);
                            return Ok(ValueRepr::Scalar(v));
                        }
                    }
                    Err(gap_err)
                }
            },
            // A fn-typed local: load the cell and call through the value.
            Callee::Local(b) => {
                // A nested fn: call the hoisted top-level fn by name (no closure).
                // Under its scalar ABI (`nested_fn_abi`) value args splat to leaves
                // and a value return arrives as leaves via `CallValueMulti`.
                if let Some(abi) = self.nested_fn_globals.get(&raw(b)).cloned() {
                    let g = self.emit_cur(Inst::Global(abi.hoisted));
                    let arg_vals =
                        self.slotted_call_args(&abi.param_names, &abi.param_shapes, &arg_idxs)?;
                    let Some(shape) = abi.ret_shape else {
                        let v = self.emit_call_value(e, g, arg_vals);
                        return Ok(ValueRepr::Scalar(v));
                    };
                    let dests: Vec<Idx<Cell>> =
                        (0..shape.leaf_count()).map(|_| self.f.new_cell(None)).collect();
                    self.emit_cur(Inst::CallValueMulti { dests: dests.clone(), callee: g, args: arg_vals });
                    let leaves: Vec<Idx<Inst>> =
                        dests.iter().map(|c| self.emit_cur(Inst::Load(*c))).collect();
                    let mut idx = 0;
                    return Ok(self.rebuild_from_leaves(&shape, &leaves, &mut idx));
                }
                let cell = *self
                    .binds
                    .get(&raw(b))
                    .ok_or_else(|| gap("call to an unbound fn-typed local"))?;
                let callee_v = self.emit_cur(Inst::Load(cell));
                if let Some(abi) = self.binding_ty(b).and_then(|t| self.fn_value_abi(t)) {
                    return self.indirect_call_by_abi(abi, callee_v, &arg_idxs);
                }
                // Nothing `nobox` in the signature -- args are boxed.
                let arg_vals: Vec<Idx<Inst>> =
                    arg_idxs.iter().map(|&a| self.lower_expr(a)).collect::<Result<_, _>>()?;
                let v = self.emit_call_value(e, callee_v, arg_vals);
                Ok(ValueRepr::Scalar(v))
            }
            // A fn-typed expression result: lower it, call through it.
            Callee::Value(ve) => {
                let abi = self.expr_fn_value_abi(ve);
                let callee_v = self.lower_expr(ve)?;
                if let Some(abi) = abi {
                    return self.indirect_call_by_abi(abi, callee_v, &arg_idxs);
                }
                let arg_vals: Vec<Idx<Inst>> =
                    arg_idxs.iter().map(|&a| self.lower_expr(a)).collect::<Result<_, _>>()?;
                let v = self.emit_call_value(e, callee_v, arg_vals);
                Ok(ValueRepr::Scalar(v))
            }
        }
    }

    /// Lower a direct (statically-resolved) call. When the callee uses the scalar
    /// value ABI, value args splat to leaves and a value return arrives as N
    /// leaves via [`Inst::CallMulti`] (rebuilt into a `Group`); otherwise it is a
    /// plain boxed [`Inst::Call`].
    ///
    /// A call of an `Iterator`-bounded callee carries bone's resolution tree
    /// (keyed by the call expr `e`); it is re-recorded against the emitted call
    /// inst so the inliner can substitute the placeholders inside the callee.
    /// The specialization decision for a call `e` to `func`: `Some((key, subst))`
    /// iff `func` is a splice-able generic `@inline`/`@pure` callee whose per-site
    /// bindings (`call_generic_bindings[e]`) include at least one VALUE-struct
    /// leaf. A primitive-only / non-leaf instantiation, or a non-inline generic,
    /// keeps the shared generic snapshot (byte-identical). Eligibility is a subset
    /// of the inliner's `classify` (not extern / lua / coroutine) so a specialized
    /// splice always resolves; a residual case falls to the site-aware rejection.
    fn call_spec(&self, e: Idx<Expr>, func: FuncRef) -> Option<(SpecKey, SpecSubst)> {
        compute_call_spec(self.ws, self.inf?, e, func, self.spec_subst)
    }

    /// [`Self::call_spec`] for the for-protocol `next` callee, whose bindings
    /// come from bone's `next_generics` rather than a call expr. Ordered into
    /// the callee's generic declaration order, which is what [`SpecKey`] is.
    fn next_call_spec(
        &self,
        func: FuncRef,
        next_generics: &rustc_hash::FxHashMap<bone_semantic::hir::VarId, TyId>,
    ) -> Option<SpecKey> {
        let inf = self.inf?;
        spec_key_from_next_generics(self.ws, inf, func, next_generics, self.spec_subst)
    }

    /// Lower a direct (statically-resolved) call. When the callee uses the scalar
    /// value ABI, value args splat to leaves and a value return arrives as N
    /// leaves via [`Inst::CallMulti`] (rebuilt into a `Group`); otherwise it is a
    /// plain boxed [`Inst::Call`].
    ///
    /// A call of an `Iterator`-bounded callee carries bone's resolution tree
    /// (keyed by the call expr `e`); it is re-recorded against the emitted call
    /// inst so the inliner can substitute the placeholders inside the callee.
    fn lower_direct_call_repr(
        &mut self,
        e: Idx<Expr>,
        func: FuncRef,
        arg_idxs: &[Idx<Expr>],
        any_named: bool,
    ) -> Result<ValueRepr, CompilerError> {
        let tree = self
            .inf
            .and_then(|i| i.bound_call_trees.get(&e))
            .filter(|t| !t.is_empty())
            .cloned();
        // A value-instantiated `@inline` generic call takes the specialized scalar
        // ABI: value args splat, the return arrives as leaves, and the emitted call
        // is tagged so the inliner splices the matching specialized snapshot. Empty
        // `subst` = the shared (unspecialized) path, byte-identical. In an unspliced
        // context the splice never fires, so the splatted call would reach the
        // SHARED generic body and drop leaves -- keep the shared boxed ABI there.
        // In an unspliced context an ABI-only specialization is dropped: no splice
        // will fire, and the splatted call would reach the SHARED generic body and
        // drop every leaf but the first, so the boxed ABI is kept to match it.
        //
        // A column-eligible instantiation is NOT dropped, because the shared body
        // is wrong rather than merely unspecialized, and because a splice is no
        // longer the only thing that can serve it: the call targets a standalone
        // specialized copy, lowered under this same substitution, so the two agree
        // on the ABI without an inliner ever running.
        let spec = self
            .call_spec(e, func)
            .filter(|(k, _)| !self.in_unspliced_ctx || spec_key_has_column_elem(self.ws, k));
        let empty = SpecSubst::new();
        let subst = spec.as_ref().map(|(_, s)| s).unwrap_or(&empty);
        // The instantiation key is recorded on the emitted call inst whenever this
        // is a specialized call -- even on the boxed (non-scalar-ABI) path, because
        // the specialized body still differs (e.g. a `native_buffer_alloc<T>` ctype
        // resolves) though the param/return ABI is unchanged.
        let key = spec.as_ref().map(|(k, _)| k.clone());
        let ret_tuple = self.ws.module(func.module).functions[func.func]
            .return_ty
            .and_then(|t| tuple_ret_shape(self.ws, subst, t));
        if let Some(shape) = &ret_tuple {
            self.reject_nobox_opaque_tuple_return(e, func, shape)?;
        } else {
            self.reject_nobox_opaque_value_return(e, func)?;
        }
        let callee = &self.ws.module(func.module).functions[func.func];
        // Resolve phantom before tuple flattening can project from its nil placeholder.
        if arg_idxs.is_empty()
            && callee.inline_body.is_some_and(|(_, body)| body.trim() == "return nil")
        {
            return Ok(ValueRepr::Scalar(self.emit_cur(Inst::Const(Lit::Nil))));
        }
        let scalar_abi = fn_scalar_abi_spec(self.ws, self.scx, func, subst);
        let Some(scalar_abi) = scalar_abi else {
            self.reject_nobox_boxed_call_args(func, arg_idxs)?;
            let args = if any_named {
                self.reorder_named_args(arg_idxs, func)?
            } else {
                arg_idxs
                    .iter()
                    .enumerate()
                    .map(|(i, &a)| {
                        let v = self.lower_expr(a)?;
                        Ok(match self.param_ty(func, i) {
                            Some(t) => self.coerced(v, a, t),
                            None => v,
                        })
                    })
                    .collect::<Result<_, CompilerError>>()?
            };
            // Above the tuple early return: the exemption is a property of the
            // CALLEE, so making it arity-dependent would be a guarantee hole the
            // day a `@gpu_only` intrinsic returns a tuple.
            self.authorize_gpu_boundary_boxes(&func, &args);
            if let Some(shape) = &ret_tuple {
                return Ok(self.emit_tuple_call_tree(func, args, shape, tree, key));
            }
            let call = self.emit_cur(Inst::Call { func, args });
            if let Some(t) = tree {
                self.f.bound_trees.insert(call, t);
            }
            if let Some(k) = key {
                self.f.spec_bindings.insert(call, k);
            }
            return Ok(ValueRepr::Scalar(call));
        };
        let args = self.scalar_call_args(
            func,
            arg_idxs,
            any_named,
            &scalar_abi.param_shapes,
        )?;
        let ret_shape = scalar_abi.ret_shape;
        let key = spec.map(|(k, _)| k);
        match ret_shape {
            Some(shape) => {
                let n = shape.leaf_count();
                let dests: Vec<Idx<Cell>> = (0..n).map(|_| self.f.new_cell(None)).collect();
                let call = self.emit_cur(Inst::CallMulti { dests: dests.clone(), func, args });
                if let Some(t) = tree {
                    self.f.bound_trees.insert(call, t);
                }
                if let Some(k) = key {
                    self.f.spec_bindings.insert(call, k);
                }
                let leaves: Vec<Idx<Inst>> =
                    dests.iter().map(|c| self.emit_cur(Inst::Load(*c))).collect();
                let mut idx = 0;
                Ok(self.rebuild_from_leaves(&shape, &leaves, &mut idx))
            }
            None => match &ret_tuple {
                Some(shape) => Ok(self.emit_tuple_call_tree(func, args, shape, tree, key)),
                None => {
                    let call = self.emit_cur(Inst::Call { func, args });
                    if let Some(t) = tree {
                        self.f.bound_trees.insert(call, t);
                    }
                    if let Some(k) = key {
                        self.f.spec_bindings.insert(call, k);
                    }
                    Ok(ValueRepr::Scalar(call))
                }
            },
        }
    }

    /// A direct call to a tuple-returning fn under the multi-return ABI: the
    /// callee returns its slots, captured into that many dest cells via
    /// [`Inst::CallMulti`]. The result value is `MakeTuple(loads)` -- a boxed use
    /// materializes the table (same as the old ABI), a destructure elides it
    /// (`bind_for_pattern`). When the shape splats, the result is instead a
    /// `Tuple` repr whose splatting elements are Groups rebuilt from their
    /// leaves, so a `nobox` element never becomes a box on the way to a bind.
    ///
    /// `tree` records an `Iterator`-bound resolution against the emitted call
    /// inst (chain roots like a directly-called bounded `next`).
    ///
    /// A tuple return does not exempt the call from its instantiation `key`:
    /// under column storage the shared generic body reads a `_data` the
    /// instantiation does not have, so the specialized copy is the only correct
    /// target.
    fn emit_tuple_call_tree(
        &mut self,
        func: FuncRef,
        args: Vec<Idx<Inst>>,
        shape: &TupleRetShape,
        tree: Option<bone_semantic::prelude::BoundTree>,
        key: Option<SpecKey>,
    ) -> ValueRepr {
        let dests: Vec<Idx<Cell>> =
            (0..shape.slot_count()).map(|_| self.f.new_cell(None)).collect();
        let call = self.emit_cur(Inst::CallMulti { dests: dests.clone(), func, args });
        if let Some(t) = tree {
            self.f.bound_trees.insert(call, t);
        }
        if let Some(k) = key {
            self.f.spec_bindings.insert(call, k);
        }
        let loads: Vec<Idx<Inst>> = dests.iter().map(|c| self.emit_cur(Inst::Load(*c))).collect();
        self.tuple_result(shape, &loads)
    }

    /// The call-site result for a captured tuple return: the plain
    /// `MakeTuple(slots)` when nothing splats, else the `Tuple` repr rebuilt
    /// from the slots.
    fn tuple_result(&mut self, shape: &TupleRetShape, slots: &[Idx<Inst>]) -> ValueRepr {
        if !shape.splats() {
            return ValueRepr::Scalar(self.emit_cur(Inst::MakeTuple(slots.to_vec())));
        }
        let mut idx = 0;
        self.tuple_from_slots(shape, slots, &mut idx)
    }

    /// An indirect call whose fn TYPE carries `nobox` values: value args splat to
    /// their leaves and a value return arrives as leaves through
    /// `CallValueMulti`, matching what the lambda/nested fn on the other end
    /// emitted from the SAME type. The callee is opaque here by construction --
    /// that is the whole point of keying on the type ([`Self::fn_value_abi`]).
    fn indirect_call_by_abi(
        &mut self,
        (param_shapes, ret_shape): (Vec<Option<ValueShape>>, Option<ValueShape>),
        callee: Idx<Inst>,
        arg_idxs: &[Idx<Expr>],
    ) -> Result<ValueRepr, CompilerError> {
        let mut argv = Vec::new();
        for (i, &a) in arg_idxs.iter().enumerate() {
            match param_shapes.get(i).and_then(|s| s.clone()) {
                Some(shape) => {
                    let r = self.lower_expr_repr(a)?;
                    argv.extend(self.repr_to_leaves(&r, &shape));
                }
                None => argv.push(self.lower_expr(a)?),
            }
        }
        let Some(shape) = ret_shape else {
            return Ok(ValueRepr::Scalar(self.emit_cur(Inst::CallValue { callee, args: argv })));
        };
        let dests: Vec<Idx<Cell>> =
            (0..shape.leaf_count()).map(|_| self.f.new_cell(None)).collect();
        self.emit_cur(Inst::CallValueMulti { dests: dests.clone(), callee, args: argv });
        let leaves: Vec<Idx<Inst>> =
            dests.iter().map(|c| self.emit_cur(Inst::Load(*c))).collect();
        let mut idx = 0;
        Ok(self.rebuild_from_leaves(&shape, &leaves, &mut idx))
    }

    /// An indirect call `callee(args)`. If the call's result type is a tuple, the
    /// fn-value (a lambda or named fn used as a value) returns N values under the
    /// multi-return ABI, captured via [`Inst::CallValueMulti`] -- a multi-assign
    /// `d0, d1 = callee(args)` (a plain `CallValue` in table position would
    /// truncate to the first return). The result is `MakeTuple(loads)`: boxed use
    /// materializes the table, a destructure elides it. The ABI is decided by the
    /// call's STATIC type, so direct and indirect callees of the same fn-type
    /// agree with no callee-identity analysis.
    fn emit_call_value(&mut self, e: Idx<Expr>, callee: Idx<Inst>, args: Vec<Idx<Inst>>) -> Idx<Inst> {
        self.emit_call_value_n(self.expr_tuple_shape(e), callee, args)
    }

    /// [`Self::emit_call_value`] with an explicit tuple shape (the safe-nav call
    /// paths pass the nil-peeled shape, since their expr type is `(..)?`).
    fn emit_call_value_n(
        &mut self,
        shape: Option<TupleRetShape>,
        callee: Idx<Inst>,
        args: Vec<Idx<Inst>>,
    ) -> Idx<Inst> {
        match shape {
            Some(shape) => {
                let dests: Vec<Idx<Cell>> =
                    (0..shape.slot_count()).map(|_| self.f.new_cell(None)).collect();
                self.emit_cur(Inst::CallValueMulti { dests: dests.clone(), callee, args });
                let loads: Vec<Idx<Inst>> =
                    dests.iter().map(|c| self.emit_cur(Inst::Load(*c))).collect();
                let r = self.tuple_result(&shape, &loads);
                self.pack(r)
            }
            None => self.emit_cur(Inst::CallValue { callee, args }),
        }
    }

    /// A scalar-ABI UFCS call `method(recv, args)` where `recv` is an
    /// already-lowered value (the safe-nav receiver). The receiver fills param 0
    /// (splat its leaves if that param is a value), args fill the rest; a value
    /// return packs to a single value (the safe-nav result is one nullable value).
    fn scalar_ufcs_call(
        &mut self,
        func: FuncRef,
        recv: Idx<Inst>,
        arg_idxs: &[Idx<Expr>],
        abi: &FnScalarAbi,
    ) -> Result<Idx<Inst>, CompilerError> {
        let mut argv = Vec::new();
        match abi.param_shapes.first().and_then(|s| s.clone()) {
            Some(shape) => argv.extend(self.cdata_leaves(recv, &shape)),
            None => argv.push(recv),
        }
        for (i, &a) in arg_idxs.iter().enumerate() {
            match abi.param_shapes.get(i + 1).and_then(|s| s.clone()) {
                Some(shape) => {
                    let r = self.lower_expr_repr(a)?;
                    argv.extend(self.repr_to_leaves(&r, &shape));
                }
                None => argv.push(self.lower_expr(a)?),
            }
        }
        match abi.ret_shape.clone() {
            Some(shape) => {
                let n = shape.leaf_count();
                let dests: Vec<Idx<Cell>> = (0..n).map(|_| self.f.new_cell(None)).collect();
                self.emit_cur(Inst::CallMulti { dests: dests.clone(), func, args: argv });
                let leaves: Vec<Idx<Inst>> =
                    dests.iter().map(|c| self.emit_cur(Inst::Load(*c))).collect();
                let mut idx = 0;
                let r = self.rebuild_from_leaves(&shape, &leaves, &mut idx);
                Ok(self.pack(r))
            }
            None => Ok(self.emit_cur(Inst::Call { func, args: argv })),
        }
    }

    /// Build the flat scalar-ABI argument list for a scalar-ABI callee: a value
    /// arg splats to its leaves, a non-value arg is boxed. Handles named-arg
    /// reordering (an omitted value param emits N nils so the callee default-fill
    /// fires; an omitted scalar param emits one nil).
    fn scalar_call_args(
        &mut self,
        func: FuncRef,
        arg_idxs: &[Idx<Expr>],
        any_named: bool,
        param_shapes: &[Option<ValueShape>],
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        if !any_named {
            let mut out = Vec::new();
            for (i, &a) in arg_idxs.iter().enumerate() {
                match param_shapes.get(i).and_then(|s| s.clone()) {
                    Some(shape) => {
                        let r = self.lower_expr_repr(a)?;
                        out.extend(self.repr_to_leaves(&r, &shape));
                    }
                    None => out.push(self.lower_expr(a)?),
                }
            }
            return Ok(out);
        }
        let param_names: Vec<Ustr> = self.ws.module(func.module).functions[func.func]
            .param_decls
            .iter()
            .map(|pd| pd.name)
            .collect();
        self.slotted_call_args(&param_names, param_shapes, arg_idxs)
    }

    /// Build a call's argument list by placing each argument into its PARAM SLOT
    /// (source order) and then splatting/nil-filling per slot: a named arg lands
    /// in the slot of that name, a struct spread `..op` by field name, everything
    /// else positionally. This is the one slot-placement rule, shared by every
    /// calling convention that has param names -- a convention with its own
    /// positional-only arg loop silently miscompiles `f(b: 2, a: 1)`.
    /// `param_shapes` shorter than `param_names` (or all-`None`) is the boxed ABI.
    fn slotted_call_args(
        &mut self,
        param_names: &[Ustr],
        param_shapes: &[Option<ValueShape>],
        arg_idxs: &[Idx<Expr>],
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        let mut slots: Vec<Option<ValueRepr>> = vec![None; param_names.len()];
        let mut next_pos = 0usize;
        for &a in arg_idxs {
            // Struct spread `..op`: decompose into per-field reprs (a struct-
            // literal operand is elided) and place each into its param slot.
            if self.body.spread_args.contains(&a) {
                for (fname, r) in self.spread_field_reprs(a)? {
                    if let Some(pidx) = param_names.iter().position(|p| *p == fname) {
                        slots[pidx] = Some(r);
                    }
                }
                continue;
            }
            let r = self.lower_expr_repr(a)?;
            let pidx = match self.body.arg_names.get(&a) {
                Some(nm) => param_names.iter().position(|p| p == nm).unwrap_or(next_pos),
                None => {
                    let i = next_pos;
                    next_pos += 1;
                    i
                }
            };
            if pidx < slots.len() {
                slots[pidx] = Some(r);
            }
        }
        let hi = slots.iter().rposition(|s| s.is_some()).map_or(0, |i| i + 1);
        let mut out = Vec::new();
        for i in 0..hi {
            match (slots[i].clone(), param_shapes.get(i).and_then(|s| s.clone())) {
                (Some(r), Some(shape)) => out.extend(self.repr_to_leaves(&r, &shape)),
                (Some(r), None) => out.push(self.pack(r)),
                (None, Some(shape)) => {
                    for _ in 0..shape.leaf_count() {
                        out.push(self.emit_cur(Inst::Const(Lit::Nil)));
                    }
                }
                (None, None) => out.push(self.emit_cur(Inst::Const(Lit::Nil))),
            }
        }
        Ok(out)
    }

    // --- ValueRepr <-> leaves <-> cdata (the scalar-ABI conversions) ---

    /// Materialize a `ValueRepr` as a single value — the one place a `Group`
    /// becomes a real `<name>_ct(...)` cdata (an escape: stored/compared/boxed).
    fn pack(&mut self, r: ValueRepr) -> Idx<Inst> {
        match r {
            ValueRepr::Scalar(v) => v,
            // A splatting tuple's box. Its `nobox` elements pack too, which is
            // exactly how a whole-tuple use reaches the guarantee's rejection.
            ValueRepr::Tuple { parts, .. } => {
                let vs = parts.into_iter().map(|p| self.pack(p)).collect();
                self.emit_cur(Inst::MakeTuple(vs))
            }
            // A flat-enum Group `[tag, _0.., <rec>..]` packs to the `{__variant}`
            // box (`MakeEnumDyn`), the enum escape -- not a `<name>_ct` cdata.
            ValueRepr::Group { name, parts, .. } if flat_enum_shape(self.ws, name).is_some() => {
                let shape = flat_enum_shape(self.ws, name).unwrap();
                let packed: Vec<Idx<Inst>> = parts.into_iter().map(|p| self.pack(p)).collect();
                let tag = packed[0];
                let positional = packed[1..1 + shape.tuple_width].to_vec();
                let named: Vec<(Ustr, Idx<Inst>)> = shape
                    .record_fields
                    .iter()
                    .enumerate()
                    .map(|(i, rf)| (*rf, packed[1 + shape.tuple_width + i]))
                    .collect();
                // A STATIC tag names a present variant: box directly. A payload-free
                // variant reuses its interned singleton (map-key / identity holds).
                if let Inst::Const(Lit::Str(variant)) = self.f.insts[tag] {
                    if self.variant_is_unit(name, variant) {
                        let g = Ustr::from(format!("__rock_enum_{name}_{variant}").as_str());
                        return self.emit_cur(Inst::Global(g));
                    }
                    return self.emit_cur(Inst::MakeEnumDyn { tag, ty: name, positional, named });
                }
                // A DYNAMIC tag may be `nil` (a null nullable enum): materialize to
                // `nil` in that case, else the box -- else a null threads to a
                // `{__variant = nil}` table that fails `== null`.
                let result = self.f.new_cell(None);
                let nil = self.emit_cur(Inst::Const(Lit::Nil));
                let is_nil = self.emit_cur(Inst::Eq(tag, nil));
                let nil_bb = self.f.new_block();
                let box_bb = self.f.new_block();
                let join = self.f.new_block();
                self.f.set_term(self.cur, Term::Branch { cond: is_nil, then_bb: nil_bb, else_bb: box_bb });
                self.cur = nil_bb;
                let z = self.emit_cur(Inst::Const(Lit::Nil));
                self.emit_cur(Inst::Store { cell: result, value: z });
                self.f.set_term(self.cur, Term::Goto(join));
                self.cur = box_bb;
                let boxed = self.emit_cur(Inst::MakeEnumDyn { tag, ty: name, positional, named });
                self.emit_cur(Inst::Store { cell: result, value: boxed });
                self.f.set_term(self.cur, Term::Goto(join));
                self.cur = join;
                self.emit_cur(Inst::Load(result))
            }
            // A leaf-table struct's physical keys ARE its leaves, so it packs
            // through the flat `<Name>_lct` ctor. Packing it field-wise would
            // box the nullable `nobox` child under its declared name -- the
            // guarantee's violation, and the only reason this struct is
            // GC-tier at all.
            ValueRepr::Group { name, .. } if value_struct_is_leaf_table(self.ws, name) => {
                let shape = value_shape(self.ws, name).expect("a leaf-table struct has a shape");
                let leaves = self.repr_to_leaves(&r, &shape);
                let fields = value_leaf_paths(&shape).into_iter().zip(leaves).collect();
                self.emit_cur(Inst::MakeValueStruct { name, fields })
            }
            ValueRepr::Group {
                name,
                parts,
                present: _,
            } => {
                // The Group's parts are in declared field order (the positional
                // `<name>_ct` ABI requires it), so they line up with the declared
                // names. The names let MIR passes forward a `Field` read to its
                // ctor operand (SROA), eliminating the cdata when it never escapes.
                // A payload-free enum field is just an `int` value (Prim), packed
                // like any primitive.
                let names = struct_field_names(self.ws, name);
                let fields = parts
                    .into_iter()
                    .enumerate()
                    .map(|(i, p)| {
                        let fname = names.get(i).copied().unwrap_or(name);
                        (fname, self.pack(p))
                    })
                    .collect();
                self.emit_cur(Inst::MakeValueStruct { name, fields })
            }
        }
    }

    /// Flatten a `ValueRepr` to its primitive leaf values per `shape` (the wire
    /// ABI). A boxed `Scalar` (e.g. a global cdata / if-join) is field-read.
    fn repr_to_leaves(&mut self, r: &ValueRepr, shape: &ValueShape) -> Vec<Idx<Inst>> {
        match r {
            // A tuple at a value-shaped sink is a boxed source: pack and
            // field-read, the `Scalar` treatment.
            ValueRepr::Tuple { .. } => {
                let v = self.pack(r.clone());
                self.repr_to_leaves(&ValueRepr::Scalar(v), shape)
            }
            ValueRepr::Group {
                parts, present, ..
            } => {
                let mut out = Vec::new();
                if shape.nullable {
                    out.push(match present {
                        Some(value) => *value,
                        None => self.emit_cur(Inst::Const(Lit::Bool(true))),
                    });
                }
                for ((_, fs), part) in shape.fields.iter().zip(parts) {
                    match fs {
                        FieldShape::Prim => out.push(self.pack(part.clone())),
                        FieldShape::Nested(sub) => out.extend(self.repr_to_leaves(part, sub)),
                    }
                }
                out
            }
            ValueRepr::Scalar(v) if shape.nullable => {
                let field_count = shape.leaf_count() - 1;
                if matches!(self.f.insts[*v], Inst::Const(Lit::Nil)) {
                    let mut out = Vec::with_capacity(1 + field_count);
                    out.push(self.emit_cur(Inst::Const(Lit::Bool(false))));
                    for _ in 0..field_count {
                        out.push(self.emit_cur(Inst::Const(Lit::Nil)));
                    }
                    return out;
                }

                let nil = self.emit_cur(Inst::Const(Lit::Nil));
                let present = self.emit_cur(Inst::NotEq(*v, nil));
                let cells: Vec<Idx<Cell>> =
                    (0..field_count).map(|_| self.f.new_cell(None)).collect();
                let nil_bb = self.f.new_block();
                let value_bb = self.f.new_block();
                let join = self.f.new_block();
                self.f.set_term(
                    self.cur,
                    Term::Branch {
                        cond: present,
                        then_bb: value_bb,
                        else_bb: nil_bb,
                    },
                );

                self.cur = nil_bb;
                for cell in &cells {
                    let value = self.emit_cur(Inst::Const(Lit::Nil));
                    self.emit_cur(Inst::Store {
                        cell: *cell,
                        value,
                    });
                }
                self.f.set_term(self.cur, Term::Goto(join));

                self.cur = value_bb;
                let mut inner = shape.clone();
                inner.nullable = false;
                let leaves = self.cdata_leaves(*v, &inner);
                for (cell, value) in cells.iter().zip(leaves) {
                    self.emit_cur(Inst::Store {
                        cell: *cell,
                        value,
                    });
                }
                self.f.set_term(self.cur, Term::Goto(join));

                self.cur = join;
                let mut out = Vec::with_capacity(1 + field_count);
                out.push(present);
                out.extend(cells.iter().map(|cell| self.emit_cur(Inst::Load(*cell))));
                out
            }
            ValueRepr::Scalar(v) => self.cdata_leaves(*v, shape),
        }
    }

    /// Flatten a boxed value to its primitive leaves via chained reads (the inverse
    /// of `pack`, used when a value arrives boxed). A flat ENUM box reads its box
    /// keys (`__variant`, positional `[i+1]`, `.recfield`) in shape order; a cdata
    /// value reads its `<name>_ct` fields.
    fn cdata_leaves(&mut self, v: Idx<Inst>, shape: &ValueShape) -> Vec<Idx<Inst>> {
        if let Some(es) = flat_enum_shape(self.ws, shape.name) {
            // NIL-SAFE spread: the box may be `nil` (a null nullable enum), and
            // `nil.__variant`/`nil[1]` would fault. Branch on nil -> all-nil leaves
            // (tag = nil, the null sentinel); else spread the box's keys.
            let n = es.leaf_count();
            let cells: Vec<Idx<Cell>> = (0..n).map(|_| self.f.new_cell(None)).collect();
            let nil = self.emit_cur(Inst::Const(Lit::Nil));
            let is_nil = self.emit_cur(Inst::Eq(v, nil));
            let nil_bb = self.f.new_block();
            let val_bb = self.f.new_block();
            let join = self.f.new_block();
            self.f.set_term(self.cur, Term::Branch { cond: is_nil, then_bb: nil_bb, else_bb: val_bb });
            self.cur = nil_bb;
            for c in &cells {
                let z = self.emit_cur(Inst::Const(Lit::Nil));
                self.emit_cur(Inst::Store { cell: *c, value: z });
            }
            self.f.set_term(self.cur, Term::Goto(join));
            self.cur = val_bb;
            let fc = self.spread_box_to_cells(v, &es);
            let leaves: Vec<Idx<Inst>> = std::iter::once(fc.tag)
                .chain(fc.positional)
                .chain(fc.named.into_iter().map(|(_, c)| c))
                .collect();
            for (c, lv) in cells.iter().zip(leaves) {
                self.emit_cur(Inst::Store { cell: *c, value: lv });
            }
            self.f.set_term(self.cur, Term::Goto(join));
            self.cur = join;
            return cells.iter().map(|c| self.emit_cur(Inst::Load(*c))).collect();
        }
        // A leaf-table box stores one key per leaf, so the split IS the key
        // reads -- the exact inverse of the `<Name>_lct` ctor, including the
        // presence key at a nullable child's own name.
        if value_struct_is_leaf_table(self.ws, shape.name) && !shape.nullable {
            return value_leaf_paths(shape)
                .into_iter()
                .map(|path| self.emit_cur(Inst::Field { base: v, field: path }))
                .collect();
        }
        let mut out = Vec::new();
        for (fname, fs) in &shape.fields {
            let fv = self.emit_cur(Inst::Field { base: v, field: *fname });
            match fs {
                FieldShape::Prim => out.push(fv),
                // A NULLABLE nested child: the stored box may be absent, so split
                // it presence-aware rather than field-reading straight off a
                // possible nil. Mirrors `repr_to_leaves`' `Scalar(v) if
                // shape.nullable` arm exactly -- presence is `v ~= nil` as leaf 0,
                // then the child's own leaves, nils on the absent arm -- because
                // the two must agree leaf-for-leaf or `rebuild_from_leaves`
                // indexes past the end. This function had NO nullable handling:
                // its one caller stripped `.nullable` first, which was safe only
                // while no shape carried it below the outermost level.
                FieldShape::Nested(sub) if sub.nullable => {
                    let field_count = sub.leaf_count() - 1;
                    let nil = self.emit_cur(Inst::Const(Lit::Nil));
                    let present = self.emit_cur(Inst::NotEq(fv, nil));
                    let cells: Vec<Idx<Cell>> =
                        (0..field_count).map(|_| self.f.new_cell(None)).collect();
                    let nil_bb = self.f.new_block();
                    let value_bb = self.f.new_block();
                    let join = self.f.new_block();
                    self.f.set_term(
                        self.cur,
                        Term::Branch { cond: present, then_bb: value_bb, else_bb: nil_bb },
                    );
                    self.cur = nil_bb;
                    for c in &cells {
                        let z = self.emit_cur(Inst::Const(Lit::Nil));
                        self.emit_cur(Inst::Store { cell: *c, value: z });
                    }
                    self.f.set_term(self.cur, Term::Goto(join));
                    self.cur = value_bb;
                    let mut inner = sub.clone();
                    inner.nullable = false;
                    let leaves = self.cdata_leaves(fv, &inner);
                    for (c, lv) in cells.iter().zip(leaves) {
                        self.emit_cur(Inst::Store { cell: *c, value: lv });
                    }
                    self.f.set_term(self.cur, Term::Goto(join));
                    self.cur = join;
                    out.push(present);
                    out.extend(
                        cells.iter().map(|c| self.emit_cur(Inst::Load(*c))).collect::<Vec<_>>(),
                    );
                }
                FieldShape::Nested(sub) => out.extend(self.cdata_leaves(fv, sub)),
            }
        }
        out
    }

    /// A whole-value read of value struct `name` out of a BOXED place (struct
    /// field, global, container element): split the box into leaves and rebuild a
    /// fresh `Group`, so `pack` materializes a COPY instead of aliasing the stored
    /// box. This is the same split-and-repack that already makes fn round-trips
    /// copy (params/returns ride the scalar ABI).
    ///
    /// Nested value fields recurse, so the snapshot survives a later place write
    /// through the owner -- a shallow expansion would snapshot the nested box's
    /// POINTER. A GC leaf is one slot, so its pointee stays shared by design. A
    /// leaf projection off the `Group` still reads its own slot at its own read
    /// point, and the unread leaves die in DCE.
    fn boxed_value_copy_repr(&mut self, boxed: Idx<Inst>, name: Ustr) -> ValueRepr {
        let Some(shape) = value_shape(self.ws, name).or_else(|| value_shape_gc(self.ws, name))
        else {
            return ValueRepr::Scalar(boxed);
        };
        let leaves = self.cdata_leaves(boxed, &shape);
        let mut idx = 0;
        self.rebuild_from_leaves(&shape, &leaves, &mut idx)
    }

    /// The `V?` twin of [`Self::boxed_value_copy_repr`]: an absent value has no
    /// leaves to split, so the copy runs on the PRESENT arm only and `nil` threads
    /// through untouched. The result is a single value (a fresh box or `nil`), not
    /// a `Group` -- a Group carries no presence for `pack`, which would materialize
    /// a box for an absent value and break `== null`.
    fn nullable_boxed_value_copy(&mut self, boxed: Idx<Inst>, name: Ustr) -> Idx<Inst> {
        let nil = self.emit_cur(Inst::Const(Lit::Nil));
        let is_nil = self.emit_cur(Inst::Eq(boxed, nil));
        let result = self.f.new_cell(None);
        let nil_bb = self.f.new_block();
        let val_bb = self.f.new_block();
        let join = self.f.new_block();
        self.f.set_term(self.cur, Term::Branch { cond: is_nil, then_bb: nil_bb, else_bb: val_bb });

        self.cur = nil_bb;
        let n = self.emit_cur(Inst::Const(Lit::Nil));
        self.store_and_join(result, n, join);

        self.cur = val_bb;
        let copy = self.boxed_value_copy_repr(boxed, name);
        let packed = self.pack(copy);
        self.store_and_join(result, packed, join);

        self.cur = join;
        self.emit_cur(Inst::Load(result))
    }

    /// One element of an indexable collection, for the loop lowerings that bind a
    /// whole element per iteration (`for x in coll`, `for (i, x) in enumerate`).
    ///
    /// Column storage has no element slot to index, so the read decomposes into K
    /// leaf reads and packs -- the same shape `let v = xs[i]` takes, which means
    /// SROA peels the pack wherever the binding is only projected from. Emitting a
    /// whole-element `Index` here instead is what made bitgun stop compiling: it
    /// reaches the emitter with no slot to name.
    fn loop_element_repr_at(
        &mut self,
        arr: Idx<Inst>,
        index: Idx<Inst>,
        kind: ContainerKind,
    ) -> Result<ValueRepr, CompilerError> {
        match kind {
            ContainerKind::VecValueColumns(elem) => self.column_element_repr(arr, index, elem),
            ContainerKind::NativeBufferValue(elem) if self.is_leaf_addressed(elem) => {
                self.nb_leaf_element_repr(arr, index, elem)
            }
            _ => Ok(ValueRepr::Scalar(self.emit_cur(Inst::Index {
                base: arr,
                index,
                kind,
            }))),
        }
    }

    fn loop_element_at(
        &mut self,
        arr: Idx<Inst>,
        index: Idx<Inst>,
        kind: ContainerKind,
    ) -> Result<Idx<Inst>, CompilerError> {
        let repr = self.loop_element_repr_at(arr, index, kind)?;
        Ok(self.pack(repr))
    }

    /// Reconstruct a nested `Group` from a flat leaf list per `shape` (the inverse
    /// of `repr_to_leaves`), consuming leaves depth-first from `*idx`.
    fn rebuild_from_leaves(
        &mut self,
        shape: &ValueShape,
        leaves: &[Idx<Inst>],
        idx: &mut usize,
    ) -> ValueRepr {
        // `shape` is borrowed from a clone at the call sites, so collect the field
        // shapes first to avoid holding the borrow across `&mut self` calls.
        let fields: Vec<FieldShape> = shape.fields.iter().map(|(_, fs)| fs.clone()).collect();
        let name = shape.name;
        let present = shape.nullable.then(|| {
            let value = leaves[*idx];
            *idx += 1;
            value
        });
        let mut parts = Vec::with_capacity(fields.len());
        for fs in &fields {
            let part = match fs {
                FieldShape::Prim => {
                    let v = leaves[*idx];
                    *idx += 1;
                    ValueRepr::Scalar(v)
                }
                FieldShape::Nested(sub) => self.rebuild_from_leaves(sub, leaves, idx),
            };
            parts.push(part);
        }
        ValueRepr::Group {
            name,
            parts,
            present,
        }
    }

    /// Index of `field` in value struct `name`'s declared field order.
    fn field_index(&self, name: Ustr, field: Ustr) -> Option<usize> {
        self.struct_field_names(name)?.iter().position(|f| *f == field)
    }

    /// Lower an expression to a `ValueRepr`: a value param read / value field
    /// projection / value literal / scalar-ABI value call stays UNBOXED (a
    /// `Group`); everything else is a `Scalar`. The ABI boundaries
    /// (params/returns/call args) consume this to splat without a cdata round-trip.
    fn lower_expr_repr(&mut self, e: Idx<Expr>) -> Result<ValueRepr, CompilerError> {
        match self.body.expr_arena[e].clone() {
            Expr::Block { statements, tail } => self.lower_block_repr(statements, tail),
            Expr::If {
                cond,
                then_branch,
                else_branch,
            } => match self.repr_join_slot(e) {
                Some(shape) => self.lower_if_repr(cond, then_branch, else_branch, shape),
                None => Ok(ValueRepr::Scalar(
                    self.lower_if(cond, then_branch, else_branch)?,
                )),
            },
            Expr::Match { scrutinee, arms } => match self.repr_join_slot(e) {
                Some(shape) => self.lower_match_repr(scrutinee, arms, shape),
                None => Ok(ValueRepr::Scalar(self.lower_match(scrutinee, arms)?)),
            },
            // A tuple literal whose type splats stays a `Tuple` repr, so a
            // `nobox` element reaches the return boundary as leaves instead of
            // being packed into the table first.
            Expr::Tuple(elems) if self.expr_tuple_shape(e).is_some_and(|s| s.splats()) => {
                let parts = elems
                    .into_iter()
                    .map(|el| self.lower_expr_repr(el))
                    .collect::<Result<_, CompilerError>>()?;
                Ok(ValueRepr::Tuple { parts, present: None })
            }
            // `t.0` off a splatting tuple projects the part REPR directly. Without
            // this the base packs to a table and the index reads a boxed element
            // back out, which re-materializes a `nobox` one. A newtype's `.0` is
            // identity and a non-splatting base keeps the packed `TupleIndex`, so
            // only the unboxed case changes.
            Expr::TupleIndex { tuple, index } if !self.expr_is_newtype(tuple) => {
                let r = self.lower_expr_repr(tuple)?;
                if let ValueRepr::Tuple { parts, .. } = r {
                    return parts
                        .into_iter()
                        .nth(index)
                        .ok_or_else(|| gap("tuple index past the end of a splatting tuple"));
                }
                let base = self.pack(r);
                Ok(ValueRepr::Scalar(self.emit_cur(Inst::TupleIndex { base, index })))
            }
            // A name bound to a splatting tuple: rebuild the unboxed Tuple repr
            // from its slot cells, so `t.0` projects a part instead of indexing a
            // packed table (which would re-materialize a `nobox` element).
            Expr::Var(b) if self.tuple_binds.contains_key(&raw(b)) => {
                let (cells, shape) = self.tuple_binds.get(&raw(b)).cloned().unwrap();
                let slots: Vec<Idx<Inst>> =
                    cells.iter().map(|c| self.emit_cur(Inst::Load(*c))).collect();
                let mut idx = 0;
                Ok(self.tuple_from_slots(&shape, &slots, &mut idx))
            }
            // A scalarized value param: rebuild the unboxed Group from leaf cells.
            Expr::Var(b) if self.group_binds.contains_key(&raw(b)) => {
                let (cells, shape) = self.group_binds.get(&raw(b)).cloned().unwrap();
                let leaves: Vec<Idx<Inst>> =
                    cells.iter().map(|c| self.emit_cur(Inst::Load(*c))).collect();
                let mut idx = 0;
                Ok(self.rebuild_from_leaves(&shape, &leaves, &mut idx))
            }
            // `base.field`: project a Group part directly (no cdata); else a
            // boxed field read off the packed base.
            Expr::FieldAccess { base, field } => {
                // A swizzle read (docs/vec-swizzles-plan.md): select the
                // components into a fresh VecN Group (len 1: the bare scalar) --
                // scalarized end to end, so a swizzle never boxes by itself.
                if let Some(idxs) = self.swizzle_of(base, field) {
                    let br = self.lower_expr_repr(base)?;
                    if let ValueRepr::Group { parts, .. } = &br {
                        let sel: Vec<ValueRepr> = idxs.iter().map(|&i| parts[i].clone()).collect();
                        return Ok(swizzle_group(sel));
                    }
                    let bv = self.pack(br);
                    return Ok(self.swizzle_from_packed(bv, &idxs));
                }
                // A flattened enum field read as a WHOLE value re-materializes the
                // box from the parent's slots (the single escape boundary).
                if let Some(shape) = self.flat_field_of(base, field) {
                    let parent = self.lower_expr(base)?;
                    return Ok(ValueRepr::Scalar(self.materialize_flat(parent, field, &shape)));
                }
                // An embedded field read as a WHOLE value re-materializes from
                // its leaves. Projection-only keeps every such read transient
                // (never bound, returned, captured or stored), so this is the
                // consuming boundary, not a reintroduced box.
                if let Some(cname) = self.embed_field_of(base, field) {
                    // A leaf-table VALUE parent can still be UNBOXED here (a
                    // local, a param, a call result): project the part off the
                    // Group rather than packing a table just to read dotted keys
                    // back out of it. A reference parent is never a Group, so
                    // this changes nothing for the `embed` case proper.
                    let br = self.lower_expr_repr(base)?;
                    if let ValueRepr::Group { name, parts, .. } = &br {
                        if let Some(i) = self.field_index(*name, field) {
                            return Ok(parts[i].clone());
                        }
                    }
                    let boxed = self.pack(br);
                    // A `nobox` child stays in LEAF form instead. The transient
                    // box is legal for an unmarked value -- it sinks -- but for
                    // a marked one it is the guarantee's violation itself, so
                    // the sink decides the representation. This is what lets a
                    // field load widen to `T?`: a Group carries no presence, so
                    // `repr_to_leaves` supplies it, exactly as it already does
                    // for a literal ctor or a call result at the same param.
                    if let Some(shape) = self.nobox_embed_group_shape(cname) {
                        let parent = boxed;
                        let mut g = self.embed_group(parent, field, &shape);
                        // A NULLABLE child carries presence in the field's own
                        // (undotted) key, so the Group takes it as its presence
                        // rather than gating the read: an absent child's leaves
                        // are nil, and every consumer branches on presence
                        // before touching them.
                        if self.embed_read_is_nullable(base, field) {
                            let present = self.emit_cur(Inst::Field { base: parent, field });
                            if let ValueRepr::Group { present: p, .. } = &mut g {
                                *p = Some(present);
                            }
                        }
                        return Ok(g);
                    }
                    let v = if self.embed_read_is_nullable(base, field) {
                        self.materialize_embed_nullable(boxed, field, cname)
                    } else {
                        self.materialize_embed(boxed, field, cname)
                    };
                    return Ok(ValueRepr::Scalar(v));
                }
                // An embed LEAF read composes to one dotted key on the parent
                // that holds it; no intermediate projection exists.
                let (pbase, key) = self.physical_field(base, field);
                if key != field {
                    let parent = self.lower_expr(pbase)?;
                    return Ok(ValueRepr::Scalar(
                        self.emit_cur(Inst::Field { base: parent, field: key }),
                    ));
                }
                let mut br = self.lower_expr_repr(base)?;
                // A leaf-table BOX has no declared-field keys -- a nested child
                // lives at dotted leaves and its own name holds only presence,
                // so `Field { base, "pos" }` would hand the consumer a bool.
                // Split it back into leaves and project the rebuilt Group; the
                // leaves nothing reads die in DCE.
                if let ValueRepr::Scalar(v) = br {
                    match self
                        .value_struct_name_of(base)
                        .filter(|n| value_struct_is_leaf_table(self.ws, *n))
                    {
                        Some(sname) => br = self.boxed_value_copy_repr(v, sname),
                        None => br = ValueRepr::Scalar(v),
                    }
                }
                if let ValueRepr::Group { name, parts, .. } = &br {
                    if let Some(i) = self.field_index(*name, field) {
                        return Ok(parts[i].clone());
                    }
                }
                // A boxed base: read the field directly. A payload-free enum field
                // is a plain `int` (the enum value IS the int), so no conversion --
                // same as any primitive field. A VALUE-struct field is a whole-value
                // read out of a mutable place, so it copies.
                let basev = self.pack(br);
                let fv = self.emit_cur(Inst::Field { base: basev, field });
                if let Some(name) = self.value_struct_of(e).filter(|_| self.is_mutable_place(base))
                {
                    return Ok(self.boxed_value_copy_repr(fv, name));
                }
                match self.nullable_place_value_of(e) {
                    Some(name) => Ok(ValueRepr::Scalar(self.nullable_boxed_value_copy(fv, name))),
                    None => Ok(ValueRepr::Scalar(fv)),
                }
            }
            // A `nobox` global lives in K scalar globals, so the read IS the leaf
            // snapshot -- no box to copy out of.
            Expr::Global(_) if self.nobox_global_place(e).is_some() => {
                let (leaves, shape) = self.nobox_global_place(e).expect("guard matched");
                Ok(self.nobox_global_load(&leaves, shape))
            }
            // A global holding a value struct is a mutable place: a whole-value read
            // copies (the leaves are snapshotted at the read point).
            Expr::Global(g) if self.value_struct_of(e).is_some() => {
                let name = self.value_struct_of(e).unwrap();
                let gv = self.emit_cur(Inst::Global(g));
                Ok(self.boxed_value_copy_repr(gv, name))
            }
            // A `V?` global copies the same way, gated on presence.
            Expr::Global(g) if self.nullable_place_value_of(e).is_some() => {
                let name = self.nullable_place_value_of(e).unwrap();
                let gv = self.emit_cur(Inst::Global(g));
                Ok(ValueRepr::Scalar(self.nullable_boxed_value_copy(gv, name)))
            }
            // A value-struct literal stays an unboxed Group (its fields lowered as
            // reprs); `pack` builds the cdata only if a boxed context needs it.
            Expr::StructLiteral { name, fields, bases } if self.is_value_struct(name) => {
                self.value_literal_repr(name, fields, bases)
            }
            // A cdata-eligible enum construction stays an unboxed Group `[tag, _0]`
            // (so a callsite/field can splat it without a cdata round-trip); `pack`
            // builds `<Enum>_ct` only if a boxed context needs it.
            Expr::EnumVariant { enum_name, variant, args, .. } if enum_value_shape(self.ws, enum_name).is_some() => {
                let shape = enum_value_shape(self.ws, enum_name).unwrap();
                self.enum_cdata_repr(&shape, variant, args)
            }
            // A flat (boxed) enum construction stays an unboxed Group `[tag, _0..,
            // <rec>..]` so a call/return/match can thread it as scalars; `pack`
            // builds the `{__variant}` box only if a boxed context needs it.
            Expr::EnumVariant { enum_name, .. } if flat_enum_shape(self.ws, enum_name).is_some() => {
                let shape = flat_enum_shape(self.ws, enum_name).unwrap();
                let cells = self.lower_flat_value(&shape, e)?;
                Ok(self.flat_cells_to_repr(&shape, cells))
            }
            Expr::Call { callee, args } => self.lower_call_repr(e, callee, args),
            // A COLUMN element read routes on the container, ahead of the
            // element-type guard below: column storage has no element slot at
            // all, and inside a body specialized at `T = E?` the element type is
            // an unsubstituted generic that no type predicate can classify.
            Expr::Index { base, index } if self.vec_column_elem_of(base).is_some() => {
                let elem = self.vec_column_elem_of(base).expect("guard matched");
                let (b, _) = self.lower_index_base(base)?;
                let i = self.lower_expr(index)?;
                self.column_element_repr(b, i, elem)
            }
            // A value-struct ELEMENT read (`buf[i]`/`fv[i]`/`v[i]`) as an unboxed
            // Group of leaf reads off the checked slot: a leaf projection reads the
            // part live, while `pack` (a whole-value use -- let-bind, arg, return,
            // store) rebuilds a fresh value instead of aliasing the array slot.
            // A `V?` element takes the same shape, gated on presence at the copy.
            Expr::Index { base, index }
                if self.value_struct_of(e).is_some()
                    || self.nullable_place_value_of(e).is_some() =>
            {
                let nullable = self.value_struct_of(e).is_none();
                let name = self
                    .value_struct_of(e)
                    .or_else(|| self.nullable_place_value_of(e))
                    .expect("guard matched");
                let (b, kind) = self.lower_index_base(base)?;
                let i = self.lower_expr(index)?;
                // Column storage: there is no element slot to read leaves off --
                // each leaf lives in its own array, so the Group is built from K
                // direct column reads. Bounds are checked ONCE for the element.
                if let ContainerKind::VecValueColumns(elem) = kind {
                    return self.column_element_repr(b, i, elem);
                }
                // A column-stored Map reaches its leaves the same way, one
                // indirection out: the spine resolves the key to a slot, and the
                // slot indexes `data`'s columns.
                if let Some(elem) = self.map_column_value_of(base) {
                    return self.map_column_index_repr(b, i, elem);
                }
                // Leaf addressing: no slot reference, no nested ctor -- each
                // leaf comes straight from its typed view.
                if let ContainerKind::NativeBufferValue(elem) = kind {
                    if self.is_leaf_addressed(elem) {
                        return self.nb_leaf_element_repr(b, i, elem);
                    }
                }
                let slot = self.emit_cur(Inst::Index { base: b, index: i, kind });
                if nullable {
                    return Ok(ValueRepr::Scalar(self.nullable_boxed_value_copy(slot, name)));
                }
                Ok(self.boxed_value_copy_repr(slot, name))
            }
            Expr::BinOp { op: BinOp::NullCoalesce, lhs, rhs }
                if self.nobox_value_name(e).is_some() =>
            {
                self.nullcoalesce_repr(e, lhs, rhs)
            }
            _ => Ok(ValueRepr::Scalar(self.lower_expr(e)?)),
        }
    }

    /// `a ?? b` whose RESULT is a `nobox` value: both arms thread leaves and join
    /// through cells, so the coalesce never packs a cdata just to choose between
    /// them. The lhs leaves are read before the branch (their block dominates
    /// both arms) and its presence leaf IS the condition -- no nil compare.
    fn nullcoalesce_repr(
        &mut self,
        e: Idx<Expr>,
        lhs: Idx<Expr>,
        rhs: Idx<Expr>,
    ) -> Result<ValueRepr, CompilerError> {
        let name = self.nobox_value_name(e).expect("caller checked");
        let shape = value_shape_of_nominal(self.ws, name)
            .ok_or_else(|| gap(format!("nobox value `{name}` has no scalar shape")))?;
        let mut lhs_shape = shape.clone();
        lhs_shape.nullable = true;

        let lhs_repr = self.lower_expr_repr(lhs)?;
        let lhs_leaves = self.repr_to_leaves(&lhs_repr, &lhs_shape);
        let (present, lhs_values) = lhs_leaves
            .split_first()
            .expect("a nullable shape carries its presence leaf first");
        let (present, lhs_values) = (*present, lhs_values.to_vec());

        let cells: Vec<Idx<Cell>> = lhs_values.iter().map(|_| self.f.new_cell(None)).collect();
        let lhs_bb = self.f.new_block();
        let rhs_bb = self.f.new_block();
        let join = self.f.new_block();
        self.f.set_term(
            self.cur,
            Term::Branch { cond: present, then_bb: lhs_bb, else_bb: rhs_bb },
        );

        self.cur = rhs_bb;
        let rhs_repr = self.lower_expr_repr(rhs)?;
        let rhs_leaves = self.repr_to_leaves(&rhs_repr, &shape);
        for (&c, &value) in cells.iter().zip(&rhs_leaves) {
            self.emit_cur(Inst::Store { cell: c, value });
        }
        if self.is_open() {
            self.f.set_term(self.cur, Term::Goto(join));
        }

        self.cur = lhs_bb;
        for (&c, &value) in cells.iter().zip(&lhs_values) {
            self.emit_cur(Inst::Store { cell: c, value });
        }
        if self.is_open() {
            self.f.set_term(self.cur, Term::Goto(join));
        }

        self.cur = join;
        let loads: Vec<Idx<Inst>> = cells.iter().map(|&c| self.emit_cur(Inst::Load(c))).collect();
        let mut idx = 0;
        Ok(self.rebuild_from_leaves(&shape, &loads, &mut idx))
    }

    /// A value-struct literal as an unboxed `Group` (declared field order, defaults
    /// filled; grouped fields share one evaluation). Mirrors `lower_struct_literal`
    /// but threads `ValueRepr`s so nested value fields stay unboxed.
    fn value_literal_repr(
        &mut self,
        name: Ustr,
        fields: comfy_la_arena::IdxRange<bone_semantic::hir::StructLitField>,
        bases: comfy_la_arena::IdxRange<bone_semantic::hir::Expr>,
    ) -> Result<ValueRepr, CompilerError> {
        // Functional-update base: evaluate once (before fields). A Group base
        // projects its parts directly (no cdata); a boxed base reads fields off
        // the packed cdata (the normal value-field read). Inference guarantees
        // at most one base of this struct's type.
        let base_repr: Option<ValueRepr> = match bases.into_iter().next() {
            Some(b) => Some(self.lower_expr_repr(b)?),
            None => None,
        };
        let base_boxed: Option<Idx<Inst>> = match &base_repr {
            Some(ValueRepr::Group { .. }) | None => None,
            Some(other) => Some(self.pack(other.clone())),
        };
        let mut litmap: Vec<(Ustr, ValueRepr)> = Vec::new();
        let mut by_value: HashMap<u32, ValueRepr> = HashMap::new();
        for fidx in fields {
            let lf = self.body.struct_lit_field_arena[fidx];
            let key: u32 = lf.value.into_raw().into();
            let r = match by_value.get(&key) {
                Some(r) => r.clone(),
                None => {
                    let r = self.lower_expr_repr(lf.value)?;
                    by_value.insert(key, r.clone());
                    r
                }
            };
            let r = self.coerce_field_repr(r, lf.value, name, lf.name);
            litmap.push((lf.name, r));
        }
        let parts: Vec<ValueRepr> = match self.struct_field_names(name) {
            Some(decl) => {
                let mut fs = Vec::new();
                for fname in decl {
                    match litmap.iter().find(|(n, _)| *n == fname) {
                        Some((_, r)) => fs.push(r.clone()),
                        None => match (&base_repr, base_boxed) {
                            (Some(ValueRepr::Group { name: bn, parts, .. }), _) => {
                                let i = self
                                    .field_index(*bn, fname)
                                    .expect("functional-update base covers every field");
                                fs.push(parts[i].clone());
                            }
                            (Some(_), Some(bi)) => {
                                fs.push(ValueRepr::Scalar(
                                    self.emit_cur(Inst::Field { base: bi, field: fname }),
                                ));
                            }
                            _ => {
                                let v = self.lower_struct_default(name, fname)?;
                                fs.push(ValueRepr::Scalar(v));
                            }
                        },
                    }
                }
                fs
            }
            None => litmap.into_iter().map(|(_, r)| r).collect(),
        };
        Ok(ValueRepr::Group {
            name,
            parts,
            present: None,
        })
    }

    /// Reorder a call's arguments (some written `name = value`) into the callee's
    /// declared parameter order. Args evaluate in SOURCE order (left-to-right);
    /// each lands in its param slot (named -> by name, positional -> next free
    /// front slot). A slot with no arg becomes an explicit `nil` so the callee's
    /// default-fill prologue triggers. Trailing empty slots are dropped (Lua nils
    /// them).
    fn reorder_named_args(
        &mut self,
        arg_idxs: &[Idx<Expr>],
        func: FuncRef,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        let param_names: Vec<Ustr> = self
            .ws
            .module(func.module)
            .functions[func.func]
            .param_decls
            .iter()
            .map(|pd| pd.name)
            .collect();
        let mut slots: Vec<Option<Idx<Inst>>> = vec![None; param_names.len()];
        let mut next_pos = 0usize;
        for &a in arg_idxs {
            // Struct spread `..op`: decompose the operand into per-field values
            // (a struct-literal operand is elided) and place each into the param
            // slot of the same name. bone verified every field matches a param;
            // a named arg after the spread overrides via last-write-wins.
            if self.body.spread_args.contains(&a) {
                for (fname, fv) in self.spread_field_insts(a)? {
                    if let Some(pidx) = param_names.iter().position(|p| *p == fname) {
                        slots[pidx] = Some(fv);
                    }
                }
                continue;
            }
            let v = self.lower_expr(a)?;
            let pidx = match self.body.arg_names.get(&a) {
                Some(nm) => param_names.iter().position(|p| p == nm).unwrap_or(next_pos),
                None => {
                    let i = next_pos;
                    next_pos += 1;
                    i
                }
            };
            if pidx < slots.len() {
                let v = match self.param_ty(func, pidx) {
                    Some(t) => self.coerced(v, a, t),
                    None => v,
                };
                slots[pidx] = Some(v);
            }
        }
        let hi = slots.iter().rposition(|s| s.is_some()).map_or(0, |i| i + 1);
        let mut out = Vec::with_capacity(hi);
        for s in slots.into_iter().take(hi) {
            out.push(match s {
                Some(v) => v,
                None => self.emit_cur(Inst::Const(Lit::Nil)),
            });
        }
        Ok(out)
    }

    /// The overload target bone resolved on expr `e` (a unary `-x`/`!x`/`~x`
    /// over a struct records its `__op_*` callee here), or `None` for a native
    /// scalar op.
    fn overload_target(&self, e: Idx<Expr>) -> Option<FuncRef> {
        self.inf
            .and_then(|i| i.resolved_overload_calls.get(&e).copied())
            .map(|(module, func)| FuncRef { module, func })
    }

    /// Resolve a named call to a [`FuncRef`]. bone records overload-resolved
    /// targets in `resolved_overload_calls`; a non-overloaded name resolves
    /// uniquely through `functions_by_name`.
    /// The emitted symbol for a FUNCTION taken by value (`let f = adder`). Prefer
    /// bone's recorded overload resolution (an overload disambiguated by the
    /// value's expected type); otherwise every candidate is a backend VARIANT of
    /// one `(name, signature)` and so mangles identically -- dedup and take it.
    /// A genuine unresolved overload rejects (reject, don't fall back).
    fn fn_value_symbol(&self, e: Idx<Expr>, name: ustr::Ustr) -> Result<String, CompilerError> {
        if let Some(fref) = self.overload_target(e) {
            let f = &self.ws.module(fref.module).functions[fref.func];
            return Ok(crate::mangle::mangle_fn(self.ws, f));
        }
        let cands = self.ws.functions_by_name.get(&name).map(|l| l.as_slice()).unwrap_or(&[]);
        let mut names: Vec<String> = cands
            .iter()
            .map(|&(m, fi)| crate::mangle::mangle_fn(self.ws, &self.ws.module(m).functions[fi]))
            .collect();
        names.sort();
        names.dedup();
        match names.as_slice() {
            [only] => Ok(only.clone()),
            [] => Err(gap(format!("unresolved function value `{name}`"))),
            _ => Err(gap(format!(
                "function value `{name}` is an unresolved overload (distinct signatures)"
            ))),
        }
    }

    /// Whether the call at `e` resolves to an `inline(lua)`-bodied definition of
    /// `name`. Guards the name-keyed intrinsic fast paths: a user fn may legally
    /// share a stdlib name, and rewriting ITS call as the intrinsic miscompiles.
    /// A rock-bodied user fn has no inline body, which is the discriminant --
    /// module identity is not, because the test harnesses build their own
    /// stdlib. Fails CLOSED: an unresolved callee is not the intrinsic.
    fn resolves_to_inline_builtin(&self, e: Idx<Expr>, name: ustr::Ustr) -> bool {
        let Ok(fref) = self.resolve_funcref(e, name) else {
            return false;
        };
        self.ws.module(fref.module).functions[fref.func]
            .inline_body
            .is_some()
    }

    fn resolve_funcref(
        &self,
        e: Idx<Expr>,
        name: ustr::Ustr,
    ) -> Result<FuncRef, CompilerError> {
        let rep = if let Some(&(module, func)) = self
            .inf
            .and_then(|i| i.resolved_overload_calls.get(&e))
        {
            FuncRef { module, func }
        } else {
            match self.ws.functions_by_name.get(&name).map(|l| l.as_slice()) {
                Some([(module, func)]) => FuncRef {
                    module: *module,
                    func: *func,
                },
                Some([]) | None => return Err(gap(format!("unresolved call `{name}`"))),
                Some(_) => {
                    return Err(gap(format!(
                        "overloaded call `{name}` with no recorded resolution"
                    )))
                }
            }
        };
        self.select_backend_variant(e, name, rep)
    }

    /// Pick the backend-variant of a collapsed call `rep` that serves the current
    /// codegen [`target`](ScalarCtx::target). bone typechecks one backend-agnostic
    /// HIR and records ONE representative per overload; codegen selects here:
    ///
    /// - `Any` target (GLSL / generic): no filtering, `rep` as-is.
    /// - `rep` already serves the target: `rep` as-is (the common Lua path).
    /// - a same-signature sibling serves the target: use it (the JS side of a
    ///   same-name pair -- the representative is the luajit variant, the sibling
    ///   the `inline(js)`/`extern(js)` one).
    /// - `Lua` target, no lua/any binding: REJECT (a JS-only decl called from
    ///   lua-reachable code).
    /// - `Js` target, no js/any binding and no sibling: leave `rep` for the JS
    ///   emitter's own host-mapping / reject (a `inline(lua)` stdlib primitive with
    ///   a hand-written JS template must still resolve there).
    fn select_backend_variant(
        &self,
        e: Idx<Expr>,
        name: ustr::Ustr,
        rep: FuncRef,
    ) -> Result<FuncRef, CompilerError> {
        use bone_semantic::hir::FnBackend;
        let target = self.scx.target;
        if target == FnBackend::Any {
            return Ok(rep);
        }
        let rep_backend = self.ws.module(rep.module).functions[rep.func].backend;
        if rep_backend == target {
            return Ok(rep);
        }
        // A backend sibling can exist ONLY when the name has >=2 candidates
        // (bone collapsed a backend-variant group to `rep`). The overwhelmingly
        // common single-candidate call skips the sibling scan entirely, keeping
        // the `Any`-serves-everything fast path off the hot overload-select path.
        let has_variants = self
            .ws
            .functions_by_name
            .get(&name)
            .is_some_and(|l| l.len() >= 2);
        if !has_variants {
            return if serves_target(rep_backend, target) {
                Ok(rep)
            } else {
                self.reject_or_defer(e, name, rep, target)
            };
        }
        // An exact-target sibling outranks an `Any` representative: backend
        // mapping beats the portable fallback (the intrinsics precedence
        // doctrine). Without this an `inline(js)` variant of a portable-bodied
        // fn is unreachable, since `Any` serves every target.
        if let Some(sib) = self.find_sibling_variant(rep, target, true) {
            return Ok(sib);
        }
        if rep_backend == FnBackend::Any {
            return Ok(rep);
        }
        // rep is the other concrete backend (e.g. lua while target is js): an
        // `Any` sibling can still serve as the portable fallback.
        if let Some(sib) = self.find_sibling_variant(rep, target, false) {
            return Ok(sib);
        }
        self.reject_or_defer(e, name, rep, target)
    }

    /// No servable binding for `target` was found: a `Lua` target REJECTS (a
    /// js-only decl reached from lua-reachable code); a `Js` target defers `rep`
    /// to the JS emitter (which may host-map a lua-bodied stdlib primitive, or
    /// reject with its own message).
    fn reject_or_defer(
        &self,
        e: Idx<Expr>,
        name: ustr::Ustr,
        rep: FuncRef,
        target: bone_semantic::hir::FnBackend,
    ) -> Result<FuncRef, CompilerError> {
        use bone_semantic::hir::FnBackend;
        match target {
            FnBackend::Lua => Err(CompilerError::new_with_label(
                self.body.expr_spans[e],
                format!(
                    "call to `{name}` has no luajit binding: its only declaration targets the `js` backend"
                ),
            )),
            _ => Ok(rep),
        }
    }

    /// A same-name, same-signature sibling of `rep` whose backend serves `target`.
    /// Signature match is param-type equality (rendered, inference-free) -- the
    /// same key bone's `collapse_backend_variants` groups on. The caller has
    /// already established the name has >=2 candidates, so `rep_sig` (a per-param
    /// render) is only built when a sibling could exist.
    fn find_sibling_variant(
        &self,
        rep: FuncRef,
        target: bone_semantic::hir::FnBackend,
        exact: bool,
    ) -> Option<FuncRef> {
        use bone_semantic::hir::FnBackend;
        let rf = &self.ws.module(rep.module).functions[rep.func];
        let name = rf.name;
        let rep_sig: Vec<String> = rf.param_decls.iter().map(|p| self.ws.render(None, p.ty)).collect();
        let cands = self.ws.functions_by_name.get(&name)?;
        for &(mid, fid) in cands.iter() {
            if mid == rep.module && fid == rep.func {
                continue;
            }
            let f = &self.ws.module(mid).functions[fid];
            let ok = if exact {
                f.backend == target
            } else {
                f.backend == FnBackend::Any || f.backend == target
            };
            if !ok {
                continue;
            }
            if f.param_decls.len() != rep_sig.len() {
                continue;
            }
            if f.param_decls.iter().zip(&rep_sig).all(|(p, r)| self.ws.render(None, p.ty) == *r) {
                return Some(FuncRef { module: mid, func: fid });
            }
        }
        None
    }

    /// `await sub` — drive the inner coroutine `sub` through its `.resume()`,
    /// re-yielding each value; the expression's value is `sub`'s return value.
    /// Built as a normal pump loop with a `yield` inside, so the coroutine
    /// transform splits it like any other yield. Mirrors rock-codegen / old skull.
    fn lower_yield_delegate(&mut self, sub_expr: Idx<Expr>) -> Result<Idx<Inst>, CompilerError> {
        let rac = self.resume_arg.ok_or_else(|| gap("await outside a coroutine"))?;
        let sub = self.lower_expr(sub_expr)?;
        // Register `sub` as the active inner (kill() chains into it).
        self.emit_cur(Inst::CoroSetInner(sub));

        // A flat-enum delegation threads the inner's yield leaves through the
        // frame-slot channel (advance + read `sub.__frame.__y*`), re-yielding them
        // as leaves -- no per-yield box on either coroutine. Otherwise the boxed
        // path below drives the inner's generic `resume`.
        if let Some(shape) = self.coroutine_yield_flat_shape(sub_expr) {
            return self.lower_yield_delegate_flat(sub, rac, &shape);
        }

        let slot = self.f.new_cell(None);
        let resume0 = self.emit_cur(Inst::Field { base: sub, field: Ustr::from("__resume") });
        let primed = self.emit_cur(Inst::CallValue { callee: resume0, args: vec![sub] });
        self.emit_cur(Inst::Store { cell: slot, value: primed });

        let header = self.f.new_block();
        let body = self.f.new_block();
        let exit = self.f.new_block();
        self.f.set_term(self.cur, Term::Goto(header));

        // header: loop while the inner's last yield wasn't nil (still producing).
        self.cur = header;
        let v = self.emit_cur(Inst::Load(slot));
        let nil = self.emit_cur(Inst::Const(Lit::Nil));
        let is_nil = self.emit_cur(Inst::Eq(v, nil));
        self.f.set_term(self.cur, Term::Branch { cond: is_nil, then_bb: exit, else_bb: body });

        // body: yield the inner's value, resume it with our resume arg.
        self.cur = body;
        let vv = self.emit_cur(Inst::Load(slot));
        let resume_bb = self.f.new_block();
        self.f.set_term(self.cur, Term::Yield { values: vec![vv], resume: resume_bb });
        self.cur = resume_bb;
        let arg = self.emit_cur(Inst::Load(rac));
        let resume1 = self.emit_cur(Inst::Field { base: sub, field: Ustr::from("__resume") });
        let v2 = self.emit_cur(Inst::CallValue { callee: resume1, args: vec![sub, arg] });
        self.emit_cur(Inst::Store { cell: slot, value: v2 });
        self.f.set_term(self.cur, Term::Goto(header));

        // exit: clear the active inner, re-raise the inner's error if any, then
        // the `await` value is the inner's return.
        self.cur = exit;
        self.yield_delegate_exit(sub)
    }

    /// The `await` epilogue (shared by the boxed + flat channels): clear the
    /// active inner, re-raise the inner's error if any, evaluate to its return.
    /// The inner object IS its frame, so `__error`/`__return` are direct field
    /// reads (what the `__last_error`/`__result` methods read). Emitted into the
    /// current block.
    fn yield_delegate_exit(&mut self, sub: Idx<Inst>) -> Result<Idx<Inst>, CompilerError> {
        let clear = self.emit_cur(Inst::Const(Lit::Nil));
        self.emit_cur(Inst::CoroSetInner(clear));
        let inner_err = self.emit_cur(Inst::Field { base: sub, field: Ustr::from("__error") });
        let nil2 = self.emit_cur(Inst::Const(Lit::Nil));
        let has_err = self.emit_cur(Inst::NotEq(inner_err, nil2));
        let raise = self.f.new_block();
        let cont = self.f.new_block();
        self.f.set_term(self.cur, Term::Branch { cond: has_err, then_bb: raise, else_bb: cont });
        self.cur = raise;
        // `error` is the Lua builtin (a global), NOT a rock fn — read it as a
        // global and call indirectly, so re-raise works with no stdlib present.
        let err_fn = self.emit_cur(Inst::Global(Ustr::from("error")));
        self.emit_cur(Inst::CallValue { callee: err_fn, args: vec![inner_err] });
        self.f.set_term(self.cur, Term::Goto(cont));
        self.cur = cont;
        Ok(self.emit_cur(Inst::Field { base: sub, field: Ustr::from("__return") }))
    }

    /// `await sub` where `sub` yields a flat enum: drive the inner via its
    /// frame-slot channel (`__advance` + the `__y*` slots on the object, which IS
    /// its frame) and re-yield the leaves through THIS coroutine's frame slots.
    /// Zero boxes on either side -- the outer transform copies `sub.__y*` into
    /// `s.__y*` at the yield point.
    fn lower_yield_delegate_flat(
        &mut self,
        sub: Idx<Inst>,
        rac: Idx<Cell>,
        shape: &EnumFlatShape,
    ) -> Result<Idx<Inst>, CompilerError> {
        let n = shape.leaf_count();
        let frame = sub;
        // Prime the inner (advance once).
        let adv0 = self.emit_cur(Inst::Field { base: sub, field: Ustr::from("__advance") });
        self.emit_cur(Inst::CallValue { callee: adv0, args: vec![sub] });

        let header = self.f.new_block();
        let body = self.f.new_block();
        let exit = self.f.new_block();
        self.f.set_term(self.cur, Term::Goto(header));

        // header: loop while the inner is still producing (`__state >= 0`).
        self.cur = header;
        let state = self.emit_cur(Inst::Field { base: frame, field: Ustr::from("__state") });
        let zero = self.emit_cur(Inst::Const(Lit::Int(0)));
        let done = self.emit_cur(Inst::IntLt(state, zero));
        self.f.set_term(self.cur, Term::Branch { cond: done, then_bb: exit, else_bb: body });

        // body: re-yield the inner's current leaves, then advance it with our arg.
        self.cur = body;
        let values: Vec<Idx<Inst>> = (0..n)
            .map(|i| self.emit_cur(Inst::Field { base: frame, field: yield_slot(i) }))
            .collect();
        let resume_bb = self.f.new_block();
        self.f.set_term(self.cur, Term::Yield { values, resume: resume_bb });
        self.cur = resume_bb;
        let arg = self.emit_cur(Inst::Load(rac));
        let adv1 = self.emit_cur(Inst::Field { base: sub, field: Ustr::from("__advance") });
        self.emit_cur(Inst::CallValue { callee: adv1, args: vec![sub, arg] });
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = exit;
        self.yield_delegate_exit(sub)
    }

    /// Resolve an operator-helper stdlib fn (`__op_eq`/`__op_ne`) bone registers
    /// under several synthetic overload entries — all the SAME `@nomangle`
    /// inline(lua) body mangling to the bare name, so any resolves identically.
    /// Picks the first (the genuine source def); deterministic by parse order.
    fn resolve_op_helper(&self, name: &str) -> Result<FuncRef, CompilerError> {
        match self
            .ws
            .functions_by_name
            .get(&Ustr::from(name))
            .and_then(|l| l.first())
        {
            Some((module, func)) => Ok(FuncRef { module: *module, func: *func }),
            None => Err(gap(format!("operator helper `{name}` not found"))),
        }
    }

    /// Resolve a global function by surface name to a [`FuncRef`], requiring a
    /// single workspace-wide definition (the `@nomangle` stdlib interp helpers —
    /// `__rock_format_float`, `inspect` — are unique, so they resolve here).
    fn resolve_global_fn(&self, name: &str) -> Result<FuncRef, CompilerError> {
        match self
            .ws
            .functions_by_name
            .get(&Ustr::from(name))
            .map(|l| l.as_slice())
        {
            Some([(module, func)]) => Ok(FuncRef { module: *module, func: *func }),
            _ => Err(gap(format!("interp helper `{name}` not uniquely resolvable"))),
        }
    }

    fn lower_native_arith(
        &mut self,
        op: &str,
        lt: ArithTy,
        rt: ArithTy,
        a: Idx<Expr>,
        b: Idx<Expr>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let lv = self.lower_expr(a)?;
        let rv = self.lower_expr(b)?;
        // Promote an int operand exactly when `arith_inst` picks a Float* variant
        // (`x + 1`, `f * i`, and `/` which is always float division): the op is
        // typed float, so an int operand is used-as-float. String concat and pure
        // int ops leave their operands untouched.
        let str_op = op == "__op_add" && (lt == ArithTy::Str || rt == ArithTy::Str);
        let to_float = !str_op && (op == "__op_div" || lt == ArithTy::Float || rt == ArithTy::Float);
        let lv = self.coerce_i2f(lv, lt == ArithTy::Int, to_float);
        let rv = self.coerce_i2f(rv, rt == ArithTy::Int, to_float);
        let inst = arith_inst(op, lt, rt, lv, rv)?;
        Ok(self.emit_cur(inst))
    }

    fn lower_binop(
        &mut self,
        op: BinOp,
        lhs: Idx<Expr>,
        rhs: Idx<Expr>,
    ) -> Result<Idx<Inst>, CompilerError> {
        match op {
            BinOp::Lt | BinOp::Leq | BinOp::Gt | BinOp::Geq => {
                // Lua `<`/`>`/... orders numbers AND strings (lexicographic), and
                // the typed Int*/Float* variants render the SAME operator — so a
                // string or generic-`T` comparison uses the non-float variant.
                // bone has type-checked both sides to the same orderable type
                // (rock emits a native compare here too). A genuinely unorderable
                // operand would be a type error upstream.
                let lf = self.arith_ty(lhs);
                let rf = self.arith_ty(rhs);
                let float = lf == Some(ArithTy::Float) || rf == Some(ArithTy::Float);
                let lv = self.lower_expr(lhs)?;
                let rv = self.lower_expr(rhs)?;
                // A mixed float compare (`x > 0`, `i < f`) promotes its int side --
                // both Float* operands must arrive float (GLSL ES has no implicit
                // int->float in a compare).
                let lv = self.coerce_i2f(lv, lf == Some(ArithTy::Int), float);
                let rv = self.coerce_i2f(rv, rf == Some(ArithTy::Int), float);
                let inst = match (op, float) {
                    (BinOp::Lt, false) => Inst::IntLt(lv, rv),
                    (BinOp::Lt, true) => Inst::FloatLt(lv, rv),
                    (BinOp::Leq, false) => Inst::IntLeq(lv, rv),
                    (BinOp::Leq, true) => Inst::FloatLeq(lv, rv),
                    (BinOp::Gt, false) => Inst::IntGt(lv, rv),
                    (BinOp::Gt, true) => Inst::FloatGt(lv, rv),
                    (BinOp::Geq, false) => Inst::IntGeq(lv, rv),
                    (BinOp::Geq, true) => Inst::FloatGeq(lv, rv),
                    _ => unreachable!(),
                };
                Ok(self.emit_cur(inst))
            }
            BinOp::Eq | BinOp::NotEq => {
                // `x == null` / `x != null` on a flat enum: the tag slot is the null
                // discriminator (nil tag == null). A threaded value has no box to
                // compare, and materializing one would compare a table to nil
                // (always false) -- so compare the tag directly.
                let null_side = if self.is_null_lit(rhs) {
                    Some(lhs)
                } else if self.is_null_lit(lhs) {
                    Some(rhs)
                } else {
                    None
                };
                if let Some(v) = null_side {
                    // A `V?` place read here is read for its PRESENCE: the copy
                    // such a read otherwise takes would allocate a cdata per nil
                    // test (`mouse_screen`'s `sim_mouse_override`, every frame).
                    if self.nobox_value_name(v).is_none()
                        && !self.expr_is_flat_enum(v)
                        && self.nullable_place_value_of(v).is_some()
                    {
                        let outer = self.presence_only.replace(v);
                        let value = self.lower_expr(v);
                        self.presence_only = outer;
                        let value = value?;
                        let nil = self.emit_cur(Inst::Const(Lit::Nil));
                        return Ok(self.emit_cur(if matches!(op, BinOp::Eq) {
                            Inst::Eq(value, nil)
                        } else {
                            Inst::NotEq(value, nil)
                        }));
                    }
                    if self.nobox_value_name(v).is_some() {
                        let repr = self.lower_expr_repr(v)?;
                        let present = match repr {
                            ValueRepr::Group {
                                present: Some(value),
                                ..
                            } => value,
                            ValueRepr::Group { present: None, .. } => {
                                self.emit_cur(Inst::Const(Lit::Bool(true)))
                            }
                            other => {
                                let value = self.pack(other);
                                let nil = self.emit_cur(Inst::Const(Lit::Nil));
                                self.emit_cur(Inst::NotEq(value, nil))
                            }
                        };
                        return Ok(if matches!(op, BinOp::Eq) {
                            self.emit_cur(Inst::Not(present))
                        } else {
                            present
                        });
                    }
                    // The tuple twin of the nobox-value case above: a `(..)?`
                    // whose element splats carries its own presence SLOT, and
                    // that slot is the answer. Packing instead would build a
                    // `MakeTuple` -- a table, never nil -- so `== null` would be
                    // false even on the absent arm.
                    if self.nullable_tuple_call_shape(v).is_some() {
                        let repr = self.lower_expr_repr(v)?;
                        let present = match repr {
                            ValueRepr::Tuple {
                                present: Some(value),
                                ..
                            } => value,
                            other => {
                                let value = self.pack(other);
                                let nil = self.emit_cur(Inst::Const(Lit::Nil));
                                self.emit_cur(Inst::NotEq(value, nil))
                            }
                        };
                        return Ok(if matches!(op, BinOp::Eq) {
                            self.emit_cur(Inst::Not(present))
                        } else {
                            present
                        });
                    }
                    if self.expr_is_flat_enum(v) {
                        let tag = match self.lower_expr_repr(v)? {
                            ValueRepr::Group { parts, .. } => {
                                self.pack(parts.into_iter().next().expect("flat enum group has a tag leaf"))
                            }
                            // a boxed flat enum: nil-or-table, compare directly.
                            other => self.pack(other),
                        };
                        let nil = self.emit_cur(Inst::Const(Lit::Nil));
                        return Ok(self.emit_cur(if matches!(op, BinOp::Eq) {
                            Inst::Eq(tag, nil)
                        } else {
                            Inst::NotEq(tag, nil)
                        }));
                    }
                }
                if let (Some(ln), Some(rn)) =
                    (self.nobox_value_name(lhs), self.nobox_value_name(rhs))
                {
                    if ln == rn {
                        let mut shape = value_shape_of_nominal(self.ws, ln)
                            .ok_or_else(|| gap(format!("nobox value `{ln}` has no scalar shape")))?;
                        shape.nullable =
                            self.expr_is_nullable(lhs) || self.expr_is_nullable(rhs);
                        let lhs_repr = self.lower_expr_repr(lhs)?;
                        let lhs_leaves = self.repr_to_leaves(&lhs_repr, &shape);
                        let rhs_repr = self.lower_expr_repr(rhs)?;
                        let rhs_leaves = self.repr_to_leaves(&rhs_repr, &shape);
                        let equal = self.emit_all_equal(&lhs_leaves, &rhs_leaves);
                        return Ok(if matches!(op, BinOp::Eq) {
                            equal
                        } else {
                            self.emit_cur(Inst::Not(equal))
                        });
                    }
                }
                let scalar = self.eq_is_scalar(lhs)
                    || self.eq_is_scalar(rhs)
                    || self.is_null_lit(lhs)
                    || self.is_null_lit(rhs);
                let lv = self.lower_expr(lhs)?;
                let rv = self.lower_expr(rhs)?;
                if !scalar {
                    // Non-scalar equality is STRUCTURAL, not Lua reference
                    // equality: route through the stdlib `__op_eq`/`__op_ne`,
                    // which walk fields structurally via `pairs()` (cdata value
                    // structs hit the `==` primitive fast path = byte-value
                    // compare).
                    let name = if matches!(op, BinOp::Eq) { "__op_eq" } else { "__op_ne" };
                    let func = self.resolve_op_helper(name)?;
                    return Ok(self.emit_cur(Inst::Call { func, args: vec![lv, rv] }));
                }
                // Mixed `float == int` promotes the int side (untyped `Eq` renders a
                // single `==`; both operands must arrive same-typed for GLSL).
                let lf = self.arith_ty(lhs);
                let rf = self.arith_ty(rhs);
                let float = lf == Some(ArithTy::Float) || rf == Some(ArithTy::Float);
                let lv = self.coerce_i2f(lv, lf == Some(ArithTy::Int), float);
                let rv = self.coerce_i2f(rv, rf == Some(ArithTy::Int), float);
                Ok(self.emit_cur(if matches!(op, BinOp::Eq) {
                    Inst::Eq(lv, rv)
                } else {
                    Inst::NotEq(lv, rv)
                }))
            }
            BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr => {
                let lv = self.lower_expr(lhs)?;
                let rv = self.lower_expr(rhs)?;
                let inst = match op {
                    BinOp::BitAnd => Inst::BitAnd(lv, rv),
                    BinOp::BitOr => Inst::BitOr(lv, rv),
                    BinOp::BitXor => Inst::BitXor(lv, rv),
                    BinOp::Shl => Inst::Shl(lv, rv),
                    BinOp::Shr => Inst::Shr(lv, rv),
                    _ => unreachable!(),
                };
                Ok(self.emit_cur(inst))
            }
            // Short-circuit operators: the rhs may not evaluate, so they lower
            // to branches with the result flowing through a cell.
            BinOp::And => self.lower_and(lhs, rhs),
            BinOp::Or => self.lower_or(lhs, rhs),
            BinOp::NullCoalesce => self.lower_nullcoalesce(lhs, rhs),
        }
    }

    /// `if c { a } else { b }` as a value: branch, store each arm's value into a
    /// shared result **cell**, load it in the join. No block params (P1). A
    /// no-`else` arm yields unit. An arm that diverges (returns/breaks) skips its
    /// store + join-goto.
    fn lower_if(
        &mut self,
        cond: Idx<Expr>,
        then_branch: Idx<Expr>,
        else_branch: Option<Idx<Expr>>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let repr =
            self.lower_if_repr(cond, then_branch, else_branch, RetSlot::Opaque)?;
        Ok(self.pack(repr))
    }

    fn lower_if_repr(
        &mut self,
        cond: Idx<Expr>,
        then_branch: Idx<Expr>,
        else_branch: Option<Idx<Expr>>,
        shape: RetSlot,
    ) -> Result<ValueRepr, CompilerError> {
        let results: Vec<Idx<Cell>> =
            (0..shape.slot_count()).map(|_| self.f.new_cell(None)).collect();
        let cond_v = self.lower_expr(cond)?;
        let then_bb = self.f.new_block();
        let else_bb = self.f.new_block();
        let join = self.f.new_block();
        self.f.set_term(
            self.cur,
            Term::Branch {
                cond: cond_v,
                then_bb,
                else_bb,
            },
        );

        self.cur = then_bb;
        let tv = self.lower_expr_repr(then_branch)?;
        self.store_repr_and_join(&results, &shape, tv, join)?;

        self.cur = else_bb;
        let ev = match else_branch {
            Some(eb) => self.lower_expr_repr(eb)?,
            None => ValueRepr::Scalar(self.emit_cur(Inst::Const(Lit::Nil))),
        };
        self.store_repr_and_join(&results, &shape, ev, join)?;

        self.cur = join;
        let slots: Vec<Idx<Inst>> =
            results.iter().map(|&cell| self.emit_cur(Inst::Load(cell))).collect();
        let mut idx = 0;
        Ok(self.slot_repr(&shape, &slots, &mut idx))
    }

    /// `if` in STATEMENT position (value discarded): branch + arms-as-statements
    /// + join, with NO result cell / store / load. Arms recurse statement-position
    /// so nested if/block/match also skip their result cells.
    fn lower_if_stmt(
        &mut self,
        cond: Idx<Expr>,
        then_branch: Idx<Expr>,
        else_branch: Option<Idx<Expr>>,
    ) -> Result<(), CompilerError> {
        let cond_v = self.lower_expr(cond)?;
        let then_bb = self.f.new_block();
        let else_bb = self.f.new_block();
        let join = self.f.new_block();
        self.f.set_term(self.cur, Term::Branch { cond: cond_v, then_bb, else_bb });

        self.cur = then_bb;
        self.lower_stmt_expr(then_branch)?;
        if self.is_open() {
            self.f.set_term(self.cur, Term::Goto(join));
        }

        self.cur = else_bb;
        if let Some(eb) = else_branch {
            self.lower_stmt_expr(eb)?;
        }
        if self.is_open() {
            self.f.set_term(self.cur, Term::Goto(join));
        }

        self.cur = join;
        Ok(())
    }

    /// If the current block is still open, store `value` into `result` and jump
    /// to `join` (the fall-through merge). A diverged arm leaves it untouched.
    fn store_and_join(&mut self, result: Idx<Cell>, value: Idx<Inst>, join: Idx<Block>) {
        if self.is_open() {
            self.emit_cur(Inst::Store {
                cell: result,
                value,
            });
            self.f.set_term(self.cur, Term::Goto(join));
        }
    }

    fn store_repr_and_join(
        &mut self,
        results: &[Idx<Cell>],
        shape: &RetSlot,
        value: ValueRepr,
        join: Idx<Block>,
    ) -> Result<(), CompilerError> {
        if !self.is_open() {
            return Ok(());
        }
        let slots = self.repr_to_slot_values(value, shape);
        if results.len() != slots.len() {
            return Err(gap(format!(
                "aggregate join produced {} slots for {} result cells",
                slots.len(),
                results.len()
            )));
        }
        for (&cell, value) in results.iter().zip(slots) {
            self.emit_cur(Inst::Store { cell, value });
        }
        self.f.set_term(self.cur, Term::Goto(join));
        Ok(())
    }

    /// `a && b` == `if a then b else false`.
    fn lower_and(&mut self, lhs: Idx<Expr>, rhs: Idx<Expr>) -> Result<Idx<Inst>, CompilerError> {
        let result = self.f.new_cell(None);
        let lv = self.lower_expr(lhs)?;
        let rhs_bb = self.f.new_block();
        let else_bb = self.f.new_block();
        let join = self.f.new_block();
        self.f.set_term(
            self.cur,
            Term::Branch {
                cond: lv,
                then_bb: rhs_bb,
                else_bb,
            },
        );
        self.cur = rhs_bb;
        let rv = self.lower_expr(rhs)?;
        self.store_and_join(result, rv, join);
        self.cur = else_bb;
        let f = self.emit_cur(Inst::Const(Lit::Bool(false)));
        self.store_and_join(result, f, join);
        self.cur = join;
        Ok(self.emit_cur(Inst::Load(result)))
    }

    /// `a || b` == `if a then true else b`.
    fn lower_or(&mut self, lhs: Idx<Expr>, rhs: Idx<Expr>) -> Result<Idx<Inst>, CompilerError> {
        let result = self.f.new_cell(None);
        let lv = self.lower_expr(lhs)?;
        let then_bb = self.f.new_block();
        let rhs_bb = self.f.new_block();
        let join = self.f.new_block();
        self.f.set_term(
            self.cur,
            Term::Branch {
                cond: lv,
                then_bb,
                else_bb: rhs_bb,
            },
        );
        self.cur = then_bb;
        let t = self.emit_cur(Inst::Const(Lit::Bool(true)));
        self.store_and_join(result, t, join);
        self.cur = rhs_bb;
        let rv = self.lower_expr(rhs)?;
        self.store_and_join(result, rv, join);
        self.cur = join;
        Ok(self.emit_cur(Inst::Load(result)))
    }

    /// `a ?? b` == `if a == nil then b else a` — `a` evaluated once.
    fn lower_nullcoalesce(
        &mut self,
        lhs: Idx<Expr>,
        rhs: Idx<Expr>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let result = self.f.new_cell(None);
        let lv = self.lower_expr(lhs)?;
        let nil = self.emit_cur(Inst::Const(Lit::Nil));
        let is_nil = self.emit_cur(Inst::Eq(lv, nil));
        let rhs_bb = self.f.new_block();
        let lhs_bb = self.f.new_block();
        let join = self.f.new_block();
        self.f.set_term(
            self.cur,
            Term::Branch {
                cond: is_nil,
                then_bb: rhs_bb,
                else_bb: lhs_bb,
            },
        );
        self.cur = rhs_bb;
        let rv = self.lower_expr(rhs)?;
        self.store_and_join(result, rv, join);
        // lhs non-nil: reuse `lv` (its defining block dominates this one).
        self.cur = lhs_bb;
        self.store_and_join(result, lv, join);
        self.cur = join;
        Ok(self.emit_cur(Inst::Load(result)))
    }

    fn loop_target(&self, what: &str) -> Result<(Idx<Block>, Idx<Block>), CompilerError> {
        self.loop_targets
            .last()
            .copied()
            .ok_or_else(|| gap(format!("`{what}` outside a loop")))
    }

    /// `while cond { body }`: header (test) -> body -> back-edge to header; exit
    /// on false. `continue` -> header, `break` -> exit.
    /// Returns true when the statement TERMINATES the enclosing list: a
    /// `while true` (constant cond) whose exit no `break` ever targeted -- its
    /// fall-through is unreachable, so the caller must not lower trailing
    /// statements into it (and a co fn body ending in one is not a fall-off).
    fn lower_while(&mut self, cond: Idx<Expr>, body: Idx<Expr>) -> Result<bool, CompilerError> {
        let header = self.f.new_block();
        let body_bb = self.f.new_block();
        let exit = self.f.new_block();
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = header;
        let const_true = matches!(self.body.expr_arena[cond], Expr::Bool(true));
        if const_true {
            // `while true`: no branch to exit -- exit is reachable only via
            // `break`, which is exactly what `used_break_targets` records.
            self.f.set_term(self.cur, Term::Goto(body_bb));
        } else {
            let cond_v = self.lower_expr(cond)?;
            self.f.set_term(
                self.cur,
                Term::Branch {
                    cond: cond_v,
                    then_bb: body_bb,
                    else_bb: exit,
                },
            );
        }

        self.cur = body_bb;
        self.loop_targets.push((header, exit));
        self.lower_stmt_expr(body)?;
        self.loop_targets.pop();
        if self.is_open() {
            self.f.set_term(self.cur, Term::Goto(header)); // back-edge
        }

        let diverges = const_true && !self.used_break_targets.contains(&exit);
        self.cur = exit;
        if diverges {
            // Unreachable: give the dead exit a harmless terminator so every
            // block ends terminated even though nothing jumps here.
            self.f.set_term(self.cur, Term::Goto(header));
        }
        Ok(diverges)
    }

    fn lower_for(
        &mut self,
        vars: BindingPattern,
        iter: Idx<Expr>,
        body: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        if let Expr::Range { low, high } = self.body.expr_arena[iter].clone() {
            return match vars {
                BindingPattern::Leaf(var_b) => self.lower_for_range(var_b, low, high, body),
                BindingPattern::Tuple(_) => Err(gap("for-range with a tuple pattern")),
            };
        }
        // User iterator protocol (bone recorded the resolution for this For):
        // desugar to `it = iter(coll); loop { (ok, ..) = next(it); !ok -> break }`.
        // Only populated for non-builtin collections, so this never shadows a
        // builtin template. `iter_fn: None` = identity (the collection IS the
        // iterator); a `CallerGeneric` next = placeholder in a bounded body.
        if let Some(info) = self.inf.and_then(|i| i.for_iter_protocol.get(&iter).cloned()) {
            return self.lower_for_user_iter(&vars, iter, body, &info);
        }
        if let Expr::Call {
            callee: Callee::Unresolved { name, .. },
            args,
        } = self.body.expr_arena[iter].clone()
        {
            if name.as_str() == "enumerate" {
                let av: Vec<Idx<Expr>> = args.into_iter().collect();
                if let [coll] = av[..] {
                    if self.iter_is_arena(coll) {
                        return self.lower_for_arena(&vars, coll, body, true);
                    }
                    return self.lower_for_enumerate(&vars, coll, body);
                }
            }
            // A non-enumerate call returning a collection: fall through to the
            // collection dispatch below. `lower_for_*` lower the iter once into a
            // value, so the call runs exactly once (no per-iteration re-eval).
        }
        if self.iter_is_arena(iter) {
            // Direct `for .. in arena` arrives with `for_iter_protocol`
            // populated (bone routes Arena to the stdlib ArenaPairIter); only
            // `enumerate(arena)` still uses the template above.
            return Err(gap(
                "for over Arena must route through the iterator protocol (for_iter_protocol missing)",
            ));
        }
        // FlatVec iterates its `buf` storage bounded by the `len` field;
        // `lower_for_array` owns that special-case. Route here so `coll_kind`
        // stays closed over Vec/Map/NativeBuffer (it can't classify FlatVec).
        if self.expr_is_flat_vec(iter) {
            return self.lower_for_array(vars, iter, body);
        }
        match self.coll_kind(iter) {
            // `for x in [T]` always arrives with `for_iter_protocol` populated
            // (bone routes Array to the stdlib VecIter protocol); reaching the
            // dispatch means bone and skull disagree -- reject, don't emit a
            // second lowering. `enumerate(vec)` keeps its own path above.
            // Either Vec flavor: columns change storage, not the iteration
            // protocol -- `VecIter.next` reads `it.vec[it.i]`, which is an
            // `Index` and picks up the column kind on its own.
            ContainerKind::Vec | ContainerKind::VecValueColumns(_) => Err(gap(
                "for over [T] must route through the iterator protocol (for_iter_protocol missing)",
            )),
            ContainerKind::NativeBuffer | ContainerKind::NativeBufferValue(_) => {
                self.lower_for_array(vars, iter, body)
            }
            ContainerKind::Map => self.lower_for_map(&vars, iter, body),
            // Synthetic: a leaf column and a leaf VIEW are both minted during
            // element lowering and are never user types, so neither can be a
            // `for` head.
            ContainerKind::VecColumn => Err(gap("for over a synthetic Vec leaf column")),
            ContainerKind::NativeBufferLeaf => Err(gap("for over a synthetic NativeBuffer leaf view")),
        }
    }

    /// `for <pat> in <coll>` over a user type via the iter/next protocol (bone
    /// recorded the resolution). Desugars to:
    ///   it = iter(coll)        -- or `it = coll` when iter is identity
    ///   loop { let (ok, <pat>) = next(it); if !ok break; <body> }
    /// `next` returns a tuple, so slice-1's multi-return ABI makes the per-element
    /// `next` call zero-alloc (a `CallMulti` into `[ok, elem..]` dests). `it` lives
    /// in a cell so `next` mutates the same cursor each turn. `continue` re-enters
    /// the header (runs `next` again); `break` exits.
    ///
    /// A `Concrete` next with a non-empty bound tree is a combinator-chain root:
    /// the tree is recorded against the emitted `CallMulti` so the inliner can
    /// substitute the placeholders inside the spliced callee. A `CallerGeneric`
    /// next (a `for` over a bounded generic inside a bounded body) emits the
    /// placeholder [`Inst::CallBoundMulti`] directly.
    fn lower_for_user_iter(
        &mut self,
        vars: &BindingPattern,
        iter_expr: Idx<Expr>,
        body: Idx<Expr>,
        info: &ForIterInfo,
    ) -> Result<(), CompilerError> {
        let coll = self.lower_expr(iter_expr)?;
        let it_val = match info.iter_fn {
            Some((m, f)) => {
                self.emit_cur(Inst::Call { func: FuncRef { module: m, func: f }, args: vec![coll] })
            }
            None => coll,
        };
        let it_cell = self.f.new_cell(Some(Ustr::from("__it")));
        self.emit_cur(Inst::Store { cell: it_cell, value: it_val });

        // next returns `(bool, E)` / `(bool, A, B, ...)`. The shape is read under
        // the SPECIALIZATION the emitted call will carry, not the caller's own:
        // when the spec is dropped the call reaches the shared generic body,
        // whose `E` is unbound and therefore never splats -- so the two sides
        // agree about how many values come back.
        let spec = match &info.next {
            BoundNext::Concrete { func: (m, f), next_generics, .. } => self
                .next_call_spec(FuncRef { module: *m, func: *f }, next_generics)
                .filter(|k| !self.in_unspliced_ctx || spec_key_has_column_elem(self.ws, k)),
            BoundNext::CallerGeneric(_) => None,
        };
        // A placeholder next is always `(bool, E)` with `E` opaque (bound
        // conformance requires exactly that shape, and the placeholder body is
        // substituted later).
        let ret_shape = match &info.next {
            BoundNext::Concrete { func: (m, f), .. } => {
                let func = FuncRef { module: *m, func: *f };
                let subst = spec
                    .as_ref()
                    .map(|k| spec_subst_from_key(self.ws, func, k))
                    .unwrap_or_default();
                self.ws.module(*m).functions[*f]
                    .return_ty
                    .and_then(|t| tuple_ret_shape(self.ws, &subst, t))
                    .ok_or_else(|| gap("for-iter `next` must return a tuple"))?
            }
            BoundNext::CallerGeneric(_) => TupleRetShape {
                nullable: false,
                slots: vec![RetSlot::Opaque, RetSlot::Opaque],
            },
        };
        if ret_shape.slots.len() < 2 {
            return Err(gap("for-iter `next` must return `(bool, E)`"));
        }
        let n = ret_shape.slot_count();

        let header = self.f.new_block();
        let body_bb = self.f.new_block();
        let exit = self.f.new_block();
        self.f.for_iter_headers.push(header);
        self.f.set_term(self.cur, Term::Goto(header));

        // header: (ok, elem..) = next(it); branch on ok.
        self.cur = header;
        let it_load = self.emit_cur(Inst::Load(it_cell));
        let dests: Vec<Idx<Cell>> = (0..n).map(|_| self.f.new_cell(None)).collect();
        match &info.next {
            BoundNext::Concrete { func: (m, f), sub, .. } => {
                let func = FuncRef { module: *m, func: *f };
                // The synthesized `next` call has no expr to key
                // `call_generic_bindings` by, so its instantiation comes from
                // bone's recorded `next_generics`. Without this the inliner
                // splices the SHARED generic snapshot, whose element access was
                // lowered against the boxed representation -- which reads a
                // `_data` that column storage does not have.
                let call = self.emit_cur(Inst::CallMulti {
                    dests: dests.clone(),
                    func,
                    args: vec![it_load],
                });
                if !sub.is_empty() {
                    self.f.bound_trees.insert(call, sub.clone());
                }
                if let Some(key) = spec {
                    self.f.spec_bindings.insert(call, key);
                }
            }
            BoundNext::CallerGeneric(gidx) => {
                self.emit_cur(Inst::CallBoundMulti {
                    dests: dests.clone(),
                    generic_idx: *gidx as u32,
                    args: vec![it_load],
                });
            }
        }
        let ok = self.emit_cur(Inst::Load(dests[0]));
        self.f.set_term(self.cur, Term::Branch { cond: ok, then_bb: body_bb, else_bb: exit });

        // body: bind the pattern to the element slots. One element binds
        // directly (its leaves, if it splats); multiple (`next -> (bool, A, B)`)
        // are a tuple `bind_pattern_slot` destructures against a matching tuple
        // pattern (no alloc).
        self.cur = body_bb;
        let loads: Vec<Idx<Inst>> =
            dests[1..].iter().map(|c| self.emit_cur(Inst::Load(*c))).collect();
        let elem_slot = match &ret_shape.slots[1..] {
            [one] => one.clone(),
            rest => RetSlot::Nested(TupleRetShape { slots: rest.to_vec(), nullable: false }),
        };
        let mut idx = 0;
        self.bind_pattern_slot(vars, &elem_slot, &loads, &mut idx);
        self.loop_targets.push((header, exit));
        self.lower_stmt_expr(body)?;
        self.loop_targets.pop();
        if self.is_open() {
            self.f.set_term(self.cur, Term::Goto(header));
        }
        self.cur = exit;
        Ok(())
    }

    /// The variant names of `e`'s enum type (empty if `e` is not an enum) — used
    /// to recognize an unqualified nullary-variant pattern at a match root.
    fn scrut_variants(&self, e: Idx<Expr>) -> HashSet<Ustr> {
        if let Some(TypeExpr::App { ctor: TypeCtor::Nominal(name), .. }) = self.expr_ty(e) {
            if let Some(&eid) = self.ws.enums_by_name.get(&name) {
                let ed = &self.ws.modules[eid.module.index()].enums[eid.def];
                return ed.variants.iter().map(|v| v.name).collect();
            }
        }
        HashSet::new()
    }

    /// The enum NAME of a scrutinee whose static type is an enum (`None` else) --
    /// threaded into `match_pattern`/`test_tag` so a payload-free enum's tag test
    /// can int-compare the ordinal instead of reading `.__variant`.
    fn scrut_enum_name(&self, e: Idx<Expr>) -> Option<Ustr> {
        match self.expr_ty(e) {
            Some(TypeExpr::App { ctor: TypeCtor::Nominal(name), .. })
                if self.ws.enums_by_name.contains_key(&name) =>
            {
                Some(name)
            }
            _ => None,
        }
    }

    /// Whether `e`'s inferred type is a transparent newtype nominal.
    fn expr_is_newtype(&self, e: Idx<Expr>) -> bool {
        matches!(
            self.expr_ty(e),
            Some(TypeExpr::App { ctor: TypeCtor::Nominal(name), .. })
                if self.ws.newtypes_by_name.contains_key(&name)
        )
    }

    /// Whether `iter`'s inferred type is an `Arena<T>` nominal.
    fn iter_is_arena(&self, iter: Idx<Expr>) -> bool {
        matches!(
            self.expr_ty(iter),
            Some(TypeExpr::App { ctor: TypeCtor::Nominal(name), .. }) if name.as_str() == "Arena"
        )
    }

    /// `for (e, v) in arena` / `for item in arena` (optionally `enumerate(arena)`)
    /// over the stdlib `Arena { entries: Vec<ArenaEntry>, .. }`: a numeric walk
    /// over `entries` skipping nil / unoccupied slots, yielding the `(Entity,
    /// data)` pair (two vars bind them apart; one var binds the 2-tuple). Mirrors
    /// rock-codegen's `IterKind::Arena`. `enumerate` binds the FIRST var to a
    /// 0-based counter advancing per yielded (occupied) element.
    fn lower_for_arena(
        &mut self,
        vars: &BindingPattern,
        iter: Idx<Expr>,
        body: Idx<Expr>,
        enumerate: bool,
    ) -> Result<(), CompilerError> {
        // Resolve the (optional enum index var, the entity/value pattern).
        let (enum_var, pat): (Option<Idx<BindingInfo>>, BindingPattern) = if enumerate {
            match vars {
                BindingPattern::Tuple(subs) if subs.len() == 2 => match &subs[0] {
                    BindingPattern::Leaf(i) => (Some(*i), subs[1].clone()),
                    _ => return Err(gap("enumerate(arena) binds (index, element)")),
                },
                _ => return Err(gap("enumerate(arena) binds a (index, element) tuple")),
            }
        } else {
            (None, vars.clone())
        };
        let single = matches!(pat, BindingPattern::Leaf(_));
        let leaves = match &pat {
            BindingPattern::Tuple(subs) if subs.len() == 2 => None,
            BindingPattern::Leaf(_) => Some(()),
            _ => return Err(gap("for-over-Arena binds one (item) or two (entity, value)")),
        };
        let _ = leaves;

        // The element type, when it is column-eligible: under bookkeeping SoA the
        // arena's `data` is an ordinary `[T]`, so at an all-scalar value `T` it is
        // COLUMN-stored and a bare `Index` would read a table that isn't there.
        let column_elem = self.arena_column_elem_of(iter);

        let arena = self.lower_expr(iter)?;
        let occupied = self.emit_cur(Inst::Field { base: arena, field: Ustr::from("occupied") });
        let generations = self.emit_cur(Inst::Field { base: arena, field: Ustr::from("generations") });
        let data = self.emit_cur(Inst::Field { base: arena, field: Ustr::from("data") });
        let n = self.emit_cur(Inst::Len { base: occupied, kind: ContainerKind::Vec });
        let i_cell = self.f.new_cell(None);
        self.f.mark_loop_binding(i_cell);
        let zero = self.emit_cur(Inst::Const(Lit::Int(0)));
        self.emit_cur(Inst::Store { cell: i_cell, value: zero });
        let enum_cell = enum_var.map(|_| {
            let c = self.f.new_cell(None);
            let z = self.emit_cur(Inst::Const(Lit::Int(0)));
            self.emit_cur(Inst::Store { cell: c, value: z });
            c
        });

        let header = self.f.new_block();
        let body_bb = self.f.new_block();
        let occ_bb = self.f.new_block();
        let do_bb = self.f.new_block();
        let latch = self.f.new_block();
        let exit = self.f.new_block();
        self.f.set_term(self.cur, Term::Goto(header));

        // header: i < n ?
        self.cur = header;
        let i = self.emit_cur(Inst::Load(i_cell));
        let c = self.emit_cur(Inst::IntLt(i, n));
        self.f.set_term(self.cur, Term::Branch { cond: c, then_bb: body_bb, else_bb: exit });

        // body: nothing to skip before the occupancy read -- the walk is bounded
        // by the occupancy column's own length, and a `[bool]` column has no
        // holes, so the old nil test had nothing left to catch.
        self.cur = body_bb;
        let idx = self.emit_cur(Inst::Load(i_cell));
        self.f.set_term(self.cur, Term::Goto(occ_bb));

        // occ: skip unoccupied.
        self.cur = occ_bb;
        let occ = self.emit_cur(Inst::Index { base: occupied, index: idx, kind: ContainerKind::Vec });
        self.f.set_term(self.cur, Term::Branch { cond: occ, then_bb: do_bb, else_bb: latch });

        // do: build Entity, bind, run body.
        self.cur = do_bb;
        // A slot's index IS its position, so the loop counter is the Entity's
        // `_index` and only the generation needs a column read. The built
        // value is an `Entity` whose fields are `_`-prefixed (soft-private).
        // `Entity` is a `value` struct (GC-carrying tier): build it as a
        // `MakeValueStruct` so it scalarizes (a non-escaping `for (e, _)` scan
        // builds no table) and, where it escapes, materializes through
        // `Entity_vt` -- carrying the `__ty` tag + the value-`__eq` metatable.
        // A raw `MakeStruct` here would leak an un-tagged, metatable-less table
        // whose `==` silently mis-compares (arena-scan entities not equal to
        // pairs-scan entities).
        let e_gen =
            self.emit_cur(Inst::Index { base: generations, index: idx, kind: ContainerKind::Vec });
        let entity = self.emit_cur(Inst::MakeValueStruct {
            name: Ustr::from("Entity"),
            fields: vec![
                (Ustr::from("_arena"), arena),
                (Ustr::from("_index"), idx),
                (Ustr::from("_generation"), e_gen),
            ],
        });
        // A column element is rebuilt from its leaves here. For an ordinary value
        // struct SROA folds the field reads back to the leaf cells, so the pack
        // costs nothing; for a `nobox` element it is an implicit materialization
        // and the verifier rejects it by name rather than emitting a quiet box.
        let data = match column_elem {
            Some(elem) => {
                let repr = self.column_element_repr(data, idx, ColumnElem::plain(elem))?;
                self.pack(repr)
            }
            None => self.emit_cur(Inst::Index { base: data, index: idx, kind: ContainerKind::Vec }),
        };
        match &pat {
            BindingPattern::Tuple(subs) if subs.len() == 2 => {
                self.bind_for_pattern(&subs[0], entity);
                self.bind_for_pattern(&subs[1], data);
            }
            _ if single => {
                let tup = self.emit_cur(Inst::MakeTuple(vec![entity, data]));
                self.bind_for_pattern(&pat, tup);
            }
            _ => return Err(gap("for-over-Arena binds one (item) or two (entity, value)")),
        }
        // enumerate counter: bind to the current count, then bump (occupied only).
        if let (Some(ev), Some(ec)) = (enum_var, enum_cell) {
            let cur = self.emit_cur(Inst::Load(ec));
            let bind_cell = self.f.new_cell(Some(self.body.binding_arena[ev].name));
            self.binds.insert(raw(ev), bind_cell);
            self.emit_cur(Inst::Store { cell: bind_cell, value: cur });
            let one = self.emit_cur(Inst::Const(Lit::Int(1)));
            let inc = self.emit_cur(Inst::IntAdd(cur, one));
            self.emit_cur(Inst::Store { cell: ec, value: inc });
        }
        self.loop_targets.push((latch, exit));
        self.lower_stmt_expr(body)?;
        self.loop_targets.pop();
        if self.is_open() {
            self.f.set_term(self.cur, Term::Goto(latch));
        }

        // latch: i += 1.
        self.cur = latch;
        let i2 = self.emit_cur(Inst::Load(i_cell));
        let one = self.emit_cur(Inst::Const(Lit::Int(1)));
        let inc = self.emit_cur(Inst::IntAdd(i2, one));
        self.emit_cur(Inst::Store { cell: i_cell, value: inc });
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = exit;
        Ok(())
    }

    /// `for (k, v) in m` / `for kv in m`: Lua stateless `next` iteration. The key
    /// cell is the cursor; a nil key after `MapNext` ends the loop.
    fn lower_for_map(
        &mut self,
        vars: &BindingPattern,
        iter: Idx<Expr>,
        body: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        // A column-stored Map iterates its SPINE: the walk yields (key, slot),
        // and the value is read leaf-wise out of `data` at that slot. No element
        // table is touched, because none exists.
        let col = self.map_column_value_of(iter);
        let m = self.lower_expr(iter)?;
        let table = match col {
            Some(_) => self.emit_cur(Inst::Field { base: m, field: Ustr::from("spine") }),
            None => m,
        };
        let key_cell = self.f.new_cell(None);
        let val_cell = self.f.new_cell(None);
        self.f.mark_loop_binding(key_cell);
        self.f.mark_loop_binding(val_cell);
        let nil = self.emit_cur(Inst::Const(Lit::Nil));
        self.emit_cur(Inst::Store {
            cell: key_cell,
            value: nil,
        });

        let header = self.f.new_block();
        let body_bb = self.f.new_block();
        let exit = self.f.new_block();
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = header;
        self.emit_cur(Inst::MapNext {
            key_cell,
            val_cell,
            table,
        });
        let k = self.emit_cur(Inst::Load(key_cell));
        let nil2 = self.emit_cur(Inst::Const(Lit::Nil));
        let done = self.emit_cur(Inst::Eq(k, nil2));
        self.f.set_term(
            self.cur,
            Term::Branch {
                cond: done,
                then_bb: exit,
                else_bb: body_bb,
            },
        );

        self.cur = body_bb;
        let vv = match col {
            Some(elem) => {
                let slot = self.emit_cur(Inst::Load(val_cell));
                let data = self.map_col_data(m);
                let repr = self.column_element_repr(data, slot, elem)?;
                Some(self.pack(repr))
            }
            None => None,
        };
        self.bind_map_entry(vars, key_cell, val_cell, vv)?;
        self.loop_targets.push((header, exit)); // continue -> header (re-runs next)
        self.lower_stmt_expr(body)?;
        self.loop_targets.pop();
        if self.is_open() {
            self.f.set_term(self.cur, Term::Goto(header));
        }

        self.cur = exit;
        Ok(())
    }

    /// `for (i, (k, v)) in enumerate(m)`: Lua stateless `next` iteration (like
    /// `lower_for_map`) with a 0-based counter cell bound to `i`, incremented in
    /// the latch so `continue` still steps it. `xpat` binds the `(k, v)` entry.
    fn lower_for_map_enumerate(
        &mut self,
        ib: Idx<BindingInfo>,
        xpat: &BindingPattern,
        iter: Idx<Expr>,
        body: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        let m = self.lower_expr(iter)?;
        let key_cell = self.f.new_cell(None);
        let val_cell = self.f.new_cell(None);
        self.f.mark_loop_binding(key_cell);
        self.f.mark_loop_binding(val_cell);
        let nil = self.emit_cur(Inst::Const(Lit::Nil));
        self.emit_cur(Inst::Store { cell: key_cell, value: nil });
        let i_cell = self.f.new_cell(Some(self.body.binding_arena[ib].name));
        self.binds.insert(raw(ib), i_cell);
        self.f.mark_loop_binding(i_cell);
        let zero = self.emit_cur(Inst::Const(Lit::Int(0)));
        self.emit_cur(Inst::Store { cell: i_cell, value: zero });

        let header = self.f.new_block();
        let body_bb = self.f.new_block();
        let latch = self.f.new_block();
        let exit = self.f.new_block();
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = header;
        self.emit_cur(Inst::MapNext { key_cell, val_cell, table: m });
        let k = self.emit_cur(Inst::Load(key_cell));
        let nil2 = self.emit_cur(Inst::Const(Lit::Nil));
        let done = self.emit_cur(Inst::Eq(k, nil2));
        self.f.set_term(self.cur, Term::Branch { cond: done, then_bb: exit, else_bb: body_bb });

        self.cur = body_bb;
        self.bind_map_entry(xpat, key_cell, val_cell, None)?;
        self.loop_targets.push((latch, exit)); // continue -> latch (steps counter)
        self.lower_stmt_expr(body)?;
        self.loop_targets.pop();
        if self.is_open() {
            self.f.set_term(self.cur, Term::Goto(latch));
        }

        self.cur = latch;
        let i2 = self.emit_cur(Inst::Load(i_cell));
        let one = self.emit_cur(Inst::Const(Lit::Int(1)));
        let inc = self.emit_cur(Inst::IntAdd(i2, one));
        self.emit_cur(Inst::Store { cell: i_cell, value: inc });
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = exit;
        Ok(())
    }

    /// `col_value` is the already-built element for a COLUMN-stored Map, whose
    /// value is leaves at a slot rather than a cell. `None` keeps the boxed
    /// walk, loading the value cell in place so the emitted order is unchanged.
    fn bind_map_entry(
        &mut self,
        vars: &BindingPattern,
        key_cell: Idx<Cell>,
        val_cell: Idx<Cell>,
        col_value: Option<Idx<Inst>>,
    ) -> Result<(), CompilerError> {
        match vars {
            BindingPattern::Tuple(subs) if subs.len() == 2 => {
                let kv = self.emit_cur(Inst::Load(key_cell));
                self.bind_for_pattern(&subs[0], kv);
                let vv = match col_value {
                    Some(v) => v,
                    None => self.emit_cur(Inst::Load(val_cell)),
                };
                self.bind_for_pattern(&subs[1], vv);
                Ok(())
            }
            BindingPattern::Leaf(_) => {
                let kv = self.emit_cur(Inst::Load(key_cell));
                let vv = match col_value {
                    Some(v) => v,
                    None => self.emit_cur(Inst::Load(val_cell)),
                };
                let pair = self.emit_cur(Inst::MakeTuple(vec![kv, vv]));
                self.bind_for_pattern(vars, pair);
                Ok(())
            }
            _ => Err(gap("for-over-Map binds one (item) or two (key, value) vars")),
        }
    }

    /// `for (i, x) in enumerate(coll)` (split bindings) or `for pair in
    /// enumerate(coll)` (one binding to the `(index, element)` tuple), over an
    /// indexable collection. The counter is a cell; `x = coll[i]`.
    fn lower_for_enumerate(
        &mut self,
        vars: &BindingPattern,
        coll: Idx<Expr>,
        body: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        // Split `(i, xpat)` vs a single binding to the whole `(i, element)` tuple.
        let split: Option<(Idx<BindingInfo>, BindingPattern)> = match vars {
            BindingPattern::Tuple(subs) if subs.len() == 2 => match &subs[0] {
                BindingPattern::Leaf(i) => Some((*i, subs[1].clone())),
                _ => return Err(gap("enumerate binds (index, element)")),
            },
            _ => None,
        };
        let kind = self.coll_kind(coll);
        if kind == ContainerKind::Map {
            let (ib, xpat) = split.ok_or_else(|| gap("enumerate over Map with a single binding"))?;
            return self.lower_for_map_enumerate(ib, &xpat, coll, body);
        }
        let arr_v = self.lower_expr(coll)?;
        let len_v = self.emit_cur(Inst::Len { base: arr_v, kind });
        // The counter cell is named after the index var in split mode; an internal
        // cell in single-binding mode (the index is folded into the tuple).
        let i_cell = match &split {
            Some((ib, _)) => {
                let c = self.f.new_cell(Some(self.body.binding_arena[*ib].name));
                self.binds.insert(raw(*ib), c);
                c
            }
            None => self.f.new_cell(None),
        };
        self.f.mark_loop_binding(i_cell);
        let zero = self.emit_cur(Inst::Const(Lit::Int(0)));
        self.emit_cur(Inst::Store {
            cell: i_cell,
            value: zero,
        });

        let header = self.f.new_block();
        let body_bb = self.f.new_block();
        let latch = self.f.new_block();
        let exit = self.f.new_block();
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = header;
        let i = self.emit_cur(Inst::Load(i_cell));
        let cmp = self.emit_cur(Inst::IntLt(i, len_v));
        self.f.set_term(
            self.cur,
            Term::Branch {
                cond: cmp,
                then_bb: body_bb,
                else_bb: exit,
            },
        );

        self.cur = body_bb;
        let i_now = self.emit_cur(Inst::Load(i_cell));
        let repr = self.loop_element_repr_at(arr_v, i_now, kind)?;
        match &split {
            // split: index already bound to i_cell; bind the element pattern.
            Some((_, xpat)) => self.bind_loop_element_repr(xpat, repr),
            // single: bind the whole `(index, element)` tuple.
            None => {
                let elem = self.pack(repr);
                let pair = self.emit_cur(Inst::MakeTuple(vec![i_now, elem]));
                self.bind_for_pattern(vars, pair);
            }
        }
        self.loop_targets.push((latch, exit));
        self.lower_stmt_expr(body)?;
        self.loop_targets.pop();
        if self.is_open() {
            self.f.set_term(self.cur, Term::Goto(latch));
        }

        self.cur = latch;
        let i2 = self.emit_cur(Inst::Load(i_cell));
        let one = self.emit_cur(Inst::Const(Lit::Int(1)));
        let inc = self.emit_cur(Inst::IntAdd(i2, one));
        self.emit_cur(Inst::Store {
            cell: i_cell,
            value: inc,
        });
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = exit;
        Ok(())
    }

    /// `for x in coll { body }` over an indexable collection: hoist the
    /// collection + its length, then `i = 0; while i < len { x = coll[i]; ...; i++ }`.
    /// The index is a cell; `continue` -> latch (still increments).
    fn lower_for_array(
        &mut self,
        vars: BindingPattern,
        iter: Idx<Expr>,
        body: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        // `for v in fv` over FlatVec: numeric loop bounded by the `len` FIELD
        // (not the buffer capacity), elements via the `buf` NativeBuffer.
        let (kind, arr_v, len_v) = if self.expr_is_flat_vec(iter) {
            let kind = self.nb_kind(iter);
            let fv = self.lower_expr(iter)?;
            let buf = self.emit_cur(Inst::Field { base: fv, field: Ustr::from("buf") });
            let len = self.emit_cur(Inst::Field { base: fv, field: Ustr::from("len") });
            (kind, buf, len)
        } else {
            let kind = self.coll_kind(iter);
            let arr_v = self.lower_expr(iter)?;
            let len_v = self.emit_cur(Inst::Len { base: arr_v, kind });
            (kind, arr_v, len_v)
        };
        let i_cell = self.f.new_cell(None);
        self.f.mark_loop_binding(i_cell);
        let zero = self.emit_cur(Inst::Const(Lit::Int(0)));
        self.emit_cur(Inst::Store {
            cell: i_cell,
            value: zero,
        });

        let header = self.f.new_block();
        let body_bb = self.f.new_block();
        let latch = self.f.new_block();
        let exit = self.f.new_block();
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = header;
        let i = self.emit_cur(Inst::Load(i_cell));
        let cmp = self.emit_cur(Inst::IntLt(i, len_v));
        self.f.set_term(
            self.cur,
            Term::Branch {
                cond: cmp,
                then_bb: body_bb,
                else_bb: exit,
            },
        );

        self.cur = body_bb;
        let i_now = self.emit_cur(Inst::Load(i_cell));
        let elem = self.loop_element_at(arr_v, i_now, kind)?;
        self.bind_for_pattern(&vars, elem);
        self.loop_targets.push((latch, exit));
        self.lower_stmt_expr(body)?;
        self.loop_targets.pop();
        if self.is_open() {
            self.f.set_term(self.cur, Term::Goto(latch));
        }

        self.cur = latch;
        let i2 = self.emit_cur(Inst::Load(i_cell));
        let one = self.emit_cur(Inst::Const(Lit::Int(1)));
        let inc = self.emit_cur(Inst::IntAdd(i2, one));
        self.emit_cur(Inst::Store {
            cell: i_cell,
            value: inc,
        });
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = exit;
        Ok(())
    }

    /// [`Self::bind_for_pattern`] for an element that arrived as a repr: a
    /// `nobox` element binds its LEAVES (`group_binds`, the shape a scalarized
    /// value param takes) instead of a packed box.
    ///
    /// Packing is what the mark forbids, and for a NULLABLE element it is also
    /// lossy in a way no later pass can undo: `pack` drops the Group's presence,
    /// so the absent elements would come back as a struct of nils rather than as
    /// `null`. SROA peels a redundant pack for the non-nullable case, which is
    /// why that one worked through the boxed bind; presence has no such recovery.
    fn bind_loop_element_repr(&mut self, vars: &BindingPattern, repr: ValueRepr) {
        if let BindingPattern::Leaf(b) = vars {
            if let Some(shape) = self.binding_ty(*b).and_then(|t| self.nobox_shape_of_ty(t)) {
                let name = self.body.binding_arena[*b].name;
                let leaves = self.repr_to_leaves(&repr, &shape);
                let cells: Vec<Idx<Cell>> = leaves
                    .iter()
                    .enumerate()
                    .map(|(li, &value)| {
                        let c = self
                            .f
                            .new_cell(Some(Ustr::from(format!("{name}__{li}").as_str())));
                        self.emit_cur(Inst::Store { cell: c, value });
                        c
                    })
                    .collect();
                self.group_binds.insert(raw(*b), (cells, shape));
                return;
            }
        }
        let value = self.pack(repr);
        self.bind_for_pattern(vars, value);
    }

    /// Bind a for-loop pattern to an element value: `Leaf` -> one cell; `Tuple`
    /// -> destructure via `TupleIndex` into a cell per element.
    fn bind_for_pattern(&mut self, vars: &BindingPattern, value: Idx<Inst>) {
        match vars {
            BindingPattern::Leaf(b) => {
                let cell = self.f.new_cell(Some(self.body.binding_arena[*b].name));
                if self.binding_is_nonnull_value_struct(*b) {
                    self.f.mark_nonnull_value_struct(cell);
                }
                self.emit_cur(Inst::Store { cell, value });
                self.binds.insert(raw(*b), cell);
            }
            BindingPattern::Tuple(subs) => {
                // Fast path: destructuring a fresh `MakeTuple` (a tuple literal or a
                // multi-return call packed as `MakeTuple(loads)`) binds directly to
                // its operands -- no table alloc, no `TupleIndex`. This is what makes
                // `let (ok, x) = next(it)` zero-alloc (slice 1).
                if let Inst::MakeTuple(vs) = &self.f.insts[value] {
                    if vs.len() == subs.len() {
                        let vs = vs.clone();
                        for (sub, v) in subs.iter().zip(vs) {
                            self.bind_for_pattern(sub, v);
                        }
                        return;
                    }
                }
                for (i, sub) in subs.iter().enumerate() {
                    let elem = self.emit_cur(Inst::TupleIndex { base: value, index: i });
                    self.bind_for_pattern(sub, elem);
                }
            }
        }
    }

    /// [`Self::bind_for_pattern`] against a SPLATTING tuple's flat slot values.
    /// A `Leaves` slot binds its leaves into `group_binds` cells -- the same form
    /// a scalarized value param takes -- so reading the binding never rebuilds a
    /// box. Any pattern/slot pairing that does not line up (a single binding for
    /// a whole nested tuple, say) rebuilds and falls back to the boxed bind,
    /// which is where a whole-value use of a `nobox` element meets the
    /// guarantee's rejection.
    fn bind_pattern_slot(
        &mut self,
        vars: &BindingPattern,
        slot: &RetSlot,
        values: &[Idx<Inst>],
        idx: &mut usize,
    ) {
        match (vars, slot) {
            (BindingPattern::Leaf(b), RetSlot::Leaves(shape)) => {
                let n = shape.leaf_count();
                let leaves = values[*idx..*idx + n].to_vec();
                *idx += n;
                self.bind_leaves(*b, &leaves, shape.clone());
            }
            (BindingPattern::Tuple(subs), RetSlot::Nested(inner))
                if subs.len() == inner.slots.len() =>
            {
                for (sub, s) in subs.iter().zip(inner.slots.clone()) {
                    self.bind_pattern_slot(sub, &s, values, idx);
                }
            }
            (vars, slot) => {
                let r = self.slot_repr(slot, values, idx);
                let v = self.pack(r);
                self.bind_for_pattern(vars, v);
            }
        }
    }

    /// Bind `b` to a value's LEAVES: one cell per leaf, registered in
    /// `group_binds`. The single form for "this binding is a scalarized value",
    /// shared by `if let` and the splatting tuple destructures.
    fn bind_leaves(&mut self, b: Idx<BindingInfo>, leaves: &[Idx<Inst>], shape: ValueShape) {
        let bname = self.body.binding_arena[b].name;
        let cells: Vec<Idx<Cell>> = leaves
            .iter()
            .enumerate()
            .map(|(li, &value)| {
                let c = self.f.new_cell(Some(Ustr::from(format!("{bname}__{li}").as_str())));
                self.emit_cur(Inst::Store { cell: c, value });
                c
            })
            .collect();
        self.group_binds.insert(raw(b), (cells, shape));
    }

    /// Destructure a splatting tuple into `vars`. The elements arrive as flat
    /// slots, so an element that is a `nobox` value binds its leaves directly.
    fn bind_tuple_repr(&mut self, vars: &BindingPattern, shape: &TupleRetShape, r: ValueRepr) {
        let values = self.tuple_repr_to_slots(r, shape);
        let mut idx = 0;
        match vars {
            BindingPattern::Tuple(subs) if subs.len() == shape.slots.len() => {
                for (sub, slot) in subs.iter().zip(shape.slots.clone()) {
                    self.bind_pattern_slot(sub, &slot, &values, &mut idx);
                }
            }
            _ => {
                let r = self.tuple_from_slots(shape, &values, &mut idx);
                let v = self.pack(r);
                self.bind_for_pattern(vars, v);
            }
        }
    }

    /// `match scrut { pat => body, ... }` as a value. Each arm is a test block
    /// (branch to its body on match, else to the next arm) and a body block that
    /// binds, checks the guard, then stores its value into a result **cell** and
    /// jumps to the join. Single-level patterns only: a sub-pattern that needs
    /// its own test (nested enum) gaps rather than silently skipping the test.
    fn lower_match(
        &mut self,
        scrut: Idx<Expr>,
        arms: comfy_la_arena::IdxRange<MatchArm>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let repr = self.lower_match_repr(scrut, arms, RetSlot::Opaque)?;
        Ok(self.pack(repr))
    }

    fn lower_match_repr(
        &mut self,
        scrut: Idx<Expr>,
        arms: comfy_la_arena::IdxRange<MatchArm>,
        shape: RetSlot,
    ) -> Result<ValueRepr, CompilerError> {
        let results: Vec<Idx<Cell>> =
            (0..shape.slot_count()).map(|_| self.f.new_cell(None)).collect();
        let variants = self.scrut_variants(scrut);
        let scrut_enum = self.scrut_enum_name(scrut);
        let scrut_s = self.lower_scrut(scrut)?;
        let join = self.f.new_block();

        for aidx in arms {
            let arm = self.body.match_arm_arena[aidx];
            let next = self.f.new_block();
            // Emit this arm's pattern tests (failures jump to `next`); on success
            // `self.cur` is the block where the bindings are in scope.
            self.match_pattern(&scrut_s, arm.pattern, next, &variants, true, scrut_enum)?;
            if let Some(g) = arm.guard {
                let gv = self.lower_expr(g)?;
                let kept = self.f.new_block();
                self.f.set_term(
                    self.cur,
                    Term::Branch {
                        cond: gv,
                        then_bb: kept,
                        else_bb: next,
                    },
                );
                self.cur = kept;
            }
            let bv = self.lower_expr_repr(arm.body)?;
            self.store_repr_and_join(&results, &shape, bv, join)?;

            self.cur = next;
        }
        // No arm matched: bone checks exhaustiveness, so this is unreachable.
        self.f.set_term(self.cur, Term::Unreachable);

        self.cur = join;
        let slots: Vec<Idx<Inst>> =
            results.iter().map(|&cell| self.emit_cur(Inst::Load(cell))).collect();
        let mut idx = 0;
        Ok(self.slot_repr(&shape, &slots, &mut idx))
    }

    /// `match` in STATEMENT position (value discarded): mirrors [`Self::lower_match`]
    /// but lowers each arm body as a statement with NO result cell / store / load.
    fn lower_match_stmt(
        &mut self,
        scrut: Idx<Expr>,
        arms: comfy_la_arena::IdxRange<MatchArm>,
    ) -> Result<(), CompilerError> {
        let variants = self.scrut_variants(scrut);
        let scrut_enum = self.scrut_enum_name(scrut);
        let scrut_s = self.lower_scrut(scrut)?;
        let join = self.f.new_block();
        for aidx in arms {
            let arm = self.body.match_arm_arena[aidx];
            let next = self.f.new_block();
            self.match_pattern(&scrut_s, arm.pattern, next, &variants, true, scrut_enum)?;
            if let Some(g) = arm.guard {
                let gv = self.lower_expr(g)?;
                let kept = self.f.new_block();
                self.f.set_term(self.cur, Term::Branch { cond: gv, then_bb: kept, else_bb: next });
                self.cur = kept;
            }
            self.lower_stmt_expr(arm.body)?;
            if self.is_open() {
                self.f.set_term(self.cur, Term::Goto(join));
            }
            self.cur = next;
        }
        self.f.set_term(self.cur, Term::Unreachable);
        self.cur = join;
        Ok(())
    }

    /// Recursively test `pat` against the value `val`, branching to `fail` on any
    /// mismatch and binding pattern variables on the success path. Advances
    /// `self.cur` to the block where matching continues (so a parent tag test
    /// always gates a child read — the access-path ordering). Emits no test for
    /// irrefutable forms (wildcard / bind / tuple-of-binds).
    fn match_pattern(
        &mut self,
        scrut: &Scrut,
        pat: Idx<Pattern>,
        fail: Idx<Block>,
        // The scrutinee enum's variant names (empty if not an enum). `top` is true
        // only at the pattern root — an unqualified `Red` there is a nullary
        // variant tag test, not a binding. `scrut_enum` is the scrutinee's enum
        // name (used at `top` to int-compare a payload-free tag); nested patterns
        // pass `None` and rely on their own `enum_name`.
        variants: &HashSet<Ustr>,
        top: bool,
        scrut_enum: Option<Ustr>,
    ) -> Result<(), CompilerError> {
        match self.body.pattern_arena[pat].clone() {
            Pattern::Wildcard => Ok(()),
            Pattern::Bind(b) => {
                let name = self.body.binding_arena[b].name;
                // `match c { Red => .. }` — bone lowers an unqualified nullary
                // variant as a catch-all bind; at the root, if the name is a
                // variant of the scrutinee enum it's a tag test (old skull parity).
                if top && variants.contains(&name) {
                    self.test_tag(scrut, name, scrut_enum, fail);
                    return Ok(());
                }
                let val = self.scrut_value(scrut);
                let cell = self.f.new_cell(Some(name));
                self.emit_cur(Inst::Store { cell, value: val });
                self.binds.insert(raw(b), cell);
                Ok(())
            }
            Pattern::Literal(le) => {
                let val = self.scrut_value(scrut);
                let lv = self.lower_expr(le)?;
                let cond = self.emit_cur(Inst::Eq(val, lv));
                self.branch_or_fail(cond, fail);
                Ok(())
            }
            Pattern::EnumVariant { enum_name, variant, sub, .. } => {
                // The variant's enum: the qualified name on the pattern, else the
                // scrutinee's enum (an unqualified `Variant` form). `test_tag`
                // gates on `payload_free_ordinal(en, variant)`: a payload-free
                // unit variant int-compares its ordinal; a payload-carrying
                // variant resolves to None there and takes the `.__variant` path.
                let en = enum_name.or(scrut_enum);
                self.test_tag(scrut, variant, en, fail);
                // A cdata enum's payload lives in its `_<i>` C field (cut 1: `_0`);
                // a table enum's is the positional `[i+1]` slot; a FLATTENED field's
                // is the parent's `{prefix}__i` slot (see `scrut_payload`).
                let cdata = en.is_some_and(|e| enum_value_shape(self.ws, e).is_some());
                for (i, sp) in sub.into_iter().enumerate() {
                    let payload = self.scrut_payload(scrut, i, cdata);
                    // Thread the sub-pattern's enum (from this variant's payload
                    // type) so a NESTED enum pattern (`L(J(x))`) tests its own tag
                    // repr -- else an unqualified `J` defaults to the `.__variant`
                    // table path and faults on a cdata inner enum.
                    let sub_enum = en.and_then(|e| self.variant_payload_enum(e, variant, i));
                    self.match_pattern(&Scrut::Val(payload), sp, fail, variants, false, sub_enum)?;
                }
                Ok(())
            }
            Pattern::Tuple { elements } => {
                let val = self.scrut_value(scrut);
                for (i, sp) in elements.into_iter().enumerate() {
                    let elem = self.emit_cur(Inst::TupleIndex { base: val, index: i });
                    self.match_pattern(&Scrut::Val(elem), sp, fail, variants, false, None)?;
                }
                Ok(())
            }
            Pattern::Record {
                enum_name,
                name,
                fields,
                ..
            } => {
                // Emit the discriminant tag test for an enum-variant record
                // pattern: qualified (`enum_name` set) OR an unqualified variant
                // name belonging to the scrutinee enum (`top`, like the Bind case).
                // A plain struct-destructure record pattern (`name` not a variant)
                // emits none. Without this, an unqualified `Circle { .. }` arm with
                // no tag test would match ANY variant and index missing fields.
                let en = enum_name.or(scrut_enum);
                if let Some(variant) = name {
                    if enum_name.is_some() || (top && variants.contains(&variant)) {
                        // `test_tag` picks the repr: a cdata record enum compares
                        // `.__tag`, a table enum `.__variant`.
                        self.test_tag(scrut, variant, en, fail);
                    }
                }
                // A FLATTENED field reads its `{prefix}__<field>` slot; a cdata enum's
                // record payload lives in `_<field>` C slots; a table enum (or a plain
                // struct destructure, `name` = None) reads the field key directly.
                let cdata = name.is_some() && en.is_some_and(|e| enum_value_shape(self.ws, e).is_some());
                for rfidx in fields {
                    let rpf = self.body.record_pat_field_arena[rfidx];
                    // An embed/nobox field has NO slot under its own name, so a
                    // BINDING for it rebuilds from the parent's dotted leaves --
                    // the same rule the field-read paths follow. `scrut_named`
                    // would emit `Inst::Field` against a key that does not exist.
                    // `embed_field_of_struct` fails closed on anything that is not
                    // a plain struct (an enum variant name is not in
                    // `struct_by_name`), so a variant record falls through.
                    let embedded = name
                        .and_then(|sname| {
                            self.embed_field_of_struct(sname, rpf.name).map(|cn| (sname, cn))
                        })
                        .filter(|_| matches!(scrut, Scrut::Val(_)));
                    let fv = match (embedded, scrut) {
                        (Some((sname, cname)), Scrut::Val(v)) => {
                            let parent = *v;
                            if self.embed_field_is_nullable(sname, rpf.name) {
                                self.materialize_embed_nullable(parent, rpf.name, cname)
                            } else {
                                self.materialize_embed(parent, rpf.name, cname)
                            }
                        }
                        _ => self.scrut_named(scrut, rpf.name, cdata),
                    };
                    self.match_pattern(&Scrut::Val(fv), rpf.pat, fail, variants, false, None)?;
                }
                Ok(())
            }
            Pattern::Range {
                start,
                end,
                inclusive,
            } => {
                // Assumes an int discriminant (the common range-pattern case).
                let val = self.scrut_value(scrut);
                if let Some(s) = start {
                    let sv = self.lower_expr(s)?;
                    let cond = self.emit_cur(Inst::IntLeq(sv, val)); // start <= val
                    self.branch_or_fail(cond, fail);
                }
                if let Some(e) = end {
                    let ev = self.lower_expr(e)?;
                    let cond = if inclusive {
                        self.emit_cur(Inst::IntLeq(val, ev))
                    } else {
                        self.emit_cur(Inst::IntLt(val, ev))
                    };
                    self.branch_or_fail(cond, fail);
                }
                Ok(())
            }
            Pattern::Or { alternatives } => {
                // `A | B | C` matches if ANY alternative does: test each in turn,
                // success -> a shared `matched` continuation, mismatch -> the next
                // alternative (last -> the arm `fail`). Alternatives must be
                // binding-free (a binding set on only one branch would be undefined
                // on the others in the shared body).
                let alts: Vec<Idx<Pattern>> = alternatives.into_iter().collect();
                for &alt in &alts {
                    if self.pattern_binds(alt, variants, top) {
                        return Err(gap("or-pattern with bindings (later)"));
                    }
                }
                let matched = self.f.new_block();
                for (i, &alt) in alts.iter().enumerate() {
                    let is_last = i + 1 == alts.len();
                    let next_alt = if is_last { fail } else { self.f.new_block() };
                    // An or-pattern preserves `top`, so a top-level `A | B` on a
                    // payload-free enum keeps the int-compare path -> forward
                    // `scrut_enum` (nested alternatives carry `None` anyway).
                    self.match_pattern(scrut, alt, next_alt, variants, top, scrut_enum)?;
                    // success path of this alternative -> the shared continuation.
                    if self.is_open() {
                        self.f.set_term(self.cur, Term::Goto(matched));
                    }
                    if !is_last {
                        self.cur = next_alt;
                    }
                }
                self.cur = matched;
                Ok(())
            }
        }
    }

    /// Whether `pat` introduces a real binding (not a top-level variant-name tag
    /// test). Used to reject or-pattern alternatives that bind.
    fn pattern_binds(&self, pat: Idx<Pattern>, variants: &HashSet<Ustr>, top: bool) -> bool {
        match self.body.pattern_arena[pat].clone() {
            Pattern::Wildcard | Pattern::Literal(_) | Pattern::Range { .. } => false,
            Pattern::Bind(b) => {
                let name = self.body.binding_arena[b].name;
                !(top && variants.contains(&name))
            }
            Pattern::EnumVariant { sub, .. } => {
                sub.into_iter().any(|p| self.pattern_binds(p, variants, false))
            }
            Pattern::Tuple { elements } => {
                elements.into_iter().any(|p| self.pattern_binds(p, variants, false))
            }
            Pattern::Record { fields, .. } => fields
                .into_iter()
                .any(|f| self.pattern_binds(self.body.record_pat_field_arena[f].pat, variants, false)),
            Pattern::Or { alternatives } => {
                alternatives.into_iter().any(|p| self.pattern_binds(p, variants, top))
            }
        }
    }

    /// The enum NAME of payload field `i` of `enum_name::variant`, if that field's
    /// type is itself an enum (`None` otherwise) -- threaded into a nested match so
    /// the inner pattern tests the inner enum's tag repr.
    fn variant_payload_enum(&self, enum_name: Ustr, variant: Ustr, i: usize) -> Option<Ustr> {
        use bone_semantic::hir::VariantPayload;
        let eid = self.ws.enums_by_name.get(&enum_name)?;
        let ed = &self.ws.modules[eid.module.index()].enums[eid.def];
        let v = ed.variants.iter().find(|v| v.name == variant)?;
        let ty = match &v.payload {
            VariantPayload::Tuple(tys) => *tys.get(i)?,
            _ => return None,
        };
        match static_ty(self.ws, ty)? {
            TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } if self.ws.enums_by_name.contains_key(&n) => {
                Some(n)
            }
            _ => None,
        }
    }

    /// Lower a match/if-let/while-let scrutinee: a flattened enum field stays a
    /// `Scrut::Flat` (tag/payload read from the parent's slots, no box); anything
    /// else lowers to an ordinary value.
    fn lower_scrut(&mut self, e: Idx<Expr>) -> Result<Scrut, CompilerError> {
        if let Expr::FieldAccess { base, field } = self.body.expr_arena[e].clone() {
            if let Some(shape) = self.flat_field_of(base, field) {
                let parent = self.lower_expr(base)?;
                return Ok(Scrut::Flat { parent, prefix: field, shape });
            }
        }
        // A flat enum that threads as a Group (scalarized param/return, or a freshly
        // constructed enum) matches from its cells -- no box.
        let r = self.lower_expr_repr(e)?;
        if let ValueRepr::Group { name, .. } = &r {
            if let Some(shape) = flat_enum_shape(self.ws, *name) {
                let ValueRepr::Group { parts, .. } = r else { unreachable!() };
                let cells = self.repr_group_to_cells(&shape, parts);
                return Ok(Scrut::Cells {
                    tag: cells.tag,
                    positional: cells.positional,
                    named: cells.named,
                    shape,
                });
            }
        }
        Ok(Scrut::Val(self.pack(r)))
    }

    /// The scrutinee as a whole value -- materializing a flattened field / threaded
    /// cells into a boxed enum (only on a path that genuinely needs the whole value).
    fn scrut_value(&mut self, scrut: &Scrut) -> Idx<Inst> {
        match scrut {
            Scrut::Val(v) => *v,
            Scrut::Flat { parent, prefix, shape } => self.materialize_flat(*parent, *prefix, shape),
            Scrut::Cells { tag, positional, named, shape } => self.emit_cur(Inst::MakeEnumDyn {
                tag: *tag,
                ty: shape.name,
                positional: positional.clone(),
                named: named.clone(),
            }),
        }
    }

    /// The scrutinee's `i`th positional payload: threaded cells hold it directly; a
    /// flat field reads the parent's `{prefix}__i` slot; a cdata enum reads its
    /// `_<i>` C field; a table enum reads the positional `[i+1]` slot.
    fn scrut_payload(&mut self, scrut: &Scrut, i: usize, cdata: bool) -> Idx<Inst> {
        match scrut {
            Scrut::Cells { positional, .. } => positional[i],
            Scrut::Flat { parent, prefix, .. } => {
                self.emit_cur(Inst::Field { base: *parent, field: flat_payload_slot(*prefix, i) })
            }
            Scrut::Val(v) if cdata => {
                self.emit_cur(Inst::Field { base: *v, field: Ustr::from(format!("_{i}").as_str()) })
            }
            Scrut::Val(v) => self.emit_cur(Inst::TupleIndex { base: *v, index: i }),
        }
    }

    /// The scrutinee's named (record) payload `field`: threaded cells hold it
    /// directly; a flat field reads the parent's `{prefix}__<field>` slot; a cdata
    /// enum reads its `_<field>` C slot; a table enum reads the field key directly.
    fn scrut_named(&mut self, scrut: &Scrut, field: Ustr, cdata: bool) -> Idx<Inst> {
        match scrut {
            Scrut::Cells { named, .. } => {
                named.iter().find(|(n, _)| *n == field).map(|(_, v)| *v).unwrap_or_else(|| {
                    self.emit_cur(Inst::Const(Lit::Nil))
                })
            }
            Scrut::Flat { parent, prefix, .. } => {
                self.emit_cur(Inst::Field { base: *parent, field: flat_named_slot(*prefix, field) })
            }
            Scrut::Val(v) => {
                let f = if cdata { slot_name(field.as_str()) } else { field };
                self.emit_cur(Inst::Field { base: *v, field: f })
            }
        }
    }

    /// Tag test -> continue on match, else `fail`. Five reprs: a payload-free enum
    /// IS its ordinal int (compare ints); a cdata enum compares `.__tag` (int); a
    /// table enum compares `.__variant` (string); a FLATTENED field compares its
    /// `{prefix}__tag` slot; threaded CELLS compare the held tag (both strings).
    fn test_tag(&mut self, scrut: &Scrut, variant: Ustr, enum_name: Option<Ustr>, fail: Idx<Block>) {
        let flat_tag = match scrut {
            Scrut::Flat { parent, prefix, .. } => {
                Some(self.emit_cur(Inst::Field { base: *parent, field: flat_tag_slot(*prefix) }))
            }
            Scrut::Cells { tag, .. } => Some(*tag),
            Scrut::Val(_) => None,
        };
        if let Some(tag) = flat_tag {
            let want = self.emit_cur(Inst::Const(Lit::Str(variant)));
            let cond = self.emit_cur(Inst::Eq(tag, want));
            self.branch_or_fail(cond, fail);
            return;
        }
        let val = self.scrut_value(scrut);
        if let Some(ord) = enum_name.and_then(|en| payload_free_ordinal(self.ws, en, variant)) {
            let want = self.emit_cur(Inst::Const(Lit::Int(ord)));
            let cond = self.emit_cur(Inst::Eq(val, want));
            self.branch_or_fail(cond, fail);
            return;
        }
        if let Some(ord) = enum_name
            .and_then(|en| enum_value_shape(self.ws, en))
            .and_then(|s| s.tag_of(variant))
        {
            let tag = self.emit_cur(Inst::Field { base: val, field: Ustr::from("__tag") });
            let want = self.emit_cur(Inst::Const(Lit::Int(ord)));
            let cond = self.emit_cur(Inst::Eq(tag, want));
            self.branch_or_fail(cond, fail);
            return;
        }
        let tag = self.emit_cur(Inst::Field {
            base: val,
            field: Ustr::from("__variant"),
        });
        let want = self.emit_cur(Inst::Const(Lit::Str(variant)));
        let cond = self.emit_cur(Inst::Eq(tag, want));
        self.branch_or_fail(cond, fail);
    }

    /// `cond` true -> a fresh continuation block (`self.cur` advances), false ->
    /// `fail`.
    fn branch_or_fail(&mut self, cond: Idx<Inst>, fail: Idx<Block>) {
        let cont = self.f.new_block();
        self.f.set_term(
            self.cur,
            Term::Branch {
                cond,
                then_bb: cont,
                else_bb: fail,
            },
        );
        self.cur = cont;
    }

    fn lower_if_let(
        &mut self,
        pattern: Idx<Pattern>,
        value: Idx<Expr>,
        then_branch: Idx<Expr>,
        else_branch: Option<Idx<Expr>>,
    ) -> Result<Idx<Inst>, CompilerError> {
        if let Pattern::Bind(b) = self.body.pattern_arena[pattern] {
            if self.expr_is_nullable(value) && self.nobox_binding_shape(b).is_some() {
                return self.lower_if_let_nobox(b, value, then_branch, else_branch);
            }
            if let Some(shape) = self.nullable_tuple_call_shape(value) {
                return self.lower_if_let_nullable_tuple(b, value, &shape, then_branch, else_branch);
            }
        }
        let result = self.f.new_cell(None);
        let scrut = self.lower_scrut(value)?;
        let else_bb = self.f.new_block();
        let join = self.f.new_block();
        // A nullable scrutinee (`if let v = some_opt`) fails the bind on nil ->
        // else branch (the runtime value of a `T?` is the unwrapped value or nil).
        // A flattened-enum-field scrutinee is never nullable (its type is a bare
        // enum), so `scrut_value` here is always a plain `Val`.
        if self.expr_is_nullable(value) {
            let val_v = self.scrut_value(&scrut);
            let nil = self.emit_cur(Inst::Const(Lit::Nil));
            let is_nil = self.emit_cur(Inst::Eq(val_v, nil));
            let cont = self.f.new_block();
            self.f.set_term(self.cur, Term::Branch { cond: is_nil, then_bb: else_bb, else_bb: cont });
            self.cur = cont;
        }
        // Match: failures jump to else_bb; on success `self.cur` holds the bindings.
        let variants = self.scrut_variants(value);
        let scrut_enum = self.scrut_enum_name(value);
        self.match_pattern(&scrut, pattern, else_bb, &variants, true, scrut_enum)?;
        let tv = self.lower_expr(then_branch)?;
        self.store_and_join(result, tv, join);

        self.cur = else_bb;
        let ev = match else_branch {
            Some(eb) => self.lower_expr(eb)?,
            None => self.emit_cur(Inst::Const(Lit::Nil)),
        };
        self.store_and_join(result, ev, join);

        self.cur = join;
        Ok(self.emit_cur(Inst::Load(result)))
    }

    /// `if let q = p` binding a nullable `nobox` value: the scrutinee's presence
    /// leaf IS the branch condition and `q` binds the remaining leaves as cells,
    /// so nothing is packed to be tested and immediately unpacked again. Only a
    /// plain `Bind` takes this path -- a destructuring pattern still matches
    /// against the boxed scrutinee.
    fn lower_if_let_nobox(
        &mut self,
        b: Idx<BindingInfo>,
        value: Idx<Expr>,
        then_branch: Idx<Expr>,
        else_branch: Option<Idx<Expr>>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let shape = self.nobox_binding_shape(b).expect("caller checked");
        let mut scrut_shape = shape.clone();
        scrut_shape.nullable = true;

        let result = self.f.new_cell(None);
        let repr = self.lower_expr_repr(value)?;
        let leaves = self.repr_to_leaves(&repr, &scrut_shape);
        let (present, values) = leaves
            .split_first()
            .expect("a nullable shape carries its presence leaf first");
        let (present, values) = (*present, values.to_vec());

        let then_bb = self.f.new_block();
        let else_bb = self.f.new_block();
        let join = self.f.new_block();
        self.f.set_term(self.cur, Term::Branch { cond: present, then_bb, else_bb });

        self.cur = then_bb;
        self.bind_leaves(b, &values, shape);
        let tv = self.lower_expr(then_branch)?;
        self.store_and_join(result, tv, join);

        self.cur = else_bb;
        let ev = match else_branch {
            Some(eb) => self.lower_expr(eb)?,
            None => self.emit_cur(Inst::Const(Lit::Nil)),
        };
        self.store_and_join(result, ev, join);

        self.cur = join;
        Ok(self.emit_cur(Inst::Load(result)))
    }

    /// The DECLARED nullable-tuple shape of a call, without lowering it.
    ///
    /// Read off the CALLEE's return type, never off `e`'s own type: safe-nav
    /// gives `h?.pair()` the same `Nullable(Tuple)` with no presence slot, so
    /// keying on the expression type would branch on a slot that is not there.
    fn nullable_tuple_call_shape(&self, e: Idx<Expr>) -> Option<TupleRetShape> {
        let Expr::Call {
            callee: Callee::Unresolved { name, .. },
            ..
        } = self.body.expr_arena[e]
        else {
            return None;
        };
        let func = self.resolve_funcref(e, name).ok()?;
        let ret = self.ws.module(func.module).functions[func.func].return_ty?;
        let shape = tuple_ret_shape(self.ws, self.spec_subst, ret)?;
        shape.nullable.then_some(shape)
    }

    /// `if let t = <call returning (..)?>` where an element splats: the presence
    /// SLOT is the branch condition and `t` binds the remaining slots as cells,
    /// so nothing is packed to be tested and immediately unpacked again. The
    /// tuple twin of [`Self::lower_if_let_nobox`].
    fn lower_if_let_nullable_tuple(
        &mut self,
        b: Idx<BindingInfo>,
        value: Idx<Expr>,
        shape: &TupleRetShape,
        then_branch: Idx<Expr>,
        else_branch: Option<Idx<Expr>>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let result = self.f.new_cell(None);
        let repr = self.lower_expr_repr(value)?;
        let ValueRepr::Tuple {
            parts,
            present: Some(present),
        } = repr
        else {
            return Err(gap("a nullable tuple scrutinee arrived without its presence slot"));
        };
        // Presence is consumed by the branch, so what `t` holds is the plain tuple.
        let bound_shape = TupleRetShape { slots: shape.slots.clone(), nullable: false };

        let then_bb = self.f.new_block();
        let else_bb = self.f.new_block();
        let join = self.f.new_block();
        self.f.set_term(self.cur, Term::Branch { cond: present, then_bb, else_bb });

        self.cur = then_bb;
        let slots = self.tuple_repr_to_slots(
            ValueRepr::Tuple { parts, present: None },
            &bound_shape,
        );
        let cells: Vec<Idx<Cell>> = slots
            .iter()
            .map(|v| {
                let c = self.f.new_cell(None);
                self.emit_cur(Inst::Store { cell: c, value: *v });
                c
            })
            .collect();
        self.tuple_binds.insert(raw(b), (cells, bound_shape));
        let tv = self.lower_expr(then_branch)?;
        self.store_and_join(result, tv, join);

        self.cur = else_bb;
        let ev = match else_branch {
            Some(eb) => self.lower_expr(eb)?,
            None => self.emit_cur(Inst::Const(Lit::Nil)),
        };
        self.store_and_join(result, ev, join);

        self.cur = join;
        Ok(self.emit_cur(Inst::Load(result)))
    }

    /// `while let pat = value { body }`: re-evaluate `value` each iteration in
    /// the header, match it, run the body on success, exit on failure.
    fn lower_while_let(
        &mut self,
        pattern: Idx<Pattern>,
        value: Idx<Expr>,
        body: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        let header = self.f.new_block();
        let exit = self.f.new_block();
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = header;
        let variants = self.scrut_variants(value);
        let scrut_enum = self.scrut_enum_name(value);
        let scrut = self.lower_scrut(value)?;
        // A nullable scrutinee (`while let v = some_opt`) exits the loop on nil
        // (the runtime value of a `T?` is the unwrapped value or nil) — mirrors
        // `if let`. Without this, a plain Bind always "matches" and the loop runs
        // with `v = nil`, then a use of `v` faults. (A flat-field scrutinee is
        // never nullable, so `scrut_value` is a plain `Val` here.)
        if self.expr_is_nullable(value) {
            let val_v = self.scrut_value(&scrut);
            let nil = self.emit_cur(Inst::Const(Lit::Nil));
            let is_nil = self.emit_cur(Inst::Eq(val_v, nil));
            let cont = self.f.new_block();
            self.f.set_term(self.cur, Term::Branch { cond: is_nil, then_bb: exit, else_bb: cont });
            self.cur = cont;
        }
        // Match: failure exits the loop; on success `self.cur` holds the bindings.
        self.match_pattern(&scrut, pattern, exit, &variants, true, scrut_enum)?;
        self.loop_targets.push((header, exit));
        self.lower_stmt_expr(body)?;
        self.loop_targets.pop();
        if self.is_open() {
            self.f.set_term(self.cur, Term::Goto(header));
        }

        self.cur = exit;
        Ok(())
    }

    /// `for i in lo..hi { body }` (exclusive): a header testing `i < hi`, a body,
    /// a **latch** that increments `i` (so `continue` still steps), and an exit.
    /// The loop var and the loop bound are both cells.
    fn lower_for_range(
        &mut self,
        var_b: Idx<BindingInfo>,
        low: Idx<Expr>,
        high: Idx<Expr>,
        body: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        let i_cell = self.f.new_cell(Some(self.body.binding_arena[var_b].name));
        self.binds.insert(raw(var_b), i_cell);
        self.f.mark_loop_binding(i_cell);
        let lo_v = self.lower_expr(low)?;
        self.emit_cur(Inst::Store {
            cell: i_cell,
            value: lo_v,
        });
        let hi_v = self.lower_expr(high)?;
        let hi_cell = self.f.new_cell(None);
        self.emit_cur(Inst::Store {
            cell: hi_cell,
            value: hi_v,
        });

        let header = self.f.new_block();
        let body_bb = self.f.new_block();
        let latch = self.f.new_block();
        let exit = self.f.new_block();
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = header;
        let i_v = self.emit_cur(Inst::Load(i_cell));
        let h_v = self.emit_cur(Inst::Load(hi_cell));
        let cmp = self.emit_cur(Inst::IntLt(i_v, h_v));
        self.f.set_term(
            self.cur,
            Term::Branch {
                cond: cmp,
                then_bb: body_bb,
                else_bb: exit,
            },
        );

        self.cur = body_bb;
        self.loop_targets.push((latch, exit)); // continue -> latch (runs the step)
        self.lower_stmt_expr(body)?;
        self.loop_targets.pop();
        if self.is_open() {
            self.f.set_term(self.cur, Term::Goto(latch));
        }

        self.cur = latch;
        let i_v2 = self.emit_cur(Inst::Load(i_cell));
        let one = self.emit_cur(Inst::Const(Lit::Int(1)));
        let inc = self.emit_cur(Inst::IntAdd(i_v2, one));
        self.emit_cur(Inst::Store {
            cell: i_cell,
            value: inc,
        });
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = exit;
        Ok(())
    }

    fn lower_exprs(
        &mut self,
        range: comfy_la_arena::IdxRange<Expr>,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        range.into_iter().map(|e| self.lower_expr(e)).collect()
    }

    /// A struct literal -> `MakeStruct`, fields in DECLARED order. Every declared
    /// field must be present; an omitted (defaulted) field gaps for now —
    /// injecting its default means lowering the `StructDefaults` body, a later
    /// step. Emitting an incomplete struct would be a silent miscompile.
    fn lower_struct_literal(
        &mut self,
        e: Idx<Expr>,
        name: Ustr,
        fields: comfy_la_arena::IdxRange<bone_semantic::hir::StructLitField>,
        bases: comfy_la_arena::IdxRange<bone_semantic::hir::Expr>,
    ) -> Result<Idx<Inst>, CompilerError> {
        // Functional-update base (`Struct { ..base, .. }`): evaluate once, before
        // fields (source order), and read unwritten fields off it. Inference
        // guarantees at most one base and that it has this struct's type.
        let base_inst: Option<Idx<Inst>> = match bases.into_iter().next() {
            Some(b) => Some(self.lower_expr(b)?),
            None => None,
        };
        // A field GROUP (`Struct { a + b + c: expr }`) expands to several fields
        // sharing ONE `value` Expr index; evaluate each distinct value-expr once
        // (correct eval count, and grouped fields share the same instance), in
        // source order. A flattened enum field evaluates to (tag, payload slots)
        // instead of one boxed value (`docs/enum-allocation-report.md`).
        let mut lit_box: Vec<(Ustr, Idx<Inst>)> = Vec::new();
        let mut lit_flat: Vec<(Ustr, FlatCells)> = Vec::new();
        let mut lit_embed: Vec<(Ustr, Vec<(Ustr, Idx<Inst>)>)> = Vec::new();
        let mut by_value: HashMap<u32, Idx<Inst>> = HashMap::new();
        let mut flat_by_value: HashMap<u32, FlatCells> = HashMap::new();
        let mut embed_by_value: HashMap<u32, (Option<Idx<Inst>>, Vec<(Ustr, Idx<Inst>)>)> =
            HashMap::new();
        for fidx in fields {
            let lf = self.body.struct_lit_field_arena[fidx];
            let key: u32 = lf.value.into_raw().into();
            if let Some(shape) = self.flat_field_of_struct(name, lf.name) {
                let cells = match flat_by_value.get(&key) {
                    Some(c) => c.clone(),
                    None => {
                        let c = self.lower_flat_value(&shape, lf.value)?;
                        flat_by_value.insert(key, c.clone());
                        c
                    }
                };
                lit_flat.push((lf.name, cells));
            } else if let Some(cname) = self.embed_field_of_struct(name, lf.name) {
                // An embedded field's leaves ARE this parent's keys, so its
                // value is spliced rather than lowered to a child box. Keyed by
                // the value expr so a field GROUP still evaluates once.
                let nullable = self.embed_field_is_nullable(name, lf.name);
                let leaves = match embed_by_value.get(&key) {
                    Some(l) => l.clone(),
                    None => {
                        let l = self.embed_value_leaves(nullable, cname, lf.value)?;
                        embed_by_value.insert(key, l.clone());
                        l
                    }
                };
                lit_embed.push((lf.name, Self::embed_field_slots(lf.name, leaves)));
            } else {
                let v = match by_value.get(&key) {
                    Some(&v) => v,
                    None => {
                        let v = self.lower_expr(lf.value)?;
                        by_value.insert(key, v);
                        v
                    }
                };
                lit_box.push((lf.name, v));
            }
        }
        let out = match self.struct_field_names(name) {
            Some(decl) => {
                let mut fs = Vec::new();
                for fname in decl {
                    if let Some((_, cells)) = lit_flat.iter().find(|(n, _)| *n == fname) {
                        fs.extend(self.flat_literal_slots(fname, cells.clone()));
                    } else if let Some((_, slots)) = lit_embed.iter().find(|(n, _)| *n == fname) {
                        fs.extend(slots.clone());
                    } else if let Some(&(_, v)) = lit_box.iter().find(|(n, _)| *n == fname) {
                        fs.push((fname, v));
                    } else if let Some(cname) = self.embed_field_of_struct(name, fname) {
                        // Unwritten embed field: copy the base's leaves through
                        // (including its presence slot), else the struct default.
                        let nullable = self.embed_field_is_nullable(name, fname);
                        match base_inst {
                            Some(bi) => {
                                for k in self.embed_leaf_paths(cname) {
                                    let key = Ustr::from(&format!("{fname}.{k}"));
                                    let v = self.emit_cur(Inst::Field { base: bi, field: key });
                                    fs.push((key, v));
                                }
                                if nullable {
                                    let p = self.emit_cur(Inst::Field { base: bi, field: fname });
                                    fs.push((fname, p));
                                }
                            }
                            None => {
                                let dv = self.lower_struct_default(name, fname)?;
                                let leaves = self.embed_leaf_reads(cname, dv);
                                fs.extend(Self::embed_field_slots(fname, (None, leaves)));
                            }
                        }
                    } else if let Some(shape) = self.flat_field_of_struct(name, fname) {
                        // Unwritten flat field: from the base if present, else the
                        // struct default; spread the box into slots either way.
                        let dv = match base_inst {
                            Some(bi) => self.emit_cur(Inst::Field { base: bi, field: fname }),
                            None => self.lower_struct_default(name, fname)?,
                        };
                        let cells = self.spread_box_to_cells(dv, &shape);
                        fs.extend(self.flat_literal_slots(fname, cells));
                    } else {
                        let v = match base_inst {
                            Some(bi) => self.emit_cur(Inst::Field { base: bi, field: fname }),
                            None => self.lower_struct_default(name, fname)?,
                        };
                        fs.push((fname, v));
                    }
                }
                fs
            }
            None => lit_box, // unknown nominal (foreign module): emit as-given
        };
        // A hand-built `NativeBuffer { ptr: .., len: .. }` must carry the same
        // leaf views the `native_buffer_alloc` intrinsic attaches -- element
        // access decides on the TYPE, so a buffer of a leaf-addressed element
        // would otherwise index a nil view. (Loud, not silent: leaf addressing
        // never changes storage, so the only failure available is a missing
        // view, and that errors on first access.)
        let out = self.with_leaf_views(e, name, out);
        // Non-value structs only — a `value` literal is built via the repr layer
        // (`value_literal_repr` -> `pack`, the single value-cdata source), so it
        // never reaches here.
        Ok(self.emit_cur(Inst::MakeStruct {
            ty: Some(name),
            fields: out,
        }))
    }

    /// Append the leaf-view fields to a hand-written `NativeBuffer` literal's
    /// fields, keyed off its declared element type. A no-op for any other
    /// struct, for a primitive element, and for one already carrying views.
    fn with_leaf_views(
        &mut self,
        e: Idx<Expr>,
        name: Ustr,
        mut fields: Vec<(Ustr, Idx<Inst>)>,
    ) -> Vec<(Ustr, Idx<Inst>)> {
        if name.as_str() != "NativeBuffer" {
            return fields;
        }
        // The element comes from the literal's own inferred type, the same
        // oracle element ACCESS consults -- so construction and access can never
        // disagree about whether a buffer is leaf-addressed.
        let Some(elem) = self.nb_value_elem(e) else {
            return fields;
        };
        let Some(&(_, ptr)) = fields.iter().find(|(f, _)| f.as_str() == "ptr") else {
            return fields;
        };
        fields.extend(self.leaf_view_fields(elem, ptr));
        fields
    }

    fn struct_field_names(&self, name: Ustr) -> Option<Vec<Ustr>> {
        let sid = *self.ws.struct_by_name.get(&name)?;
        let sd = &self.ws.modules[sid.module.index()].structs[sid.def];
        Some(sd.fields.iter().map(|f| f.name).collect())
    }

    /// Whether nominal `name` is a `value` struct (C-layout cdata).
    /// Whether `e`'s type is a `value` struct -- the shape whose rvalue lowering
    /// packs a fresh cdata rather than handing back shared storage.
    fn expr_is_value_struct(&self, e: Idx<Expr>) -> bool {
        match self.expr_ty(e) {
            Some(TypeExpr::App { ctor: TypeCtor::Nominal(name), .. }) => self.is_value_struct(name),
            _ => false,
        }
    }

    fn is_value_struct(&self, name: Ustr) -> bool {
        self.ws
            .struct_by_name
            .get(&name)
            .map(|sid| self.ws.modules[sid.module.index()].structs[sid.def].is_value)
            .unwrap_or(false)
    }

    /// Whether `e` denotes a mutable PLACE -- storage a later write can reach
    /// through, so a whole-value read out of it must copy. A value local/param is
    /// NOT one (bone rejects leaf writes there), nor is a call result; without
    /// that exclusion every uniform-struct field read would rebuild its value.
    ///
    /// Sibling, NOT a duplicate: bone's `value_field_write_is_place`
    /// (`bone-inference/src/fields.rs`) decides whether a write TARGET is legal
    /// storage. This decides whether a later write can reach a value being READ,
    /// so it stops at the first container/global (`vlocal.buf[i]` is shared
    /// storage even though the chain starts at a value binding) where bone walks
    /// to the root. They must AGREE that a temporary is neither.
    fn is_mutable_place(&self, e: Idx<Expr>) -> bool {
        match &self.body.expr_arena[e] {
            Expr::Global(_) | Expr::Index { .. } => true,
            Expr::FieldAccess { base, .. } | Expr::SafeNav { base, .. } => {
                self.is_mutable_place(*base)
            }
            // A reference-struct binding aliases shared storage; a value binding
            // is a copy of its own.
            Expr::Var(b) => !self.binding_is_nonnull_value_struct(*b),
            _ => false,
        }
    }

    /// The value-struct type of `e`, EITHER tier. `None` for a non-value type or
    /// unsolved inference, and for a `V?` -- an ABSENT value has no leaves, so an
    /// unconditional split would index nil. A nullable read that still needs the
    /// copy takes the presence-aware path instead
    /// ([`Self::nullable_place_value_of`]).
    fn value_struct_of(&self, e: Idx<Expr>) -> Option<Ustr> {
        if self.expr_is_nullable(e) {
            return None;
        }
        self.value_struct_name_of(e)
    }

    /// The value-struct type of `e` IGNORING nullability (`expr_ty` peels
    /// `Nullable`, so a `V?` resolves to `V` here).
    fn value_struct_name_of(&self, e: Idx<Expr>) -> Option<Ustr> {
        let TypeExpr::App { ctor: TypeCtor::Nominal(name), .. } = self.expr_ty(e)? else {
            return None;
        };
        self.is_value_struct(name).then_some(name)
    }

    /// The value-struct type of a NULLABLE whole-value read out of a mutable place
    /// -- a field, a global, a container element. The copy is required exactly as
    /// it is for the non-nullable twin (`value_struct_of(e).filter(|_|
    /// is_mutable_place(base))`); presence is the only difference, and it is
    /// supplied by `nullable_boxed_value_copy`.
    ///
    /// A `nobox` global is excluded: its leaves ARE the storage, so the read is
    /// already a snapshot and there is no box to copy out of. So is a read whose
    /// only consumer is a nil test ([`Builder::presence_only`]).
    fn nullable_place_value_of(&self, e: Idx<Expr>) -> Option<Ustr> {
        if self.presence_only == Some(e) {
            return None;
        }
        if !self.expr_is_nullable(e) {
            return None;
        }
        let is_place = match &self.body.expr_arena[e] {
            Expr::Global(_) => self.nobox_global_place(e).is_none(),
            Expr::Index { .. } => true,
            Expr::FieldAccess { base, .. } => self.is_mutable_place(*base),
            _ => false,
        };
        if !is_place {
            return None;
        }
        self.value_struct_name_of(e)
    }

    /// The nominal type name of a non-nullable AGGREGATE expression -- a value
    /// struct or a cdata-payload enum. [`Self::value_struct_of`] gates on
    /// `is_value_struct`, which excludes enums; `value_shape` accepts both (it
    /// falls back to `enum_value_shape`), so resolving the name is enough and
    /// the shape lookup stays the thing that fails closed on a non-aggregate.
    fn value_aggregate_of(&self, e: Idx<Expr>) -> Option<Ustr> {
        if self.expr_is_nullable(e) {
            return None;
        }
        let TypeExpr::App { ctor: TypeCtor::Nominal(name), .. } = self.expr_ty(e)? else {
            return None;
        };
        Some(name)
    }

    /// Whether binding `b`'s bone type is a NON-nullable value struct -- the one
    /// license for SROA to rematerialize an escaping load of its cell (item 8).
    /// `resolve_tyid` chases `Infer` but keeps `Nullable`, so a `V?` binding is a
    /// `Nullable` node here and returns false; only a bare value-struct `App`
    /// qualifies. Unsolved inference / non-value types return false (conservative).
    fn binding_is_nonnull_value_struct(&self, b: Idx<BindingInfo>) -> bool {
        let Some(ty) = self.binding_ty(b) else { return false };
        let Some(rt) = self.resolve_tyid(ty) else { return false };
        matches!(
            self.ty_node(rt),
            Some(TypeExpr::App { ctor: TypeCtor::Nominal(name), .. }) if self.is_value_struct(name)
        )
    }

    /// Whether `recv`'s static type is a struct with a fn-typed field named
    /// `field` — recovers a `recv.field(args)` UFCS call that bone left as an
    /// unresolved call `field` (it's really an indirect call through the field).
    fn struct_field_is_fn(&self, recv: Idx<Expr>, field: Ustr) -> bool {
        let Some(TypeExpr::App { ctor: TypeCtor::Nominal(sname), .. }) = self.expr_ty(recv) else {
            return false;
        };
        let Some(sid) = self.ws.struct_by_name.get(&sname) else {
            return false;
        };
        let sd = &self.ws.modules[sid.module.index()].structs[sid.def];
        sd.fields
            .iter()
            .find(|f| f.name == field)
            .map(|f| matches!(self.resolve_ty(f.ty), Some(TypeExpr::Function { .. })))
            .unwrap_or(false)
    }

    /// Whether `name` is a module-level global of fn type — recovers `g(args)`
    /// where `g` is a fn-valued global (not a declared function).
    fn is_fn_global(&self, name: Ustr) -> bool {
        self.ws
            .globals_by_name
            .get(&name)
            .copied()
            .map(|ty| matches!(self.resolve_ty(ty), Some(TypeExpr::Function { .. })))
            .unwrap_or(false)
    }

    /// Inject a struct field's default by lowering its initializer from the
    /// struct's `StructDefaults` body (a different body, possibly a different
    /// module) into the current CFG. Self-contained defaults (literals) work; a
    /// default that reads a sibling field gaps (no `SiblingRef` lowering).
    fn lower_struct_default(
        &mut self,
        struct_name: Ustr,
        field: Ustr,
    ) -> Result<Idx<Inst>, CompilerError> {
        let sid = *self
            .ws
            .struct_by_name
            .get(&struct_name)
            .ok_or_else(|| gap(format!("unknown struct `{struct_name}` for default")))?;
        let owner = BodyOwner::StructDefaults(sid.def);
        let defaults_body = self.ws.modules[sid.module.index()]
            .bodies
            .get(&owner)
            .ok_or_else(|| {
                gap(format!(
                    "struct `{struct_name}` field `{field}` omitted but has no default"
                ))
            })?;
        let def_expr = *defaults_body.field_defaults.get(&field).ok_or_else(|| {
            gap(format!(
                "struct `{struct_name}` literal omits field `{field}` (no default)"
            ))
        })?;
        let defaults_inf = self
            .ws
            .inference(sid.module)
            .and_then(|ir| ir.per_fn.get(&owner));
        // Lower the default expr in the defaults body's context, into the current
        // block, then restore.
        let saved_body = self.body;
        let saved_inf = self.inf;
        self.body = defaults_body;
        self.inf = defaults_inf;
        let result = self.lower_expr(def_expr);
        self.body = saved_body;
        self.inf = saved_inf;
        result
    }

    /// An enum variant -> `MakeEnum`. Unit variants have empty payload; tuple
    /// variants fill `positional`; record variants fill `named`.
    /// A cdata-eligible enum (`enum_value_shape`) as a `value`-struct-style Group
    /// `[tag, _0, .., _{n-1}]`: the variant's int tag + its payload values in the
    /// shared positional slots, padding any unused slot with the slot type's zero.
    /// `pack` turns it into `<Enum>_ct(tag, _0, ..)`; the value-struct ABI/SROA/
    /// field machinery do the rest.
    fn enum_cdata_repr(
        &mut self,
        shape: &EnumCdataShape,
        variant: Ustr,
        args: VariantArgs,
    ) -> Result<ValueRepr, CompilerError> {
        let tag = shape.tag_of(variant).ok_or_else(|| gap("enum variant not in shape"))?;
        let tag_inst = self.emit_cur(Inst::Const(Lit::Int(tag)));
        // Map this variant's payload values onto their slot names.
        let mut by_slot: HashMap<Ustr, Idx<Inst>> = HashMap::new();
        match args {
            VariantArgs::Unit => {}
            VariantArgs::Tuple(range) => {
                for (i, v) in self.lower_exprs(range)?.into_iter().enumerate() {
                    by_slot.insert(slot_name(&i.to_string()), v);
                }
            }
            VariantArgs::Record(fields) => {
                for fidx in fields {
                    let lf = self.body.struct_lit_field_arena[fidx];
                    let v = self.lower_expr(lf.value)?;
                    by_slot.insert(slot_name(lf.name.as_str()), v);
                }
            }
        }
        let mut parts = Vec::with_capacity(1 + shape.slots.len());
        parts.push(ValueRepr::Scalar(tag_inst));
        for (nm, c) in &shape.slots {
            let v = match by_slot.get(nm) {
                Some(v) => *v,
                None => {
                    // This variant has no such field -> pad with the slot's zero.
                    let zero = match *c {
                        "float" => Lit::Float(0.0),
                        "bool" => Lit::Bool(false),
                        _ => Lit::Int(0),
                    };
                    self.emit_cur(Inst::Const(zero))
                }
            };
            parts.push(ValueRepr::Scalar(v));
        }
        Ok(ValueRepr::Group {
            name: shape.name,
            parts,
            present: None,
        })
    }

    /// The compile-time expansion kind for `func`, routed through the intrinsic
    /// registry ([`crate::intrinsics::expand_kind`]) -- the ONE recognizer, so
    /// there is no second bespoke annotation check.
    fn fn_expand_kind(&self, func: FuncRef) -> Option<ExpandKind> {
        crate::intrinsics::expand_kind(&self.ws.module(func.module).functions[func.func])
    }

    fn expand_materialize(
        &mut self,
        arg_idxs: &[Idx<Expr>],
    ) -> Result<ValueRepr, CompilerError> {
        let [arg] = arg_idxs else {
            return Err(gap("materialize expects one argument"));
        };
        let repr = self.lower_expr_repr(*arg)?;
        Ok(ValueRepr::Scalar(self.materialize_repr(repr)))
    }

    fn materialize_repr(&mut self, repr: ValueRepr) -> Idx<Inst> {
        match repr {
            ValueRepr::Scalar(value) => value,
            ValueRepr::Tuple { parts, .. } => {
                let vs = parts.into_iter().map(|p| self.materialize_repr(p)).collect();
                self.emit_cur(Inst::MakeTuple(vs))
            }
            ValueRepr::Group { name, parts, .. } => {
                let names = struct_field_names(self.ws, name);
                let fields = parts
                    .into_iter()
                    .enumerate()
                    .map(|(i, part)| {
                        let field = names.get(i).copied().unwrap_or(name);
                        (field, self.materialize_repr(part))
                    })
                    .collect();
                self.emit_cur(Inst::MaterializeValueStruct { name, fields })
            }
        }
    }

    /// Expand a `@struct_expand fn f<T>(u: T) -> [(string, E)]` call into its
    /// literal pairs list. See the free helpers above and
    /// docs/shaders.md (Layer 2).
    fn expand_struct_pairs(
        &mut self,
        call: Idx<Expr>,
        func: FuncRef,
        arg_idxs: &[Idx<Expr>],
    ) -> Result<ValueRepr, CompilerError> {
        let call_span = self.body.expr_spans[call];
        let fname = self.ws.module(func.module).functions[func.func].name;
        let reject = |span: Span, msg: String| CompilerError::new_with_label(span, msg);

        let [arg] = arg_idxs else {
            return Err(reject(
                call_span,
                format!("`@struct_expand` fn `{fname}` takes exactly one argument"),
            ));
        };
        let arg = *arg;
        let arg_span = self.body.expr_spans[arg];

        let ret = self.ws.module(func.module).functions[func.func]
            .return_ty
            .ok_or_else(|| {
                reject(call_span, format!("`@struct_expand` fn `{fname}` must return `[(string, <enum>)]`"))
            })?;
        let enum_name = pairs_return_enum(self.ws, ret).ok_or_else(|| {
            reject(
                call_span,
                format!(
                    "`@struct_expand` fn `{fname}` must return `[(string, <enum>)]`, got `{}`",
                    self.ws.render(None, ret)
                ),
            )
        })?;

        // The argument must be a value struct: its Group's parts are the field
        // values in declaration order, each lowered exactly once.
        let repr = self.lower_expr_repr(arg)?;
        let ValueRepr::Group {
            name: struct_name,
            parts,
            ..
        } = repr
        else {
            return Err(reject(
                arg_span,
                format!("`@struct_expand` fn `{fname}` argument must be a value struct"),
            ));
        };
        let Some(fields) = value_struct_field_tys(self.ws, struct_name) else {
            return Err(reject(
                arg_span,
                format!("`@struct_expand` fn `{fname}` argument must be a value struct, got `{struct_name}`"),
            ));
        };
        // Validation (`struct_expand_rejection`) already gated this program, so a
        // resolution failure here is unreachable -- surface it rather than skip.
        let variants = struct_expand_variants(self.ws, enum_name, &fields)
            .map_err(|msg| reject(arg_span, format!("`@struct_expand`: {msg}")))?;

        let mut tuples = Vec::with_capacity(fields.len());
        for (i, ((field, _), variant)) in fields.iter().zip(variants).enumerate() {
            let payload = self.pack(parts[i].clone());
            // The pairs enum is table-backed when any payload is non-scalar, so a
            // `nobox` payload has nowhere to live but a box -- the same boundary
            // `uniform(x: Vec2)` marks with an explicit `materialize`. Here the
            // compiler mints the box itself and there is no source line to mark,
            // so it mints the AUTHORIZED one rather than rejecting a program the
            // hand-written overload accepts.
            self.authorize_nobox_box(payload);
            let variant_v = self.make_single_payload_variant(enum_name, variant, payload)?;
            let key = self.emit_cur(Inst::Const(Lit::Str(*field)));
            let tup = self.emit_cur(Inst::MakeTuple(vec![key, variant_v]));
            tuples.push(tup);
        }
        Ok(ValueRepr::Scalar(self.emit_cur(Inst::MakeVec(tuples))))
    }

    /// Expand a `rock_shader(vert, frag)` call to its pipeline key const
    /// `"<vert>+<frag>"` (docs/shaders.md). The driver
    /// (`skull::mir_emit`) emits the matching top-level `__rock_shader_register`;
    /// this side produces only the key -- no GLSL, so skull-mir stays free of the
    /// GLSL backend. Rejections flow through the shared `resolve_shader_pair`; the
    /// fatal gate is `shader_expand_rejection`.
    fn expand_shader_pipeline(
        &mut self,
        call: Idx<Expr>,
        arg_idxs: &[Idx<Expr>],
    ) -> Result<ValueRepr, CompilerError> {
        let pair = resolve_shader_pair(self.ws, self.body, call, arg_idxs)?;
        let key = self.emit_cur(Inst::Const(Lit::Str(Ustr::from(pair.key.as_str()))));
        Ok(ValueRepr::Scalar(key))
    }

    /// Construct `enum_name::variant(payload)` for a variant with a single
    /// positional payload, mirroring [`Self::lower_enum_variant`]'s
    /// representation choice (cdata `<Enum>_ct` when the enum is cdata-eligible,
    /// else the boxed `{__variant}` table).
    fn make_single_payload_variant(
        &mut self,
        enum_name: Ustr,
        variant: Ustr,
        payload: Idx<Inst>,
    ) -> Result<Idx<Inst>, CompilerError> {
        if let Some(shape) = enum_value_shape(self.ws, enum_name) {
            let tag = shape
                .tag_of(variant)
                .ok_or_else(|| gap("struct_expand: variant not in cdata enum shape"))?;
            let tag_inst = self.emit_cur(Inst::Const(Lit::Int(tag)));
            let mut parts = vec![ValueRepr::Scalar(tag_inst)];
            for (nm, c) in &shape.slots {
                let v = if *nm == slot_name("0") {
                    payload
                } else {
                    let zero = match *c {
                        "float" => Lit::Float(0.0),
                        "bool" => Lit::Bool(false),
                        _ => Lit::Int(0),
                    };
                    self.emit_cur(Inst::Const(zero))
                };
                parts.push(ValueRepr::Scalar(v));
            }
            return Ok(self.pack(ValueRepr::Group {
                name: enum_name,
                parts,
                present: None,
            }));
        }
        Ok(self.emit_cur(Inst::MakeEnum {
            tag: variant,
            ty: enum_name,
            positional: vec![payload],
            named: vec![],
        }))
    }

    fn lower_enum_variant(
        &mut self,
        enum_name: Ustr,
        variant: Ustr,
        args: VariantArgs,
    ) -> Result<Idx<Inst>, CompilerError> {
        if let Some(shape) = enum_value_shape(self.ws, enum_name) {
            let r = self.enum_cdata_repr(&shape, variant, args)?;
            return Ok(self.pack(r));
        }
        if let VariantArgs::Unit = args {
            // A payload-free enum is an `int` value GLOBALLY (the variant's
            // declaration-order ordinal). Fast int `match`/`==`/Map keys, and
            // C-layout-eligible as a value-struct field. See
            // docs/skull-enum-value-types.md.
            if let Some(ord) = payload_free_ordinal(self.ws, enum_name, variant) {
                return Ok(self.emit_cur(Inst::Const(Lit::Int(ord))));
            }
            // A unit variant of a MIXED enum (e.g. `Option::None`) keeps the
            // INTERNED singleton table (emitted by `enum_runtime_defs`): the
            // enum still has payload variants, so its values stay tables, and the
            // singleton is reference-equal across uses + usable as a Map key.
            let g = Ustr::from(format!("__rock_enum_{enum_name}_{variant}").as_str());
            return Ok(self.emit_cur(Inst::Global(g)));
        }
        let (positional, named) = match args {
            VariantArgs::Unit => (Vec::new(), Vec::new()),
            VariantArgs::Tuple(range) => (self.lower_exprs(range)?, Vec::new()),
            VariantArgs::Record(fields) => {
                let mut named = Vec::new();
                for fidx in fields {
                    let lf = self.body.struct_lit_field_arena[fidx];
                    let v = self.lower_expr(lf.value)?;
                    named.push((lf.name, v));
                }
                (Vec::new(), named)
            }
        };
        Ok(self.emit_cur(Inst::MakeEnum {
            tag: variant,
            ty: enum_name,
            positional,
            named,
        }))
    }

    // --- enum field flattening (docs/enum-allocation-report.md) ---

    /// `Some(shape)` iff `base.field` is a flattenable enum field: `base` is a
    /// non-`value` struct and the field's declared type is a [`flat_enum_shape`]
    /// enum. Drives the construct/store/match/materialize rewrites.
    /// A swizzle read (`v.yx`, `c.rgb` -- docs/vec-swizzles-plan.md): the base's
    /// inferred type is a float vec and `field` is a valid synthetic swizzle for
    /// it. Bone already accepted the read, so this only distinguishes swizzles
    /// from real fields (len-1 `x/y/z/w`) and non-vec bases.
    fn swizzle_of(&self, base: Idx<Expr>, field: Ustr) -> Option<Vec<usize>> {
        if field.len() == 1 && "xyzw".contains(field.as_str()) {
            return None;
        }
        let TypeExpr::App { ctor: TypeCtor::Nominal(sname), .. } = self.expr_ty(base)? else {
            return None;
        };
        bone_semantic::inference::vec_swizzle_components(sname.as_str(), field.as_str())
    }

    /// Component `Field` reads off an already-lowered (packed) vec value, grouped
    /// per `swizzle_group`. The read order is the swizzle's component order.
    fn swizzle_from_packed(&mut self, bv: Idx<Inst>, idxs: &[usize]) -> ValueRepr {
        let comps = ["x", "y", "z", "w"];
        let parts: Vec<ValueRepr> = idxs
            .iter()
            .map(|&i| {
                ValueRepr::Scalar(self.emit_cur(Inst::Field { base: bv, field: Ustr::from(comps[i]) }))
            })
            .collect();
        swizzle_group(parts)
    }

    /// Lower an enum-typed value into its flattened cells: the discriminant
    /// (variant-name string) plus ALL union slots (every positional + every record
    /// field), nil where the live variant doesn't fill them so stale slots from a
    /// prior wider/other-shaped variant are cleared. A direct `E::V(..)` ctor fuses
    /// to a string const + the lowered args (no box); any other enum expression is
    /// lowered to a box once and spread (`__variant` + `[i+1]` + `.field`).
    fn lower_flat_value(
        &mut self,
        shape: &EnumFlatShape,
        value: Idx<Expr>,
    ) -> Result<FlatCells, CompilerError> {
        if let Expr::EnumVariant { enum_name, variant, args, .. } = self.body.expr_arena[value].clone() {
            if enum_name == shape.name {
                let tag = self.emit_cur(Inst::Const(Lit::Str(variant)));
                let (pos_args, rec_args): (Vec<Idx<Inst>>, Vec<(Ustr, Idx<Inst>)>) = match args {
                    VariantArgs::Unit => (Vec::new(), Vec::new()),
                    VariantArgs::Tuple(range) => (self.lower_exprs(range)?, Vec::new()),
                    VariantArgs::Record(fields) => {
                        let mut rec = Vec::new();
                        for fidx in fields {
                            let lf = self.body.struct_lit_field_arena[fidx];
                            let v = self.lower_expr(lf.value)?;
                            rec.push((lf.name, v));
                        }
                        (Vec::new(), rec)
                    }
                };
                let nil = || Inst::Const(Lit::Nil);
                let positional = (0..shape.tuple_width)
                    .map(|i| pos_args.get(i).copied().unwrap_or_else(|| self.emit_cur(nil())))
                    .collect();
                let named = shape
                    .record_fields
                    .iter()
                    .map(|rf| {
                        let v = rec_args
                            .iter()
                            .find(|(n, _)| n == rf)
                            .map(|(_, v)| *v)
                            .unwrap_or_else(|| self.emit_cur(nil()));
                        (*rf, v)
                    })
                    .collect();
                return Ok(FlatCells { tag, positional, named });
            }
        }
        // General enum value: thread it as a repr. A flat-enum Group (a scalarized
        // param/return/local, or a fresh ctor) yields its cells directly -- no box;
        // a Scalar (a genuinely boxed source: global singleton, `any`, if-join) is
        // spread back out.
        let r = self.lower_expr_repr(value)?;
        match r {
            ValueRepr::Group { name, parts, .. } if flat_enum_shape(self.ws, name).is_some() => {
                Ok(self.repr_group_to_cells(shape, parts))
            }
            other => {
                let bv = self.pack(other);
                Ok(self.spread_box_to_cells(bv, shape))
            }
        }
    }

    /// Whether `variant` of `enum_name` is a Unit (payload-free) variant -- its
    /// materialized box is the interned singleton `__rock_enum_<E>_<variant>`.
    fn variant_is_unit(&self, enum_name: Ustr, variant: Ustr) -> bool {
        self.ws
            .enums_by_name
            .get(&enum_name)
            .map(|eid| &self.ws.modules[eid.module.index()].enums[eid.def])
            .and_then(|ed| ed.variants.iter().find(|v| v.name == variant))
            .is_some_and(|v| matches!(v.payload, VariantPayload::Unit))
    }

    /// Whether `e`'s inferred type is a flat enum (`expr_ty` peels `Nullable`, so
    /// this is true for both `E` and `E?`).
    fn expr_is_flat_enum(&self, e: Idx<Expr>) -> bool {
        matches!(
            self.expr_ty(e),
            Some(TypeExpr::App { ctor: TypeCtor::Nominal(n), .. }) if flat_enum_shape(self.ws, n).is_some()
        )
    }

    /// The flat-enum `ValueShape` of `e`'s (non-nullable) inferred type, else `None`
    /// -- the gate for threading a `let`/reassign target as leaf cells.
    fn flat_enum_value_shape_of(&self, e: Idx<Expr>) -> Option<ValueShape> {
        match self.expr_ty(e)? {
            TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } => {
                flat_enum_shape(self.ws, n).map(|s| s.value_shape())
            }
            _ => None,
        }
    }

    /// The `EnumFlatShape` of `e`'s (`Nullable`-peeled) inferred type, else `None`.
    fn flat_enum_shape_of_expr(&self, e: Idx<Expr>) -> Option<EnumFlatShape> {
        match self.expr_ty(e)? {
            TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } => flat_enum_shape(self.ws, n),
            _ => None,
        }
    }

    /// Whether `e`'s inferred type is a `Coroutine<..>` -- the guard that a
    /// `resume(e)` call is a real coroutine resume (not a user fn named `resume`).
    fn is_coroutine_recv(&self, e: Idx<Expr>) -> bool {
        matches!(
            self.expr_ty(e),
            Some(TypeExpr::App { ctor: TypeCtor::Nominal(n), .. }) if n.as_str() == "Coroutine"
        )
    }

    /// Read a resumed flat-enum yield value from the coroutine's frame slots as an
    /// unboxed `Group` -- the concrete `c.resume()` channel. The object IS the
    /// frame: `cobj.__y*` holds the leaves the `__step` wrote; when the coroutine
    /// is terminal (`__state < 0`) the value is null, threaded as an all-nil group
    /// (`tag = nil` sentinel, matching the nullable flat-enum ABI). The consumer
    /// must have already advanced the coroutine (`cobj.__advance(cobj, arg)`)
    /// before this read.
    fn resume_frame_read(&mut self, cobj: Idx<Inst>, shape: &EnumFlatShape) -> ValueRepr {
        let n = shape.leaf_count();
        let frame = cobj;
        let state = self.emit_cur(Inst::Field { base: frame, field: Ustr::from("__state") });
        let zero = self.emit_cur(Inst::Const(Lit::Int(0)));
        let done = self.emit_cur(Inst::IntLt(state, zero));
        let cells: Vec<Idx<Cell>> = (0..n).map(|_| self.f.new_cell(None)).collect();
        let done_bb = self.f.new_block();
        let live_bb = self.f.new_block();
        let join = self.f.new_block();
        self.f.set_term(self.cur, Term::Branch { cond: done, then_bb: done_bb, else_bb: live_bb });
        // Terminal: all-nil leaves (tag = nil => `== null`).
        self.cur = done_bb;
        for c in &cells {
            let z = self.emit_cur(Inst::Const(Lit::Nil));
            self.emit_cur(Inst::Store { cell: *c, value: z });
        }
        self.f.set_term(self.cur, Term::Goto(join));
        // Live: spread the reused frame slots into the leaf cells.
        self.cur = live_bb;
        for (i, c) in cells.iter().enumerate() {
            let v = self.emit_cur(Inst::Field { base: frame, field: yield_slot(i) });
            self.emit_cur(Inst::Store { cell: *c, value: v });
        }
        self.f.set_term(self.cur, Term::Goto(join));
        self.cur = join;
        let leaves: Vec<Idx<Inst>> = cells.iter().map(|c| self.emit_cur(Inst::Load(*c))).collect();
        let mut idx = 0;
        self.rebuild_from_leaves(&shape.value_shape(), &leaves, &mut idx)
    }

    /// Split a flat-enum `ValueRepr::Group` into its cells (the inverse of
    /// `flat_cells_to_repr`) -- no box. Parts are leaf Scalars in `value_shape`
    /// order: `[tag, _0.., <rec>..]`.
    fn repr_group_to_cells(&mut self, shape: &EnumFlatShape, parts: Vec<ValueRepr>) -> FlatCells {
        let packed: Vec<Idx<Inst>> = parts.into_iter().map(|p| self.pack(p)).collect();
        let tag = packed[0];
        let positional = packed[1..1 + shape.tuple_width].to_vec();
        let named = shape
            .record_fields
            .iter()
            .enumerate()
            .map(|(i, rf)| (*rf, packed[1 + shape.tuple_width + i]))
            .collect();
        FlatCells { tag, positional, named }
    }

    /// A flat enum's runtime cells as a `ValueRepr::Group` in `value_shape` leaf
    /// order (`[tag, _0.., <rec>..]`) -- the scalar-ABI carrier for a flat enum.
    fn flat_cells_to_repr(&self, shape: &EnumFlatShape, cells: FlatCells) -> ValueRepr {
        let mut parts = Vec::with_capacity(shape.leaf_count());
        parts.push(ValueRepr::Scalar(cells.tag));
        parts.extend(cells.positional.into_iter().map(ValueRepr::Scalar));
        parts.extend(cells.named.into_iter().map(|(_, c)| ValueRepr::Scalar(c)));
        ValueRepr::Group {
            name: shape.name,
            parts,
            present: None,
        }
    }

    /// Read a boxed enum's keys back out into flattened cells (`__variant` -> tag,
    /// `[i+1]` -> positional, `.field` -> named) -- the box-to-slots spread.
    fn spread_box_to_cells(&mut self, bv: Idx<Inst>, shape: &EnumFlatShape) -> FlatCells {
        let tag = self.emit_cur(Inst::Field { base: bv, field: Ustr::from("__variant") });
        let positional = (0..shape.tuple_width)
            .map(|i| self.emit_cur(Inst::TupleIndex { base: bv, index: i }))
            .collect();
        let named = shape
            .record_fields
            .clone()
            .into_iter()
            .map(|rf| (rf, self.emit_cur(Inst::Field { base: bv, field: rf })))
            .collect();
        FlatCells { tag, positional, named }
    }

    /// Re-materialize a flattened enum field `parent.{prefix}` to a boxed enum
    /// (`MakeEnumDyn`) at a whole-value-read boundary.
    fn materialize_flat(&mut self, parent: Idx<Inst>, prefix: Ustr, shape: &EnumFlatShape) -> Idx<Inst> {
        let tag = self.emit_cur(Inst::Field { base: parent, field: flat_tag_slot(prefix) });
        let positional = (0..shape.tuple_width)
            .map(|i| self.emit_cur(Inst::Field { base: parent, field: flat_payload_slot(prefix, i) }))
            .collect();
        let named = shape
            .record_fields
            .clone()
            .into_iter()
            .map(|rf| (rf, self.emit_cur(Inst::Field { base: parent, field: flat_named_slot(prefix, rf) })))
            .collect();
        self.emit_cur(Inst::MakeEnumDyn { tag, ty: shape.name, positional, named })
    }

    /// Emit the per-slot stores for a flattened enum field assignment
    /// `parent.{prefix} = <cells>` (tag + every positional + every named slot).
    fn store_flat_cells(&mut self, parent: Idx<Inst>, prefix: Ustr, cells: FlatCells) {
        self.emit_cur(Inst::StoreField { base: parent, field: flat_tag_slot(prefix), value: cells.tag });
        for (i, p) in cells.positional.into_iter().enumerate() {
            self.emit_cur(Inst::StoreField { base: parent, field: flat_payload_slot(prefix, i), value: p });
        }
        for (rf, v) in cells.named {
            self.emit_cur(Inst::StoreField { base: parent, field: flat_named_slot(prefix, rf), value: v });
        }
    }

    /// The `(slot_name, value)` pairs for a flattened enum field in a struct literal
    /// (tag + every positional + every named slot), keyed by the parent field name.
    fn flat_literal_slots(&self, field: Ustr, cells: FlatCells) -> Vec<(Ustr, Idx<Inst>)> {
        let mut out = Vec::with_capacity(1 + cells.positional.len() + cells.named.len());
        out.push((flat_tag_slot(field), cells.tag));
        for (i, p) in cells.positional.into_iter().enumerate() {
            out.push((flat_payload_slot(field, i), p));
        }
        for (rf, v) in cells.named {
            out.push((flat_named_slot(field, rf), v));
        }
        out
    }

    fn expr_is_flat_vec(&self, e: Idx<Expr>) -> bool {
        matches!(self.expr_ty(e),
            Some(TypeExpr::App { ctor: TypeCtor::Nominal(n), .. }) if n.as_str() == "FlatVec")
    }

    /// The value-STRUCT element of `e`'s `NativeBuffer<E>` / `FlatVec<E>` type,
    /// or `None` when `E` is a primitive / unresolved generic / not value-shaped.
    /// Drives [`ContainerKind::NativeBufferValue`]; an opaque generic `E` stays
    /// plain `NativeBuffer` (matching the alloc intrinsic, which also requires a
    /// concrete element).
    fn nb_value_elem(&self, e: Idx<Expr>) -> Option<Ustr> {
        let ty = self.resolve_tyid(*self.inf?.expr_types.get(e)?)?;
        let TypeExpr::App { ctor: TypeCtor::Nominal(n), params } = self.ty_node(ty)? else {
            return None;
        };
        if !matches!(n.as_str(), "NativeBuffer" | "FlatVec") {
            return None;
        }
        let p = params.into_iter().next()?;
        let elem = self.resolve_tyid(ty.with_idx(p))?;
        let en = match self.ty_node(elem)? {
            TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } => n,
            // Resolve through the active specialization, exactly as
            // `vec_column_elem_of` does. Inside a SPECIALIZED body (the `@inline`
            // FlatVec surface at a concrete site) the element is the real struct
            // rather than an opaque `T`; without this the stdlib bodies keep the
            // boxed path, which for a `nobox` element is not merely slower but a
            // guarantee violation. An UNSUBSTITUTED generic stays boxed on
            // purpose -- the shared snapshot must keep the representation every
            // instantiation can read.
            TypeExpr::Generic(var, _) => match self.spec_subst.get(&var) {
                Some(ty) => ty.nominal()?,
                _ => return None,
            },
            _ => return None,
        };
        is_ffi_clean_value_struct(self.ws, en).then_some(en)
    }

    /// The column-eligible element of `e`'s `Arena<E>` type, if any. The twin of
    /// [`Self::vec_column_elem_of`] one container out: an arena's `data` field is
    /// an ordinary `[E]`, so it columnizes on exactly the same rule, and the
    /// hardcoded `for`-over-arena walk has to read it the same way the Vec path
    /// would.
    fn arena_column_elem_of(&self, e: Idx<Expr>) -> Option<Ustr> {
        let ty = self.resolve_tyid(*self.inf?.expr_types.get(e)?)?;
        let TypeExpr::App { ctor: TypeCtor::Nominal(_), params } = self.ty_node(ty)? else {
            return None;
        };
        let p = params.into_iter().next()?;
        let elem = self.resolve_tyid(ty.with_idx(p))?;
        let en = match self.ty_node(elem)? {
            TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } => n,
            TypeExpr::Generic(var, _) => match self.spec_subst.get(&var) {
                Some(ty) => ty.nominal()?,
                _ => return None,
            },
            _ => return None,
        };
        vec_column_elem(self.ws, en, false)
    }

    /// The Vec container kind for base expr `e`: an all-scalar value-struct
    /// element stores as flat columns, everything else keeps the single `_data`
    /// backing. An unresolved / generic element is NOT eligible -- the columns
    /// need a concrete value-struct leaf list, and an opaque `T` has none.
    fn vec_kind(&self, e: Idx<Expr>) -> ContainerKind {
        match self.vec_column_elem_of(e) {
            Some(elem) => ContainerKind::VecValueColumns(elem),
            None => ContainerKind::Vec,
        }
    }

    /// The column-eligible value-struct element of `e`'s `[E]` type, if any.
    ///
    /// Resolves through the active [`SpecSubst`], so inside a SPECIALIZED body
    /// (`VecIter<Vec2>.next`, `fn f<T>(xs: [T])` at a `[Vec2]` site) the element
    /// is the concrete struct rather than an opaque `T`. Without this the
    /// stdlib iterator lowers `it.vec[it.i]` against the boxed representation
    /// and reads a `_data` that column storage does not have.
    fn vec_column_elem_of(&self, e: Idx<Expr>) -> Option<ColumnElem> {
        let ty = self.resolve_tyid(*self.inf?.expr_types.get(e)?)?;
        let TypeExpr::App { ctor: TypeCtor::Array, params } = self.ty_node(ty)? else {
            return None;
        };
        let p = params.into_iter().next()?;
        let mut elem = self.resolve_tyid(ty.with_idx(p))?;
        // A NULLABLE element columnizes only when the payload is `nobox`. For an
        // unmarked value struct the boxed element is a legal representation and
        // already works, while columnizing it would newly route the materializing
        // helpers (`__rock_col_sort`, `to_lua_array`, serialization) at a shape
        // they have no presence concept for. The mark is what makes the box
        // forbidden, so the mark is what obliges the presence column.
        //
        // Asked BEFORE the peel, and through the one nullability query, so a
        // spelled-out `[E?]` and a generic bound to `E?` answer alike -- the
        // latter's `?` lives in the binding, and `ty_node` substitutes the
        // payload over it.
        let nullable = self.is_nullable_ty(elem);
        if let Some(TypeExpr::Nullable(inner)) = self.ty_node(elem) {
            elem = self.resolve_tyid(elem.with_idx(inner))?;
        }
        let en = match self.ty_node(elem)? {
            TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } => n,
            // An UNSUBSTITUTED generic element stays boxed: the shared
            // (non-specialized) snapshot of a generic body must keep the
            // representation that every instantiation can read.
            TypeExpr::Generic(var, _) => match self.spec_subst.get(&var) {
                Some(ty) => ty.peel_nullable().0.nominal()?,
                _ => return None,
            },
            _ => return None,
        };
        if nullable && !is_nobox_nominal(self.ws, en) {
            return None;
        }
        vec_column_elem(self.ws, en, nullable).map(|name| ColumnElem { name, nullable })
    }

    /// The column shape of `elem`: its value shape, marked nullable when the
    /// ELEMENT type is. Every column site builds its shape here, so the leaf
    /// wire (`repr_to_leaves` / `rebuild_from_leaves`, which both lead a nullable
    /// shape with presence) and the column keys ([`column_leaf_paths`]) can only
    /// agree.
    fn column_shape(&self, elem: ColumnElem) -> Result<ValueShape, CompilerError> {
        let mut shape = value_shape(self.ws, elem.name)
            .ok_or_else(|| gap("column element is not a value struct"))?;
        shape.nullable = elem.nullable;
        Ok(shape)
    }

    /// The column-eligible value-struct VALUE of `e`'s `Map<K, V>` type, if any.
    ///
    /// The Map sibling of [`Self::vec_column_elem_of`], resolving through the
    /// active [`SpecSubst`] for the same reason: inside a specialized generic
    /// body the value must read as the concrete struct, or the body lowers
    /// against a bare table the column representation does not have.
    ///
    /// The KEY is deliberately not consulted -- a value-struct key rejects
    /// upstream (LuaJIT hashes cdata by pointer), and the spine stores whatever
    /// key the boxed representation already stored.
    fn map_column_value_of(&self, e: Idx<Expr>) -> Option<ColumnElem> {
        let ty = self.resolve_tyid(*self.inf?.expr_types.get(e)?)?;
        let TypeExpr::App { ctor: TypeCtor::Map, params } = self.ty_node(ty)? else {
            return None;
        };
        let v = params.into_iter().nth(1)?;
        let mut val = self.resolve_tyid(ty.with_idx(v))?;
        // `Map<K, P?>` is column-stored too, and unlike `[P?]` it needs no
        // presence column: an absent value collapses to key REMOVAL, because a
        // bare Map already cannot tell an absent key from a key holding null.
        if let Some(TypeExpr::Nullable(inner)) = self.ty_node(val) {
            val = self.resolve_tyid(val.with_idx(inner))?;
        }
        let vn = match self.ty_node(val)? {
            TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } => n,
            TypeExpr::Generic(var, _) => match self.spec_subst.get(&var) {
                Some(ty) => ty.nominal()?,
                _ => return None,
            },
            _ => return None,
        };
        // `nullable: false` is the point of the peel above -- the spine carries
        // presence, so the value columns never need a presence leaf.
        vec_column_elem(self.ws, vn, false).map(|name| ColumnElem { name, nullable: false })
    }

    /// `m.data`, the column Vec a column-stored Map's values live in. Every
    /// leaf read/write goes through the ordinary Vec column machinery on this.
    fn map_col_data(&mut self, m: Idx<Inst>) -> Idx<Inst> {
        self.emit_cur(Inst::Field { base: m, field: Ustr::from("data") })
    }

    /// The NativeBuffer container kind for base expr `e`: value-struct elements
    /// carry their name so the JS emitter can lower flat slots (Lua identical).
    fn nb_kind(&self, e: Idx<Expr>) -> ContainerKind {
        match self.nb_value_elem(e) {
            Some(elem) => ContainerKind::NativeBufferValue(elem),
            None => ContainerKind::NativeBuffer,
        }
    }

    /// Lower an index-expr base to `(base inst, container kind)`. A
    /// `FlatVec<T>` base indexes its `buf` NativeBuffer storage -- project the
    /// field and index with NativeBuffer semantics, so the value-element slot
    /// machinery and debug bounds check apply unchanged. Note `fv[i]` is
    /// therefore CAPACITY-checked in debug; `fv.get(i)` is the len-checked
    /// accessor.
    fn lower_index_base(
        &mut self,
        base: Idx<Expr>,
    ) -> Result<(Idx<Inst>, ContainerKind), CompilerError> {
        if self.expr_is_flat_vec(base) {
            let kind = self.nb_kind(base);
            let b = self.lower_expr(base)?;
            let buf = self.emit_cur(Inst::Field { base: b, field: Ustr::from("buf") });
            return Ok((buf, kind));
        }
        let kind = self.coll_kind(base);
        let b = self.lower_expr(base)?;
        Ok((b, kind))
    }

    /// The collection kind of a base expression, from its inferred type — drives
    /// the index/length shape (decision 9). bone's index/iter whitelist is closed
    /// (`Vec`/`Map`/`NativeBuffer`+sized), so any other type reaching here is a
    /// front-end/back-end disagreement, not a program the user could write.
    fn coll_kind(&self, base: Idx<Expr>) -> ContainerKind {
        match self.expr_ty(base) {
            Some(TypeExpr::App {
                ctor: TypeCtor::Array,
                ..
            }) => self.vec_kind(base),
            Some(TypeExpr::App {
                ctor: TypeCtor::Map,
                ..
            }) => ContainerKind::Map,
            Some(TypeExpr::App {
                ctor: TypeCtor::Nominal(name),
                ..
            }) if is_native_buffer(name.as_str()) => self.nb_kind(base),
            _ => {
                let rendered = self
                    .inf
                    .and_then(|i| i.expr_types.get(base).copied())
                    .map(|t| self.ws.render(self.inf, t))
                    .unwrap_or_else(|| "<unknown>".to_string());
                unreachable!(
                    "coll_kind: expected an indexable container (Vec/Map/NativeBuffer); \
                     bone admitted an index/iter on `{rendered}`"
                )
            }
        }
    }

    // --- type queries (decision: ops are typed at BUILD time) ---

    fn arith_ty(&self, e: Idx<Expr>) -> Option<ArithTy> {
        self.arith_ty_of(self.expr_ty(e)?)
    }

    /// int->float subsumption funnel. Wraps `value` in [`Inst::IntToFloat`] iff
    /// bone typed `value_expr` int and `expected` resolves to float -- the single
    /// sink for general int->float coercion (`fn f(i: int) -> float { return i }`,
    /// `takes_float(int_expr)`, an int literal in a float slot). Fires ONLY
    /// int-expr-in-float-slot; never int->int (loop counters/bounds, idiv/imod
    /// operands, int==int stay untouched -- the counted-loop latch recognition
    /// keeps matching `Const(Lit::Int(1))`). Identity on luajit; materialized by
    /// non-Lua backends. See `docs/shaders.md`.
    fn coerced(&mut self, value: Idx<Inst>, value_expr: Idx<Expr>, expected: TyId) -> Idx<Inst> {
        let to_float = self.resolve_ty(expected).and_then(|t| self.arith_ty_of(t)) == Some(ArithTy::Float);
        self.coerce_i2f(value, self.arith_ty(value_expr) == Some(ArithTy::Int), to_float)
    }

    /// A variable binding's type: the inferred type, falling back to the declared
    /// annotation. The expected type at a `let`/assignment coercion sink and the
    /// [`Cell::ty`] for a binding cell.
    fn binding_ty(&self, name: Idx<BindingInfo>) -> Option<TyId> {
        self.inf
            .and_then(|i| i.binding_types.get(name).copied())
            .or(self.body.binding_arena[name].declared_ty)
    }

    /// [`Self::coerced`] against a `let`/assignment target binding's type.
    fn coerced_to_binding(&mut self, value: Idx<Inst>, value_expr: Idx<Expr>, name: Idx<BindingInfo>) -> Idx<Inst> {
        match self.binding_ty(name) {
            Some(t) => self.coerced(value, value_expr, t),
            None => value,
        }
    }

    /// [`Self::coerced`] keyed on already-computed operand classes -- the scalar-op
    /// path knows the operand's [`ArithTy`] and whether the resolved op is float,
    /// but has no single expected `TyId` to pass to `coerced`.
    fn coerce_i2f(&mut self, value: Idx<Inst>, from_int: bool, to_float: bool) -> Idx<Inst> {
        if from_int && to_float {
            self.emit_cur(Inst::IntToFloat(value))
        } else {
            value
        }
    }

    /// The declared type of a callee's positional parameter `pidx` (the expected
    /// type at a call-arg coercion sink), or `None` past the fixed arity.
    fn param_ty(&self, func: FuncRef, pidx: usize) -> Option<TyId> {
        self.ws.module(func.module).functions[func.func]
            .param_decls
            .get(pidx)
            .map(|pd| pd.ty)
    }

    /// The caller reaches this only when [`fn_scalar_abi_spec`] returns `None`.
    /// Under `values_first_class`, a `value` struct is already a first-class
    /// `vecN`, so there is no box to authorize.
    /// Re-mark a nobox box packed for a `@gpu_only` callee as an EXPLICIT one.
    ///
    /// Emission is identical (both render `<name>_ct`), the verifier ignores
    /// `MaterializeValueStruct`, and the flatten pass already treats it as
    /// sanctioned -- so this authorizes exactly the box the GPU boundary needs
    /// through the channel that already exists for authorized boxes, instead of
    /// switching the guarantee off for the calling body.
    ///
    /// Args are lowered per call site, so each box here is this call's own.
    fn authorize_gpu_boundary_boxes(&mut self, func: &FuncRef, args: &[Idx<Inst>]) {
        let callee = &self.ws.module(func.module).functions[func.func];
        if !crate::intrinsics::is_gpu_only(callee) {
            return;
        }
        for &a in args {
            self.authorize_nobox_box(a);
        }
    }

    /// Re-mark one implicit nobox box as AUTHORIZED, in place. The single
    /// conversion point: every boundary the compiler MINTS a sanctioned box at
    /// goes through here, so "which boxes are authorized" is one list of callers
    /// rather than a repeated `MakeValueStruct` -> `MaterializeValueStruct`
    /// rewrite. A no-op on anything that is not an implicit box of a `nobox`
    /// struct.
    fn authorize_nobox_box(&mut self, inst: Idx<Inst>) {
        let Inst::MakeValueStruct { name, fields } = &self.f.insts[inst] else { return };
        let Some(sid) = self.ws.struct_by_name.get(name) else { return };
        if !self.ws.modules[sid.module.index()].structs[sid.def].is_nobox {
            return;
        }
        let (name, fields) = (*name, fields.clone());
        self.f.insts[inst] = Inst::MaterializeValueStruct { name, fields };
    }

    fn reject_nobox_boxed_call_args(
        &self,
        func: FuncRef,
        args: &[Idx<Expr>],
    ) -> Result<(), CompilerError> {
        if self.scx.values_first_class {
            return Ok(());
        }
        let callee = &self.ws.module(func.module).functions[func.func];
        // A `@gpu_only` intrinsic's boxed param is not a real boundary: on the
        // GPU the arg is a native `vec2`/`vec4` and no box exists, and the Lua
        // body only `error(...)`s. Rather than exempting the CALLER's whole
        // body, the box is AUTHORIZED at this one boundary
        // ([`Self::authorize_gpu_boundary_boxes`]) -- the guarantee keeps
        // applying to everything else the caller does.
        // Rationale for shader work: docs/shaders.md, "nobox values and shaders".
        if crate::intrinsics::is_gpu_only(callee) {
            return Ok(());
        }
        for &arg in args {
            if self.is_explicit_materialize_call(arg) {
                continue;
            }
            let Some(TypeExpr::App {
                ctor: TypeCtor::Nominal(name),
                ..
            }) = self.expr_ty(arg)
            else {
                continue;
            };
            let Some(sid) = self.ws.struct_by_name.get(&name) else {
                continue;
            };
            if self.ws.modules[sid.module.index()].structs[sid.def].is_nobox {
                return Err(CompilerError::new_with_label(
                    self.body.expr_spans[arg],
                    format!(
                        "nobox value `{name}` cannot cross boxed call boundary `{}`",
                        callee.name
                    ),
                ));
            }
        }
        Ok(())
    }

    /// The return-position companion of [`Self::reject_nobox_boxed_call_args`]:
    /// a `nobox` element only rides a tuple return as leaves if skull emits the
    /// callee's body. An `extern`, or an `inline(lua)` body that has not opted
    /// in with `@scalar_abi`, writes its own `return` -- so the call site would
    /// read leaves the body never produced. That is a boxed boundary; reject it
    /// where an argument would be rejected.
    fn reject_nobox_opaque_tuple_return(
        &self,
        e: Idx<Expr>,
        func: FuncRef,
        shape: &TupleRetShape,
    ) -> Result<(), CompilerError> {
        if self.scx.values_first_class || !shape.splats() {
            return Ok(());
        }
        let callee = &self.ws.module(func.module).functions[func.func];
        let opaque = callee.is_extern
            || (callee.inline_body.is_some()
                && !callee.annotations.iter().any(|a| a.name.name.as_str() == "scalar_abi"));
        if !opaque {
            return Ok(());
        }
        Err(CompilerError::new_with_label(
            self.body.expr_spans[e],
            format!(
                "nobox value cannot cross the boxed tuple return of `{}` \
                 (an `inline(lua)` body must opt in with `@scalar_abi` and return the leaves)",
                callee.name
            ),
        ))
    }

    /// The BARE-return companion of [`Self::reject_nobox_opaque_tuple_return`].
    /// That one covers `-> (bool, NV2)`; a plain `-> NV2` had no check in any
    /// position, so an opaque body handing back a hand-built box compiled clean
    /// AND PASSED -- Lua's `.field` does not distinguish a table from cdata, so
    /// the guarantee was violated with no diagnostic. Live shape:
    /// `stone_measure_text_vec2_for` and its `inline(js)` twin.
    ///
    /// Keyed on the SIGNATURE, which is transport-independent -- the JS backend
    /// packs leaves into an array rather than multi-returning them, and this
    /// check is correct for both.
    fn reject_nobox_opaque_value_return(
        &self,
        e: Idx<Expr>,
        func: FuncRef,
    ) -> Result<(), CompilerError> {
        if self.scx.values_first_class {
            return Ok(());
        }
        let callee = &self.ws.module(func.module).functions[func.func];
        // Same authorization as the arg direction: a `@gpu_only` intrinsic's
        // boxed return is not a real boundary (on the GPU it is a native vector,
        // and the Lua body only `error(...)`s).
        if crate::intrinsics::is_gpu_only(callee) {
            return Ok(());
        }
        let opaque = callee.is_extern
            || (callee.inline_body.is_some()
                && !callee.annotations.iter().any(|a| a.name.name.as_str() == "scalar_abi"));
        if !opaque {
            return Ok(());
        }
        let Some(ret) = callee.return_ty else {
            return Ok(());
        };
        let Some(TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            ..
        }) = static_ty(self.ws, ret)
        else {
            return Ok(());
        };
        let Some(sid) = self.ws.struct_by_name.get(&name) else {
            return Ok(());
        };
        if !self.ws.modules[sid.module.index()].structs[sid.def].is_nobox {
            return Ok(());
        }
        Err(CompilerError::new_with_label(
            self.body.expr_spans[e],
            format!(
                "nobox value `{name}` cannot cross the boxed return of `{}` \
                 (an `inline(lua)` body must opt in with `@scalar_abi` and return the leaves)",
                callee.name
            ),
        ))
    }

    fn is_explicit_materialize_call(&self, expr: Idx<Expr>) -> bool {
        let Expr::Call {
            callee: Callee::Unresolved { name, .. },
            ..
        } = self.body.expr_arena[expr]
        else {
            return false;
        };
        self.resolve_funcref(expr, name)
            .ok()
            .is_some_and(|func| matches!(self.fn_expand_kind(func), Some(ExpandKind::Materialize)))
    }

    /// A (value) struct field's declared type, for the value-struct-literal
    /// construction sink (`Vec2 { x: 0 }` -> the `x` field coerces int->float).
    fn struct_field_ty(&self, name: Ustr, field: Ustr) -> Option<TyId> {
        let sid = *self.ws.struct_by_name.get(&name)?;
        let sd = &self.ws.modules[sid.module.index()].structs[sid.def];
        sd.fields.iter().find(|f| f.name == field).map(|f| f.ty)
    }

    /// Coerce a value-struct field's repr int->float at CONSTRUCTION (before any
    /// scalar-ABI splat, per the plan): only a `Scalar` leaf can be an int; a
    /// `Group` / nested-value repr is already composite.
    fn coerce_field_repr(&mut self, r: ValueRepr, value_expr: Idx<Expr>, struct_name: Ustr, field: Ustr) -> ValueRepr {
        match r {
            ValueRepr::Scalar(s) => match self.struct_field_ty(struct_name, field) {
                Some(fty) => ValueRepr::Scalar(self.coerced(s, value_expr, fty)),
                None => ValueRepr::Scalar(s),
            },
            other => other,
        }
    }


    /// The scalar class of a resolved type, seeing through a TRANSPARENT newtype
    /// to its wrapped type (a `newtype Score(int)` is the raw int at runtime, so
    /// `<`/`+`/... on it are native int ops — matching its identity lowering).
    fn arith_ty_of(&self, te: TypeExpr) -> Option<ArithTy> {
        match te {
            TypeExpr::Builtin(BuiltinTy::Int) => Some(ArithTy::Int),
            TypeExpr::Builtin(BuiltinTy::Float) => Some(ArithTy::Float),
            TypeExpr::Builtin(BuiltinTy::String) => Some(ArithTy::Str),
            TypeExpr::App { ctor: TypeCtor::Nominal(name), .. }
                if self.ws.newtypes_by_name.contains_key(&name) =>
            {
                let nid = self.ws.newtypes_by_name.get(&name)?;
                let wrapped = self.ws.modules[nid.module.index()].newtypes[nid.def].wrapped;
                self.arith_ty_of(self.resolve_ty(wrapped)?)
            }
            _ => None,
        }
    }

    fn eq_is_scalar(&self, e: Idx<Expr>) -> bool {
        matches!(
            self.expr_ty(e),
            Some(TypeExpr::Builtin(
                BuiltinTy::Int | BuiltinTy::Float | BuiltinTy::Bool | BuiltinTy::String
            ))
        ) || self.is_payloadless_enum(e)
    }

    fn nobox_value_name(&self, e: Idx<Expr>) -> Option<Ustr> {
        let TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            ..
        } = self.expr_ty(e)?
        else {
            return None;
        };
        self.is_nobox_struct(name).then_some(name)
    }

    /// Under `values_first_class` NOTHING is nobox: the flag says the target has no
    /// boxed representation to avoid, so a `value` struct is already a first-class
    /// `vecN` and there is no leaf form to prefer. Answering `true` here would put
    /// a local into leaf cells named `<name>__<i>`, which is both a duplicated
    /// initializer and a RESERVED GLSL identifier (spec 3.7).
    ///
    /// The gate lives on the predicate rather than on each shape helper because
    /// every representation decision reaches nobox-ness through here -- binding
    /// shapes, expression shapes, signature splat gates and the column-storage
    /// rejections alike. Gating one consumer leaves the others to remember it, and
    /// they did not: `Statement::Let` falls through `nobox_binding_shape` to
    /// `nobox_value_shape_of`, so gating the binding shape alone still emitted
    /// `shift__0`.
    fn is_nobox_struct(&self, name: Ustr) -> bool {
        if self.scx.values_first_class {
            return false;
        }
        self.ws
            .struct_by_name
            .get(&name)
            .is_some_and(|sid| self.ws.modules[sid.module.index()].structs[sid.def].is_nobox)
    }

    fn nobox_value_shape_of(&self, e: Idx<Expr>) -> Option<ValueShape> {
        let mut shape = value_shape_of_nominal(self.ws, self.nobox_value_name(e)?)?;
        shape.nullable = self.expr_is_nullable(e);
        Some(shape)
    }

    fn nobox_binding_shape(&self, binding: Idx<BindingInfo>) -> Option<ValueShape> {
        self.nobox_shape_of_ty(self.binding_ty(binding)?)
    }

    /// The leaf shape of a `nobox`-typed `ty` (nullable included: presence is
    /// leaf 0), `None` for every other type.
    /// Argument positions of a `ShaderPipeline` expansion, where a bare fn name is
    /// consumed by the compiler and never becomes a runtime fn value:
    /// `rock_shader(frag)` lowers its argument to the entry's NAME as a string
    /// literal, so no function-value ABI applies there.
    fn expansion_arg_exprs(&self) -> HashSet<Idx<Expr>> {
        let mut out = HashSet::new();
        for (e, expr) in self.body.expr_arena.iter() {
            let Expr::Call { callee, args } = expr else { continue };
            let Callee::Unresolved { name, .. } = callee else { continue };
            let Ok(func) = self.resolve_funcref(e, *name) else { continue };
            // ONLY `ShaderPipeline`. It resolves its argument to a fn NAME and emits a
            // string literal, so the name never becomes a runtime value. The sibling
            // kinds evaluate theirs -- `expand_materialize` calls `lower_expr_repr` on
            // it -- so exempting them let `materialize(<nobox-carrying fn>)` bypass the
            // ABI rejection entirely, which is a hole, not a narrowing.
            if matches!(self.fn_expand_kind(func), Some(ExpandKind::ShaderPipeline)) {
                out.extend(args.clone().into_iter());
            }
        }
        out
    }

    fn reject_incompatible_nobox_fn_value_abi(&self) -> Result<(), CompilerError> {
        if self.scx.values_first_class {
            return Ok(());
        }
        let expansion_args = self.expansion_arg_exprs();
        for (e, expr) in self.body.expr_arena.iter() {
            let Expr::Global(name) = expr else { continue };
            if expansion_args.contains(&e) {
                continue;
            }
            let Some(overloads) = self.ws.functions_by_name.get(name) else { continue };
            let incompatible = overloads.iter().any(|(mid, fid)| {
                let fref = FuncRef { module: *mid, func: *fid };
                let expected = fn_nobox_abi_spec(self.ws, fref, &SpecSubst::new());
                expected.is_some() && fn_scalar_abi(self.ws, self.scx, fref) != expected
            });
            if incompatible {
                return Err(CompilerError::new_with_label(
                    self.body.expr_spans[e],
                    format!(
                        "`{name}` cannot be used as a fn value because its fixed ABI does not \
                         match the type-derived `nobox` fn-value ABI"
                    ),
                ));
            }
        }
        Ok(())
    }

    /// The leaf ABI of a FN TYPE, or `None` when nothing in the signature is a
    /// `nobox` value and the boxed fn-value ABI stands unchanged.
    ///
    /// Keyed on the TYPE and nothing else. An indirect call site cannot see its
    /// callee, so the definition, a direct call and a call through a fn value
    /// can only agree by all three reading the same signature -- the rule the
    /// tuple-return ABI already follows (`lambda_ret_tuple_shape` next to
    /// `expr_tuple_shape`). Any callee-identity test here would make the two
    /// ends disagree exactly where the callee is opaque, which is the case that
    /// matters.
    ///
    /// Narrowed to `nobox` types on purpose: an ordinary value struct has no
    /// guarantee to keep, so widening this to every FFI-clean value would
    /// re-ABI every lambda in the tree for no correctness gain. Both ends
    /// compute the same predicate from the same type, so they still agree.
    fn fn_value_abi(&self, ty: TyId) -> Option<(Vec<Option<ValueShape>>, Option<ValueShape>)> {
        let ty = self.resolve_tyid(ty)?;
        let TypeExpr::Function { params, ret } = self.ty_node(ty)? else { return None };
        let ps: Vec<Option<ValueShape>> =
            params.into_iter().map(|p| self.nobox_shape_of_ty(ty.with_idx(p))).collect();
        let rs = self.nobox_shape_of_ty(ty.with_idx(ret));
        if rs.is_none() && ps.iter().all(Option::is_none) {
            return None;
        }
        Some((ps, rs))
    }

    /// [`Self::fn_value_abi`] for the fn VALUE an expression denotes.
    fn expr_fn_value_abi(
        &self,
        e: Idx<Expr>,
    ) -> Option<(Vec<Option<ValueShape>>, Option<ValueShape>)> {
        self.fn_value_abi(*self.inf?.expr_types.get(e)?)
    }

    fn nobox_shape_of_ty(&self, ty: TyId) -> Option<ValueShape> {
        // The BODY arena's spelling of [`nullable_nominal_shape`]: same rule, its
        // own type-node lookup. `is_nobox_struct` (not `is_nobox_nominal`) is the
        // one real difference -- it also honours `scx.values_first_class`, which
        // the GLSL backend sets -- so the guard stays here and the core sees an
        // already-filtered nominal.
        if !self.scx.values_first_class {
            let mut shape = nullable_nominal_shape(self.ws, ty, &|t| {
                self.ty_node(self.resolve_tyid(t)?)
            })?;
            // A generic bound to `E?` reaches the core as its payload nominal, so
            // the `?` has to be re-applied from the binding. Idempotent for a
            // spelled-out `E?`, which the core already marked.
            shape.nullable |= self.is_nullable_ty(ty);
            return Some(shape);
        }
        None
    }

    /// The leaf globals a place rooted at a `nobox` global denotes, plus the
    /// shape of the projected part (`None` = one primitive leaf). `SPOT` is the
    /// whole group, `SPOT.x` / `SPOT.a.b` a contiguous sub-range of it -- the
    /// leaf order is `repr_to_leaves`', so a projection is a slice, never a
    /// scatter. `None` when the place is not rooted at a `nobox` global.
    fn nobox_global_place(&self, e: Idx<Expr>) -> Option<(Vec<Ustr>, Option<ValueShape>)> {
        match self.body.expr_arena[e].clone() {
            // The DECLARED type, never `e`'s: a refined read of a `global X: NV2?`
            // has expr type `NV2`, whose 2-leaf shape would address the presence
            // leaf as `x`. Storage layout is a property of the global, not of the
            // read site.
            Expr::Global(name) => {
                let shape = nobox_declared_shape(self.ws, *self.ws.globals_by_name.get(&name)?)?;
                let leaves =
                    (0..shape.leaf_count()).map(|i| nobox_global_leaf_name(name, i)).collect();
                Some((leaves, Some(shape)))
            }
            Expr::FieldAccess { base, field } => {
                let (leaves, Some(shape)) = self.nobox_global_place(base)? else {
                    return None;
                };
                let mut off = usize::from(shape.nullable);
                for (fname, fs) in &shape.fields {
                    let (n, sub) = match fs {
                        FieldShape::Prim => (1, None),
                        FieldShape::Nested(s) => (s.leaf_count(), Some(s.clone())),
                    };
                    if *fname == field {
                        return Some((leaves[off..off + n].to_vec(), sub));
                    }
                    off += n;
                }
                None
            }
            _ => None,
        }
    }

    /// Read a `nobox` global place back as an unboxed repr.
    fn nobox_global_load(
        &mut self,
        leaves: &[Ustr],
        shape: Option<ValueShape>,
    ) -> ValueRepr {
        let vals: Vec<Idx<Inst>> =
            leaves.iter().map(|g| self.emit_cur(Inst::Global(*g))).collect();
        match shape {
            Some(shape) => {
                let mut idx = 0;
                self.rebuild_from_leaves(&shape, &vals, &mut idx)
            }
            None => ValueRepr::Scalar(vals[0]),
        }
    }

    /// Write `value` into a `nobox` global place, leaf by leaf.
    fn nobox_global_store(
        &mut self,
        leaves: &[Ustr],
        shape: Option<ValueShape>,
        value: Idx<Expr>,
    ) -> Result<(), CompilerError> {
        let vals = match shape {
            Some(shape) => {
                let r = self.lower_expr_repr(value)?;
                self.repr_to_leaves(&r, &shape)
            }
            None => vec![self.lower_expr(value)?],
        };
        for (g, v) in leaves.iter().zip(vals) {
            self.emit_cur(Inst::StoreGlobal { name: *g, value: v });
        }
        Ok(())
    }

    fn emit_all_equal(&mut self, lhs: &[Idx<Inst>], rhs: &[Idx<Inst>]) -> Idx<Inst> {
        debug_assert_eq!(lhs.len(), rhs.len());
        let result = self.f.new_cell(None);
        let fail = self.f.new_block();
        let join = self.f.new_block();
        for (&a, &b) in lhs.iter().zip(rhs) {
            let equal = self.emit_cur(Inst::Eq(a, b));
            let next = self.f.new_block();
            self.f.set_term(
                self.cur,
                Term::Branch {
                    cond: equal,
                    then_bb: next,
                    else_bb: fail,
                },
            );
            self.cur = next;
        }
        let yes = self.emit_cur(Inst::Const(Lit::Bool(true)));
        self.emit_cur(Inst::Store {
            cell: result,
            value: yes,
        });
        self.f.set_term(self.cur, Term::Goto(join));

        self.cur = fail;
        let no = self.emit_cur(Inst::Const(Lit::Bool(false)));
        self.emit_cur(Inst::Store {
            cell: result,
            value: no,
        });
        self.f.set_term(self.cur, Term::Goto(join));

        self.cur = join;
        self.emit_cur(Inst::Load(result))
    }

    /// Whether `e`'s static type is an enum ALL of whose variants are unit (no
    /// payload). Those variants lower to interned singleton globals — reference-
    /// equal across uses — so `==`/`!=` is native Lua `==`/`~=`, not the dynamic
    /// `__op_eq` (type()/pairs()) walk (static-over-dynamic; trace-friendly).
    fn is_payloadless_enum(&self, e: Idx<Expr>) -> bool {
        let Some(TypeExpr::App { ctor: TypeCtor::Nominal(name), .. }) = self.expr_ty(e) else {
            return false;
        };
        let Some(eid) = self.ws.enums_by_name.get(&name) else {
            return false;
        };
        let ed = &self.ws.modules[eid.module.index()].enums[eid.def];
        ed.variants.iter().all(|v| matches!(v.payload, bone_semantic::hir::VariantPayload::Unit))
    }

    fn is_null_lit(&self, e: Idx<Expr>) -> bool {
        matches!(self.body.expr_arena[e], Expr::Null)
    }

    /// Whether `e`'s type is `T?` (a `Nullable`, chasing `Infer` but NOT peeling
    /// the `Nullable` like `expr_ty` does). Drives the `if let` nil-guard.
    fn expr_is_nullable(&self, e: Idx<Expr>) -> bool {
        self.inf
            .and_then(|i| i.expr_types.get(e).copied())
            .map(|ty| self.is_nullable_ty(ty))
            .unwrap_or(false)
    }

    fn is_nullable_ty(&self, ty: TyId) -> bool {
        // A generic bound to `E?` is nullable, and `ty_node` cannot say so: it
        // substitutes the PAYLOAD (a `Nullable` node needs an arena index).
        // Read the raw node first, before that substitution erases the `?`.
        if let Some(TypeExpr::Generic(var, _)) = self.raw_ty_node(ty) {
            if let Some(b) = self.spec_subst.get(&var) {
                return b.peel_nullable().1;
            }
        }
        match self.ty_node(ty) {
            Some(TypeExpr::Nullable(_)) => true,
            Some(TypeExpr::Infer(var, _)) => self
                .inf
                .and_then(|i| i.substitutions.get(var))
                .map(|s| self.is_nullable_ty(s))
                .unwrap_or(false),
            _ => false,
        }
    }

    /// The declared field names of a call-site spread operand `..op`, in struct
    /// order. bone-inference has already verified the operand is a nominal
    /// struct whose fields all match callee params.
    fn spread_operand_fields(&self, op: Idx<Expr>) -> Result<Vec<Ustr>, CompilerError> {
        match self.expr_ty(op) {
            Some(TypeExpr::App { ctor: TypeCtor::Nominal(name), .. }) => self
                .struct_field_names(name)
                .ok_or_else(|| gap(format!("spread operand `{name}` has no known fields"))),
            _ => Err(gap("spread operand is not a nominal struct")),
        }
    }

    /// A spread operand `..op` decomposed to (field name, boxed value) pairs. A
    /// struct-LITERAL operand is ELIDED: its field exprs lower directly (base
    /// first, then explicit fields in source order for side effects; unwritten
    /// fields from a functional-update base or the struct default), so no
    /// intermediate table is built. Any other operand -- and a literal with a
    /// flattened-enum field, which needs slot expansion -- is lowered once and
    /// its fields read off it.
    fn spread_field_insts(&mut self, op: Idx<Expr>) -> Result<Vec<(Ustr, Idx<Inst>)>, CompilerError> {
        if let Expr::StructLiteral { name, fields, bases } = self.body.expr_arena[op].clone() {
            let decl = self.struct_field_names(name).unwrap_or_default();
            if !decl.iter().any(|f| self.flat_field_of_struct(name, *f).is_some()) {
                let base_inst = match bases.into_iter().next() {
                    Some(b) => Some(self.lower_expr(b)?),
                    None => None,
                };
                let mut explicit: Vec<(Ustr, Idx<Inst>)> = Vec::new();
                let mut by_value: HashMap<u32, Idx<Inst>> = HashMap::new();
                for fidx in fields {
                    let lf = self.body.struct_lit_field_arena[fidx];
                    let key: u32 = lf.value.into_raw().into();
                    let v = match by_value.get(&key) {
                        Some(&v) => v,
                        None => {
                            let v = self.lower_expr(lf.value)?;
                            by_value.insert(key, v);
                            v
                        }
                    };
                    explicit.push((lf.name, v));
                }
                let mut out = Vec::new();
                for fname in decl {
                    let v = if let Some((_, v)) = explicit.iter().find(|(n, _)| *n == fname) {
                        *v
                    } else if let Some(bi) = base_inst {
                        self.emit_cur(Inst::Field { base: bi, field: fname })
                    } else {
                        self.lower_struct_default(name, fname)?
                    };
                    out.push((fname, v));
                }
                return Ok(out);
            }
        }
        let base = self.lower_expr(op)?;
        let mut out = Vec::new();
        for f in self.spread_operand_fields(op)? {
            out.push((f, self.emit_cur(Inst::Field { base, field: f })));
        }
        Ok(out)
    }

    /// The `ValueRepr` form of [`spread_field_insts`], for the scalar-ABI arg
    /// path. Same literal-elision; a Group base projects its parts, a boxed base
    /// reads fields off the packed cdata.
    fn spread_field_reprs(&mut self, op: Idx<Expr>) -> Result<Vec<(Ustr, ValueRepr)>, CompilerError> {
        if let Expr::StructLiteral { name, fields, bases } = self.body.expr_arena[op].clone() {
            let decl = self.struct_field_names(name).unwrap_or_default();
            if !decl.iter().any(|f| self.flat_field_of_struct(name, *f).is_some()) {
                let base_repr = match bases.into_iter().next() {
                    Some(b) => Some(self.lower_expr_repr(b)?),
                    None => None,
                };
                let base_boxed = match &base_repr {
                    Some(ValueRepr::Group { .. }) | None => None,
                    Some(other) => Some(self.pack(other.clone())),
                };
                let mut explicit: Vec<(Ustr, ValueRepr)> = Vec::new();
                let mut by_value: HashMap<u32, ValueRepr> = HashMap::new();
                for fidx in fields {
                    let lf = self.body.struct_lit_field_arena[fidx];
                    let key: u32 = lf.value.into_raw().into();
                    let r = match by_value.get(&key) {
                        Some(r) => r.clone(),
                        None => {
                            let r = self.lower_expr_repr(lf.value)?;
                            by_value.insert(key, r.clone());
                            r
                        }
                    };
                    explicit.push((lf.name, r));
                }
                let mut out = Vec::new();
                for fname in decl {
                    let r = if let Some((_, r)) = explicit.iter().find(|(n, _)| *n == fname) {
                        r.clone()
                    } else {
                        self.spread_base_field_repr(&base_repr, base_boxed, name, fname)?
                    };
                    out.push((fname, r));
                }
                return Ok(out);
            }
        }
        let base_repr = self.lower_expr_repr(op)?;
        let base_boxed = match &base_repr {
            ValueRepr::Group { .. } => None,
            other => Some(self.pack(other.clone())),
        };
        let name = match self.expr_ty(op) {
            Some(TypeExpr::App { ctor: TypeCtor::Nominal(name), .. }) => name,
            _ => return Err(gap("spread operand is not a nominal struct")),
        };
        let mut out = Vec::new();
        for fname in self.struct_field_names(name).unwrap_or_default() {
            let r = self.spread_base_field_repr(&Some(base_repr.clone()), base_boxed, name, fname)?;
            out.push((fname, r));
        }
        Ok(out)
    }

    /// Read field `fname` from an already-lowered spread base: project a Group
    /// part, or read the field off the packed cdata; a missing base falls back
    /// to the struct default.
    fn spread_base_field_repr(
        &mut self,
        base_repr: &Option<ValueRepr>,
        base_boxed: Option<Idx<Inst>>,
        name: Ustr,
        fname: Ustr,
    ) -> Result<ValueRepr, CompilerError> {
        Ok(match (base_repr, base_boxed) {
            (Some(ValueRepr::Group { name: bn, parts, .. }), _) => {
                let i = self.field_index(*bn, fname).ok_or_else(|| gap("spread field"))?;
                parts[i].clone()
            }
            (Some(_), Some(bi)) => {
                ValueRepr::Scalar(self.emit_cur(Inst::Field { base: bi, field: fname }))
            }
            _ => ValueRepr::Scalar(self.lower_struct_default(name, fname)?),
        })
    }

    /// The resolved type of an expression (chasing `Infer` vars + peeling
    /// `Nullable`), or `None` when inference is absent/unsolved.
    fn expr_ty(&self, e: Idx<Expr>) -> Option<TypeExpr> {
        let ty = *self.inf?.expr_types.get(e)?;
        self.resolve_ty(ty)
    }

    /// `Some` iff `e`'s inferred type is a (non-nullable) tuple. Used to route a
    /// tuple result across a call boundary (the call expr's own type IS the
    /// callee's return type, so direct and INDIRECT calls agree).
    /// `resolve_tyid` (not `resolve_ty`) so a `Nullable(Tuple)` doesn't peel to a
    /// bare tuple and get mis-splatted.
    fn expr_tuple_shape(&self, e: Idx<Expr>) -> Option<TupleRetShape> {
        self.tyid_tuple_ret_shape(self.resolve_tyid(*self.inf?.expr_types.get(e)?)?)
    }

    fn repr_join_slot(&self, e: Idx<Expr>) -> Option<RetSlot> {
        if let Some(shape) = self.expr_tuple_shape(e).filter(TupleRetShape::splats) {
            return Some(RetSlot::Nested(shape));
        }
        self.nobox_value_shape_of(e).map(RetSlot::Leaves)
    }

    /// `Some` iff `e` is a lambda whose RETURN type is a (non-nullable) tuple --
    /// the lambda body then splats its slots so an indirect call site captures
    /// them (multi-return ABI). Read off the lambda's inferred `fn(..) -> (..)`
    /// type so it agrees with the call site's `expr_tuple_shape`.
    fn lambda_ret_tuple_shape(&self, e: Idx<Expr>) -> Option<TupleRetShape> {
        let ty = self.resolve_tyid(*self.inf?.expr_types.get(e)?)?;
        let TypeExpr::Function { ret, .. } = self.ty_node(ty)? else { return None };
        self.tyid_tuple_ret_shape(self.resolve_tyid(ty.with_idx(ret))?)
    }

    /// [`tuple_ret_shape`] for a type that may live in the BODY's inference arena
    /// (a call expression's own type, a lambda's return) rather than the
    /// workspace one. `ty_node` applies the enclosing specialization
    /// substitution, so a generic element resolves to the same slot the
    /// signature-side walker sees.
    fn tyid_tuple_ret_shape(&self, ty: TyId) -> Option<TupleRetShape> {
        let ty = self.resolve_tyid(ty)?;
        let TypeExpr::Tuple { elements } = self.ty_node(ty)? else { return None };
        let slots = elements
            .into_iter()
            .map(|el| self.body_ret_slot(ty.with_idx(el)))
            .collect();
        Some(TupleRetShape { slots, nullable: false })
    }

    fn body_ret_slot(&self, ty: TyId) -> RetSlot {
        if let Some(shape) = self.body_nobox_shape(ty) {
            return RetSlot::Leaves(shape);
        }
        match self.tyid_tuple_ret_shape(ty) {
            Some(inner) if inner.splats() => RetSlot::Nested(inner),
            _ => RetSlot::Opaque,
        }
    }

    /// [`Self::body_sig_value_shape`] narrowed to `nobox` nominals -- the splat
    /// gate, mirroring [`nobox_sig_shape_spec`] on the body arena.
    fn body_nobox_shape(&self, ty: TyId) -> Option<ValueShape> {
        let ty = self.resolve_tyid(ty)?;
        match self.ty_node(ty)? {
            TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } if self.is_nobox_struct(n) => {
                sig_shape_of_nominal(self.ws, n)
            }
            TypeExpr::Nullable(inner) => {
                let inner = self.resolve_tyid(ty.with_idx(inner))?;
                match self.ty_node(inner)? {
                    TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } if self.is_nobox_struct(n) => {
                        sig_shape_of_nullable_nominal(self.ws, n)
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// The C-array element type name for a `native_buffer_alloc<T>()` call whose
    /// result type is `NativeBuffer<T>` -- an FFI-clean value struct's cdef name,
    /// or `int`/`float`/`bool`. A GC-carrying value struct, an unresolved (opaque
    /// generic `T`), or any other element rejects with a spanned emit-time error:
    /// the intrinsic can only emit `ffi.new("<ctype>[?]")` when the concrete
    /// element type is known here. `span` labels the call expression.
    /// The `(field, view)` pairs a `NativeBuffer` of element `ctype` carries --
    /// one typed leaf view per distinct primitive C type among its leaves.
    /// Empty for a primitive or non-leaf-addressable element, which keeps those
    /// buffers exactly as they are today.
    ///
    /// Attached at CONSTRUCTION so the cast happens once per buffer. Casting per
    /// access would allocate a pointer cdata each time, which is the allocation
    /// leaf addressing exists to remove.
    fn leaf_view_fields(&mut self, ctype: Ustr, ptr: Idx<Inst>) -> Vec<(Ustr, Idx<Inst>)> {
        let Ok(ctys) = leaf_view_ctys(self.ws, ctype) else {
            return Vec::new();
        };
        ctys.into_iter()
            .map(|cty| {
                let view = self.emit_cur(Inst::FfiViewCast { cty: Ustr::from(cty), ptr });
                (leaf_view_field(cty), view)
            })
            .collect()
    }

    fn native_buffer_alloc_ctype(&self, e: Idx<Expr>, span: Span) -> Result<Ustr, CompilerError> {
        let inf = self.inf.ok_or_else(|| {
            CompilerError::new_with_label(span, "`native_buffer_alloc` needs inference".into())
        })?;
        let ret = *inf.expr_types.get(e).ok_or_else(|| {
            CompilerError::new_with_label(span, "`native_buffer_alloc` call is untyped".into())
        })?;
        // In a specialized splice the `NativeBuffer<T>` element resolves through
        // `spec_subst` to a concrete type, so the intrinsic can name the ctype the
        // shared generic body could not. An empty subst is exactly the shared path.
        let r = if self.spec_subst.is_empty() {
            native_buffer_alloc_elem_ctype(self.ws, inf, ret)
        } else {
            native_buffer_alloc_elem_ctype_spec(self.ws, inf, ret, self.spec_subst)
        };
        r.map_err(|msg| CompilerError::new_with_label(span, msg))
    }

    /// `Some` iff `e`'s type is a tuple OR a `(tuple)?` -- the nil-peeled form
    /// for safe-nav calls, whose result type is nullable but whose callee still
    /// splats the inner tuple.
    fn peeled_tuple_shape(&self, e: Idx<Expr>) -> Option<TupleRetShape> {
        let ty = self.resolve_tyid(*self.inf?.expr_types.get(e)?)?;
        let inner = match self.ty_node(ty)? {
            TypeExpr::Nullable(i) => self.resolve_tyid(ty.with_idx(i))?,
            _ => ty,
        };
        self.tyid_tuple_ret_shape(inner)
    }

    fn resolve_ty(&self, ty: TyId) -> Option<TypeExpr> {
        let te = self.ty_node(ty)?;
        match te {
            TypeExpr::Infer(var, _) => {
                let solved = self.inf?.substitutions.get(var)?;
                self.resolve_ty(solved)
            }
            TypeExpr::Nullable(inner) => self.resolve_ty(ty.with_idx(inner)),
            other => Some(other),
        }
    }

    /// The stored type node, WITHOUT the specialization substitution -- the only
    /// place a generic's own identity is still visible, which is what
    /// [`Self::is_nullable_ty`] needs (the substitution answers with the payload).
    fn raw_ty_node(&self, ty: TyId) -> Option<TypeExpr> {
        Some(match ty {
            TyId::Static(mid, idx) => self.ws.module(mid).ty_arena[idx],
            TyId::Local(idx) => self.inf?.ty_arena[idx],
        })
    }

    fn ty_node(&self, ty: TyId) -> Option<TypeExpr> {
        let node = self.raw_ty_node(ty)?;
        // In a specialized instantiation, a generic param resolves to its concrete
        // leaf so all downstream type-driven lowering (field projection, container
        // element, scalar shape) sees the real type. Synthesized leaves carry no
        // App params -- v0 specializes leaf types only -- so no consumer indexes
        // into `new_empty()`.
        if let TypeExpr::Generic(var, _) = node {
            if let Some(ty) = self.spec_subst.get(&var) {
                return Some(reified_type_node(ty));
            }
        }
        Some(node)
    }

    /// The final `TyId` after chasing `Infer` substitutions (NOT peeling
    /// `Nullable` -- unlike `resolve_ty`, this keeps the `TyId` so its App params
    /// can be indexed in the correct arena via `with_idx`).
    fn resolve_tyid(&self, ty: TyId) -> Option<TyId> {
        match self.ty_node(ty)? {
            TypeExpr::Infer(var, _) => {
                let solved = self.inf?.substitutions.get(var)?;
                self.resolve_tyid(solved)
            }
            _ => Some(ty),
        }
    }

    /// The `EnumFlatShape` of `Y` when `e`'s type is `Coroutine<Y, ..>` and `Y` is
    /// a flat enum -- the shape flowing through a `await` delegation to a flat-enum
    /// coroutine (so the delegate threads the inner's leaves instead of a box).
    fn coroutine_yield_flat_shape(&self, e: Idx<Expr>) -> Option<EnumFlatShape> {
        let ty = self.resolve_tyid(*self.inf?.expr_types.get(e)?)?;
        let TypeExpr::App { ctor: TypeCtor::Nominal(n), params } = self.ty_node(ty)? else {
            return None;
        };
        if n.as_str() != "Coroutine" {
            return None;
        }
        let y_ty = self.resolve_tyid(ty.with_idx(params.into_iter().next()?))?;
        match self.ty_node(y_ty)? {
            TypeExpr::App { ctor: TypeCtor::Nominal(yn), .. } => flat_enum_shape(self.ws, yn),
            _ => None,
        }
    }

}

// -- Map value-struct key rejection (a fatal emit-time pass; see
// `value_struct_map_key`). These mirror the `Lowerer` type helpers but take
// `(ws, inf)` directly so the check runs without building a full lowering
// context. --

fn ty_node(ws: &Workspace, inf: &BodyInference, ty: TyId) -> Option<TypeExpr> {
    match ty {
        TyId::Static(mid, idx) => Some(ws.module(mid).ty_arena[idx]),
        TyId::Local(idx) => Some(inf.ty_arena[idx]),
    }
}

fn resolve_tyid(ws: &Workspace, inf: &BodyInference, ty: TyId) -> Option<TyId> {
    match ty_node(ws, inf, ty)? {
        TypeExpr::Infer(var, _) => resolve_tyid(ws, inf, inf.substitutions.get(var)?),
        _ => Some(ty),
    }
}

fn map_is_value_struct(ws: &Workspace, name: Ustr) -> bool {
    ws.struct_by_name
        .get(&name)
        .map(|sid| ws.modules[sid.module.index()].structs[sid.def].is_value)
        .unwrap_or(false)
}

/// When `ty` resolves to `Map<K, V>` with `K` a `value` struct, returns K's name.
fn map_key_value_struct(ws: &Workspace, inf: &BodyInference, ty: TyId) -> Option<Ustr> {
    let ty = resolve_tyid(ws, inf, ty)?;
    let TypeExpr::App { ctor: TypeCtor::Map, params } = ty_node(ws, inf, ty)? else {
        return None;
    };
    let k = resolve_tyid(ws, inf, ty.with_idx(params.into_iter().next()?))?;
    match ty_node(ws, inf, k)? {
        TypeExpr::App { ctor: TypeCtor::Nominal(name), .. } if map_is_value_struct(ws, name) => {
            Some(name)
        }
        _ => None,
    }
}

/// Scan module `mid`'s function bodies for a `Map<K, V>` whose key `K` is a
/// `value` struct. Value structs are C-layout cdata; as Lua table keys they
/// hash by pointer identity, so a runtime-built key silently misses an entry
/// stored under an equal-by-value but distinct instance (store `mk(1,2)`, read
/// a fresh `mk(1,2)` -> nil). int / string / bool / float / newtype keys are Lua
/// value-keyed and stay allowed. Returns the first offending (key name, span).
pub fn value_struct_map_key(ws: &Workspace, mid: ModuleId) -> Option<(Ustr, Span)> {
    let module = ws.module(mid);
    let ir = ws.inference(mid)?;
    for (fid, _f) in module.functions.iter() {
        let owner = BodyOwner::Function(fid);
        let (Some(body), Some(inf)) = (module.bodies.get(&owner), ir.per_fn.get(&owner)) else {
            continue;
        };
        for (e, tyid) in inf.expr_types.iter() {
            if let Some(name) = map_key_value_struct(ws, inf, *tyid) {
                return Some((name, body.expr_spans[e]));
            }
        }
    }
    None
}

/// A GC-carrying `value` struct (table repr, no C layout) named at a nominal
/// position. Not a legal C boundary type: no `ffi.cdef` typedef exists, so
/// `ffi.new`/`sizeof`/`ffi.istype`/the extern ABI cannot name it.
fn nominal_is_gc_value(ws: &Workspace, name: Ustr) -> bool {
    map_is_value_struct(ws, name) && value_struct_is_gc(ws, name)
}

/// When `ty` resolves to `NativeBuffer<E>` with `E` a GC-carrying value struct,
/// returns E's name -- `ffi.new("E[?]")` would name a non-existent ctype.
fn native_buffer_gc_elem(ws: &Workspace, inf: &BodyInference, ty: TyId) -> Option<Ustr> {
    let ty = resolve_tyid(ws, inf, ty)?;
    let TypeExpr::App { ctor: TypeCtor::Nominal(nb), params } = ty_node(ws, inf, ty)? else {
        return None;
    };
    if nb.as_str() != "NativeBuffer" {
        return None;
    }
    let elem = resolve_tyid(ws, inf, ty.with_idx(params.into_iter().next()?))?;
    match ty_node(ws, inf, elem)? {
        TypeExpr::App { ctor: TypeCtor::Nominal(name), .. } if nominal_is_gc_value(ws, name) => {
            Some(name)
        }
        _ => None,
    }
}

/// The C-array element type name for a `native_buffer_alloc<T>` call whose
/// result type `ret` is `NativeBuffer<T>`. `Ok` is an FFI-clean value struct's
/// cdef name, or `int`/`float`/`bool`. `Err(reason)` when `T` is a GC-carrying
/// value struct, an unresolved generic (opaque `T`), or any non-C-array type --
/// the intrinsic can only emit `ffi.new("<ctype>[?]")` when the concrete element
/// is known at the call site. The single source for both the lowering
/// interception and the hard emit-time [`native_buffer_alloc_reject`] pass.
fn native_buffer_alloc_elem_ctype(
    ws: &Workspace,
    inf: &BodyInference,
    ret: TyId,
) -> Result<Ustr, String> {
    let ret = resolve_tyid(ws, inf, ret)
        .ok_or_else(|| "`native_buffer_alloc` result type could not be resolved".to_string())?;
    let TypeExpr::App { ctor: TypeCtor::Nominal(nb), params } = ty_node(ws, inf, ret)
        .ok_or_else(|| "`native_buffer_alloc` result is not a `NativeBuffer<T>`".to_string())?
    else {
        return Err("`native_buffer_alloc` must produce a `NativeBuffer<T>`".to_string());
    };
    if nb.as_str() != "NativeBuffer" {
        return Err(format!("`native_buffer_alloc` result is `{nb}`, expected `NativeBuffer<T>`"));
    }
    let elem = params
        .into_iter()
        .next()
        .and_then(|p| resolve_tyid(ws, inf, ret.with_idx(p)))
        .ok_or_else(|| "`NativeBuffer` element type is missing".to_string())?;
    match ty_node(ws, inf, elem) {
        Some(TypeExpr::Builtin(BuiltinTy::Int)) => Ok(Ustr::from("int")),
        Some(TypeExpr::Builtin(BuiltinTy::Float)) => Ok(Ustr::from("float")),
        Some(TypeExpr::Builtin(BuiltinTy::Bool)) => Ok(Ustr::from("bool")),
        Some(TypeExpr::App { ctor: TypeCtor::Nominal(name), .. }) if map_is_value_struct(ws, name) => {
            if value_struct_is_gc(ws, name) {
                Err(format!(
                    "`native_buffer_alloc<{name}>` is invalid: `{name}` is a GC-carrying `value` \
                     struct (immutable Lua table) with no C layout, so `ffi.new(\"{name}[?]\")` \
                     names no ctype. Only FFI-clean value structs (C-mappable fields) and \
                     `int`/`float`/`bool` are allocatable."
                ))
            } else {
                Ok(name)
            }
        }
        Some(TypeExpr::Generic(..)) | Some(TypeExpr::Infer(..)) => Err(
            "`native_buffer_alloc<T>` needs a concrete element type here, but `T` is an \
             unresolved generic. skull emits generic bodies once with `T` opaque, so it cannot \
             name the ffi ctype -- call it where the element type is concrete (a per-type \
             wrapper fn), not inside a generic body."
                .to_string(),
        ),
        _ => Err(
            "`native_buffer_alloc<T>` requires `T` to be an FFI-clean `value` struct or \
             `int`/`float`/`bool` -- a heap struct / non-C-array type has no flat layout."
                .to_string(),
        ),
    }
}

/// Whether a `NativeBuffer<E>` result type's element `E` is an unresolved generic
/// param (in `inf`) -- the opaque-`T` case that call-site specialization resolves,
/// distinct from a concrete-but-unallocatable element (heap / GC value struct).
fn native_buffer_elem_is_generic(ws: &Workspace, inf: &BodyInference, ret: TyId) -> bool {
    let Some(ret) = resolve_tyid(ws, inf, ret) else { return false };
    let Some(TypeExpr::App { ctor: TypeCtor::Nominal(nb), params }) = ty_node(ws, inf, ret) else {
        return false;
    };
    if nb.as_str() != "NativeBuffer" {
        return false;
    }
    let Some(elem) = params.into_iter().next().map(|p| ret.with_idx(p)) else {
        return false;
    };
    // Peel Infer chains but NOT a Generic -- a Generic is the opaque case.
    let Some(elem) = resolve_tyid(ws, inf, elem) else { return false };
    matches!(ty_node(ws, inf, elem), Some(TypeExpr::Generic(..)))
}

/// The verdict of scanning every workspace call to an intrinsic-bearing generic
/// `F`: whether its standalone (opaque-`T`) body is reachable un-specialized.
/// If `fref` is a generic fn whose body DIRECTLY calls `native_buffer_alloc<T>`
/// with an opaque-generic element (the FlatVec-storage shape), return that call's
/// `NativeBuffer<T>` result type (in `fref`'s own inference) so a call site can
/// check its specialization resolves the element ctype. `None` otherwise.
fn fn_opaque_intrinsic_ret(ws: &Workspace, fref: FuncRef) -> Option<TyId> {
    let f = &ws.module(fref.module).functions[fref.func];
    if f.generics.is_empty() || f.inline_body.is_some() {
        return None;
    }
    let body = ws.module(fref.module).bodies.get(&BodyOwner::Function(fref.func))?;
    let inf = ws.inference(fref.module)?.per_fn.get(&BodyOwner::Function(fref.func))?;
    for (e, expr) in body.expr_arena.iter() {
        let Expr::Call { callee: Callee::Unresolved { name, .. }, .. } = expr else {
            continue;
        };
        if name.as_str() != "native_buffer_alloc" {
            continue;
        }
        if let Some(&ret) = inf.expr_types.get(e) {
            if native_buffer_elem_is_generic(ws, inf, ret) {
                return Some(ret);
            }
        }
    }
    None
}

/// Whether calls to `fref` can reach an opaque-`T` `native_buffer_alloc`
/// whose element depends on `fref`'s own instantiation: directly
/// ([`fn_opaque_intrinsic_ret`]) or TRANSITIVELY through a nested generic
/// call whose bindings do not all reify to concrete leaves (i.e. the nested
/// call passes `fref`'s generics through -- `push` -> `grow` -> intrinsic).
/// A nested call whose every binding reifies concrete specializes at any
/// lowering of this body and is not a pass-through. Cycle-safe; conservative
/// (does not track WHICH generic flows to the intrinsic).
fn fn_opaque_intrinsic_transitive(
    ws: &Workspace,
    fref: FuncRef,
    seen: &mut HashSet<FuncRef>,
) -> bool {
    if !seen.insert(fref) {
        return false;
    }
    if fn_opaque_intrinsic_ret(ws, fref).is_some() {
        return true;
    }
    let f = &ws.module(fref.module).functions[fref.func];
    if f.generics.is_empty() || f.inline_body.is_some() {
        return false;
    }
    let Some(inf) = ws
        .inference(fref.module)
        .and_then(|ir| ir.per_fn.get(&BodyOwner::Function(fref.func)))
    else {
        return false;
    };
    for (c, resolved) in inf.resolved_overload_calls.iter() {
        let callee = FuncRef { module: resolved.0, func: resolved.1 };
        let Some(bindings) = inf.call_generic_bindings.get(c) else { continue };
        let empty = SpecSubst::new();
        if bindings.iter().all(|&b| reify_type(ws, inf, b, &empty).is_some()) {
            continue;
        }
        if fn_opaque_intrinsic_transitive(ws, callee, seen) {
            return true;
        }
    }
    false
}

/// [`native_buffer_alloc_elem_ctype`] under a specialization `subst`: a
/// `NativeBuffer<T>` element that is a generic bound in `subst` resolves to its
/// concrete type's ctype (FFI-clean value name / `int`/`float`/`bool`), or `Err`
/// if that type still is not allocatable (a GC-carrying value struct).
/// A non-substituted element defers to the concrete [`native_buffer_alloc_elem_ctype`].
fn native_buffer_alloc_elem_ctype_spec(
    ws: &Workspace,
    inf: &BodyInference,
    ret: TyId,
    subst: &SpecSubst,
) -> Result<Ustr, String> {
    let Some(ret) = resolve_tyid(ws, inf, ret) else {
        return native_buffer_alloc_elem_ctype(ws, inf, ret);
    };
    let leaf = match ty_node(ws, inf, ret) {
        Some(TypeExpr::App { ctor: TypeCtor::Nominal(nb), params }) if nb.as_str() == "NativeBuffer" => {
            params
                .into_iter()
                .next()
                .and_then(|p| resolve_tyid(ws, inf, ret.with_idx(p)))
                .and_then(|e| match ty_node(ws, inf, e) {
                    Some(TypeExpr::Generic(var, _)) => subst.get(&var).cloned(),
                    _ => None,
                })
        }
        _ => None,
    };
    match leaf {
        Some(ReifiedType::Prim(p)) => Ok(Ustr::from(p)),
        Some(ty) if ty.nominal().is_some() => {
            let n = ty.nominal().unwrap();
            if !map_is_value_struct(ws, n) {
                Err(format!("`native_buffer_alloc<{n}>` is invalid: `{n}` is not a `value` struct."))
            } else if value_struct_is_gc(ws, n) {
                Err(format!(
                    "`native_buffer_alloc<{n}>` is invalid: `{n}` is a GC-carrying `value` struct \
                     (no C layout), so `ffi.new(\"{n}[?]\")` names no ctype."
                ))
            } else {
                Ok(n)
            }
        }
        Some(_) => Err(
            "`native_buffer_alloc<T>` requires a primitive or FFI-clean value-struct element"
                .to_string(),
        ),
        None => native_buffer_alloc_elem_ctype(ws, inf, ret),
    }
}

/// Hard emit-time rejection for `native_buffer_alloc<T>` uses that would emit
/// broken / dropped code. Runs as a dedicated pass in `emit_full` (beside
/// [`gc_value_c_boundary`]) so the error is SURFACED, not swallowed by the lenient
/// per-body lowering-gap skip. Two scans over `mid`:
///
/// 1. A DIRECT intrinsic call in this module whose element is a CONCRETE
///    unallocatable type (a heap struct / GC value struct -- the P1 cases).
/// 2. SITE-AWARE (docs/flatvec-plan.md P2): a call to a generic fn whose body
///    allocates via an opaque-`T` intrinsic (e.g. `flat_vec_new<T>`, possibly in
///    the stdlib), where THIS call does not specialize the element to an FFI-clean
///    value struct -- so it would reach the shared generic body that cannot name
///    the ctype (a primitive, GC-value, or nested-generic instantiation). The call
///    site is named.
///
/// An opaque-generic intrinsic that is never called un-specialized (all callers
/// specialize, or -- a library fn -- has no callers at all) is exempt: its
/// standalone body's lowering-gap skip is then correct. Returns the first offender.
pub fn native_buffer_alloc_reject(ws: &Workspace, mid: ModuleId) -> Option<(String, Span)> {
    let module = ws.module(mid);
    let ir = ws.inference(mid)?;
    // 1. Direct intrinsic calls with a concrete unallocatable element.
    for (owner, body) in module.bodies.iter() {
        let Some(inf) = ir.per_fn.get(owner) else { continue };
        for (e, expr) in body.expr_arena.iter() {
            let Expr::Call { callee: Callee::Unresolved { name, .. }, .. } = expr else {
                continue;
            };
            if name.as_str() != "native_buffer_alloc" {
                continue;
            }
            let Some(&ret) = inf.expr_types.get(e) else { continue };
            // Opaque-generic elements are a per-CALL-SITE property (scan 2), never
            // rejected on the definition body -- specialization may resolve them.
            if native_buffer_elem_is_generic(ws, inf, ret) {
                continue;
            }
            if let Err(msg) = native_buffer_alloc_elem_ctype(ws, inf, ret) {
                return Some((msg, body.expr_spans[e]));
            }
        }
    }
    // 2. Calls to an opaque-`T`-intrinsic-bearing generic fn that this site does
    //    not specialize to an allocatable element.
    for (owner, body) in module.bodies.iter() {
        // A call INSIDE an @inline/@pure generic body (e.g. the stdlib `push`'s
        // own `fv.grow()`) is not an independent surviving use: it specializes
        // when the enclosing generic is specialized, and is dead in the enclosing
        // fn's standalone body otherwise. Its fate is decided at the ENCLOSING
        // fn's call sites, which are scanned as their own (eventually
        // non-generic) roots -- skip. A NON-eligible generic is NEVER
        // specialized (the inliner only splices @inline/@pure), so its standalone
        // body is the only emission and an intrinsic-bearing call inside it
        // survives -- gate it like a non-inlined context.
        // Whether the inliner processes this body -- specialization only happens
        // via inlining. An ordinary (non-coroutine) function is inlined; a
        // global initializer / decorator / `co fn` is NOT inlined, so a
        // specialized call there would SURVIVE un-inlined (dangling nil) and
        // must reject.
        let owner_inlined = match owner {
            BodyOwner::Function(fid) => {
                let f = &ws.module(mid).functions[*fid];
                if !f.generics.is_empty() {
                    if f.annotations
                        .iter()
                        .any(|a| matches!(a.name.name.as_str(), "inline" | "pure"))
                    {
                        continue;
                    }
                    false
                } else {
                    !f.is_coroutine
                }
            }
            _ => false,
        };
        let Some(inf) = ir.per_fn.get(owner) else { continue };
        for (c, resolved) in inf.resolved_overload_calls.iter() {
            let c = *c;
            let fref = FuncRef { module: resolved.0, func: resolved.1 };
            let intr_ret = fn_opaque_intrinsic_ret(ws, fref);
            if intr_ret.is_none()
                && !fn_opaque_intrinsic_transitive(ws, fref, &mut HashSet::new())
            {
                continue;
            }
            let Some(fref_inf) = ws
                .inference(fref.module)
                .and_then(|ir| ir.per_fn.get(&BodyOwner::Function(fref.func)))
            else {
                continue;
            };
            let resolves = owner_inlined
                && match intr_ret {
                    // Direct: validate the intrinsic's element under this site's subst.
                    Some(intr_ret) => compute_call_spec(ws, inf, c, fref, &SpecSubst::new())
                        .is_some_and(|(_, subst)| {
                            native_buffer_alloc_elem_ctype_spec(ws, fref_inf, intr_ret, &subst)
                                .is_ok()
                        }),
                    // Transitive (the intrinsic lives deeper in the chain): the
                    // site must specialize, and conservatively every reified
                    // leaf must be allocatable (a GC-value leaf reaching the
                    // intrinsic would fail cdef resolution inside the splice,
                    // which lowering swallows -- reject it loudly here).
                    None => compute_call_spec(ws, inf, c, fref, &SpecSubst::new()).is_some_and(
                        |(key, _)| {
                            key.iter().all(|l| match l {
                                Some(ReifiedType::Prim(_)) => true,
                                Some(ty) => {
                                    !ty.any_nominal(&|name| nominal_is_gc_value(ws, name))
                                }
                                None => false,
                            })
                        },
                    ),
                };
            if !resolves {
                let fname = ws.module(fref.module).functions[fref.func].name;
                let msg = format!(
                    "`{fname}` allocates via `native_buffer_alloc<T>` (directly or through \
                     nested generic calls), which needs a concrete, FFI-clean element type. \
                     This call does not specialize `{fname}` to an allocatable `value`-struct \
                     element (its generic binds to a primitive, a GC-carrying value struct, a \
                     nested generic, or nothing inferrable), so it would reach the shared \
                     generic body that cannot name the ffi ctype. Call it at a concrete \
                     FFI-clean `value`-struct instantiation, or use a per-type buffer \
                     (`native_buffer_int`/`_float`) for primitive elements. If this call is \
                     inside a generic fn, mark that fn `@inline` so it specializes at its own \
                     call sites -- a plain generic fn is never specialized."
                );
                return Some((msg, body.expr_spans[c]));
            }
        }
    }
    None
}

/// Reject a GC-carrying value struct used at a genuine C boundary (an `extern`
/// signature, or a `NativeBuffer<T>` element) -- a spanned emit-time error, since
/// the cdef the boundary needs was never emitted for this tier. Replaces the old
/// silent skip-then-crash (unfuck #51). Returns `(message, span)` for the first
/// offender, scanning externs then body types. FFI-clean value structs are
/// unaffected (they have a cdef and stay boundary-legal).
pub fn gc_value_c_boundary(ws: &Workspace, mid: ModuleId) -> Option<(String, Span)> {
    let module = ws.module(mid);
    // 1. extern signatures: a param/return that is a GC-carrying value struct.
    for (_, f) in module.functions.iter() {
        if !f.is_extern {
            continue;
        }
        let tys = f.param_decls.iter().map(|p| p.ty).chain(f.return_ty);
        for ty in tys {
            if let Some(TypeExpr::App { ctor: TypeCtor::Nominal(n), .. }) = static_ty(ws, ty) {
                if nominal_is_gc_value(ws, n) {
                    return Some((format!(
                        "`{n}` is a GC-carrying `value` struct (immutable Lua table) and cannot \
                         cross an `extern` C boundary -- it has no C layout. Use an FFI-clean \
                         value struct (fields int/float/bool or nested FFI-clean value structs), \
                         or pass the fields individually."
                    ), f.name_span));
                }
            }
        }
    }
    // 2. NativeBuffer<GcValue> element anywhere in a body's inferred types.
    let ir = ws.inference(mid)?;
    for (owner, body) in module.bodies.iter() {
        let Some(inf) = ir.per_fn.get(owner) else {
            continue;
        };
        for (e, tyid) in inf.expr_types.iter() {
            if let Some(n) = native_buffer_gc_elem(ws, inf, *tyid) {
                return Some((format!(
                    "`NativeBuffer<{n}>` is invalid: `{n}` is a GC-carrying `value` struct \
                     (immutable Lua table) with no C layout, so `ffi.new(\"{n}[?]\")` names no \
                     ctype. Only FFI-clean value structs (C-mappable fields) are NativeBuffer \
                     elements."
                ), body.expr_spans[e]));
            }
        }
    }
    None
}

// -- `@struct_expand` emit-time rejection (docs/shaders.md, Layer 2).
// The lowering expander replaces a valid call; this pass is the FATAL gate that
// rejects an invalid one, since a per-fn lowering error would only be recorded as
// a lenient skip. Both resolve fields through `struct_expand_variants`, so they
// never diverge. --

/// The declaration-order `(field, ty)` list of value struct `name`, else `None`.
fn resolve_call_fn(
    ws: &Workspace,
    inf: &BodyInference,
    e: Idx<Expr>,
    name: Ustr,
) -> Option<(ModuleId, Idx<Function>)> {
    if let Some(&r) = inf.resolved_overload_calls.get(&e) {
        return Some(r);
    }
    match ws.functions_by_name.get(&name).map(|l| l.as_slice()) {
        Some([single]) => Some(*single),
        _ => None,
    }
}

/// Scan module `mid`'s bodies for a `@struct_expand` call that the lowering
/// expander could not honor: a non-value-struct argument, a field whose type has
/// no payload-unique variant of the target enum, a field type matching two
/// variants, or a return type that is not `[(string, <enum>)]`. Returns the first
/// spanned rejection.
pub fn struct_expand_rejection(ws: &Workspace, mid: ModuleId) -> Option<CompilerError> {
    let module = ws.module(mid);
    let ir = ws.inference(mid)?;
    for (owner, body) in module.bodies.iter() {
        let Some(inf) = ir.per_fn.get(owner) else { continue };
        for (e, expr) in body.expr_arena.iter() {
            let Expr::Call { callee: Callee::Unresolved { name, .. }, args } = expr else {
                continue;
            };
            let Some((fmod, fid)) = resolve_call_fn(ws, inf, e, *name) else { continue };
            let f = &ws.module(fmod).functions[fid];
            if !matches!(crate::intrinsics::expand_kind(f), Some(ExpandKind::StructPairs)) {
                continue;
            }
            let fname = f.name;
            let call_span = body.expr_spans[e];
            let reject = |span: Span, msg: String| Some(CompilerError::new_with_label(span, msg));

            let arg_idxs: Vec<Idx<Expr>> = args.into_iter().collect();
            let [arg] = arg_idxs[..] else {
                return reject(call_span, format!("`@struct_expand` fn `{fname}` takes exactly one argument"));
            };
            let arg_span = body.expr_spans[arg];

            let Some(enum_name) = f.return_ty.and_then(|ret| pairs_return_enum(ws, ret)) else {
                return reject(
                    call_span,
                    format!(
                        "`@struct_expand` fn `{fname}` must return `[(string, <enum>)]`, got `{}`",
                        f.return_ty.map(|r| ws.render(None, r)).unwrap_or_else(|| "()".to_string())
                    ),
                );
            };

            let struct_name = inf
                .expr_types
                .get(arg)
                .and_then(|t| resolve_tyid(ws, inf, *t))
                .and_then(|t| match ty_node(ws, inf, t) {
                    Some(TypeExpr::App { ctor: TypeCtor::Nominal(n), .. }) => Some(n),
                    _ => None,
                });
            let Some(fields) = struct_name.and_then(|n| value_struct_field_tys(ws, n)) else {
                return reject(
                    arg_span,
                    format!("`@struct_expand` fn `{fname}` argument must be a value struct"),
                );
            };

            if let Err(msg) = struct_expand_variants(ws, enum_name, &fields) {
                return reject(arg_span, format!("`@struct_expand`: {msg}"));
            }
        }
    }
    None
}

// -- `@shader_expand` (`rock_shader`) expansion + rejection
// (docs/shaders.md). A call collapses to the pipeline key const
// `"<vert>+<frag>"`; the driver (skull::mir_emit) emits one top-level
// `__rock_shader_register(key, glsl)` per distinct pair. The lowering expander,
// the fatal rejection gate, and the driver's pair collector all resolve args
// through `resolve_shader_pair`, so they never diverge. skull-mir stays free of
// GLSL emission (the layering constraint): only FuncRefs + the key cross up. --

/// A resolved `rock_shader(...)` pipeline. Two forms:
/// - two-arg `rock_shader(vert, frag)`: `vert = Some(..)`, `key = "<vert>+<frag>"`;
///   the driver runs `emit_shader_pipeline`.
/// - one-arg fullscreen `rock_shader(frag)`: `vert = None`, `key = "<frag>"` (a
///   bare name, never containing '+', so it can never collide with a pair key);
///   the driver runs `emit_shader_entry`.
/// `key` is the pipeline name const `pipeline()` looks up.
#[derive(Clone, Debug)]
pub struct ShaderPair {
    pub key: String,
    pub vert: Option<FuncRef>,
    pub frag: FuncRef,
}

pub struct ShaderCollection {
    pub pairs: Vec<ShaderPair>,
    pub callsites: Vec<(ModuleId, BodyOwner)>,
}

/// The `@shader(<stage>)` stage of `f` (`"vertex"`/`"fragment"`), else `None`.
/// A local read of the annotation (skull-codegen-glsl has the same helper, but
/// skull-mir must not depend on it -- the layering constraint).
fn shader_stage_of(f: &Function) -> Option<&'static str> {
    let ann = f.annotations.iter().find(|a| a.name.name.as_str() == "shader")?;
    match ann.args.first().map(|a| &a.value) {
        Some(bone_semantic::hir::AnnoValue::Ident(id)) if id.name.as_str() == "vertex" => Some("vertex"),
        Some(bone_semantic::hir::AnnoValue::Ident(id)) if id.name.as_str() == "fragment" => {
            Some("fragment")
        }
        _ => None,
    }
}

/// Resolve one `rock_shader` argument to a direct reference to a top-level
/// `@shader(<want_stage>)` fn. Rejects (spanned) a non-`Expr::Global` arg (a
/// local, lambda, literal, call -- anything that is not a bare top-level fn
/// name), a name that is not a unique top-level fn, a non-`@shader` fn, or a
/// stage mismatch.
fn resolve_shader_arg(
    ws: &Workspace,
    body: &Body,
    arg: Idx<Expr>,
    want_stage: &str,
) -> Result<FuncRef, CompilerError> {
    let span = body.expr_spans[arg];
    let reject = |msg: String| CompilerError::new_with_label(span, msg);
    let Expr::Global(name) = body.expr_arena[arg] else {
        return Err(reject(format!(
            "`rock_shader` argument must be a direct reference to a top-level `@shader({want_stage})` fn"
        )));
    };
    let fref = match ws.functions_by_name.get(&name).map(|l| l.as_slice()) {
        Some([(module, func)]) => FuncRef { module: *module, func: *func },
        _ => {
            return Err(reject(format!(
                "`rock_shader` argument `{name}` is not a unique top-level fn"
            )))
        }
    };
    let f = &ws.module(fref.module).functions[fref.func];
    match shader_stage_of(f) {
        Some(stage) if stage == want_stage => Ok(fref),
        Some(other) => Err(reject(format!(
            "`rock_shader` expects a `@shader({want_stage})` fn here, but `{name}` is `@shader({other})`"
        ))),
        None => Err(reject(format!(
            "`rock_shader` argument `{name}` is not a `@shader` fn"
        ))),
    }
}

/// Resolve a `rock_shader(...)` call's args to a [`ShaderPair`], or the first
/// spanned rejection. Arity dispatches the form: one arg -> fullscreen
/// (`vert = None`, key = bare frag name), two args -> pair (key = `"<vert>+<frag>"`).
/// Any other arity rejects, naming BOTH legal forms. Shared by the lowering
/// expander ([`Builder::expand_shader_pipeline`]), the fatal gate
/// ([`shader_expand_rejection`]), and the driver collector
/// ([`collect_shader_pairs`]) -- one resolver, never forked.
pub fn resolve_shader_pair(
    ws: &Workspace,
    body: &Body,
    call_expr: Idx<Expr>,
    args: &[Idx<Expr>],
) -> Result<ShaderPair, CompilerError> {
    match args {
        [frag_arg] => {
            let frag = resolve_shader_arg(ws, body, *frag_arg, "fragment")?;
            let fname = ws.module(frag.module).functions[frag.func].name;
            // Bare frag name -- no '+', so it can never collide with a pair key.
            Ok(ShaderPair { key: fname.to_string(), vert: None, frag })
        }
        [vert_arg, frag_arg] => {
            let vert = resolve_shader_arg(ws, body, *vert_arg, "vertex")?;
            let frag = resolve_shader_arg(ws, body, *frag_arg, "fragment")?;
            let vname = ws.module(vert.module).functions[vert.func].name;
            let fname = ws.module(frag.module).functions[frag.func].name;
            Ok(ShaderPair { key: format!("{vname}+{fname}"), vert: Some(vert), frag })
        }
        _ => Err(CompilerError::new_with_label(
            body.expr_spans[call_expr],
            "`rock_shader` takes either one argument -- rock_shader(fragment_fn) \
             (fullscreen) -- or two -- rock_shader(vertex_fn, fragment_fn)"
                .to_string(),
        )),
    }
}

/// Scan module `mid`'s bodies for a `rock_shader` call the lowering expander
/// could not honor (bad arity, a non-`@shader` / non-direct-reference arg, a
/// stage mismatch). Returns the first spanned rejection. FATAL gate: a per-fn
/// lowering error is only a lenient skip, so validation lives here.
pub fn shader_expand_rejection(ws: &Workspace, mid: ModuleId) -> Option<CompilerError> {
    let module = ws.module(mid);
    let ir = ws.inference(mid)?;
    for (owner, body) in module.bodies.iter() {
        let Some(inf) = ir.per_fn.get(owner) else { continue };
        for (e, expr) in body.expr_arena.iter() {
            let Expr::Call { callee: Callee::Unresolved { name, .. }, args } = expr else {
                continue;
            };
            let Some((fmod, fid)) = resolve_call_fn(ws, inf, e, *name) else { continue };
            if !matches!(
                crate::intrinsics::expand_kind(&ws.module(fmod).functions[fid]),
                Some(ExpandKind::ShaderPipeline)
            ) {
                continue;
            }
            let arg_idxs: Vec<Idx<Expr>> = args.into_iter().collect();
            if let Err(err) = resolve_shader_pair(ws, body, e, &arg_idxs) {
                return Some(err);
            }
        }
    }
    None
}

/// Every distinct `rock_shader` pair across the workspace, deduped by key and
/// sorted by key (deterministic registration order -> byte-stable re-emission).
/// The driver ([`skull::mir_emit`]) turns each into one top-level
/// `__rock_shader_register`. Any resolution failure surfaces as `Err` (the fatal
/// [`shader_expand_rejection`] gate runs first, so this is normally infallible).
pub fn collect_shader_pairs(ws: &Workspace) -> Result<Vec<ShaderPair>, CompilerError> {
    Ok(collect_shader_collection(ws)?.pairs)
}

pub fn body_has_shader_pipeline_call(
    ws: &Workspace,
    mid: ModuleId,
    owner: BodyOwner,
) -> bool {
    let module = ws.module(mid);
    let Some(body) = module.bodies.get(&owner) else { return false };
    let inf = ws.inference(mid).and_then(|ir| ir.per_fn.get(&owner));
    body.expr_arena.iter().any(|(e, expr)| {
        let Expr::Call { callee: Callee::Unresolved { name, .. }, .. } = expr else {
            return false;
        };
        let resolved = match inf {
            Some(inf) => resolve_call_fn(ws, inf, e, *name),
            None => match ws.functions_by_name.get(name).map(|l| l.as_slice()) {
                Some([single]) => Some(*single),
                _ => None,
            },
        };
        resolved.is_some_and(|(fmod, fid)| {
            matches!(
                crate::intrinsics::expand_kind(&ws.module(fmod).functions[fid]),
                Some(ExpandKind::ShaderPipeline)
            )
        })
    })
}

pub fn collect_shader_collection(ws: &Workspace) -> Result<ShaderCollection, CompilerError> {
    let mut by_key: std::collections::BTreeMap<String, ShaderPair> =
        std::collections::BTreeMap::new();
    let mut callsites: HashSet<(ModuleId, BodyOwner)> = HashSet::new();
    for (_, &mid) in ws.module_ids.iter() {
        let module = ws.module(mid);
        let ir = ws.inference(mid);
        for (owner, body) in module.bodies.iter() {
            let inf = ir.and_then(|ir| ir.per_fn.get(owner));
            for (e, expr) in body.expr_arena.iter() {
                let Expr::Call { callee: Callee::Unresolved { name, .. }, args } = expr else {
                    continue;
                };
                let resolved = match inf {
                    Some(inf) => resolve_call_fn(ws, inf, e, *name),
                    None => match ws.functions_by_name.get(name).map(|l| l.as_slice()) {
                        Some([single]) => Some(*single),
                        _ => None,
                    },
                };
                let Some((fmod, fid)) = resolved else { continue };
                if !matches!(
                    crate::intrinsics::expand_kind(&ws.module(fmod).functions[fid]),
                    Some(ExpandKind::ShaderPipeline)
                ) {
                    continue;
                }
                callsites.insert((mid, *owner));
                let arg_idxs: Vec<Idx<Expr>> = args.into_iter().collect();
                let pair = resolve_shader_pair(ws, body, e, &arg_idxs)?;
                by_key.entry(pair.key.clone()).or_insert(pair);
            }
        }
    }
    Ok(ShaderCollection {
        pairs: by_key.into_values().collect(),
        callsites: callsites.into_iter().collect(),
    })
}

/// Reject CPU-side calls to `@gpu_only` intrinsics and references to GPU-side
/// functions as runtime fn values. Shader-pipeline expansion arguments are names,
/// not runtime values, and remain legal.
pub fn gpu_only_call_rejection(ws: &Workspace, mid: ModuleId) -> Option<CompilerError> {
    let module = ws.module(mid);
    let ir = ws.inference(mid)?;
    for (owner, body) in module.bodies.iter() {
        let Some(inf) = ir.per_fn.get(owner) else { continue };
        let caller_is_gpu = match owner {
            BodyOwner::Function(fid) => crate::intrinsics::is_gpu_context(&module.functions[*fid]),
            _ => false,
        };
        if caller_is_gpu {
            continue;
        }
        let mut shader_expansion_args = HashSet::new();
        for (e, expr) in body.expr_arena.iter() {
            let Expr::Call {
                callee: Callee::Unresolved { name, .. },
                args,
            } = expr
            else {
                continue;
            };
            let Some((fmod, fid)) = resolve_call_fn(ws, inf, e, *name) else {
                continue;
            };
            let f = &ws.module(fmod).functions[fid];
            if matches!(
                crate::intrinsics::expand_kind(f),
                Some(ExpandKind::ShaderPipeline)
            ) {
                shader_expansion_args.extend(args.clone().into_iter());
            }
        }
        for (e, expr) in body.expr_arena.iter() {
            match expr {
                Expr::Call {
                    callee: Callee::Unresolved { name, .. },
                    ..
                } => {
                    let Some((fmod, fid)) = resolve_call_fn(ws, inf, e, *name) else {
                        continue;
                    };
                    let f = &ws.module(fmod).functions[fid];
                    if !crate::intrinsics::is_gpu_only(f) {
                        continue;
                    }
                    let fname = f.name;
                    return Some(CompilerError::new_with_label(
                        body.expr_spans[e],
                        format!(
                            "`{fname}` is GPU-only and cannot be called from CPU (Lua) code. \
                             Shader helper code that calls `{fname}` must annotate its fn \
                             `@shader(...)` or `@gpu_only`."
                        ),
                    ));
                }
                Expr::Global(name) if !shader_expansion_args.contains(&e) => {
                    let Some((fmod, fid)) = resolve_call_fn(ws, inf, e, *name) else {
                        continue;
                    };
                    let f = &ws.module(fmod).functions[fid];
                    if !crate::intrinsics::is_gpu_context(f) {
                        continue;
                    }
                    let fname = f.name;
                    let kind =
                        if crate::intrinsics::is_gpu_only(f) { "GPU-only" } else { "a shader entry" };
                    return Some(CompilerError::new_with_label(
                        body.expr_spans[e],
                        format!(
                            "`{fname}` is {kind} and cannot be used as a fn value from CPU \
                             (Lua) code"
                        ),
                    ));
                }
                _ => {}
            }
        }
    }
    None
}

fn const_int(body: &Body, e: Idx<Expr>) -> Option<i64> {
    match body.expr_arena[e] {
        Expr::Int(v) => Some(v),
        Expr::UnaryMinus(inner) => match body.expr_arena[inner] {
            Expr::Int(v) => Some(-v),
            _ => None,
        },
        _ => None,
    }
}

/// Scan module `mid`'s bodies for a `string_char(<literal>)` call whose literal
/// argument is outside ASCII 0..=127. `string_char` is `string.char` on luajit:
/// a code > 127 emits a lone high byte (invalid UTF-8 alone, a different code
/// unit on a UTF-16 backend); a negative code aborts `string.char` at runtime.
/// Portable only for 0..=127; for code points use `from_codepoint`. Dynamic
/// (non-literal) arguments are out of scope. Returns the offending arg's span.
pub fn string_char_literal_out_of_range(ws: &Workspace, mid: ModuleId) -> Option<Span> {
    let module = ws.module(mid);
    for (_owner, body) in module.bodies.iter() {
        for (_e, expr) in body.expr_arena.iter() {
            let Expr::Call { callee: Callee::Unresolved { name, .. }, args } = *expr else {
                continue;
            };
            if name.as_str() != "string_char" {
                continue;
            }
            let Some(arg) = args.into_iter().next() else { continue };
            let Some(code) = const_int(body, arg) else { continue };
            if !(0..=127).contains(&code) {
                return Some(body.expr_spans[arg]);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests;
