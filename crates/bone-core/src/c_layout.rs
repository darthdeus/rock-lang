//! C value layout and declared symbol spelling shared by FFI codegen and MIR lowering.

use std::borrow::Cow;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use ustr::Ustr;

use crate::hir::{BuiltinTy, EnumDef, StructDef, TyId, TypeCtor, TypeExpr, VariantPayload};
use crate::workspace::Workspace;

const SEP: &str = "_";

/// Size and alignment of a primitive C type as LuaJIT lays it out.
pub fn c_prim_size_align(cty: &str) -> Option<(usize, usize)> {
    match cty {
        "double" => Some((8, 8)),
        "int" | "uint32_t" | "float" => Some((4, 4)),
        "bool" => Some((1, 1)),
        _ => None,
    }
}

pub fn round_up(n: usize, align: usize) -> usize {
    n.div_ceil(align) * align
}

pub fn primitive_c_type(ty: &TypeExpr) -> Option<&'static str> {
    match ty {
        TypeExpr::Builtin(BuiltinTy::Int) => Some("int"),
        TypeExpr::Builtin(BuiltinTy::Uint) => Some("uint32_t"),
        TypeExpr::Builtin(BuiltinTy::Float) => Some("double"),
        TypeExpr::Builtin(BuiltinTy::F32) => Some("float"),
        TypeExpr::Builtin(BuiltinTy::Bool) => Some("bool"),
        _ => None,
    }
}

pub fn static_ty(ws: &Workspace, ty: TyId) -> Option<TypeExpr> {
    match ty {
        TyId::Static(m, idx) => Some(ws.module(m).ty_arena[idx]),
        TyId::Local(_) => None,
    }
}

/// Stdlib descriptors (`Ptr`, `NativeBuffer`) are excluded: the backend supplies
/// their one host slot, which a copy shares and never rebuilds.
pub fn has_value_plan(sd: &StructDef) -> bool {
    sd.is_value && !crate::modules::is_descriptor_key(sd.name)
}

pub fn is_registry_handle_decl(decl_name: &str) -> bool {
    matches!(decl_name, "Arena" | "Entity" | "Scene")
}

pub fn enum_definition(ws: &Workspace, name: Ustr) -> Option<&EnumDef> {
    let id = ws.nominals.enum_id(name)?;
    Some(&ws.modules[id.module.index()].enums[id.def])
}

pub fn is_unit_enum(ed: &EnumDef) -> bool {
    ed.variants
        .iter()
        .all(|v| matches!(v.payload, VariantPayload::Unit))
}

pub fn is_payload_free_enum(ws: &Workspace, name: Ustr) -> bool {
    enum_definition(ws, name).is_some_and(is_unit_enum)
}

fn enum_payload_c_type(ws: &Workspace, ty: TyId) -> Option<&'static str> {
    let ty = static_ty(ws, ty)?;
    // F32 describes a field's storage and is not an enum payload type.
    if matches!(ty, TypeExpr::Builtin(BuiltinTy::F32)) {
        return None;
    }
    primitive_c_type(&ty)
}

