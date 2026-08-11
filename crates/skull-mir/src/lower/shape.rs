//! Static analysis of value- and enum-struct REPRESENTATION over a `Workspace`:
//! leaf shapes and their C layout, column eligibility, the specialization key and
//! its reified types, return/bound-`next` shapes, and the GC-vs-cdata-vs-leaf-table
//! tier decisions every emitter consults.
//!
//! Nothing here builds MIR. It answers "what does this type look like", which is
//! why four other crates depend on it: `value_struct_is_leaf_table` in particular
//! documents itself as THE oracle that the `_lct` ctor, the leaf-form pack site
//! and the flat read all consult, so the write and read encodings cannot diverge.

use super::*;

use bone_semantic::hir::{BuiltinTy, Expr, TyId, TypeCtor, TypeExpr, VariantPayload};
use bone_semantic::inference::Workspace;
use comfy_la_arena::{Idx, IdxRange};
use ustr::Ustr;

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

/// Whether `name` is an FFI-clean value STRUCT proper (C-layout cdata array
/// element on luajit) -- the element predicate for
/// [`ContainerKind::NativeBufferValue`](crate::mir::ContainerKind). Struct-only:
/// a cdata-eligible enum's value shape does not qualify (its element access
/// goes through match lowering, not `Field`).
pub fn is_ffi_clean_value_struct(ws: &Workspace, name: Ustr) -> bool {
    value_shape_rec(ws, name, &mut Vec::new()).is_some()
}

/// Max leaf count that stores as columns. Above it, columns fall off a measured
/// write cliff (9.3x on LuaJIT / 9.1x on V8 at K=16, x86; 17.1x / 11.7x on
/// arm64) and strided AoS wins -- so a wider type keeps today's representation
/// rather than silently taking the slow layout. Every collection site in the
/// tree is K <= 4. Numbers + the AoS tier: `docs/vec-value-columns-plan.md`.
pub(crate) const MAX_COLUMN_LEAVES: usize = 4;

/// The element name of `[E]` / `[E?]` eligible for flat column storage: `E` is
/// an all-scalar `value` struct whose COLUMN COUNT is at most
/// [`MAX_COLUMN_LEAVES`].
///
/// `nullable` adds the presence column to that count, because it is a column
/// like any other and the write cliff the constant guards is a function of how
/// many arrays a row touches. A nullable NESTED field already counts its
/// presence tag the same way (`leaf_paths_rec` emits it, `leaf_count` counts
/// it), so this is that rule at the top level, not a second budget.
///
/// `len` is excluded as a top-level leaf name because the columns share the
/// container table with the Vec's own `len` key, and a leaf column would
/// clobber the length. Only a TOP-LEVEL leaf can collide -- a nested leaf path
/// contains a `.`, which no identifier can. An excluded type is not an error:
/// it simply keeps the boxed representation.
pub fn vec_column_elem(ws: &Workspace, name: Ustr, nullable: bool) -> Option<Ustr> {
    let shape = value_shape_rec(ws, name, &mut Vec::new())?;
    let paths = value_leaf_paths(&shape);
    let columns = paths.len() + usize::from(nullable);
    (columns <= MAX_COLUMN_LEAVES && !paths.iter().any(|p| p.as_str() == "len")).then_some(name)
}

/// The one column decision over an already-resolved ELEMENT: leaf-shape
/// eligibility, then target availability.
///
/// Every container oracle below differs ONLY in how it gets from its own
/// `MonoTy` to `(element, nullable)`. What makes an element column-eligible is
/// this function and nothing else, so a container cannot pick up (or silently
/// miss) a rule its siblings have -- which is exactly how the Arena path came
/// to omit `columns_available`.
fn column_elem_payload(
    ws: &Workspace,
    target: bone_semantic::hir::FnBackend,
    elem: Ustr,
    nullable: bool,
) -> Option<ColumnElem> {
    vec_column_elem(ws, elem, nullable)
        .filter(|&name| columns_available(ws, target, name))
        .map(|name| ColumnElem { name, nullable })
}

/// Column eligibility for a `[E]`, decided on the CONCRETE type.
///
/// **The one oracle.** `Builder::vec_column_elem_of` is a thin caller of this,
/// and the instance graph calls it directly -- it holds no `Builder` and no
/// `SpecSubst`, so before the extraction it could not ask at all. The decision
/// no longer runs through the lossy `TypeExpr` adapter on either side.
///
/// A nullable element columnizes only when its payload is `nobox`: for an
/// unmarked value struct the boxed element is a legal representation that
/// already works, while columnizing it would route the materializing helpers
/// (`__rock_col_sort`, `to_lua_array`, serialization) at a shape they have no
/// presence concept for. The mark is what forbids the box, so the mark is what
/// obliges the presence column.
///
/// Nullability is read STRUCTURALLY here, which is why a spelled-out `[E?]` and
/// a generic bound to `E?` cannot answer differently: both are one `MonoTy`.
pub fn vec_column_elem_mono(
    ws: &Workspace,
    target: bone_semantic::hir::FnBackend,
    ty: &MonoTy,
) -> Option<ColumnElem> {
    let MonoTy::App { ctor: MonoCtor::Array, params } = ty else { return None };
    let (elem, nullable) = params.first()?.peel_nullable();
    let en = elem.nominal()?;
    if nullable && !is_nobox_nominal(ws, en) {
        return None;
    }
    column_elem_payload(ws, target, en, nullable)
}

/// The `Map<K, V>` sibling of [`vec_column_elem_mono`].
///
/// `Map<K, P?>` is column-stored too and, unlike `[P?]`, needs no presence
/// column: an absent value collapses to key REMOVAL, because a bare Map already
/// cannot tell an absent key from a key holding null. The KEY is deliberately
/// not consulted -- a value-struct key rejects upstream (LuaJIT hashes cdata by
/// pointer), and the spine stores whatever key the boxed representation stored.
///
/// The VALUE goes through [`column_elem_payload`] like every other container,
/// so it now consults `columns_available` too. That is inert while the filter
/// returns `true` unconditionally (verified: emitted Lua is byte-identical for
/// bitgun, mag and testbed), and it is the point -- a per-target availability
/// rule must not reach two of the three containers.
pub fn map_column_value_mono(
    ws: &Workspace,
    target: bone_semantic::hir::FnBackend,
    ty: &MonoTy,
) -> Option<ColumnElem> {
    let MonoTy::App { ctor: MonoCtor::Map, params } = ty else { return None };
    let (val, _) = params.get(1)?.peel_nullable();
    let vn = val.nominal()?;
    column_elem_payload(ws, target, vn, false)
}

/// The `Arena<E>` sibling of [`vec_column_elem_mono`].
///
/// An arena's `data` is an ordinary `[E]`, so it columnizes on exactly the same
/// rule and is decided by the same [`column_elem_payload`]. A second hand-rolled
/// walk lived in `Builder::arena_column_elem_of` and silently omitted the target
/// filter.
///
/// A nullable element is refused rather than given a presence column: the
/// `for`-over-arena walk reads `occupied`/`generations` for liveness and has no
/// presence concept, so admitting one would mint a column set its only reader
/// cannot address. That is also what the walk it replaces did.
pub fn arena_column_elem_mono(
    ws: &Workspace,
    target: bone_semantic::hir::FnBackend,
    ty: &MonoTy,
) -> Option<ColumnElem> {
    let MonoTy::App { ctor: MonoCtor::Nominal(name), params } = ty else { return None };
    if name.as_str() != "Arena" {
        return None;
    }
    let elem = params.first()?;
    if matches!(elem, MonoTy::Nullable(_)) {
        return None;
    }
    let en = elem.nominal()?;
    column_elem_payload(ws, target, en, false)
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
pub fn value_leaf_presence_flags_of(ws: &Workspace, name: Ustr) -> Option<Vec<bool>> {
    Some(value_leaf_presence_flags(&value_shape(ws, name)?))
}

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
pub fn leaf_addr_plan(ws: &Workspace, elem: Ustr) -> Result<Vec<LeafAddr>, String> {
    let (leaves, size, _align) = value_leaf_layout_checked(ws, elem)?;
    leaves
        .into_iter()
        .map(|l| {
            let (sz, _) = c_prim_size_align(l.cty)
                .ok_or_else(|| format!("leaf `{}` of `{elem}` is not primitive", l.path))?;
            if size % sz != 0 || l.byte_off % sz != 0 {
                return Err(format!(
                    "leaf `{}` of `{elem}` ({} bytes) does not divide its {sz}-byte view: \
                     stride {size}, offset {}",
                    l.path, sz, l.byte_off
                ));
            }
            Ok(LeafAddr {
                path: l.path,
                view_field: leaf_view_field(l.cty),
                stride: (size / sz) as i64,
                offset: (l.byte_off / sz) as i64,
            })
        })
        .collect()
}

/// `sizeof(elem)` as LuaJIT lays it out -- the per-element byte stride every
/// leaf view is strided against. The JS backend sizes its ArrayBuffer with it,
/// so the two backends' storage is the same bytes in the same places.
pub fn value_leaf_size(ws: &Workspace, elem: Ustr) -> Result<usize, String> {
    value_leaf_layout_checked(ws, elem).map(|(_, size, _)| size)
}

/// The C type a leaf VIEW field carries -- the inverse of [`leaf_view_field`].
/// The JS emitter needs it to pick the TypedArray and to know a `bool` view
/// stores 0/1 where the language wants `true`/`false`.
pub fn leaf_view_cty(field: &str) -> Option<&'static str> {
    LEAF_VIEWS.iter().find(|(_, f, _)| *f == field).map(|(t, ..)| *t)
}

/// The distinct leaf view C types of `elem`, in first-use order -- the views a
/// buffer of `elem` must carry.
pub fn leaf_view_ctys(ws: &Workspace, elem: Ustr) -> Result<Vec<&'static str>, String> {
    let (leaves, _, _) = value_leaf_layout_checked(ws, elem)?;
    let mut out: Vec<&'static str> = Vec::new();
    for l in leaves {
        if !out.contains(&l.cty) {
            out.push(l.cty);
        }
    }
    Ok(out)
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
    ("bool", 1),
    ("uint8_t", 1),
    ("int8_t", 1),
    ("uint16_t", 2),
    ("int16_t", 2),
    ("uint32_t", 4),
];

