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

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use bone_semantic::hir::{
    BinOp, BindingInfo, BindingPattern, Body, BodyOwner, BuiltinTy, Callee, Expr, Function,
    MatchArm, Pattern, Statement, TyId, TypeCtor, TypeExpr, VariantArgs, VariantPayload,
};
use ustr::Ustr;
use bone_semantic::inference::{BodyInference, Workspace};
use bone_semantic::prelude::{BoundNext, ForIterInfo, ModuleId, Span};
use comfy_la_arena::{Idx, IdxRange, RawIdx};
use skull_parser::CompilerError;

use crate::intrinsics::ExpandKind;
use crate::mir::*;
use crate::concrete::{ConcreteBodyPlan, ConcreteOwner, ConcretePlans, ConcreteTypeTables};
use crate::monoty::{MonoTy, MonoTyId};

type ConcreteTypeCache = RefCell<rustc_hash::FxHashMap<MonoTyId, MonoTy>>;

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

/// The backend ABI of one emitted body. The scalar-ABI POLICY lives inside and
/// is private to it, so a definition and the call that targets it cannot reach
/// it from two places.
mod abi;
pub use abi::{
    boundary_requires_boxed_representation, declared_boundary_class, requires_concrete_body_plan,
    BackendFnAbiPlan, BoundaryClass, FunctionRepPlan, GlobalPlan, RepArena, RepError, RepId,
    RepLayout,
    RepGuarantees, RepPlan, RepUse, RepUseFacts,
};

mod struct_plan;
pub use struct_plan::{struct_field_plan, StructFieldPlan};

mod coroutine;
pub use coroutine::CoroutinePlan;

/// The `impl Builder` walk, split by topic. Each is a plain `impl Builder<'_>`
/// block in a child module -- Rust allows the impl to be spread across the
/// crate, and a child sees this module's private items, so nothing needed
/// widening to make the split.
mod call;
mod container;
pub use container::{ContainerFamily, ContainerPlan, ContainerStorage, PlaceBundle};
mod control;
mod dispatch;
pub use dispatch::{
    dispatched_callee_decl, dispatched_callee_decls,
    DispatchCtx, EqRoute, FormatPlan, HolePlan, StringifyRoute, StructuralRender,
};
mod embed;
mod expr;
mod flat;
mod interp;
mod kinds;
mod loops;
mod nobox;
mod pattern;
mod resolve;
pub use resolve::{
    backend_variant, backend_variant_group, backend_variant_instance, canonical_variant,
    sibling_variant, NoServingBinding,
};
mod stmt;
mod storage;
mod structs;
mod types;

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
    for i in 0..shape.tuple_width() {
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
    /// (`docs/shaders.md` finding 5). The toggle is contained in the ABI
    /// policy behind [`BackendFnAbiPlan`], which definitions and direct calls
    /// both read.
    values_first_class: bool,
    /// The codegen backend this lowering feeds. Drives backend-variant selection
    /// at every resolved call ([`Builder::resolve_funcref`]): `Lua` rejects a
    /// call whose only binding is a JS decl; `Js` swaps a call to its JS sibling
    /// when the collapsed representative is a luajit-native variant. `Any`
    /// disables the filtering entirely (the GLSL backend + generic/test callers).
    target: bone_semantic::hir::FnBackend,
    /// **The build's one type arena.**
    ///
    /// An [`Instance`](crate::instance::Instance) is an identity only while one
    /// arena is reachable from everything that mints or reads one, and lowering
    /// is where a callee instantiation is first named. It lives here because
    /// this is the build-scoped context every lowering entry point already
    /// carries -- threading a second parameter through them would put the same
    /// object in two places and let a caller supply the wrong one.
    ///
    /// It is a SHARED HANDLE, not an owned arena. The retained
    /// `InstanceGraph` mints instances against the same object, and neither
    /// side can own it outright: moving the arena here would leave the graph's
    /// `MonoTyId`s dangling, and copying it would be the second identity this
    /// stage exists to remove. [`ScalarCtx::sharing`] is how the two are joined.
    interner: std::sync::Arc<crate::monoty::MonoInterner>,
    /// The build's one physical representation arena. It follows this context
    /// so target-specific ABI identities survive from body entry through calls
    /// and publishers.
    rep_arena: std::sync::Arc<abi::RepArena>,
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
            interner: std::sync::Arc::new(crate::monoty::MonoInterner::new()),
            rep_arena: std::sync::Arc::new(abi::RepArena::default()),
        }
    }

    /// **The build's one type arena**, through which every [`Instance`] is
    /// minted and read.
    ///
    /// [`Instance`]: crate::instance::Instance
    pub fn interner(&self) -> &crate::monoty::MonoInterner {
        &self.interner
    }

    /// Share the arena a retained `InstanceGraph` mints against, so the graph's
    /// ids and emission's ids are the same ids.
    ///
    /// Takes the HANDLE, not an arena: the graph keeps minting after this, so a
    /// snapshot would go stale the moment an incremental update expanded it.
    /// The arena is monotonic, so emission naming a type the graph never
    /// reached simply extends the shared one.
    pub fn sharing(mut self, interner: std::sync::Arc<crate::monoty::MonoInterner>) -> Self {
        self.interner = interner;
        self
    }

    /// The shared handle, for a consumer that must mint against the SAME arena
    /// later -- an `InstanceGraph` built on demand rather than up front.
    pub fn interner_handle(&self) -> std::sync::Arc<crate::monoty::MonoInterner> {
        std::sync::Arc::clone(&self.interner)
    }

    pub fn rep_arena(&self) -> std::sync::Arc<abi::RepArena> {
        std::sync::Arc::clone(&self.rep_arena)
    }

    /// Set the codegen backend this lowering feeds (see [`target`]).
    /// The backend this context lowers for. Read by consumers that must ask the
    /// EMITTER's own declaration selection rather than restate it -- the site
    /// join compares a target-selected sibling against the target-neutral
    /// graph's representative through `backend_variant`.
    pub fn target(&self) -> bone_semantic::hir::FnBackend {
        self.target
    }

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

/// A hoisted nested `fn`: the private top-level name it resolves to, and the
/// value ABI its definition and every call site both read (so the two can never
/// drift, the same single-gate discipline as [`BackendFnAbiPlan`]).
#[derive(Clone)]
struct NestedFn {
    hoisted: Ustr,
    /// Declared param names, so a call site can place named args into slots
    /// (`slotted_call_args`) exactly as a top-level callee's would be.
    param_names: Vec<Ustr>,
    representation: FunctionRepPlan,
}

impl NestedFn {
    fn uses_scalar_abi(&self) -> bool {
        self.representation.uses_scalar_abi()
    }

    pub(crate) fn param_slots(&self, arena: &RepArena) -> Vec<RetSlot> {
        self.representation.params().iter().map(|id| arena.ret_slot(*id)).collect()
    }

