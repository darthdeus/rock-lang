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

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ReifiedCtor {
    Array,
    Map,
    Nominal(Ustr),
}

/// An arena-free concrete type binding for a generic param at a specialized
/// call site. It keys cross-body snapshots without retaining caller-arena
/// `TyId`s, and preserves nested arguments such as `VecIter<Vec2>`.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum ReifiedType {
    Prim(&'static str),
    App {
        ctor: ReifiedCtor,
        params: Vec<ReifiedType>,
    },
    /// A generic bound to `E?`. Distinct from `App` because nullability decides
    /// the STORAGE of the binding (a nullable column element leads with a
    /// presence column, a nullable `nobox` signature slot with a presence leaf),
    /// and a binding that dropped the `?` would specialize a body to the wrong
    /// arity rather than merely to a slower one.
    Nullable(Box<ReifiedType>),
    /// A generic bound to a TUPLE (`zip`'s `(A, B)`, `enumerate`'s `(int, T)`).
    /// Its own variant rather than an `App`, because a tuple has no nominal and
    /// its slots decide an ARITY: a body specialized at `(Vec2?, int)` returns
    /// four values where one at `(Vec2, int)` returns three.
    ///
    /// This is the one binding [`reified_type_node`] cannot answer -- a
    /// `TypeExpr::Tuple` holds an arena range and there is no arena here to mint
    /// one. Consumers that need the slots read the binding DIRECTLY
    /// ([`reified_ret_slot`]); `ty_node` keeps returning the generic, exactly as
    /// it did when the binding did not reify at all.
    Tuple(Vec<ReifiedType>),
}

impl ReifiedType {
    /// The bound NOMINAL, and `None` for a nullable binding on purpose: every
    /// caller of this asks "is this generic exactly this struct", and a
    /// presence-carrying binding is not. The presence-aware callers peel with
    /// [`Self::peel_nullable`] and handle the flag explicitly.
    pub(crate) fn nominal(&self) -> Option<Ustr> {
        match self {
            Self::App { ctor: ReifiedCtor::Nominal(name), .. } => Some(*name),
            _ => None,
        }
    }

    /// `(payload, nullable)` -- the binding with its `?` stripped.
    pub(crate) fn peel_nullable(&self) -> (&ReifiedType, bool) {
        match self {
            Self::Nullable(inner) => (inner.as_ref(), true),
            other => (other, false),
        }
    }

    pub(crate) fn any_nominal(&self, pred: &impl Fn(Ustr) -> bool) -> bool {
        match self {
            Self::Prim(_) => false,
            Self::Nullable(inner) => inner.any_nominal(pred),
            Self::Tuple(parts) => parts.iter().any(|part| part.any_nominal(pred)),
            Self::App { ctor, params } => {
                matches!(ctor, ReifiedCtor::Nominal(name) if pred(*name))
                    || params.iter().any(|param| param.any_nominal(pred))
            }
        }
    }
}

/// Whether any generic in `key` binds a type whose STORAGE the shared generic
/// body cannot serve -- i.e. whether specializing is a CORRECTNESS requirement
/// rather than an optimization.
///
/// In a context the inliner never enters, an ordinary call simply forgoes the
/// scalar ABI (`in_unspliced_ctx`) because the shared generic body still serves a
/// boxed value correctly. The `for`-protocol `next` can only do the same for an
/// element the boxed representation still stores: under column storage the shared
/// body reads a `_data` that does not exist. So an unspliceable `next` keeps its
/// spec binding exactly when the element is column-stored, and the post-emit
/// validator turns that into a rejection instead of a miscompile.
///
/// Two storage forms qualify, and the key cannot tell which position the type is
/// used in -- so it over-approximates, which costs an extra specialized copy and
/// can never cost correctness:
///
/// - a column-stored `Vec` element, where the shared body reads a missing `_data`
/// - on JS ONLY, a leaf-addressed `NativeBuffer` element. luajit's `.ptr` is a
///   `T*`, so `p[i]` yields an element reference and the shared body is right;
///   JS backs the buffer with a TypedArray, so `ptr[i]` is a NUMBER and a whole
///   element read from an opaque `T` silently returns a leaf. That is a
///   miscompile, not a slowdown, so it does not wait for an annotation.
pub(crate) fn spec_key_has_target_storage(
    ws: &Workspace,
    key: &SpecKey,
    target: bone_semantic::hir::FnBackend,
) -> bool {
    let js = target == bone_semantic::hir::FnBackend::Js;
    key.iter().flatten().any(|ty| {
        ty.any_nominal(&|name| {
            // Either flavour counts: the question is whether the shared boxed
            // body would read a `_data` the storage does not have, and a
            // nullable element is column-stored on exactly the same terms.
            ((vec_column_elem(ws, name, false).is_some()
                || vec_column_elem(ws, name, true).is_some())
                && columns_available(ws, target, name))
                || (js && leaf_addr_plan(ws, name).is_ok())
        })
    })
}