/// One `_G.__ROCK_NB_STORAGE` registration: `__rock_nb_storage(<ctype>, <byte
/// width>, { <view field> = "<cast type>", ... })`.
///
/// THE compiler-owned description of concrete buffer storage. A consumer that
/// holds a storage cdata asks the registry what its bytes mean -- element ctype,
/// element width, and the exact leaf views minted over it -- instead of guessing
/// from a `__ty` string or re-deriving a layout. The element widths and the view
/// set come from the same oracles the emitter itself uses
/// ([`NB_PRIM_ELEMS`], [`value_leaf_layout_checked`], [`LEAF_VIEWS`]), so a
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
pub fn nb_value_storage_lua(elem: Ustr, leaves: &[LeafSlot], size: usize) -> String {
    let mut views: Vec<&'static str> = Vec::new();
    for l in leaves {
        if !views.contains(&l.cty) {
            views.push(l.cty);
        }
    }
    nb_storage_call(elem.as_str(), size, &views)
}

/// [`value_leaf_paths`] by type name -- the `ValueShape`-side leaf order, for
/// callers outside this module that need to compare it against a layout walk
/// (`value_shape` itself is internal).
pub fn value_leaf_paths_of(ws: &Workspace, name: Ustr) -> Option<Vec<Ustr>> {
    Some(value_leaf_paths(&value_shape(ws, name)?))
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
        "int" | "float" => Some((4, 4)),
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
/// prevent. Its traversal mirrors [`value_shape_rec`] arm for arm so the leaf
/// ORDER matches [`value_leaf_paths`], but that is two separate walkers staying
/// in step, NOT something this function enforces. Leaves are matched to offsets
/// POSITIONALLY, so if they drift, every runtime `ffi.offsetof` assertion still
/// passes (each path/offset pair is self-consistent) while repr leaves are
/// written to the wrong addresses.
///
/// [`value_leaf_layout_checked`] is the only way out of this module, and it
/// REJECTS on drift. Keep it that way -- a `pub` raw walker is an offset source
/// a future lowering can reach for without the invariant.
pub(crate) fn value_leaf_layout(ws: &Workspace, name: Ustr) -> Option<(Vec<LeafSlot>, usize, usize)> {
    let mut out = Vec::new();
    let (size, align) = layout_rec(ws, name, "", 0, "", &mut out, &mut Vec::new())?;
    Some((out, size, align))
}

/// [`value_leaf_layout`] with the two-walker agreement enforced: the layout's
/// leaf path sequence must equal [`value_leaf_paths`] over the same type, which
/// is the invariant that lets leaves be matched to offsets positionally.
///
/// `Err` on either a missing layout or a disagreement. Both are invariant
/// failures for a type that reached an offset-consuming site, not conditions to
/// skip past -- skipping would let leaf addressing run on unvalidated offsets,
/// which is silent memory corruption rather than a failure.
pub fn value_leaf_layout_checked(
    ws: &Workspace,
    name: Ustr,
) -> Result<(Vec<LeafSlot>, usize, usize), String> {
    let Some((leaves, size, align)) = value_leaf_layout(ws, name) else {
        return Err(format!("`{name}` has no C leaf layout"));
    };
    let Some(shape) = value_shape(ws, name) else {
        return Err(format!("`{name}` has a C leaf layout but no value shape"));
    };
    let want = value_leaf_paths(&shape);
    let got: Vec<Ustr> = leaves.iter().map(|l| l.path).collect();
    if want != got {
        return Err(format!(
            "leaf walkers disagree for `{name}`: value_leaf_paths {want:?} vs \
             value_leaf_layout {got:?} -- leaves are matched to offsets by \
             position, so this would write every leaf to the wrong address"
        ));
    }
    Ok((leaves, size, align))
}

/// Append `name`'s leaves at `base` (a byte offset), returning its `(size, align)`.
/// `ofs` is the `ffi.offsetof` composition accumulated down to `name`'s start.
pub(crate) fn layout_rec(
    ws: &Workspace,
    name: Ustr,
    prefix: &str,
    base: usize,
    ofs: &str,
    out: &mut Vec<LeafSlot>,
    visiting: &mut Vec<Ustr>,
) -> Option<(usize, usize)> {
    // A cdata-eligible enum is a struct in its own right (`{ int __tag; <prim>
    // _0; .. }`), not a value struct -- `value_shape_rec` nests it, so this
    // must nest it identically.
    if let Some(es) = enum_value_shape(ws, name) {
        let mut off = base;
        let mut align = 4; // the `int __tag`
        let mut push = |nm: &str, cty: &'static str, off: &mut usize, align: &mut usize| {
            let (sz, al) = c_prim_size_align(cty)?;
            *off = round_up(*off, al);
            out.push(LeafSlot {
                path: leaf_path(prefix, nm),
                cty,
                byte_off: *off,
                offsetof_expr: join_offsetof(ofs, name, nm),
            });
            *off += sz;
            *align = (*align).max(al);
            Some(())
        };
        push("__tag", "int", &mut off, &mut align)?;
        for (nm, cty) in &es.slots {
            push(nm.as_str(), cty, &mut off, &mut align)?;
        }
        return Some((round_up(off - base, align), align));
    }

    let sid = *ws.struct_by_name.get(&name)?;
    let sd = &ws.modules[sid.module.index()].structs[sid.def];
    if !sd.is_value || visiting.contains(&name) {
        return None;
    }
    visiting.push(name);
    let mut off = base;
    let mut align = 1usize;
    for f in &sd.fields {
        let cty = value_field_c_type(ws, f.ty)?;
        match c_prim_size_align(&cty) {
            Some((sz, al)) => {
                // Leak-free: the only strings reaching here are the three
                // literals `value_field_c_type` returns for primitives.
                let cty: &'static str = match cty.as_str() {
                    "int" => "int",
                    "float" => "float",
                    "bool" => "bool",
                    _ => return None,
                };
                off = round_up(off, al);
                out.push(LeafSlot {
                    path: leaf_path(prefix, f.name.as_str()),
                    cty,
                    byte_off: off,
                    offsetof_expr: join_offsetof(ofs, name, f.name.as_str()),
                });
                off += sz;
                align = align.max(al);
            }
            // A nested value struct or a cdata enum: recurse under the dotted
            // prefix, aligning to the nested aggregate's own alignment.
            None => {
                let nested = Ustr::from(cty.as_str());
                let probe_at = round_up(off, nested_align(ws, nested, visiting)?);
                let (sz, al) = layout_rec(
                    ws,
                    nested,
                    &format!("{prefix}{}.", f.name),
                    probe_at,
                    &join_offsetof(ofs, name, f.name.as_str()),
                    out,
                    visiting,
                )?;
                off = probe_at + sz;
                align = align.max(al);
            }
        }
    }
    visiting.pop();
    Some((round_up(off - base, align), align))
}

/// The alignment of a nested aggregate, computed without emitting its leaves.
pub(crate) fn nested_align(ws: &Workspace, name: Ustr, visiting: &mut Vec<Ustr>) -> Option<usize> {
    let mut scratch = Vec::new();
    Some(layout_rec(ws, name, "", 0, "", &mut scratch, visiting)?.1)
}

/// Extend an `ffi.offsetof` composition by one field hop.
pub(crate) fn join_offsetof(prefix: &str, owner: Ustr, field: &str) -> String {
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValueShape {
    pub(crate) name: Ustr,
    pub(crate) fields: Vec<(Ustr, FieldShape)>,
    pub(crate) nullable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
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
    value_shape_rec(ws, name, &mut Vec::new())
        .or_else(|| enum_value_shape(ws, name).map(|e| e.value_shape()))
}

/// Whether `name` is a GC-FREE (all-scalar) value struct: every leaf is a
/// primitive or a nested all-scalar value struct, so no field is a boxed
/// string/array/nullable/generic slot. Such a struct is byte-value comparable,
/// so a plain-object emission plus structural deep-eq reproduces luajit's
/// value-eq exactly. Backends (skull-codegen-js T8b) gate value-struct emission
/// on this: a GC-carrying value struct compares its pointer fields by REFERENCE
/// on luajit, which a generic contents-eq cannot reproduce.
pub fn value_struct_is_gc_free(ws: &Workspace, name: Ustr) -> bool {
    value_shape(ws, name).is_some()
}

/// Declaration field order of `name` iff it is a GC-free value STRUCT proper --
/// NOT an enum value-shape (which `value_struct_is_gc_free` also accepts via the
/// `enum_value_shape` fallback). `value_shape_rec` is struct-only (it requires a
/// `struct_by_name` entry), so this cleanly separates the two: a genuine value
/// struct renders `Name { f: v }` colon-format on JS, an enum box still rejects.
pub fn gc_free_value_struct_field_order(ws: &Workspace, name: Ustr) -> Option<Vec<Ustr>> {
    value_shape_rec(ws, name, &mut Vec::new()).map(|_| struct_field_names(ws, name))
}

pub(crate) fn value_shape_rec(ws: &Workspace, name: Ustr, visiting: &mut Vec<Ustr>) -> Option<ValueShape> {
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
            // A NULLABLE `nobox` child threads its own presence tag as leaf 0 of
            // its nested shape -- the same rule the signature/local/global
            // contexts already apply at the outermost slot, reached through the
            // one resolver rather than re-spelled here. Without this arm the
            // field type fell to the catch-all and disqualified the WHOLE struct,
            // which is mechanism A: the parent then had no shape, fell back to a
            // box, and the materialization verifier refused it.
            //
            // Gated on `nobox` by the resolver: an UNMARKED nullable value field
            // keeps its box + nil sentinel, which is legal today (a box is not a
            // guarantee violation for an unmarked type), so this changes no
            // existing emission.
            TypeExpr::Nullable(_) => match nullable_nominal_shape(ws, f.ty, &|t| static_ty(ws, t))
            {
                Some(sub) => FieldShape::Nested(sub),
                None => {
                    visiting.pop();
                    return None;
                }
            },
            _ => {
                visiting.pop();
                return None;
            }
        };
        fields.push((f.name, fs));
    }
    visiting.pop();
    Some(ValueShape {
        name,
        fields,
        nullable: false,
    })
}

/// The scalar-ABI shape of a GC-CARRYING value struct (`value_struct_is_gc`):
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
    value_shape_gc_rec(ws, name, &mut Vec::new())
}

