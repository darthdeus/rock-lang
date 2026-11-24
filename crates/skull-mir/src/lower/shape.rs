use super::*;

use bone_core::hir::{BuiltinTy, EnumDef, FnBackend, TyId, TypeCtor, TypeExpr, VariantPayload};
use bone_core::workspace::Workspace;
use comfy_la_arena::Idx;
use ustr::Ustr;

use crate::monoty::{MonoCtor, MonoSubst, MonoTy};

/// Stdlib descriptors (`Ptr`, `NativeBuffer`) are excluded: the backend supplies
/// their one host slot, which a copy shares and never rebuilds.
pub fn has_value_plan(sd: &bone_core::hir::StructDef) -> bool {
    sd.is_value && !bone_core::modules::is_descriptor_key(sd.name)
}

pub(crate) fn is_value_struct(ws: &Workspace, name: Ustr) -> bool {
    ws.nominals
        .struct_id(name)
        .is_some_and(|sid| has_value_plan(&ws.modules[sid.module.index()].structs[sid.def]))
}

/// Explicit `nobox` bypasses the leaf budget because boxing is forbidden.
pub(crate) const MAX_COLUMN_LEAVES: usize = 4;

pub(crate) fn is_json_value(ws: &Workspace, name: Ustr) -> bool {
    name.as_str() == "JsonValue"
        && ws.nominals.enum_id(name).is_some_and(|id| {
            bone_core::modules::is_stdlib_module_file(ws.module(id.module).file.as_str())
        })
}