/// Whether `[elem]` takes column storage when lowering for `target`.
///
/// Every target now serves columns, so this is unconditional. It survives as a
/// named predicate because [`spec_key_has_target_storage`] asks the question for a
/// DIFFERENT reason -- whether specializing a generic callee is a correctness
/// requirement -- and that reasoning is only sound while the two agree.
pub(crate) fn columns_available(
    _ws: &Workspace,
    _target: bone_semantic::hir::FnBackend,
    _elem: Ustr,
) -> bool {
    true
}

/// A callee's per-generic instantiation at one call site, in declaration order.
/// `None` = that generic did not reify (no specialization for it).
/// The snapshot cache key alongside the `FuncRef`.
pub type SpecKey = Vec<Option<ReifiedType>>;

/// Callee generic `VarId` -> its concrete type tree, for specialized lowering.
/// Only the reified generics appear; a `None` in the [`SpecKey`] is absent here.
pub type SpecSubst = std::collections::HashMap<bone_semantic::hir::VarId, ReifiedType>;

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
/// the binding directly through [`reified_ret_slot`].
pub(crate) fn reified_type_node(ty: &ReifiedType) -> Option<TypeExpr> {
    Some(match ty {
        ReifiedType::Nullable(inner) => return reified_type_node(inner),
        ReifiedType::Tuple(_) => return None,
        ReifiedType::App { ctor, .. } => TypeExpr::App {
            ctor: match ctor {
                ReifiedCtor::Array => TypeCtor::Array,
                ReifiedCtor::Map => TypeCtor::Map,
                ReifiedCtor::Nominal(name) => TypeCtor::Nominal(*name),
            },
            params: IdxRange::new_empty(),
        },
        ReifiedType::Prim("int") => TypeExpr::Builtin(BuiltinTy::Int),
        ReifiedType::Prim("float") => TypeExpr::Builtin(BuiltinTy::Float),
        ReifiedType::Prim("bool") => TypeExpr::Builtin(BuiltinTy::Bool),
        ReifiedType::Prim(_) => TypeExpr::Builtin(BuiltinTy::Error),
    })
}