pub(crate) fn value_shape_gc_rec(ws: &Workspace, name: Ustr, visiting: &mut Vec<Ustr>) -> Option<ValueShape> {
    if !value_struct_is_gc(ws, name) {
        return None;
    }
    let sid = *ws.struct_by_name.get(&name)?;
    let sd = &ws.modules[sid.module.index()].structs[sid.def];
    if visiting.contains(&name) {
        return None;
    }
    visiting.push(name);
    let mut fields = Vec::with_capacity(sd.fields.len());
    for f in &sd.fields {
        let fs = match static_ty(ws, f.ty) {
            Some(TypeExpr::App { ctor: TypeCtor::Nominal(n), .. }) => {
                if payload_free_enum_variants(ws, n).is_some() {
                    FieldShape::Prim
                } else if let Some(es) = enum_value_shape(ws, n) {
                    FieldShape::Nested(es.value_shape())
                } else if map_is_value_struct(ws, n) {
                    // Nested value struct (either tier): thread its own leaves; a
                    // cyclic/odd shape falls back to one boxed slot.
                    match value_shape(ws, n).or_else(|| value_shape_gc_rec(ws, n, visiting)) {
                        Some(sub) => FieldShape::Nested(sub),
                        None => FieldShape::Prim,
                    }
                } else {
                    // Array / Arena / Map / other nominal collection -> one slot.
                    FieldShape::Prim
                }
            }
            // A NULLABLE `nobox` child threads leaves + presence, exactly as in
            // `value_shape_rec`. The two constructors are alternates for the SAME
            // struct, so they must agree on leaf COUNT: giving the arm to one and
            // not the other makes a producer and a consumer disagree, and
            // `rebuild_from_leaves` indexes past the end. That is not theoretical
            // -- it is what `a_nullable_nobox_field_round_trips_both_ways` (a
            // GC-tier outer) caught when only `value_shape_rec` had it.
            Some(TypeExpr::Nullable(_))
                if nullable_nominal_shape(ws, f.ty, &|t| static_ty(ws, t)).is_some() =>
            {
                FieldShape::Nested(
                    nullable_nominal_shape(ws, f.ty, &|t| static_ty(ws, t))
                        .expect("guard matched"),
                )
            }
            // Builtin scalar, generic `T`, non-nobox nullable, tuple -> one slot.
            _ => FieldShape::Prim,
        };
        fields.push((f.name, fs));
    }
    visiting.pop();
    Some(ValueShape {
        name,
        fields,
        nullable: false,
    })
}

/// Declared field names of struct `name`, in declaration order (the positional
/// `<name>_ct` ctor order). A cdata-eligible enum is `[__tag, _0]` (it rides the
/// value-struct `<Enum>_ct` path). Empty if `name` is neither.
pub fn struct_field_names(ws: &Workspace, name: Ustr) -> Vec<Ustr> {
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

/// The two rejection messages, shared so the emit-time validator
/// ([`super::vertex_expand_rejection`]) and the lowering expander cannot diverge
/// in what they accept.
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
         whose element is a concrete `value` struct -- the layout is derived from that \
         element, so an unsubstituted generic has no layout to derive"
    )
}

/// The `value`-struct element of a `FlatVec<E>` / `NativeBuffer<E>` expression,
/// outside a lowering [`Builder`]. Mirrors [`Builder::nb_value_elem`] minus the
/// specialization substitution, which the validator has no active
/// specialization for -- an unsubstituted generic is exactly what this must
/// reject anyway.
///
/// PREMISE (pre-monomorphization): source bodies can hold an
/// unsubstituted generic. Under totality none exist, this rejects a now-legal
/// program, and the validator must become instance-driven or be deleted.
pub(crate) fn flat_elem_of(ws: &Workspace, inf: &BodyInference, arg: Idx<Expr>) -> Option<Ustr> {
    let ty = resolve_tyid(ws, inf, *inf.expr_types.get(arg)?)?;
    let TypeExpr::App { ctor: TypeCtor::Nominal(n), params } = ty_node(ws, inf, ty)? else {
        return None;
    };
    if !matches!(n.as_str(), "NativeBuffer" | "FlatVec") {
        return None;
    }
    let elem = resolve_tyid(ws, inf, ty.with_idx(params.into_iter().next()?))?;
    match ty_node(ws, inf, elem)? {
        TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } => Some(n),
        _ => None,
    }
}

/// [`crate::gpu_layout::uniform_block`] with the two-walker agreement enforced:
/// the plan's leaf-path sequence must equal [`value_leaf_paths`] over the same
/// type. The std140 side derives its members from field TYPES (matching `Vec2`
/// and `Mat4` by nominal name), the value side flattens the struct into leaves,
/// and the packer binds one to the other BY POSITION -- so a reordered vector or
/// matrix keeps every count equal while writing each leaf to the wrong offset.
///
/// The exact twin of [`value_leaf_layout_checked`] for the uniform context, and
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

/// The value-struct name of expression `e` outside a lowering [`Builder`].
/// Mirrors [`Builder::value_struct_name_of`] minus the specialization
/// substitution, which the validator has no active specialization for -- an
/// unsubstituted generic is exactly what this must reject anyway.
///
/// PREMISE (pre-monomorphization): source bodies can hold an
/// unsubstituted generic. Under totality none exist, this rejects a now-legal
/// program, and the validator must become instance-driven or be deleted.
pub(crate) fn value_struct_name_at(
    ws: &Workspace,
    inf: &BodyInference,
    e: Idx<Expr>,
) -> Option<Ustr> {
    let ty = resolve_tyid(ws, inf, *inf.expr_types.get(e)?)?;
    let TypeExpr::App { ctor: TypeCtor::Nominal(name), .. } = ty_node(ws, inf, ty)? else {
        return None;
    };
    let sid = *ws.struct_by_name.get(&name)?;
    ws.modules[sid.module.index()].structs[sid.def].is_value.then_some(name)
}

/// The two `@uniform_expand` rejection messages, shared so the emit-time
/// validator and the lowering expander cannot diverge in what they accept.
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
         std140 layout is derived from it, so an unsubstituted generic has none. Reach this \
         through the `set_uniform_block` macro, which expands at the call site"
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

/// The scalarizable shape of a signature type (a param/return TyId): `Some` iff
/// it is a NON-nullable nominal value struct OR a flat-eligible (boxed) enum, or a
/// NULLABLE flat enum -- all thread as leaf scalars. A nullable flat enum threads
/// with `tag = nil` as the null sentinel (a present variant always has a non-nil
/// string tag, so no extra null bit is needed). Nullable VALUE structs still keep
/// the single-cdata ABI (cdata has no spare field for a nil discriminator).
pub(crate) fn sig_value_shape(ws: &Workspace, ty: TyId) -> Option<ValueShape> {
    match static_ty(ws, ty)? {
        TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } => sig_shape_of_nominal(ws, n),
        TypeExpr::Nullable(inner) => match static_ty(ws, ty.with_idx(inner))? {
            TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } => {
                sig_shape_of_nullable_nominal(ws, n)
            }
            _ => None,
        },
        _ => None,
    }
}

/// The shape policy behind [`sig_value_shape`]'s non-nullable arm, shared with the
/// body-local walker ([`Builder::body_sig_value_shape`]) so the two arenas can
/// never disagree about what threads as leaves.
pub(crate) fn sig_shape_of_nominal(ws: &Workspace, n: Ustr) -> Option<ValueShape> {
    value_shape(ws, n)
        .or_else(|| value_shape_gc(ws, n))
        .or_else(|| flat_enum_shape(ws, n).map(|s| s.value_shape()))
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
    flat_enum_shape(ws, n).map(|s| s.value_shape())
}

