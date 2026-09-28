//! Structural value formatting: the one diagnostic text for enums, value
//! records and handles (docs/c-backend-memory-ownership.md "Enum formatting").
//!
//! [`format_tree`] decides the text from the logical type alone, so a flat,
//! cdata, boxed or leaf-widened enum renders identically. Lowering walks the
//! tree against whatever representation the value arrived in; the instance
//! walk reads the same tree's [`FmtNode::obligations`].
use super::*;
use crate::monoty::{convert_decl, MonoSubst};
use bone_semantic::hir::VariantPayload;

/// How an absent nullable payload prints: Rock's spelling, not the backend's.
const NULL_TEXT: &str = "null";

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FmtNode {
    /// A scalar, a string or a payload-free enum.
    Leaf(StringifyRoute),
    Nullable(Box<FmtNode>),
    Enum { name: Ustr, variants: Vec<FmtVariant> },
    /// `Name { field: <..>, .. }`.
    Record { name: Ustr, fields: Vec<(Ustr, FmtNode)> },
    /// `Entity<T>(scene=.., ..)`: the handle's identity integers, unresolved.
    Handle { name: Ustr, label: String, fields: Vec<(&'static str, Ustr)> },
    /// `Name(<inner>)` for a transparent newtype.
    Newtype { name: Ustr, inner: Box<FmtNode> },
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FmtVariant {
    pub name: Ustr,
    pub payload: FmtPayload,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FmtPayload {
    Unit,
    Tuple(Vec<FmtNode>),
    Record(Vec<(Ustr, FmtNode)>),
}

impl FmtNode {
    /// Every stdlib call a render of this tree synthesizes, keyed by the path
    /// lowering passes to `dispatched_call` for it.
    pub fn obligations(&self, path: &mut Vec<u32>, out: &mut Vec<(Vec<u32>, &'static str)>) {
        match self {
            Self::Leaf(route) => out.extend(route.synthesized_callee().map(|n| (path.clone(), n))),
            Self::Nullable(inner) => inner.obligations(path, out),
            Self::Enum { variants, .. } => {
                for (vi, v) in variants.iter().enumerate() {
                    path.push(vi as u32);
                    let nodes: Vec<&FmtNode> = match &v.payload {
                        FmtPayload::Unit => Vec::new(),
                        FmtPayload::Tuple(nodes) => nodes.iter().collect(),
                        FmtPayload::Record(fields) => fields.iter().map(|(_, n)| n).collect(),
                    };
                    for (i, node) in nodes.into_iter().enumerate() {
                        path.push(i as u32);
                        node.obligations(path, out);
                        path.pop();
                    }
                    path.pop();
                }
            }
            Self::Record { fields, .. } => {
                for (i, (_, node)) in fields.iter().enumerate() {
                    path.push(i as u32);
                    node.obligations(path, out);
                    path.pop();
                }
            }
            Self::Handle { .. } => {}
            Self::Newtype { inner, .. } => {
                path.push(0);
                inner.obligations(path, out);
                path.pop();
            }
        }
    }
}

/// Whether `name` is a type the structural formatter owns at the top level of
/// a hole: an enum, a value record or a handle. Identity structs keep `inspect`.
pub fn formats_structurally(ws: &Workspace, name: Ustr) -> bool {
    if handle_fields(name).is_some() || ws.enums_by_name.contains_key(&name) {
        return true;
    }
    ws.struct_by_name
        .get(&name)
        .is_some_and(|sid| ws.module(sid.module).structs[sid.def].is_value)
}

fn handle_fields(name: Ustr) -> Option<&'static [(&'static str, &'static str)]> {
    use bone_semantic::prelude::is_intrinsic_key;
    if is_intrinsic_key(name, "Entity") {
        Some(&[("scene", "_scene_id"), ("arena", "_arena_id"), ("slot", "_slot"), ("generation", "_generation")])
    } else if is_intrinsic_key(name, "Arena") {
        Some(&[("scene", "_scene_id"), ("arena", "_arena_id")])
    } else if is_intrinsic_key(name, "Scene") {
        Some(&[("scene", "_scene_id")])
    } else {
        None
    }
}

/// A nominal's name as its source spells it. A v1 package type's name is its
/// DefKey path (`pkg::module::Name`); the text uses the declared name.
pub fn display_name(name: Ustr) -> &'static str {
    let s: &'static str = name.as_str();
    s.rsplit("::").next().unwrap_or(s)
}

/// The source spelling of a type, for a handle's type argument.
fn type_label(ty: &MonoTy) -> String {
    match ty {
        MonoTy::Int => "int".into(),
        MonoTy::Uint => "uint".into(),
        MonoTy::Float => "float".into(),
        MonoTy::Bool => "bool".into(),
        MonoTy::String => "string".into(),
        MonoTy::Any => "any".into(),
        MonoTy::Unit => "unit".into(),
        MonoTy::Never => "never".into(),
        MonoTy::Nullable(inner) => format!("{}?", type_label(inner)),
        MonoTy::Tuple(parts) => {
            format!("({})", parts.iter().map(type_label).collect::<Vec<_>>().join(", "))
        }
        MonoTy::App { ctor: MonoCtor::Array, params } if params.len() == 1 => {
            format!("[{}]", type_label(&params[0]))
        }
        MonoTy::App { ctor: MonoCtor::Nominal(n), params } if params.is_empty() => {
            display_name(*n).to_string()
        }
        MonoTy::App { ctor: MonoCtor::Nominal(n), params } => format!(
            "{}<{}>",
            display_name(*n),
            params.iter().map(type_label).collect::<Vec<_>>().join(", ")
        ),
        other => other.render(),
    }
}

fn nominal_subst(ws: &Workspace, generics: &[TyId], params: &[MonoTy]) -> Option<MonoSubst> {
    if generics.len() != params.len() {
        return None;
    }
    let mut subst = MonoSubst::default();
    for (generic, param) in generics.iter().zip(params) {
        let TypeExpr::Generic(var, _) = crate::monoty::TyEnv::Decl.decl_node(ws, *generic).ok()? else {
            return None;
        };
        subst.insert(var, param.clone());
    }
    Some(subst)
}

/// The format tree for a value of type `ty`, or why the formatter cannot print
/// it. Unsupported kinds are a formatter limit (spec), reported at compile time.
pub fn format_tree(ws: &Workspace, ty: &MonoTy) -> Result<FmtNode, String> {
    format_tree_in(ws, ty, &mut Vec::new())
}

fn format_tree_in(ws: &Workspace, ty: &MonoTy, visiting: &mut Vec<Ustr>) -> Result<FmtNode, String> {
    Ok(match ty {
        MonoTy::Int | MonoTy::Uint | MonoTy::Bool => FmtNode::Leaf(StringifyRoute::ToString),
        MonoTy::Float => FmtNode::Leaf(StringifyRoute::FormatFloat),
        MonoTy::String => FmtNode::Leaf(StringifyRoute::EscapedString),
        MonoTy::Nullable(inner) => FmtNode::Nullable(Box::new(format_tree_in(ws, inner, visiting)?)),
        MonoTy::App { ctor: MonoCtor::Nominal(name), params } => {
            let name = *name;
            if let Some(fields) = handle_fields(name) {
                let label = type_label(ty);
                return Ok(FmtNode::Handle {
                    name,
                    label,
                    fields: fields.iter().map(|(l, f)| (*l, Ustr::from(f))).collect(),
                });
            }
            if visiting.contains(&name) {
                return Err(format!("`{name}` is recursive"));
            }
            visiting.push(name);
            let node = nominal_tree(ws, name, params, visiting);
            visiting.pop();
            node?
        }
        MonoTy::App { ctor: MonoCtor::Array | MonoCtor::Map, .. } | MonoTy::Tuple(_) => {
            return Err(format!("`{}` is a container", type_label(ty)));
        }
        MonoTy::App { ctor: MonoCtor::Ref, .. } => {
            return Err(format!("`{}` is a raw reference", type_label(ty)));
        }
        MonoTy::Function { .. } => return Err(format!("`{}` is a callback", type_label(ty))),
        other => return Err(format!("`{}` has no structural rendering", type_label(other))),
    })
}

fn nominal_tree(
    ws: &Workspace,
    name: Ustr,
    params: &[MonoTy],
    visiting: &mut Vec<Ustr>,
) -> Result<FmtNode, String> {
    let field_tree = |ty: TyId, subst: &MonoSubst, visiting: &mut Vec<Ustr>| {
        let ty = convert_decl(ws, ty, subst).map_err(|r| format!("a field of `{name}` has no type ({r:?})"))?;
        format_tree_in(ws, &ty, visiting)
    };
    if let Some(nid) = ws.newtypes_by_name.get(&name) {
        let def = &ws.module(nid.module).newtypes[nid.def];
        let subst = nominal_subst(ws, &def.generics, params).ok_or_else(|| format!("`{name}` arity"))?;
        return Ok(FmtNode::Newtype { name, inner: Box::new(field_tree(def.wrapped, &subst, visiting)?) });
    }
    if let Some(eid) = ws.enums_by_name.get(&name) {
        if payload_free_enum_variants(ws, name).is_some() {
            return Ok(FmtNode::Leaf(StringifyRoute::EnumNames(name)));
        }
        let def = &ws.module(eid.module).enums[eid.def];
        let subst = nominal_subst(ws, &def.generics, params).ok_or_else(|| format!("`{name}` arity"))?;
        let mut variants = Vec::with_capacity(def.variants.len());
        for v in &def.variants {
            let payload = match &v.payload {
                VariantPayload::Unit => FmtPayload::Unit,
                VariantPayload::Tuple(tys) => FmtPayload::Tuple(
                    tys.iter().map(|ty| field_tree(*ty, &subst, visiting)).collect::<Result<_, _>>()?,
                ),
                VariantPayload::Record(fields) => FmtPayload::Record(
                    fields
                        .iter()
                        .map(|f| Ok((f.name, field_tree(f.ty, &subst, visiting)?)))
                        .collect::<Result<_, String>>()?,
                ),
            };
            variants.push(FmtVariant { name: v.name, payload });
        }
        return Ok(FmtNode::Enum { name, variants });
    }
    if let Some(sid) = ws.struct_by_name.get(&name) {
        let def = &ws.module(sid.module).structs[sid.def];
        if !def.is_value {
            return Err(format!("`{name}` is an identity struct"));
        }
        let subst = nominal_subst(ws, &def.generics, params).ok_or_else(|| format!("`{name}` arity"))?;
        let fields = def
            .fields
            .iter()
            .map(|f| Ok((f.name, field_tree(f.ty, &subst, visiting)?)))
            .collect::<Result<_, String>>()?;
        return Ok(FmtNode::Record { name, fields });
    }
    Err(format!("`{name}` has no structural rendering"))
}

impl Builder<'_> {
    /// Render `repr`, a value whose format tree is `node`.
    pub(crate) fn render_fmt(
        &mut self,
        node: &FmtNode,
        path: Vec<u32>,
        repr: ValueRepr,
    ) -> Result<Idx<Inst>, CompilerError> {
        match node {
            FmtNode::Leaf(route) => {
                let v = self.pack(repr)?;
                self.render_stringify(route, path, v)
            }
            FmtNode::Nullable(inner) => match repr {
                ValueRepr::Group { name, parts, present: Some(present) } => {
                    let group = ValueRepr::Group { name, parts, present: None };
                    self.render_present_or_null(present, true, NULL_TEXT, |s| s.render_fmt(inner, path, group))
                }
                ValueRepr::Scalar(v) => {
                    let nil = self.emit_cur(Inst::Const(Lit::Nil));
                    let is_nil = self.emit_cur(Inst::Eq(v, nil));
                    self.render_present_or_null(is_nil, false, NULL_TEXT, |s| {
                        s.render_fmt(inner, path, ValueRepr::Scalar(v))
                    })
                }
                other => self.render_fmt(inner, path, other),
            },
            FmtNode::Enum { name, .. } => {
                let scrut = self.scrut_from_repr(repr)?;
                self.render_enum(node, *name, path, &scrut)
            }
            FmtNode::Record { name, fields } => {
                let parts = self.record_parts(*name, fields.iter().map(|(f, _)| *f), repr)?;
                let mut pieces = Vec::with_capacity(fields.len());
                for (i, ((field, child), part)) in fields.iter().zip(parts).enumerate() {
                    let mut child_path = path.clone();
                    child_path.push(i as u32);
                    let rendered = self.render_fmt(child, child_path, part)?;
                    pieces.push((format!("{field}: "), rendered));
                }
                let name = display_name(*name);
                if pieces.is_empty() {
                    return Ok(self.emit_cur(Inst::Const(Lit::Str(Ustr::from(format!("{name} {{}}").as_str())))));
                }
                Ok(self.join_pieces(&format!("{name} {{ "), pieces, " }"))
            }
            FmtNode::Handle { name, label, fields } => {
                let parts = self.record_parts(*name, fields.iter().map(|(_, f)| *f), repr)?;
                let mut pieces = Vec::with_capacity(fields.len());
                for ((l, _), part) in fields.iter().zip(parts) {
                    let v = self.pack(part)?;
                    let rendered = self.emit_cur(Inst::ToString(v));
                    pieces.push((format!("{l}="), rendered));
                }
                Ok(self.join_pieces(&format!("{label}("), pieces, ")"))
            }
            FmtNode::Newtype { name, inner } => {
                let mut child_path = path;
                child_path.push(0);
                let rendered = self.render_fmt(inner, child_path, repr)?;
                Ok(self.join_pieces(&format!("{}(", display_name(*name)), vec![(String::new(), rendered)], ")"))
            }
        }
    }

    /// `open` + `label_i .. piece_i` joined by ", " + `close`, one flat concat run.
    fn join_pieces(&mut self, open: &str, pieces: Vec<(String, Idx<Inst>)>, close: &str) -> Idx<Inst> {
        let mut out = self.emit_cur(Inst::Const(Lit::Str(Ustr::from(open))));
        for (i, (label, piece)) in pieces.into_iter().enumerate() {
            let sep = if i > 0 { format!(", {label}") } else { label };
            if !sep.is_empty() {
                let s = self.emit_cur(Inst::Const(Lit::Str(Ustr::from(sep.as_str()))));
                out = self.emit_cur(Inst::StringConcat(out, s));
            }
            out = self.emit_cur(Inst::StringConcat(out, piece));
        }
        let c = self.emit_cur(Inst::Const(Lit::Str(Ustr::from(close))));
        self.emit_cur(Inst::StringConcat(out, c))
    }

    /// `cond` selects the rendered value (`cond_is_present` true) or `null_text`.
    pub(crate) fn render_present_or_null(
        &mut self,
        cond: Idx<Inst>,
        cond_is_present: bool,
        null_text: &str,
        render: impl FnOnce(&mut Self) -> Result<Idx<Inst>, CompilerError>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let result = self.f.new_cell(None);
        let value_bb = self.f.new_block();
        let null_bb = self.f.new_block();
        let join = self.f.new_block();
        let (then_bb, else_bb) = if cond_is_present { (value_bb, null_bb) } else { (null_bb, value_bb) };
        self.f.set_term(self.cur, Term::Branch { cond, then_bb, else_bb });

        self.cur = null_bb;
        let null = self.emit_cur(Inst::Const(Lit::Str(Ustr::from(null_text))));
        self.store_and_join(result, null, join);

        self.cur = value_bb;
        let rendered = render(self)?;
        self.store_and_join(result, rendered, join);

        self.cur = join;
        Ok(self.emit_cur(Inst::Load(result)))
    }

    /// The declared fields `wanted` of a value record or handle, each as its own
    /// representation: a scalarized group already holds them in declaration
    /// order; a materialized value (cdata or table) is read by field name.
    fn record_parts(
        &mut self,
        name: Ustr,
        wanted: impl Iterator<Item = Ustr>,
        repr: ValueRepr,
    ) -> Result<Vec<ValueRepr>, CompilerError> {
        let sid = *self
            .ws
            .struct_by_name
            .get(&name)
            .ok_or_else(|| gap(format!("formatted record `{name}` has no definition")))?;
        let declared: Vec<Ustr> =
            self.ws.module(sid.module).structs[sid.def].fields.iter().map(|f| f.name).collect();
        match repr {
            ValueRepr::Group { parts, present: None, .. } => {
                if parts.len() != declared.len() {
                    return Err(gap(format!("formatted record `{name}` has a mismatched group")));
                }
                wanted
                    .map(|w| {
                        declared
                            .iter()
                            .position(|d| *d == w)
                            .map(|i| parts[i].clone())
                            .ok_or_else(|| gap(format!("`{name}` has no field `{w}`")))
                    })
                    .collect()
            }
            other => {
                let v = self.pack(other)?;
                Ok(wanted
                    .map(|w| ValueRepr::Scalar(self.emit_cur(Inst::Field { base: v, field: w })))
                    .collect())
            }
        }
    }

    /// One tag test per variant, each arm rendering its payload in declaration
    /// order. Payload reads follow `match_pattern`'s, so every layout agrees.
    pub(crate) fn render_enum(
        &mut self,
        node: &FmtNode,
        name: Ustr,
        path: Vec<u32>,
        scrut: &Scrut,
    ) -> Result<Idx<Inst>, CompilerError> {
        let FmtNode::Enum { variants, .. } = node else {
            return Err(gap("render_enum on a non-enum format node"));
        };
        let plan = EnumPlan::for_name(self.ws, name)
            .ok_or_else(|| gap(format!("formatted enum `{name}` has no plan")))?;
        let cdata = plan.cdata().is_some();
        let flat_shape = plan.flat().cloned();
        let result = self.f.new_cell(None);
        let join = self.f.new_block();
        for (vi, variant) in variants.iter().enumerate() {
            let fail = self.f.new_block();
            self.test_tag(scrut, variant.name, Some(name), fail);
            let head = format!("{}::{}", display_name(name), variant.name);
            let rendered = match &variant.payload {
                FmtPayload::Unit => self.emit_cur(Inst::Const(Lit::Str(Ustr::from(head.as_str())))),
                FmtPayload::Tuple(nodes) => {
                    let mut pieces = Vec::with_capacity(nodes.len());
                    for (i, child) in nodes.iter().enumerate() {
                        let child_path = [path.as_slice(), &[vi as u32, i as u32]].concat();
                        let slot = flat_shape.as_ref().map_or(i, |s| s.slot_offset(i));
                        let piece = match self.flat_payload_leaf_shape(Some(name), variant.name, i) {
                            Some(vs) => {
                                let shape = flat_shape
                                    .as_ref()
                                    .ok_or_else(|| gap("flat enum payload lost its compiler-owned shape"))?;
                                if let Some(inner) = self.nested_flat_enum_scrut(scrut, slot, &vs)? {
                                    self.render_enum(child, vs.name, child_path, &inner)?
                                } else {
                                    let leaves = self.scrut_payload_leaves(scrut, i, &vs, shape);
                                    let repr = leaves_to_repr(&vs, &mut leaves.into_iter())?;
                                    self.render_fmt(child, child_path, repr)?
                                }
                            }
                            None => {
                                let payload = self.scrut_payload(scrut, slot, cdata);
                                self.render_fmt(child, child_path, ValueRepr::Scalar(payload))?
                            }
                        };
                        pieces.push((String::new(), piece));
                    }
                    self.join_pieces(&format!("{head}("), pieces, ")")
                }
                FmtPayload::Record(fields) => {
                    let mut pieces = Vec::with_capacity(fields.len());
                    for (i, (field, child)) in fields.iter().enumerate() {
                        let child_path = [path.as_slice(), &[vi as u32, i as u32]].concat();
                        let payload = self.scrut_named(scrut, *field, cdata);
                        let piece = self.render_fmt(child, child_path, ValueRepr::Scalar(payload))?;
                        pieces.push((format!("{field}: "), piece));
                    }
                    self.join_pieces(&format!("{head} {{ "), pieces, " }")
                }
            };
            self.store_and_join(result, rendered, join);
            self.cur = fail;
        }
        // Every tag is covered above; a value reaching here is corrupt.
        self.f.set_term(self.cur, Term::Unreachable);
        self.cur = join;
        Ok(self.emit_cur(Inst::Load(result)))
    }
}

/// Rebuild a widened payload position's leaves as the nested group its value
/// shape describes.
fn leaves_to_repr(
    shape: &ValueShape,
    leaves: &mut impl Iterator<Item = Idx<Inst>>,
) -> Result<ValueRepr, CompilerError> {
    if shape.nullable {
        return Err(gap(format!("nullable widened payload `{}` cannot be formatted", shape.name)));
    }
    let mut parts = Vec::with_capacity(shape.fields.len());
    for (_, field) in &shape.fields {
        parts.push(match field {
            FieldShape::Prim => ValueRepr::Scalar(
                leaves.next().ok_or_else(|| gap(format!("`{}` ran out of leaves", shape.name)))?,
            ),
            FieldShape::Nested(inner) => leaves_to_repr(inner, leaves)?,
        });
    }
    Ok(ValueRepr::Group { name: shape.name, parts, present: None })
}