pub(crate) fn dense_flat_enum_vec_eligible(ws: &Workspace, name: Ustr) -> bool {
    fn dense_payload_ty(ws: &Workspace, ty: TyId, visiting: &mut Vec<Ustr>) -> bool {
        match static_ty(ws, ty) {
            Some(TypeExpr::Builtin(BuiltinTy::Int | BuiltinTy::Uint | BuiltinTy::Float)) => true,
            Some(TypeExpr::App {
                ctor: TypeCtor::Nominal(child),
                ..
            }) if is_payload_free_enum(ws, child) => true,
            Some(TypeExpr::App {
                ctor: TypeCtor::Nominal(child),
                ..
            }) => {
                let Some(sid) = ws.nominals.struct_id(child) else {
                    return false;
                };
                let decl = &ws.modules[sid.module.index()].structs[sid.def];
                if !has_value_plan(decl) || visiting.contains(&child) {
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
    let Some(eid) = ws.nominals.enum_id(name) else {
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

/// Keep a data leaf named `len` distinct from the backing Vec's length.
pub(crate) fn column_key(path: Ustr) -> Ustr {
    if path == "len" {
        crate::names::ustr("@len")
    } else {
        path
    }
}

/// Nullable presence first: a top-level presence leaf has no field path, so
/// this adds the key [`value_leaf_paths`] omits.
pub(crate) fn column_leaf_paths(shape: &ValueShape) -> Vec<Ustr> {
    let presence = shape
        .nullable
        .then(|| crate::names::ustr(VEC_COLUMN_PRESENCE_FIELD));
    presence
        .into_iter()
        .chain(value_leaf_paths(shape).into_iter().map(column_key))
        .collect()
}

/// Dotted column keys: `.` cannot appear in a Rock identifier, so keys never
/// collide with a sibling leaf.
pub fn value_leaf_paths(shape: &ValueShape) -> Vec<Ustr> {
    leaf_names(shape, "", ".")
        .into_iter()
        .map(|s| crate::names::ustr(&s))
        .collect()
}

/// Storage canonicalizes an absent presence tag to `nil`; a data leaf must
/// never be touched, or a `false` bool field would erase itself.
pub fn value_leaf_presence_flags(shape: &ValueShape) -> Vec<bool> {
    let mut out = Vec::new();
    for_each_leaf(shape, &mut String::new(), "", &mut |_, presence| {
        out.push(presence)
    });
    out
}

pub(crate) fn track_enum_payload_presence(ws: &Workspace, shape: &ValueShape) -> bool {
    shape.nullable || (is_nobox_nominal(ws, shape.name) && has_nested_presence(shape))
}

fn has_nested_presence(shape: &ValueShape) -> bool {
    shape.fields.iter().any(|(_, field)| {
        matches!(field, FieldShape::Nested(child) if child.nullable || has_nested_presence(child))
    })
}

pub(crate) fn leaf_names(shape: &ValueShape, prefix: &str, sep: &str) -> Vec<String> {
    let mut out = Vec::new();
    for_each_leaf(shape, &mut prefix.to_owned(), sep, &mut |path, _| {
        out.push(path.to_owned())
    });
    out
}

pub(crate) fn leaf_keys(shape: &ValueShape, prefix: &str) -> Vec<(Ustr, bool)> {
    let mut out = Vec::new();
    for_each_leaf(shape, &mut prefix.to_owned(), ".", &mut |path, presence| {
        out.push((crate::names::ustr(path), presence))
    });
    out
}

fn for_each_leaf(
    shape: &ValueShape,
    path: &mut String,
    sep: &str,
    visit: &mut impl FnMut(&str, bool),
) {
    // Nested nullable presence is keyed at the field's own path, before its children.
    if shape.nullable && !path.is_empty() {
        visit(path, true);
    }
    let prefix = path.len();
    for (fname, fs) in &shape.fields {
        // Join each field to its ancestors so paths and Lua parameter names stay
        // unique for repeated nested value types.
        if prefix != 0 {
            path.push_str(sep);
        }
        path.push_str(fname);
        match fs {
            FieldShape::Prim => visit(path, false),
            FieldShape::Nested(sub) => for_each_leaf(sub, path, sep, visit),
        }
        path.truncate(prefix);
    }
}

/// Every leaf view `(leaf C type, buffer field, cast type)`. The runtime
/// `__ROCK_LEAF_VIEWS` table is generated from this list.
pub const LEAF_VIEWS: &[(&str, &str, &str)] = &[
    ("double", "__vd", "double*"),
    ("float", "__vf", "float*"),
    ("int", "__vi", "int*"),
    ("uint32_t", "__vu", "uint32_t*"),
    ("bool", "__vb", "bool*"),
];

/// One field per distinct leaf type, made once at construction and read per
/// element access, never per leaf.
pub fn leaf_view_field(cty: &str) -> Ustr {
    match LEAF_VIEWS.iter().find(|(t, ..)| *t == cty) {
        Some((_, field, _)) => crate::names::ustr(field),
        None => unreachable!("no leaf view for non-primitive C type `{cty}`"),
    }
}

pub fn is_leaf_view_field(field: &str) -> bool {
    LEAF_VIEWS.iter().any(|(_, f, _)| *f == field)
}

pub fn leaf_views_lua_table() -> String {
    let entries: Vec<String> = LEAF_VIEWS
        .iter()
        .map(|(_, field, cast)| format!("{field} = \"{cast}\""))
        .collect();
    format!("{{ {} }}", entries.join(", "))
}

/// Leaf location in units of its own view's element size.
/// Both numbers are compile-time constants, so an access is
/// `view[i * stride + offset]`: pure arithmetic, no allocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeafAddr {
    pub path: Ustr,
    pub view_field: Ustr,
    pub stride: i64,
    pub offset: i64,
}

pub fn leaf_view_cty(field: &str) -> Option<&'static str> {
    LEAF_VIEWS
        .iter()
        .find(|(_, f, _)| *f == field)
        .map(|(t, ..)| *t)
}

/// Non-value-struct `NativeBuffer` element types as `(C type, byte width)`.
/// Separate from [`c_prim_size_align`]: `uint8_t` is an element, never a leaf.
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
    format!(
        "__rock_nb_storage(\"{ctype}\", {size}, {{ {} }})\n",
        views.join(", ")
    )
}

/// A primitive element carries no leaf views: `buf.ptr` already addresses it.
pub fn nb_prim_storage_lua() -> String {
    NB_PRIM_ELEMS
        .iter()
        .map(|(cty, size)| nb_storage_call(cty, *size, &[]))
        .collect()
}

pub fn nb_value_storage_lua(
    ws: &Workspace,
    elem: Ustr,
    leaves: &[LeafSlot],
    size: usize,
) -> String {
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

/// `byte_off` is in bytes from the start of the struct; `path` is the
/// [`value_leaf_paths`] key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeafSlot {
    pub path: Ustr,
    pub cty: &'static str,
    pub byte_off: usize,
    /// Emitted C type symbols of nested structs along `path`, outermost first;
    /// the start-up `ffi.offsetof` check composes them to verify `byte_off`.
    pub owners: Vec<Ustr>,
}

/// Size and alignment of a primitive C type as LuaJIT lays it out.
/// The only types [`value_c_fields`] admits as leaves.
pub(crate) fn c_prim_size_align(cty: &str) -> Option<(usize, usize)> {
    match cty {
        "double" => Some((8, 8)),
        "int" | "uint32_t" | "float" => Some((4, 4)),
        "bool" => Some((1, 1)),
        _ => None,
    }
}

/// Positional leaf order must match [`value_leaf_paths`], or values are
/// written to the wrong addresses.
pub(crate) fn value_leaf_layout(
    ws: &Workspace,
    name: Ustr,
) -> Option<(Vec<LeafSlot>, usize, usize)> {
    let plan = planned_value(ws, name)?;
    let layout = plan.cdata.as_ref()?;
    let mut leaves = Vec::new();
    layout.write_leaves(ws, "", 0, &[], &mut leaves);
    Some((leaves, layout.size(), layout.align))
}

struct CLayout {
    fields: Vec<(Ustr, usize, CMember)>,
    end: usize,
    align: usize,
}

enum CMember {
    Scalar(&'static str),
    Aggregate(Ustr, CLayout),
}

impl CLayout {
    fn new() -> Self {
        Self {
            fields: Vec::new(),
            end: 0,
            align: 1,
        }
    }

    fn size(&self) -> usize {
        round_up(self.end, self.align)
    }

    fn push(&mut self, name: Ustr, member: CMember) {
        let (size, align) = match &member {
            CMember::Scalar(ty) => c_prim_size_align(ty).expect("planned primitive type"),
            CMember::Aggregate(_, child) => (child.size(), child.align),
        };
        let offset = round_up(self.end, align);
        self.fields.push((name, offset, member));
        self.end = offset + size;
        self.align = self.align.max(align);
    }

    fn write_leaves(
        &self,
        ws: &Workspace,
        prefix: &str,
        base: usize,
        owners: &[Ustr],
        leaves: &mut Vec<LeafSlot>,
    ) {
        for (field, offset, member) in &self.fields {
            let byte_off = base + offset;
            match member {
                CMember::Scalar(cty) => leaves.push(LeafSlot {
                    path: leaf_path(prefix, field.as_str()),
                    cty,
                    byte_off,
                    owners: owners.to_vec(),
                }),
                CMember::Aggregate(name, child) => {
                    let mut owners = owners.to_vec();
                    owners.push(crate::mangle::type_symbol(ws, *name));
                    child.write_leaves(ws, &format!("{prefix}{field}."), byte_off, &owners, leaves);
                }
            }
        }
    }
}

pub(crate) fn leaf_path(prefix: &str, name: &str) -> Ustr {
    if prefix.is_empty() {
        crate::names::ustr(name)
    } else {
        crate::names::ustr(&format!("{prefix}{name}"))
    }
}

pub(crate) fn round_up(n: usize, align: usize) -> usize {
    n.div_ceil(align) * align
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ValueShape {
    pub(crate) name: Ustr,
    pub(crate) fields: Vec<(Ustr, FieldShape)>,
    pub(crate) nullable: bool,
}

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
    pub fn name(&self) -> Ustr {
        self.name
    }

    /// Total primitive-leaf count: the scalar-ABI arity.
    pub(crate) fn leaf_count(&self) -> usize {
        self.fields
            .iter()
            .map(|(_, fs)| fs.leaf_count())
            .sum::<usize>()
            + usize::from(self.nullable)
    }

    pub(super) fn has_registry_field(&self) -> bool {
        self.fields.iter().any(|(_, field)| match field {
            FieldShape::Prim => false,
            FieldShape::Nested(child) => {
                is_registry_handle(child.name) || child.has_registry_field()
            }
        })
    }
}

impl FieldShape {
    pub(crate) fn leaf_count(&self) -> usize {
        match self {
            FieldShape::Prim => 1,
            FieldShape::Nested(s) => s.leaf_count(),
        }
    }
}

pub(crate) struct PlannedValue {
    pub(crate) storage: ValueStorage,
    pub(super) shape: ValueShape,
    cdata: Option<CLayout>,
    pub(crate) scalarizable: bool,
    pub(crate) gc_contains_nobox: bool,
    contains_flat_enum: bool,
}

impl PlannedValue {
    pub(super) fn scalar_shape(&self) -> Option<&ValueShape> {
        self.scalarizable.then_some(&self.shape)
    }
}

pub(super) fn nominal_name_from_ty(ws: &Workspace, ty: TyId) -> Option<(Ustr, bool)> {
    let mut node = static_ty(ws, ty)?;
    let mut nullable = false;
    if let TypeExpr::Nullable(inner) = node {
        nullable = true;
        node = static_ty(ws, ty.with_idx(inner))?;
    }
    match node {
        TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            params,
        } if !params.is_empty() && is_generic_nominal(ws, name) => {
            // An enum layout that depends on an unbound generic has no name to
            // plan under, so it stays opaque; a struct keeps its declaration.
            let inner = if nullable {
                let TypeExpr::Nullable(inner) = static_ty(ws, ty)? else {
                    return None;
                };
                ty.with_idx(inner)
            } else {
                ty
            };
            match crate::monoty::convert_decl(ws, inner, &MonoSubst::default()) {
                Ok(mono) => Some((mono.nominal()?, nullable)),
                Err(_) if ws.nominals.struct_id(name).is_some() => Some((name, nullable)),
                Err(_) => None,
            }
        }
        TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            ..
        } => Some((name, nullable)),
        _ => None,
    }
}

/// The nominal a declared type is planned under; see [`nominal_name_from_ty`].
pub fn static_layout_nominal(ws: &Workspace, ty: TyId) -> Option<Ustr> {
    nominal_name_from_ty(ws, ty).map(|(name, _)| name)
}

/// A declaration's or an instance's enum definition.
pub fn enum_decl(ws: &Workspace, name: Ustr) -> Option<&EnumDef> {
    enum_definition(ws, name)
}

/// A payload or field type of the layout `owner` substituted for that
/// instance, when it names one of the instance's generics.
pub fn instance_payload_ty(ws: &Workspace, owner: Ustr, ty: TyId) -> Option<MonoTy> {
    substituted(ws, ty, &crate::layout_instance::layout_subst(ws, owner)?)
}

pub(crate) fn substituted(ws: &Workspace, ty: TyId, subst: &MonoSubst) -> Option<MonoTy> {
    match PayloadTy::of(ws, ty, Some(subst))? {
        PayloadTy::Mono(mono) => Some(mono),
        PayloadTy::Decl(_) => None,
    }
}

/// Every struct and enum instance a build reaches through its types,
/// declarations and plans, sorted by name so emission is deterministic.
pub fn layout_instances_in(ws: &Workspace, arena: &RepArena) -> Vec<Ustr> {
    fn mono(ty: &MonoTy, out: &mut Vec<Ustr>) {
        match ty {
            MonoTy::Nullable(inner) => mono(inner, out),
            MonoTy::Tuple(parts) => parts.iter().for_each(|part| mono(part, out)),
            MonoTy::App { ctor, params } => {
                if let MonoCtor::Nominal(name) = ctor {
                    out.push(*name);
                }
                params.iter().for_each(|param| mono(param, out));
            }
            MonoTy::Function { params, ret } => {
                params.iter().for_each(|param| mono(param, out));
                mono(ret, out);
            }
            _ => {}
        }
    }
    fn shape(vs: &ValueShape, out: &mut Vec<Ustr>) {
        out.push(vs.name);
        for (_, field) in &vs.fields {
            if let FieldShape::Nested(child) = field {
                shape(child, out);
            }
        }
    }
    let mut work = Vec::new();
    arena.interner().iter().for_each(|ty| mono(ty, &mut work));
    for module in ws.modules.iter() {
        work.extend(module.structs.iter().map(|(_, sd)| sd.name));
        work.extend(module.enums.iter().map(|(_, ed)| ed.name));
    }
    let mut seen = HashSet::default();
    let mut out = Vec::new();
    while let Some(name) = work.pop() {
        if !seen.insert(name) {
            continue;
        }
        if crate::layout_instance::instance(name).is_some() {
            out.push(name);
        }
        if let Some(id) = ws.nominals.struct_id(name) {
            let subst = crate::layout_instance::layout_subst(ws, name);
            for field in &ws.modules[id.module.index()].structs[id.def].fields {
                match subst
                    .as_ref()
                    .and_then(|subst| substituted(ws, field.ty, subst))
                {
                    Some(ty) => mono(&ty, &mut work),
                    None => work.extend(static_layout_nominal(ws, field.ty)),
                }
            }
            if let Some(plan) = planned_value(ws, name) {
                shape(&plan.shape, &mut work);
            }
            continue;
        }
        let Some(flat) = EnumPlan::for_name(ws, name).and_then(|plan| plan.flat().cloned()) else {
            continue;
        };
        for position in &flat.positions {
            position.shapes.iter().for_each(|vs| shape(vs, &mut work));
        }
        for named in flat.named_shapes.iter() {
            shape(&named.shape, &mut work);
        }
    }
    out.sort();
    out
}

fn is_generic_nominal(ws: &Workspace, name: Ustr) -> bool {
    enum_definition(ws, name).is_some_and(|ed| !ed.generics.is_empty())
        || ws.nominals.struct_id(name).is_some_and(|id| {
            !ws.modules[id.module.index()].structs[id.def]
                .generics
                .is_empty()
        })
}

fn is_registry_handle(name: Ustr) -> bool {
    matches!(
        crate::layout_instance::decl_name(name).as_str(),
        "Arena" | "Entity" | "Scene"
    )
}

fn planned_value_rec(ws: &Workspace, name: Ustr, visiting: &mut Vec<Ustr>) -> Option<PlannedValue> {
    if let Some(enum_plan) = EnumPlan::for_name(ws, name)
        && let Some(cdata) = enum_plan.cdata()
    {
        let mut layout = CLayout::new();
        layout.push(crate::names::ustr("__tag"), CMember::Scalar("int"));
        for &(name, ty) in &cdata.slots {
            layout.push(name, CMember::Scalar(ty));
        }
        return Some(PlannedValue {
            shape: cdata.value_shape(),
            storage: ValueStorage::CData,
            cdata: Some(layout),
            scalarizable: true,
            gc_contains_nobox: false,
            contains_flat_enum: false,
        });
    }
    let sid = ws.nominals.struct_id(name)?;
    let sd = &ws.modules[sid.module.index()].structs[sid.def];
    if !has_value_plan(sd) || visiting.contains(&name) {
        return None;
    }
    visiting.push(name);
    // Registry type arguments select a row type. Escaping ID lanes remain Lua numbers.
    let handle = is_registry_handle(name);
    let mut cdata = (sd.generics.is_empty() || handle).then(CLayout::new);
    let mut scalarizable = true;
    let mut contains_flat_enum = false;
    let mut gc_contains_nobox = false;
    let mut fields = Vec::with_capacity(sd.fields.len());

    let subst = crate::layout_instance::layout_subst(ws, name);
    for field in &sd.fields {
        let storage = PayloadTy::of(ws, field.storage_ty, subst.as_ref());
        let primitive = storage.as_ref().and_then(|ty| match ty.nominal(ws) {
            Some((name, false)) if is_payload_free_enum(ws, name) => Some("int"),
            _ if handle && ty.is_int(ws) => Some("double"),
            _ => ty.primitive_c_type(ws),
        });
        let (shape, ctype) = if let Some(primitive) = primitive {
            (FieldShape::Prim, Some(CMember::Scalar(primitive)))
        } else {
            let child = match storage.as_ref().and_then(|ty| ty.nominal(ws)) {
                Some((child, false)) => Some((child, false)),
                Some(_) => PayloadTy::of(ws, field.ty, subst.as_ref())
                    .and_then(|ty| ty.nominal(ws))
                    .filter(|&(child, nullable)| nullable && is_nobox_nominal(ws, child)),
                None => None,
            };
            let mut member = None;
            let shape = match child {
                Some((child, false)) if let Some(shape) = flat_enum_value_shape(ws, child) => {
                    scalarizable = false;
                    contains_flat_enum = true;
                    FieldShape::Nested(shape)
                }
                Some((child, nullable))
                    if let Some(mut plan) = planned_value_rec(ws, child, visiting) =>
                {
                    scalarizable &= plan.scalarizable;
                    contains_flat_enum |= plan.contains_flat_enum;
                    gc_contains_nobox |= plan.gc_contains_nobox || is_nobox_nominal(ws, child);
                    plan.shape.nullable = nullable;
                    if !nullable {
                        member = plan.cdata.map(|layout| CMember::Aggregate(child, layout));
                    }
                    FieldShape::Nested(plan.shape)
                }
                _ => {
                    scalarizable = false;
                    FieldShape::Prim
                }
            };
            (shape, member)
        };
        if let Some(member) = ctype {
            if let Some(layout) = &mut cdata {
                layout.push(field.name, member);
            }
        } else {
            cdata = None;
        }
        fields.push((field.name, shape));
    }
    visiting.pop();
    let storage = if cdata.is_some() {
        ValueStorage::CData
    } else if scalarizable && sd.generics.is_empty() {
        ValueStorage::LeafTable
    } else {
        ValueStorage::GcTable
    };
    Some(PlannedValue {
        storage,
        shape: ValueShape {
            name,
            fields,
            nullable: false,
        },
        cdata,
        scalarizable,
        gc_contains_nobox: storage == ValueStorage::GcTable && gc_contains_nobox,
        contains_flat_enum,
    })
}

pub(crate) fn planned_value(ws: &Workspace, name: Ustr) -> Option<Rc<PlannedValue>> {
    ShapeMemo::with_active(
        |memo| memo.planned.get(&name).cloned(),
        |memo, plan| {
            memo.planned.insert(name, plan.clone());
        },
        || planned_value_rec(ws, name, &mut Vec::new()).map(Rc::new),
    )
}

/// Per-thread memo of declaration plans, scoped to a representation arena
/// generation; outside an active [`ShapeMemoScope`] plans are computed directly.
#[derive(Default)]
struct ShapeMemo {
    arena_gen: u64,
    planned: rustc_hash::FxHashMap<Ustr, Option<Rc<PlannedValue>>>,
    enums: rustc_hash::FxHashMap<Ustr, Rc<EnumPlan>>,
    gc_inline: rustc_hash::FxHashMap<(FnBackend, Ustr), Option<Rc<[GcTableField]>>>,
    same_layout: rustc_hash::FxHashMap<Ustr, bool>,
}

thread_local! {
    static SHAPE_MEMO: std::cell::RefCell<ShapeMemo> = std::cell::RefCell::new(ShapeMemo::default());
    static SHAPE_MEMO_ACTIVE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

impl ShapeMemo {
    fn with_active<T: Clone>(
        get: impl FnOnce(&ShapeMemo) -> Option<T>,
        put: impl FnOnce(&mut ShapeMemo, &T),
        compute: impl FnOnce() -> T,
    ) -> T {
        let generation = SHAPE_MEMO_ACTIVE.with(|active| active.get());
        if generation == 0 {
            return compute();
        }
        let hit = SHAPE_MEMO.with(|memo| {
            let mut memo = memo.borrow_mut();
            if memo.arena_gen != generation {
                memo.arena_gen = generation;
                memo.planned.clear();
                memo.enums.clear();
                memo.gc_inline.clear();
                memo.same_layout.clear();
            }
            get(&memo)
        });
        if let Some(value) = hit {
            return value;
        }
        // Computed outside the borrow: a nested plan request re-enters here.
        let value = compute();
        SHAPE_MEMO.with(|memo| {
            let mut memo = memo.borrow_mut();
            if memo.arena_gen == generation {
                put(&mut memo, &value);
            }
        });
        value
    }
}

pub(crate) fn gc_table_inline_fields(
    ws: &Workspace,
    target: FnBackend,
    name: Ustr,
) -> Result<Option<Rc<[GcTableField]>>, RepError> {
    ShapeMemo::with_active(
        |memo| memo.gc_inline.get(&(target, name)).cloned().map(Ok),
        |memo, fields| {
            if let Ok(fields) = fields {
                memo.gc_inline.insert((target, name), fields.clone());
            }
        },
        || {
            let fields = crate::lower::gc_table_fields(ws, target, name)?;
            Ok(fields
                .iter()
                .any(GcTableField::is_inline)
                .then(|| fields.into()))
        },
    )
}

/// Whether the layout instance `name` lays out like its declaration.
pub(crate) fn instance_matches_declaration(ws: &Workspace, name: Ustr) -> bool {
    ShapeMemo::with_active(
        |memo| memo.same_layout.get(&name).copied(),
        |memo, same| {
            memo.same_layout.insert(name, *same);
        },
        || {
            if ws.nominals.enum_id(name).is_some() {
                EnumPlan::instance_matches_declaration(ws, name)
            } else {
                super::struct_instance_matches_declaration(ws, name)
            }
        },
    )
}

/// Nests, restoring the enclosing scope on drop.
pub(crate) struct ShapeMemoScope {
    previous: u64,
}

impl ShapeMemoScope {
    pub(crate) fn enter(arena_gen: u64) -> Self {
        let previous = SHAPE_MEMO_ACTIVE.with(|active| active.replace(arena_gen));
        Self { previous }
    }
}

impl Drop for ShapeMemoScope {
    fn drop(&mut self) {
        SHAPE_MEMO_ACTIVE.with(|active| active.set(self.previous));
    }
}

pub(crate) fn planned_field_is_nested(ws: &Workspace, name: Ustr, field: Ustr) -> bool {
    planned_value(ws, name).is_some_and(|plan| {
        plan.shape
            .fields
            .iter()
            .any(|(f, shape)| *f == field && matches!(shape, FieldShape::Nested(_)))
    })
}

pub(crate) fn planned_value_contains_flat_enum(ws: &Workspace, name: Ustr) -> bool {
    planned_value(ws, name).is_some_and(|plan| plan.contains_flat_enum)
}

/// Accepts an instance name and answers its declaration.
pub(super) fn enum_definition(ws: &Workspace, name: Ustr) -> Option<&EnumDef> {
    let id = ws.nominals.enum_id(name)?;
    Some(&ws.modules[id.module.index()].enums[id.def])
}

fn is_unit_enum(ed: &EnumDef) -> bool {
    ed.variants
        .iter()
        .all(|v| matches!(v.payload, VariantPayload::Unit))
}

pub(crate) fn is_payload_free_enum(ws: &Workspace, name: Ustr) -> bool {
    enum_definition(ws, name).is_some_and(is_unit_enum)
}

/// The index is the variant's int tag.
pub(crate) fn payload_free_enum_variants(ws: &Workspace, name: Ustr) -> Option<Vec<Ustr>> {
    let ed = enum_definition(ws, name)?;
    let mut out = Vec::with_capacity(ed.variants.len());
    for v in &ed.variants {
        if !matches!(v.payload, VariantPayload::Unit) {
            return None;
        }
        out.push(v.name);
    }
    Some(out)
}

/// A Lua value with no mutable heap storage is already its own copy.
pub(crate) fn lua_value_is_immutable(ws: &Workspace, ty: &MonoTy) -> bool {
    match ty {
        MonoTy::Int | MonoTy::Uint | MonoTy::Float | MonoTy::Bool | MonoTy::String => true,
        MonoTy::Nullable(inner) => lua_value_is_immutable(ws, inner),
        MonoTy::App {
            ctor: MonoCtor::Nominal(name),
            params,
        } if params.is_empty() => lua_nominal_is_immutable(ws, *name, &mut Vec::new()),
        _ => false,
    }
}

fn lua_nominal_is_immutable(ws: &Workspace, name: Ustr, visiting: &mut Vec<Ustr>) -> bool {
    if is_payload_free_enum(ws, name) {
        return true;
    }
    let Some(sid) = ws.nominals.struct_id(name) else {
        return false;
    };
    let sd = &ws.modules[sid.module.index()].structs[sid.def];
    if !has_value_plan(sd) || !sd.is_nobox || !sd.generics.is_empty() || visiting.contains(&name) {
        return false;
    }
    visiting.push(name);
    let immutable = sd
        .fields
        .iter()
        .all(|field| lua_decl_is_immutable(ws, field.ty, visiting));
    visiting.pop();
    immutable
}

fn lua_decl_is_immutable(ws: &Workspace, ty: TyId, visiting: &mut Vec<Ustr>) -> bool {
    match static_ty(ws, ty) {
        Some(TypeExpr::Builtin(
            BuiltinTy::Int
            | BuiltinTy::Uint
            | BuiltinTy::Float
            | BuiltinTy::Bool
            | BuiltinTy::String,
        )) => true,
        Some(TypeExpr::Nullable(inner)) => lua_decl_is_immutable(ws, ty.with_idx(inner), visiting),
        Some(TypeExpr::App {
            ctor: TypeCtor::Nominal(child),
            params,
        }) if params.is_empty() => lua_nominal_is_immutable(ws, child, visiting),
        _ => false,
    }
}

/// A payload-free enum variant's declaration-order index is its runtime `int`.
pub(crate) fn payload_free_ordinal(ws: &Workspace, name: Ustr, variant: Ustr) -> Option<i64> {
    payload_free_enum_variants(ws, name)?
        .iter()
        .position(|v| *v == variant)
        .map(|p| p as i64)
}

/// Boxes sanctioned later at boundaries such as `@gpu_only` go through
/// `authorize_nobox_box`.
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
            BoxKind::Implicit => Inst::MakeValueStruct {
                name,
                representation,
                form,
                fields,
            },
            BoxKind::Explicit => Inst::MaterializeValueStruct {
                name,
                representation,
                form,
                fields,
            },
        }
    }
}