/// The value shape of a `name` nominal (value struct or flat enum), or `None`.
pub(crate) fn value_shape_of_nominal(ws: &Workspace, name: Ustr) -> Option<ValueShape> {
    value_shape(ws, name)
        .or_else(|| value_shape_gc(ws, name))
        .or_else(|| flat_enum_shape(ws, name).map(|s| s.value_shape()))
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

/// THE nullable-nominal shape rule, arena-independent: a `nobox` nominal resolves
/// to its leaf shape, and a NULLABLE one to the same shape with `nullable = true`
/// (presence is leaf 0). Everything else is `None`.
///
/// `resolve` is the caller's type-node lookup, which is the only thing that
/// differed between the copies this replaces: the workspace arena reads through
/// `static_ty`, a body arena through `resolve_tyid`/`ty_node`. Every context that
/// needs the rule calls this -- `nobox_declared_shape` (globals + top-level
/// signatures) and [`Builder::nobox_shape_of_ty`] (body-arena locals) -- so a new
/// context joins by calling it rather than by copying fifteen lines.
pub(crate) fn nullable_nominal_shape(
    ws: &Workspace,
    ty: TyId,
    resolve: &dyn Fn(TyId) -> Option<TypeExpr>,
) -> Option<ValueShape> {
    let nobox_shape = |name: Ustr| {
        is_nobox_nominal(ws, name).then(|| value_shape_of_nominal(ws, name)).flatten()
    };
    match resolve(ty)? {
        TypeExpr::App { ctor: TypeCtor::Nominal(name), .. } => nobox_shape(name),
        TypeExpr::Nullable(inner) => {
            let TypeExpr::App { ctor: TypeCtor::Nominal(name), .. } = resolve(ty.with_idx(inner))?
            else {
                return None;
            };
            let mut shape = nobox_shape(name)?;
            shape.nullable = true;
            Some(shape)
        }
        _ => None,
    }
}

/// The leaf shape of a `nobox`-typed DECLARED type (nullable included: presence
/// is leaf 0), `None` for every other type. bone requires an explicit annotation
/// on every global, so this is exact for a `GlobalDef`'s `ty`, and equally for a
/// top-level fn's param/return -- both are workspace-arena types `static_ty` can
/// see and the body-arena walker cannot.
pub(crate) fn nobox_declared_shape(ws: &Workspace, ty: TyId) -> Option<ValueShape> {
    nullable_nominal_shape(ws, ty, &|t| static_ty(ws, t))
}

pub(crate) fn nobox_declared_shape_spec(
    ws: &Workspace,
    subst: &SpecSubst,
    ty: TyId,
) -> Option<ValueShape> {
    if let TyId::Static(m, idx) = ty {
        match ws.module(m).ty_arena[idx] {
            // A generic bound to `E?` keeps its `?` in the BINDING, so the
            // declared slot is nullable and leads with a presence leaf. This is
            // still a DECLARED type read at a declaration (`fn .. -> T` at a site
            // that bound `T = E?`) -- never a nullability inferred off an
            // expression, which is what manufactures a phantom presence slot.
            TypeExpr::Generic(var, _) => {
                let (inner, nullable) = subst.get(&var)?.peel_nullable();
                let name = inner.nominal()?;
                let mut shape = is_nobox_nominal(ws, name)
                    .then(|| value_shape_of_nominal(ws, name))
                    .flatten()?;
                shape.nullable = nullable;
                return Some(shape);
            }
            TypeExpr::Nullable(inner) => {
                if let Some(TypeExpr::Generic(var, _)) = static_ty(ws, ty.with_idx(inner)) {
                    let name = spec_nominal(subst, var)?;
                    let mut shape = is_nobox_nominal(ws, name)
                        .then(|| value_shape_of_nominal(ws, name))
                        .flatten()?;
                    shape.nullable = true;
                    return Some(shape);
                }
            }
            _ => {}
        }
    }
    nobox_declared_shape(ws, ty)
}

/// The leaf globals a `nobox`-typed global is stored in, `None` for every other
/// global. THE single source for the emitted binding, the reads and writes, and
/// the reflection registry -- so `--dump-state` and snapshot restore address the
/// storage the program actually uses instead of a base name holding nil.
pub fn nobox_global_leaves(ws: &Workspace, name: Ustr, ty: TyId) -> Option<Vec<Ustr>> {
    let shape = nobox_declared_shape(ws, ty)?;
    Some((0..shape.leaf_count()).map(|i| nobox_global_leaf_name(name, i)).collect())
}

/// The value-struct type a `nobox` global's logical value has, plus its leaf
/// PATHS in ctor order -- what reflection needs to rebuild the logical value from
/// the leaf globals and to take it apart again. `None` for every other global,
/// on the same gate as [`nobox_global_leaves`], so accessors exist for exactly
/// the globals whose storage IS leaves.
///
/// Paths, not just an arity, because both directions address by path: the ctor is
/// positional over this sequence, and the write side assigns `v.<path>` per leaf.
///
/// The `bool` is whether the global is NULLABLE. Its presence leaf leads the
/// leaves but has no path -- presence is not addressable as `v.<something>` --
/// so the accessor must branch on it rather than read it, and the caller needs
/// to know the two sequences line up only from leaf 1 on.
pub fn nobox_global_shape_paths(ws: &Workspace, ty: TyId) -> Option<(Ustr, Vec<Ustr>, bool)> {
    let shape = nobox_declared_shape(ws, ty)?;
    Some((shape.name, value_leaf_paths(&shape), shape.nullable))
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
    /// From [`value_struct_is_leaf_table`] and [`value_struct_is_gc`] -- the
    /// oracles the `_lct` ctor and the leaf-form pack site already consult.
    pub ctor_suffix: &'static str,
    /// The path of a leaf that is a nullable child's PRESENCE tag, if any. From
    /// [`value_leaf_presence_flags`], positionally. Informational: a leaf-table
    /// ctor takes tags like any other leaf, so this does NOT by itself make a
    /// shape unbuildable.
    pub presence_path: Option<Ustr>,
    /// Why no accessor can be built, if so. The ctor is positional, so the
    /// accessor is only sound when the ctor's parameter sequence IS the leaf
    /// sequence. `_lct` is that by construction. `_ct` and `_vt` are positional
    /// over DECLARED FIELDS, which coincides with the leaves only while every
    /// field is primitive -- a nested value field makes the two differ in both
    /// arity and type, and the mismatch is silent (`Held_ct` fed six floats
    /// where it wants an `Inner` cdata and four floats).
    pub unbuildable: Option<String>,
}

pub fn nobox_global_accessor_shape(ws: &Workspace, ty: TyId) -> Option<NoboxGlobalAccessorShape> {
    let shape = nobox_declared_shape(ws, ty)?;
    let paths = value_leaf_paths(&shape);
    // The global's OWN presence leaf leads the leaves and has no path, so the
    // flags line up with `paths` from that offset on -- the same offset the
    // caller splits the leaves at.
    let flags = value_leaf_presence_flags(&shape);
    let flags = if shape.nullable { &flags[1..] } else { &flags[..] };
    let presence_path =
        flags.iter().zip(paths.iter()).find(|(is_tag, _)| **is_tag).map(|(_, p)| *p);
    let ctor_suffix = if value_struct_is_leaf_table(ws, shape.name) {
        "_lct"
    } else if value_struct_is_gc(ws, shape.name) {
        "_vt"
    } else {
        "_ct"
    };
    let unbuildable = (ctor_suffix != "_lct" && paths.iter().any(|p| p.contains('.'))).then(|| {
        format!(
            "`{}{ctor_suffix}` is positional over DECLARED FIELDS, but this type's \
             storage is {} leaves including a nested one (`{}`), so the accessor \
             would pass a leaf where the ctor wants a nested value",
            shape.name,
            paths.len(),
            paths.iter().find(|p| p.contains('.')).expect("just found one"),
        )
    });
    Some(NoboxGlobalAccessorShape {
        ty_name: shape.name,
        paths,
        nullable: shape.nullable,
        ctor_suffix,
        presence_path,
        unbuildable,
    })
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

/// The physical form an opaque body would WRONGLY assume for a container
/// parameter, or `None` when the boxed assumption still holds.
///
/// One oracle for every container constructor 2.4 names
/// (`docs/boundary-contracts.md`), so the rejection rule cannot drift from the
/// storage lowering actually selects.
///
/// The split is not stylistic:
///
/// - `[E]`/`Vec<E>` is per-leaf column arrays with no `_data`, and `Map<K, E>`
///   at a column-eligible `E` is a spine wrapper rather than a bare key table.
///
/// - `Arena<E>`'s `data` IS a column vec at a column-eligible `E`, on every
///   target, so an opaque body's `a.data._data` is nil everywhere.
/// - `FlatVec<E>` and `NativeBuffer<E>` are `{ptr, len}` over C storage. On
///   luajit `ptr` is an `E*`, so `ptr[i]` is an element REFERENCE and the body
///   is right; JS backs them with flat slots, where `ptr[i]` is a leaf. Hence a
///   JS-only rejection, the same asymmetry the spec-key trigger already makes.
/// The nominal container constructors 2.4 covers. Asked BEFORE the element is
/// resolved: reading a concrete type is not free of consequence -- `concrete_ty`
/// reports "bone typed no expression here" as an error -- so a `Sampler2D`
/// parameter must never be asked in the first place.
pub fn is_nominal_container(name: Ustr) -> bool {
    matches!(name.as_str(), "Arena" | "FlatVec" | "NativeBuffer")
}

/// The name [`opaque_container_hazard`] knows a builtin container by.
pub const VEC_CONTAINER: &str = "Vec";
pub const MAP_CONTAINER: &str = "Map";

pub fn opaque_container_hazard(
    ws: &Workspace,
    target: bone_semantic::hir::FnBackend,
    container: Ustr,
    elem: Ustr,
    nullable: bool,
) -> Option<&'static str> {
    let js = target == bone_semantic::hir::FnBackend::Js;
    // The element's own nullability, NOT either flavour: an unmarked `[BV2?]`
    // is a boxed vec while its non-nullable twin `[BV2]` is columns, and a
    // REJECTION may not over-approximate the way a spec-key trigger could.
    let columns =
        vec_column_elem(ws, elem, nullable).is_some() && columns_available(ws, target, elem);
    match container.as_str() {
        "Vec" if columns => Some("`_data`"),
        "Map" if columns => Some("a bare key table"),
        "Arena" if columns => Some("`data._data`"),
        "FlatVec" | "NativeBuffer" if js && leaf_addr_plan(ws, elem).is_ok() => {
            Some("a per-element view over `ptr`")
        }
        _ => None,
    }
}

/// Whether `[elem]` takes column storage when lowering for `target`.
///
/// Every target now serves columns, so this is unconditional. It survives as a
/// named predicate because [`opaque_container_hazard`] asks it per container
/// constructor, where the answer decides a rejection.
pub(crate) fn columns_available(
    _ws: &Workspace,
    _target: bone_semantic::hir::FnBackend,
    _elem: Ustr,
) -> bool {
    true
}

/// A callee's instantiation at one call site: one concrete type per declared
/// generic, in declaration order. TOTAL by construction.
///
/// Absence lives OUTSIDE this type, in the `Option<SpecKey>` every producer
/// returns -- `None` means no specialized delivery. It used to live inside, as
/// a per-slot `Option`, and that shape admitted two things nothing wanted. A
/// PARTIAL key named neither the shared body nor one instantiation (measured:
/// 267,671 total constructions and zero partial), and an EMPTY key was the
/// shared-body sentinel for a non-generic callee -- 756 of them, normalized
/// back to `Shared` by `EmittedBodyId::new` rather than decided by whoever
/// built it.
///
/// The empty case is the one that had to go. A non-generic concrete instance
/// legitimately has no arguments, so "empty means shared" is a conflation that
/// cannot survive into an `Instance`-keyed identity. Producers decide absence
/// now; nothing normalizes it afterwards.
///
/// This is `skull::instance_graph::Instance`'s `args` in everything but
/// interning -- a PREMISE that the narrowing is safe, not evidence the two
/// identities are one. That convergence is the cache-key step, and it is named
/// in `docs/parked/mono.md`.
pub type SpecKey = Vec<MonoTy>;

/// Callee generic `VarId` -> its concrete type tree, for specialized lowering.
/// Total against the declaration's generics, exactly as [`SpecKey`] is.
///
/// THE SAME TYPE as [`crate::monoty::MonoSubst`], which is what an instance of
/// the mono graph carries. One substitution vocabulary, so an instance's own
/// bindings drive lowering and the ABI plan directly rather than through a
/// conversion that could lose or reorder them.
pub type SpecSubst = crate::monoty::MonoSubst;

/// The root `TypeExpr` a substituted generic resolves to. Nested arguments stay
/// in [`SpecSubst`] for recursive call specialization; the synthesized root has
/// no arena range of its own.
/// A NULLABLE binding substitutes to its PAYLOAD, which is what the arena spells
/// for a written-out `E?` too: `resolve_ty` peels `Nullable`, so `expr_ty` at a
/// `E?` expression already answers `E`, and nullability is a separate question
/// ([`Builder::is_nullable_ty`], which grows the matching generic arm). Modelling
/// it any other way would need a `TypeExpr::Nullable` node, and that holds an
/// arena index this has no arena to mint.
/// The substituted type NODE for a binding, or `None` where no node can stand
/// for it.
///
/// A tuple is the `None`: `TypeExpr::Tuple` holds an `IdxRange` into a type
/// arena, and there is none to mint here. Answering with an EMPTY range would be
/// far worse than answering nothing -- `tyid_tuple_ret_shape` would read a
/// 0-slot tuple and `unit_ty` would read the binding as unit, both silently. The
/// caller falls back to the unsubstituted generic, which is exactly what it saw
/// before tuples reified at all; the consumers that need a tuple's slots read
/// the binding directly through [`mono_ret_slot`].
///
/// `any`, unit, `never` and a function type answer `None` on the same terms.
/// They are perfectly good [`SpecKey`] slots -- identity names them -- but the
/// arena has no node to spell them with here, and inventing one would be a
/// guess. The caller falls back to the unsubstituted generic, which is what a
/// consumer reading a CONSTRUCTOR or a NOMINAL wants anyway.
pub(crate) fn mono_type_node(ty: &MonoTy) -> Option<TypeExpr> {
    Some(match ty {
        MonoTy::Nullable(inner) => return mono_type_node(inner),
        MonoTy::Tuple(_) => return None,
        MonoTy::Any | MonoTy::Unit | MonoTy::Never | MonoTy::Function { .. } => return None,
        MonoTy::App { ctor, .. } => TypeExpr::App {
            ctor: match ctor {
                MonoCtor::Array => TypeCtor::Array,
                MonoCtor::Map => TypeCtor::Map,
                MonoCtor::Nominal(name) => TypeCtor::Nominal(*name),
            },
            params: IdxRange::new_empty(),
        },
        MonoTy::Int => TypeExpr::Builtin(BuiltinTy::Int),
        MonoTy::Float => TypeExpr::Builtin(BuiltinTy::Float),
        MonoTy::Bool => TypeExpr::Builtin(BuiltinTy::Bool),
        MonoTy::String => TypeExpr::Builtin(BuiltinTy::String),
    })
}

/// The [`RetSlot`] of a binding, read straight off the reified type.
///
/// The counterpart to [`ret_slot`] for the one case that has no type node to go
/// through: a generic bound to a TUPLE. Mirrors [`nobox_sig_shape_spec`]'s
/// policy exactly -- only a `nobox` nominal splats, and a nullable one takes the
/// nullable shape -- so a tuple slot answers the same as the spelled-out type in
/// that position would.
pub(crate) fn mono_ret_slot(ws: &Workspace, ty: &MonoTy) -> RetSlot {
    if let MonoTy::Tuple(parts) = ty {
        // The same collapse [`ret_slot`] makes: a tuple no part of which splats
        // is the pre-existing boxed ABI, so it occupies ONE slot. Without it the
        // two oracles disagree on a NESTED tuple -- the generic side counted an
        // inner `(Entity<T>, T)` as two wire values while the concrete callee
        // returned it as one box.
        let shape = mono_tuple_shape(ws, parts);
        return if shape.splats() { RetSlot::Nested(shape) } else { RetSlot::Opaque };
    }
    let (payload, nullable) = ty.peel_nullable();
    let Some(nominal) = payload.nominal().filter(|n| is_nobox_nominal(ws, *n)) else {
        return RetSlot::Opaque;
    };
    let shape = if nullable {
        sig_shape_of_nullable_nominal(ws, nominal)
    } else {
        sig_shape_of_nominal(ws, nominal)
    };
    shape.map_or(RetSlot::Opaque, RetSlot::Leaves)
}

/// The per-part slots of a reified tuple, WITHOUT the collapse
/// [`mono_ret_slot`] applies. Read where the tuple's arity is the question --
/// a destructure, a `next`'s `(bool, E)` -- rather than what slot it occupies.
pub(crate) fn mono_tuple_shape(ws: &Workspace, parts: &[MonoTy]) -> TupleRetShape {
    let slots = parts.iter().map(|part| mono_ret_slot(ws, part)).collect();
    TupleRetShape { slots, nullable: false }
}

/// The concrete binding for `ty` under `enclosing`, or `None` when no concrete
/// type exists here at all.
///
/// ONE oracle answers this and the identity question alike
/// ([`crate::monoty::convert`]), which is the whole content of plan S 2a/S 2b.
/// It used to be a second, narrower walk whose `_ => None` arm declined valid
/// `any`, unit, `never` and function types -- and `None` is not a type, it is the
/// ABSENCE of one, shared by every binding that landed in it. So three
/// instantiations at `unit`, `any` and `fn(int) -> int` mangled to one symbol
/// and one emitted body served three representations. `MonoTy` has a
/// constructor for each of the four, so there was never anything to represent;
/// the hole was the admission.
///
/// `None` now means exactly what [`crate::monoty::MonoTyReason`] means: a
/// generic absent from `enclosing` (this is a shared generic body), an
/// unresolved inference var, recovery poison, or a missing node. Those are
/// compiler states, not language surface, and [`ablation_partial_key_decline`]
/// reads the reason rather than re-deriving the predicate.
pub(crate) fn reify_type(
    ws: &Workspace,
    inf: &BodyInference,
    ty: TyId,
    enclosing: &SpecSubst,
) -> Option<MonoTy> {
    crate::monoty::convert(ws, inf, ty, enclosing).ok()
}

/// The specialization decision for a call `e` (in a body with inference `inf`) to
/// `func`: `Some((key, subst))` iff `func` is a splice-able generic `@inline`/
/// `@pure` callee whose per-site bindings (`call_generic_bindings[e]`) include at
/// least one VALUE-struct leaf. The single source consulted by both the caller
/// lowering ([`Builder::call_spec`]) and the site-aware rejection pass, so the two
/// never disagree on which calls specialize.
///
/// `Err` is a COMPILER-state rejection, never a language boundary: bone records
/// `call_generic_bindings` for every successfully resolved generic direct call,
/// so its absence here means the resolution that produced this `FuncRef` and the
/// inference that recorded the bindings disagree. The synthesized `for`-protocol
/// `next` is not caught by that: it carries `next_generics` and never reaches
/// this function ([`spec_key_from_next_generics`] is its entry).
/// The CONCRETE INSTANCE a generic direct call targets, independent of whether
/// the lowerer chooses to deliver a specialized body for it.
///
/// [`compute_call_spec`] answers a POLICY question -- is a specialized snapshot
/// worth splicing here -- and returns `None` when the answer is no. Reading that
/// `None` as "the callee instance is unknown" is what made a declined
/// specialization indistinguishable from a call whose instantiation was never
/// inferred. This answers the IDENTITY question, which has no policy in it: the
/// call's own bindings, reified under the enclosing substitution.
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
    let Some(bindings) = inf.call_generic_bindings.get(&e) else { return Ok(None) };
    instance_subst_from_bindings(ws, inf, func, bindings, enclosing)
}