/// The [`RetSlot`] of a binding, read straight off the reified type.
///
/// The counterpart to [`ret_slot`] for the one case that has no type node to go
/// through: a generic bound to a TUPLE. Mirrors [`nobox_sig_shape_spec`]'s
/// policy exactly -- only a `nobox` nominal splats, and a nullable one takes the
/// nullable shape -- so a tuple slot answers the same as the spelled-out type in
/// that position would.
pub(crate) fn reified_ret_slot(ws: &Workspace, ty: &ReifiedType) -> RetSlot {
    if let ReifiedType::Tuple(parts) = ty {
        // The same collapse [`ret_slot`] makes: a tuple no part of which splats
        // is the pre-existing boxed ABI, so it occupies ONE slot. Without it the
        // two oracles disagree on a NESTED tuple -- the generic side counted an
        // inner `(Entity<T>, T)` as two wire values while the concrete callee
        // returned it as one box.
        let shape = reified_tuple_shape(ws, parts);
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
/// [`reified_ret_slot`] applies. Read where the tuple's arity is the question --
/// a destructure, a `next`'s `(bool, E)` -- rather than what slot it occupies.
pub(crate) fn reified_tuple_shape(ws: &Workspace, parts: &[ReifiedType]) -> TupleRetShape {
    let slots = parts.iter().map(|part| reified_ret_slot(ws, part)).collect();
    TupleRetShape { slots, nullable: false }
}

pub(crate) fn reify_type(
    ws: &Workspace,
    inf: &BodyInference,
    ty: TyId,
    enclosing: &SpecSubst,
) -> Option<ReifiedType> {
    let ty = resolve_tyid(ws, inf, ty)?;
    match ty_node(ws, inf, ty)? {
        TypeExpr::Builtin(BuiltinTy::Int) => Some(ReifiedType::Prim("int")),
        TypeExpr::Builtin(BuiltinTy::Float) => Some(ReifiedType::Prim("float")),
        TypeExpr::Builtin(BuiltinTy::Bool) => Some(ReifiedType::Prim("bool")),
        // A string leaf is opaque to every shape decision, but it still has to
        // REIFY: a tuple reifies only when all its parts do, so leaving this out
        // made `(T, string)` unreifiable and the substitution then had no entry
        // for the generic bound to it -- which is the one thing `ty_node` cannot
        // answer. It cannot widen specialization on its own (`any_value` gates
        // that on a value-struct leaf, which a prim is not).
        TypeExpr::Builtin(BuiltinTy::String) => Some(ReifiedType::Prim("string")),
        TypeExpr::Generic(var, _) => enclosing.get(&var).cloned(),
        TypeExpr::Tuple { elements } => {
            let parts = elements
                .into_iter()
                .map(|el| reify_type(ws, inf, ty.with_idx(el), enclosing))
                .collect::<Option<Vec<_>>>()?;
            Some(ReifiedType::Tuple(parts))
        }
        TypeExpr::Nullable(inner) => Some(ReifiedType::Nullable(Box::new(reify_type(
            ws,
            inf,
            ty.with_idx(inner),
            enclosing,
        )?))),
        TypeExpr::App { ctor, params } => {
            let ctor = match ctor {
                TypeCtor::Array => ReifiedCtor::Array,
                TypeCtor::Map => ReifiedCtor::Map,
                TypeCtor::Nominal(name) => ReifiedCtor::Nominal(name),
            };
            let params = params
                .into_iter()
                .map(|param| reify_type(ws, inf, ty.with_idx(param), enclosing))
                .collect::<Option<Vec<_>>>()?;
            Some(ReifiedType::App { ctor, params })
        }
        _ => None,
    }
}

/// The specialization decision for a call `e` (in a body with inference `inf`) to
/// `func`: `Some((key, subst))` iff `func` is a splice-able generic `@inline`/
/// `@pure` callee whose per-site bindings (`call_generic_bindings[e]`) include at
/// least one VALUE-struct leaf. The single source consulted by both the caller
/// lowering ([`Builder::call_spec`]) and the site-aware rejection pass, so the two
/// never disagree on which calls specialize.
pub(crate) fn compute_call_spec(
    ws: &Workspace,
    inf: &BodyInference,
    e: Idx<Expr>,
    func: FuncRef,
    enclosing: &SpecSubst,
    target: bone_semantic::hir::FnBackend,
) -> Option<(SpecKey, SpecSubst)> {
    let f = &ws.module(func.module).functions[func.func];
    if f.generics.is_empty() || f.is_extern || f.inline_body.is_some() || f.is_coroutine {
        return None;
    }
    let bindings = inf.call_generic_bindings.get(&e)?;
    let (key, subst) = compute_spec_from_bindings(ws, inf, func, bindings, enclosing)?;
    // `@inline`/`@pure` is the OPT-IN gate: those callees specialize for speed, and
    // whether it happens is the author's call.
    //
    // A column-eligible element is not an optimization. The shared body reads a
    // `_data` that this instantiation's storage does not have, so it is WRONG, not
    // slow -- the same asymmetry that decides whether an unspliceable call may
    // fall back to the shared body. Specializing is the only correct lowering, so
    // it does not wait for an annotation. This is what makes the plain-generic
    // stdlib Vec surface (`clear`, `pop`, `remove_at`, ...) work over columns.
    let opted_in =
        f.annotations.iter().any(|a| matches!(a.name.name.as_str(), "inline" | "pure"));
    if !opted_in && !spec_key_has_target_storage(ws, &key, target) {
        return None;
    }
    Some((key, subst))
}

/// [`compute_call_spec`] against an explicit binding list instead of a call
/// expr. The for-protocol `next` call is SYNTHESIZED and has no expr to key
/// `call_generic_bindings` by, so it supplies its bindings from
/// `BoundNext::Concrete::next_generics` (which bone records at resolution).
/// Shared body so an iterator call and a direct call can never disagree on
/// which instantiations specialize.
pub(crate) fn compute_spec_from_bindings(
    ws: &Workspace,
    inf: &BodyInference,
    func: FuncRef,
    bindings: &[TyId],
    enclosing: &SpecSubst,
) -> Option<(SpecKey, SpecSubst)> {
    let f = &ws.module(func.module).functions[func.func];
    if bindings.len() != f.generics.len() {
        return None;
    }
    let mut key: SpecKey = Vec::with_capacity(bindings.len());
    let mut subst = SpecSubst::new();
    let mut any_value = false;
    for (g, b) in f.generics.iter().zip(bindings) {
        let Some(TypeExpr::Generic(var, _)) = ty_node(ws, inf, *g) else {
            key.push(None);
            continue;
        };
        match reify_type(ws, inf, *b, enclosing) {
            Some(ty) => {
                any_value |= ty.any_nominal(&|name| map_is_value_struct(ws, name));
                subst.insert(var, ty.clone());
                key.push(Some(ty));
            }
            None => key.push(None),
        }
    }
    any_value.then_some((key, subst))
}

pub fn spec_key_from_next_generics(
    ws: &Workspace,
    inf: &BodyInference,
    func: FuncRef,
    next_generics: &rustc_hash::FxHashMap<bone_semantic::hir::VarId, TyId>,
    enclosing: &SpecSubst,
) -> Option<SpecKey> {
    let f = &ws.module(func.module).functions[func.func];
    let bindings: Vec<TyId> = f
        .generics
        .iter()
        .map(|generic| match ty_node(ws, inf, *generic) {
            Some(TypeExpr::Generic(var, _)) => next_generics.get(&var).copied(),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    compute_spec_from_bindings(ws, inf, func, &bindings, enclosing).map(|(key, _)| key)
}

/// Rebuild the [`SpecSubst`] (callee generic `VarId` -> concrete type) for `fref`
/// from a [`SpecKey`] (trees in declaration order). The inliner holds only the
/// arena-free key in its cache; this re-pairs it with the callee's generic
/// `VarId`s to drive [`lower_function_spec`]. An empty / all-`None` key yields an
/// empty subst (the shared snapshot).
pub fn spec_subst_from_key(ws: &Workspace, fref: FuncRef, key: &SpecKey) -> SpecSubst {
    let f = &ws.module(fref.module).functions[fref.func];
    let mut subst = SpecSubst::new();
    for (g, ty) in f.generics.iter().zip(key) {
        if let (Some(TypeExpr::Generic(var, _)), Some(reified)) = (static_ty(ws, *g), ty) {
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
    // `reified_type_node`), so its slots come off the binding itself. Only here:
    // this is the sole consumer that needs a tuple's arity, and answering it
    // from the binding keeps `ty_node` honest about what it cannot mint.
    if let Some(TypeExpr::Generic(var, _)) = static_ty(ws, ty) {
        if let Some(ReifiedType::Tuple(parts)) = subst.get(&var) {
            return Some(reified_tuple_shape(ws, parts));
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
/// left is the case specialization cannot express -- a generic bound to a tuple,
/// which does not reify -- so the element stays opaque and has to be rebuilt.
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
    /// to a tuple does not reify, so the body never specialized.
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
    if !matches!(static_ty(ws, ty), Some(TypeExpr::Tuple { .. })) {
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
    /// Positional payload slots `f__0 .. f__{tuple_width-1}` (max tuple arity).
    pub(crate) tuple_width: usize,
    /// Named payload slots `f__<name>` (union of record-variant field names,
    /// first-seen order). A tuple position and a record field never collide --
    /// the former is a number, the latter an identifier.
    pub(crate) record_fields: Vec<Ustr>,
}

impl EnumFlatShape {
    /// The number of scalar leaves this enum threads as across a call: the
    /// discriminant + every positional slot + every record slot.
    pub(crate) fn leaf_count(&self) -> usize {
        1 + self.tuple_width + self.record_fields.len()
    }

    /// The `ValueShape` the scalar ABI / SROA see: `[__tag, _0.., <rec>..]`, all
    /// primitive leaves. `pack`/`cdata_leaves` re-detect the enum by name and use
    /// box semantics (the leaf NAMES here are nominal -- never read structurally).
    pub(crate) fn value_shape(&self) -> ValueShape {
        let mut fields = vec![(Ustr::from("__tag"), FieldShape::Prim)];
        for i in 0..self.tuple_width {
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