/// A value struct lives as an unboxed `Group` of per-field reprs; everything
/// else is one already-lowered `Scalar`.
#[derive(Clone)]
pub(crate) enum ValueRepr {
    Scalar(Idx<Inst>),
    Group {
        name: Ustr,
        parts: Vec<ValueRepr>,
        present: Option<Idx<Inst>>,
    },
    /// Produced only by a splatting tuple return. Packing it emits `MakeTuple` of
    /// packed parts, which is how a `nobox` element still reaches the rejection.
    Tuple {
        parts: Vec<ValueRepr>,
        /// For a `(..)?` return: the presence bool that led the wire.
        present: Option<Idx<Inst>>,
    },
}

/// Forces a caller to state which RepId a representation conforms to.
#[derive(Clone)]
pub(crate) struct ValueBundle {
    representation: RepId,
    value: ValueRepr,
}

impl ValueBundle {
    pub(crate) fn new(representation: RepId, value: ValueRepr) -> Self {
        Self {
            representation,
            value,
        }
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

/// A box is absent when nil; an unboxed value when its tag says so. Packing it
/// to ask the nil question builds a non-nil ctor and deletes the absent arm.
#[derive(Clone)]
pub(crate) enum SafeNavRecv {
    Boxed(Idx<Inst>),
    Unboxed { repr: ValueRepr, present: Idx<Inst> },
}

#[derive(Clone, Copy)]
pub(crate) enum SpreadDest {
    Container(ContainerKind),
    Flat(ColumnElem, FlatStorage),
}

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
            name: crate::names::ustr(name),
            parts,
            present: None,
        }
    }
}

