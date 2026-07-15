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

/// The static, recursive shape of a scalarizable `value` struct: declared fields,
/// each a primitive leaf or a nested (also-scalarizable) value struct.
#[derive(Clone, Debug)]
pub struct ValueShape {
    name: Ustr,
    fields: Vec<(Ustr, FieldShape)>,
}

#[derive(Clone, Debug)]
enum FieldShape {
    Prim,
    Nested(ValueShape),
}

impl ValueShape {
    /// Total primitive-leaf count — the scalar-ABI arity (params/returns/args).
    fn leaf_count(&self) -> usize {
        self.fields
            .iter()
            .map(|(_, fs)| match fs {
                FieldShape::Prim => 1,
                FieldShape::Nested(s) => s.leaf_count(),
            })
            .sum()
    }
}

/// `Some(ordered variant names)` iff `name` is a PAYLOAD-FREE enum (every variant
/// `Unit`) — the tier-1 scalarizable case. The index is the variant's int tag.
fn payload_free_enum_variants(ws: &Workspace, name: Ustr) -> Option<Vec<Ustr>> {
    let eid = *ws.enums_by_name.get(&name)?;
    let ed = &ws.modules[eid.module.index()].enums[eid.def];
    let mut out = Vec::with_capacity(ed.variants.len());
    for v in &ed.variants {
        if !matches!(v.payload, VariantPayload::Unit) {
            return None;
        }
        out.push(v.name);
    }
    Some(out)
}

/// `Some(ordinal)` iff `name` is a payload-free enum and `variant` is one of its
/// variants — the variant's declaration-order index, which IS its runtime `int`
/// value. `None` for a mixed/payload enum (those keep the table representation).
fn payload_free_ordinal(ws: &Workspace, name: Ustr, variant: Ustr) -> Option<i64> {
    payload_free_enum_variants(ws, name)?
        .iter()
        .position(|v| *v == variant)
        .map(|p| p as i64)
}

/// The runtime carrier threaded through lowering: a value struct lives as an
/// unboxed `Group` of per-field reprs (recursive); everything else is a `Scalar`
/// (one already-lowered value — a primitive, or a boxed cdata).
#[derive(Clone)]
enum ValueRepr {
    Scalar(Idx<Inst>),
    Group { name: Ustr, parts: Vec<ValueRepr> },
}

/// The `TypeExpr` of a `Static` TyId (declared types — struct fields, signature
/// params/returns are all static). `None` for a `Local`/inference TyId.
fn static_ty(ws: &Workspace, ty: TyId) -> Option<TypeExpr> {
    match ty {
        TyId::Static(m, idx) => Some(ws.module(m).ty_arena[idx]),
        TyId::Local(_) => None,
    }
}

/// `Some(shape)` iff `name` is a `value` struct all of whose fields are primitive
/// (int/float/bool) or nested scalarizable value structs — the rock eligibility
/// rule, matching `c_field_type` (string/array/nullable/generic fields disqualify,
/// so a real cdata always exists as the escape representation). Cyclic field
/// types disqualify.
fn value_shape(ws: &Workspace, name: Ustr) -> Option<ValueShape> {
    value_shape_rec(ws, name, &mut Vec::new())
        .or_else(|| enum_value_shape(ws, name).map(|e| e.value_shape()))
}

fn value_shape_rec(ws: &Workspace, name: Ustr, visiting: &mut Vec<Ustr>) -> Option<ValueShape> {
    let sid = *ws.struct_by_name.get(&name)?;
    let sd = &ws.modules[sid.module.index()].structs[sid.def];
    if !sd.is_value || visiting.contains(&name) {
        return None;
    }
    visiting.push(name);
    let mut fields = Vec::with_capacity(sd.fields.len());
    for f in &sd.fields {
        let fs = match static_ty(ws, f.ty)? {
            TypeExpr::Builtin(BuiltinTy::Int | BuiltinTy::Float | BuiltinTy::Bool) => FieldShape::Prim,
            TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } => {
                // A payload-free enum is an `int` value GLOBALLY (the variant's
                // declaration-order tag), so as a field it is just a `Prim` int --
                // no conversion. A cdata payload enum nests its `[__tag, _0]` leaves
                // (flattened into the host cdef). Else a nested value struct; else
                // disqualified.
                if payload_free_enum_variants(ws, n).is_some() {
                    FieldShape::Prim
                } else if let Some(es) = enum_value_shape(ws, n) {
                    FieldShape::Nested(es.value_shape())
                } else {
                    FieldShape::Nested(value_shape_rec(ws, n, visiting)?)
                }
            }
            _ => {
                visiting.pop();
                return None;
            }
        };
        fields.push((f.name, fs));
    }
    visiting.pop();
    Some(ValueShape { name, fields })
}

/// Declared field names of struct `name`, in declaration order (the positional
/// `<name>_ct` ctor order). A cdata-eligible enum is `[__tag, _0]` (it rides the
/// value-struct `<Enum>_ct` path). Empty if `name` is neither.
fn struct_field_names(ws: &Workspace, name: Ustr) -> Vec<Ustr> {
    let Some(sid) = ws.struct_by_name.get(&name) else {
        if let Some(es) = enum_value_shape(ws, name) {
            return es.field_names();
        }
        return Vec::new();
    };
    ws.modules[sid.module.index()].structs[sid.def]
        .fields
        .iter()
        .map(|f| f.name)
        .collect()
}

// --- `@struct_expand` intrinsic (docs/pass-uniforms-design.md, Layer 2) --------
//
// A call to a fn declared `@struct_expand fn f<T>(u: T) -> [(string, E)]` is
// replaced at lowering by the literal pairs list: one `(field_name, E::V(u.field))`
// per struct field, declaration order, the variant chosen STRUCTURALLY (the unique
// variant of `E` whose single payload type equals the field type). skull never
// hardcodes the engine's `UniformValue`.

/// The enum name `E` in a return type shaped exactly `[(string, E)]` (E nominal
/// and a real enum). `None` for any other shape.
fn pairs_return_enum(ws: &Workspace, ret: TyId) -> Option<Ustr> {
    let TypeExpr::App { ctor: TypeCtor::Array, params } = static_ty(ws, ret)? else {
        return None;
    };
    let elem = ret.with_idx(params.into_iter().next()?);
    let TypeExpr::Tuple { elements } = static_ty(ws, elem)? else {
        return None;
    };
    let mut it = elements.into_iter();
    let (a, b) = (it.next()?, it.next()?);
    if it.next().is_some() {
        return None;
    }
    if !matches!(static_ty(ws, elem.with_idx(a))?, TypeExpr::Builtin(BuiltinTy::String)) {
        return None;
    }
    let TypeExpr::App { ctor: TypeCtor::Nominal(name), .. } = static_ty(ws, elem.with_idx(b))? else {
        return None;
    };
    ws.enums_by_name.get(&name)?;
    Some(name)
}

/// Value struct `name`'s fields as `(name, ty)` in declaration order. `None` when
/// `name` is not a `value` struct.
fn value_struct_field_tys(ws: &Workspace, name: Ustr) -> Option<Vec<(Ustr, TyId)>> {
    let sid = *ws.struct_by_name.get(&name)?;
    let sd = &ws.modules[sid.module.index()].structs[sid.def];
    if !sd.is_value {
        return None;
    }
    Some(sd.fields.iter().map(|f| (f.name, f.ty)).collect())
}

/// Variant names of `enum_name` whose payload is a single type rendering to
/// `want`. Zero -> no match; more than one -> ambiguous.
fn variants_for_payload(ws: &Workspace, enum_name: Ustr, want: &str) -> Vec<Ustr> {
    let Some(&eid) = ws.enums_by_name.get(&enum_name) else {
        return Vec::new();
    };
    let ed = &ws.modules[eid.module.index()].enums[eid.def];
    ed.variants
        .iter()
        .filter(|v| match &v.payload {
            VariantPayload::Tuple(tys) if tys.len() == 1 => ws.render(None, tys[0]) == want,
            _ => false,
        })
        .map(|v| v.name)
        .collect()
}

/// Map each `(field, ty)` to the unique variant of `enum_name` carrying that
/// payload type (declaration order preserved), or the first offending field's
/// rejection message. The single source of the field->variant rule -- both the
/// emit-time validator ([`struct_expand_rejection`]) and the lowering expander
/// ([`Builder::expand_struct_pairs`]) resolve through it, so they never diverge.
fn struct_expand_variants(
    ws: &Workspace,
    enum_name: Ustr,
    field_tys: &[(Ustr, TyId)],
) -> Result<Vec<Ustr>, String> {
    let mut out = Vec::with_capacity(field_tys.len());
    for (field, fty) in field_tys {
        let want = ws.render(None, *fty);
        match variants_for_payload(ws, enum_name, &want).as_slice() {
            [v] => out.push(*v),
            [] => {
                return Err(format!(
                    "field `{field}: {want}` has no variant of `{enum_name}` carrying that payload type"
                ))
            }
            _ => {
                return Err(format!(
                    "field `{field}: {want}` matches more than one variant of `{enum_name}` (ambiguous payload type)"
                ))
            }
        }
    }
    Ok(out)
}

/// The scalarizable shape of a signature type (a param/return TyId): `Some` iff
/// it is a NON-nullable nominal value struct OR a flat-eligible (boxed) enum, or a
/// NULLABLE flat enum -- all thread as leaf scalars. A nullable flat enum threads
/// with `tag = nil` as the null sentinel (a present variant always has a non-nil
/// string tag, so no extra null bit is needed). Nullable VALUE structs still keep
/// the single-cdata ABI (cdata has no spare field for a nil discriminator).
fn sig_value_shape(ws: &Workspace, ty: TyId) -> Option<ValueShape> {
    match static_ty(ws, ty)? {
        TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } => {
            value_shape(ws, n).or_else(|| flat_enum_shape(ws, n).map(|s| s.value_shape()))
        }
        TypeExpr::Nullable(inner) => match static_ty(ws, ty.with_idx(inner))? {
            TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } => {
                flat_enum_shape(ws, n).map(|s| s.value_shape())
            }
            _ => None,
        },
        _ => None,
    }
}

/// Arity of a tuple RETURN type -- `Some(n)` iff `ty` is a (non-nullable) tuple
/// `(A, B, ...)`. Drives the multi-return ABI: a tuple-returning fn returns its N
/// elements as N Lua values instead of one heap table (slice 1,
/// `docs/iterator-implementation-plan.md`). A `Nullable(Tuple)` return stays boxed
/// (no room for the null discriminant across a multi-value boundary).
fn tuple_arity(ws: &Workspace, ty: TyId) -> Option<usize> {
    match static_ty(ws, ty)? {
        TypeExpr::Tuple { elements } => Some(elements.into_iter().count()),
        _ => None,
    }
}

/// Cdata enum shape: an enum whose every variant is `Unit` or a positional TUPLE
/// of scalars (int/float/bool) becomes a `value`-struct-style cdata
/// `{ int32 __tag; <T0> _0; <T1> _1; ... }` -- reusing the whole value-struct
/// machinery (C-layout, byte-hashable, value `==`, scalar ABI, SROA, value-struct
/// field). Slots are SHARED positionally across variants (lean): slot `i` exists
/// iff some variant has an `i`th payload field, and its type must be IDENTICAL
/// across every variant that has it (a per-position type conflict -> not eligible,
/// table+SROA). A variant with fewer fields pads the unused slots. Reflection is
/// sacrificed (no `{__variant}` table); see docs/skull-enum-increment2-plan.md.
/// Record / ref payloads are NOT yet eligible (cut 2b+). Payload-free enums are
/// plain ints (increment 1), not this.
pub struct EnumCdataShape {
    pub name: Ustr,
    /// Ordered C slots `(slot_name, c_type)` -- `_<i>` for a tuple field at
    /// position `i`, `_<field>` for a record field. Shared across variants; a slot
    /// name's type must be identical everywhere it occurs.
    pub slots: Vec<(Ustr, &'static str)>,
    /// (variant name, payload arity); the tag is the variant's declaration index.
    pub variants: Vec<(Ustr, usize)>,
}

impl EnumCdataShape {
    /// The variant's runtime int tag (its declaration-order index).
    pub fn tag_of(&self, v: Ustr) -> Option<i64> {
        self.variants.iter().position(|(n, _)| *n == v).map(|p| p as i64)
    }

    /// All field names in `<Enum>_ct` ctor order: `[__tag, <slot names>]`.
    pub fn field_names(&self) -> Vec<Ustr> {
        let mut v = Vec::with_capacity(1 + self.slots.len());
        v.push(Ustr::from("__tag"));
        v.extend(self.slots.iter().map(|(n, _)| *n));
        v
    }

    /// The `ValueShape` the scalar ABI / SROA / pack see: `[__tag, <slots>]`, all
    /// primitive leaves (the cdata's C fields).
    fn value_shape(&self) -> ValueShape {
        ValueShape {
            name: self.name,
            fields: self.field_names().into_iter().map(|n| (n, FieldShape::Prim)).collect(),
        }
    }
}

/// The slot name for a payload field: `_<i>` for a tuple position, `_<field>` for
/// a record field. (Record field names are identifiers, so they never collide
/// with the numeric tuple form.)
fn slot_name(suffix: &str) -> Ustr {
    Ustr::from(format!("_{suffix}").as_str())
}

fn scalar_c_type(ws: &Workspace, ty: TyId) -> Option<&'static str> {
    match static_ty(ws, ty)? {
        TypeExpr::Builtin(BuiltinTy::Int) => Some("int"),
        TypeExpr::Builtin(BuiltinTy::Float) => Some("float"),
        TypeExpr::Builtin(BuiltinTy::Bool) => Some("bool"),
        _ => None,
    }
}