/// The concrete instance a set of generic bindings resolves to.
///
/// Converts through [`crate::monoty::convert`] -- the one TOTAL logical-type
/// oracle, which `skull::instance_graph::instantiate` and the delivery-side
/// [`reify_type`] also read. Two converters for one question is what lets a
/// record and the graph disagree while both look complete.
///
/// The two UNCONSTRAINED reasons are legitimate `None`s, and they are exactly
/// the pair `instance_graph::convert_fail` calls `Fail::Unconstrained`:
/// `UnboundGeneric` means the enclosing body is a shared generic snapshot, so
/// this call site decides no instantiation; `UnresolvedInfer` means no
/// constraint reaches the slot at all, which plan S 0h decided to REJECT and
/// deferred to the Stage-3 cutover -- rejecting it here instead would make it
/// fatal ahead of that decision. Both are what the census's
/// `callee-no-instance` counts. Every other reason is a compiler-state
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
            Err(MonoTyReason::UnboundGeneric(_) | MonoTyReason::UnresolvedInfer(_)) => {
                return Ok(None)
            }
            Err(reason) => {
                return Err(CompilerError::new_with_label(
                    f.name_span,
                    format!(
                        "internal: a call to `{}` supplies a type argument that is not a logical                          type ({}), so the instance it names cannot be stated",
                        f.name,
                        reason.bucket()
                    ),
                ))
            }
        }
    }
    Ok(Some(subst))
}

pub(crate) fn compute_call_spec(
    ws: &Workspace,
    inf: &BodyInference,
    e: Idx<Expr>,
    func: FuncRef,
    enclosing: &SpecSubst,
) -> Result<Option<(SpecKey, SpecSubst)>, CompilerError> {
    let f = &ws.module(func.module).functions[func.func];
    if f.generics.is_empty() {
        return Ok(None);
    }
    // A `co fn` IS eligible, and it is the one eligible callee the inliner never
    // sees: the call constructs a coroutine object rather than running the body,
    // so specialization is delivered by a standalone definition put through
    // `coro_transform` (`skull::mir_emit::emit_coroutine_chunk`) rather than by a
    // splice. `extern` and `inline(..)` stay excluded -- an opaque body has no
    // Rock CFG to specialize at all.
    if f.is_extern || f.inline_body.is_some() {
        return Ok(None);
    }
    let Some(bindings) = inf.call_generic_bindings.get(&e) else {
        return Err(CompilerError::new_with_label(
            f.name_span,
            format!(
                "internal: no recorded generic bindings for a resolved call to generic `{}`. \
                 Inference records a binding list for every resolved generic direct call, so \
                 this call resolved to a callee its own instantiation was never inferred for.",
                f.name
            ),
        ));
    };
    // Every complete instantiation specializes. The `@inline`/`@pure` opt-in and
    // the column-storage trigger were OPTIMIZATION gates over a shared generic
    // body, choosing when a second definition was worth minting; with emission
    // owned by the instance there is no shared body for a declined call to reach,
    // so there is nothing left to gate.
    Ok(compute_spec_from_bindings(ws, inf, func, bindings, enclosing)?)
}

