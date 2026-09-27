//! Static analysis of value- and enum-struct REPRESENTATION over a `Workspace`:
//! leaf shapes and their C layout, column eligibility, the specialization key and
//! its reified types, return/bound-`next` shapes, and the GC-vs-cdata-vs-leaf-table
//! tier decisions every emitter consults.
//!
//! Nothing here builds MIR. It answers "what does this type look like", which is
//! why four other crates depend on it: the physical value-storage plan is shared
//! by the ABI planner, pack sites and backend scaffolding, so the write and read
//! encodings cannot diverge.

use super::*;

use bone_semantic::hir::{BuiltinTy, Expr, TyId, TypeCtor, TypeExpr, VariantPayload};
use bone_semantic::inference::Workspace;
use comfy_la_arena::Idx;
use ustr::Ustr;

use crate::concrete::ConcreteSignature;
use crate::monoty::{MonoCtor, MonoTy};

pub fn value_struct_all_float_leaf_count(ws: &Workspace, name: Ustr) -> Option<usize> {
    all_float_leaf_count_rec(ws, name, &mut Vec::new())
}

pub(crate) fn all_float_leaf_count_rec(ws: &Workspace, name: Ustr, visiting: &mut Vec<Ustr>) -> Option<usize> {
    let sid = *ws.struct_by_name.get(&name)?;
    let sd = &ws.modules[sid.module.index()].structs[sid.def];
    if !sd.is_value || visiting.contains(&name) {
        return None;
    }
    visiting.push(name);
    let mut total = 0usize;
    for f in &sd.fields {
        let n = match static_ty(ws, f.ty)? {
            TypeExpr::Builtin(BuiltinTy::Float) => 1,
            TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } => {
                match all_float_leaf_count_rec(ws, n, visiting) {
                    Some(k) => k,
                    None => {
                        visiting.pop();
                        return None;
                    }
                }
            }
            _ => {
                visiting.pop();
                return None;
            }
        };
        total += n;
    }
    visiting.pop();
    Some(total)
}

/// One immediate field of an all-float value struct in its flattened float-slot
/// layout: `offset` in float units from the struct's start, and whether it is a
/// float leaf or a nested value struct (named, for recursive descent).
pub struct ValueSlotField {
    pub name: Ustr,
    pub offset: usize,
    /// `None` = float leaf (width 1); `Some(n)` = nested value struct `n`.
    pub nested: Option<Ustr>,
}

/// Immediate fields of all-float value STRUCT `name` with their flattened
/// float-slot offsets, declaration order (the C layout / Float32Array order).
/// `None` unless `name` is a value struct proper whose every leaf is float --
/// the exact set `FfiNewArray` lowers to a flat `Float32Array` on JS.
pub fn value_struct_slot_fields(ws: &Workspace, name: Ustr) -> Option<Vec<ValueSlotField>> {
    value_struct_all_float_leaf_count(ws, name)?;
    let sid = *ws.struct_by_name.get(&name)?;
    let sd = &ws.modules[sid.module.index()].structs[sid.def];
    let mut out = Vec::with_capacity(sd.fields.len());
    let mut offset = 0usize;
    for f in &sd.fields {
        match static_ty(ws, f.ty)? {
            TypeExpr::Builtin(BuiltinTy::Float) => {
                out.push(ValueSlotField { name: f.name, offset, nested: None });
                offset += 1;
            }
            TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } => {
                let w = value_struct_all_float_leaf_count(ws, n)?;
                out.push(ValueSlotField { name: f.name, offset, nested: Some(n) });
                offset += w;
            }
            _ => return None,
        }
    }
    Some(out)
}

/// Max leaf count that stores an unmarked value in columns. Above it, columns
/// fall off a measured write cliff (9.3x on LuaJIT / 9.1x on V8 at K=16, x86;
/// 17.1x / 11.7x on arm64), so an unmarked wider type stays boxed. An explicit
/// `nobox` guarantee bypasses this heuristic because boxing is not a legal
/// fallback. Numbers + the AoS tier: `docs/vec-value-columns-plan.md`.
pub(crate) const MAX_COLUMN_LEAVES: usize = 4;

pub(crate) fn dense_flat_enum_vec_eligible(ws: &Workspace, name: Ustr) -> bool {
    fn dense_payload_ty(ws: &Workspace, ty: TyId, visiting: &mut Vec<Ustr>) -> bool {
        match static_ty(ws, ty) {
            Some(TypeExpr::Builtin(BuiltinTy::Int | BuiltinTy::Uint | BuiltinTy::Float)) => true,
            Some(TypeExpr::App {
                ctor: TypeCtor::Nominal(child),
                ..
            }) if payload_free_enum_variants(ws, child).is_some() => true,
            Some(TypeExpr::App {
                ctor: TypeCtor::Nominal(child),
                ..
            }) => {
                let Some(sid) = ws.struct_by_name.get(&child) else {
                    return false;
                };
                let decl = &ws.modules[sid.module.index()].structs[sid.def];
                if !decl.is_value || visiting.contains(&child) {
                    return false;
                }
                visiting.push(child);
                let dense = decl
                    .fields
                    .iter()
                    .all(|field| dense_payload_ty(ws, field.ty, visiting));
                visiting.pop();
                dense
            }
            _ => false,
        }
    }

    let Some(plan) = EnumPlan::for_name(ws, name) else {
        return false;
    };
    let Some(shape) = plan.flat() else {
        return false;
    };
    if shape.leaf_count() <= MAX_COLUMN_LEAVES {
        return false;
    }
    let Some(eid) = ws.enums_by_name.get(&name) else {
        return false;
    };
    ws.modules[eid.module.index()].enums[eid.def]
        .variants
        .iter()
        .all(|variant| match &variant.payload {
            VariantPayload::Unit => true,
            VariantPayload::Tuple(tys) => tys
                .iter()
                .all(|ty| dense_payload_ty(ws, *ty, &mut vec![name])),
            VariantPayload::Record(_) => false,
        })
}

/// The column KEYS of `shape`, presence first when the element type is nullable.
///
/// This is [`value_leaf_paths`] plus the one key that has no path of its own: a
/// TOP-LEVEL nullable shape's presence leaf is not addressable as `v.<field>`,
/// so `value_leaf_paths` deliberately omits it while `leaf_count` counts it.
/// Column storage is the one consumer that must NAME it -- it needs a table key
/// per leaf -- and it names it here rather than in `leaf_paths_rec`, whose
/// result the global/reflection accessors read positionally against that same
/// omission.
pub fn column_leaf_paths(shape: &ValueShape) -> Vec<Ustr> {
    let paths = value_leaf_paths(shape);
    if !shape.nullable {
        return paths;
    }
    let mut out = Vec::with_capacity(paths.len() + 1);
    out.push(Ustr::from(VEC_COLUMN_PRESENCE_FIELD));
    out.extend(paths);
    out
}

/// Every leaf path of value shape `shape`, declaration order, dotted for nested
/// value fields (`min.x`). These ARE the column keys. Dotted for the same
/// reason `embed` storage is: `.` cannot appear in a Rock identifier, so the
/// encoding is injective and can never collide with a sibling leaf.
pub fn value_leaf_paths(shape: &ValueShape) -> Vec<Ustr> {
    leaf_names(shape, "", ".").into_iter().map(|s| Ustr::from(&s)).collect()
}

/// Which entries of [`value_leaf_paths`] are a nullable child's PRESENCE tag
/// rather than a data leaf, positionally. Storage canonicalizes an absent tag to
/// `nil` (the form a `= null` write and an absent literal both produce), so the
/// ctor has to know which of its params is a tag; a data leaf must never be
/// touched, or a `false` bool field would erase itself.
pub fn value_leaf_presence_flags(shape: &ValueShape) -> Vec<bool> {
    let mut out = Vec::new();
    presence_flags_rec(shape, false, &mut out);
    out
}

pub(crate) fn presence_flags_rec(shape: &ValueShape, nested: bool, out: &mut Vec<bool>) {
    if shape.nullable && nested {
        out.push(true);
    }
    for (_, fs) in &shape.fields {
        match fs {
            FieldShape::Prim => out.push(false),
            FieldShape::Nested(sub) => presence_flags_rec(sub, true, out),
        }
    }
}

/// [`value_leaf_presence_flags`] by type name.
/// THE leaf traversal: every primitive leaf of `shape` in declaration order,
/// each named by joining its ancestor field names with `sep` under `prefix`.
/// `sep = "."` yields the column keys / dotted source paths, `sep = "__"` the
/// Lua leaf-param names -- one walk so the two encodings can never disagree on
/// WHICH leaves exist or in what order (`repr_to_leaves`/`leaf_count` are the
/// third and fourth consumers of that order). A leaf's identity is its position;
/// only the spelling is per-caller.
pub(crate) fn leaf_names(shape: &ValueShape, prefix: &str, sep: &str) -> Vec<String> {
    let mut out = Vec::new();
    leaf_names_rec(shape, prefix, sep, &mut out);
    out
}

pub(crate) fn leaf_names_rec(shape: &ValueShape, prefix: &str, sep: &str, out: &mut Vec<String>) {
    // A NULLABLE nested shape leads with its presence leaf, matching
    // `repr_to_leaves` (which pushes presence before the fields) and `leaf_count`
    // (which counts it). Its key is the field's own name with no child appended
    // -- injective against its own children under `.` (`a` vs `a.x`), which is
    // the convention embed storage already uses for a nullable child. Without it
    // the column set is one short of `leaf_count` and a `[Outer]` literal fails
    // with a column arity mismatch, and the Lua params drift one short of what
    // `repr_to_leaves` splats.
    //
    // Only the NESTED case: a top-level nullable shape has no field name to key
    // on, and no column path reaches one today (a column element is
    // non-nullable), so that case is deliberately left as it was.
    if shape.nullable && !prefix.is_empty() {
        out.push(prefix.to_string());
    }
    for (fname, fs) in &shape.fields {
        // The field name must JOIN its ancestors. Recursing with the prefix
        // unchanged made the Lua namer non-injective -- `Rect { tl: V2, br: V2 }`
        // emitted `f(r__x, r__y, r__x, r__y)`, and Lua binds the later pair over
        // the earlier, so a hand-written body silently read `br` where it wrote
        // `tl`.
        let joined =
            if prefix.is_empty() { fname.to_string() } else { format!("{prefix}{sep}{fname}") };
        match fs {
            FieldShape::Prim => out.push(joined),
            FieldShape::Nested(sub) => leaf_names_rec(sub, &joined, sep, out),
        }
    }
}

/// Every leaf VIEW the compiler mints: `(leaf C type, buffer field, cast type)`.
///
/// THE single source of truth. [`leaf_view_field`] selects from it,
/// [`is_leaf_view_field`] tests it, and the runtime `__ROCK_LEAF_VIEWS` table is
/// GENERATED from it (`skull::prelude`) so the Rock-side consumers -- view
/// refresh, serialization, inspect -- cannot drift from the emitter. Adding a
/// leaf type here reaches all of them; a hand-written list in any of them is the
/// bug this replaces.
pub const LEAF_VIEWS: &[(&str, &str, &str)] = &[
    ("float", "__vf", "float*"),
    ("int", "__vi", "int*"),
    ("uint32_t", "__vu", "uint32_t*"),
    ("bool", "__vb", "bool*"),
];

/// The buffer-table field holding the typed leaf VIEW for primitive C type
/// `cty`. One field per distinct leaf type, made once at construction
/// ([`Inst::FfiViewCast`]) and read per element access, never per leaf.
pub fn leaf_view_field(cty: &str) -> Ustr {
    match LEAF_VIEWS.iter().find(|(t, ..)| *t == cty) {
        Some((_, field, _)) => Ustr::from(field),
        None => unreachable!("no leaf view for non-primitive C type `{cty}`"),
    }
}