pub(crate) fn static_ty(ws: &Workspace, ty: TyId) -> Option<TypeExpr> {
    match ty {
        TyId::Static(m, idx) => Some(ws.module(m).ty_arena[idx]),
        TyId::Local(_) => None,
    }
}

pub(crate) fn value_shape(ws: &Workspace, name: Ustr) -> Option<ValueShape> {
    planned_value(ws, name)?.scalar_shape().cloned()
}

/// The one storage classification; callers must not rediscover `_ct`, `_vt`
/// or `_lct` from field syntax.
pub(crate) fn declared_value_storage(
    ws: &Workspace,
    _target: bone_core::hir::FnBackend,
    name: Ustr,
) -> Option<ValueStorage> {
    planned_value(ws, name).map(|plan| plan.storage)
}

pub fn gc_free_value_struct_field_order(ws: &Workspace, name: Ustr) -> Option<Vec<Ustr>> {
    ws.nominals.struct_id(name)?;
    planned_value(ws, name)?
        .scalarizable
        .then(|| struct_field_names(ws, name))
}

pub(crate) fn value_shape_gc(ws: &Workspace, name: Ustr) -> Option<ValueShape> {
    let plan = planned_value(ws, name)?;
    (plan.storage != ValueStorage::CData).then(|| plan.shape.clone())
}

/// Declared field names in positional `<name>_ct` ctor order; a cdata-eligible
/// enum is `[__tag, _0]`.
pub fn struct_field_names(ws: &Workspace, name: Ustr) -> Vec<Ustr> {
    let Some(sid) = ws.nominals.struct_id(name) else {
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
        TypeExpr::App {
            ctor: TypeCtor::Array,
            params,
        } => {
            let elem = ret.with_idx(params.into_iter().next()?);
            matches!(static_ty(ws, elem)?, TypeExpr::Builtin(BuiltinTy::Int))
                .then_some(VertexExpandRet::Sizes)
        }
        _ => None,
    }
}

pub(crate) fn vertex_expand_ret_msg(ws: &Workspace, fname: Ustr, ret: Option<TyId>) -> String {
    format!(
        "`@vertex_expand` fn `{fname}` must return `int` (the stride in floats) or `[int]` \
         (per-attribute component counts), got `{}`",
        ret.map(|r| ws.render(None, r))
            .unwrap_or_else(|| "()".to_string())
    )
}

pub(crate) fn vertex_expand_arg_msg(fname: Ustr) -> String {
    format!(
        "`@vertex_expand` fn `{fname}` argument must be a `FlatVec<E>` or `NativeBuffer<E>` \
         whose element is a concrete `value` struct -- the layout is derived from that element"
    )
}