/// `Some(shape)` iff `name` is a cdata-eligible enum (see [`EnumCdataShape`]).
pub fn enum_value_shape(ws: &Workspace, name: Ustr) -> Option<EnumCdataShape> {
    let eid = *ws.enums_by_name.get(&name)?;
    let ed = &ws.modules[eid.module.index()].enums[eid.def];
    let mut slots: Vec<(Ustr, &'static str)> = Vec::new();
    let mut variants: Vec<(Ustr, usize)> = Vec::with_capacity(ed.variants.len());
    let mut any_payload = false;
    // Register a slot (name -> type); reject a type conflict on an existing name.
    let mut add_slot = |nm: Ustr, c: &'static str| -> Option<()> {
        match slots.iter().find(|(n, _)| *n == nm) {
            Some((_, existing)) if *existing != c => None, // slot type conflict
            Some(_) => Some(()),
            None => {
                slots.push((nm, c));
                Some(())
            }
        }
    };
    for v in &ed.variants {
        let arity = match &v.payload {
            VariantPayload::Unit => 0,
            VariantPayload::Tuple(tys) => {
                for (i, ty) in tys.iter().enumerate() {
                    add_slot(slot_name(&i.to_string()), scalar_c_type(ws, *ty)?)?;
                }
                any_payload |= !tys.is_empty();
                tys.len()
            }
            VariantPayload::Record(fields) => {
                for f in fields {
                    add_slot(slot_name(f.name.as_str()), scalar_c_type(ws, f.ty)?)?;
                }
                any_payload |= !fields.is_empty();
                fields.len()
            }
        };
        variants.push((v.name, arity));
    }
    if !any_payload {
        return None; // payload-free -> int (increment 1)
    }
    Some(EnumCdataShape { name, slots, variants })
}

/// The flatten shape of an enum that boxes today and is eligible to be inlined
/// into a parent struct's Lua table (`docs/enum-allocation-report.md`). Eligible iff
/// the enum has payload variants (not payload-free -> those are plain ints), is NOT
/// cdata-eligible (those scalarize via `EnumCdataShape`), and every payload variant
/// is `Unit` or `Tuple` (record payloads keep boxing for now). Closure / string /
/// ref payloads are fine -- the slots are dynamically-typed Lua fields. The
/// discriminant is the variant NAME (a string, mirroring the boxed `__variant`), so
/// flattened slots are a direct spread of the box's keys; box <-> slot conversion is
/// a field copy with no name<->ordinal table.
#[derive(Clone, Debug)]
pub struct EnumFlatShape {
    name: Ustr,
    /// Positional payload slots `f__0 .. f__{tuple_width-1}` (max tuple arity).
    tuple_width: usize,
    /// Named payload slots `f__<name>` (union of record-variant field names,
    /// first-seen order). A tuple position and a record field never collide --
    /// the former is a number, the latter an identifier.
    record_fields: Vec<Ustr>,
}

impl EnumFlatShape {
    /// The number of scalar leaves this enum threads as across a call: the
    /// discriminant + every positional slot + every record slot.
    fn leaf_count(&self) -> usize {
        1 + self.tuple_width + self.record_fields.len()
    }

    /// The `ValueShape` the scalar ABI / SROA see: `[__tag, _0.., <rec>..]`, all
    /// primitive leaves. `pack`/`cdata_leaves` re-detect the enum by name and use
    /// box semantics (the leaf NAMES here are nominal -- never read structurally).
    fn value_shape(&self) -> ValueShape {
        let mut fields = vec![(Ustr::from("__tag"), FieldShape::Prim)];
        for i in 0..self.tuple_width {
            fields.push((Ustr::from(format!("_{i}").as_str()), FieldShape::Prim));
        }
        for rf in &self.record_fields {
            fields.push((*rf, FieldShape::Prim));
        }
        ValueShape { name: self.name, fields }
    }
}

/// `Some(shape)` iff `name` is a mixed/boxed enum eligible for field flattening.
fn flat_enum_shape(ws: &Workspace, name: Ustr) -> Option<EnumFlatShape> {
    let eid = *ws.enums_by_name.get(&name)?;
    // Payload-free enums are ints; cdata-eligible enums scalarize via cdata.
    if payload_free_enum_variants(ws, name).is_some() || enum_value_shape(ws, name).is_some() {
        return None;
    }
    let ed = &ws.modules[eid.module.index()].enums[eid.def];
    let mut tuple_width = 0;
    let mut record_fields: Vec<Ustr> = Vec::new();
    for v in &ed.variants {
        match &v.payload {
            VariantPayload::Unit => {}
            VariantPayload::Tuple(tys) => tuple_width = tuple_width.max(tys.len()),
            VariantPayload::Record(fields) => {
                for f in fields {
                    if !record_fields.contains(&f.name) {
                        record_fields.push(f.name);
                    }
                }
            }
        }
    }
    Some(EnumFlatShape { name, tuple_width, record_fields })
}

/// The frame-slot yield channel of a `co fn` whose declared return type is
/// `Coroutine<Y, ...>` and `Y` is a flat (boxed) enum. `Some` -> the coroutine
/// yields a flat enum: the transform writes its leaves into reused `s.__y*` frame
/// slots (0 alloc) and the concrete consumer reads them threaded, while the
/// generic/boxed `resume()` rebuilds a `{__variant}` box from those slots (the
/// info here is exactly what that rebuild needs). `None` -> `Y` is a scalar /
/// non-flat type: `resume()` returns the single value, no frame-slot channel.
pub struct YieldChannel {
    pub enum_name: Ustr,
    pub tuple_width: usize,
    pub record_fields: Vec<Ustr>,
}

impl YieldChannel {
    /// Total scalar leaves: discriminant + positional slots + record slots. These
    /// are the `s.__y0 .. s.__y{leaf_count-1}` frame slots.
    pub fn leaf_count(&self) -> usize {
        1 + self.tuple_width + self.record_fields.len()
    }
}

/// The [`YieldChannel`] of a co fn's declared return type `Coroutine<Y, ...>`.
pub fn coroutine_yield_channel(ws: &Workspace, return_ty: TyId) -> Option<YieldChannel> {
    let TyId::Static(m, idx) = return_ty else { return None };
    let TypeExpr::App { params, .. } = ws.module(m).ty_arena[idx] else { return None };
    let y_idx = params.into_iter().next()?;
    let TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } = ws.module(m).ty_arena[y_idx] else {
        return None;
    };
    let shape = flat_enum_shape(ws, n)?;
    Some(YieldChannel {
        enum_name: shape.name,
        tuple_width: shape.tuple_width,
        record_fields: shape.record_fields,
    })
}

/// The reused frame slot name for the `i`th yield leaf: `__y{i}` (tag at 0, then
/// positional, then record leaves -- `value_shape` leaf order).
pub fn yield_slot(i: usize) -> Ustr {
    Ustr::from(format!("__y{i}").as_str())
}

/// The parent-table slot holding a flattened enum field's discriminant (variant
/// name string), e.g. field `sig` -> `sig__tag`.
fn flat_tag_slot(field: Ustr) -> Ustr {
    Ustr::from(format!("{field}__tag").as_str())
}

/// The parent-table slot holding a flattened enum field's `i`th positional payload,
/// e.g. field `sig` payload 0 -> `sig__0`.
fn flat_payload_slot(field: Ustr, i: usize) -> Ustr {
    Ustr::from(format!("{field}__{i}").as_str())
}