/// Whether `field` is one of the leaf-view keys [`leaf_view_field`] mints.
pub fn is_leaf_view_field(field: &str) -> bool {
    LEAF_VIEWS.iter().any(|(_, f, _)| *f == field)
}

/// The `__ROCK_LEAF_VIEWS` prelude table: leaf-view field -> `ffi.cast` type.
/// Rock-side consumers key off it instead of spelling `__vf`/`__vi`/`__vb`.
pub fn leaf_views_lua_table() -> String {
    let entries: Vec<String> = LEAF_VIEWS
        .iter()
        .map(|(_, field, cast)| format!("{field} = \"{cast}\""))
        .collect();
    format!("{{ {} }}", entries.join(", "))
}

/// Where one leaf lives, in units of ITS OWN view's element size: the buffer
/// field holding that view, the per-element stride, and the leaf's offset within
/// an element. Both numbers are compile-time constants, so a leaf access is
/// `view[i * stride + offset]` -- pure arithmetic, no allocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeafAddr {
    pub path: Ustr,
    pub view_field: Ustr,
    pub stride: i64,
    pub offset: i64,
}

/// The leaf addressing plan for FFI-clean value struct `elem`: one [`LeafAddr`]
/// per leaf, in [`value_leaf_paths`] order (the order reprs are flattened in).
///
/// The unit conversions are exact by construction: `sizeof(elem)` is a multiple
/// of `elem`'s alignment, which is the max leaf size, and every leaf size is a
/// power of two no larger than that; a leaf's own byte offset is a multiple of
/// its size because C aligns each member. Both divisions are checked anyway --
/// an inexact one would silently address the wrong element.
/// The C type a leaf VIEW field carries -- the inverse of [`leaf_view_field`].
/// The JS emitter needs it to pick the TypedArray and to know a `bool` view
/// stores 0/1 where the language wants `true`/`false`.
pub fn leaf_view_cty(field: &str) -> Option<&'static str> {
    LEAF_VIEWS.iter().find(|(_, f, _)| *f == field).map(|(t, ..)| *t)
}

/// Every element type a `NativeBuffer`'s storage can be allocated with that is
/// NOT a value struct: the primitives `native_buffer_alloc<T>` accepts and the
/// sized phantoms (`NativeBufferU8` ...) the stdlib's `__nb_alloc_*` ctors
/// build. Entries are `(C type, byte width)`.
///
/// Separate from [`c_prim_size_align`], which answers for a value struct's
/// LEAVES -- `uint8_t` is never a leaf but is an element in its own right.
pub const NB_PRIM_ELEMS: &[(&str, usize)] = &[
    ("float", 4),
    ("int", 4),
    ("uint32_t", 4),
    ("bool", 1),
    ("uint8_t", 1),
    ("int8_t", 1),
    ("uint16_t", 2),
    ("int16_t", 2),
];

/// One `_G.__ROCK_NB_STORAGE` registration: `__rock_nb_storage(<ctype>, <byte
/// width>, { <view field> = "<cast type>", ... })`.
///
/// THE compiler-owned description of concrete buffer storage. A consumer that
/// holds a storage cdata asks the registry what its bytes mean -- element ctype,
/// element width, and the exact leaf views minted over it -- instead of guessing
/// from a `__ty` string or re-deriving a layout. The element widths and the view
/// set come from the same oracles the emitter itself uses
/// ([`NB_PRIM_ELEMS`], the owned `RepPlan` C layout, [`LEAF_VIEWS`]), so a
/// registry entry cannot describe a layout the emitter does not produce.
fn nb_storage_call(ctype: &str, size: usize, views: &[&str]) -> String {
    let views: Vec<String> = views
        .iter()
        .map(|cty| {
            let (_, field, cast) = LEAF_VIEWS
                .iter()
                .find(|(t, ..)| t == cty)
                .expect("a leaf view C type must be in LEAF_VIEWS");
            format!("{field} = \"{cast}\"")
        })
        .collect();
    format!("__rock_nb_storage(\"{ctype}\", {size}, {{ {} }})\n", views.join(", "))
}

/// The registrations for [`NB_PRIM_ELEMS`]. A primitive element carries no leaf
/// views -- its storage IS one C type, so `buf.ptr` already addresses it.
pub fn nb_prim_storage_lua() -> String {
    NB_PRIM_ELEMS.iter().map(|(cty, size)| nb_storage_call(cty, *size, &[])).collect()
}

/// The registration for FFI-clean value struct `elem` used as a buffer element,
/// derived from the layout its caller ALREADY validated. Takes the checked
/// leaves and size rather than re-walking: the emitter needs one layout per
/// struct and the registry is a second view of it, not a second computation.
pub fn nb_value_storage_lua(ws: &Workspace, elem: Ustr, leaves: &[LeafSlot], size: usize) -> String {
    // `__rock_nb_storage` runs `ffi.typeof` on this name: a C type, not a DefKey.
    let elem = crate::mangle::type_symbol(ws, elem);
    let mut views: Vec<&'static str> = Vec::new();
    for l in leaves {
        if !views.contains(&l.cty) {
            views.push(l.cty);
        }
    }
    nb_storage_call(elem.as_str(), size, &views)
}

/// One primitive leaf of an FFI-clean value struct's C layout: its dotted path
/// (the same key [`value_leaf_paths`] produces), its C type, and its BYTE offset
/// from the start of the struct.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeafSlot {
    pub path: Ustr,
    pub cty: &'static str,
    pub byte_off: usize,
    /// The same offset expressed as a LuaJIT `ffi.offsetof` composition along
    /// this leaf's field chain (`ffi.offsetof("Vert","pos")+ffi.offsetof("V2","x")`).
    /// Emitted as a start-up assertion against `byte_off`: the Rust layout model
    /// and LuaJIT's must agree, and a divergence corrupts memory silently rather
    /// than failing, so it is checked rather than trusted.
    pub offsetof_expr: String,
}

/// Size and alignment of a primitive C type as LuaJIT lays it out. The only
/// types [`value_field_c_type`] admits as leaves.
pub(crate) fn c_prim_size_align(cty: &str) -> Option<(usize, usize)> {
    match cty {
        "int" | "uint32_t" | "float" => Some((4, 4)),
        "bool" => Some((1, 1)),
        _ => None,
    }
}

/// The C layout of FFI-clean value struct `name`: every primitive leaf with its
/// byte offset, plus the struct's own `(size, align)`.
///
/// Standard C rules, matching what LuaJIT computes for the emitted typedef: each
/// member is padded up to its own alignment, the struct's alignment is the max
/// of its members', and its size is rounded up to that. The agreement is NOT
/// taken on trust -- `value_leaf_layout_matches_luajit` cross-checks every
/// offset against `ffi.offsetof` at run time, because a divergence here is
/// silent memory corruption rather than a failure.
///
/// PRIVATE on purpose: the raw walker is the bypass this module exists to
/// prevent. Its traversal mirrors the planned field tree arm for arm so the leaf
/// ORDER matches [`value_leaf_paths`], but that is two separate walkers staying
/// in step, NOT something this function enforces. Leaves are matched to offsets
/// POSITIONALLY, so if they drift, every runtime `ffi.offsetof` assertion still
/// passes (each path/offset pair is self-consistent) while repr leaves are
/// written to the wrong addresses.
///
/// `RepArena::nominal_leaf_layout` is the only public way out of this module,
/// and it rejects on drift. Keep the raw walker private so a future lowering
/// cannot reach around the representation plan.
pub(crate) fn value_leaf_layout(ws: &Workspace, name: Ustr) -> Option<(Vec<LeafSlot>, usize, usize)> {
    let plan = planned_value(ws, name)?;
    if plan.storage != ValueStorage::CData {
        return None;
    }
    let mut out = Vec::new();
    let (size, align) = planned_layout_rec(ws, &plan, "", 0, "", &mut out)?;
    Some((out, size, align))
}

fn planned_layout_rec(
    ws: &Workspace,
    plan: &PlannedValue,
    prefix: &str,
    base: usize,
    ofs: &str,
    out: &mut Vec<LeafSlot>,
) -> Option<(usize, usize)> {
    let mut off = base;
    let mut align = 1usize;
    for (field, planned) in &plan.fields {
        match planned {
            PlannedField::Primitive(cty) => {
                let (size, field_align) = c_prim_size_align(cty)?;
                off = round_up(off, field_align);
                out.push(LeafSlot {
                    path: leaf_path(prefix, field.as_str()),
                    cty,
                    byte_off: off,
                    offsetof_expr: join_offsetof(ws, ofs, plan.name, field.as_str()),
                });
                off += size;
                align = align.max(field_align);
            }
            PlannedField::Nested(child) => {
                let child_base = round_up(off, planned_alignment(child)?);
                let (size, child_align) = planned_layout_rec(
                    ws,
                    child,
                    &format!("{}{field}.", prefix),
                    child_base,
                    &join_offsetof(ws, ofs, plan.name, field.as_str()),
                    out,
                )?;
                off = child_base + size;
                align = align.max(child_align);
            }
            PlannedField::FlatEnum(_) | PlannedField::Nullable(_) | PlannedField::Opaque => {
                return None;
            }
        }
    }
    Some((round_up(off - base, align), align))
}

fn planned_alignment(plan: &PlannedValue) -> Option<usize> {
    plan.fields.iter().try_fold(1usize, |align, (_, field)| {
        let field_align = match field {
            PlannedField::Primitive(cty) => c_prim_size_align(cty)?.1,
            PlannedField::Nested(child) => planned_alignment(child)?,
            PlannedField::FlatEnum(_) | PlannedField::Nullable(_) | PlannedField::Opaque => {
                return None;
            }
        };
        Some(align.max(field_align))
    })
}

/// Extend an `ffi.offsetof` composition by one field hop. `owner` names a C
/// TYPE, so a v1 nominal contributes its emitted symbol, never its DefKey.
pub(crate) fn join_offsetof(ws: &Workspace, prefix: &str, owner: Ustr, field: &str) -> String {
    let owner = crate::mangle::type_symbol(ws, owner);
    let hop = format!("ffi.offsetof(\"{owner}\", \"{field}\")");
    if prefix.is_empty() {
        hop
    } else {
        format!("{prefix} + {hop}")
    }
}

pub(crate) fn leaf_path(prefix: &str, name: &str) -> Ustr {
    if prefix.is_empty() {
        Ustr::from(name)
    } else {
        Ustr::from(&format!("{prefix}{name}"))
    }
}

pub(crate) fn round_up(n: usize, align: usize) -> usize {
    n.div_ceil(align) * align
}

/// The static, recursive shape of a scalarizable `value` struct: declared fields,
/// each a primitive leaf or a nested (also-scalarizable) value struct.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ValueShape {
    pub(crate) name: Ustr,
    pub(crate) fields: Vec<(Ustr, FieldShape)>,
    pub(crate) nullable: bool,
}

/// Physical materialization selected for a value-shaped nominal. The target is
/// carried by the owning `RepPlan`; this enum is the operational vocabulary its
/// consumers use after planning.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ValueStorage {
    CData,
    GcTable,
    LeafTable,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum FieldShape {
    Prim,
    Nested(ValueShape),
}

impl ValueShape {
    /// Total primitive-leaf count — the scalar-ABI arity (params/returns/args).
    pub(crate) fn leaf_count(&self) -> usize {
        self.fields
            .iter()
            .map(|(_, fs)| match fs {
                FieldShape::Prim => 1,
                FieldShape::Nested(s) => s.leaf_count(),
            })
            .sum::<usize>()
            + usize::from(self.nullable)
    }
}