/// Mismatched leaf paths would write equal-count leaves to the wrong offsets.
pub fn uniform_block_checked(
    ws: &Workspace,
    name: Ustr,
) -> Result<crate::gpu_layout::UniformBlockPlan, String> {
    let plan = crate::gpu_layout::uniform_block(ws, name).map_err(|e| e.message.clone())?;
    let Some(shape) = value_shape(ws, name) else {
        return Err(format!("`{name}` has a std140 plan but no value shape"));
    };
    let want: Vec<String> = value_leaf_paths(&shape)
        .iter()
        .map(|u| u.to_string())
        .collect();
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

pub(crate) fn uniform_semantic_checked(
    ws: &Workspace,
    name: Ustr,
) -> Result<Vec<(Ustr, crate::gpu_layout::WireKind)>, String> {
    let members =
        crate::gpu_layout::uniform_group_members(ws, name).map_err(|e| e.message.clone())?;
    let Some(shape) = value_shape(ws, name) else {
        return Err(format!(
            "`{name}` has a uniform interface but no value shape"
        ));
    };
    let want: Vec<String> = value_leaf_paths(&shape)
        .iter()
        .map(|u| u.to_string())
        .collect();
    let got: Vec<String> = members
        .iter()
        .flat_map(|(field, kind)| {
            kind.leaf_suffixes()
                .into_iter()
                .map(move |suffix| format!("{field}{suffix}"))
        })
        .collect();
    if want != got {
        return Err(format!(
            "logical uniform and value walkers disagree for `{name}`: value leaves {want:?} vs interface leaves {got:?}"
        ));
    }
    Ok(members)
}

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
    let Some(sid) = ws.nominals.struct_id(block_name) else {
        return Err(format!(
            "`@uniform_values` must return a named packet struct, got `{block_name}`"
        ));
    };
    let sd = &ws.modules[sid.module.index()].structs[sid.def];
    let Some(fp) = sd.fields.iter().find(|f| f.name.as_str() == "fingerprint") else {
        return Err(format!("`{block_name}` needs an `int fingerprint` field"));
    };
    if !matches!(
        static_ty(ws, fp.ty),
        Some(TypeExpr::Builtin(BuiltinTy::Int))
    ) {
        return Err(format!("`{block_name}.fingerprint` must be an int"));
    }
    let Some(leaves) = sd.fields.iter().find(|f| f.name.as_str() == "leaves") else {
        return Err(format!(
            "`{block_name}` needs a `[CanonicalUniformLeaf] leaves` field"
        ));
    };
    let Some(TypeExpr::App {
        ctor: TypeCtor::Array,
        params,
    }) = static_ty(ws, leaves.ty)
    else {
        return Err(format!(
            "`{block_name}.leaves` must be an array of canonical leaves"
        ));
    };
    let elem = leaves.ty.with_idx(
        params
            .into_iter()
            .next()
            .ok_or_else(|| format!("`{block_name}.leaves` must name its canonical leaf type"))?,
    );
    let TypeExpr::App {
        ctor: TypeCtor::Nominal(leaf_enum),
        ..
    } = static_ty(ws, elem)
        .ok_or_else(|| format!("`{block_name}.leaves` must contain a named canonical leaf enum"))?
    else {
        return Err(format!(
            "`{block_name}.leaves` must contain a named canonical leaf enum"
        ));
    };
    let Some(eid) = ws.nominals.enum_id(leaf_enum) else {
        return Err(format!("canonical leaf type `{leaf_enum}` is not an enum"));
    };
    let ed = &ws.modules[eid.module.index()].enums[eid.def];
    if ed.variants.len() != 4 {
        return Err(format!(
            "canonical leaf enum `{leaf_enum}` must have float, int, uint and bool variants"
        ));
    }
    let mut float_variant = None;
    let mut int_variant = None;
    let mut uint_variant = None;
    let mut bool_variant = None;
    for variant in &ed.variants {
        let VariantPayload::Tuple(types) = &variant.payload else {
            return Err(format!(
                "canonical leaf `{}` must carry one primitive payload",
                variant.name
            ));
        };
        let Some(&ty) = types.first() else {
            return Err(format!(
                "canonical leaf `{}` must carry one primitive payload",
                variant.name
            ));
        };
        if types.len() != 1 {
            return Err(format!(
                "canonical leaf `{}` must carry one primitive payload",
                variant.name
            ));
        }
        match static_ty(ws, ty) {
            Some(TypeExpr::Builtin(BuiltinTy::Float)) => float_variant = Some(variant.name),
            Some(TypeExpr::Builtin(BuiltinTy::Int)) => int_variant = Some(variant.name),
            Some(TypeExpr::Builtin(BuiltinTy::Uint)) => uint_variant = Some(variant.name),
            Some(TypeExpr::Builtin(BuiltinTy::Bool)) => bool_variant = Some(variant.name),
            _ => {
                return Err(format!(
                    "canonical leaf `{}` must carry float, int, uint or bool",
                    variant.name
                ));
            }
        }
    }
    let (Some(float_variant), Some(int_variant), Some(uint_variant), Some(bool_variant)) =
        (float_variant, int_variant, uint_variant, bool_variant)
    else {
        return Err(format!(
            "canonical leaf enum `{leaf_enum}` needs float, int, uint and bool variants"
        ));
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

pub(crate) fn value_struct_field_tys(ws: &Workspace, name: Ustr) -> Option<Vec<(Ustr, TyId)>> {
    let sid = ws.nominals.struct_id(name)?;
    let sd = &ws.modules[sid.module.index()].structs[sid.def];
    if !has_value_plan(sd) {
        return None;
    }
    Some(sd.fields.iter().map(|f| (f.name, f.ty)).collect())
}

/// A flat enum encodes absence in its tag, so only structs gain a presence lane.
pub(crate) fn sig_shape_of_nullable_nominal(ws: &Workspace, n: Ustr) -> Option<ValueShape> {
    if ws.nominals.struct_id(n).is_some()
        && let Some(mut shape) = planned_value(ws, n).map(|plan| plan.shape.clone())
    {
        shape.nullable = true;
        return Some(shape);
    }
    flat_enum_value_shape(ws, n)
}

pub(crate) fn value_shape_of_nominal(ws: &Workspace, name: Ustr) -> Option<ValueShape> {
    planned_value(ws, name)
        .map(|plan| plan.shape.clone())
        .or_else(|| flat_enum_value_shape(ws, name))
}

fn flat_enum_value_shape(ws: &Workspace, name: Ustr) -> Option<ValueShape> {
    EnumPlan::for_name(ws, name)?
        .flat()
        .map(EnumFlatShape::value_shape)
}

/// A `nobox` global is one scalar Lua global per leaf, never one boxed cdata.
pub(crate) fn nobox_global_leaf_name(base: Ustr, leaf: usize) -> Ustr {
    crate::names::ustr(&format!("__nbg_{base}_{leaf}"))
}

pub(crate) fn is_nobox_nominal(ws: &Workspace, name: Ustr) -> bool {
    ws.nominals
        .struct_id(name)
        .is_some_and(|sid| ws.modules[sid.module.index()].structs[sid.def].is_nobox)
}

pub struct NoboxGlobalAccessorShape {
    pub ty_name: Ustr,
    pub paths: Vec<Ustr>,
    pub nullable: bool,
    pub storage: ValueStorage,
    pub ctor_suffix: &'static str,
    /// `_lct` is positional over physical leaves and `_vt` over box keys, so a GC
    /// table with a nested boxed child cannot be rebuilt from flattened storage.
    pub unbuildable: Option<String>,
}

pub fn nobox_global_accessor_shape(
    ws: &Workspace,
    scx: &ScalarCtx,
    plan: &GlobalPlan,
) -> Result<Option<NoboxGlobalAccessorShape>, RepError> {
    let Some(shape) = plan.value_shape() else {
        return Ok(None);
    };
    let paths = value_leaf_paths(shape);
    let storage = plan.value_storage().ok_or_else(|| RepError {
        span: None,
        message: format!(
            "`{}` has no physical storage plan for global accessors",
            shape.name
        ),
    })?;
    let has_nested = paths.iter().any(|p| p.contains('.'));
    let ctor_suffix = match storage {
        ValueStorage::LeafTable => "_lct",
        ValueStorage::GcTable => "_vt",
        ValueStorage::CData if has_nested => "_lct",
        ValueStorage::CData => "_ct",
    };
    // `_vt` is positional over its box keys: a declared field whole, a `nobox`
    // field by its dotted leaves. It is the accessor's ctor exactly when those
    // keys are the storage's leaf paths.
    let unbuildable = if storage == ValueStorage::GcTable {
        let keys: Vec<Ustr> = super::gc_table_fields(ws, scx.target, shape.name)?
            .iter()
            .flat_map(|f| f.keys().into_iter().map(|(key, _)| key))
            .collect();
        (keys != paths).then(|| {
            format!(
                "`{}{ctor_suffix}` is positional over its box keys {keys:?}, but this \
                 type's storage is the leaves {paths:?}, so the accessor would pass a leaf \
                 where the ctor wants a nested value",
                shape.name,
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
        unbuildable,
    }))
}

pub type SpecSubst = crate::monoty::MonoSubst;

pub(crate) fn mono_value_shape(ws: &Workspace, ty: &MonoTy) -> Option<ValueShape> {
    let (payload, nullable) = ty.peel_nullable();
    let name = payload.nominal()?;
    if nullable {
        sig_shape_of_nullable_nominal(ws, name)
    } else {
        value_shape_of_nominal(ws, name)
    }
}

pub(crate) fn mono_nobox_value_shape(ws: &Workspace, ty: &MonoTy) -> Option<ValueShape> {
    let name = ty.peel_nullable().0.nominal()?;
    if !is_nobox_nominal(ws, name) {
        return None;
    }
    mono_value_shape(ws, ty)
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

pub(crate) fn value_shape_can_fill(actual: &ValueShape, expected: &ValueShape) -> bool {
    actual.name == expected.name
        && (!actual.nullable || expected.nullable)
        && actual.fields.len() == expected.fields.len()
        && actual.fields.iter().zip(&expected.fields).all(
            |((actual_name, actual_field), (expected_name, expected_field))| {
                actual_name == expected_name
                    && match (actual_field, expected_field) {
                        (FieldShape::Prim, FieldShape::Prim) => true,
                        (FieldShape::Nested(actual), FieldShape::Nested(expected)) => {
                            value_shape_can_fill(actual, expected)
                        }
                        _ => false,
                    }
            },
        )
}

/// Cdata enum layout: payload slots are shared by position with identical C
/// types across variants; shorter variants pad unused slots.
/// Record and ref payloads are ineligible; payload-free enums stay plain ints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnumCdataShape {
    pub name: Ustr,
    /// `_<i>` for a tuple field at position `i`, `_<field>` for a record field.
    pub slots: Vec<(Ustr, &'static str)>,
    /// `(variant, payload arity)`; the tag is the variant's declaration index.
    pub variants: Vec<(Ustr, usize)>,
}

impl EnumCdataShape {
    pub(crate) fn tag_of(&self, v: Ustr) -> Option<i64> {
        self.variants
            .iter()
            .position(|(n, _)| *n == v)
            .map(|p| p as i64)
    }

    /// Field names in `<Enum>_ct` ctor order: `[__tag, <slot names>]`.
    pub fn field_names(&self) -> Vec<Ustr> {
        let mut v = Vec::with_capacity(1 + self.slots.len());
        v.push(crate::names::ustr("__tag"));
        v.extend(self.slots.iter().map(|(n, _)| *n));
        v
    }

    pub(crate) fn value_shape(&self) -> ValueShape {
        ValueShape {
            name: self.name,
            fields: self
                .field_names()
                .into_iter()
                .map(|n| (n, FieldShape::Prim))
                .collect(),
            nullable: false,
        }
    }
}

/// `_<i>` for a tuple position, `_<field>` for a record field.
/// Record field names are identifiers, so they never collide with tuple positions.
pub(crate) fn slot_name(suffix: &str) -> Ustr {
    crate::names::concat(&["_", suffix])
}

fn primitive_c_type(ty: &TypeExpr) -> Option<&'static str> {
    match ty {
        TypeExpr::Builtin(BuiltinTy::Int) => Some("int"),
        TypeExpr::Builtin(BuiltinTy::Uint) => Some("uint32_t"),
        TypeExpr::Builtin(BuiltinTy::Float) => Some("double"),
        TypeExpr::Builtin(BuiltinTy::F32) => Some("float"),
        TypeExpr::Builtin(BuiltinTy::Bool) => Some("bool"),
        _ => None,
    }
}

fn enum_payload_c_type(ws: &Workspace, ty: TyId) -> Option<&'static str> {
    let ty = static_ty(ws, ty)?;
    // F32 describes a field's storage and is not an enum payload type.
    if matches!(ty, TypeExpr::Builtin(BuiltinTy::F32)) {
        return None;
    }
    primitive_c_type(&ty)
}

fn build_enum_cdata_shape(ws: &Workspace, ed: &EnumDef) -> Option<EnumCdataShape> {
    let mut slots: Vec<(Ustr, &'static str)> = Vec::new();
    let mut variants: Vec<(Ustr, usize)> = Vec::with_capacity(ed.variants.len());
    let mut add_slot = |nm: Ustr, c: &'static str| -> Option<()> {
        match slots.iter().find(|(n, _)| *n == nm) {
            Some((_, existing)) if *existing != c => None,
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
                    add_slot(slot_name(&i.to_string()), enum_payload_c_type(ws, *ty)?)?;
                }
                tys.len()
            }
            VariantPayload::Record(fields) => {
                for f in fields {
                    add_slot(slot_name(f.name.as_str()), enum_payload_c_type(ws, f.ty)?)?;
                }
                fields.len()
            }
        };
        variants.push((v.name, arity));
    }
    // A payload-free enum is a plain int.
    (!slots.is_empty()).then_some(EnumCdataShape {
        name: ed.name,
        slots,
        variants,
    })
}