/// Why a generic Rock call would keep the SHARED body, classified into an S 0e
/// category -- or `None` when the call does specialize and there is nothing to
/// report.
///
/// Separate from [`compute_call_spec`] because the classification needs no
/// context and the RECORDING needs plenty (the caller's name, the call's span),
/// which only the `Builder` has. Keeping the discriminants here keeps them
/// beside the gates they describe, so a gate change and its classification
/// cannot drift apart.
pub(crate) fn ablation_decline(
    ws: &Workspace,
    inf: &BodyInference,
    e: Idx<Expr>,
    func: FuncRef,
    enclosing: &SpecSubst,
) -> Option<(crate::ablation::FatalCategory, String)> {
    use crate::ablation::FatalCategory as C;
    let f = &ws.module(func.module).functions[func.func];
    if f.generics.is_empty() {
        return None;
    }
    if f.is_extern || f.inline_body.is_some() {
        return Some((
            C::TargetBoundary,
            "a generic foreign body (`extern` / `inline(..)`): under totality this must be \
             an explicit BoundaryId, never a shared generic Rock body"
                .to_string(),
        ));
    }
    // An `Iterator`-bounded body is never emitted standalone -- it exists only
    // as an inline splice (`skull_transform::inline::classify`). With inlining
    // disabled it has no concrete form at all, which is plan S 12's subject.
    if f.generic_bounds.iter().any(Option::is_some) {
        return Some((
            C::UnsupportedSurface,
            "an `Iterator`-bounded body exists ONLY as an inline splice: its body holds \
             placeholder bound-calls and it is never emitted standalone (plan S 12)"
                .to_string(),
        ));
    }
    let Some(bindings) = inf.call_generic_bindings.get(&e) else {
        return Some((
            C::MissingGraphEdge,
            "no recorded generic bindings at this call, so the instantiation cannot be \
             named"
                .to_string(),
        ));
    };
    if bindings.len() != f.generics.len() {
        return Some((
            C::CompilerInvariant,
            format!(
                "declares {} generic(s) but the call site supplied {} binding(s)",
                f.generics.len(),
                bindings.len()
            ),
        ));
    }
    for g in &f.generics {
        if !matches!(ty_node(ws, inf, *g), Some(TypeExpr::Generic(..))) {
            return Some((
                C::CompilerInvariant,
                "a declared generic is not a generic type variable".to_string(),
            ));
        }
    }
    ablation_partial_key_decline(ws, inf, bindings, enclosing)
}

/// Why a binding list cannot produce a COMPLETE [`SpecKey`] -- or `None` when
/// every slot converts and the key names exactly one instantiation.
///
/// There is no "valid type the key cannot name" arm, and its absence is
/// structural rather than an omission: the key is built from
/// [`crate::monoty::convert`] (see [`reify_type`]), so a binding that converts
/// IS in the key. Plan S 2a's category emptied when the two walks became one --
/// while they were separate, this function's discriminant was the disagreement
/// between them, and there is none to read now.
///
/// So every remaining reason is a compiler state, and the split is which repair
/// it calls for: an unbound generic says the CALLER is a shared generic body
/// (Stage 3 work), and everything else is an invariant failure.
pub(crate) fn ablation_partial_key_decline(
    ws: &Workspace,
    inf: &BodyInference,
    bindings: &[TyId],
    enclosing: &SpecSubst,
) -> Option<(crate::ablation::FatalCategory, String)> {
    use crate::ablation::FatalCategory as C;
    use crate::monoty::MonoTyReason;
    let reason = bindings
        .iter()
        .find_map(|b| crate::monoty::convert(ws, inf, *b, enclosing).err())?;
    Some(match reason {
        // The enclosing instance never named this generic, so the caller itself
        // is a shared generic body -- a graph gap, not a key gap.
        MonoTyReason::UnboundGeneric(name) => (
            C::MissingGraphEdge,
            format!(
                "the binding names generic `{name}`, absent from the enclosing \
                 substitution: this call is being lowered inside a SHARED generic body, so \
                 no instantiation exists to key on"
            ),
        ),
        r => (
            C::CompilerInvariant,
            format!(
                "a generic binding does not convert to a logical type ({}): inference \
                 state, not a language surface",
                r.bucket()
            ),
        ),
    })
}

/// [`compute_call_spec`] against an explicit binding list instead of a call
/// expr. The for-protocol `next` call is SYNTHESIZED and has no expr to key
/// `call_generic_bindings` by, so it supplies its bindings from
/// `BoundNext::Concrete::next_generics` (which bone records at resolution).
/// Shared body so an iterator call and a direct call can never disagree on
/// which instantiations specialize.
///
/// Both `Err`s are compiler-state rejections: a binding list is built per callee
/// so it has that callee's arity, and every entry of `Function::generics` is the
/// `TypeExpr::Generic` the declaration minted. A silent `None` here would leave
/// the generic absent from the [`SpecSubst`] while the key still claimed the
/// instantiation.
pub(crate) fn compute_spec_from_bindings(
    ws: &Workspace,
    inf: &BodyInference,
    func: FuncRef,
    bindings: &[TyId],
    enclosing: &SpecSubst,
) -> Result<Option<(SpecKey, SpecSubst)>, CompilerError> {
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
    // A non-generic declaration has no instantiation to name. It used to return
    // `Some(vec![])` and rely on `EmittedBodyId::new` reading empty as "the
    // shared body" -- absence decided two layers away from the producer, by a
    // sentinel that a non-generic `Instance` legitimately collides with.
    // A non-generic declaration has no instantiation to name. It used to return
    // `Some(vec![])` and rely on `EmittedBodyId::new` reading empty as "the
    // shared body" -- absence decided two layers away from the producer, by a
    // sentinel that a non-generic `Instance` legitimately collides with.
    if f.generics.is_empty() {
        return Ok(None);
    }
    let mut key: SpecKey = Vec::with_capacity(bindings.len());
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
        // One unreified slot and there is no key at all. A partial key named the
        // DECLARATION through `spec_fn_name`, i.e. the same absent body a
        // missing key names, so building one was never a third answer.
        let Some(ty) = reify_type(ws, inf, *b, enclosing) else { return Ok(None) };
        subst.insert(var, ty.clone());
        key.push(ty);
    }
    // EVERY complete key specializes. There is no shared generic body to decline
    // to any more, so a declined key is not a slower lowering -- it is a call
    // spelling a name nothing defines.
    //
    // An INCOMPLETE instantiation is the Stage-2 `ConcreteTypeUnavailable`
    // rejection's business, and it left through the `reify_type` arm above.
    Ok(Some((key, subst)))
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
) -> Result<Option<SpecKey>, CompilerError> {
    let f = &ws.module(func.module).functions[func.func];
    let mut bindings = Vec::with_capacity(f.generics.len());
    for generic in &f.generics {
        let Some(TypeExpr::Generic(var, _)) = ty_node(ws, inf, *generic) else {
            return Err(CompilerError::new_with_label(
                f.name_span,
                format!(
                    "internal: declared generic of `{}` is not a generic type variable",
                    f.name
                ),
            ));
        };
        // A generic the iterator resolution left unbound is not an invariant
        // failure -- the `next` witness binds only what its own bound names.
        let Some(bound) = next_generics.get(&var).copied() else { return Ok(None) };
        bindings.push(bound);
    }
    Ok(compute_spec_from_bindings(ws, inf, func, &bindings, enclosing)?.map(|(key, _)| key))
}

/// [`ablation_decline`] for the for-protocol `next` witness, whose bindings come
/// from `next_generics` rather than a call expr.
///
/// This path had NO classifier: [`spec_key_from_next_generics`] answered `None`
/// for an unbound witness generic and the call took the shared body with nothing
/// recorded. Under totality that is a fatal like any other, and leaving it
/// silent is how the census under-reports the very witness gap S 0e is meant to
/// price.
pub(crate) fn ablation_next_decline(
    ws: &Workspace,
    inf: &BodyInference,
    func: FuncRef,
    next_generics: &rustc_hash::FxHashMap<bone_semantic::hir::VarId, TyId>,
    enclosing: &SpecSubst,
) -> Option<(crate::ablation::FatalCategory, String)> {
    use crate::ablation::FatalCategory as C;
    let f = &ws.module(func.module).functions[func.func];
    let mut bindings = Vec::with_capacity(f.generics.len());
    for generic in &f.generics {
        let Some(TypeExpr::Generic(var, name)) = ty_node(ws, inf, *generic) else {
            return Some((
                C::CompilerInvariant,
                "a declared generic is not a generic type variable".to_string(),
            ));
        };
        let Some(bound) = next_generics.get(&var).copied() else {
            return Some((
                C::MissingGraphEdge,
                format!(
                    "the iterator witness records no binding for generic `{name}`: bone \
                     records `next_generics` only for what the `next` bound itself names \
                     (plan S 12)"
                ),
            ));
        };
        bindings.push(bound);
    }
    ablation_partial_key_decline(ws, inf, &bindings, enclosing)
}

/// Rebuild the [`SpecSubst`] (callee generic `VarId` -> concrete type) for `fref`
/// from a [`SpecKey`] (trees in declaration order). The inliner holds only the
/// arena-free key in its cache; this re-pairs it with the callee's generic
/// `VarId`s to drive [`lower_emitted_body`]. An empty / all-`None` key yields an
/// empty subst (the shared snapshot).
/// The inverse of [`spec_subst_from_key`], and the E1 cut's premise: for a body
/// emission actually writes, a [`SpecKey`] carries nothing the [`SpecSubst`]
/// does not.
///
/// `Some` only for a TOTAL rebuild -- every declared generic present in `subst`
/// -- so the caller cannot mistake a partial answer for a key. That is the same
/// completeness `compute_spec_from_bindings` demands before it returns a key at
/// all, and it is what makes the two directions a round trip rather than a pair
/// of lossy projections.
///
/// The declaration supplies the ORDER, which is the only thing a substitution
/// lacks: a key is positional over `f.generics` and a subst is keyed by `VarId`.
pub fn spec_key_from_subst(ws: &Workspace, fref: FuncRef, subst: &SpecSubst) -> Option<SpecKey> {
    let f = &ws.module(fref.module).functions[fref.func];
    if f.generics.is_empty() {
        return None;
    }
    let mut key = SpecKey::with_capacity(f.generics.len());
    for g in f.generics.iter() {
        let Some(TypeExpr::Generic(var, _)) = static_ty(ws, *g) else { return None };
        key.push(subst.get(&var)?.clone());
    }
    Some(key)
}

pub fn spec_subst_from_key(ws: &Workspace, fref: FuncRef, key: &SpecKey) -> SpecSubst {
    let f = &ws.module(fref.module).functions[fref.func];
    let mut subst = SpecSubst::default();
    for (g, reified) in f.generics.iter().zip(key) {
        if let Some(TypeExpr::Generic(var, _)) = static_ty(ws, *g) {
            subst.insert(var, reified.clone());
        }
    }
    subst
}