#[derive(Clone, Debug)]
pub(crate) enum PlannedField {
    Primitive(&'static str),
    Nested(Box<PlannedValue>),
    FlatEnum(ValueShape),
    Nullable(Box<PlannedValue>),
    Opaque,
}

/// One recursive nominal plan. The storage tier and both scalar/GC projections
/// are computed in this walk; later consumers only project this tree.
#[derive(Clone, Debug)]
pub(crate) struct PlannedValue {
    pub(crate) name: Ustr,
    pub(crate) storage: ValueStorage,
    pub(crate) fields: Vec<(Ustr, PlannedField)>,
    pub(crate) shape: Option<ValueShape>,
    pub(crate) gc_shape: Option<ValueShape>,
}

fn planned_shape(
    name: Ustr,
    fields: &[(Ustr, PlannedField)],
    gc: bool,
) -> Option<ValueShape> {
    let mut out = Vec::with_capacity(fields.len());
    for (field, planned) in fields {
        let shape = match planned {
            PlannedField::Primitive(_) => FieldShape::Prim,
            PlannedField::Opaque if gc => FieldShape::Prim,
            PlannedField::Opaque => return None,
            PlannedField::FlatEnum(shape) if gc => FieldShape::Nested(shape.clone()),
            PlannedField::FlatEnum(_) => return None,
            PlannedField::Nested(child) => {
                let child = if gc {
                    child.shape.clone().or_else(|| child.gc_shape.clone())
                } else {
                    child.shape.clone()
                }?;
                FieldShape::Nested(child)
            }
            PlannedField::Nullable(child) => {
                let mut child = if gc {
                    child.shape.clone().or_else(|| child.gc_shape.clone())
                } else {
                    child.shape.clone()
                }?;
                child.nullable = true;
                FieldShape::Nested(child)
            }
        };
        out.push((*field, shape));
    }
    Some(ValueShape { name, fields: out, nullable: false })
}

fn nominal_name_from_ty(ws: &Workspace, ty: TyId) -> Option<(Ustr, bool)> {
    let mut node = static_ty(ws, ty)?;
    let mut nullable = false;
    if let TypeExpr::Nullable(inner) = node {
        nullable = true;
        node = static_ty(ws, ty.with_idx(inner))?;
    }
    match node {
        TypeExpr::App { ctor: TypeCtor::Nominal(name), .. } => Some((name, nullable)),
        _ => None,
    }
}

fn planned_value_rec(
    ws: &Workspace,
    name: Ustr,
    visiting: &mut Vec<Ustr>,
) -> Option<PlannedValue> {
    if let Some(enum_plan) = EnumPlan::for_name(ws, name).and_then(|plan| plan.cdata().cloned()) {
        let mut fields = vec![(Ustr::from("__tag"), PlannedField::Primitive("int"))];
        fields.extend(
            enum_plan
                .slots
                .iter()
                .map(|(field, cty)| (*field, PlannedField::Primitive(*cty))),
        );
        return Some(PlannedValue {
            name,
            storage: ValueStorage::CData,
            shape: Some(enum_plan.value_shape()),
            gc_shape: None,
            fields,
        });
    }
    let sid = *ws.struct_by_name.get(&name)?;
    let sd = &ws.modules[sid.module.index()].structs[sid.def];
    if !sd.is_value || visiting.contains(&name) {
        return None;
    }
    visiting.push(name);
    let mut fields = Vec::with_capacity(sd.fields.len());
    let mut cdata = sd.generics.is_empty();
    let mut scalarizable = true;
    for field in &sd.fields {
        let planned = match static_ty(ws, field.ty) {
            Some(TypeExpr::Builtin(BuiltinTy::Int)) => PlannedField::Primitive("int"),
            Some(TypeExpr::Builtin(BuiltinTy::Uint)) => PlannedField::Primitive("uint32_t"),
            Some(TypeExpr::Builtin(BuiltinTy::Float)) => PlannedField::Primitive("float"),
            Some(TypeExpr::Builtin(BuiltinTy::Bool)) => PlannedField::Primitive("bool"),
            Some(TypeExpr::App { ctor: TypeCtor::Nominal(child), .. })
                if payload_free_enum_variants(ws, child).is_some() =>
            {
                PlannedField::Primitive("int")
            }
            Some(TypeExpr::App { ctor: TypeCtor::Nominal(child), .. }) => {
                if let Some(flat) =
                    EnumPlan::for_name(ws, child).and_then(|plan| plan.flat().cloned())
                {
                    cdata = false;
                    scalarizable = false;
                    fields.push((field.name, PlannedField::FlatEnum(flat.value_shape())));
                    continue;
                }
                let Some(child_plan) = planned_value_rec(ws, child, visiting) else {
                    cdata = false;
                    scalarizable = false;
                    fields.push((field.name, PlannedField::Opaque));
                    continue;
                };
                cdata &= child_plan.storage == ValueStorage::CData;
                scalarizable &= child_plan.shape.is_some();
                PlannedField::Nested(Box::new(child_plan))
            }
            Some(TypeExpr::Nullable(_)) => {
                let Some((child, true)) = nominal_name_from_ty(ws, field.ty) else {
                    cdata = false;
                    scalarizable = false;
                    fields.push((field.name, PlannedField::Opaque));
                    continue;
                };
                let Some(child_plan) = planned_value_rec(ws, child, visiting) else {
                    cdata = false;
                    scalarizable = false;
                    fields.push((field.name, PlannedField::Opaque));
                    continue;
                };
                let nobox = ws.struct_by_name.get(&child).is_some_and(|sid| {
                    ws.modules[sid.module.index()].structs[sid.def].is_nobox
                });
                if !nobox {
                    cdata = false;
                    scalarizable = false;
                    PlannedField::Opaque
                } else {
                    cdata = false;
                    scalarizable &= child_plan.shape.is_some();
                    PlannedField::Nullable(Box::new(child_plan))
                }
            }
            _ => {
                cdata = false;
                scalarizable = false;
                PlannedField::Opaque
            }
        };
        if matches!(planned, PlannedField::Opaque) {
            cdata = false;
        }
        fields.push((field.name, planned));
    }
    let shape = scalarizable.then(|| planned_shape(name, &fields, false)).flatten();
    let storage = if cdata {
        ValueStorage::CData
    } else if scalarizable && sd.generics.is_empty() {
        ValueStorage::LeafTable
    } else {
        ValueStorage::GcTable
    };
    let gc_shape = (storage != ValueStorage::CData)
        .then(|| planned_shape(name, &fields, true))
        .flatten();
    visiting.pop();
    Some(PlannedValue { name, storage, fields, shape, gc_shape })
}

pub(crate) fn planned_value(ws: &Workspace, name: Ustr) -> Option<PlannedValue> {
    planned_value_rec(ws, name, &mut Vec::new())
}

pub(crate) fn planned_value_contains_flat_enum(ws: &Workspace, name: Ustr) -> bool {
    fn contains(plan: &PlannedValue) -> bool {
        plan.fields.iter().any(|(_, field)| match field {
            PlannedField::FlatEnum(_) => true,
            PlannedField::Nested(child) | PlannedField::Nullable(child) => contains(child),
            PlannedField::Primitive(_) | PlannedField::Opaque => false,
        })
    }

    planned_value(ws, name).is_some_and(|plan| contains(&plan))
}

/// `Some(ordered variant names)` iff `name` is a PAYLOAD-FREE enum (every variant
/// `Unit`) — the tier-1 scalarizable case. The index is the variant's int tag.
pub(crate) fn payload_free_enum_variants(ws: &Workspace, name: Ustr) -> Option<Vec<Ustr>> {
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
pub(crate) fn payload_free_ordinal(ws: &Workspace, name: Ustr, variant: Ustr) -> Option<i64> {
    payload_free_enum_variants(ws, name)?
        .iter()
        .position(|v| *v == variant)
        .map(|p| p as i64)
}

/// Whether a box a pack mints is the ordinary implicit one or the box
/// `materialize(v)` asked for. The two RENDER identically; only the `nobox`
/// guarantee distinguishes them, and it does so through the instruction the
/// ctor lands in. Carried through the whole tree, so the nested boxes an
/// explicit box has to build to exist are authorized with it.
///
/// This is the mint-time half of the authorization; `authorize_nobox_box` is the
/// after-the-fact half, for a boundary (`@gpu_only`) whose arguments are already
/// lowered by the time the compiler decides to sanction their box.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BoxKind {
    Implicit,
    Explicit,
}

impl BoxKind {
    pub(crate) fn value_ctor(
        self,
        name: Ustr,
        representation: RepId,
        form: ValueCtorForm,
        fields: Vec<(Ustr, Idx<Inst>)>,
    ) -> Inst {
        match self {
            BoxKind::Implicit => Inst::MakeValueStruct { name, representation, form, fields },
            BoxKind::Explicit => {
                Inst::MaterializeValueStruct { name, representation, form, fields }
            }
        }
    }
}

/// The runtime carrier threaded through lowering: a value struct lives as an
/// unboxed `Group` of per-field reprs (recursive); everything else is a `Scalar`
/// (one already-lowered value — a primitive, or a boxed cdata).
#[derive(Clone)]
pub(crate) enum ValueRepr {
    Scalar(Idx<Inst>),
    Group {
        name: Ustr,
        parts: Vec<ValueRepr>,
        present: Option<Idx<Inst>>,
    },
    /// A tuple whose elements are still reprs -- produced only by a tuple return
    /// that splats (`TupleRetShape::splats`), so an element that arrived as
    /// leaves can reach a destructure without being packed. Packing one emits
    /// the ordinary `MakeTuple` of packed parts, which is how a whole-tuple use
    /// of a `nobox` element still reaches the guarantee's rejection.
    Tuple {
        parts: Vec<ValueRepr>,
        /// The declared return was `(..)?`: the presence bool that led the wire.
        /// Same convention as `Group::present`, one level up.
        present: Option<Idx<Inst>>,
    },
}

/// A runtime value together with the physical plan that authorized its shape.
/// `ValueRepr` remains the operational carrier; this bundle prevents a caller
/// from handing a representation to a consumer without stating which RepId it
/// conforms to.
#[derive(Clone)]
pub(crate) struct ValueBundle {
    representation: RepId,
    value: ValueRepr,
}

impl ValueBundle {
    pub(crate) fn new(representation: RepId, value: ValueRepr) -> Self {
        Self { representation, value }
    }

    pub(crate) fn representation(&self) -> RepId {
        self.representation
    }

    pub(crate) fn value(&self) -> &ValueRepr {
        &self.value
    }