pub enum CFieldType {
    Scalar(&'static str),
    Aggregate(Ustr),
}

impl From<&CMember> for CFieldType {
    fn from(member: &CMember) -> Self {
        match member {
            CMember::Scalar(ty) => Self::Scalar(ty),
            CMember::Aggregate(name, _) => Self::Aggregate(*name),
        }
    }
}

pub fn value_c_fields(ws: &Workspace, name: Ustr) -> Option<Vec<(Ustr, CFieldType)>> {
    let plan = planned_value(ws, name)?;
    Some(
        plan.cdata
            .as_ref()?
            .fields
            .iter()
            .map(|(name, _, member)| (*name, member.into()))
            .collect(),
    )
}

pub struct ValueCLayout {
    pub size: usize,
    pub align: usize,
    pub fields: Vec<(Ustr, usize, CFieldType)>,
}

pub fn value_c_layout(ws: &Workspace, name: Ustr) -> Option<ValueCLayout> {
    let plan = planned_value(ws, name)?;
    let layout = plan.cdata.as_ref()?;
    Some(ValueCLayout {
        size: layout.size(),
        align: layout.align,
        fields: layout
            .fields
            .iter()
            .map(|(name, offset, member)| (*name, *offset, member.into()))
            .collect(),
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GcFieldEq {
    Reference,
    Native,
    Structural,
}

pub fn gc_field_eq(ws: &Workspace, ty: TyId) -> GcFieldEq {
    match static_ty(ws, ty) {
        Some(ref primitive) if primitive_c_type(primitive).is_some() => GcFieldEq::Native,
        Some(TypeExpr::Nullable(inner)) => gc_field_eq(ws, ty.with_idx(inner)),
        Some(TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            ..
        }) => nominal_gc_eq(ws, name),
        _ => GcFieldEq::Reference,
    }
}

/// [`gc_field_eq`] of a field of the layout `owner`, substituted when `owner`
/// is an instance.
pub(crate) fn owner_field_eq(ws: &Workspace, owner: Ustr, ty: TyId) -> GcFieldEq {
    let Some(mono) = instance_payload_ty(ws, owner, ty) else {
        return gc_field_eq(ws, ty);
    };
    match mono.peel_nullable().0 {
        MonoTy::Int | MonoTy::Uint | MonoTy::Float | MonoTy::Bool => GcFieldEq::Native,
        MonoTy::App {
            ctor: MonoCtor::Nominal(name),
            ..
        } => nominal_gc_eq(ws, *name),
        _ => GcFieldEq::Reference,
    }
}

fn nominal_gc_eq(ws: &Workspace, name: Ustr) -> GcFieldEq {
    if is_value_struct(ws, name)
        || is_payload_free_enum(ws, name)
        || EnumPlan::for_name(ws, name).is_some_and(|plan| plan.cdata().is_some())
    {
        GcFieldEq::Native
    } else if ws.nominals.enum_id(name).is_some() {
        GcFieldEq::Structural
    } else {
        GcFieldEq::Reference
    }
}

/// Flattened Lua table layout for a boxed non-cdata enum; the variant name
/// remains the discriminant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnumFlatShape {
    pub(crate) name: Ustr,
    // Variant shapes share leading slots at each position. An empty list needs one opaque slot.
    positions: Vec<EnumPosition>,
    width: usize,
    named_shapes: std::sync::Arc<[EnumNamedPayload]>,
    /// Physical named slots, including dotted record leaves.
    pub(crate) record_fields: std::sync::Arc<[Ustr]>,
    /// `(variant, position, index into that position's shapes)`.
    payloads: std::sync::Arc<[(Ustr, usize, usize)]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct EnumPosition {
    offset: usize,
    shapes: Vec<ValueShape>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EnumNamedPayload {
    variant: Ustr,
    field: Ustr,
    pub(crate) shape: ValueShape,
    pub(crate) keys: Vec<Ustr>,
}

pub(super) fn enum_position_ty(
    ws: &Workspace,
    name: Ustr,
    variant: Ustr,
    i: usize,
) -> Option<TyId> {
    let ed = enum_definition(ws, name)?;
    let VariantPayload::Tuple(types) = &ed.variants.iter().find(|v| v.name == variant)?.payload
    else {
        return None;
    };
    types.get(i).copied()
}

impl EnumFlatShape {
    pub fn name(&self) -> Ustr {
        self.name
    }

    pub fn record_fields(&self) -> &[Ustr] {
        &self.record_fields
    }

    /// Total positional slots `_0 .. _{tuple_width-1}`.
    /// Distinct from the position count: a `nobox` position spans several slots.
    pub fn tuple_width(&self) -> usize {
        self.width
    }

    /// Positions past the end answer the total width, so a narrower variant has
    /// nothing to write.
    pub fn slot_offset(&self, pos: usize) -> usize {
        self.positions.get(pos).map_or(self.width, |p| p.offset)
    }

    fn find_position_shape(&self, pos: usize, name: Ustr, nullable: bool) -> Option<&ValueShape> {
        self.positions
            .get(pos)?
            .shapes
            .iter()
            .find(|shape| shape.name == name && shape.nullable == nullable)
    }

    /// The constructor, pattern and descriptor use the variant's declared type,
    /// substituted for an instance.
    pub(crate) fn payload_shape(
        &self,
        _ws: &Workspace,
        variant: Ustr,
        pos: usize,
    ) -> Option<&ValueShape> {
        let &(_, _, index) = self
            .payloads
            .iter()
            .find(|(v, p, _)| *v == variant && *p == pos)?;
        self.positions.get(pos)?.shapes.get(index)
    }

    fn same_layout(&self, other: &Self) -> bool {
        self.positions == other.positions
            && self.width == other.width
            && self.named_shapes == other.named_shapes
            && self.record_fields == other.record_fields
            && self.payloads == other.payloads
    }

    pub fn has_inline_record_payload(&self, ws: &Workspace) -> bool {
        !self.named_shapes.is_empty()
            || self
                .positions
                .iter()
                .any(|p| p.shapes.iter().any(|s| track_enum_payload_presence(ws, s)))
    }

    pub(crate) fn named_payload(&self, variant: Ustr, field: Ustr) -> Option<&EnumNamedPayload> {
        self.named_shapes
            .iter()
            .find(|p| p.variant == variant && p.field == field)
    }

    pub fn position_leaf_paths(&self, pos: usize, name: Ustr) -> Option<Vec<Ustr>> {
        self.find_position_shape(pos, name, false)
            .map(value_leaf_paths)
    }

    pub fn leaf_count(&self) -> usize {
        1 + self.tuple_width() + self.record_fields.len()
    }

    /// The leaves a holder keys under the field: tag, then payload slots.
    pub(crate) fn value_shape(&self) -> ValueShape {
        ValueShape {
            name: self.name,
            fields: super::flat_leaf_names(self.tuple_width(), &self.record_fields)
                .map(|leaf| (leaf, FieldShape::Prim))
                .collect(),
            nullable: false,
        }
    }
}

fn build_flat_enum_shape(
    ws: &Workspace,
    ed: &EnumDef,
    name: Ustr,
    subst: Option<&MonoSubst>,
    visiting: &mut Vec<Ustr>,
) -> EnumFlatShape {
    visiting.push(name);
    // Distinct value payload shapes share a position by width. Letting one fall
    // back to the opaque slot would reintroduce an aggregate at the enum boundary.
    let mut positions: Vec<Vec<ValueShape>> = Vec::new();
    let mut record_fields: Vec<Ustr> = Vec::new();
    let mut named_shapes = Vec::new();
    let mut payloads = Vec::new();
    for v in &ed.variants {
        match &v.payload {
            VariantPayload::Unit => {}
            VariantPayload::Tuple(tys) => {
                if positions.len() < tys.len() {
                    positions.resize(tys.len(), Vec::new());
                }
                for (i, ty) in tys.iter().enumerate() {
                    let Some(vs) = PayloadTy::of(ws, *ty, subst)
                        .and_then(|ty| flat_payload_shape(ws, &ty, visiting))
                    else {
                        continue;
                    };
                    let index = match positions[i]
                        .iter()
                        .position(|prev| prev.name == vs.name && prev.nullable == vs.nullable)
                    {
                        Some(index) => index,
                        None => {
                            positions[i].push(vs);
                            positions[i].len() - 1
                        }
                    };
                    payloads.push((v.name, i, index));
                }
            }
            VariantPayload::Record(fields) => {
                for f in fields {
                    if let Some(shape) = PayloadTy::of(ws, f.ty, subst)
                        .and_then(|ty| nobox_payload_shape(ws, &ty, visiting))
                    {
                        let keys: Vec<_> = leaf_keys(&shape, f.name.as_str())
                            .into_iter()
                            .map(|(key, _)| key)
                            .collect();
                        for &key in &keys {
                            if !record_fields.contains(&key) {
                                record_fields.push(key);
                            }
                        }
                        named_shapes.push(EnumNamedPayload {
                            variant: v.name,
                            field: f.name,
                            shape,
                            keys,
                        });
                    } else if !record_fields.contains(&f.name) {
                        record_fields.push(f.name);
                    }
                }
            }
        }
    }
    visiting.pop();
    let mut width = 0;
    let positions = positions
        .into_iter()
        .map(|shapes| {
            let offset = width;
            width += shapes
                .iter()
                .map(ValueShape::leaf_count)
                .max()
                .unwrap_or(0)
                .max(1);
            EnumPosition { offset, shapes }
        })
        .collect();
    EnumFlatShape {
        name,
        positions,
        width,
        named_shapes: named_shapes.into(),
        record_fields: record_fields.into(),
        payloads: payloads.into(),
    }
}

fn nonflat_enum_layout(ws: &Workspace, ed: &EnumDef) -> Option<EnumLayout> {
    if is_unit_enum(ed) {
        Some(EnumLayout::PayloadFree)
    } else {
        build_enum_cdata_shape(ws, ed).map(EnumLayout::Cdata)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnumLayout {
    PayloadFree,
    Cdata(EnumCdataShape),
    Flat(EnumFlatShape),
}

/// The single static representation decision for an enum; no caller chooses
/// between the cdata and flat views independently.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnumPlan {
    name: Ustr,
    layout: EnumLayout,
}

impl EnumPlan {
    pub fn for_name(ws: &Workspace, name: Ustr) -> Option<Rc<Self>> {
        let ed = enum_definition(ws, name)?;
        Some(ShapeMemo::with_active(
            |memo| memo.enums.get(&name).cloned(),
            |memo, plan| {
                memo.enums.insert(name, plan.clone());
            },
            || {
                let layout = nonflat_enum_layout(ws, ed).unwrap_or_else(|| {
                    EnumLayout::Flat(flat_enum_shape(ws, ed, name, &mut Vec::new()))
                });
                Rc::new(Self { name, layout })
            },
        ))
    }

    /// Whether an instance plans exactly like its declaration, which then
    /// serves it under the declaration's name.
    pub(crate) fn instance_matches_declaration(ws: &Workspace, name: Ustr) -> bool {
        let base = crate::layout_instance::decl_name(name);
        match (Self::for_name(ws, name), Self::for_name(ws, base)) {
            (Some(inst), Some(decl)) => match (&inst.layout, &decl.layout) {
                (EnumLayout::Flat(inst), EnumLayout::Flat(decl)) => inst.same_layout(decl),
                _ => true,
            },
            _ => true,
        }
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

/// A payload's or field's declared type, or its substitution when it names a
/// generic of the instance being planned.
pub(crate) enum PayloadTy {
    Decl(TyId),
    Mono(MonoTy),
}

impl PayloadTy {
    pub(crate) fn of(ws: &Workspace, ty: TyId, subst: Option<&MonoSubst>) -> Option<Self> {
        match subst {
            Some(subst) if mentions_generic(ws, ty) => crate::monoty::convert_decl(ws, ty, subst)
                .ok()
                .map(Self::Mono),
            _ => Some(Self::Decl(ty)),
        }
    }

    pub(crate) fn nominal(&self, ws: &Workspace) -> Option<(Ustr, bool)> {
        match self {
            Self::Decl(ty) => nominal_name_from_ty(ws, *ty),
            Self::Mono(ty) => {
                let (inner, nullable) = ty.peel_nullable();
                Some((inner.nominal()?, nullable))
            }
        }
    }

    fn is_int(&self, ws: &Workspace) -> bool {
        match self {
            Self::Decl(ty) => matches!(static_ty(ws, *ty), Some(TypeExpr::Builtin(BuiltinTy::Int))),
            Self::Mono(ty) => *ty == MonoTy::Int,
        }
    }

    fn primitive_c_type(&self, ws: &Workspace) -> Option<&'static str> {
        match self {
            Self::Decl(ty) => primitive_c_type(&static_ty(ws, *ty)?),
            Self::Mono(MonoTy::Int) => Some("int"),
            Self::Mono(MonoTy::Uint) => Some("uint32_t"),
            Self::Mono(MonoTy::Float) => Some("double"),
            Self::Mono(MonoTy::Bool) => Some("bool"),
            Self::Mono(_) => None,
        }
    }

    fn reaches(&self, ws: &Workspace, goals: &[Ustr], skip_boxed_optional: bool) -> bool {
        let mut seen = HashSet::default();
        match self {
            Self::Decl(ty) => reaches_enum_in(ws, *ty, goals, skip_boxed_optional, &mut seen),
            Self::Mono(ty) => reaches_enum_in_mono(ws, ty, goals, skip_boxed_optional, &mut seen),
        }
    }
}

pub(crate) fn mentions_generic(ws: &Workspace, ty: TyId) -> bool {
    let TyId::Static(mid, idx) = ty else {
        return false;
    };
    let sub = |idx| mentions_generic(ws, TyId::Static(mid, idx));
    match ws.module(mid).ty_arena[idx] {
        TypeExpr::Generic(..) | TypeExpr::Infer(..) => true,
        TypeExpr::Nullable(inner) => sub(inner),
        TypeExpr::Tuple { elements } => elements.into_iter().any(sub),
        TypeExpr::App { params, .. } => params.into_iter().any(sub),
        TypeExpr::Function { params, ret } => params.into_iter().any(sub) || sub(ret),
        TypeExpr::Builtin(_) => false,
    }
}

fn nobox_payload_shape(ws: &Workspace, ty: &PayloadTy, visiting: &[Ustr]) -> Option<ValueShape> {
    let (name, nullable) = ty.nominal(ws)?;
    if !is_nobox_nominal(ws, name) || ty.reaches(ws, visiting, true) {
        return None;
    }
    let mut shape = value_shape_of_nominal(ws, name)?;
    shape.nullable = nullable;
    Some(shape)
}

/// Keyed on the declared payload type, so the ctor and the match arm derive
/// the same layout without seeing each other.
fn flat_payload_shape(
    ws: &Workspace,
    ty: &PayloadTy,
    visiting: &mut Vec<Ustr>,
) -> Option<ValueShape> {
    let (n, nullable) = ty.nominal(ws)?;
    if nullable || is_nobox_nominal(ws, n) {
        return nobox_payload_shape(ws, ty, visiting);
    }
    if is_value_struct(ws, n) {
        // Planned without `visiting`, so a payload cycling back to an enum under
        // construction stays opaque.
        if ty.reaches(ws, visiting, false) {
            return None;
        }
        return value_shape_of_nominal(ws, n);
    }
    let ed = enum_definition(ws, n)?;
    if visiting
        .iter()
        .any(|v| crate::layout_instance::decl_name(*v) == ed.name)
        || nonflat_enum_layout(ws, ed).is_some()
    {
        return None;
    }
    Some(flat_enum_shape(ws, ed, n, visiting).value_shape())
}

/// `name` is the declaration or one of its instances.
fn flat_enum_shape(
    ws: &Workspace,
    ed: &EnumDef,
    name: Ustr,
    visiting: &mut Vec<Ustr>,
) -> EnumFlatShape {
    let subst = crate::layout_instance::instance(name)
        .and_then(|inst| crate::layout_instance::instance_subst(ws, &inst));
    build_flat_enum_shape(ws, ed, name, subst.as_ref(), visiting)
}

/// The substituted counterpart of [`reaches_enum_in`]; a declaration reached
/// from here is walked as declared.
fn reaches_enum_in_mono(
    ws: &Workspace,
    ty: &MonoTy,
    goals: &[Ustr],
    skip_boxed_optional: bool,
    seen: &mut HashSet<Ustr>,
) -> bool {
    let reaches = |ty: &MonoTy, seen: &mut HashSet<Ustr>| {
        reaches_enum_in_mono(ws, ty, goals, skip_boxed_optional, seen)
    };
    match ty {
        MonoTy::Nullable(inner) => {
            (!skip_boxed_optional
                || inner
                    .nominal()
                    .is_some_and(|name| is_nobox_nominal(ws, name)))
                && reaches(inner, seen)
        }
        MonoTy::Tuple(parts) => parts.iter().any(|part| reaches(part, seen)),
        MonoTy::App { ctor, params } => {
            if params.iter().any(|param| reaches(param, seen)) {
                return true;
            }
            let MonoCtor::Nominal(n) = ctor else {
                return false;
            };
            let decl = crate::layout_instance::decl_name(*n);
            if goals
                .iter()
                .any(|goal| crate::layout_instance::decl_name(*goal) == decl)
            {
                return true;
            }
            if !seen.insert(decl) {
                return false;
            }
            if let Some(sid) = ws.nominals.struct_id(decl) {
                let sd = &ws.modules[sid.module.index()].structs[sid.def];
                return sd.is_value
                    && sd
                        .fields
                        .iter()
                        .any(|f| reaches_enum_in(ws, f.ty, goals, skip_boxed_optional, seen));
            }
            let Some(ed) = enum_definition(ws, decl) else {
                return false;
            };
            let subst = crate::layout_instance::instance(*n)
                .and_then(|inst| crate::layout_instance::instance_subst(ws, &inst));
            let payload_reaches =
                |ty: TyId, seen: &mut HashSet<Ustr>| match PayloadTy::of(ws, ty, subst.as_ref()) {
                    Some(PayloadTy::Mono(mono)) => reaches(&mono, seen),
                    Some(PayloadTy::Decl(ty)) => {
                        reaches_enum_in(ws, ty, goals, skip_boxed_optional, seen)
                    }
                    None => false,
                };
            ed.variants.iter().any(|v| match &v.payload {
                VariantPayload::Unit => false,
                VariantPayload::Tuple(tys) => tys.iter().any(|t| payload_reaches(*t, seen)),
                VariantPayload::Record(fields) => {
                    fields.iter().any(|f| payload_reaches(f.ty, seen))
                }
            })
        }
        _ => false,
    }
}

fn reaches_enum_in(
    ws: &Workspace,
    ty: TyId,
    goals: &[Ustr],
    skip_boxed_optional: bool,
    seen: &mut HashSet<Ustr>,
) -> bool {
    let TyId::Static(mid, _) = ty else {
        return false;
    };
    let sub = |idx| TyId::Static(mid, idx);
    let reaches =
        |ty, seen: &mut HashSet<Ustr>| reaches_enum_in(ws, ty, goals, skip_boxed_optional, seen);
    match static_ty(ws, ty) {
        // Only nullable nobox records are expanded by the value field planner.
        Some(TypeExpr::Nullable(inner)) => {
            (!skip_boxed_optional
                || nominal_name_from_ty(ws, sub(inner))
                    .is_some_and(|(name, _)| is_nobox_nominal(ws, name)))
                && reaches(sub(inner), seen)
        }
        Some(TypeExpr::Tuple { elements }) => {
            elements.into_iter().any(|idx| reaches(sub(idx), seen))
        }
        Some(TypeExpr::App { ctor, params }) => {
            if params.into_iter().any(|idx| reaches(sub(idx), seen)) {
                return true;
            }
            let TypeCtor::Nominal(n) = ctor else {
                return false;
            };
            if goals
                .iter()
                .any(|goal| crate::layout_instance::decl_name(*goal) == n)
            {
                return true;
            }
            if !seen.insert(n) {
                return false;
            }
            if let Some(sid) = ws.nominals.struct_id(n) {
                let sd = &ws.modules[sid.module.index()].structs[sid.def];
                return sd.is_value && sd.fields.iter().any(|f| reaches(f.ty, seen));
            }
            if let Some(eid) = ws.nominals.enum_id(n) {
                let ed = &ws.modules[eid.module.index()].enums[eid.def];
                return ed.variants.iter().any(|v| match &v.payload {
                    VariantPayload::Unit => false,
                    VariantPayload::Tuple(tys) => tys.iter().any(|t| reaches(*t, seen)),
                    VariantPayload::Record(fields) => fields.iter().any(|f| reaches(f.ty, seen)),
                });
            }
            false
        }
        _ => false,
    }
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
    let Some(eid) = ws.nominals.enum_id(name) else {
        return false;
    };
    visiting.push(name);
    let ed = &ws.modules[eid.module.index()].enums[eid.def];
    let leafwise = ed.variants.iter().all(|variant| match &variant.payload {
        VariantPayload::Unit => true,
        VariantPayload::Tuple(tys) => tys
            .iter()
            .all(|ty| equality_payload_is_leafwise(ws, *ty, visiting)),
        VariantPayload::Record(fields) => fields
            .iter()
            .all(|field| equality_payload_is_leafwise(ws, field.ty, visiting)),
    });
    visiting.pop();
    leafwise
}

fn equality_payload_is_leafwise(ws: &Workspace, ty: TyId, visiting: &mut Vec<Ustr>) -> bool {
    let TyId::Static(mid, idx) = ty else {
        return false;
    };
    match ws.module(mid).ty_arena[idx] {
        TypeExpr::Builtin(
            BuiltinTy::Unit
            | BuiltinTy::Int
            | BuiltinTy::Uint
            | BuiltinTy::Float
            | BuiltinTy::F32
            | BuiltinTy::Bool
            | BuiltinTy::String
            | BuiltinTy::Never,
        )
        | TypeExpr::Function { .. } => true,
        TypeExpr::Nullable(inner) => {
            equality_payload_is_leafwise(ws, TyId::Static(mid, inner), visiting)
        }
        TypeExpr::App {
            ctor: TypeCtor::Nominal(n),
            ..
        } => {
            if is_value_struct(ws, n) {
                return true;
            }
            let Some(plan) = EnumPlan::for_name(ws, n) else {
                return false;
            };
            match plan.layout() {
                EnumLayout::PayloadFree | EnumLayout::Cdata(_) => true,
                EnumLayout::Flat(_) => flat_enum_equality_is_leafwise_inner(ws, n, visiting),
            }
        }
        TypeExpr::Builtin(BuiltinTy::Any | BuiltinTy::Error)
        | TypeExpr::Generic(..)
        | TypeExpr::Infer(..)
        | TypeExpr::App { .. }
        | TypeExpr::Tuple { .. } => false,
    }
}

/// The transform writes a flat-enum yield's leaves into reused `__y*` frame
/// slots (no allocation); the generic `resume()` rebuilds a box from them.
pub(crate) fn coroutine_yield_channel_mono(ws: &Workspace, ty: &MonoTy) -> Option<EnumFlatShape> {
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
    if !bone_core::modules::is_intrinsic_key(*name, "Coroutine") {
        return None;
    }
    let MonoTy::App {
        ctor: MonoCtor::Nominal(yield_name),
        ..
    } = params.first()?
    else {
        return None;
    };
    EnumPlan::for_name(ws, *yield_name).and_then(|plan| plan.flat().cloned())
}

/// The tag precedes positional and record leaves.
pub fn yield_slot(i: usize) -> Ustr {
    crate::names::ustr(format!("__y{i}").as_str())
}