    pub(crate) fn result_shape(&self, arena: &RepArena) -> Option<ValueShape> {
        self.representation.result().and_then(|id| arena.value_shape(id))
    }
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
    let plan = BackendFnAbiPlan::for_boundary_declaration(ws, scx, fref);
    let Some(names) = plan.lua_param_names(ws) else {
        return Vec::new();
    };
    let origins = scalar_abi_param_origins(ws, &plan);
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
/// unambiguous). Built by the same walk under the `.` separator, over the SAME
/// plan the names come from -- a second plan here would be free to describe a
/// different parameter list than the one being checked.
fn scalar_abi_param_origins(ws: &Workspace, plan: &BackendFnAbiPlan) -> Vec<String> {
    if !plan.uses_scalar_abi() {
        return Vec::new();
    }
    let shapes = plan.param_shapes();
    let decl = plan.decl();
    let f = &ws.module(decl.module).functions[decl.func];
    let mut out = Vec::new();
    for (pd, slot) in f.param_decls.iter().zip(shapes.iter()) {
        slot_param_origins(pd.name.as_str(), slot, &mut out);
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

/// The emitted param names ONE declared param contributes, per its wire slot.
/// A tuple slot indexes into the base (`pq__0__x`) using the same `__` join the
/// leaf encoding already uses, so the whole list stays one namespace and the
/// injectivity check below sees tuple leaves like any other.
fn slot_param_names(base: &str, slot: &RetSlot, out: &mut Vec<String>) {
    match slot {
        RetSlot::Opaque => out.push(base.to_string()),
        RetSlot::Leaves(shape) => leaf_param_names(base, shape, out),
        RetSlot::Nested(inner) => {
            // The presence leads the slots, so it takes the FIRST name -- the
            // order `bind_param_slot` binds cells in and `tuple_repr_to_slots`
            // writes values in. Skipping it emits a list one short of
            // `slot_count`, which Lua nil-fills rather than rejects.
            if inner.nullable {
                out.push(format!("{base}{LEAF_PARAM_SEP}p"));
            }
            for (i, s) in inner.slots.iter().enumerate() {
                slot_param_names(&format!("{base}{LEAF_PARAM_SEP}{i}"), s, out);
            }
        }
    }
}

/// [`slot_param_names`] in the human-facing spelling the collision diagnostic
/// uses -- the same traversal, so a reported origin always names a real emitted
/// param.
fn slot_param_origins(base: &str, slot: &RetSlot, out: &mut Vec<String>) {
    match slot {
        RetSlot::Opaque => out.push(format!("the declared param `{base}`")),
        RetSlot::Leaves(shape) => out.extend(
            leaf_names(shape, base, ".")
                .into_iter()
                .map(|p| format!("the leaf `{p}` of value param `{base}`")),
        ),
        RetSlot::Nested(inner) => {
            if inner.nullable {
                out.push(format!("the presence of nullable tuple param `{base}`"));
            }
            for (i, s) in inner.slots.iter().enumerate() {
                slot_param_origins(&format!("{base}.{i}"), s, out);
            }
        }
    }
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
    Uint,
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
        "__op_add" if lt == ArithTy::Uint && rt == ArithTy::Uint => Inst::UintAdd(lv, rv),
        "__op_add" => Inst::IntAdd(lv, rv),
        "__op_sub" if float => Inst::FloatSub(lv, rv),
        "__op_sub" if lt == ArithTy::Uint && rt == ArithTy::Uint => Inst::UintSub(lv, rv),
        "__op_sub" => Inst::IntSub(lv, rv),
        "__op_mul" if float => Inst::FloatMul(lv, rv),
        "__op_mul" if lt == ArithTy::Uint && rt == ArithTy::Uint => Inst::UintMul(lv, rv),
        "__op_mul" => Inst::IntMul(lv, rv),
        // `/` is always float division (Lua `/` promotes int operands);
        // integer floor division is the separate `./` -> `__op_idiv`, which
        // codegen renders `math.floor(a / b)`.
        "__op_div" => Inst::FloatDiv(lv, rv),
        "__op_idiv" if lt == ArithTy::Uint && rt == ArithTy::Uint => Inst::UintDiv(lv, rv),
        "__op_idiv" => Inst::IntDiv(lv, rv),
        "__op_mod" if float => Inst::FloatMod(lv, rv),
        "__op_mod" if lt == ArithTy::Uint && rt == ArithTy::Uint => Inst::UintMod(lv, rv),
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
        Expr::As { value, target, kind } => {
            tag!(23);
            target.hash(h);
            kind.hash(h);
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
        Expr::Call { callee, args, .. } => {
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

pub fn lower_function_with_plans(
    ws: &Workspace,
    scx: &ScalarCtx,
    plan: &ConcreteBodyPlan,
    concrete_plans: &ConcretePlans,
) -> Result<MirFunc, CompilerError> {
    lower_instance_with_plans(ws, scx, plan, concrete_plans)
}

/// Lower an ordinary function while resolving every concrete callee ABI from
/// the same graph-owned plan set. A missing selected callee is an error rather
/// than a fallback to declaration-shaped ABI facts.
pub fn lower_instance_with_plans(
    ws: &Workspace,
    scx: &ScalarCtx,
    plan: &ConcreteBodyPlan,
    concrete_plans: &ConcretePlans,
) -> Result<MirFunc, CompilerError> {
    lower_instance_with_plan_inner(ws, scx, plan, concrete_plans)
}

fn lower_instance_with_plan_inner(
    ws: &Workspace,
    scx: &ScalarCtx,
    plan: &ConcreteBodyPlan,
    concrete_plans: &ConcretePlans,
) -> Result<MirFunc, CompilerError> {
    let inst = plan
        .function_instance()
        .ok_or_else(|| gap("concrete plan does not own a function instance"))?;
    let (mid, fid) = (inst.func.module, inst.func.func);
    let module = ws.module(mid);
    let body = module
        .bodies
        .get(&BodyOwner::Function(fid))
        .ok_or_else(|| gap("concrete plan function has no body"))?;
    let inf = ws
        .inference(mid)
        .and_then(|ir| ir.per_fn.get(&BodyOwner::Function(fid)));
    if let Some(inf) = inf {
        let func = &module.functions[fid];
        validate_concrete_type_tables(
            ws,
            inf,
            body,
            &plan.types,
            "concrete plan",
            func.param_decls
                .iter()
                .map(|param| param.ty)
                .chain(func.return_ty),
        )?;
        for (expr, _) in inf.expr_types.iter() {
            if plan.types.expr_type(expr).is_none() && !plan.types.is_untyped_expr(expr) {
                return Err(gap(format!("concrete plan is missing expression type {expr:?}")));
            }
        }
        for (binding, _) in inf.binding_types.iter() {
            if plan.types.binding_type(binding).is_none()
                && !plan.types.is_untyped_binding(binding)
            {
                return Err(gap(format!("concrete plan is missing binding type {binding:?}")));
            }
        }
    }
    for site in plan.callees.keys() {
        if !plan.callee_signatures.contains_key(site) {
            return Err(gap(format!(
                "concrete plan is missing the callee signature for site {:?}",
                site.op
            )));
        }
        if !matches!(site.domain, crate::site::SiteDomain::Own)
            && plan.domain_types(&site.domain).is_none()
        {
            return Err(gap(format!(
                "concrete plan is missing type tables for site domain {:?}",
                site.domain
            )));
        }
    }
    validate_concrete_callee_plans(ws, scx, plan, concrete_plans)?;
    lower_body(ws, scx, inst.func, inst.clone(), plan, concrete_plans)
}

fn lower_body(
    ws: &Workspace,
    scx: &ScalarCtx,
    self_ref: FuncRef,
    subject: crate::instance::Instance,
    concrete_plan: &ConcreteBodyPlan,
    concrete_plans: &ConcretePlans,
) -> Result<MirFunc, CompilerError> {
    let (mid, fid) = (self_ref.module, self_ref.func);
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
    f.rep_arena = Some(scx.rep_arena());
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
    // leaf cells and a value return becomes a multi-value `Return`. This body's
    // own ABI comes from the one plan the calls that target it read.
    let abi = BackendFnAbiPlan::for_concrete_plan(ws, scx, concrete_plan);
    f.abi_plan = Some(abi.clone());
    let param_reps = abi.representation().params().to_vec();
    let ret_rep = abi.representation().result();
    let mut b = Builder {
        ws,
        scx,
        concrete_plan,
        concrete_plans,
        concrete_type_cache: RefCell::new(rustc_hash::FxHashMap::default()),
        generic_bounds: &func.generic_bounds,
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
        param_reps,
        ret_rep,
        fn_name_span: Some(func.name_span),
        cur_span: None,
        cur_expr: None,
        cur_domain: crate::site::SiteDomain::Own,
        cur_op: crate::site::SiteOp::DirectCall,
        site_owner: Some(subject.clone()),
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
    b.lower_params()?;
    b.fill_param_defaults()?;
    b.lower_root()?;
    Ok(b.f)
}

/// Lower an always-emitted global from its graph-owned concrete plan.
pub fn lower_global_with_plans(
    ws: &Workspace,
    scx: &ScalarCtx,
    plan: &ConcreteBodyPlan,
    concrete_plans: &ConcretePlans,
    name: Ustr,
) -> Result<Option<MirFunc>, CompilerError> {
    let ConcreteOwner::Global { module, index } = &plan.owner else {
        return Err(gap("concrete plan does not own a global"));
    };
    let module_ref = ws.module(*module);
    if *index as usize >= module_ref.globals.len() {
        return Err(gap("concrete global plan has an invalid global index"));
    }
    let gid = Idx::from_raw(RawIdx::from_u32(*index));
    validate_always_plan(ws, scx, *module, BodyOwner::Global(gid), plan, concrete_plans)?;
    lower_global_inner(ws, scx, *module, gid, name, plan, concrete_plans)
}

fn lower_global_inner(
    ws: &Workspace,
    scx: &ScalarCtx,
    mid: ModuleId,
    gid: Idx<bone_semantic::hir::GlobalDef>,
    name: Ustr,
    concrete_plan: &ConcreteBodyPlan,
    concrete_plans: &ConcretePlans,
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
    let mut f = MirFunc::new(name);
    f.rep_arena = Some(scx.rep_arena());
    let cur = f.entry();
    let mut b = Builder {
        ws,
        scx,
        concrete_plan,
        concrete_plans,
        concrete_type_cache: RefCell::new(rustc_hash::FxHashMap::default()),
        generic_bounds: &[],
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
        param_reps: Vec::new(),
        ret_rep: None,
        fn_name_span: None,
        cur_span: None,
        cur_expr: None,
        cur_domain: crate::site::SiteDomain::Own,
        cur_op: crate::site::SiteOp::DirectCall,
        site_owner: None,
        group_binds: HashMap::new(),
        tuple_binds: HashMap::new(),
        static_cells: HashMap::new(),
        static_owner: name,
        static_counters: HashMap::new(),
        nested_fn_globals: HashMap::new(),
        nested_fn_counters: HashMap::new(),
        presence_only: None,
    };
    let global_plan = GlobalPlan::for_concrete_result(
        ws,
        scx,
        concrete_plan.signature.result.map(|id| b.concrete_mono(id)),
    )
    .map_err(|error| match error.span.or(b.cur_span) {
        Some(span) => CompilerError::new_with_label(span, error.message),
        None => CompilerError::new_no_location(error.message),
    })?;
    b.f.global_plan = Some(global_plan);
    // A `nobox` global is category-2 storage: its concrete plan owns the layout,
    // so the initializer RETURNS leaves and `mir_emit` binds one Lua global per
    // leaf. Without this the body packs a box the verifier then rejects.
    b.ret_rep = (!scx.values_first_class)
        .then(|| b.f.global_plan.as_ref().expect("global plan").representation());
    b.lower_root()?;
    Ok(Some(b.f))
}

/// Lower an always-emitted decorator from its graph-owned concrete plan.
pub fn lower_decorator_with_plans(
    ws: &Workspace,
    scx: &ScalarCtx,
    plan: &ConcreteBodyPlan,
    concrete_plans: &ConcretePlans,
    name: Ustr,
) -> Result<MirFunc, CompilerError> {
    let ConcreteOwner::Decorator { module, index } = &plan.owner else {
        return Err(gap("concrete plan does not own a decorator"));
    };
    let module_ref = ws.module(*module);
    if *index as usize >= module_ref.decorators.len() {
        return Err(gap("concrete decorator plan has an invalid decorator index"));
    }
    let did = Idx::from_raw(RawIdx::from_u32(*index));
    validate_always_plan(ws, scx, *module, BodyOwner::Decorator(did), plan, concrete_plans)?;
    lower_decorator_inner(ws, scx, *module, did, name, plan, concrete_plans)
}

fn validate_always_plan(
    ws: &Workspace,
    scx: &ScalarCtx,
    mid: ModuleId,
    owner: BodyOwner,
    plan: &ConcreteBodyPlan,
    concrete_plans: &ConcretePlans,
) -> Result<(), CompilerError> {
    let inf = ws
        .inference(mid)
        .and_then(|ir| ir.per_fn.get(&owner))
        .ok_or_else(|| gap("concrete always-emitted plan has no inference"))?;
    let body = ws
        .module(mid)
        .bodies
        .get(&owner)
        .ok_or_else(|| gap("concrete always-emitted plan has no body"))?;
    let declared_types = match owner {
        BodyOwner::Global(index) => vec![ws.module(mid).globals[index].ty],
        BodyOwner::Decorator(index) => ws.module(mid).decorators[index]
            .param_decls
            .iter()
            .map(|param| param.ty)
            .collect(),
        BodyOwner::Function(_)
        | BodyOwner::StructDefaults(_)
        | BodyOwner::Enum(_)
        | BodyOwner::Newtype(_) => Vec::new(),
    };
    validate_concrete_type_tables(
        ws,
        inf,
        body,
        &plan.types,
        "concrete always-emitted plan",
        declared_types,
    )?;
    for (expr, _) in inf.expr_types.iter() {
        if plan.types.expr_type(expr).is_none() && !plan.types.is_untyped_expr(expr) {
            return Err(gap(format!(
                "concrete always-emitted plan is missing expression type {expr:?}"
            )));
        }
    }
    for (binding, _) in inf.binding_types.iter() {
        if plan.types.binding_type(binding).is_none()
            && !plan.types.is_untyped_binding(binding)
        {
            return Err(gap(format!(
                "concrete always-emitted plan is missing binding type {binding:?}"
            )));
        }
    }
    for site in plan.callees.keys() {
        if !plan.callee_signatures.contains_key(site) {
            return Err(gap(format!(
                "concrete always-emitted plan is missing the callee signature for site {:?}",
                site.op
            )));
        }
        if !matches!(site.domain, crate::site::SiteDomain::Own)
            && plan.domain_types(&site.domain).is_none()
        {
            return Err(gap(format!(
                "concrete always-emitted plan is missing type tables for site domain {:?}",
                site.domain
            )));
        }
    }
    validate_concrete_callee_plans(ws, scx, plan, concrete_plans)?;
    Ok(())
}

fn validate_concrete_callee_plans(
    ws: &Workspace,
    scx: &ScalarCtx,
    plan: &ConcreteBodyPlan,
    concrete_plans: &ConcretePlans,
) -> Result<(), CompilerError> {
    for (site, owner) in &plan.callees {
        let needs_plan = requires_concrete_body_plan(ws, owner)
            && crate::monoty::too_deep_to_instantiate(owner.arg_types(scx.interner()).iter()).is_none();
        if !needs_plan {
            continue;
        }
        let callee_plan = concrete_plans.get(owner).ok_or_else(|| {
            gap(format!(
                "concrete callee body plan is missing for site {:?}",
                site.op
            ))
        })?;
        let recorded = plan.callee_signatures.get(site).ok_or_else(|| {
            gap(format!(
                "concrete plan is missing the callee signature for site {:?}",
                site.op
            ))
        })?;
        if recorded != &callee_plan.signature {
            return Err(gap(format!(
                "concrete callee signature disagrees with its body plan at site {:?}",
                site.op
            )));
        }
    }
    Ok(())
}

fn validate_concrete_type_tables(
    ws: &Workspace,
    inf: &BodyInference,
    body: &Body,
    tables: &ConcreteTypeTables,
    label: &str,
    declared_types: impl IntoIterator<Item = TyId>,
) -> Result<(), CompilerError> {
    fn visit(
        ws: &Workspace,
        inf: &BodyInference,
        tables: &ConcreteTypeTables,
        label: &str,
        ty: TyId,
        seen: &mut HashSet<TyId>,
    ) -> Result<(), CompilerError> {
        if !seen.insert(ty) {
            return Ok(());
        }
        if tables.ty_type(ty).is_none() {
            return Err(gap(format!("{label} is missing concrete type node {ty:?}")));
        }
        let node = match ty {
            TyId::Static(mid, idx) => ws.module(mid).ty_arena[idx],
            TyId::Local(idx) => inf.ty_arena[idx],
        };
        match node {
            TypeExpr::Nullable(inner) => visit(ws, inf, tables, label, ty.with_idx(inner), seen),
            TypeExpr::App { params, .. } => {
                for param in params {
                    visit(ws, inf, tables, label, ty.with_idx(param), seen)?;
                }
                Ok(())
            }
            TypeExpr::Function { params, ret } => {
                for param in params {
                    visit(ws, inf, tables, label, ty.with_idx(param), seen)?;
                }
                visit(ws, inf, tables, label, ty.with_idx(ret), seen)
            }
            TypeExpr::Tuple { elements } => {
                for element in elements {
                    visit(ws, inf, tables, label, ty.with_idx(element), seen)?;
                }
                Ok(())
            }
            TypeExpr::Infer(var, _) => match inf.substitutions.get(var) {
                Some(next) => visit(ws, inf, tables, label, next, seen),
                None => Ok(()),
            },
            TypeExpr::Builtin(_) | TypeExpr::Generic(..) => Ok(()),
        }
    }

    let mut seen = HashSet::new();
    for (expr, ty) in inf.expr_types.iter() {
        if tables.is_untyped_expr(expr) {
            continue;
        }
        visit(ws, inf, tables, label, *ty, &mut seen)?;
    }
    for (binding, ty) in inf.binding_types.iter() {
        if tables.is_untyped_binding(binding) {
            continue;
        }
        visit(ws, inf, tables, label, *ty, &mut seen)?;
    }
    for ty in declared_types {
        visit(ws, inf, tables, label, ty, &mut seen)?;
    }
    for (_, binding) in body.binding_arena.iter() {
        if let Some(ty) = binding.declared_ty {
            visit(ws, inf, tables, label, ty, &mut seen)?;
        }
    }
    for (_, node) in body.expr_arena.iter() {
        if let Expr::As { target, .. } = node {
            visit(ws, inf, tables, label, *target, &mut seen)?;
        }
    }
    for (expr, node) in body.expr_arena.iter() {
        let Expr::StructLiteral { name, .. } = node else { continue };
        let fields = tables.struct_field_types.get(expr).ok_or_else(|| {
            gap(format!(
                "{label} is missing concrete fields for struct literal `{name}` ({expr:?})"
            ))
        })?;
        let sid = ws
            .struct_by_name
            .get(&name)
            .ok_or_else(|| gap(format!("{label} names unknown struct `{name}`")))?;
        let def = &ws.modules[sid.module.index()].structs[sid.def];
        for field in &def.fields {
            if !fields.contains_key(&field.name) {
                return Err(gap(format!(
                    "{label} is missing concrete field `{name}.{}` ({expr:?})",
                    field.name
                )));
            }
        }
    }
    Ok(())
}

fn lower_decorator_inner(
    ws: &Workspace,
    scx: &ScalarCtx,
    mid: ModuleId,
    did: Idx<bone_semantic::hir::DecoratorDef>,
    name: Ustr,
    concrete_plan: &ConcreteBodyPlan,
    concrete_plans: &ConcretePlans,
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
    let mut f = MirFunc::new(name);
    f.rep_arena = Some(scx.rep_arena());
    let cur = f.entry();
    let representation = scx
        .rep_arena()
        .boxed_function(scx.target(), concrete_plan.signature.params.len());
    f.abi_representation = Some(representation.clone());
    let param_reps = representation.params().to_vec();
    let ret_rep = concrete_plan
        .signature
        .result
        .map(|_| representation.result().expect("boxed decorator result plan"));
    let mut b = Builder {
        ws,
        scx,
        concrete_plan,
        concrete_plans,
        concrete_type_cache: RefCell::new(rustc_hash::FxHashMap::default()),
        generic_bounds: &[],
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
        // Decorator bodies never run `inline_functions` -- box value args, but
        // their declared parameters still need the owned ABI slots.
        param_reps,
        ret_rep,
        fn_name_span: None,
        cur_span: None,
        cur_expr: None,
        cur_domain: crate::site::SiteDomain::Own,
        cur_op: crate::site::SiteOp::DirectCall,
        site_owner: None,
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
    b.lower_params()?;
    b.fill_param_defaults()?;
    b.lower_root()?;
    Ok(b.f)
}

struct Builder<'a> {
    ws: &'a Workspace,
    scx: &'a ScalarCtx,
    concrete_plan: &'a ConcreteBodyPlan,
    concrete_plans: &'a ConcretePlans,
    concrete_type_cache: ConcreteTypeCache,
    /// The enclosing fn's `generic_bounds`, parallel to its `generics`:
    /// `Some(elem)` where a generic is declared `I: Iterator<elem>`. THE declared
    /// source for a placeholder `next`'s element shape -- the call's inferred
    /// type is checked once against the bound and does not carry the
    /// instantiation, so reading the element off it plans the wrong shape.
    generic_bounds: &'a [Option<TyId>],
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
    /// Per-param physical representation identity, parallel to
    /// `body.param_bindings`. Shape structs are projected only at the
    /// flatten/rebuild seams; this is the body-owned ABI answer.
    param_reps: Vec<RepId>,
    /// The body's physical result representation identity, if it has one.
    ret_rep: Option<RepId>,
    /// The fn's name span, for spanned emit-time rejections that have no better
    /// anchor (the non-unit-coroutine fall-off check). `None` for non-fn bodies.
    fn_name_span: Option<Span>,
    /// The source span currently being lowered, maintained by [`Builder::lower_expr`].
    /// Instructions a post-transform check can reject stamp it into
    /// [`MirFunc::inst_spans`] so the diagnostic points at the construct rather
    /// than at the enclosing fn.
    cur_span: Option<Span>,
    /// The source EXPRESSION currently being lowered, maintained by
    /// [`Builder::lower_expr`] exactly beside [`Self::cur_span`]. Together with
    /// [`Self::cur_op`] this is the [`crate::site::Site`] every call
    /// instruction emitted from here down is stamped with, so the identity is
    /// recorded at ONE choke point ([`Builder::emit_cur`]) rather than at each
    /// of the call-emitting sites -- a per-site record would make a MISSING one
    /// indistinguishable from a legitimately siteless call.
    ///
    /// `None` only outside any expression (a fn prologue), which is the
    /// exclusion the site census reports by cause.
    cur_expr: Option<Idx<Expr>>,
    /// WHICH expression arena [`Self::cur_expr`] indexes. A struct-default
    /// initializer is lowered from the declaring struct's `StructDefaults` body
    /// into this CFG while `site_owner` still names the enclosing function, so
    /// its index is only meaningful against THAT arena -- and the walk records
    /// it under the same domain.
    cur_domain: crate::site::SiteDomain,
    /// The concrete INSTANCE being lowered, which OWNS every site recorded from
    /// this builder.
    /// `None` for the always-emitted units -- a global initializer and a
    /// decorator body are not functions and have no `FuncRef`, so their calls
    /// carry no site and the census reports them under that cause rather than
    /// losing them.
    site_owner: Option<crate::instance::Instance>,
    /// Which OPERATION of [`Self::cur_expr`] is being lowered. `DirectCall` is
    /// the default because a call emitted while lowering an expression is that
    /// expression's own call; the witness and synthesized routes override it
    /// for their sub-lowering through [`Builder::with_site_op`], because one
    /// expression carries several distinct callee obligations and they must
    /// stay distinguishable.
    cur_op: crate::site::SiteOp,
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
    fn rep_slot(&self, id: RepId) -> RetSlot {
        self.scx.rep_arena().ret_slot(id)
    }

    fn param_slot(&self, index: usize) -> Result<RetSlot, CompilerError> {
        self.param_reps
            .get(index)
            .copied()
            .map(|id| self.rep_slot(id))
            .ok_or_else(|| self.gap_here(format!("parameter {index} has no representation plan")))
    }

    fn result_shape(&self) -> Option<ValueShape> {
        self.ret_rep.and_then(|id| self.scx.rep_arena().value_shape(id))
    }

    fn result_tuple(&self) -> Option<TupleRetShape> {
        self.ret_rep.and_then(|id| self.scx.rep_arena().tuple_shape(id))
    }

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
        let is_call = matches!(
            inst,
            Inst::Call { .. }
                | Inst::CallMulti { .. }
                | Inst::CallValue { .. }
                | Inst::CallValueMulti { .. }
                | Inst::CallBoundMulti { .. }
        );
        let id = self.f.emit(self.cur, inst);
        if let Some(span) = self.cur_span {
            self.f.inst_spans.insert(id, span);
        }
        // THE choke point for call-site identity: every call FORM entering the
        // arena is stamped here, so a missing stamp is a property of this ONE
        // branch rather than of 48 call-emitting sites each remembering to
        // record. `CallBoundMulti` is included: it is the `for` protocol's
        // `next` witness on the caller-generic path, and its concrete sibling on
        // the other arm carries a site, so leaving it out made the SAME source
        // construct recorded or not depending on which arm it took. It is still
        // a call a body may not carry into emission, which is a separate
        // rejection.
        //
        // Not yet total: a call emitted outside any expression carries no site,
        // reported by `skull::site_join::census` rather than assumed away.
        // A fn-value MINT is stamped too. It is not a call, but it is a callee
        // OBLIGATION -- the one place a declaration becomes a spelled value --
        // and the graph records it at the same `Site { expr, FnValue }`. Keyed
        // on the operation rather than on the instruction, because
        // `Inst::Global` is also a module global and a `static` cell and those
        // name no callee.
        let is_mint = self.cur_op == crate::site::SiteOp::FnValue;
        if is_call || is_mint {
            if let (Some(owner), Some(expr)) = (self.site_owner.clone(), self.cur_expr) {
                let owner = crate::site::intern_owner(&mut self.f.site_owners, owner);
                self.f.call_sites.insert(
                    id,
                    crate::site::CallSite {
                        owner,
                        site: crate::site::Site {
                            domain: self.cur_domain.clone(),
                            expr,
                            op: self.cur_op.clone(),
                        },
                    },
                );
            }
        }
        id
    }

    /// The shared type/dispatch context this builder resolves through. Every
    /// predicate below is a delegate: `DispatchCtx` is the one implementation,
    /// so the instance walk asking the same question cannot get a second answer
    /// (`skull_mir::lower::dispatch`).
    pub(crate) fn concrete_mono(&self, id: MonoTyId) -> MonoTy {
        let cache = &self.concrete_type_cache;
        if let Some(ty) = cache.borrow().get(&id).cloned() {
            return ty;
        }
        let ty = self.scx.interner().get(id);
        cache.borrow_mut().insert(id, ty.clone());
        ty
    }

    pub(crate) fn dispatch(&self) -> dispatch::DispatchCtx<'_> {
        dispatch::DispatchCtx {
            ws: self.ws,
            inf: self.inf,
            body: self.body,
            concrete_plan: Some(self.concrete_plan),
            concrete_types: self.concrete_plan.domain_types(&self.cur_domain),
            concrete_type_cache: Some(&self.concrete_type_cache),
            interner: Some(self.scx.interner()),
            rep_arena: self.scx.rep_arena.as_ref(),
            target: self.scx.target,
            logical_subst: None,
            values_first_class: self.scx.values_first_class,
        }
    }

    /// Lower `f` with the site EXPRESSION overridden, restoring it after.
    ///
    /// The symmetric half of [`Self::with_site_op`]. Most calls inherit the
    /// cursor `lower_expr`/`lower_expr_repr` install, but a PLACE expression is
    /// never lowered as a value -- `lower_assign_effect` dispatches on
    /// `expr_arena[target]` directly -- so the calls its arms synthesize had no
    /// cursor at all in statement position, and the enclosing one in value
    /// position. Both are wrong against a walk that keys those obligations on
    /// the `Expr::Index` node itself.
    fn with_site_expr<T>(&mut self, expr: Idx<Expr>, f: impl FnOnce(&mut Self) -> T) -> T {
        let outer = self.cur_expr.replace(expr);
        let out = f(self);
        self.cur_expr = outer;
        out
    }

    /// Lower `f` with the site DOMAIN overridden, restoring it after.
    ///
    /// The third scoped half, beside the expression and the operation: a body
    /// lowered from another arena keeps its own indices, and the domain is what
    /// makes them readable. A flag rather than a cursor swap, because the nested
    /// `lower_expr` installs the foreign index itself.
    fn in_domain<T>(
        &mut self,
        domain: crate::site::SiteDomain,
        f: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let outer = std::mem::replace(&mut self.cur_domain, domain);
        let out = f(self);
        self.cur_domain = outer;
        out
    }

    /// Lower `f` with the site OPERATION overridden, restoring it after. The
    /// witness and synthesized routes emit calls that belong to the enclosing
    /// expression but are NOT its own call, and a `for` head carries several at
    /// one expression -- so the operation has to be scoped, not global.
    fn with_site_op<T>(
        &mut self,
        op: crate::site::SiteOp,
        f: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let outer = std::mem::replace(&mut self.cur_op, op);
        let out = f(self);
        self.cur_op = outer;
        out
    }

    /// Whether the current block is still open (no terminator set yet). A
    /// diverging sub-expression (`return`/`break`/`continue`) sets a real
    /// terminator, so after lowering it `is_open()` is false and the caller
    /// must NOT append a fall-through `Store`/`Goto`.
    fn is_open(&self) -> bool {
        matches!(self.f.blocks[self.cur].term, Term::Unreachable)
    }

    fn lower_params(&mut self) -> Result<(), CompilerError> {
        let pbs: Vec<Idx<BindingInfo>> = self.body.param_bindings.into_iter().collect();
        for (i, pb) in pbs.iter().enumerate() {
            let name = self.body.binding_arena[*pb].name;
            let slot = self.param_slot(i)?;
            self.bind_param_slot(*pb, name, &slot);
        }
        Ok(())
    }

    /// Bind one declared param to the cells its wire slot occupies, in the Lua
    /// parameter list.
    ///
    /// THE definition-side counterpart of `slot_param_names`, and the single
    /// implementation for all three definition forms (a top-level body, a
    /// hoisted nested fn, a lambda). They must agree with the caller on both the
    /// COUNT and the depth-first ORDER: Lua nil-fills a short call rather than
    /// rejecting it, so a disagreement here is a silent wrong answer, not an
    /// error. A splatting param has no single cell, so it binds as a GROUP (a
    /// value's leaves) or a TUPLE (a tuple's slots) and reads rebuild from them.
    pub(crate) fn bind_param_slot(
        &mut self,
        pb: Idx<BindingInfo>,
        name: Ustr,
        slot: &RetSlot,
    ) {
        if !slot.splats() {
            let cty = self.binding_ty(pb);
            let cell = self.f.new_cell_typed(Some(name), cty);
            self.f.params.push(cell);
            self.binds.insert(raw(pb), cell);
            return;
        }
        let cells: Vec<Idx<Cell>> = (0..slot.slot_count())
            .map(|li| {
                let c = self.f.new_cell(Some(Ustr::from(format!("{name}__{li}").as_str())));
                self.f.params.push(c);
                c
            })
            .collect();
        match slot {
            RetSlot::Leaves(shape) => {
                self.group_binds.insert(raw(pb), (cells, shape.clone()));
            }
            RetSlot::Nested(inner) => {
                self.tuple_binds.insert(raw(pb), (cells, inner.clone()));
            }
            RetSlot::Opaque => unreachable!("guarded by `splats`"),
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
                let leaves = self.repr_to_storage_leaves(&r, &shape, "struct default")?;
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
        let plan = self.concrete_plan;
        let Some(result_id) = plan.signature.result else { return Ok(()) };
        let result = self.concrete_mono(result_id);
        let crate::monoty::MonoTy::App {
            ctor: crate::monoty::MonoCtor::Nominal(name),
            params,
        } = result
        else {
            return Ok(())
        };
        if name.as_str() != "Coroutine" {
            return Ok(())
        }
        let Some(result) = params.get(1) else { return Ok(()) };
        let nonunit_concrete = match result {
            crate::monoty::MonoTy::Unit | crate::monoty::MonoTy::Nullable(_) => false,
            crate::monoty::MonoTy::Tuple(parts) => !parts.is_empty(),
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
        if self
            .concrete_plan
            .signature
            .result
            .is_some_and(|id| matches!(self.concrete_mono(id), crate::monoty::MonoTy::Unit))
        {
            self.lower_expr(e)?;
            return Ok(vec![]);
        }
        if let Some(shape) = self.result_shape() {
            let is_phantom = match &self.body.expr_arena[e] {
                Expr::Call { callee: Callee::Unresolved { name, .. }, args, .. } => {
                    name.as_str() == "phantom" && args.clone().into_iter().next().is_none()
                }
                _ => false,
            };
            if is_phantom {
                self.lower_expr(e)?;
                return Ok((0..shape.leaf_count())
                    .map(|_| self.emit_cur(Inst::Const(Lit::Nil)))
                    .collect());
            }
            let r = self.lower_expr_repr(e)?;
            if self.scx.rep_arena().value_shape(r.representation()).is_none() {
                return self.opaque_boundary_leaves(&r, &shape, "return boundary");
            }
            return self.repr_to_leaves_bundle_after_presence(&r, &shape);
        }
        if let Some(shape) = self.result_tuple() {
            if shape.nullable {
                return self.nullable_tuple_return_slots(e, &shape);
            }
            if shape.splats() {
                let r = self.lower_expr_repr(e)?.into_value();
                return self.tuple_repr_to_slots(r, &shape);
            }
            let v = self.lower_expr(e)?;
            return Ok(self.tuple_return_operands(v, shape.slots.len()));
        }
        let v = self.lower_expr(e)?;
        let v = match self.concrete_plan.signature.result {
            Some(result) => {
                let result = self.concrete_mono(result);
                self.coerced_mono(v, e, &result)
            }
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
        let r = self.lower_expr_repr(e)?.into_value();
        if let ValueRepr::Scalar(v) = &r {
            if !matches!(self.f.insts[*v], Inst::Const(Lit::Nil)) {
                return Err(gap(
                    "returning an already-packed nullable tuple through the splatting ABI \
                     (return the tuple literal or `null` directly)",
                ));
            }
            return Ok(self.absent_tuple_slots(shape));
        }
        self.tuple_repr_to_slots(r, shape)
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
    ///
    /// This owns the leading presence at a nullable shape, both arms, so it is
    /// the exact inverse of [`Self::tuple_from_slots`] -- one oracle for where
    /// presence sits on the wire. A repr that carries its OWN tag forwards that
    /// tag: `return maybe(b)` hands on the callee's presence, and synthesizing
    /// `true` here instead reported every absent forward as present.
    fn tuple_repr_to_slots(
        &mut self,
        r: ValueRepr,
        shape: &TupleRetShape,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        // A phantom tuple has no aggregate to project; every wire slot is absent.
        if let ValueRepr::Scalar(v) = &r {
            if matches!(self.f.insts[*v], Inst::Const(Lit::Nil)) {
                return Ok(self.absent_tuple_slots(shape));
            }
        }
        let present = match (shape.nullable, &r) {
            (false, _) => None,
            (true, ValueRepr::Tuple { present: Some(p), .. }) => Some(*p),
            (true, _) => Some(self.emit_cur(Inst::Const(Lit::Bool(true)))),
        };
        let parts: Vec<ValueRepr> = match r {
            ValueRepr::Tuple { parts, .. } if parts.len() == shape.slots.len() => parts,
            other => {
                let base = self.pack(other)?;
                (0..shape.slots.len())
                    .map(|index| ValueRepr::Scalar(self.emit_cur(Inst::TupleIndex { base, index })))
                    .collect()
            }
        };
        let mut out = Vec::with_capacity(shape.slot_count());
        out.extend(present);
        for (slot, part) in shape.slots.iter().zip(parts) {
            match slot {
                RetSlot::Opaque => out.push(self.pack(part)?),
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
                    let leaves = self.repr_to_leaves_shape(&part, &nullable)?;
                    out.extend(leaves.into_iter().skip(1));
                }
                RetSlot::Leaves(vs) => out.extend(self.repr_to_leaves_shape(&part, vs)?),
                RetSlot::Nested(inner) => out.extend(self.tuple_repr_to_slots(part, inner)?),
            }
        }
        Ok(out)
    }

    /// Rebuild the tuple repr `shape` describes from a flat slot list -- the
    /// inverse of [`Self::tuple_repr_to_slots`], used at a call site to hand a
    /// destructure its elements without ever boxing a `nobox` one.
    pub(crate) fn tuple_from_slots(
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

    /// The values ONE argument contributes to a flat call-argument list, per its
    /// param slot. Every calling convention routes through this so a caller and
    /// its callee cannot disagree about an argument's arity -- the failure mode
    /// there is silent, since a short list is nil-filled rather than rejected.
    pub(crate) fn arg_values_for_slot(
        &mut self,
        slot: &RetSlot,
        a: Idx<Expr>,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        if !slot.splats() {
            return Ok(vec![self.lower_expr(a)?]);
        }
        let r = self.lower_expr_repr(a)?;
        self.repr_to_slot_values_bundle(&r, slot)
    }

}

// -- Map value-struct key rejection (a fatal emit-time pass; see
// `value_struct_map_key`). These mirror the `Lowerer` type helpers but take
// `(ws, inf)` directly so the check runs without building a full lowering
// context. --

pub(crate) fn ty_node(ws: &Workspace, inf: &BodyInference, ty: TyId) -> Option<TypeExpr> {
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

/// Why `K` cannot be a `Map` key, or `None` when it is admitted.
///
/// An ALLOW-LIST, not a scan for known-bad shapes. Lua keys a table by identity
/// for everything that is not a scalar, so the admitted set is the closed one --
/// int/float/string/bool are value-keyed, and a `newtype` is admitted exactly
/// when what it wraps is. Every other concrete key misses silently: a cdata
/// (`value` struct, cdata-tier payload enum) hashes by pointer, and a table
/// (tuple, array, plain struct, table-tier payload enum) by table identity.
///
/// An UNRESOLVED key is admitted, and that is load-bearing rather than lenient:
/// `fn Map<K, V>.insert` in the stdlib is generic, and rejecting a type
/// parameter would refuse the declaration of every Map method. Only a key that
/// resolves to a concrete non-admitted type is a finding.
fn map_key_rejection_reason(
    ws: &Workspace,
    inf: &BodyInference,
    k: TyId,
    depth: u32,
) -> Option<String> {
    // A newtype chain is finite, but a malformed self-wrapping one must not spin.
    if depth > 8 {
        return None;
    }
    match ty_node(ws, inf, k)? {
        TypeExpr::Builtin(
            BuiltinTy::Int
            | BuiltinTy::Uint
            | BuiltinTy::Float
            | BuiltinTy::Bool
            | BuiltinTy::String
            // Not key shapes, but nothing here can decide against them either.
            | BuiltinTy::Any
            | BuiltinTy::Unit
            | BuiltinTy::Never
            | BuiltinTy::Error,
        ) => None,
        // A type parameter or an uninstantiated call-site var: undecidable here.
        TypeExpr::Generic(..) | TypeExpr::Infer(..) => None,
        TypeExpr::App { ctor: TypeCtor::Nominal(name), .. } => {
            if let Some(nid) = ws.newtypes_by_name.get(&name) {
                let wrapped = ws.modules[nid.module.index()].newtypes[nid.def].wrapped;
                let wrapped = resolve_tyid(ws, inf, wrapped)?;
                return map_key_rejection_reason(ws, inf, wrapped, depth + 1).map(|why| {
                    format!("`{name}` wraps {why}, and a newtype is keyed as what it wraps")
                });
            }
            // A payload-free enum lowers to an int ordinal, so it IS value-keyed.
            // One payload variant is enough to make the type cdata- or
            // table-carrying, and the key that misses is the payload-carrying
            // one -- a `Color::Red` control would hit and hide it.
            if let Some(eid) = ws.enums_by_name.get(&name) {
                let e = &ws.modules[eid.module.index()].enums[eid.def];
                return e
                    .variants
                    .iter()
                    .any(|v| !matches!(v.payload, VariantPayload::Unit))
                    .then(|| {
                        format!(
                            "`{name}` has payload-carrying variants, keyed by cdata or object \
                             identity (a payload-free enum is an int ordinal and stays allowed)"
                        )
                    });
            }
            if map_is_value_struct(ws, name) {
                Some(format!("`{name}` is a `value` struct, keyed by cdata identity"))
            } else {
                Some(format!("`{name}` is a struct, keyed by object identity"))
            }
        }
        TypeExpr::Tuple { .. } => {
            Some("a tuple is an aggregate, keyed by object identity".to_string())
        }
        TypeExpr::App { ctor: TypeCtor::Array, .. } => {
            Some("an array is an aggregate, keyed by object identity".to_string())
        }
        TypeExpr::App { ctor: TypeCtor::Map, .. } => {
            Some("a `Map` is an aggregate, keyed by object identity".to_string())
        }
        TypeExpr::Nullable(..) => {
            Some("a nullable key cannot be distinguished from a missing entry".to_string())
        }
        TypeExpr::Function { .. } => {
            Some("a function is keyed by identity".to_string())
        }
    }
}

fn map_is_value_struct(ws: &Workspace, name: Ustr) -> bool {
    ws.struct_by_name
        .get(&name)
        .map(|sid| ws.modules[sid.module.index()].structs[sid.def].is_value)
        .unwrap_or(false)
}

/// When `ty` resolves to `Map<K, V>` with an inadmissible `K`, returns why.
fn map_key_rejected(ws: &Workspace, inf: &BodyInference, ty: TyId) -> Option<String> {
    let ty = resolve_tyid(ws, inf, ty)?;
    let TypeExpr::App { ctor: TypeCtor::Map, params } = ty_node(ws, inf, ty)? else {
        return None;
    };
    let k = resolve_tyid(ws, inf, ty.with_idx(params.into_iter().next()?))?;
    map_key_rejection_reason(ws, inf, k, 0)
}

/// Scan module `mid`'s function bodies for a `Map<K, V>` whose key `K` is not
/// value-keyed, and return the first offending (message, span).
///
/// The failure this prevents is SILENT: a runtime-built key misses an entry
/// stored under an equal-by-value one, so `m.insert(k, v)` twice at equal `k`
/// grows the map instead of replacing (measured: `Map<(int,int), string>` -> len
/// 2, against a string control at len 1). Nothing at runtime reports it, and
/// `a == b` can be true on the very keys that miss.
pub fn map_key_rejection(ws: &Workspace, mid: ModuleId) -> Option<(String, Span)> {
    let module = ws.module(mid);
    let ir = ws.inference(mid)?;
    for (fid, _f) in module.functions.iter() {
        let owner = BodyOwner::Function(fid);
        let (Some(body), Some(inf)) = (module.bodies.get(&owner), ir.per_fn.get(&owner)) else {
            continue;
        };
        for (e, tyid) in inf.expr_types.iter() {
            if let Some(why) = map_key_rejected(ws, inf, *tyid) {
                return Some((why, body.expr_spans[e]));
            }
        }
    }
    None
}

/// A GC-carrying `value` struct (table repr, no C layout) named at a nominal
/// position. Not a legal C boundary type: no `ffi.cdef` typedef exists, so
/// `ffi.new`/`sizeof`/`ffi.istype`/the extern ABI cannot name it.
fn nominal_is_gc_value(ws: &Workspace, name: Ustr) -> bool {
    map_is_value_struct(ws, name)
        && !matches!(
            crate::lower::declared_value_storage(ws, bone_semantic::hir::FnBackend::Lua, name),
            Some(ValueStorage::CData)
        )
}

/// Reject a GC-carrying value struct used at a genuine C boundary (an `extern`
/// signature) -- a spanned emit-time error, since
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
    None
}

// -- compile-time expansion admission ---------------------------------------
// The lowering expander replaces a valid call; this pass is the FATAL gate that
// rejects source-independent misuse, since a per-fn lowering error would only be
// recorded as a lenient skip. Layout-dependent checks stay in the concrete
// expanders, where the retained instance substitution is available.

/// Resolve a direct expansion call through inference first, falling back to a
/// unique workspace overload when inference did not record one.
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

fn struct_expand_rejection_for_call(
    ws: &Workspace,
    body: &Body,
    call: Idx<Expr>,
    f: &Function,
    args: &[Idx<Expr>],
) -> Option<CompilerError> {
    let fname = f.name;
    let call_span = body.expr_spans[call];
    let reject = |span: Span, msg: String| Some(CompilerError::new_with_label(span, msg));

    let [_arg] = args else {
        return reject(call_span, format!("`@struct_expand` fn `{fname}` takes exactly one argument"));
    };

    if f.return_ty.and_then(|ret| pairs_return_enum(ws, ret)).is_none() {
        return reject(
            call_span,
            format!(
                "`@struct_expand` fn `{fname}` must return `[(string, <enum>)]`, got `{}`",
                f.return_ty.map(|r| ws.render(None, r)).unwrap_or_else(|| "()".to_string())
            ),
        );
    }
    // Field layout belongs to the retained concrete expander. Source preflight
    // keeps only the permanent arity and return-shape gate above.
    None
}

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

#[derive(Clone, Debug)]
pub struct ComputeEntry {
    pub key: String,
    pub entry: FuncRef,
}

pub struct ComputeCollection {
    pub entries: Vec<ComputeEntry>,
    pub callsites: Vec<(ModuleId, BodyOwner)>,
}

/// The `@shader(<stage>)` stage of `f`, else `None`.
/// A local read of the annotation (skull-codegen-glsl has the same helper, but
/// skull-mir must not depend on it -- the layering constraint).
fn shader_stage_of(f: &Function) -> Option<&'static str> {
    let ann = f.annotations.iter().find(|a| a.name.name.as_str() == "shader")?;
    match ann.args.first().map(|a| &a.value) {
        Some(bone_semantic::hir::AnnoValue::Ident(id)) if id.name.as_str() == "vertex" => Some("vertex"),
        Some(bone_semantic::hir::AnnoValue::Ident(id)) if id.name.as_str() == "fragment" => {
            Some("fragment")
        }
        Some(bone_semantic::hir::AnnoValue::Ident(id)) if id.name.as_str() == "compute" => {
            Some("compute")
        }
        _ => None,
    }
}

pub fn resolve_compute_entry(
    ws: &Workspace,
    body: &Body,
    call_expr: Idx<Expr>,
    args: &[Idx<Expr>],
) -> Result<FuncRef, CompilerError> {
    let span = body.expr_spans[call_expr];
    let reject = |message: String| CompilerError::new_with_label(span, message);
    let [arg] = args else {
        return Err(reject(
            "`rock_compute` takes exactly one direct @shader(compute) reference".to_string(),
        ));
    };
    let Expr::Global(name) = body.expr_arena[*arg] else {
        return Err(reject(
            "`rock_compute` argument must be a direct reference to a top-level @shader(compute) fn"
                .to_string(),
        ));
    };
    let fref = match ws.functions_by_name.get(&name).map(|list| list.as_slice()) {
        Some([(module, func)]) => FuncRef { module: *module, func: *func },
        _ => {
            return Err(reject(format!(
                "`rock_compute` argument `{name}` must be a direct reference to a unique top-level fn"
            )))
        }
    };
    let function = &ws.module(fref.module).functions[fref.func];
    match shader_stage_of(function) {
        Some("compute") => Ok(fref),
        Some(stage) => Err(reject(format!(
            "`rock_compute` expects a @shader(compute) fn here, but `{name}` is @shader({stage})"
        ))),
        None => Err(reject(format!(
            "`rock_compute` argument `{name}` is not a @shader(compute) fn"
        ))),
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
        Some(stage) if stage == want_stage => {
            if let Some(annotation) = f
                .param_decls
                .iter()
                .flat_map(|param| param.annotations.iter())
                .find(|annotation| annotation.name.name.as_str() == "storage")
            {
                return Err(CompilerError::new_with_label(
                    annotation.span,
                    format!(
                        "graphics entry `{}` cannot declare @storage resources; storage resources are compute-only",
                        f.name
                    ),
                ));
            }
            Ok(fref)
        }
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

fn shader_expand_rejection_for_call(
    ws: &Workspace,
    body: &Body,
    call: Idx<Expr>,
    args: &[Idx<Expr>],
) -> Option<CompilerError> {
    resolve_shader_pair(ws, body, call, args).err()
}

/// Emit-time validation for source-independent `@uniform_expand` shape errors.
fn uniform_expand_rejection_for_call(
    ws: &Workspace,
    body: &Body,
    call: Idx<Expr>,
    f: &Function,
    args: &[Idx<Expr>],
) -> Option<CompilerError> {
    let fname = f.name;
    let reject = |span: Span, msg: String| Some(CompilerError::new_with_label(span, msg));

    match args {
        [_u] => {
            let returns_int = f
                .return_ty
                .map(|rt| ws.render(None, rt) == "int")
                .unwrap_or(false);
            if !returns_int {
                return reject(body.expr_spans[call], uniform_expand_arity_msg(fname));
            }
        }
        [_, _, _] => {}
        _ => return reject(body.expr_spans[call], uniform_expand_arity_msg(fname)),
    }
    // The uniform layout is derived only by the concrete expander.
    None
}

fn uniform_values_rejection_for_call(
    ws: &Workspace,
    inf: &BodyInference,
    body: &Body,
    call: Idx<Expr>,
    f: &Function,
    args: &[Idx<Expr>],
) -> Option<CompilerError> {
    let fname = f.name;
    let [_u] = args else {
        return Some(CompilerError::new_with_label(
            body.expr_spans[call],
            format!("`@uniform_values` fn `{fname}` takes exactly one uniform group"),
        ));
    };
    let Some(&ret_ty) = inf.expr_types.get(call) else {
        return Some(CompilerError::new_with_label(
            body.expr_spans[call],
            format!("`@uniform_values` fn `{fname}` has no inferred packet result"),
        ));
    };
    let Some(ret_ty) = resolve_tyid(ws, inf, ret_ty) else {
        return Some(CompilerError::new_with_label(
            body.expr_spans[call],
            format!("`@uniform_values` fn `{fname}` has no concrete packet result"),
        ));
    };
    let Some(TypeExpr::App { ctor: TypeCtor::Nominal(block_name), .. }) = static_ty(ws, ret_ty) else {
        return Some(CompilerError::new_with_label(
            body.expr_spans[call],
            format!("`@uniform_values` fn `{fname}` must return the canonical packet struct"),
        ));
    };
    if let Err(msg) = uniform_values_return_shape(ws, block_name) {
        return Some(CompilerError::new_with_label(
            body.expr_spans[call],
            format!("`@uniform_values`: {msg}"),
        ));
    }
    None
}

/// Emit-time validation for `@vertex_expand` calls: misuse is a spanned
/// compile error, never a call that reaches the fn's error body.
fn vertex_expand_rejection_for_call(
    ws: &Workspace,
    body: &Body,
    call: Idx<Expr>,
    f: &Function,
    args: &[Idx<Expr>],
) -> Option<CompilerError> {
    let fname = f.name;
    let call_span = body.expr_spans[call];
    let reject = |span: Span, msg: String| Some(CompilerError::new_with_label(span, msg));

    let [_arg] = args else {
        return reject(
            call_span,
            format!("`@vertex_expand` fn `{fname}` takes exactly one argument"),
        );
    };
    if f.return_ty.and_then(|r| vertex_expand_return(ws, r)).is_none() {
        return reject(call_span, vertex_expand_ret_msg(ws, fname, f.return_ty));
    }
    // The element type and vertex plan belong to the concrete expander.
    None
}

struct ExpansionRejections {
    struct_pairs: Option<CompilerError>,
    shader_pipeline: Option<CompilerError>,
    compute_artifact: Option<CompilerError>,
    vertex_layout: Option<CompilerError>,
    uniform_block: Option<CompilerError>,
    uniform_values: Option<CompilerError>,
}

impl ExpansionRejections {
    fn record(&mut self, kind: ExpandKind, rejection: Option<CompilerError>) {
        let Some(rejection) = rejection else { return };
        let slot = match kind {
            ExpandKind::StructPairs => &mut self.struct_pairs,
            ExpandKind::ShaderPipeline => &mut self.shader_pipeline,
            ExpandKind::ComputeArtifact => &mut self.compute_artifact,
            ExpandKind::VertexLayout => &mut self.vertex_layout,
            ExpandKind::UniformBlock => &mut self.uniform_block,
            ExpandKind::UniformValues => &mut self.uniform_values,
            ExpandKind::Materialize => return,
        };
        if slot.is_none() {
            *slot = Some(rejection);
        }
    }

    fn finish(self) -> Option<CompilerError> {
        self.struct_pairs
            .or(self.shader_pipeline)
            .or(self.compute_artifact)
            .or(self.vertex_layout)
            .or(self.uniform_block)
            .or(self.uniform_values)
    }
}

/// Scan module `mid` once for every direct expansion call. The first rejection
/// is retained per expansion category, then the categories are applied in the
/// historical priority order rather than in lexical order.
pub fn expansion_rejection(ws: &Workspace, mid: ModuleId) -> Option<CompilerError> {
    let module = ws.module(mid);
    let ir = ws.inference(mid)?;
    let mut rejections = ExpansionRejections {
        struct_pairs: None,
        shader_pipeline: None,
        compute_artifact: None,
        vertex_layout: None,
        uniform_block: None,
        uniform_values: None,
    };
    for (owner, body) in module.bodies.iter() {
        let Some(inf) = ir.per_fn.get(owner) else { continue };
        for (e, expr) in body.expr_arena.iter() {
            let Expr::Call { callee: Callee::Unresolved { name, .. }, args, .. } = expr else {
                continue;
            };
            let Some((fmod, fid)) = resolve_call_fn(ws, inf, e, *name) else { continue };
            let f = &ws.module(fmod).functions[fid];
            let Some(kind) = crate::intrinsics::expand_kind(f) else { continue };
            let arg_idxs: Vec<Idx<Expr>> = args.into_iter().collect();
            let rejection = match kind {
                ExpandKind::StructPairs => {
                    struct_expand_rejection_for_call(ws, body, e, f, &arg_idxs)
                }
                ExpandKind::ShaderPipeline => {
                    shader_expand_rejection_for_call(ws, body, e, &arg_idxs)
                }
                ExpandKind::ComputeArtifact => {
                    resolve_compute_entry(ws, body, e, &arg_idxs).err()
                }
                ExpandKind::VertexLayout => {
                    vertex_expand_rejection_for_call(ws, body, e, f, &arg_idxs)
                }
                ExpandKind::UniformBlock => {
                    uniform_expand_rejection_for_call(ws, body, e, f, &arg_idxs)
                }
                ExpandKind::UniformValues => {
                    uniform_values_rejection_for_call(ws, inf, body, e, f, &arg_idxs)
                }
                ExpandKind::Materialize => None,
            };
            rejections.record(kind, rejection);
        }
    }
    rejections.finish()
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

pub fn body_has_compute_artifact_call(
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
            None => match ws.functions_by_name.get(name).map(|list| list.as_slice()) {
                Some([single]) => Some(*single),
                _ => None,
            },
        };
        resolved.is_some_and(|(fmod, fid)| {
            matches!(
                crate::intrinsics::expand_kind(&ws.module(fmod).functions[fid]),
                Some(ExpandKind::ComputeArtifact)
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
                let Expr::Call { callee: Callee::Unresolved { name, .. }, args, .. } = expr else {
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

pub fn collect_compute_collection(ws: &Workspace) -> Result<ComputeCollection, CompilerError> {
    let mut by_key = std::collections::BTreeMap::<String, ComputeEntry>::new();
    let mut callsites = HashSet::new();
    for (_, &mid) in ws.module_ids.iter() {
        let module = ws.module(mid);
        let ir = ws.inference(mid);
        for (owner, body) in module.bodies.iter() {
            let inf = ir.and_then(|ir| ir.per_fn.get(owner));
            for (e, expr) in body.expr_arena.iter() {
                let Expr::Call { callee: Callee::Unresolved { name, .. }, args, .. } = expr else {
                    continue;
                };
                let resolved = match inf {
                    Some(inf) => resolve_call_fn(ws, inf, e, *name),
                    None => match ws.functions_by_name.get(name).map(|list| list.as_slice()) {
                        Some([single]) => Some(*single),
                        _ => None,
                    },
                };
                let Some((fmod, fid)) = resolved else { continue };
                let function = &ws.module(fmod).functions[fid];
                if !matches!(crate::intrinsics::expand_kind(function), Some(ExpandKind::ComputeArtifact)) {
                    continue;
                }
                callsites.insert((mid, *owner));
                let arg_idxs: Vec<Idx<Expr>> = args.into_iter().collect();
                let entry = resolve_compute_entry(ws, body, e, &arg_idxs)?;
                let key = ws.module(entry.module).functions[entry.func].name.to_string();
                by_key.entry(key.clone()).or_insert(ComputeEntry { key, entry });
            }
        }
    }
    Ok(ComputeCollection { entries: by_key.into_values().collect(), callsites: callsites.into_iter().collect() })
}

/// Reject CPU-side calls to `@gpu_only` intrinsics and references to GPU-side
/// functions as runtime fn values. Shader-pipeline expansion arguments are names,
/// not runtime values, and remain legal.
pub fn gpu_only_call_rejection(ws: &Workspace, mid: ModuleId) -> Option<CompilerError> {
    let module = ws.module(mid);
    let ir = ws.inference(mid)?;
    for (_, function) in module.functions.iter() {
        if function
            .param_decls
            .iter()
            .any(|param| is_workgroup_array_type(ws, param.ty))
        {
            return Some(CompilerError::new_with_label(
                function.name_span,
                "WorkgroupArray is workgroup memory and cannot cross a function boundary".to_string(),
            ));
        }
        if function.return_ty.is_some_and(|ty| is_workgroup_array_type(ws, ty)) {
            return Some(CompilerError::new_with_label(
                function.name_span,
                "WorkgroupArray is workgroup memory and cannot be returned".to_string(),
            ));
        }
    }
    for (_, global) in module.globals.iter() {
        if is_workgroup_array_type(ws, global.ty) {
            return Some(CompilerError::new_with_label(
                global.span,
                "WorkgroupArray is workgroup memory and cannot be stored globally".to_string(),
            ));
        }
    }
    for (owner, body) in module.bodies.iter() {
        let Some(inf) = ir.per_fn.get(owner) else { continue };
        let caller_is_gpu = match owner {
            BodyOwner::Function(fid) => {
                let function = &module.functions[*fid];
                crate::intrinsics::is_gpu_context(function) || function_has_gpu_resource_param(ws, function)
            }
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
                ..
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
                Some(ExpandKind::ShaderPipeline | ExpandKind::ComputeArtifact)
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
                    if !crate::intrinsics::is_gpu_only(f) && !function_has_gpu_resource_param(ws, f) {
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
                    if !crate::intrinsics::is_gpu_context(f) && !function_has_gpu_resource_param(ws, f) {
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

fn function_has_gpu_resource_param(ws: &Workspace, function: &Function) -> bool {
    function.param_decls.iter().any(|param| {
        let TyId::Static(module, index) = param.ty else { return false };
        let TypeExpr::App { ctor: TypeCtor::Nominal(name), .. } = ws.module(module).ty_arena[index] else {
            return false;
        };
        matches!(name.as_str(), "StorageBuffer" | "AtomicStorageBuffer" | "WorkgroupArray")
    })
}

fn is_workgroup_array_type(ws: &Workspace, ty: TyId) -> bool {
    let TyId::Static(module, index) = ty else { return false };
    let TypeExpr::App { ctor: TypeCtor::Nominal(name), .. } = ws.module(module).ty_arena[index] else {
        return false;
    };
    name.as_str() == "WorkgroupArray"
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
            let Expr::Call { callee: Callee::Unresolved { name, .. }, args, .. } = *expr else {
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

/// Support for `crate::spec_census`: expose concrete generic call identities
/// without giving the diagnostic census its own type conversion.
pub(crate) mod shape_census_support {
    use super::shape::{callee_instance_subst, spec_key_from_subst, SpecSubst};
    use crate::monoty::MonoTy;
    use crate::mir::FuncRef;
    use bone_semantic::inference::{BodyInference, Workspace};

    /// Every complete generic call identity in this body, plus the count of
    /// compiler-state sites that could not state one.
    pub(crate) fn concrete_calls_in_body(
        ws: &Workspace,
        inf: &BodyInference,
    ) -> (Vec<(FuncRef, Vec<MonoTy>)>, usize) {
        // caller is supplied by the census; this returns callee-side data only.
        let empty = SpecSubst::default();
        let mut out = Vec::new();
        let mut rejected = 0usize;
        let mut calls: Vec<_> = inf.resolved_overload_calls.iter().collect();
        calls.sort_by_key(|(e, _)| **e);
        for (e, (m, f)) in calls {
            let func = FuncRef { module: *m, func: *f };
            match callee_instance_subst(ws, inf, *e, func, &empty) {
                Ok(Some(subst)) => match spec_key_from_subst(ws, func, &subst) {
                    Some(key) => out.push((func, key)),
                    None => rejected += 1,
                },
                Ok(None) => {}
                Err(_) => rejected += 1,
            }
        }
        (out, rejected)
    }
}