/// [`sig_value_shape`] under a specialization substitution: a param/return type
/// that is a generic bound to a value-struct leaf resolves to that leaf's shape.
/// A `T?` resolves the same way through the NULLABLE policy, so a generic
/// nullable inherits the same asymmetry a spelled-out one has
/// ([`sig_shape_of_nullable_nominal`]) instead of falling through to
/// [`sig_value_shape`], whose `Nullable` arm matches no nominal for a generic
/// inner. Every non-generic type falls through to the plain [`sig_value_shape`].
pub(crate) fn sig_value_shape_spec(ws: &Workspace, subst: &SpecSubst, ty: TyId) -> Option<ValueShape> {
    if let TyId::Static(m, idx) = ty {
        match ws.module(m).ty_arena[idx] {
            // A generic bound to `E?` carries the `?` in the BINDING, so it takes
            // the nullable policy exactly as a spelled-out one does.
            TypeExpr::Generic(var, _) => {
                return match subst.get(&var)?.peel_nullable() {
                    (inner, false) => value_shape_of_nominal(ws, inner.nominal()?),
                    (inner, true) => sig_shape_of_nullable_nominal(ws, inner.nominal()?),
                };
            }
            TypeExpr::Nullable(inner) => {
                if let Some(TypeExpr::Generic(var, _)) = static_ty(ws, ty.with_idx(inner)) {
                    let (payload, _) = subst.get(&var)?.peel_nullable();
                    return sig_shape_of_nullable_nominal(ws, payload.nominal()?);
                }
            }
            _ => {}
        }
    }
    sig_value_shape(ws, ty)
}