    pub(crate) fn into_value(self) -> ValueRepr {
        self.value
    }

}

/// A safe-nav receiver after lowering: either an ordinary boxed value, or an
/// UNBOXED nullable value that carries its own presence tag.
///
/// The distinction exists because the two have different honest absence tests.
/// A box is absent when it is nil; an unboxed value is absent when its presence
/// tag says so, and packing it to ask the nil question instead builds a ctor
/// that is statically non-nil -- which deletes the absent arm outright.
#[derive(Clone)]
pub(crate) enum SafeNavRecv {
    Boxed(Idx<Inst>),
    Unboxed { repr: ValueRepr, present: Idx<Inst> },
}

#[derive(Clone, Copy)]
pub(crate) enum SpreadDest {
    Container(ContainerKind),
    Columns(ColumnElem),
    Strided(ColumnElem),
}

/// The result repr of a swizzle: one selected component is the bare scalar;
/// several become a fresh VecN Group in swizzle order.
pub(crate) fn swizzle_group(parts: Vec<ValueRepr>) -> ValueRepr {
    if parts.len() == 1 {
        parts.into_iter().next().expect("len checked")
    } else {
        let name = match parts.len() {
            2 => "Vec2",
            3 => "Vec3",
            _ => "Vec4",
        };
        ValueRepr::Group {
            name: Ustr::from(name),
            parts,
            present: None,
        }
    }
}

/// The `TypeExpr` of a `Static` TyId (declared types — struct fields, signature
/// params/returns are all static). `None` for a `Local`/inference TyId.
pub(crate) fn static_ty(ws: &Workspace, ty: TyId) -> Option<TypeExpr> {
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
pub(crate) fn value_shape(ws: &Workspace, name: Ustr) -> Option<ValueShape> {
    planned_value(ws, name)?.shape
}

/// The target/context-specific physical storage of a nominal value. This is
/// the one classification used by RepPlan projections and backend scaffolding;
/// callers must not rediscover `_ct`, `_vt` or `_lct` from field syntax.
pub(crate) fn declared_value_storage(
    ws: &Workspace,
    _target: bone_semantic::hir::FnBackend,
    name: Ustr,
) -> Option<ValueStorage> {
    planned_value(ws, name).map(|plan| plan.storage)
}

/// Declaration field order of `name` iff it is a GC-free value STRUCT proper.
pub fn gc_free_value_struct_field_order(ws: &Workspace, name: Ustr) -> Option<Vec<Ustr>> {
    ws.struct_by_name.get(&name)?;
    planned_value(ws, name)?.shape.map(|_| struct_field_names(ws, name))
}

/// The scalar-ABI shape of a GC-CARRYING value struct:
/// every declared field is ONE leaf -- a `Prim` for a scalar, generic `T`,
/// array/arena/map, or string (a single boxed slot), or `Nested` for a nested
/// value struct / cdata enum field (which threads its own leaves). Uniform
/// per-field slotting: the field COUNT is fixed by the declaration, so a generic
/// `T` field is one slot regardless of the instantiation -- one classification,
/// one slot shape. `None` unless `name` is a GC-carrying value struct.
///
/// Materialization uses the table `<Name>_vt` ctor (chosen at the pack site by
/// tier), never `<Name>_ct`; the leaf threading is repr-agnostic (`pack` builds
/// an `Inst::MakeValueStruct` that codegen renders per tier).
pub(crate) fn value_shape_gc(ws: &Workspace, name: Ustr) -> Option<ValueShape> {
    planned_value(ws, name)?.gc_shape
}

/// Declared field names of struct `name`, in declaration order (the positional
/// `<name>_ct` ctor order). A cdata-eligible enum is `[__tag, _0]` (it rides the
/// value-struct `<Enum>_ct` path). Empty if `name` is neither.
pub fn struct_field_names(ws: &Workspace, name: Ustr) -> Vec<Ustr> {
    let Some(sid) = ws.struct_by_name.get(&name) else {
        if let Some(es) = EnumPlan::for_name(ws, name).and_then(|plan| plan.cdata().cloned()) {
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

// --- `@struct_expand` intrinsic (docs/shaders.md, Layer 2) --------
//
// A call to a fn declared `@struct_expand fn f<T>(u: T) -> [(string, E)]` is
// replaced at lowering by the literal pairs list: one `(field_name, E::V(u.field))`
// per struct field, declaration order, the variant chosen STRUCTURALLY (the unique
// variant of `E` whose single payload type equals the field type). skull never
// hardcodes the engine's `UniformValue`.

/// The enum name `E` in a return type shaped exactly `[(string, E)]` (E nominal
/// and a real enum). `None` for any other shape.
pub(crate) fn pairs_return_enum(ws: &Workspace, ret: TyId) -> Option<Ustr> {
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

/// Which half of the vertex layout a `@vertex_expand` fn asks for, chosen by
/// its return type the way `@struct_expand` chooses by its pairs shape.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum VertexExpandRet {
    /// `-> int`: the element stride in floats.
    Stride,
    /// `-> [int]`: per-attribute component counts, in location order.
    Sizes,
}

pub(crate) fn vertex_expand_return(ws: &Workspace, ret: TyId) -> Option<VertexExpandRet> {
    match static_ty(ws, ret)? {
        TypeExpr::Builtin(BuiltinTy::Int) => Some(VertexExpandRet::Stride),
        TypeExpr::App { ctor: TypeCtor::Array, params } => {
            let elem = ret.with_idx(params.into_iter().next()?);
            matches!(static_ty(ws, elem)?, TypeExpr::Builtin(BuiltinTy::Int))
                .then_some(VertexExpandRet::Sizes)
        }
        _ => None,
    }
}

/// The return-shape rejection message shared by source admission and the
/// lowering expander.
pub(crate) fn vertex_expand_ret_msg(ws: &Workspace, fname: Ustr, ret: Option<TyId>) -> String {
    format!(
        "`@vertex_expand` fn `{fname}` must return `int` (the stride in floats) or `[int]` \
         (per-attribute component counts), got `{}`",
        ret.map(|r| ws.render(None, r)).unwrap_or_else(|| "()".to_string())
    )
}

pub(crate) fn vertex_expand_arg_msg(fname: Ustr) -> String {
    format!(
        "`@vertex_expand` fn `{fname}` argument must be a `FlatVec<E>` or `NativeBuffer<E>` \
         whose element is a concrete `value` struct -- the layout is derived from that element"
    )
}

/// [`crate::gpu_layout::uniform_block`] with the two-walker agreement enforced:
/// the plan's leaf-path sequence must equal [`value_leaf_paths`] over the same
/// type. The std140 side derives its members from field TYPES (matching `Vec2`
/// and `Mat4` by nominal name), the value side flattens the struct into leaves,
/// and the packer binds one to the other BY POSITION -- so a reordered vector or
/// matrix keeps every count equal while writing each leaf to the wrong offset.
///
/// The exact twin of the owned RepPlan C-layout projection for the uniform context, and
/// `Err` for the same reason: a type that reached a packing site and cannot be
/// walked consistently is an invariant failure, not a case to skip past.
pub fn uniform_block_checked(
    ws: &Workspace,
    name: Ustr,
) -> Result<crate::gpu_layout::UniformBlockPlan, String> {
    let plan = crate::gpu_layout::uniform_block(ws, name).map_err(|e| e.message.clone())?;
    let Some(shape) = value_shape(ws, name) else {
        return Err(format!("`{name}` has a std140 plan but no value shape"));
    };
    let want: Vec<String> = value_leaf_paths(&shape).iter().map(|u| u.to_string()).collect();
    let got = plan.leaf_paths();
    if want != got {
        return Err(format!(
            "leaf walkers disagree for `{name}`: value_leaf_paths {want:?} vs std140 plan {got:?} \
             -- leaves are matched to std140 slots by position, so this would write every leaf to \
             the wrong offset"
        ));
    }
    Ok(plan)
}

/// The logical uniform group and value walkers agree without consulting a
/// physical block layout. This is the compiler-owned input to every target's
/// uniform projection.
pub(crate) fn uniform_semantic_checked(
    ws: &Workspace,
    name: Ustr,
) -> Result<Vec<(Ustr, crate::gpu_layout::WireKind)>, String> {
    let members = crate::gpu_layout::uniform_group_members(ws, name)
        .map_err(|e| e.message.clone())?;
    let Some(shape) = value_shape(ws, name) else {
        return Err(format!("`{name}` has a uniform interface but no value shape"));
    };
    let want: Vec<String> = value_leaf_paths(&shape).iter().map(|u| u.to_string()).collect();
    let got: Vec<String> = members
        .iter()
        .flat_map(|(field, kind)| kind.leaf_suffixes().into_iter().map(move |suffix| format!("{field}{suffix}")))
        .collect();
    if want != got {
        return Err(format!(
            "logical uniform and value walkers disagree for `{name}`: value leaves {want:?} vs interface leaves {got:?}"
        ));
    }
    Ok(members)
}

/// The compiler-owned return shape of `@uniform_values`. Keeping this shape
/// structural lets tests and backend modules use the same typed packet without
/// teaching the runtime a second uniform schema.
pub(crate) struct UniformValuesShape {
    pub block_name: Ustr,
    pub fingerprint_field: Ustr,
    pub leaves_field: Ustr,
    pub leaf_enum: Ustr,
    pub float_variant: Ustr,
    pub int_variant: Ustr,
    pub uint_variant: Ustr,
    pub bool_variant: Ustr,
}

pub(crate) fn uniform_values_return_shape(
    ws: &Workspace,
    block_name: Ustr,
) -> Result<UniformValuesShape, String> {
    let Some(&sid) = ws.struct_by_name.get(&block_name) else {
        return Err(format!("`@uniform_values` must return a named packet struct, got `{block_name}`"));
    };
    let sd = &ws.modules[sid.module.index()].structs[sid.def];
    let Some(fp) = sd.fields.iter().find(|f| f.name.as_str() == "fingerprint") else {
        return Err(format!("`{block_name}` needs an `int fingerprint` field"));
    };
    if !matches!(static_ty(ws, fp.ty), Some(TypeExpr::Builtin(BuiltinTy::Int))) {
        return Err(format!("`{block_name}.fingerprint` must be an int"));
    }
    let Some(leaves) = sd.fields.iter().find(|f| f.name.as_str() == "leaves") else {
        return Err(format!("`{block_name}` needs a `[CanonicalUniformLeaf] leaves` field"));
    };
    let Some(TypeExpr::App { ctor: TypeCtor::Array, params }) = static_ty(ws, leaves.ty) else {
        return Err(format!("`{block_name}.leaves` must be an array of canonical leaves"));
    };
    let elem = leaves.ty.with_idx(params.into_iter().next().ok_or_else(|| {
        format!("`{block_name}.leaves` must name its canonical leaf type")
    })?);
    let TypeExpr::App { ctor: TypeCtor::Nominal(leaf_enum), .. } = static_ty(ws, elem).ok_or_else(|| {
        format!("`{block_name}.leaves` must contain a named canonical leaf enum")
    })? else {
        return Err(format!("`{block_name}.leaves` must contain a named canonical leaf enum"));
    };
    let Some(&eid) = ws.enums_by_name.get(&leaf_enum) else {
        return Err(format!("canonical leaf type `{leaf_enum}` is not an enum"));
    };
    let ed = &ws.modules[eid.module.index()].enums[eid.def];
    if ed.variants.len() != 4 {
        return Err(format!("canonical leaf enum `{leaf_enum}` must have float, int, uint and bool variants"));
    }
    let mut float_variant = None;
    let mut int_variant = None;
    let mut uint_variant = None;
    let mut bool_variant = None;
    for variant in &ed.variants {
        let VariantPayload::Tuple(types) = &variant.payload else {
            return Err(format!("canonical leaf `{}` must carry one primitive payload", variant.name));
        };
        let Some(&ty) = types.first() else {
            return Err(format!("canonical leaf `{}` must carry one primitive payload", variant.name));
        };
        if types.len() != 1 {
            return Err(format!("canonical leaf `{}` must carry one primitive payload", variant.name));
        }
        match static_ty(ws, ty) {
            Some(TypeExpr::Builtin(BuiltinTy::Float)) => float_variant = Some(variant.name),
            Some(TypeExpr::Builtin(BuiltinTy::Int)) => int_variant = Some(variant.name),
            Some(TypeExpr::Builtin(BuiltinTy::Uint)) => uint_variant = Some(variant.name),
            Some(TypeExpr::Builtin(BuiltinTy::Bool)) => bool_variant = Some(variant.name),
            _ => return Err(format!("canonical leaf `{}` must carry float, int, uint or bool", variant.name)),
        }
    }
    let (Some(float_variant), Some(int_variant), Some(uint_variant), Some(bool_variant)) =
        (float_variant, int_variant, uint_variant, bool_variant)
    else {
        return Err(format!("canonical leaf enum `{leaf_enum}` needs float, int, uint and bool variants"));
    };
    Ok(UniformValuesShape {
        block_name,
        fingerprint_field: fp.name,
        leaves_field: leaves.name,
        leaf_enum,
        float_variant,
        int_variant,
        uint_variant,
        bool_variant,
    })
}

/// The arity rejection message shared by source admission and the lowering
/// expander.
pub(crate) fn uniform_expand_arity_msg(fname: Ustr) -> String {
    format!(
        "`@uniform_expand` fn `{fname}` takes either three arguments -- the float view, the \
         int view, and the uniform group (the packer) -- or one, the uniform group alone \
         (its layout fingerprint)"
    )
}

pub(crate) fn uniform_expand_arg_msg(fname: Ustr) -> String {
    format!(
        "`@uniform_expand` fn `{fname}`'s uniform-group argument must be a concrete `value` struct -- the \
         std140 layout is derived from it"
    )
}

/// Value struct `name`'s fields as `(name, ty)` in declaration order. `None` when
/// `name` is not a `value` struct.
pub(crate) fn value_struct_field_tys(ws: &Workspace, name: Ustr) -> Option<Vec<(Ustr, TyId)>> {
    let sid = *ws.struct_by_name.get(&name)?;
    let sd = &ws.modules[sid.module.index()].structs[sid.def];
    if !sd.is_value {
        return None;
    }
    Some(sd.fields.iter().map(|f| (f.name, f.ty)).collect())
}

/// Variant names of `enum_name` whose payload is a single type rendering to
/// `want`. Zero -> no match; more than one -> ambiguous.
pub(crate) fn variants_for_payload(ws: &Workspace, enum_name: Ustr, want: &str) -> Vec<Ustr> {
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
pub(crate) fn struct_expand_variants(
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

pub(crate) fn sig_shape_of_nominal(ws: &Workspace, n: Ustr) -> Option<ValueShape> {
    value_shape(ws, n)
        .or_else(|| value_shape_gc(ws, n))
        .or_else(|| EnumPlan::for_name(ws, n).and_then(|plan| plan.flat().map(|s| s.value_shape())))
}

/// [`sig_shape_of_nominal`] for a NULLABLE nominal.
pub(crate) fn sig_shape_of_nullable_nominal(ws: &Workspace, n: Ustr) -> Option<ValueShape> {
    if ws
        .struct_by_name
        .get(&n)
        .is_some_and(|sid| ws.modules[sid.module.index()].structs[sid.def].is_nobox)
    {
        let mut shape = value_shape(ws, n).or_else(|| value_shape_gc(ws, n))?;
        shape.nullable = true;
        return Some(shape);
    }
    EnumPlan::for_name(ws, n).and_then(|plan| plan.flat().map(|s| s.value_shape()))
}

/// The value shape of a `name` nominal (value struct or flat enum), or `None`.
pub(crate) fn value_shape_of_nominal(ws: &Workspace, name: Ustr) -> Option<ValueShape> {
    value_shape(ws, name)
        .or_else(|| value_shape_gc(ws, name))
        .or_else(|| EnumPlan::for_name(ws, name).and_then(|plan| plan.flat().map(|s| s.value_shape())))
}

/// The Lua global holding leaf `leaf` of the `nobox` global `base`. A global is
/// category-2 storage -- its layout is derived from the stored type -- so a
/// `nobox` global is K scalar globals, never one boxed cdata.
pub fn nobox_global_leaf_name(base: Ustr, leaf: usize) -> Ustr {
    Ustr::from(&format!("__nbg_{base}_{leaf}"))
}

/// Whether `name` is a `nobox` value struct.
pub fn is_nobox_nominal(ws: &Workspace, name: Ustr) -> bool {
    ws.struct_by_name
        .get(&name)
        .is_some_and(|sid| ws.modules[sid.module.index()].structs[sid.def].is_nobox)
}

/// Everything the reflection accessor for a `nobox` global decides, answered
/// from the SAME oracles the emitter uses rather than from the leaf paths'
/// punctuation.
///
/// The two questions used to be read off the path list: "is any path a proper
/// PREFIX of another" for the presence leaf, and "does any path contain a dot"
/// for the ctor suffix. Both are proxies, and a `nobox` child with NO data
/// leaves defeats both -- it contributes exactly one leaf under its own name
/// with nothing below it, so the sequence is flat and undotted while a presence
/// tag is right there and the type is leaf-table. That is the same shape
/// `tests/nobox_zero_leaf_child.rock` pins for the Lua ctor site.
pub struct NoboxGlobalAccessorShape {
    pub ty_name: Ustr,
    pub paths: Vec<Ustr>,
    pub nullable: bool,
    pub storage: ValueStorage,
    /// From the shared physical storage classification -- the same answer the
    /// `_lct` ctor and the leaf-form pack site consume.
    pub ctor_suffix: &'static str,
    /// The path of a leaf that is a nullable child's PRESENCE tag, if any. From
    /// [`value_leaf_presence_flags`], positionally. Informational: a leaf-table
    /// ctor takes tags like any other leaf, so this does NOT by itself make a
    /// shape unbuildable.
    pub presence_path: Option<Ustr>,
    /// Why no accessor can be built, if so. `_lct` is positional over physical
    /// leaves, while `_vt` is positional over declared fields; nested GC-table
    /// fields therefore cannot be rebuilt from the flattened global storage.
    pub unbuildable: Option<String>,
}

pub fn nobox_global_accessor_shape(
    ws: &Workspace,
    scx: &ScalarCtx,
    ty: TyId,
) -> Result<Option<NoboxGlobalAccessorShape>, RepError> {
    let plan = GlobalPlan::for_declared_type(ws, scx, ty)?;
    let Some(shape) = plan.value_shape() else { return Ok(None) };
    let paths = plan.leaf_paths().ok_or_else(|| RepError {
        span: None,
        message: format!("`{}` has no physical leaf-path plan for global accessors", shape.name),
    })?;
    // `value_leaf_paths` omits the global's own presence leaf, and
    // `value_leaf_presence_flags` omits it too; both sequences therefore line
    // up directly. Nested nullable fields remain represented in both.
    let flags = plan.presence_flags().ok_or_else(|| RepError {
        span: None,
        message: format!("`{}` has no physical presence plan for global accessors", shape.name),
    })?;
    if flags.len() != paths.len() {
        return Err(RepError {
            span: None,
            message: format!(
                "`{}` has {} presence entries for {} leaf paths",
                shape.name,
                flags.len(),
                paths.len()
            ),
        });
    }
    let presence_path =
        flags.iter().zip(paths.iter()).find(|(is_tag, _)| **is_tag).map(|(_, p)| *p);
    let storage = plan.value_storage().ok_or_else(|| RepError {
        span: None,
        message: format!("`{}` has no physical storage plan for global accessors", shape.name),
    })?;
    let has_nested = paths.iter().any(|p| p.contains('.'));
    let ctor_suffix = match storage {
        ValueStorage::LeafTable => "_lct",
        ValueStorage::GcTable => "_vt",
        ValueStorage::CData if has_nested => "_lct",
        ValueStorage::CData => "_ct",
    };
    let unbuildable = if storage == ValueStorage::GcTable {
        paths.iter().find(|p| p.contains('.')).map(|nested| {
            format!(
                "`{}{ctor_suffix}` is positional over DECLARED FIELDS, but this type's \
                 storage is {} leaves including a nested one (`{nested}`), so the accessor \
                 would pass a leaf where the ctor wants a nested value",
                shape.name,
                paths.len(),
            )
        })
    } else {
        None
    };
    Ok(Some(NoboxGlobalAccessorShape {
        ty_name: shape.name,
        paths,
        nullable: shape.nullable,
        storage,
        ctor_suffix,
        presence_path,
        unbuildable,
    }))
}

/// Structural depth of an instantiation type, counting a leaf as 1. Polymorphic
/// recursion (`f<T> -> f<[T]>`) is exactly type GROWTH, so depth is its
/// observable and a plain tree walk is the whole detector -- no
/// instantiation-graph machinery.
///
/// A CONSTRUCTOR counts, so a parameterless nominal is 1 and `Vec<Vec2>` is 2 --
/// unlike [`MonoTy::depth`], which measures nesting alone. Both cap on the depth
/// of the TYPE rather than on a path length: a path length is a property of how
/// the walk arrived, so capping on it makes membership depend on discovery
/// order.
pub fn growth_depth(t: &MonoTy) -> u32 {
    match t {
        MonoTy::Int
        | MonoTy::Uint
        | MonoTy::Float
        | MonoTy::Bool
        | MonoTy::String
        | MonoTy::Any
        | MonoTy::Unit
        | MonoTy::Never => 1,
        MonoTy::Nullable(inner) => 1 + growth_depth(inner),
        MonoTy::App { params, .. } => {
            1 + params.iter().map(growth_depth).max().unwrap_or(0)
        }
        MonoTy::Tuple(parts) => 1 + parts.iter().map(growth_depth).max().unwrap_or(0),
        MonoTy::Function { params, ret } => {
            1 + params.iter().map(growth_depth).chain([growth_depth(ret)]).max().unwrap_or(0)
        }
    }
}

/// Depth at which the ablation calls an instantiation polymorphic growth.
///
/// Chosen to sit ABOVE anything the corpus produces: the deepest real
/// instantiation measured is well under this, so a firing means genuine growth
/// rather than an ordinarily nested container. It is a REPORTING threshold in a
/// diagnostic mode, never a limit on what compiles.
pub const ABLATION_GROWTH_DEPTH: u32 = 8;

/// Callee generic `VarId` -> its concrete type tree, for specialized lowering.
/// Total against the declaration's generics, exactly as [`Vec<MonoTy>`] is.
///
/// THE SAME TYPE as [`crate::monoty::MonoSubst`], which is what an instance of
/// the mono graph carries. One substitution vocabulary, so an instance's own
/// bindings drive lowering and the ABI plan directly rather than through a
/// conversion that could lose or reorder them.
pub type SpecSubst = crate::monoty::MonoSubst;

pub(crate) fn mono_value_shape(ws: &Workspace, ty: &MonoTy) -> Option<ValueShape> {
    let (payload, nullable) = peel_nullable_layers(ty);
    let MonoTy::App { ctor: MonoCtor::Nominal(name), .. } = payload else {
        return None;
    };
    if nullable {
        sig_shape_of_nullable_nominal(ws, *name)
    } else {
        value_shape_of_nominal(ws, *name)
    }
}

pub(crate) fn mono_nobox_value_shape(ws: &Workspace, ty: &MonoTy) -> Option<ValueShape> {
    let (payload, nullable) = peel_nullable_layers(ty);
    let MonoTy::App { ctor: MonoCtor::Nominal(name), .. } = payload else {
        return None;
    };
    is_nobox_nominal(ws, *name).then(|| {
        if nullable {
            sig_shape_of_nullable_nominal(ws, *name)
        } else {
            sig_shape_of_nominal(ws, *name)
        }
    })?
}

fn peel_nullable_layers(mut ty: &MonoTy) -> (&MonoTy, bool) {
    let mut nullable = false;
    while let MonoTy::Nullable(inner) = ty {
        nullable = true;
        ty = inner.as_ref();
    }
    (ty, nullable)
}

/// The concrete instance a generic direct call targets, independently of
/// inlining or target policy. Missing bindings for a resolved generic call are
/// compiler-state errors; an incomplete binding is an explicit absent instance.
///
/// `None` means the instantiation is genuinely not complete here -- a callee
/// with no generics, or one whose bindings do not all reify (a caller-generic
/// slot inside a shared body).
pub fn callee_instance_subst(
    ws: &Workspace,
    inf: &BodyInference,
    e: Idx<Expr>,
    func: FuncRef,
    enclosing: &SpecSubst,
) -> Result<Option<SpecSubst>, CompilerError> {
    let f = &ws.module(func.module).functions[func.func];
    if f.generics.is_empty() {
        return Ok(None);
    }
    let Some(bindings) = inf.call_generic_bindings.get(&e) else {
        return Err(CompilerError::new_with_label(
            f.name_span,
            format!(
                "internal: no recorded generic bindings for a resolved call to `{}`",
                f.name
            ),
        ));
    };
    instance_subst_from_bindings(ws, inf, func, bindings, enclosing)
}

/// The concrete instance a set of generic bindings resolves to.
///
/// Converts through [`crate::monoty::convert`] -- the one TOTAL logical-type
/// oracle shared by graph construction and delivery.
///
/// `UnboundGeneric` is the one legitimate `None`: the enclosing body is a
/// shared generic snapshot, so this call site decides no instantiation.
/// `UnresolvedInfer` is a compiler-state rejection carrying the callee's span;
/// it is not a partial physical plan. Every other reason is also a compiler-state
/// rejection carrying the callee's span: a recovery type, a missing node or a
/// cyclic substitution chain is not a type the program is allowed to have
/// reached here.
pub(crate) fn instance_subst_from_bindings(
    ws: &Workspace,
    inf: &BodyInference,
    func: FuncRef,
    bindings: &[TyId],
    enclosing: &SpecSubst,
) -> Result<Option<SpecSubst>, CompilerError> {
    use crate::monoty::MonoTyReason;
    let f = &ws.module(func.module).functions[func.func];
    if bindings.len() != f.generics.len() {
        return Err(CompilerError::new_with_label(
            f.name_span,
            format!(
                "internal: `{}` declares {} generic(s) but its call site supplied {} binding(s)",
                f.name,
                f.generics.len(),
                bindings.len()
            ),
        ));
    }
    let mut subst = SpecSubst::default();
    for (g, b) in f.generics.iter().zip(bindings) {
        let Some(TypeExpr::Generic(var, _)) = ty_node(ws, inf, *g) else {
            return Err(CompilerError::new_with_label(
                f.name_span,
                format!(
                    "internal: declared generic of `{}` is not a generic type variable",
                    f.name
                ),
            ));
        };
        match crate::monoty::convert(ws, inf, *b, enclosing) {
            Ok(ty) => {
                subst.insert(var, ty);
            }
            Err(MonoTyReason::UnboundGeneric(_)) => return Ok(None),
            Err(MonoTyReason::UnresolvedInfer(name)) => {
                return Err(CompilerError::new_with_label(
                    f.name_span,
                    format!(
                        "internal: a call to `{}` has unresolved inference type `{name}`",
                        f.name
                    ),
                ))
            }
            Err(reason) => {
                return Err(CompilerError::new_with_label(
                    f.name_span,
                    format!(
                        "internal: a call to `{}` supplies a type argument that is not a logical type ({}), so the instance it names cannot be stated",
                        f.name,
                        reason.bucket()
                    ),
                ))
            }
        }
    }
    Ok(Some(subst))
}


/// The CONCRETE INSTANCE an iterator-protocol WITNESS resolves to, from the
/// `VarId -> TyId` map bone recorded for it.
///
/// The identity sibling of [`spec_key_from_next_generics`], and the same
/// separation [`callee_instance_subst`] makes for a direct call: that one stops
/// at the delivery policy, this one answers which instance the witness means.
/// One function serves both witnesses -- `IterWitness::generics` and
/// `next_generics` are the same shape, and a second copy would be free to
/// disagree about one of them.
pub fn witness_instance_subst(
    ws: &Workspace,
    inf: &BodyInference,
    func: FuncRef,
    generics: &rustc_hash::FxHashMap<bone_semantic::hir::VarId, TyId>,
    enclosing: &SpecSubst,
) -> Result<Option<SpecSubst>, CompilerError> {
    let f = &ws.module(func.module).functions[func.func];
    if f.generics.is_empty() {
        return Ok(None);
    }
    let mut bindings = Vec::with_capacity(f.generics.len());
    for generic in &f.generics {
        let Some(TypeExpr::Generic(var, _)) = ty_node(ws, inf, *generic) else { return Ok(None) };
        // A generic the resolution left unbound leaves the instantiation
        // INCOMPLETE, which the census reports as its own cause rather than
        // being papered over with a default.
        let Some(bound) = generics.get(&var).copied() else { return Ok(None) };
        bindings.push(bound);
    }
    instance_subst_from_bindings(ws, inf, func, &bindings, enclosing)
}

pub fn spec_key_from_next_generics(
    ws: &Workspace,
    inf: &BodyInference,
    func: FuncRef,
    next_generics: &rustc_hash::FxHashMap<bone_semantic::hir::VarId, TyId>,
    enclosing: &SpecSubst,
) -> Result<Option<Vec<MonoTy>>, CompilerError> {
    let Some(subst) = witness_instance_subst(ws, inf, func, next_generics, enclosing)? else {
        return Ok(None);
    };
    Ok(spec_key_from_subst(ws, func, &subst))
}

/// Rebuild the [`SpecSubst`] (callee generic `VarId` -> concrete type) for `fref`
/// from a [`Vec<MonoTy>`] (trees in declaration order). The inliner holds only the
/// arena-free key in its cache; this re-pairs it with the callee's generic
/// `VarId`s to drive concrete lowering. An empty key belongs only to a
/// non-generic declaration; an incomplete generic substitution has no key.
/// The inverse of [`spec_subst_from_key`], and the E1 cut's premise: for a body
/// emission actually writes, a [`Vec<MonoTy>`] carries nothing the [`SpecSubst`]
/// does not.
///
/// `Some` only for a TOTAL rebuild -- every declared generic present in `subst`
/// -- so the caller cannot mistake a partial answer for a key. This is what
/// makes the two directions a round trip rather than a pair of lossy projections.
///
/// The declaration supplies the ORDER, which is the only thing a substitution
/// lacks: a key is positional over `f.generics` and a subst is keyed by `VarId`.
pub fn spec_key_from_subst(ws: &Workspace, fref: FuncRef, subst: &SpecSubst) -> Option<Vec<MonoTy>> {
    let f = &ws.module(fref.module).functions[fref.func];
    if f.generics.is_empty() {
        return None;
    }
    let mut key: Vec<MonoTy> = Vec::with_capacity(f.generics.len());
    for g in f.generics.iter() {
        let Some(TypeExpr::Generic(var, _)) = static_ty(ws, *g) else { return None };
        key.push(subst.get(&var)?.clone());
    }
    Some(key)
}

pub fn spec_subst_from_key(ws: &Workspace, fref: FuncRef, key: &Vec<MonoTy>) -> SpecSubst {
    let f = &ws.module(fref.module).functions[fref.func];
    let mut subst = SpecSubst::default();
    for (g, reified) in f.generics.iter().zip(key) {
        if let Some(TypeExpr::Generic(var, _)) = static_ty(ws, *g) {
            subst.insert(var, reified.clone());
        }
    }
    subst
}

/// The wire shape of ONE element of a tuple return.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RetSlot {
    /// One opaque value -- today's ABI, and what every element that is not a
    /// `nobox` value keeps.
    Opaque,
    /// A `nobox` value: its leaves ride the return directly, so the element
    /// never needs a box to cross the boundary.
    Leaves(ValueShape),
    /// A nested tuple that itself carries a `nobox` element somewhere inside.
    Nested(TupleRetShape),
}

impl RetSlot {
    pub(crate) fn slot_count(&self) -> usize {
        match self {
            RetSlot::Opaque => 1,
            RetSlot::Leaves(shape) => shape.leaf_count(),
            RetSlot::Nested(inner) => inner.slot_count(),
        }
    }

    pub(crate) fn splats(&self) -> bool {
        !matches!(self, RetSlot::Opaque)
    }
}

pub(crate) fn value_shape_can_fill(actual: &ValueShape, expected: &ValueShape) -> bool {
    actual.name == expected.name
        && (!actual.nullable || expected.nullable)
        && actual.fields.len() == expected.fields.len()
        && actual
            .fields
            .iter()
            .zip(&expected.fields)
            .all(|((actual_name, actual_field), (expected_name, expected_field))| {
                actual_name == expected_name
                    && match (actual_field, expected_field) {
                        (FieldShape::Prim, FieldShape::Prim) => true,
                        (FieldShape::Nested(actual), FieldShape::Nested(expected)) => {
                            value_shape_can_fill(actual, expected)
                        }
                        _ => false,
                    }
            })
}

pub(crate) fn ret_slot_can_fill(actual: &RetSlot, expected: &RetSlot) -> bool {
    match (actual, expected) {
        (RetSlot::Opaque, RetSlot::Opaque) => true,
        (RetSlot::Leaves(actual), RetSlot::Leaves(expected)) => {
            value_shape_can_fill(actual, expected)
        }
        (RetSlot::Nested(actual), RetSlot::Nested(expected)) => {
            (!actual.nullable || expected.nullable)
                && actual.slots.len() == expected.slots.len()
                && actual
                    .slots
                    .iter()
                    .zip(&expected.slots)
                    .all(|(actual, expected)| ret_slot_can_fill(actual, expected))
        }
        _ => false,
    }
}

/// Per-element wire shape of a tuple RETURN under the multi-return ABI.
///
/// Only a `nobox` element splats. An unmarked value struct keeps its single
/// boxed slot: marking a type is the one thing that re-shapes a return, which
/// is what lets an `@scalar_abi inline(lua)` body hand-write a `(bool, Vec2)`
/// return without the compiler moving the wire underneath it.
///
/// The shape is derived from the return TYPE alone -- no callee identity -- so a
/// direct call, an indirect call through a fn value and the definition all
/// agree by construction, exactly as the plain multi-return arity does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TupleRetShape {
    pub(crate) slots: Vec<RetSlot>,
    /// The DECLARED return was `(..)?`: a presence bool leads the wire, then the
    /// slots (`false, nil, ...` when absent). Same presence-leads convention as
    /// `ValueShape::nullable` one level down.
    ///
    /// Only ever set from a declared return type, never inferred from an
    /// expression's type: safe-nav manufactures the identical
    /// `Nullable(Tuple)` with NO presence slot, so keying on nullability there
    /// would add a phantom slot and corrupt every safe-nav tuple call.
    pub(crate) nullable: bool,
}

impl TupleRetShape {
    /// Total returned values.
    pub(crate) fn slot_count(&self) -> usize {
        self.slots.iter().map(RetSlot::slot_count).sum::<usize>() + self.nullable as usize
    }

    /// Whether any element carries leaves -- `false` means this is byte-for-byte
    /// the pre-existing ABI and every splat-aware path stays off.
    pub(crate) fn splats(&self) -> bool {
        self.slots.iter().any(RetSlot::splats)
    }
}

/// What a concrete `next` actually returns at a splice site, for the placeholder
/// `CallBoundMulti` whose arity bound conformance fixes at `(bool, E)`.
///
/// `slot_count` is the real returned-value count and `elem` names the element's
/// value struct plus its leaf FIELDS in ctor order when the element splats. The
/// splice sizes its dests from the former and repacks the element from the
/// latter; see `resolve_cloned_bound_calls`.
///
/// Read only where the LOWERING could not resolve the element itself: a body
/// specialized at a value element splits `E` into leaf cells there
/// (`MirFunc::bound_next_split`) and this only CHECKS the two agree. What is
/// left is a generic bound to a TUPLE: it reifies and is in the key like any
/// other type, but its children live in the concrete plan rather than a source
/// type arena, so the rebuild below owns that shape directly.
///
/// The three cases are kept DISTINCT on purpose: "nothing to repack" and "a shape
/// the flat repack cannot express" must not collapse into one `None`, or an
/// unsupported element silently takes the do-nothing path and miscompiles.
pub enum BoundNextElem {
    /// Today's ABI -- one opaque slot. Nothing to do.
    Opaque,
    /// The element splats; the plan says how to put it back together.
    Rebuild(ElemRebuild),
    /// Splats through a shape the repack cannot express (a nullable value, whose
    /// presence leaf has no field to pair with). The splice REJECTS on this rather
    /// than guess a leaf-to-field pairing -- a wrong pairing writes one leaf's
    /// value into another field, silently.
    ///
    /// Reached only when the lowering left the element opaque, which for a
    /// nullable value now means a TUPLE element (`zip`'s pair): its concrete
    /// tuple shape is carried by the binding plan rather than a source node.
    Unsupported,
}

/// How to rebuild one splatting element from the wire values, public because the
/// splice that consumes it lives in `skull-transform`. It mirrors [`RetSlot`],
/// which stays private: this is the part the other crate needs, not the shape
/// machinery behind it.
pub enum ElemRebuild {
    /// One wire value, used as-is (an unmarked element inside a splatting tuple).
    Passthrough,
    /// A `nobox` value: its struct name, owned representation, and leaf fields
    /// in ctor order.
    Value(Ustr, RepId, Vec<Ustr>),
    /// A tuple element (`zip`'s pair, `enumerate`'s `(int, T)`): rebuild each part,
    /// then pack them.
    Tuple(Vec<ElemRebuild>),
}

impl ElemRebuild {
    /// Wire values this plan consumes.
    pub fn leaf_count(&self) -> usize {
        match self {
            ElemRebuild::Passthrough => 1,
            ElemRebuild::Value(_, _, fields) => fields.len(),
            ElemRebuild::Tuple(parts) => parts.iter().map(Self::leaf_count).sum(),
        }
    }
}

fn elem_rebuild_from_rep(arena: &RepArena, id: RepId) -> Option<ElemRebuild> {
    let plan = arena.try_get(id)?;
    match plan.layout() {
        RepLayout::Opaque | RepLayout::Foreign { .. } => Some(ElemRebuild::Passthrough),
        RepLayout::Leaves(shape) if !shape.nullable => {
            let fields: Option<Vec<Ustr>> = shape
                .fields
                .iter()
                .map(|(n, fs)| matches!(fs, FieldShape::Prim).then_some(*n))
                .collect();
            fields.map(|f| ElemRebuild::Value(shape.name, id, f))
        }
        RepLayout::Tuple { slots, nullable: false } => slots
            .iter()
            .map(|slot| elem_rebuild_from_rep(arena, *slot))
            .collect::<Option<Vec<_>>>()
            .map(ElemRebuild::Tuple),
        RepLayout::Leaves(_) | RepLayout::Tuple { nullable: true, .. } => None,
    }
}

pub struct BoundNextShape {
    pub slot_count: usize,
    pub elem: BoundNextElem,
}

/// The same bound-`next` shape from the selected concrete body signature.
/// Production standalone resolution already owns this signature; rebuilding
/// it from the witness declaration would reintroduce the source-shaped oracle.
pub fn bound_next_shape_from_concrete(
    ws: &Workspace,
    scx: &ScalarCtx,
    interner: &crate::monoty::MonoInterner,
    signature: &ConcreteSignature,
) -> Result<BoundNextShape, RepError> {
    let result = signature.result.map(|id| interner.get(id)).ok_or_else(|| RepError {
        span: None,
        message: "a concrete bound witness has no result type".into(),
    })?;
    let id = scx
        .rep_arena()
        .plan(
            ws,
            scx.target(),
            &result,
            RepUse::Return,
            &RepUseFacts { tuple_spelled: true, span: None },
            RepGuarantees { nobox: true, allow_opaque: true },
        )
        ?;
    let shape = scx.rep_arena().tuple_shape(id).ok_or_else(|| RepError {
        span: None,
        message: format!("bound witness result `{}` has no tuple return plan", result.render()),
    })?;
    let plan = scx.rep_arena().try_get(id).ok_or_else(|| RepError {
        span: None,
        message: "bound witness return plan does not belong to the build arena".into(),
    })?;
    let RepLayout::Tuple { slots, .. } = plan.layout() else {
        return Err(RepError {
            span: None,
            message: "bound witness return plan is not a tuple layout".into(),
        });
    };
    // The element is slot 1 alone, or slots 1.. as a tuple -- the same pairing
    // `lower_for_user_iter` makes when it binds the pattern, so the two paths
    // cannot disagree about what the element IS.
    let elem_slots = match slots.get(1..).ok_or_else(|| RepError {
        span: None,
        message: "bound witness return plan has no success flag and element slots".into(),
    })? {
        [] => {
            return Err(RepError {
                span: None,
                message: "bound witness return plan has no element slot".into(),
            })
        }
        ids => ids,
    };
    let arena = scx.rep_arena();
    let elem_splats = elem_slots.iter().any(|slot| arena.get(*slot).splats(&arena));
    let elem = if elem_splats {
        let rebuild = if elem_slots.len() == 1 {
            elem_rebuild_from_rep(&arena, elem_slots[0])
        } else {
            elem_slots
                .iter()
                .map(|slot| elem_rebuild_from_rep(&arena, *slot))
                .collect::<Option<Vec<_>>>()
                .map(ElemRebuild::Tuple)
        };
        match rebuild {
            Some(plan) => BoundNextElem::Rebuild(plan),
            None => BoundNextElem::Unsupported,
        }
    } else {
        BoundNextElem::Opaque
    };
    Ok(BoundNextShape { slot_count: shape.slot_count(), elem })
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
#[derive(Clone, Debug, PartialEq, Eq)]
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
    pub(crate) fn value_shape(&self) -> ValueShape {
        ValueShape {
            name: self.name,
            fields: self.field_names().into_iter().map(|n| (n, FieldShape::Prim)).collect(),
            nullable: false,
        }
    }
}

/// The slot name for a payload field: `_<i>` for a tuple position, `_<field>` for
/// a record field. (Record field names are identifiers, so they never collide
/// with the numeric tuple form.)
pub(crate) fn slot_name(suffix: &str) -> Ustr {
    Ustr::from(format!("_{suffix}").as_str())
}

pub(crate) fn scalar_c_type(ws: &Workspace, ty: TyId) -> Option<&'static str> {
    match static_ty(ws, ty)? {
        TypeExpr::Builtin(BuiltinTy::Int) => Some("int"),
        TypeExpr::Builtin(BuiltinTy::Uint) => Some("uint32_t"),
        TypeExpr::Builtin(BuiltinTy::Float) => Some("float"),
        TypeExpr::Builtin(BuiltinTy::Bool) => Some("bool"),
        _ => None,
    }
}

/// `Some(shape)` iff `name` is a cdata-eligible enum (see [`EnumCdataShape`]).
fn build_enum_cdata_shape(ws: &Workspace, name: Ustr) -> Option<EnumCdataShape> {
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

/// The C type for a `value`-struct field, or `None` when the field is NOT
/// C-layout-mappable (`[T]`, `Arena<T>`, `Map`, `string`, a generic `T`, or a
/// GC-carrying nested value struct). This is the single mappability oracle:
/// skull's cdef emission uses the string, and the shared storage plan classifies
/// a struct GC-carrying when any field maps to `None`. Mirrors rock's
/// `rock_type_to_c_type`. A nested payload-free enum stores as its int tag; a
/// cdata-eligible payload enum nests its own `{__tag, _0}` ctype.
pub fn value_field_c_type(ws: &Workspace, ty: TyId) -> Option<String> {
    value_field_c_type_guarded(ws, ty)
}

pub(crate) fn value_field_c_type_guarded(ws: &Workspace, ty: TyId) -> Option<String> {
    match static_ty(ws, ty)? {
        TypeExpr::Builtin(BuiltinTy::Int) => Some("int".into()),
        TypeExpr::Builtin(BuiltinTy::Uint) => Some("uint32_t".into()),
        TypeExpr::Builtin(BuiltinTy::Float) => Some("float".into()),
        TypeExpr::Builtin(BuiltinTy::Bool) => Some("bool".into()),
        // A nested value struct is C-mappable iff it is itself FFI-clean; a
        // GC-carrying nested value struct makes this field (and its parent)
        // GC-carrying too.
        TypeExpr::App { ctor: TypeCtor::Nominal(name), .. } if map_is_value_struct(ws, name) => {
            if value_storage_is_gc_guarded(ws, name) {
                None
            } else {
                Some(name.as_str().to_string())
            }
        }
        TypeExpr::App { ctor: TypeCtor::Nominal(name), .. }
            if payload_free_enum_variants(ws, name).is_some() =>
        {
            Some("int".into())
        }
        TypeExpr::App { ctor: TypeCtor::Nominal(name), .. }
            if EnumPlan::for_name(ws, name).is_some_and(|plan| plan.cdata().is_some()) =>
        {
            Some(name.as_str().to_string())
        }
        _ => None,
    }
}

/// Whether a value-struct field is compared STRUCTURALLY in a GC-tier `__eq`
/// (`==`, which for scalars/enums is value-equality and for a nested value
/// struct fires that struct's own metatable), versus BY REFERENCE (`rawequal`).
/// Scalars, enums, and nested value structs (either tier) are structural; a true
/// GC-ref field (`[T]`, `Arena<T>`, `Map`, a generic `T`, `string`) is by
/// reference -- `string` gives the same answer either way (Lua interns; JS `===`
/// matches), and the contract fixes GC-refs to identity. Consumed by BOTH the
/// Lua GC-tier `_vt.__eq` emitter (`skull::value_struct_gc_defs`) and the JS
/// `__rock_vt_eq` registry -- the single field-dispatch oracle, sibling to
/// the shared physical storage plan.
pub fn gc_eq_is_structural(ws: &Workspace, ty: TyId) -> bool {
    if value_field_c_type(ws, ty).is_some() {
        return true;
    }
    matches!(
        static_ty(ws, ty),
        Some(TypeExpr::App { ctor: TypeCtor::Nominal(name), .. }) if map_is_value_struct(ws, name)
    )
}

pub(crate) fn value_storage_is_gc_guarded(ws: &Workspace, name: Ustr) -> bool {
    planned_value(ws, name).is_some_and(|plan| plan.storage != ValueStorage::CData)
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnumFlatShape {
    pub(crate) name: Ustr,
    /// One entry per payload POSITION, widest variant first-wins. A position is
    /// several SLOTS when it carries a scalarizable value, so this is not the slot
    /// count -- ask [`EnumFlatShape::tuple_width`] for that and
    /// [`EnumFlatShape::slot_offset`] for where a position starts.
    pub(crate) slots: Vec<FlatSlot>,
    /// Named payload slots `f__<name>` (union of record-variant field names,
    /// first-seen order). A tuple position and a record field never collide --
    /// the former is a number, the latter an identifier.
    pub(crate) record_fields: Vec<Ustr>,
}

/// The wire shape of ONE positional payload of a flat enum.
///
/// A payload slot is a dynamically-typed Lua field, so ANY single value fits in
/// one -- except a scalarizable value, which can be several leaves. Widening the
/// position to its leaves keeps it on the scalar ABI: the value's
/// leaves ARE the slots, so an enum payload never materializes one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FlatSlot {
    /// One dynamic slot -- today's shape, and what every non-scalarizable payload keeps.
    Opaque,
    /// The scalarizable value payloads SHARING this position, in variant-declaration
    /// order and deduplicated by name. The position is as wide as the widest;
    /// each variant writes and reads only its own leaves in the leading slots and
    /// nil-pads the rest, discriminated by the tag the ctor makes static and the
    /// match already tests. Never empty.
    Leaves(Vec<ValueShape>),
}

impl FlatSlot {
    /// Slots this position occupies: the widest payload sharing it.
    pub(crate) fn width(&self) -> usize {
        match self {
            FlatSlot::Opaque => 1,
            FlatSlot::Leaves(vss) => vss
                .iter()
                .map(ValueShape::leaf_count)
                .max()
                .expect("a widened position carries at least one shape"),
        }
    }
}

impl EnumFlatShape {
    /// Named physical columns shared by this enum's record variants.
    pub fn record_fields(&self) -> &[Ustr] {
        &self.record_fields
    }

    /// Total positional SLOTS -- `_0 .. _{tuple_width-1}`. Distinct from the
    /// position count (`slots.len()`): a `nobox` position spans several.
    pub fn tuple_width(&self) -> usize {
        self.slots.iter().map(FlatSlot::width).sum()
    }

    /// The first slot index of payload position `pos`. Positions past the end
    /// answer with the total width, so a variant narrower than the union simply
    /// has nothing to write.
    pub fn slot_offset(&self, pos: usize) -> usize {
        self.slots.iter().take(pos).map(FlatSlot::width).sum()
    }

    /// The leaf shape position `pos` holds for a payload of nominal `name`, else
    /// `None`. THE oracle for both the ctor's placement and a match arm's read,
    /// so a write and a read of one position cannot disagree: a position may be
    /// widened for a SIBLING variant while this variant's payload there is
    /// ordinary, or a `nobox` value of a different shape.
    pub(crate) fn position_shape(&self, pos: usize, name: Option<Ustr>) -> Option<&ValueShape> {
        let FlatSlot::Leaves(vss) = self.slots.get(pos)? else {
            return None;
        };
        let n = name?;
        vss.iter().find(|vs| vs.name == n)
    }

    pub fn position_leaf_paths(&self, pos: usize, name: Ustr) -> Option<Vec<Ustr>> {
        self.position_shape(pos, Some(name)).map(value_leaf_paths)
    }

    /// The number of scalar leaves this enum threads as across a call: the
    /// discriminant + every positional slot + every record slot.
    pub(crate) fn leaf_count(&self) -> usize {
        1 + self.tuple_width() + self.record_fields.len()
    }

    /// The `ValueShape` the scalar ABI / SROA see: `[__tag, _0.., <rec>..]`, all
    /// primitive leaves. `pack`/`cdata_leaves` re-detect the enum by name and use
    /// box semantics (the leaf NAMES here are nominal -- never read structurally).
    pub(crate) fn value_shape(&self) -> ValueShape {
        let mut fields = vec![(Ustr::from("__tag"), FieldShape::Prim)];
        for i in 0..self.tuple_width() {
            fields.push((Ustr::from(format!("_{i}").as_str()), FieldShape::Prim));
        }
        for rf in &self.record_fields {
            fields.push((*rf, FieldShape::Prim));
        }
        ValueShape {
            name: self.name,
            fields,
            nullable: false,
        }
    }
}

/// `Some(shape)` iff `name` is a mixed/boxed enum eligible for field flattening.
fn build_flat_enum_shape(ws: &Workspace, name: Ustr) -> Option<EnumFlatShape> {
    build_flat_enum_shape_inner(ws, name, &mut Vec::new())
}

fn build_flat_enum_shape_inner(
    ws: &Workspace,
    name: Ustr,
    visiting: &mut Vec<Ustr>,
) -> Option<EnumFlatShape> {
    let eid = *ws.enums_by_name.get(&name)?;
    // Payload-free enums are ints; cdata-eligible enums scalarize via cdata.
    if payload_free_enum_variants(ws, name).is_some() || build_enum_cdata_shape(ws, name).is_some() {
        return None;
    }
    if visiting.contains(&name) {
        return None;
    }
    visiting.push(name);
    let ed = &ws.modules[eid.module.index()].enums[eid.def];
    // Per position: every DISTINCT scalarizable value payload shape any variant
    // carries there. Empty = no such payload, so one opaque slot. Several shapes
    // share the position by width, each variant addressing its own leaves under
    // the tag -- the alternative, letting the losing variant fall back to the
    // opaque slot, would reintroduce an aggregate at the enum boundary.
    let mut want: Vec<Vec<ValueShape>> = Vec::new();
    let mut record_fields: Vec<Ustr> = Vec::new();
    for v in &ed.variants {
        match &v.payload {
            VariantPayload::Unit => {}
            VariantPayload::Tuple(tys) => {
                if want.len() < tys.len() {
                    want.resize(tys.len(), Vec::new());
                }
                for (i, ty) in tys.iter().enumerate() {
                    let Some(vs) = flat_payload_shape(ws, *ty, visiting) else { continue };
                    if !want[i].iter().any(|prev| prev.name == vs.name) {
                        want[i].push(vs);
                    }
                }
            }
            VariantPayload::Record(fields) => {
                for f in fields {
                    if !record_fields.contains(&f.name) {
                        record_fields.push(f.name);
                    }
                }
            }
        }
    }
    let slots = want
        .into_iter()
        .map(|w| if w.is_empty() { FlatSlot::Opaque } else { FlatSlot::Leaves(w) })
        .collect();
    visiting.pop();
    Some(EnumFlatShape { name, slots, record_fields })
}

/// The single static representation decision for an enum. The operational
/// cdata and flat shapes remain available as views, but neither caller chooses
/// between them independently.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnumLayout {
    PayloadFree,
    Cdata(EnumCdataShape),
    Flat(EnumFlatShape),
    Boxed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnumPlan {
    name: Ustr,
    layout: EnumLayout,
}

impl EnumPlan {
    pub fn for_name(ws: &Workspace, name: Ustr) -> Option<Self> {
        ws.enums_by_name.get(&name)?;
        let layout = if payload_free_enum_variants(ws, name).is_some() {
            EnumLayout::PayloadFree
        } else if let Some(shape) = build_enum_cdata_shape(ws, name) {
            EnumLayout::Cdata(shape)
        } else if let Some(shape) = build_flat_enum_shape(ws, name) {
            EnumLayout::Flat(shape)
        } else {
            EnumLayout::Boxed
        };
        Some(Self { name, layout })
    }

    pub fn name(&self) -> Ustr {
        self.name
    }

    pub fn layout(&self) -> &EnumLayout {
        &self.layout
    }

    pub fn cdata(&self) -> Option<&EnumCdataShape> {
        match &self.layout {
            EnumLayout::Cdata(shape) => Some(shape),
            _ => None,
        }
    }

    pub fn flat(&self) -> Option<&EnumFlatShape> {
        match &self.layout {
            EnumLayout::Flat(shape) => Some(shape),
            _ => None,
        }
    }

    pub fn is_payload_free(&self) -> bool {
        matches!(self.layout, EnumLayout::PayloadFree)
    }
}

/// The leaf shape of a scalarizable payload, else `None`.
///
/// Keyed on the DECLARED payload type, like every other splat decision on this
/// arc: both the ctor that writes the slots and the match arm that reads them
/// derive the layout from the enum declaration, so neither has to see the other.
fn flat_payload_shape(
    ws: &Workspace,
    ty: TyId,
    visiting: &mut Vec<Ustr>,
) -> Option<ValueShape> {
    let TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } = static_ty(ws, ty)? else {
        return None;
    };
    if map_is_value_struct(ws, n) {
        return sig_shape_of_nominal(ws, n);
    }
    build_flat_enum_shape_inner(ws, n, visiting).map(|shape| shape.value_shape())
}

pub(crate) fn flat_enum_equality_is_leafwise(ws: &Workspace, name: Ustr) -> bool {
    flat_enum_equality_is_leafwise_inner(ws, name, &mut Vec::new())
}

fn flat_enum_equality_is_leafwise_inner(
    ws: &Workspace,
    name: Ustr,
    visiting: &mut Vec<Ustr>,
) -> bool {
    if visiting.contains(&name) {
        return false;
    }
    let Some(eid) = ws.enums_by_name.get(&name) else {
        return false;
    };
    visiting.push(name);
    let ed = &ws.modules[eid.module.index()].enums[eid.def];
    let leafwise = ed.variants.iter().all(|variant| match &variant.payload {
        VariantPayload::Unit => true,
        VariantPayload::Tuple(tys) => {
            tys.iter().all(|ty| equality_payload_is_leafwise(ws, *ty, visiting))
        }
        VariantPayload::Record(fields) => {
            fields.iter().all(|field| equality_payload_is_leafwise(ws, field.ty, visiting))
        }
    });
    visiting.pop();
    leafwise
}

fn equality_payload_is_leafwise(
    ws: &Workspace,
    ty: TyId,
    visiting: &mut Vec<Ustr>,
) -> bool {
    let TyId::Static(mid, idx) = ty else {
        return false;
    };
    match ws.module(mid).ty_arena[idx] {
        TypeExpr::Builtin(
            BuiltinTy::Unit
            | BuiltinTy::Int
            | BuiltinTy::Uint
            | BuiltinTy::Float
            | BuiltinTy::Bool
            | BuiltinTy::String
            | BuiltinTy::Never,
        )
        | TypeExpr::Function { .. } => true,
        TypeExpr::Nullable(inner) => {
            equality_payload_is_leafwise(ws, TyId::Static(mid, inner), visiting)
        }
        TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } => {
            if map_is_value_struct(ws, n) {
                return true;
            }
            let Some(plan) = EnumPlan::for_name(ws, n) else {
                return false;
            };
            match plan.layout() {
                EnumLayout::PayloadFree | EnumLayout::Cdata(_) => true,
                EnumLayout::Flat(_) => flat_enum_equality_is_leafwise_inner(ws, n, visiting),
                EnumLayout::Boxed => false,
            }
        }
        TypeExpr::Builtin(BuiltinTy::Any | BuiltinTy::Error)
        | TypeExpr::Generic(..)
        | TypeExpr::Infer(..)
        | TypeExpr::App { .. }
        | TypeExpr::Tuple { .. } => false,
    }
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

    pub(crate) fn value_shape(&self) -> ValueShape {
        let mut fields = vec![(Ustr::from("__tag"), FieldShape::Prim)];
        for i in 0..self.tuple_width {
            fields.push((Ustr::from(format!("_{i}").as_str()), FieldShape::Prim));
        }
        for field in &self.record_fields {
            fields.push((*field, FieldShape::Prim));
        }
        ValueShape {
            name: self.enum_name,
            fields,
            nullable: false,
        }
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
    let shape = EnumPlan::for_name(ws, n).and_then(|plan| plan.flat().cloned())?;
    Some(YieldChannel {
        enum_name: shape.name,
        tuple_width: shape.tuple_width(),
        record_fields: shape.record_fields,
    })
}

/// The same channel oracle for a fully concrete logical coroutine type. The
/// declaration-shaped variant is retained for source-only analysis; emitted
/// coroutine bodies use this concrete form.
pub fn coroutine_yield_channel_mono(ws: &Workspace, ty: &MonoTy) -> Option<YieldChannel> {
    let (ty, nullable) = ty.peel_nullable();
    if nullable {
        return None;
    }
    let MonoTy::App {
        ctor: MonoCtor::Nominal(name),
        params,
    } = ty
    else {
        return None;
    };
    if !bone_semantic::prelude::is_intrinsic_key(*name, "Coroutine") {
        return None;
    }
    let MonoTy::App {
        ctor: MonoCtor::Nominal(yield_name),
        ..
    } = params.first()?
    else {
        return None;
    };
    let shape = EnumPlan::for_name(ws, *yield_name).and_then(|plan| plan.flat().cloned())?;
    Some(YieldChannel {
        enum_name: shape.name,
        tuple_width: shape.tuple_width(),
        record_fields: shape.record_fields,
    })
}

/// The reused frame slot name for the `i`th yield leaf: `__y{i}` (tag at 0, then
/// positional, then record leaves -- `value_shape` leaf order).
pub fn yield_slot(i: usize) -> Ustr {
    Ustr::from(format!("__y{i}").as_str())
}