/// Payload slots and variant arities for a cdata-eligible enum.
/// Returns `None` for payload-free enums (plain ints) or enums with non-scalar payloads.
pub fn enum_cdata_slots(
    ws: &Workspace,
    ed: &EnumDef,
) -> Option<(Vec<(Ustr, &'static str)>, Vec<(Ustr, usize)>)> {
    if !ed.generics.is_empty() {
        return None;
    }
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
                    add_slot(Ustr::from(&format!("_{i}")), enum_payload_c_type(ws, *ty)?)?;
                }
                tys.len()
            }
            VariantPayload::Record(fields) => {
                for f in fields {
                    add_slot(
                        Ustr::from(&format!("_{}", f.name)),
                        enum_payload_c_type(ws, f.ty)?,
                    )?;
                }
                fields.len()
            }
        };
        variants.push((v.name, arity));
    }
    (!slots.is_empty()).then_some((slots, variants))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CLayout {
    pub fields: Vec<(Ustr, usize, CMember)>,
    pub end: usize,
    pub align: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CMember {
    Scalar(&'static str),
    Aggregate(Ustr, CLayout),
}

impl CLayout {
    pub fn new() -> Self {
        Self {
            fields: Vec::new(),
            end: 0,
            align: 1,
        }
    }

    pub fn size(&self) -> usize {
        round_up(self.end, self.align)
    }

    pub fn push(&mut self, name: Ustr, member: CMember) {
        let (size, align) = match &member {
            CMember::Scalar(ty) => c_prim_size_align(ty).expect("planned primitive type"),
            CMember::Aggregate(_, child) => (child.size(), child.align),
        };
        let offset = round_up(self.end, align);
        self.fields.push((name, offset, member));
        self.end = offset + size;
        self.align = self.align.max(align);
    }
}

impl Default for CLayout {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValueCLayout {
    pub size: usize,
    pub align: usize,
    pub fields: Vec<(Ustr, usize, CFieldType)>,
}

fn c_layout_rec(ws: &Workspace, name: Ustr, visiting: &mut Vec<Ustr>) -> Option<CLayout> {
    if let Some(ed) = enum_definition(ws, name)
        && let Some((slots, _)) = enum_cdata_slots(ws, ed)
    {
        let mut layout = CLayout::new();
        layout.push(Ustr::from("__tag"), CMember::Scalar("int"));
        for (slot, ty) in slots {
            layout.push(slot, CMember::Scalar(ty));
        }
        return Some(layout);
    }
    let sid = ws.nominals.struct_id(name)?;
    let sd = &ws.modules[sid.module.index()].structs[sid.def];
    let handle = is_registry_handle_decl(name.as_str());
    if !has_value_plan(sd) || (!sd.generics.is_empty() && !handle) || visiting.contains(&name) {
        return None;
    }
    visiting.push(name);
    let mut layout = CLayout::new();
    for field in &sd.fields {
        let ty = static_ty(ws, field.storage_ty)?;
        let member = match ty {
            TypeExpr::Builtin(BuiltinTy::Int) if handle => CMember::Scalar("double"),
            ref other if primitive_c_type(other).is_some() => {
                CMember::Scalar(primitive_c_type(other).unwrap())
            }
            TypeExpr::App {
                ctor: TypeCtor::Nominal(child),
                ..
            } if is_payload_free_enum(ws, child) => CMember::Scalar("int"),
            TypeExpr::App {
                ctor: TypeCtor::Nominal(child),
                ..
            } => {
                let child_layout = c_layout_rec(ws, child, visiting)?;
                CMember::Aggregate(child, child_layout)
            }
            _ => {
                visiting.pop();
                return None;
            }
        };
        layout.push(field.name, member);
    }
    visiting.pop();
    Some(layout)
}

/// Computes the C struct layout of a non-generic value struct, cdata enum, or registry handle.
pub fn value_c_layout(ws: &Workspace, name: Ustr) -> Option<ValueCLayout> {
    let layout = c_layout_rec(ws, name, &mut Vec::new())?;
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

/// Classifiers use this spelling; emitted identifiers use [`ident_of_key`].
pub fn declared_name(key: &str) -> &str {
    match last_path_separator(key) {
        Some(cut) => &key[cut + 2..],
        None => key,
    }
}

fn last_path_separator(key: &str) -> Option<usize> {
    let bytes = key.as_bytes();
    (1..bytes.len())
        .rev()
        .find(|&i| bytes[i] == b':' && bytes[i - 1] == b':')
        .map(|i| i - 1)
}

/// Hashes the prefix bytes with std's `DefaultHasher`: stable across processes
/// and compiler builds, unlike a `Ustr` hash, whose seed is per-build.
pub fn prefix_digest(prefix: &str) -> String {
    let mut h = DefaultHasher::new();
    prefix.hash(&mut h);
    format!("{:08x}", h.finish() as u32)
}

/// A qualified key becomes `<name>_<digest>`: the prefix is never flattened
/// textually, so module `a::b` and a module named `a_b` cannot converge.
pub fn ident_of_key(key: &str) -> Cow<'_, str> {
    let artifact = crate::modules::artifact_name(key);
    if artifact != key {
        return Cow::Borrowed(artifact);
    }
    match last_path_separator(key) {
        None => Cow::Borrowed(key),
        Some(cut) => Cow::Owned(format!(
            "{}{SEP}{}",
            &key[cut + 2..],
            prefix_digest(&key[..cut])
        )),
    }
}

/// Symbol spelling for a declared (non-monomorphized) nominal type.
/// A `@nomangle` struct keeps its declared name because inline Lua spells it.
pub fn declared_type_symbol(ws: &Workspace, key: Ustr) -> Ustr {
    let exempt = ws
        .nominals
        .struct_id(key)
        .is_some_and(|sid| ws.modules[sid.module.index()].structs[sid.def].nomangle);
    if exempt {
        Ustr::from(declared_name(key.as_str()))
    } else {
        match ident_of_key(key.as_str()) {
            Cow::Borrowed(ident) => Ustr::from(ident),
            Cow::Owned(owned) => Ustr::from(&owned),
        }
    }
}