/// The parent-table slot holding a flattened enum field's named (record) payload,
/// e.g. field `shape` record field `r` -> `shape__r`.
fn flat_named_slot(field: Ustr, rec_field: Ustr) -> Ustr {
    Ustr::from(format!("{field}__{rec_field}").as_str())
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

/// Workspace-wide scalar-ABI context: the set of function names referenced as a
/// VALUE (a fn pointer) anywhere. Such a fn is called indirectly (`CallValue`,
/// which can't splat), so it MUST keep the single-cdata ABI on its definition and
/// at every direct call site too — otherwise the two ABIs drift. Computed once.
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
    /// (`docs/glsl-spike.md` finding 5). The toggle is contained because
    /// [`fn_uses_scalar_abi`] is the single gate the definition and every call site
    /// both consult, so flipping it here can never drift the two ABIs apart.
    values_first_class: bool,
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
        Self { taken_by_ref, per_body, ref_counts, values_first_class: false }
    }

    /// Like [`new`](Self::new) but with the scalarized value ABI disabled: value
    /// structs stay first-class single values everywhere (the GLSL backend's
    /// requirement -- a `vecN` is not scalar leaves). See [`values_first_class`].
    pub fn new_values_first_class(ws: &Workspace) -> Self {
        let mut scx = Self::new(ws);
        scx.values_first_class = true;
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

/// Whether `fref` participates in the scalar value ABI on BOTH its definition and
/// every call site (so the two never drift). True iff it has a scalarizable value
/// param/return AND either (a) it carries an explicit `@scalar_abi` opt-in (the
/// author committed to per-field `<base>__<field>` params — used by hot
/// `inline(lua)` engine fns like `append_vertex`), or (b) it's an ordinary rock
/// body that is NOT called by a fixed name from outside skull's view
/// (`@nomangle` / `@test` / `main`) and NOT taken by reference (an indirect callee
/// can't splat). A non-scalar callee receives/returns a single cdata.
fn fn_uses_scalar_abi(ws: &Workspace, scx: &ScalarCtx, fref: FuncRef) -> bool {
    if scx.values_first_class {
        return false;
    }
    let f = &ws.module(fref.module).functions[fref.func];
    if f.is_extern {
        return false;
    }
    let has_scalar_abi = f.annotations.iter().any(|a| a.name.name.as_str() == "scalar_abi");
    if !has_scalar_abi {
        // Implicit scalarization is limited to ordinary rock-bodied fns reachable
        // only by direct call. `@scalar_abi` overrides all of these gates.
        if f.lua_body.is_some()
            || f.name.as_str() == "main"
            || scx.taken_by_ref.contains(&f.name)
            || f.annotations.iter().any(|a| matches!(a.name.name.as_str(), "nomangle" | "test"))
        {
            return false;
        }
    }
    f.param_decls.iter().any(|pd| sig_value_shape(ws, pd.ty).is_some())
        || f.return_ty.is_some_and(|t| sig_value_shape(ws, t).is_some())
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
    if !fn_uses_scalar_abi(ws, scx, fref) {
        return None;
    }
    let f = &ws.module(fref.module).functions[fref.func];
    let mut names = Vec::new();
    for pd in &f.param_decls {
        match sig_value_shape(ws, pd.ty) {
            Some(shape) => leaf_param_names(pd.name.as_str(), &shape, &mut names),
            None => names.push(pd.name.as_str().to_string()),
        }
    }
    Some(names)
}

/// Append `<base>__<leaf>` for every primitive leaf of `shape`, depth-first
/// (same traversal as `repr_to_leaves`/`cdata_leaves`).
fn leaf_param_names(base: &str, shape: &ValueShape, out: &mut Vec<String>) {
    for (fname, fs) in &shape.fields {
        match fs {
            FieldShape::Prim => out.push(format!("{base}__{fname}")),
            FieldShape::Nested(sub) => leaf_param_names(base, sub, out),
        }
    }
}

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
        Expr::Tuple(es) | Expr::Array(es) | Expr::NativeArray(es) => {
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
    let module = ws.module(mid);
    let func = &module.functions[fid];
    if func.lua_body.is_some() {
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
    // leaf cells and a value return becomes a multi-value `Return`.
    let self_ref = FuncRef { module: mid, func: fid };
    let (param_shapes, ret_shape) = if fn_uses_scalar_abi(ws, scx, self_ref) {
        let ps = func.param_decls.iter().map(|pd| sig_value_shape(ws, pd.ty)).collect();
        let rs = func.return_ty.and_then(|t| sig_value_shape(ws, t));
        (ps, rs)
    } else {
        (Vec::new(), None)
    };
    let mut b = Builder {
        ws,
        scx,
        body,
        inf,
        f,
        cur,
        binds: HashMap::new(),
        loop_targets: Vec::new(),
        defers: Vec::new(),
        in_defer_body: false,
        resume_arg,
        param_shapes,
        ret_shape,
        ret_tuple: func.return_ty.and_then(|t| tuple_arity(ws, t)),
        ret_ty: func.return_ty,
        group_binds: HashMap::new(),
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
    };
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
    let mut b = Builder {
        ws,
        scx,
        body,
        inf,
        f,
        cur,
        binds: HashMap::new(),
        loop_targets: Vec::new(),
        defers: Vec::new(),
        in_defer_body: false,
        resume_arg: None,
        param_shapes: Vec::new(),
        ret_shape: None,
        ret_tuple: None,
        ret_ty: None,
        group_binds: HashMap::new(),
        static_cells: HashMap::new(),
        static_owner: name,
        static_counters: HashMap::new(),
        nested_fn_globals: HashMap::new(),
        nested_fn_counters: HashMap::new(),
    };
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
    let mut b = Builder {
        ws,
        scx,
        body,
        inf,
        f,
        cur,
        binds: HashMap::new(),
        loop_targets: Vec::new(),
        defers: Vec::new(),
        in_defer_body: false,
        resume_arg: None,
        param_shapes: Vec::new(),
        ret_shape: None,
        ret_tuple: None,
        ret_ty: None,
        group_binds: HashMap::new(),
        static_cells: HashMap::new(),
        static_owner: name,
        static_counters: HashMap::new(),
        nested_fn_globals: HashMap::new(),
        nested_fn_counters: HashMap::new(),
    };
    // `target` is param 0, then the decorator's declared params.
    let tcell = b.f.new_cell(Some(b.body.binding_arena[target].name));
    b.f.params.push(tcell);
    b.binds.insert(raw(target), tcell);
    b.lower_params();
    b.fill_param_defaults()?;
    b.lower_root()?;
    Ok(b.f)
}

struct Builder<'a> {
    ws: &'a Workspace,
    scx: &'a ScalarCtx,
    body: &'a Body,
    inf: Option<&'a BodyInference>,
    f: MirFunc,
    cur: Idx<Block>,
    /// raw binding index -> the variable cell it maps to.
    binds: HashMap<u32, Idx<Cell>>,
    /// Innermost-last stack of `(continue_target, break_target)` blocks.
    loop_targets: Vec<(Idx<Block>, Idx<Block>)>,
    /// Innermost-last stack of pending `defer` bodies per block scope; spliced
    /// (LIFO) at block exit and before every `return`. In a co fn each entry
    /// also carries its terminal-cleanup flag cell (see `MirFunc::coro_defers`);
    /// the splice clears it so kill/error never double-run a completed defer.
    defers: Vec<Vec<(Idx<Expr>, Option<Idx<Cell>>)>>,
    /// True while lowering a `defer` body (inline splice or terminal copy) --
    /// `yield`/`yield*` inside a defer body is a compile error.
    in_defer_body: bool,
    /// In a `co fn`, the cell that `yield e` evaluates to (the value passed to
    /// the next `resume(arg)`). `None` outside a coroutine.
    resume_arg: Option<Idx<Cell>>,
    /// Per-param scalarized shape: `Some` for a value param expanded to leaf
    /// cells (parallel to `body.param_bindings`). Empty when the fn doesn't use
    /// the scalar ABI (globals, decorators, non-value fns).
    param_shapes: Vec<Option<ValueShape>>,
    /// `Some` when the fn returns a value struct via the scalar ABI (N leaves).
    ret_shape: Option<ValueShape>,
    /// `Some(n)` when the fn's return type is a tuple of arity `n` -- it returns
    /// N Lua values (multi-return ABI), not one heap table. Set for named fns;
    /// lambdas decide per-body at their own return site.
    ret_tuple: Option<usize>,
    /// The fn's declared return type -- the expected type at a scalar `return`
    /// sink (int->float subsumption coercion, see [`Builder::coerced`]).
    ret_ty: Option<TyId>,
    /// A value PARAM binding -> (struct name, its leaf cells, shape). A read of
    /// the param reconstructs the unboxed `Group` from these; a field read
    /// projects a leaf directly (no cdata).
    group_binds: HashMap<u32, (Vec<Idx<Cell>>, ValueShape)>,
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
    /// call / value-ref of the binding resolves to. Inherited (cloned) by a
    /// nested fn's own builder so it can reference itself + prior siblings.
    nested_fn_globals: HashMap<u32, Ustr>,
    /// Per nested-fn source-name counter (source order), the `static_counters`
    /// analogue for shadowing duplicates.
    nested_fn_counters: HashMap<Ustr, u32>,
}

impl Builder<'_> {
    fn emit_cur(&mut self, inst: Inst) -> Idx<Inst> {
        self.f.emit(self.cur, inst)
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

    /// The return-value list for `opt`: under the scalar value ABI a value return
    /// flattens to its leaf scalars (`return v0, v1, ...`); otherwise it's the
    /// single (boxed) value, or empty for a bare return.
    fn return_leaves(&mut self, opt: Option<Idx<Expr>>) -> Result<Vec<Idx<Inst>>, CompilerError> {
        let Some(e) = opt else { return Ok(vec![]) };
        if let Some(shape) = self.ret_shape.clone() {
            let r = self.lower_expr_repr(e)?;
            return Ok(self.repr_to_leaves(&r, &shape));
        }
        if let Some(n) = self.ret_tuple {
            let v = self.lower_expr(e)?;
            return Ok(self.tuple_return_operands(v, n));
        }
        let v = self.lower_expr(e)?;
        let v = match self.ret_ty {
            Some(rt) => self.coerced(v, e, rt),
            None => v,
        };
        Ok(vec![v])
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

    /// Hoist a nested `fn` to a private top-level [`MirFunc`] `__local_<owner>_
    /// <name>_<ctr>`. The name is registered in `nested_fn_globals` BEFORE the
    /// body is lowered so self-recursion resolves; the child inherits the
    /// parent's in-scope statics + sibling nested fns (decl-before-use).
    fn lower_nested_fn(
        &mut self,
        name: Idx<BindingInfo>,
        params: IdxRange<BindingInfo>,
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
        self.nested_fn_globals.insert(raw(name), hoisted);

        let mf = MirFunc::new(hoisted);
        let entry = mf.entry();
        let mut child = Builder {
            ws: self.ws,
            scx: self.scx,
            body: self.body,
            inf: self.inf,
            f: mf,
            cur: entry,
            binds: HashMap::new(),
            loop_targets: Vec::new(),
            defers: Vec::new(),
            in_defer_body: false,
            resume_arg: None,
            param_shapes: Vec::new(),
            ret_shape: None,
            ret_tuple: None,
            // A nested fn is emitted as a sibling top-level Lua fn and is never a
            // GLSL callee (referenced indirectly), so a return coercion here is
            // Lua-identity only; leaving it off matches today's emitted text.
            ret_ty: None,
            group_binds: HashMap::new(),
            // In-scope statics + sibling nested fns are module-level, so the
            // hoisted body may reference them; enclosing params/locals are NOT
            // inherited (bone rejects capturing them).
            static_cells: self.static_cells.clone(),
            static_owner: hoisted,
            static_counters: HashMap::new(),
            nested_fn_globals: self.nested_fn_globals.clone(),
            nested_fn_counters: HashMap::new(),
        };

        for pb in params {
            let cell = child.f.new_cell(Some(child.body.binding_arena[pb].name));
            child.f.params.push(cell);
            child.binds.insert(raw(pb), cell);
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
            match self.body.statement_arena[sidx].clone() {
                Statement::Let { name, value } => {
                    // A flat-enum local threads as leaf cells (like a scalarized
                    // param), so a later `match x` reads the cells with no box and a
                    // whole-value use rebuilds the box on demand. A NULLABLE flat-enum
                    // local threads too -- `tag = nil` is the null sentinel, handled by
                    // the nil-safe spread / `== null` tag test / conditional materialize.
                    if let Some(shape) = self.flat_enum_value_shape_of(value) {
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
                    let v = self.lower_expr(value)?;
                    self.bind_for_pattern(&vars, v);
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
                Statement::Fn { name, params, ret_ty: _, body } => {
                    self.lower_nested_fn(name, params, body)?;
                }
                Statement::Comment(_) => {}
                Statement::While { cond, body } => {
                    self.lower_while(cond, body)?;
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

    /// One inline defer splice: run the body, then clear the co-fn terminal
    /// flag (if any) so a later kill/error never double-runs a completed defer.
    fn lower_defer_splice(&mut self, body: Idx<Expr>, flag: Option<Idx<Cell>>) -> Result<(), CompilerError> {
        let was = std::mem::replace(&mut self.in_defer_body, true);
        let res = self.lower_stmt_expr(body);
        self.in_defer_body = was;
        res?;
        if let Some(f) = flag {
            let off = self.emit_cur(Inst::Const(Lit::Bool(false)));
            self.emit_cur(Inst::Store { cell: f, value: off });
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
        let v = self.lower_expr_inner(e)?;
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

            // A field-ref key path (`.field` resolved against `FieldOf<T, V>`)
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
                Ok(self.emit_cur(Inst::Global(self.nested_fn_globals[&raw(b)])))
            }
            Expr::Var(b) => {
                let cell = *self
                    .binds
                    .get(&raw(b))
                    .ok_or_else(|| gap("read of an unbound variable"))?;
                Ok(self.emit_cur(Inst::Load(cell)))
            }
            Expr::Global(name) => Ok(self.emit_cur(Inst::Global(name))),

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
                self.defers.push(Vec::new());
                let terminated = self.lower_stmts(statements)?;
                if terminated {
                    self.defers.pop();
                    return Ok(self.emit_cur(Inst::Const(Lit::Nil)));
                }
                let v = match tail {
                    Some(t) => self.lower_expr(t)?,
                    None => self.emit_cur(Inst::Const(Lit::Nil)),
                };
                // Block-scope defers run after the tail value, before the block
                // yields it.
                self.run_top_frame()?;
                self.defers.pop();
                Ok(v)
            }

            Expr::Call { callee, args } => self.lower_call(e, callee, args),

            // --- aggregates + projections + container reads ---
            Expr::Tuple(elems) => {
                let vs = self.lower_exprs(elems)?;
                Ok(self.emit_cur(Inst::MakeTuple(vs)))
            }
            Expr::Array(elems) => self.lower_array_literal(elems, ContainerKind::Vec),
            Expr::NativeArray(elems) => self.lower_array_literal(elems, ContainerKind::NativeArray),
            Expr::Index { base, index } => {
                let kind = self.coll_kind(base);
                let b = self.lower_expr(base)?;
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
            Expr::TupleIndex { tuple, index } => {
                let b = self.lower_expr(tuple)?;
                // `w.0` on a transparent newtype is identity -- its runtime value
                // IS the inner, not a positional table -- so `TupleIndex` (a
                // `[1]` read) would index a scalar and yield nil.
                if self.expr_is_newtype(tuple) {
                    Ok(b)
                } else {
                    Ok(self.emit_cur(Inst::TupleIndex { base: b, index }))
                }
            }
            // A value-struct literal is built via the repr layer (so a callsite
            // can splat it without a cdata); a plain struct is a Lua table.
            Expr::StructLiteral { name, fields, bases } if self.is_value_struct(name) => {
                let r = self.value_literal_repr(name, fields, bases)?;
                Ok(self.pack(r))
            }
            Expr::StructLiteral { name, fields, bases } => {
                self.lower_struct_literal(name, fields, bases)
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
                    return Err(gap("`yield*` inside a `defer` body is not allowed"));
                }
                self.lower_yield_delegate(sub)
            }

            // A closure: lower its body into a sub-CFG in the shared arenas.
            // Captured outer vars are the enclosing cells (Lua upvalues); the
            // closure's params are fresh cells (its Lua arg list).
            Expr::Lambda { params, body } => {
                let param_cells: Vec<Idx<Cell>> = params
                    .into_iter()
                    .map(|pb| {
                        let cell = self.f.new_cell(Some(self.body.binding_arena[pb].name));
                        self.binds.insert(raw(pb), cell);
                        cell
                    })
                    .collect();
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
                let saved_ret_tuple = self.ret_tuple;
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
                // lambda body is not the defer body's code (a `yield`/`yield*`
                // there is already rejected by bone as yield-in-plain-lambda, so
                // the reset unmasks no diagnostic -- it just makes the flag honest).
                let saved_defers = std::mem::take(&mut self.defers);
                let saved_in_defer = std::mem::replace(&mut self.in_defer_body, false);
                self.ret_tuple = self.lambda_ret_tuple_arity(e);
                let v = self.lower_expr(body)?;
                if self.is_open() {
                    let leaves = match self.ret_tuple {
                        Some(n) => self.tuple_return_operands(v, n),
                        None => vec![v],
                    };
                    self.f.set_term(self.cur, Term::Return(leaves));
                }
                self.defers = saved_defers;
                self.in_defer_body = saved_in_defer;
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
                    Err(_) => self.emit_cur(Inst::Field { base: bv, field }),
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
            Expr::FieldAccess { base, field } => {
                let b = self.lower_expr(base)?;
                // A flattened enum field: write the discriminant + payload slots
                // directly on the parent table (no nested box).
                if let Some(shape) = self.flat_field_of(base, field) {
                    let cells = self.lower_flat_value(&shape, value)?;
                    self.store_flat_cells(b, field, cells);
                    return Ok(());
                }
                let v = self.lower_expr(value)?;
                self.emit_cur(Inst::StoreField {
                    base: b,
                    field,
                    value: v,
                });
            }
            Expr::Index { base, index } => {
                let kind = self.coll_kind(base);
                let b = self.lower_expr(base)?;
                let i = self.lower_expr(index)?;
                let v = self.lower_expr(value)?;
                self.emit_cur(Inst::StoreIndex {
                    base: b,
                    index: i,
                    value: v,
                    kind,
                });
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
                let recv = self.lower_expr(base)?;
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
            Expr::Global(name) => {
                let cur = self.emit_cur(Inst::Global(name));
                let rv = self.lower_expr(value)?;
                let new_v = self.compound_combine(scalar, op_fn, overload, cur, rv)?;
                self.emit_cur(Inst::StoreGlobal { name, value: new_v });
            }
            Expr::FieldAccess { base, field } => {
                let b = self.lower_expr(base)?;
                let cur = self.emit_cur(Inst::Field { base: b, field });
                let rv = self.lower_expr(value)?;
                let new_v = self.compound_combine(scalar, op_fn, overload, cur, rv)?;
                self.emit_cur(Inst::StoreField { base: b, field, value: new_v });
            }
            Expr::Index { base, index } => {
                let kind = self.coll_kind(base);
                let b = self.lower_expr(base)?;
                let i = self.lower_expr(index)?;
                let cur = self.emit_cur(Inst::Index { base: b, index: i, kind });
                let rv = self.lower_expr(value)?;
                let new_v = self.compound_combine(scalar, op_fn, overload, cur, rv)?;
                self.emit_cur(Inst::StoreIndex { base: b, index: i, value: new_v, kind });
            }
            // `base?.field <op>= value` — skip silently if any `?.` link is null.
            // The read-modify-write and `value` run only on the non-null path.
            Expr::SafeNav { base, field } => {
                let recv = self.lower_expr(base)?;
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
            Expr::Global(name) => {
                let old = self.emit_cur(Inst::Global(name));
                let new = bump(self, old);
                self.emit_cur(Inst::StoreGlobal { name, value: new });
                Ok(if postfix { old } else { new })
            }
            Expr::FieldAccess { base, field } => {
                let b = self.lower_expr(base)?;
                let old = self.emit_cur(Inst::Field { base: b, field });
                let new = bump(self, old);
                self.emit_cur(Inst::StoreField { base: b, field, value: new });
                Ok(if postfix { old } else { new })
            }
            Expr::Index { base, index } => {
                let kind = self.coll_kind(base);
                let b = self.lower_expr(base)?;
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
            let arity = self.peeled_tuple_arity(e);
            self.emit_call_value_n(arity, fieldv, argv)
        } else {
            // `recv?.method(args)` UFCS: `method(recv, args)`, recv as arg 0.
            let func = self.resolve_funcref(e, method)?;
            let arg_idxs: Vec<Idx<Expr>> = args.into_iter().collect();
            if fn_uses_scalar_abi(self.ws, self.scx, func) {
                // Scalar-ABI callee: splat the value receiver + value args into
                // leaves (a value return packs to the single safe-nav result).
                self.scalar_ufcs_call(func, recv, &arg_idxs)?
            } else {
                let mut argv = vec![recv];
                for a in arg_idxs {
                    argv.push(self.lower_expr(a)?);
                }
                // A tuple-returning method splats N values; capture + pack into
                // the single (boxed, nullable) safe-nav result.
                let ret_ty = self.ws.module(func.module).functions[func.func].return_ty;
                match ret_ty.and_then(|t| tuple_arity(self.ws, t)) {
                    Some(n) => {
                        let r = self.emit_tuple_call(func, argv, n);
                        self.pack(r)
                    }
                    None => self.emit_cur(Inst::Call { func, args: argv }),
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

    /// `[..]` / `![..]`, with spread support. No spread -> a single
    /// `MakeVec`/`MakeArray`. With a `..x` element, build the empty array then
    /// push each element / splice each spread (`ArrayPush`), so spreads of
    /// tuples/structs/arrays splice element-wise.
    fn lower_array_literal(
        &mut self,
        elems: IdxRange<Expr>,
        kind: ContainerKind,
    ) -> Result<Idx<Inst>, CompilerError> {
        let idxs: Vec<Idx<Expr>> = elems.into_iter().collect();
        let has_spread = idxs
            .iter()
            .any(|&e| matches!(self.body.expr_arena[e], Expr::Spread(_)));
        if !has_spread {
            let vs: Vec<Idx<Inst>> = idxs.iter().map(|&e| self.lower_expr(e)).collect::<Result<_, _>>()?;
            return Ok(self.emit_cur(match kind {
                ContainerKind::Vec => Inst::MakeVec(vs),
                _ => Inst::MakeArray(vs),
            }));
        }
        let arr = self.emit_cur(match kind {
            ContainerKind::Vec => Inst::MakeVec(vec![]),
            _ => Inst::MakeArray(vec![]),
        });
        for &e in &idxs {
            if let Expr::Spread(inner) = self.body.expr_arena[e] {
                self.spread_into(arr, inner, kind)?;
            } else {
                let v = self.lower_expr(e)?;
                self.emit_cur(Inst::ArrayPush { arr, value: v, kind });
            }
        }
        Ok(arr)
    }

    /// Splice `inner` into the array `arr` (representation `kind`): a tuple by its
    /// static arity, a struct by its declared fields in order, an array/Vec via a
    /// runtime copy loop. Other shapes gap.
    fn spread_into(
        &mut self,
        arr: Idx<Inst>,
        inner: Idx<Expr>,
        kind: ContainerKind,
    ) -> Result<(), CompilerError> {
        let v = self.lower_expr(inner)?;
        match self.expr_ty(inner) {
            Some(TypeExpr::Tuple { elements }) => {
                for i in 0..elements.into_iter().count() {
                    let comp = self.emit_cur(Inst::TupleIndex { base: v, index: i });
                    self.emit_cur(Inst::ArrayPush { arr, value: comp, kind });
                }
                Ok(())
            }
            Some(TypeExpr::App { ctor: TypeCtor::Array | TypeCtor::Map, .. }) => {
                self.spread_array_loop(arr, inner, v, kind);
                Ok(())
            }
            Some(TypeExpr::App { ctor: TypeCtor::Nominal(name), .. }) => {
                let fields = self
                    .struct_field_names(name)
                    .ok_or_else(|| gap("spread of a non-struct nominal type"))?;
                for field in fields {
                    let comp = self.emit_cur(Inst::Field { base: v, field });
                    self.emit_cur(Inst::ArrayPush { arr, value: comp, kind });
                }
                Ok(())
            }
            _ => Err(gap("spread of an unsupported type")),
        }
    }

    /// Runtime copy of every element of `src` (read per its `coll_kind`) into
    /// `arr` (appended per `dest_kind`).
    fn spread_array_loop(
        &mut self,
        arr: Idx<Inst>,
        src_expr: Idx<Expr>,
        src: Idx<Inst>,
        dest_kind: ContainerKind,
    ) {
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
        let elem = self.emit_cur(Inst::Index { base: src, index: i_now, kind: src_kind });
        self.emit_cur(Inst::ArrayPush { arr, value: elem, kind: dest_kind });
        let i2 = self.emit_cur(Inst::Load(i_cell));
        let one = self.emit_cur(Inst::Const(Lit::Int(1)));
        let inc = self.emit_cur(Inst::IntAdd(i2, one));
        self.emit_cur(Inst::Store { cell: i_cell, value: inc });
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = exit;
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
                // A `@struct_expand` fn (docs/pass-uniforms-design.md) is replaced
                // by its literal pairs list -- keyed on the declaration annotation
                // (checked off the resolved FuncRef, so no user fn is hijacked by
                // name and the common non-annotated call pays only one field read).
                Ok(func) if self.fn_has_struct_expand(func) => {
                    self.expand_struct_pairs(e, func, &arg_idxs)
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
            // A fn-typed local: load the cell and call through the value. An
            // indirect call can't splat — args are boxed.
            Callee::Local(b) => {
                // A nested fn: call the hoisted top-level fn by name (no closure).
                if let Some(&hoisted) = self.nested_fn_globals.get(&raw(b)) {
                    let g = self.emit_cur(Inst::Global(hoisted));
                    let arg_vals: Vec<Idx<Inst>> =
                        arg_idxs.iter().map(|&a| self.lower_expr(a)).collect::<Result<_, _>>()?;
                    let v = self.emit_call_value(e, g, arg_vals);
                    return Ok(ValueRepr::Scalar(v));
                }
                let cell = *self
                    .binds
                    .get(&raw(b))
                    .ok_or_else(|| gap("call to an unbound fn-typed local"))?;
                let callee_v = self.emit_cur(Inst::Load(cell));
                let arg_vals: Vec<Idx<Inst>> =
                    arg_idxs.iter().map(|&a| self.lower_expr(a)).collect::<Result<_, _>>()?;
                let v = self.emit_call_value(e, callee_v, arg_vals);
                Ok(ValueRepr::Scalar(v))
            }
            // A fn-typed expression result: lower it, call through it.
            Callee::Value(ve) => {
                let callee_v = self.lower_expr(ve)?;
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
        let ret_tuple = self.ws.module(func.module).functions[func.func]
            .return_ty
            .and_then(|t| tuple_arity(self.ws, t));
        if !fn_uses_scalar_abi(self.ws, self.scx, func) {
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
            if let Some(n) = ret_tuple {
                return Ok(self.emit_tuple_call_tree(func, args, n, tree));
            }
            let call = self.emit_cur(Inst::Call { func, args });
            if let Some(t) = tree {
                self.f.bound_trees.insert(call, t);
            }
            return Ok(ValueRepr::Scalar(call));
        }
        let args = self.scalar_call_args(func, arg_idxs, any_named)?;
        let ret_shape = self.ws.module(func.module).functions[func.func]
            .return_ty
            .and_then(|t| sig_value_shape(self.ws, t));
        match ret_shape {
            Some(shape) => {
                let n = shape.leaf_count();
                let dests: Vec<Idx<Cell>> = (0..n).map(|_| self.f.new_cell(None)).collect();
                let call = self.emit_cur(Inst::CallMulti { dests: dests.clone(), func, args });
                if let Some(t) = tree {
                    self.f.bound_trees.insert(call, t);
                }
                let leaves: Vec<Idx<Inst>> =
                    dests.iter().map(|c| self.emit_cur(Inst::Load(*c))).collect();
                let mut idx = 0;
                Ok(self.rebuild_from_leaves(&shape, &leaves, &mut idx))
            }
            None => match ret_tuple {
                Some(n) => Ok(self.emit_tuple_call_tree(func, args, n, tree)),
                None => {
                    let call = self.emit_cur(Inst::Call { func, args });
                    if let Some(t) = tree {
                        self.f.bound_trees.insert(call, t);
                    }
                    Ok(ValueRepr::Scalar(call))
                }
            },
        }
    }

    /// A direct call to a tuple-returning fn under the multi-return ABI: the
    /// callee returns N values, captured into N dest cells via [`Inst::CallMulti`].
    /// The result value is `MakeTuple(loads)` -- a boxed use materializes the
    /// table (same as the old ABI), a destructure elides it (`bind_for_pattern`).
    fn emit_tuple_call(&mut self, func: FuncRef, args: Vec<Idx<Inst>>, n: usize) -> ValueRepr {
        self.emit_tuple_call_tree(func, args, n, None)
    }

    /// [`Self::emit_tuple_call`] recording an `Iterator`-bound resolution tree
    /// against the emitted call inst (chain roots like a directly-called
    /// bounded `next`).
    fn emit_tuple_call_tree(
        &mut self,
        func: FuncRef,
        args: Vec<Idx<Inst>>,
        n: usize,
        tree: Option<bone_semantic::prelude::BoundTree>,
    ) -> ValueRepr {
        let dests: Vec<Idx<Cell>> = (0..n).map(|_| self.f.new_cell(None)).collect();
        let call = self.emit_cur(Inst::CallMulti { dests: dests.clone(), func, args });
        if let Some(t) = tree {
            self.f.bound_trees.insert(call, t);
        }
        let loads: Vec<Idx<Inst>> = dests.iter().map(|c| self.emit_cur(Inst::Load(*c))).collect();
        ValueRepr::Scalar(self.emit_cur(Inst::MakeTuple(loads)))
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
        self.emit_call_value_n(self.expr_tuple_arity(e), callee, args)
    }

    /// [`Self::emit_call_value`] with an explicit tuple arity (the safe-nav call
    /// paths pass the nil-peeled arity, since their expr type is `(..)?`).
    fn emit_call_value_n(&mut self, arity: Option<usize>, callee: Idx<Inst>, args: Vec<Idx<Inst>>) -> Idx<Inst> {
        match arity {
            Some(n) => {
                let dests: Vec<Idx<Cell>> = (0..n).map(|_| self.f.new_cell(None)).collect();
                self.emit_cur(Inst::CallValueMulti { dests: dests.clone(), callee, args });
                let loads: Vec<Idx<Inst>> =
                    dests.iter().map(|c| self.emit_cur(Inst::Load(*c))).collect();
                self.emit_cur(Inst::MakeTuple(loads))
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
    ) -> Result<Idx<Inst>, CompilerError> {
        let param_shapes: Vec<Option<ValueShape>> = self.ws.module(func.module).functions[func.func]
            .param_decls
            .iter()
            .map(|pd| sig_value_shape(self.ws, pd.ty))
            .collect();
        let mut argv = Vec::new();
        match param_shapes.first().and_then(|s| s.clone()) {
            Some(shape) => argv.extend(self.cdata_leaves(recv, &shape)),
            None => argv.push(recv),
        }
        for (i, &a) in arg_idxs.iter().enumerate() {
            match param_shapes.get(i + 1).and_then(|s| s.clone()) {
                Some(shape) => {
                    let r = self.lower_expr_repr(a)?;
                    argv.extend(self.repr_to_leaves(&r, &shape));
                }
                None => argv.push(self.lower_expr(a)?),
            }
        }
        let ret_shape = self.ws.module(func.module).functions[func.func]
            .return_ty
            .and_then(|t| sig_value_shape(self.ws, t));
        match ret_shape {
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
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        let param_shapes: Vec<Option<ValueShape>> = self.ws.module(func.module).functions[func.func]
            .param_decls
            .iter()
            .map(|pd| sig_value_shape(self.ws, pd.ty))
            .collect();
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
        // Named/gap path: lower each arg into its param SLOT (source order), then
        // splat/nil-fill per slot. Mirrors `reorder_named_args` + splatting.
        let param_names: Vec<Ustr> = self.ws.module(func.module).functions[func.func]
            .param_decls
            .iter()
            .map(|pd| pd.name)
            .collect();
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
            // A flat-enum Group `[tag, _0.., <rec>..]` packs to the `{__variant}`
            // box (`MakeEnumDyn`), the enum escape -- not a `<name>_ct` cdata.
            ValueRepr::Group { name, parts } if flat_enum_shape(self.ws, name).is_some() => {
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
            ValueRepr::Group { name, parts } => {
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
            ValueRepr::Group { parts, .. } => {
                let mut out = Vec::new();
                for ((_, fs), part) in shape.fields.iter().zip(parts) {
                    match fs {
                        FieldShape::Prim => out.push(self.pack(part.clone())),
                        FieldShape::Nested(sub) => out.extend(self.repr_to_leaves(part, sub)),
                    }
                }
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
        let mut out = Vec::new();
        for (fname, fs) in &shape.fields {
            let fv = self.emit_cur(Inst::Field { base: v, field: *fname });
            match fs {
                FieldShape::Prim => out.push(fv),
                FieldShape::Nested(sub) => out.extend(self.cdata_leaves(fv, sub)),
            }
        }
        out
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
        ValueRepr::Group { name, parts }
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
                // A flattened enum field read as a WHOLE value re-materializes the
                // box from the parent's slots (the single escape boundary).
                if let Some(shape) = self.flat_field_of(base, field) {
                    let parent = self.lower_expr(base)?;
                    return Ok(ValueRepr::Scalar(self.materialize_flat(parent, field, &shape)));
                }
                let br = self.lower_expr_repr(base)?;
                if let ValueRepr::Group { name, parts } = &br {
                    if let Some(i) = self.field_index(*name, field) {
                        return Ok(parts[i].clone());
                    }
                }
                // A boxed value struct: read the field directly. A payload-free
                // enum field is a plain `int` (the enum value IS the int), so no
                // conversion -- same as any primitive field.
                let basev = self.pack(br);
                Ok(ValueRepr::Scalar(self.emit_cur(Inst::Field { base: basev, field })))
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
            _ => Ok(ValueRepr::Scalar(self.lower_expr(e)?)),
        }
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
                            (Some(ValueRepr::Group { name: bn, parts }), _) => {
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
        Ok(ValueRepr::Group { name, parts })
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
    fn resolve_funcref(
        &self,
        e: Idx<Expr>,
        name: ustr::Ustr,
    ) -> Result<FuncRef, CompilerError> {
        if let Some(&(module, func)) = self
            .inf
            .and_then(|i| i.resolved_overload_calls.get(&e))
        {
            return Ok(FuncRef { module, func });
        }
        match self.ws.functions_by_name.get(&name).map(|l| l.as_slice()) {
            Some([(module, func)]) => Ok(FuncRef {
                module: *module,
                func: *func,
            }),
            Some([]) | None => Err(gap(format!("unresolved call `{name}`"))),
            Some(_) => Err(gap(format!(
                "overloaded call `{name}` with no recorded resolution"
            ))),
        }
    }

    /// `yield* sub` — drive the inner coroutine `sub` through its `.resume()`,
    /// re-yielding each value; the expression's value is `sub`'s return value.
    /// Built as a normal pump loop with a `yield` inside, so the coroutine
    /// transform splits it like any other yield. Mirrors rock-codegen / old skull.
    fn lower_yield_delegate(&mut self, sub_expr: Idx<Expr>) -> Result<Idx<Inst>, CompilerError> {
        let rac = self.resume_arg.ok_or_else(|| gap("yield* outside a coroutine"))?;
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
        // the `yield*` value is the inner's return.
        self.cur = exit;
        self.yield_delegate_exit(sub)
    }

    /// The `yield*` epilogue (shared by the boxed + flat channels): clear the
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

    /// `yield* sub` where `sub` yields a flat enum: drive the inner via its
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

    /// Lower an interpolated string to a left-folded `..` chain. A `Literal` part
    /// is a string const; a `{x}` hole stringifies by its static type (D8: the
    /// dispatch is decided HERE, not at the backend) — scalar -> [`Inst::ToString`]
    /// (`tostring`), float -> a `__rock_format_float` call, composite -> `inspect`,
    /// transparent newtype -> `Name(<tostring inner>)`. A `{x:spec}` hole gaps
    /// (format-spec lowering is its own follow-up; skull refuses rather than
    /// mis-format). An empty interpolation is the `""` const.
    fn lower_interp(
        &mut self,
        parts: IdxRange<bone_semantic::hir::InterpPart>,
    ) -> Result<Idx<Inst>, CompilerError> {
        use bone_semantic::hir::InterpPart;
        let mut acc: Option<Idx<Inst>> = None;
        for p in parts {
            let s = match self.body.interp_part_arena[p].clone() {
                InterpPart::Literal(lit) => self.emit_cur(Inst::Const(Lit::Str(lit))),
                InterpPart::Value { expr, spec } => {
                    let v = self.lower_expr(expr)?;
                    match spec {
                        None => self.stringify(expr, v)?,
                        Some(spec) => self.lower_format(expr, v, &spec)?,
                    }
                }
            };
            acc = Some(match acc {
                None => s,
                Some(lhs) => self.emit_cur(Inst::StringConcat(lhs, s)),
            });
        }
        Ok(acc.unwrap_or_else(|| self.emit_cur(Inst::Const(Lit::Str(Ustr::from(""))))))
    }

    /// Default `{x}` stringify for value `v` of expression `expr`, dispatched on
    /// `expr`'s static type (mirrors rock-codegen's interp dispatch).
    fn stringify(&mut self, expr: Idx<Expr>, v: Idx<Inst>) -> Result<Idx<Inst>, CompilerError> {
        // A literal `{null}` hole renders `tostring(nil)` = "nil".
        if matches!(self.body.expr_arena[expr], Expr::Null) {
            return Ok(self.emit_cur(Inst::ToString(v)));
        }
        match self.expr_ty(expr) {
            Some(TypeExpr::Builtin(BuiltinTy::Int | BuiltinTy::Bool | BuiltinTy::String)) => {
                Ok(self.emit_cur(Inst::ToString(v)))
            }
            Some(TypeExpr::Builtin(BuiltinTy::Float)) => {
                let func = self.resolve_global_fn("__rock_format_float")?;
                Ok(self.emit_cur(Inst::Call { func, args: vec![v] }))
            }
            // A transparent newtype renders `Name(<inner>)`; its runtime value IS
            // the raw inner, so `tostring` it directly.
            Some(TypeExpr::App { ctor: TypeCtor::Nominal(name), .. })
                if self.ws.newtypes_by_name.contains_key(&name) =>
            {
                let open = self.emit_cur(Inst::Const(Lit::Str(Ustr::from(format!("{name}(").as_str()))));
                let inner = self.emit_cur(Inst::ToString(v));
                let close = self.emit_cur(Inst::Const(Lit::Str(Ustr::from(")"))));
                let a = self.emit_cur(Inst::StringConcat(open, inner));
                Ok(self.emit_cur(Inst::StringConcat(a, close)))
            }
            // Composite (tuple/array/map/struct/enum) -> structured `inspect`.
            Some(TypeExpr::Tuple { .. })
            | Some(TypeExpr::App { ctor: TypeCtor::Array | TypeCtor::Map, .. }) => {
                let func = self.resolve_global_fn("inspect")?;
                Ok(self.emit_cur(Inst::Call { func, args: vec![v] }))
            }
            // A payload-free enum value is its ordinal int -> render the variant
            // name statically from `__rock_enum_<E>_names[tag]` (0-indexed, the
            // table holds the full `<E>::<V>` display string, matching rock's
            // inspect output -- verified by skull-diff). No `inspect` round-trip;
            // the value carries no type tag at runtime, so this is the only point
            // the static type is known. The `inspect(any)` escape (a payload-free
            // enum passed to `inspect`/stored in `any`/inside a composite) is the
            // deferred box-on-escape-into-`any` work; see docs/skull-enum-value-types.md.
            Some(TypeExpr::App { ctor: TypeCtor::Nominal(name), .. })
                if payload_free_enum_variants(self.ws, name).is_some() =>
            {
                let names =
                    self.emit_cur(Inst::Global(Ustr::from(format!("__rock_enum_{name}_names").as_str())));
                Ok(self.emit_cur(Inst::Index { base: names, index: v, kind: ContainerKind::Map }))
            }
            Some(TypeExpr::App { ctor: TypeCtor::Nominal(name), .. })
                if self.ws.struct_by_name.contains_key(&name)
                    || self.ws.enums_by_name.contains_key(&name) =>
            {
                let func = self.resolve_global_fn("inspect")?;
                Ok(self.emit_cur(Inst::Call { func, args: vec![v] }))
            }
            _ => Err(gap("interpolation hole of unknown/unsupported type")),
        }
    }

    /// Format value `v` of `expr` per `spec` (`{x:.2f}` etc.). A static spec
    /// emits one `string.format("<fmt>", v)`; dynamic width/precision assemble
    /// the conversion at runtime (`FormatDyn`); grouping/center/fill wrap with
    /// the stdlib `__rock_fmt_grouping`/`__rock_fmt_pad`; a composite with `.N`
    /// precision routes through `inspect_formatted`. Mirrors the old skull /
    /// rock-codegen dispatch. The `%`/`b` conversions (raw prelude globals, not
    /// FuncRef-resolvable) gap for now.
    fn lower_format(
        &mut self,
        expr: Idx<Expr>,
        v: Idx<Inst>,
        spec: &bone_semantic::hir::InterpSpec,
    ) -> Result<Idx<Inst>, CompilerError> {
        // Effective conversion letter when the spec omits one (`{x:5}`).
        let inferred = match self.expr_ty(expr) {
            Some(TypeExpr::Builtin(BuiltinTy::Int)) => Some('d'),
            Some(TypeExpr::Builtin(BuiltinTy::Float)) => Some('f'),
            Some(TypeExpr::Builtin(BuiltinTy::String | BuiltinTy::Bool)) => Some('s'),
            _ => None,
        };
        // A composite with `.N` precision and no explicit type is a recursive
        // precision-aware stringify -> `inspect_formatted`.
        let has_precision = spec.precision.is_some() || spec.precision_expr.is_some();
        let plain_spec = spec.format_type.is_none()
            && spec.grouping.is_none()
            && spec.fill.is_none()
            && spec.width.is_none()
            && spec.width_expr.is_none()
            && spec.alignment.is_none();
        if has_precision && plain_spec && inferred.is_none() && self.is_composite_expr(expr) {
            let prec = self.lower_precision(spec)?;
            let func = self.resolve_global_fn("inspect_formatted")?;
            return Ok(self.emit_cur(Inst::Call { func, args: vec![v, prec] }));
        }

        let grouping = spec.grouping;
        let has_width = spec.width.is_some() || spec.width_expr.is_some();
        let ty = spec.format_type.or(inferred);
        let special = matches!(spec.format_type, Some('b') | Some('%'));
        let val_is_float = matches!(self.expr_ty(expr), Some(TypeExpr::Builtin(BuiltinTy::Float)));
        let needs_pad = spec.fill.is_some()
            || matches!(spec.alignment, Some('^'))
            || ((grouping.is_some() || special) && has_width);
        let skip_width = grouping.is_some() || needs_pad || special;

        // `%` (percent) and `b` (binary) have no printf equivalent -> the stdlib
        // helpers; width/align deferred to the pad helper (skip_width set above).
        let mut out = if spec.format_type == Some('%') {
            let prec = self.lower_precision(spec)?;
            let func = self.resolve_global_fn("__rock_fmt_percent")?;
            self.emit_cur(Inst::Call { func, args: vec![v, prec] })
        } else if ty == Some('b') {
            let func = self.resolve_global_fn("__rock_fmt_binary")?;
            self.emit_cur(Inst::Call { func, args: vec![v] })
        } else {
            // printf path. An integer-class conversion on a float floors first
            // (Lua `%d` truncates toward zero; rock floors toward -inf).
            let v = if val_is_float && matches!(ty, Some('d' | 'x' | 'X' | 'o')) {
                self.emit_cur(Inst::FloatToInt(v))
            } else {
                v
            };
            let pieces = fmt_pieces(spec, inferred, skip_width, needs_pad)
                .ok_or_else(|| gap("interpolation format specifier (grouping / fill / center)"))?;
            self.emit_format_pieces(v, pieces)?
        };

        if let Some(sep) = grouping {
            // `_` with a radix conversion groups by nibble (4), else by 3.
            let group = match (sep, spec.format_type.or(inferred)) {
                ('_', Some('x' | 'X' | 'o' | 'b')) => 4i64,
                _ => 3i64,
            };
            let sep_v = self.emit_cur(Inst::Const(Lit::Str(Ustr::from(sep.to_string().as_str()))));
            let grp_v = self.emit_cur(Inst::Const(Lit::Int(group)));
            let func = self.resolve_global_fn("__rock_fmt_grouping")?;
            out = self.emit_cur(Inst::Call { func, args: vec![out, sep_v, grp_v] });
        }
        if needs_pad {
            // Pad to the field width AFTER grouping (separators count toward it).
            let w = match (spec.width, spec.width_expr) {
                (Some(w), _) => self.emit_cur(Inst::Const(Lit::Int(w as i64))),
                (None, Some(we)) => self.lower_expr(we)?,
                (None, None) => self.emit_cur(Inst::Const(Lit::Int(0))),
            };
            let align = spec.alignment.map(|c| c.to_string()).unwrap_or_else(|| ">".to_string());
            let align_v = self.emit_cur(Inst::Const(Lit::Str(Ustr::from(align.as_str()))));
            let fill = spec.fill.unwrap_or(' ').to_string();
            let fill_v = self.emit_cur(Inst::Const(Lit::Str(Ustr::from(fill.as_str()))));
            let func = self.resolve_global_fn("__rock_fmt_pad")?;
            out = self.emit_cur(Inst::Call { func, args: vec![out, w, align_v, fill_v] });
        }
        Ok(out)
    }

    /// A spec's precision as a value: static `Int`, dynamic `{.{p}}` expression,
    /// or `-1` (none).
    fn lower_precision(&mut self, spec: &bone_semantic::hir::InterpSpec) -> Result<Idx<Inst>, CompilerError> {
        match (spec.precision, spec.precision_expr) {
            (Some(p), _) => Ok(self.emit_cur(Inst::Const(Lit::Int(p as i64)))),
            (None, Some(pe)) => self.lower_expr(pe),
            (None, None) => Ok(self.emit_cur(Inst::Const(Lit::Int(-1)))),
        }
    }

    /// Whether `e`'s type is a composite (tuple/array/map/struct/enum) — what
    /// `inspect`/`inspect_formatted` render structurally.
    fn is_composite_expr(&self, e: Idx<Expr>) -> bool {
        match self.expr_ty(e) {
            Some(TypeExpr::Tuple { .. }) => true,
            Some(TypeExpr::App { ctor: TypeCtor::Array | TypeCtor::Map, .. }) => true,
            Some(TypeExpr::App { ctor: TypeCtor::Nominal(name), .. }) => {
                self.ws.struct_by_name.contains_key(&name) || self.ws.enums_by_name.contains_key(&name)
            }
            _ => false,
        }
    }

    /// Emit `string.format` from decomposed [`FmtPiece`]s: a single static
    /// conversion ([`Inst::Format`]) when all-literal, else assemble the
    /// conversion string at runtime and [`Inst::FormatDyn`].
    fn emit_format_pieces(
        &mut self,
        v: Idx<Inst>,
        pieces: Vec<FmtPiece>,
    ) -> Result<Idx<Inst>, CompilerError> {
        if let [FmtPiece::Lit(s)] = &pieces[..] {
            return Ok(self.emit_cur(Inst::Format { value: v, fmt: Ustr::from(s.as_str()) }));
        }
        let mut fmt: Option<Idx<Inst>> = None;
        for piece in pieces {
            let part = match piece {
                FmtPiece::Lit(s) => self.emit_cur(Inst::Const(Lit::Str(Ustr::from(s.as_str())))),
                FmtPiece::DynInt(e) => {
                    let ev = self.lower_expr(e)?;
                    self.emit_cur(Inst::ToString(ev))
                }
            };
            fmt = Some(match fmt {
                None => part,
                Some(lhs) => self.emit_cur(Inst::StringConcat(lhs, part)),
            });
        }
        let fmt = fmt.unwrap_or_else(|| self.emit_cur(Inst::Const(Lit::Str(Ustr::from("")))));
        Ok(self.emit_cur(Inst::FormatDyn { value: v, fmt }))
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
                    if self.expr_is_flat_enum(v) {
                        let tag = match self.lower_expr_repr(v)? {
                            ValueRepr::Group { parts, .. } => {
                                self.pack(parts.into_iter().next().expect("flat enum group has a tag leaf"))
                            }
                            // a boxed flat enum: nil-or-table, compare directly.
                            ValueRepr::Scalar(s) => s,
                        };
                        let nil = self.emit_cur(Inst::Const(Lit::Nil));
                        return Ok(self.emit_cur(if matches!(op, BinOp::Eq) {
                            Inst::Eq(tag, nil)
                        } else {
                            Inst::NotEq(tag, nil)
                        }));
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
        let result = self.f.new_cell(None);
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
    fn lower_while(&mut self, cond: Idx<Expr>, body: Idx<Expr>) -> Result<(), CompilerError> {
        let header = self.f.new_block();
        let body_bb = self.f.new_block();
        let exit = self.f.new_block();
        self.f.set_term(self.cur, Term::Goto(header));

        self.cur = header;
        let cond_v = self.lower_expr(cond)?;
        self.f.set_term(
            self.cur,
            Term::Branch {
                cond: cond_v,
                then_bb: body_bb,
                else_bb: exit,
            },
        );

        self.cur = body_bb;
        self.loop_targets.push((header, exit));
        self.lower_stmt_expr(body)?;
        self.loop_targets.pop();
        if self.is_open() {
            self.f.set_term(self.cur, Term::Goto(header)); // back-edge
        }

        self.cur = exit;
        Ok(())
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
        match self.coll_kind(iter) {
            // `for x in [T]` always arrives with `for_iter_protocol` populated
            // (bone routes Array to the stdlib VecIter protocol); reaching the
            // dispatch means bone and skull disagree -- reject, don't emit a
            // second lowering. `enumerate(vec)` keeps its own path above.
            ContainerKind::Vec => Err(gap(
                "for over [T] must route through the iterator protocol (for_iter_protocol missing)",
            )),
            ContainerKind::NativeArray | ContainerKind::NativeBuffer => {
                self.lower_for_array(vars, iter, body)
            }
            ContainerKind::Map => self.lower_for_map(&vars, iter, body),
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

        // next returns `(bool, E)` / `(bool, A, B, ...)`: arity = 1 + element
        // leaves. A placeholder next is always arity 2 (bound conformance
        // requires exactly `(bool, E)`).
        let n = match &info.next {
            BoundNext::Concrete { func: (m, f), .. } => self
                .ws
                .module(*m)
                .functions[*f]
                .return_ty
                .and_then(|t| tuple_arity(self.ws, t))
                .ok_or_else(|| gap("for-iter `next` must return a tuple"))?,
            BoundNext::CallerGeneric(_) => 2,
        };
        if n < 2 {
            return Err(gap("for-iter `next` must return `(bool, E)`"));
        }

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
            BoundNext::Concrete { func: (m, f), sub } => {
                let func = FuncRef { module: *m, func: *f };
                let call = self.emit_cur(Inst::CallMulti {
                    dests: dests.clone(),
                    func,
                    args: vec![it_load],
                });
                if !sub.is_empty() {
                    self.f.bound_trees.insert(call, sub.clone());
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

        // body: bind the pattern to the element leaves. A single element binds
        // directly; multiple (`next -> (bool, A, B)`) pack into a MakeTuple that
        // `bind_for_pattern` elides against a matching tuple pattern (no alloc).
        self.cur = body_bb;
        let elem_val = if n == 2 {
            self.emit_cur(Inst::Load(dests[1]))
        } else {
            let loads: Vec<Idx<Inst>> =
                dests[1..].iter().map(|c| self.emit_cur(Inst::Load(*c))).collect();
            self.emit_cur(Inst::MakeTuple(loads))
        };
        self.bind_for_pattern(vars, elem_val);
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

        let arena = self.lower_expr(iter)?;
        let entries = self.emit_cur(Inst::Field { base: arena, field: Ustr::from("entries") });
        let n = self.emit_cur(Inst::Len { base: entries, kind: ContainerKind::Vec });
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

        // body: read entry; skip nil.
        self.cur = body_bb;
        let idx = self.emit_cur(Inst::Load(i_cell));
        let entry = self.emit_cur(Inst::Index { base: entries, index: idx, kind: ContainerKind::Vec });
        let nil = self.emit_cur(Inst::Const(Lit::Nil));
        let isnil = self.emit_cur(Inst::Eq(entry, nil));
        self.f.set_term(self.cur, Term::Branch { cond: isnil, then_bb: latch, else_bb: occ_bb });

        // occ: skip unoccupied.
        self.cur = occ_bb;
        let occ = self.emit_cur(Inst::Field { base: entry, field: Ustr::from("is_occupied") });
        self.f.set_term(self.cur, Term::Branch { cond: occ, then_bb: do_bb, else_bb: latch });

        // do: build Entity, bind, run body.
        self.cur = do_bb;
        // `entry` is an `ArenaEntry` (fields `index`/`generation`); the built
        // value is an `Entity` whose fields are `_`-prefixed (soft-private).
        let e_index = self.emit_cur(Inst::Field { base: entry, field: Ustr::from("index") });
        let e_gen = self.emit_cur(Inst::Field { base: entry, field: Ustr::from("generation") });
        let entity = self.emit_cur(Inst::MakeStruct {
            ty: None,
            fields: vec![
                (Ustr::from("_arena"), arena),
                (Ustr::from("_index"), e_index),
                (Ustr::from("_generation"), e_gen),
            ],
        });
        let data = self.emit_cur(Inst::Field { base: entry, field: Ustr::from("_data") });
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
        let m = self.lower_expr(iter)?;
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
            table: m,
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
        self.bind_map_entry(vars, key_cell, val_cell)?;
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
        self.bind_map_entry(xpat, key_cell, val_cell)?;
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

    fn bind_map_entry(
        &mut self,
        vars: &BindingPattern,
        key_cell: Idx<Cell>,
        val_cell: Idx<Cell>,
    ) -> Result<(), CompilerError> {
        match vars {
            BindingPattern::Tuple(subs) if subs.len() == 2 => {
                let kv = self.emit_cur(Inst::Load(key_cell));
                self.bind_for_pattern(&subs[0], kv);
                let vv = self.emit_cur(Inst::Load(val_cell));
                self.bind_for_pattern(&subs[1], vv);
                Ok(())
            }
            BindingPattern::Leaf(_) => {
                let kv = self.emit_cur(Inst::Load(key_cell));
                let vv = self.emit_cur(Inst::Load(val_cell));
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
        let elem = self.emit_cur(Inst::Index {
            base: arr_v,
            index: i_now,
            kind,
        });
        match &split {
            // split: index already bound to i_cell; bind the element pattern.
            Some((_, xpat)) => self.bind_for_pattern(xpat, elem),
            // single: bind the whole `(index, element)` tuple.
            None => {
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
        let kind = self.coll_kind(iter);
        let arr_v = self.lower_expr(iter)?;
        let len_v = self.emit_cur(Inst::Len { base: arr_v, kind });
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
        let elem = self.emit_cur(Inst::Index {
            base: arr_v,
            index: i_now,
            kind,
        });
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

    /// Bind a for-loop pattern to an element value: `Leaf` -> one cell; `Tuple`
    /// -> destructure via `TupleIndex` into a cell per element.
    fn bind_for_pattern(&mut self, vars: &BindingPattern, value: Idx<Inst>) {
        match vars {
            BindingPattern::Leaf(b) => {
                let cell = self.f.new_cell(Some(self.body.binding_arena[*b].name));
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
        let result = self.f.new_cell(None);
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
            let bv = self.lower_expr(arm.body)?;
            self.store_and_join(result, bv, join);

            self.cur = next;
        }
        // No arm matched: bone checks exhaustiveness, so this is unreachable.
        self.f.set_term(self.cur, Term::Unreachable);

        self.cur = join;
        Ok(self.emit_cur(Inst::Load(result)))
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
                    let fv = self.scrut_named(scrut, rpf.name, cdata);
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
        let mut by_value: HashMap<u32, Idx<Inst>> = HashMap::new();
        let mut flat_by_value: HashMap<u32, FlatCells> = HashMap::new();
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
                    } else if let Some(&(_, v)) = lit_box.iter().find(|(n, _)| *n == fname) {
                        fs.push((fname, v));
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
        // Non-value structs only — a `value` literal is built via the repr layer
        // (`value_literal_repr` -> `pack`, the single value-cdata source), so it
        // never reaches here.
        Ok(self.emit_cur(Inst::MakeStruct {
            ty: Some(name),
            fields: out,
        }))
    }

    fn struct_field_names(&self, name: Ustr) -> Option<Vec<Ustr>> {
        let sid = *self.ws.struct_by_name.get(&name)?;
        let sd = &self.ws.modules[sid.module.index()].structs[sid.def];
        Some(sd.fields.iter().map(|f| f.name).collect())
    }

    /// Whether nominal `name` is a `value` struct (C-layout cdata).
    fn is_value_struct(&self, name: Ustr) -> bool {
        self.ws
            .struct_by_name
            .get(&name)
            .map(|sid| self.ws.modules[sid.module.index()].structs[sid.def].is_value)
            .unwrap_or(false)
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
        Ok(ValueRepr::Group { name: shape.name, parts })
    }

    /// True iff `func` carries the `@struct_expand` annotation.
    fn fn_has_struct_expand(&self, func: FuncRef) -> bool {
        self.ws.module(func.module).functions[func.func]
            .annotations
            .iter()
            .any(|a| a.name.name.as_str() == "struct_expand")
    }

    /// Expand a `@struct_expand fn f<T>(u: T) -> [(string, E)]` call into its
    /// literal pairs list. See the free helpers above and
    /// docs/pass-uniforms-design.md (Layer 2).
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
        let ValueRepr::Group { name: struct_name, parts } = repr else {
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
            let variant_v = self.make_single_payload_variant(enum_name, variant, payload)?;
            let key = self.emit_cur(Inst::Const(Lit::Str(*field)));
            let tup = self.emit_cur(Inst::MakeTuple(vec![key, variant_v]));
            tuples.push(tup);
        }
        Ok(ValueRepr::Scalar(self.emit_cur(Inst::MakeVec(tuples))))
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
            return Ok(self.pack(ValueRepr::Group { name: enum_name, parts }));
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
    fn flat_field_of(&self, base: Idx<Expr>, field: Ustr) -> Option<EnumFlatShape> {
        let TypeExpr::App { ctor: TypeCtor::Nominal(sname), .. } = self.expr_ty(base)? else {
            return None;
        };
        if self.is_value_struct(sname) {
            return None;
        }
        let sid = *self.ws.struct_by_name.get(&sname)?;
        let sd = &self.ws.modules[sid.module.index()].structs[sid.def];
        let fdef = sd.fields.iter().find(|f| f.name == field)?;
        let TypeExpr::App { ctor: TypeCtor::Nominal(ename), .. } = static_ty(self.ws, fdef.ty)? else {
            return None;
        };
        flat_enum_shape(self.ws, ename)
    }

    /// Same as [`Self::flat_field_of`] but for a known struct name + field (used at a
    /// struct literal, where the literal's nominal type is the struct being built).
    fn flat_field_of_struct(&self, sname: Ustr, field: Ustr) -> Option<EnumFlatShape> {
        if self.is_value_struct(sname) {
            return None;
        }
        let sid = *self.ws.struct_by_name.get(&sname)?;
        let sd = &self.ws.modules[sid.module.index()].structs[sid.def];
        let fdef = sd.fields.iter().find(|f| f.name == field)?;
        let TypeExpr::App { ctor: TypeCtor::Nominal(ename), .. } = static_ty(self.ws, fdef.ty)? else {
            return None;
        };
        flat_enum_shape(self.ws, ename)
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
            ValueRepr::Group { name, parts } if flat_enum_shape(self.ws, name).is_some() => {
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
        ValueRepr::Group { name: shape.name, parts }
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

    /// The collection kind of a base expression, from its inferred type — drives
    /// the index/length shape (decision 9). Defaults to `NativeArray`.
    fn coll_kind(&self, base: Idx<Expr>) -> ContainerKind {
        match self.expr_ty(base) {
            Some(TypeExpr::App {
                ctor: TypeCtor::Array,
                ..
            }) => ContainerKind::Vec,
            Some(TypeExpr::App {
                ctor: TypeCtor::Map,
                ..
            }) => ContainerKind::Map,
            Some(TypeExpr::App {
                ctor: TypeCtor::Nominal(name),
                ..
            }) if is_native_buffer(name.as_str()) => ContainerKind::NativeBuffer,
            _ => ContainerKind::NativeArray,
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
    /// non-Lua backends. See `docs/typed-mir-coercion-plan.md`.
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
            (Some(ValueRepr::Group { name: bn, parts }), _) => {
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

    /// `Some(n)` iff `e`'s inferred type is a (non-nullable) tuple of arity `n`.
    /// Used to route a tuple result across a call boundary (the call expr's own
    /// type IS the callee's return type, so direct and INDIRECT calls agree).
    /// `resolve_tyid` (not `resolve_ty`) so a `Nullable(Tuple)` doesn't peel to a
    /// bare tuple and get mis-splatted.
    fn expr_tuple_arity(&self, e: Idx<Expr>) -> Option<usize> {
        self.tyid_tuple_arity(self.resolve_tyid(*self.inf?.expr_types.get(e)?)?)
    }

    /// `Some(n)` iff `e` is a lambda whose RETURN type is a (non-nullable) tuple of
    /// arity `n` -- the lambda body then splats N values so an indirect call site
    /// captures them (multi-return ABI). Read off the lambda's inferred `fn(..) ->
    /// (..)` type so it agrees with the call site's `expr_tuple_arity`.
    fn lambda_ret_tuple_arity(&self, e: Idx<Expr>) -> Option<usize> {
        let ty = self.resolve_tyid(*self.inf?.expr_types.get(e)?)?;
        let TypeExpr::Function { ret, .. } = self.ty_node(ty)? else { return None };
        self.tyid_tuple_arity(self.resolve_tyid(ty.with_idx(ret))?)
    }

    fn tyid_tuple_arity(&self, ty: TyId) -> Option<usize> {
        match self.ty_node(ty)? {
            TypeExpr::Tuple { elements } => Some(elements.into_iter().count()),
            _ => None,
        }
    }

    /// `Some(n)` iff `e`'s type is a tuple OR a `(tuple)?` of arity `n` -- the
    /// nil-peeled form for safe-nav calls, whose result type is nullable but whose
    /// callee still splats the inner tuple.
    fn peeled_tuple_arity(&self, e: Idx<Expr>) -> Option<usize> {
        let ty = self.resolve_tyid(*self.inf?.expr_types.get(e)?)?;
        let inner = match self.ty_node(ty)? {
            TypeExpr::Nullable(i) => self.resolve_tyid(ty.with_idx(i))?,
            _ => ty,
        };
        self.tyid_tuple_arity(inner)
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

    fn ty_node(&self, ty: TyId) -> Option<TypeExpr> {
        match ty {
            TyId::Static(mid, idx) => Some(self.ws.module(mid).ty_arena[idx]),
            TyId::Local(idx) => Some(self.inf?.ty_arena[idx]),
        }
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
    /// a flat enum -- the shape flowing through a `yield*` delegation to a flat-enum
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

/// When `ty` resolves to `NativeArray<E>` with `E` a nullable type, returns true.
fn na_optional_elem(ws: &Workspace, inf: &BodyInference, ty: TyId) -> bool {
    let Some(ty) = resolve_tyid(ws, inf, ty) else { return false };
    let Some(TypeExpr::App { ctor: TypeCtor::Nominal(name), params }) = ty_node(ws, inf, ty)
    else {
        return false;
    };
    if name.as_str() != "NativeArray" {
        return false;
    }
    let Some(elem_idx) = params.into_iter().next() else { return false };
    let Some(elem) = resolve_tyid(ws, inf, ty.with_idx(elem_idx)) else { return false };
    matches!(ty_node(ws, inf, elem), Some(TypeExpr::Nullable(..)))
}

/// Scan module `mid`'s bodies for an expression whose inferred type is
/// `NativeArray<T?>`. `![T?]` is a bare Lua table; `#` / `ipairs` stop at the
/// first nil, so an embedded `null` silently truncates iteration -- use `[T?]`
/// (Vec, tracked `.len`) instead. bone rejects the annotated forms; an inferred
/// literal (`![1, null]`) slips through, so this is the emit-time gate. Returns
/// the first offending expression's span.
pub fn native_array_optional_elem(ws: &Workspace, mid: ModuleId) -> Option<Span> {
    let module = ws.module(mid);
    let ir = ws.inference(mid)?;
    for (owner, body) in module.bodies.iter() {
        let Some(inf) = ir.per_fn.get(owner) else { continue };
        for (e, tyid) in inf.expr_types.iter() {
            if na_optional_elem(ws, inf, *tyid) {
                return Some(body.expr_spans[e]);
            }
        }
    }
    None
}

// -- `@struct_expand` emit-time rejection (docs/pass-uniforms-design.md, Layer 2).
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
            if !f.annotations.iter().any(|a| a.name.name.as_str() == "struct_expand") {
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
mod tests {
    use super::*;

    /// Lower `fn_name` from `modules` (last = the user `<test>` module) and
    /// return its MIR dump.
    fn lower_dump(mut modules: Vec<skull_parser::Ast>, fn_name: &str) -> String {
        let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
        let mid = *ws
            .module_ids
            .get(&ustr::Ustr::from("<test>"))
            .expect("user module id");
        let m = ws.module(mid);
        let (fid, _) = m
            .functions
            .iter()
            .find(|(_, f)| f.name.as_str() == fn_name)
            .expect("function by name");
        lower_function(&ws, &ScalarCtx::new(&ws), mid, fid)
            .unwrap_or_else(|e| panic!("lower failed: {}", e.message))
            .to_string()
    }

    /// Stdlib-free: enough for literals, vars, `let`, and builtin operators.
    fn mir(src: &str, fn_name: &str) -> String {
        let ast = skull_parser::parse_module(src, ustr::Ustr::from("<test>")).expect("parse");
        lower_dump(vec![ast], fn_name)
    }

    /// Stdlib-loaded: needed for `__op_*` arithmetic overloads. The stdlib is
    /// parsed once (cached) and re-checked per call alongside the snippet.
    fn mir_std(src: &str, fn_name: &str) -> String {
        use std::sync::LazyLock;
        static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
            rock_host::stdlib_loader::stdlib_files()
                .iter()
                .map(|f| {
                    let id = ustr::Ustr::from(format!("<stdlib:{}>", f.name).as_str());
                    skull_parser::parse_module(f.contents, id).expect("stdlib parse")
                })
                .collect()
        });
        let mut modules = STDLIB.clone();
        modules.push(skull_parser::parse_module(src, ustr::Ustr::from("<test>")).expect("parse"));
        lower_dump(modules, fn_name)
    }

    #[test]
    fn value_struct_literal_scalarizes_at_boundary_else_cdata() {
        // Under the scalar value ABI a value RETURN flattens to leaf scalars — no
        // `V2_ct(...)` cdata is built at the boundary (the result is `return x, y`).
        let dump = mir_std(
            "value V2 { x: int, y: int }\n fn f() -> V2 { V2 { y: 7, x: 3 } }",
            "f",
        );
        assert!(!dump.contains("V2_ct"), "value return must scalarize (no cdata ctor), got:\n{dump}");
        assert!(!dump.contains("make_struct") && !dump.contains("__ty"), "value struct must not be a table:\n{dump}");
        // But an ESCAPING value literal (into a `[V2]` element) materializes the
        // `<name>_ct(...)` cdata via `pack` — value semantics for the collection.
        let esc = mir_std(
            "value V2 { x: int, y: int }\n fn g() -> [V2] { [V2 { y: 7, x: 3 }] }",
            "g",
        );
        // Display carries declared field names (for SROA): `V2_ct(x: v.., y: v..)`.
        assert!(esc.contains("V2_ct(x:"), "an escaping value literal must build a cdata ctor, got:\n{esc}");
    }

    #[test]
    fn compound_assign_to_native_array_index() {
        let dump = mir_std(
            "fn f() -> int { let arr = ![10, 20, 30]\n arr[1] += 5\n arr[1] }",
            "f",
        );
        assert!(dump.contains("store_index"), "expected a store_index, got:\n{dump}");
        assert!(dump.contains("int_add") || dump.contains("add"), "expected an add, got:\n{dump}");
    }

    #[test]
    fn compound_assign_to_struct_field() {
        let dump = mir_std(
            "struct P { x: int }\n fn f() -> int { let p = P { x: 1 }\n p.x += 9\n p.x }",
            "f",
        );
        assert!(dump.contains("store_field"), "expected a store_field, got:\n{dump}");
    }

    #[test]
    fn int_literal_and_return() {
        assert_eq!(
            mir("fn five() -> int { 5 }", "five"),
            "\
fn five() {
  bb0:
    v0 = const 5
    return v0
}"
        );
    }

    #[test]
    fn param_is_a_cell_read_via_load() {
        assert_eq!(
            mir("fn id(x: int) -> int { x }", "id"),
            "\
fn id(c0) {
  bb0:
    v0 = load c0
    return v0
}"
        );
    }

    #[test]
    fn let_bindings_store_to_cells() {
        assert_eq!(
            mir("fn f() -> int { let a = 1; let b = 2; a }", "f"),
            "\
fn f() {
  bb0:
    v0 = const 1
    store c0, v0
    v1 = const 2
    store c1, v1
    v2 = load c0
    return v2
}"
        );
    }

    #[test]
    fn int_arithmetic_dispatches_to_typed_op() {
        assert_eq!(
            mir_std("fn add(a: int, b: int) -> int { a + b }", "add"),
            "\
fn add(c0, c1) {
  bb0:
    v0 = load c0
    v1 = load c1
    v2 = int.add v0, v1
    return v2
}"
        );
    }

    #[test]
    fn float_arithmetic_dispatches_to_float_op() {
        assert_eq!(
            mir_std("fn addf(a: float, b: float) -> float { a + b }", "addf"),
            "\
fn addf(c0, c1) {
  bb0:
    v0 = load c0
    v1 = load c1
    v2 = float.add v0, v1
    return v2
}"
        );
    }

    #[test]
    fn string_plus_is_concat() {
        assert_eq!(
            mir_std("fn cat(a: string, b: string) -> string { a + b }", "cat"),
            "\
fn cat(c0, c1) {
  bb0:
    v0 = load c0
    v1 = load c1
    v2 = str.concat v0, v1
    return v2
}"
        );
    }

    #[test]
    fn int_comparison_is_typed() {
        assert_eq!(
            mir("fn lt(a: int, b: int) -> bool { a < b }", "lt"),
            "\
fn lt(c0, c1) {
  bb0:
    v0 = load c0
    v1 = load c1
    v2 = int.lt v0, v1
    return v2
}"
        );
    }

    #[test]
    fn direct_call_resolves_to_a_funcref() {
        // user module is mid 1 (builtin is 0); `g` is fn 0, `f` is fn 1, so the
        // call to `g` carries FuncRef 1:0 — a (module, fn) ref, NOT a string.
        assert_eq!(
            mir("fn g() -> int { 7 }\nfn f() -> int { g() }", "f"),
            "\
fn f() {
  bb0:
    v0 = call 1:0()
    return v0
}"
        );
    }

    #[test]
    fn calling_a_fn_typed_param_is_call_value() {
        assert_eq!(
            mir("fn ap(f: fn() -> int) -> int { f() }", "ap"),
            "\
fn ap(c0) {
  bb0:
    v0 = load c0
    v1 = call_value v0()
    return v1
}"
        );
    }

    // --- Phase 2: control flow as branches + cells ---

    /// P1, the central bet: `if`-as-value joins through a result **cell** (store
    /// in each arm, load in the join) with NO block params. This lowering output
    /// is byte-identical to the Phase-0 hand-built `sign` shape contract
    /// (`skull-mir/tests/shape.rs`) — RED->GREEN proof the design holds.
    #[test]
    fn if_as_value_joins_through_a_cell() {
        assert_eq!(
            mir("fn sign(x: int) -> int { if x < 0 { 0 } else { 1 } }", "sign"),
            "\
fn sign(c0) {
  bb0:
    v0 = load c0
    v1 = const 0
    v2 = int.lt v0, v1
    branch v2 -> bb1, bb2
  bb1:
    v3 = const 0
    store c1, v3
    goto bb3
  bb2:
    v4 = const 1
    store c1, v4
    goto bb3
  bb3:
    v5 = load c1
    return v5
}"
        );
    }

    #[test]
    fn and_short_circuits_through_a_cell() {
        assert_eq!(
            mir("fn f(a: bool, b: bool) -> bool { a && b }", "f"),
            "\
fn f(c0, c1) {
  bb0:
    v0 = load c0
    branch v0 -> bb1, bb2
  bb1:
    v1 = load c1
    store c2, v1
    goto bb3
  bb2:
    v2 = const false
    store c2, v2
    goto bb3
  bb3:
    v3 = load c2
    return v3
}"
        );
    }

    #[test]
    fn or_short_circuits_through_a_cell() {
        assert_eq!(
            mir("fn f(a: bool, b: bool) -> bool { a || b }", "f"),
            "\
fn f(c0, c1) {
  bb0:
    v0 = load c0
    branch v0 -> bb1, bb2
  bb1:
    v1 = const true
    store c2, v1
    goto bb3
  bb2:
    v2 = load c1
    store c2, v2
    goto bb3
  bb3:
    v3 = load c2
    return v3
}"
        );
    }

    #[test]
    fn null_coalesce_evaluates_lhs_once() {
        assert_eq!(
            mir("fn f(a: int?) -> int { a ?? 0 }", "f"),
            "\
fn f(c0) {
  bb0:
    v0 = load c0
    v1 = const nil
    v2 = eq v0, v1
    branch v2 -> bb1, bb2
  bb1:
    v3 = const 0
    store c1, v3
    goto bb3
  bb2:
    store c1, v0
    goto bb3
  bb3:
    v4 = load c1
    return v4
}"
        );
    }

    #[test]
    fn while_loop_is_header_body_backedge_exit() {
        assert_eq!(
            mir("fn f(c: bool) { while c { } }", "f"),
            "\
fn f(c0) {
  bb0:
    goto bb1
  bb1:
    v0 = load c0
    branch v0 -> bb2, bb3
  bb2:
    goto bb1
  bb3:
    return
}"
        );
    }

    #[test]
    fn for_range_has_header_body_latch_exit() {
        // loop var `i` and bound are cells; the latch increments so `continue`
        // still steps. No block params.
        assert_eq!(
            mir("fn f(n: int) { for i in 0..n { } }", "f"),
            "\
fn f(c0) {
  bb0:
    v0 = const 0
    store c1, v0
    v1 = load c0
    store c2, v1
    goto bb1
  bb1:
    v2 = load c1
    v3 = load c2
    v4 = int.lt v2, v3
    branch v4 -> bb2, bb4
  bb2:
    goto bb3
  bb3:
    v5 = load c1
    v6 = const 1
    v7 = int.add v5, v6
    store c1, v7
    goto bb1
  bb4:
    return
}"
        );
    }

    // --- Phase 2.5: aggregates + container reads ---

    #[test]
    fn vec_literal_builds_a_vec() {
        assert_eq!(
            mir("fn f() -> [int] { [1, 2, 3] }", "f"),
            "\
fn f() {
  bb0:
    v0 = const 1
    v1 = const 2
    v2 = const 3
    v3 = vec [v0, v1, v2]
    return v3
}"
        );
    }

    #[test]
    fn native_array_literal_builds_an_array() {
        assert_eq!(
            mir("fn f() -> ![int] { ![1, 2] }", "f"),
            "\
fn f() {
  bb0:
    v0 = const 1
    v1 = const 2
    v2 = array [v0, v1]
    return v2
}"
        );
    }

    #[test]
    fn tuple_return_splats_multi_value() {
        // A tuple-return fn splats its N elements (multi-return ABI) instead of
        // returning one heap table. The `tuple` inst here is dead (elided from the
        // return) and DCE'd by `cleanup` before codegen.
        assert_eq!(
            mir("fn f() -> (int, bool) { (1, true) }", "f"),
            "\
fn f() {
  bb0:
    v0 = const 1
    v1 = const true
    v2 = tuple [v0, v1]
    return v0, v1
}"
        );
    }

    #[test]
    fn vec_index_carries_container_kind() {
        assert_eq!(
            mir("fn f(xs: [int]) -> int { xs[0] }", "f"),
            "\
fn f(c0) {
  bb0:
    v0 = load c0
    v1 = const 0
    v2 = index.Vec v0[v1]
    return v2
}"
        );
    }

    #[test]
    fn tuple_index_projects() {
        assert_eq!(
            mir("fn f(t: (int, bool)) -> int { t.0 }", "f"),
            "\
fn f(c0) {
  bb0:
    v0 = load c0
    v1 = tuple_index v0.0
    return v1
}"
        );
    }

    #[test]
    fn enum_unit_variant() {
        // A payload-free enum is just its integer tag: the unit variant lowers
        // to a plain int const (Red = 0), not an interned singleton table.
        assert_eq!(
            mir("enum Color { Red, Green }\nfn f() -> Color { Color::Red }", "f"),
            "\
fn f() {
  bb0:
    v0 = const 0
    return v0
}"
        );
    }

    #[test]
    fn enum_tuple_variant_carries_payload() {
        // A scalar-payload enum scalarizes to (tag, _0, _1). The widest variant
        // Square has two fields, so Circle(5) is (0, 5, 0) -- the unused _1 slot
        // is zero-filled.
        assert_eq!(
            mir(
                "enum Shape { Circle(int), Square(int, int) }\nfn f() -> Shape { Shape::Circle(5) }",
                "f"
            ),
            "\
fn f() {
  bb0:
    v0 = const 0
    v1 = const 5
    v2 = const 0
    return v0, v1, v2
}"
        );
    }

    #[test]
    fn struct_literal_in_declared_field_order() {
        assert_eq!(
            mir(
                "struct Point { x: int, y: int }\nfn f() -> Point { Point { x: 1, y: 2 } }",
                "f"
            ),
            "\
fn f() {
  bb0:
    v0 = const 1
    v1 = const 2
    v2 = struct Point {x=v0, y=v1}
    return v2
}"
        );
    }

    #[test]
    fn struct_literal_injects_omitted_field_default() {
        // `y` is omitted; its `= 0` default is lowered from the StructDefaults
        // body inline at the literal site.
        assert_eq!(
            mir(
                "struct P { x: int, y: int = 0 }\nfn f() -> P { P { x: 1 } }",
                "f"
            ),
            "\
fn f() {
  bb0:
    v0 = const 1
    v1 = const 0
    v2 = struct P {x=v0, y=v1}
    return v2
}"
        );
    }

    #[test]
    fn struct_field_access() {
        assert_eq!(
            mir(
                "struct Point { x: int, y: int }\nfn f(p: Point) -> int { p.x }",
                "f"
            ),
            "\
fn f(c0) {
  bb0:
    v0 = load c0
    v1 = field v0.x
    return v1
}"
        );
    }

    #[test]
    fn for_over_array_indexes_each_element() {
        // Native array: the builtin counted template. (`[T]` routes through
        // the stdlib VecIter protocol instead and never reaches this path.)
        assert_eq!(
            mir("fn f(xs: ![int]) { for x in xs { } }", "f"),
            "\
fn f(c0) {
  bb0:
    v0 = load c0
    v1 = len.NativeArray v0
    v2 = const 0
    store c1, v2
    goto bb1
  bb1:
    v3 = load c1
    v4 = int.lt v3, v1
    branch v4 -> bb2, bb4
  bb2:
    v5 = load c1
    v6 = index.NativeArray v0[v5]
    store c2, v6
    goto bb3
  bb3:
    v7 = load c1
    v8 = const 1
    v9 = int.add v7, v8
    store c1, v9
    goto bb1
  bb4:
    return
}"
        );
    }

    // --- Phase 2.5 long tail: assignment targets ---

    #[test]
    fn reassignment_stores_to_the_cell() {
        assert_eq!(
            mir("fn f() -> int { let x = 1; x = 2; x }", "f"),
            "\
fn f() {
  bb0:
    v0 = const 1
    store c0, v0
    v1 = const 2
    store c0, v1
    v2 = load c0
    return v2
}"
        );
    }

    #[test]
    fn field_assignment_is_store_field() {
        assert_eq!(
            mir("struct P { x: int, y: int }\nfn f(p: P) -> int { p.x = 5; p.x }", "f"),
            "\
fn f(c0) {
  bb0:
    v0 = load c0
    v1 = const 5
    store_field v0.x, v1
    v2 = load c0
    v3 = field v2.x
    return v3
}"
        );
    }

    #[test]
    fn index_assignment_is_store_index() {
        assert_eq!(
            mir("fn f(xs: [int]) -> int { xs[0] = 9; xs[0] }", "f"),
            "\
fn f(c0) {
  bb0:
    v0 = load c0
    v1 = const 0
    v2 = const 9
    store_index.Vec v0[v1], v2
    v3 = load c0
    v4 = const 0
    v5 = index.Vec v3[v4]
    return v5
}"
        );
    }

    #[test]
    fn global_assignment_is_store_global() {
        assert_eq!(
            mir("global G: int = 0\nfn f() -> int { G = 5; G }", "f"),
            "\
fn f() {
  bb0:
    v0 = const 5
    store_global G, v0
    v1 = global G
    return v1
}"
        );
    }

    #[test]
    fn compound_assignment_reads_ops_writes_back() {
        assert_eq!(
            mir_std("fn f(n: int) -> int { let x = 0; x += n; x }", "f"),
            "\
fn f(c0) {
  bb0:
    v0 = const 0
    store c1, v0
    v1 = load c1
    v2 = load c0
    v3 = int.add v1, v2
    store c1, v3
    v4 = load c1
    return v4
}"
        );
    }

    #[test]
    fn match_enum_tests_tag_and_binds_payload_through_cells() {
        // A scalar-payload enum is cdata: passed as (tag, _0) scalars,
        // reconstructed via `Opt_ct`, then matched by int-tag compare
        // (Some = 0, None = 1) reading the `_0` payload field.
        assert_eq!(
            mir(
                "enum Opt { Some(int), None }\nfn f(o: Opt) -> int { match o { Opt::Some(n) => n, Opt::None => 0 } }",
                "f"
            ),
            "\
fn f(c0, c1) {
  bb0:
    v0 = load c0
    v1 = load c1
    v2 = Opt_ct(__tag: v0, _0: v1)
    v3 = field v2.__tag
    v4 = const 0
    v5 = eq v3, v4
    branch v5 -> bb3, bb2
  bb1:
    v6 = load c2
    return v6
  bb2:
    v7 = field v2.__tag
    v8 = const 1
    v9 = eq v7, v8
    branch v9 -> bb5, bb4
  bb3:
    v10 = field v2._0
    store c3, v10
    v11 = load c3
    store c2, v11
    goto bb1
  bb4:
    unreachable
  bb5:
    v12 = const 0
    store c2, v12
    goto bb1
}"
        );
    }

    #[test]
    fn match_nested_enum_gates_child_read_behind_parent_tag() {
        // `Some(Circle(r))`: the inner Circle tag test must be GATED behind the
        // outer Some tag (parent tag before any child read) — a cont block after
        // each tag test. This is what the single-level version couldn't do.
        //
        // The outer Opt rides the flat-enum param ABI: tag leaf `c0` (variant
        // name string) + payload slot `c1` (the Shape, cdata in the dynamic
        // slot). The gate: bb3 (inner `.__tag` int compare) and bb4 (`._0`
        // bind) are reachable only through the outer "Some" branch; the
        // wildcard arm bb2 is the fallthrough of BOTH tag tests.
        assert_eq!(
            mir(
                "enum Shape { Circle(int), Square(int) }\nenum Opt { Some(Shape), None }\nfn f(o: Opt) -> int { match o { Opt::Some(Shape::Circle(r)) => r, _ => 0 } }",
                "f",
            ),
            "\
fn f(c0, c1) {
  bb0:
    v0 = load c0
    v1 = load c1
    v2 = const \"Some\"
    v3 = eq v0, v2
    branch v3 -> bb3, bb2
  bb1:
    v4 = load c2
    return v4
  bb2:
    v5 = const 0
    store c2, v5
    goto bb1
  bb3:
    v6 = field v1.__tag
    v7 = const 0
    v8 = eq v6, v7
    branch v8 -> bb4, bb2
  bb4:
    v9 = field v1._0
    store c3, v9
    v10 = load c3
    store c2, v10
    goto bb1
  bb5:
    unreachable
}"
        );
    }

    #[test]
    fn if_let_binds_on_match_else_branch_otherwise() {
        // Scalar-payload enum: cdata reconstructed from (tag, _0) scalars; the
        // if-let is an int-tag compare (Some = 0) binding the `_0` payload.
        assert_eq!(
            mir(
                "enum Opt { Some(int), None }\nfn f(o: Opt) -> int { if let Opt::Some(n) = o { n } else { 0 } }",
                "f"
            ),
            "\
fn f(c0, c1) {
  bb0:
    v0 = load c0
    v1 = load c1
    v2 = Opt_ct(__tag: v0, _0: v1)
    v3 = field v2.__tag
    v4 = const 0
    v5 = eq v3, v4
    branch v5 -> bb3, bb1
  bb1:
    v6 = const 0
    store c2, v6
    goto bb2
  bb2:
    v7 = load c2
    return v7
  bb3:
    v8 = field v2._0
    store c3, v8
    v9 = load c3
    store c2, v9
    goto bb2
}"
        );
    }

    #[test]
    fn for_over_map_uses_next_iteration() {
        assert_eq!(
            mir("fn f(m: Map<int, int>) { for (k, v) in m { } }", "f"),
            "\
fn f(c0) {
  bb0:
    v0 = load c0
    v1 = const nil
    store c1, v1
    goto bb1
  bb1:
    c1, c2 = next(v0)
    v2 = load c1
    v3 = const nil
    v4 = eq v2, v3
    branch v4 -> bb3, bb2
  bb2:
    v5 = load c1
    store c3, v5
    v6 = load c2
    store c4, v6
    goto bb1
  bb3:
    return
}"
        );
    }

    #[test]
    fn for_enumerate_binds_index_and_element() {
        assert_eq!(
            mir_std("fn f(xs: [int]) { for (i, x) in enumerate(xs) { } }", "f"),
            "\
fn f(c0) {
  bb0:
    v0 = load c0
    v1 = len.Vec v0
    v2 = const 0
    store c1, v2
    goto bb1
  bb1:
    v3 = load c1
    v4 = int.lt v3, v1
    branch v4 -> bb2, bb4
  bb2:
    v5 = load c1
    v6 = index.Vec v0[v5]
    store c2, v6
    goto bb3
  bb3:
    v7 = load c1
    v8 = const 1
    v9 = int.add v7, v8
    store c1, v9
    goto bb1
  bb4:
    return
}"
        );
    }

    #[test]
    fn break_jumps_to_loop_exit() {
        assert_eq!(
            mir("fn f(c: bool) { while c { break } }", "f"),
            "\
fn f(c0) {
  bb0:
    goto bb1
  bb1:
    v0 = load c0
    branch v0 -> bb2, bb3
  bb2:
    goto bb3
  bb3:
    return
}"
        );
    }
}