/// The nominal `var` is bound to under `subst`, or `None` when it is unbound or
/// bound to a non-nominal (a tuple, a fn type, a primitive).
pub(crate) fn spec_nominal(subst: &SpecSubst, var: bone_semantic::hir::VarId) -> Option<Ustr> {
    subst.get(&var)?.nominal()
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
    /// Only ever set from a declared return type ([`tuple_ret_shape`]), never
    /// inferred from an expression's type: safe-nav manufactures the identical
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

/// The multi-return ABI of a tuple RETURN type: `Some` iff `ty` is a tuple
/// `(A, B, ...)`, which returns its slots as separate values instead of one heap
/// table (slice 1, `docs/iterator-implementation-plan.md`).
///
/// A `Nullable(Tuple)` joins that wire with a presence bool leading its slots --
/// but ONLY when an element actually splats. A nullable tuple of ordinary
/// elements has nothing to gain from the multi-value form and keeps its single
/// boxed table (and `nil`), byte-for-byte the pre-existing ABI. THE declared-type
/// entry point: `TupleRetShape::nullable` is set here and nowhere else.
pub(crate) fn tuple_ret_shape(ws: &Workspace, subst: &SpecSubst, ty: TyId) -> Option<TupleRetShape> {
    // A generic bound to a tuple has no type node to read (see
    // `mono_type_node`), so its slots come off the binding itself. Only here:
    // this is the sole consumer that needs a tuple's arity, and answering it
    // from the binding keeps `ty_node` honest about what it cannot mint.
    if let Some(TypeExpr::Generic(var, _)) = static_ty(ws, ty) {
        if let Some(MonoTy::Tuple(parts)) = subst.get(&var) {
            return Some(mono_tuple_shape(ws, parts));
        }
    }
    let (elem_ty, elements, nullable) = match static_ty(ws, ty)? {
        TypeExpr::Tuple { elements } => (ty, elements, false),
        TypeExpr::Nullable(inner) => {
            let inner_ty = ty.with_idx(inner);
            let TypeExpr::Tuple { elements } = static_ty(ws, inner_ty)? else {
                return None;
            };
            (inner_ty, elements, true)
        }
        _ => return None,
    };
    let slots = elements
        .into_iter()
        .map(|el| ret_slot(ws, subst, elem_ty.with_idx(el)))
        .collect();
    let shape = TupleRetShape { slots, nullable };
    if nullable && !shape.splats() {
        return None;
    }
    Some(shape)
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
/// other type, but [`mono_type_node`] has no arena to mint its node in, so
/// lowering saw an unsubstituted generic and left the element opaque for the
/// rebuild below.
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
    /// nullable value now means a TUPLE element (`zip`'s pair): a generic bound
    /// to a tuple has no [`mono_type_node`], so lowering read it as an
    /// unsubstituted generic.
    Unsupported,
}

/// How to rebuild one splatting element from the wire values, public because the
/// splice that consumes it lives in `skull-transform`. It mirrors [`RetSlot`],
/// which stays private: this is the part the other crate needs, not the shape
/// machinery behind it.
pub enum ElemRebuild {
    /// One wire value, used as-is (an unmarked element inside a splatting tuple).
    Passthrough,
    /// A `nobox` value: its struct name and leaf fields in ctor order.
    Value(Ustr, Vec<Ustr>),
    /// A tuple element (`zip`'s pair, `enumerate`'s `(int, T)`): rebuild each part,
    /// then pack them.
    Tuple(Vec<ElemRebuild>),
}

impl ElemRebuild {
    /// Wire values this plan consumes.
    pub fn leaf_count(&self) -> usize {
        match self {
            ElemRebuild::Passthrough => 1,
            ElemRebuild::Value(_, fields) => fields.len(),
            ElemRebuild::Tuple(parts) => parts.iter().map(Self::leaf_count).sum(),
        }
    }
}

pub(crate) fn elem_rebuild(slot: &RetSlot) -> Option<ElemRebuild> {
    match slot {
        RetSlot::Opaque => Some(ElemRebuild::Passthrough),
        RetSlot::Leaves(vs) if !vs.nullable => {
            let fields: Option<Vec<Ustr>> = vs
                .fields
                .iter()
                .map(|(n, fs)| matches!(fs, FieldShape::Prim).then_some(*n))
                .collect();
            fields.map(|f| ElemRebuild::Value(vs.name, f))
        }
        RetSlot::Nested(inner) if !inner.nullable => {
            inner.slots.iter().map(elem_rebuild).collect::<Option<Vec<_>>>().map(ElemRebuild::Tuple)
        }
        RetSlot::Leaves(_) | RetSlot::Nested(_) => None,
    }
}

pub struct BoundNextShape {
    pub slot_count: usize,
    pub elem: BoundNextElem,
}

pub fn bound_next_shape(
    ws: &Workspace,
    fref: FuncRef,
    key: Option<&SpecKey>,
) -> Option<BoundNextShape> {
    let subst = key.map(|k| spec_subst_from_key(ws, fref, k)).unwrap_or_default();
    let ret = ws.module(fref.module).functions[fref.func].return_ty?;
    let shape = tuple_ret_shape(ws, &subst, ret)?;
    // The element is slot 1 alone, or slots 1.. as a tuple -- the same pairing
    // `lower_for_user_iter` makes when it binds the pattern, so the two paths
    // cannot disagree about what the element IS.
    let elem_slot = match shape.slots.get(1..)? {
        [] => return None,
        [one] => one.clone(),
        rest => RetSlot::Nested(TupleRetShape { slots: rest.to_vec(), nullable: false }),
    };
    let elem = if elem_slot.splats() {
        match elem_rebuild(&elem_slot) {
            Some(plan) => BoundNextElem::Rebuild(plan),
            None => BoundNextElem::Unsupported,
        }
    } else {
        BoundNextElem::Opaque
    };
    Some(BoundNextShape { slot_count: shape.slot_count(), elem })
}

/// The wire slot of a PARAM, under the wide (`@scalar_abi`) shape rule.
///
/// Same vocabulary as [`ret_slot`], and deliberately the same tuple rule: only a
/// `nobox` element splats, so params and returns agree about what a tuple of
/// values is on the wire. Without the `Nested` arm a param could only say
/// "one box or one value's leaves", and a tuple carrying a `nobox` element had
/// nowhere to go but the box the mark forbids -- a `ValueShape` is a NAMED
/// struct shape, which a tuple is not.
pub(crate) fn sig_param_slot_spec(ws: &Workspace, subst: &SpecSubst, ty: TyId) -> RetSlot {
    match sig_value_shape_spec(ws, subst, ty) {
        Some(shape) => RetSlot::Leaves(shape),
        None => tuple_param_slot(ws, subst, ty),
    }
}

/// [`sig_param_slot_spec`] under the `nobox`-only rule (the ABI a fn taken by
/// reference narrows to).
pub(crate) fn nobox_param_slot_spec(ws: &Workspace, subst: &SpecSubst, ty: TyId) -> RetSlot {
    match nobox_declared_shape_spec(ws, subst, ty) {
        Some(shape) => RetSlot::Leaves(shape),
        None => tuple_param_slot(ws, subst, ty),
    }
}

fn tuple_param_slot(ws: &Workspace, subst: &SpecSubst, ty: TyId) -> RetSlot {
    // A tuple the signature SPELLS, not a generic that happens to be bound to
    // one. Both would agree between caller and callee -- they read the same
    // substitution -- so this is a choice, and it is made on where the box ends
    // up: a generic param's body is what decides whether the tuple can stay
    // leaves, and a body that stores it (`Vec<T>.push` -> `vec_push`) boxes it
    // anyway. Splatting into one only moves the materialization deeper, out of
    // the caller's line and into the spliced stdlib body, which costs the
    // diagnostic its span and buys nothing.
    //
    // A `(A, B)?` is spelled just as much as a `(A, B)`, and its presence rides
    // the param list the way it already rides a return -- so it is admitted
    // here and `tuple_ret_shape` sets the tag, as the sole declared-type entry
    // point. A generic bound to a nullable tuple is still excluded, for the
    // reason above.
    let spelled_tuple = match static_ty(ws, ty) {
        Some(TypeExpr::Tuple { .. }) => true,
        Some(TypeExpr::Nullable(inner)) => {
            matches!(static_ty(ws, ty.with_idx(inner)), Some(TypeExpr::Tuple { .. }))
        }
        _ => false,
    };
    if !spelled_tuple {
        return RetSlot::Opaque;
    }
    match tuple_ret_shape(ws, subst, ty) {
        Some(inner) if inner.splats() => RetSlot::Nested(inner),
        _ => RetSlot::Opaque,
    }
}

pub(crate) fn ret_slot(ws: &Workspace, subst: &SpecSubst, ty: TyId) -> RetSlot {
    if let Some(shape) = nobox_sig_shape_spec(ws, subst, ty) {
        return RetSlot::Leaves(shape);
    }
    match tuple_ret_shape(ws, subst, ty) {
        Some(inner) if inner.splats() => RetSlot::Nested(inner),
        _ => RetSlot::Opaque,
    }
}

/// [`sig_value_shape_spec`] narrowed to `nobox` nominals -- the splat gate. A
/// generic resolves through the substitution, so a `VecIter<NV2>.next` reaches
/// the same answer a spelled-out `-> (bool, NV2)` does.
pub(crate) fn nobox_sig_shape_spec(ws: &Workspace, subst: &SpecSubst, ty: TyId) -> Option<ValueShape> {
    let (nominal, nullable) = sig_spec_nominal(ws, subst, ty)?;
    if !is_nobox_nominal(ws, nominal) {
        return None;
    }
    if nullable {
        return sig_shape_of_nullable_nominal(ws, nominal);
    }
    sig_shape_of_nominal(ws, nominal)
}

/// `(nominal, nullable)` for a signature slot under a specialization: the type's
/// own `?` OR-ed with the `?` of whatever a generic is BOUND to.
///
/// `T?` at a site that bound `T = E?` is one nullable, not two -- Rock has no
/// double nullable, and the wire carries one presence leaf either way. Reading
/// the binding is still reading a DECLARATION (the site's own type argument),
/// never a nullability inferred from an expression.
pub(crate) fn sig_spec_nominal(ws: &Workspace, subst: &SpecSubst, ty: TyId) -> Option<(Ustr, bool)> {
    match static_ty(ws, ty)? {
        TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } => Some((n, false)),
        TypeExpr::Generic(var, _) => {
            let (inner, nullable) = subst.get(&var)?.peel_nullable();
            Some((inner.nominal()?, nullable))
        }
        TypeExpr::Nullable(inner) => {
            let (n, _) = sig_spec_nominal(ws, subst, ty.with_idx(inner))?;
            Some((n, true))
        }
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

/// The C type for a `value`-struct field, or `None` when the field is NOT
/// C-layout-mappable (`[T]`, `Arena<T>`, `Map`, `string`, a generic `T`, or a
/// GC-carrying nested value struct). This is the single mappability oracle:
/// skull's cdef emission uses the string, and [`value_struct_is_gc`] classifies
/// a struct GC-carrying when any field maps to `None`. Mirrors rock's
/// `rock_type_to_c_type`. A nested payload-free enum stores as its int tag; a
/// cdata-eligible payload enum nests its own `{__tag, _0}` ctype.
pub fn value_field_c_type(ws: &Workspace, ty: TyId) -> Option<String> {
    value_field_c_type_guarded(ws, ty, &mut Vec::new())
}

pub(crate) fn value_field_c_type_guarded(ws: &Workspace, ty: TyId, stack: &mut Vec<Ustr>) -> Option<String> {
    match static_ty(ws, ty)? {
        TypeExpr::Builtin(BuiltinTy::Int) => Some("int".into()),
        TypeExpr::Builtin(BuiltinTy::Float) => Some("float".into()),
        TypeExpr::Builtin(BuiltinTy::Bool) => Some("bool".into()),
        // A nested value struct is C-mappable iff it is itself FFI-clean; a
        // GC-carrying nested value struct makes this field (and its parent)
        // GC-carrying too.
        TypeExpr::App { ctor: TypeCtor::Nominal(name), .. } if map_is_value_struct(ws, name) => {
            if value_struct_is_gc_guarded(ws, name, stack) {
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
            if enum_value_shape(ws, name).is_some() =>
        {
            Some(name.as_str().to_string())
        }
        _ => None,
    }
}

/// Whether a `value` struct is GC-CARRYING (table-repr tier): it is generic, or
/// any field is not C-layout-mappable (transitively). FFI-clean is the negation:
/// non-generic AND every field C-mappable. `false` for a non-value / unknown
/// name. The tier is static per type and decided once here -- every pack site
/// (`Name_ct` vs `Name_vt`) and the prelude emission consult this one oracle.
pub fn value_struct_is_gc(ws: &Workspace, name: Ustr) -> bool {
    value_struct_is_gc_guarded(ws, name, &mut Vec::new())
}

/// A GC-tier value struct that nonetheless has a FULL leaf shape, so it stores
/// flat: one table key per [`value_leaf_paths`] entry (dotted for a nested
/// field, the field's own name for a nullable child's presence tag) instead of
/// one key per declared field.
///
/// The only shape that lands here is a nullable `nobox` field: presence is not a
/// C slot, so the outer cannot be cdata, while every leaf below it is still a
/// primitive. Every other GC cause -- a string/array/generic field -- has no
/// leaf shape at all and keeps the declared-field `<Name>_vt` table. Generics
/// are excluded outright: a `value Wrap<T = int> { a: float }` has a leaf shape
/// and is GC-tier for a reason that has nothing to do with leaves.
///
/// This is the ONE oracle: the `<Name>_lct` table ctor, the leaf-form pack site,
/// and the flat read all consult it, so the write and read encodings cannot
/// diverge.
pub fn value_struct_is_leaf_table(ws: &Workspace, name: Ustr) -> bool {
    if !value_struct_is_gc(ws, name) {
        return false;
    }
    let Some(sid) = ws.struct_by_name.get(&name) else { return false };
    if !ws.modules[sid.module.index()].structs[sid.def].generics.is_empty() {
        return false;
    }
    value_shape_rec(ws, name, &mut Vec::new()).is_some()
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
/// [`value_struct_is_gc`].
pub fn gc_eq_is_structural(ws: &Workspace, ty: TyId) -> bool {
    if value_field_c_type(ws, ty).is_some() {
        return true;
    }
    matches!(
        static_ty(ws, ty),
        Some(TypeExpr::App { ctor: TypeCtor::Nominal(name), .. }) if map_is_value_struct(ws, name)
    )
}

pub(crate) fn value_struct_is_gc_guarded(ws: &Workspace, name: Ustr, stack: &mut Vec<Ustr>) -> bool {
    let Some(sid) = ws.struct_by_name.get(&name) else {
        return false;
    };
    let s = &ws.modules[sid.module.index()].structs[sid.def];
    if !s.is_value {
        return false;
    }
    if !s.generics.is_empty() {
        return true;
    }
    // A definition cycle among value-struct fields is not C-mappable (an
    // infinite C layout); value_struct_defs rejects genuine cycles separately.
    if stack.contains(&name) {
        return true;
    }
    stack.push(name);
    let field_tys: Vec<TyId> = s.fields.iter().map(|f| f.ty).collect();
    let gc = field_tys
        .iter()
        .any(|ty| value_field_c_type_guarded(ws, *ty, stack).is_none());
    stack.pop();
    gc
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
    pub(crate) name: Ustr,
    /// One entry per payload POSITION, widest variant first-wins. A position is
    /// several SLOTS when it carries a `nobox` value, so this is not the slot
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
/// one -- except a `nobox` value, which is several leaves and has no box to sit
/// in. Widening the position to its leaves is the whole point: the value's
/// leaves ARE the slots, so an enum payload never materializes one.
#[derive(Clone, Debug)]
pub(crate) enum FlatSlot {
    /// One dynamic slot -- today's shape, and what every non-`nobox` payload keeps.
    Opaque,
    /// The `nobox` value payloads SHARING this position, in variant-declaration
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
    /// Total positional SLOTS -- `_0 .. _{tuple_width-1}`. Distinct from the
    /// position count (`slots.len()`): a `nobox` position spans several.
    pub(crate) fn tuple_width(&self) -> usize {
        self.slots.iter().map(FlatSlot::width).sum()
    }

    /// The first slot index of payload position `pos`. Positions past the end
    /// answer with the total width, so a variant narrower than the union simply
    /// has nothing to write.
    pub(crate) fn slot_offset(&self, pos: usize) -> usize {
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
pub(crate) fn flat_enum_shape(ws: &Workspace, name: Ustr) -> Option<EnumFlatShape> {
    let eid = *ws.enums_by_name.get(&name)?;
    // Payload-free enums are ints; cdata-eligible enums scalarize via cdata.
    if payload_free_enum_variants(ws, name).is_some() || enum_value_shape(ws, name).is_some() {
        return None;
    }
    let ed = &ws.modules[eid.module.index()].enums[eid.def];
    // Per position: every DISTINCT `nobox` payload shape any variant carries
    // there. Empty = no `nobox` payload seen, so one opaque slot. Several shapes
    // share the position by width, each variant addressing its own leaves under
    // the tag -- the alternative, letting the losing variant fall back to the
    // opaque slot, is the implicit materialization the mark forbids.
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
                    let Some(vs) = nobox_payload_shape(ws, *ty) else { continue };
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
    Some(EnumFlatShape { name, slots, record_fields })
}

/// The leaf shape of a payload type that is a `nobox` value struct, else `None`.
///
/// Keyed on the DECLARED payload type, like every other splat decision on this
/// arc: both the ctor that writes the slots and the match arm that reads them
/// derive the layout from the enum declaration, so neither has to see the other.
fn nobox_payload_shape(ws: &Workspace, ty: TyId) -> Option<ValueShape> {
    let TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } = static_ty(ws, ty)? else {
        return None;
    };
    if !is_nobox_nominal(ws, n) {
        return None;
    }
    sig_shape_of_nominal(ws, n)
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
        tuple_width: shape.tuple_width(),
        record_fields: shape.record_fields,
    })
}

/// The reused frame slot name for the `i`th yield leaf: `__y{i}` (tag at 0, then
/// positional, then record leaves -- `value_shape` leaf order).
pub fn yield_slot(i: usize) -> Ustr {
    Ustr::from(format!("__y{i}").as_str())
}

#[cfg(test)]
mod adapter_tests {
    use super::*;

    /// A primitive binding's node is that primitive. `string` in particular is
    /// NOT the recovery builtin: recovery poison means "inference failed", and
    /// answering it for a valid `string` hands every consumer a type that names
    /// no value. The other three are here as the controls that say this test is
    /// about the mapping rather than about one arm.
    #[test]
    fn a_primitive_binding_maps_to_its_own_builtin() {
        for (bound, want) in [
            (MonoTy::Int, BuiltinTy::Int),
            (MonoTy::Float, BuiltinTy::Float),
            (MonoTy::Bool, BuiltinTy::Bool),
            (MonoTy::String, BuiltinTy::String),
        ] {
            assert_eq!(
                mono_type_node(&bound),
                Some(TypeExpr::Builtin(want)),
                "{bound:?} must substitute to its own builtin"
            );
        }
        assert!(
            !matches!(
                mono_type_node(&MonoTy::String),
                Some(TypeExpr::Builtin(BuiltinTy::Error))
            ),
            "a valid `string` binding must never be spelled as recovery poison"
        );
    }
}
