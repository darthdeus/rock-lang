use super::*;
use crate::monoty::{MonoSubst, convert_decl};
use bone_core::hir::VariantPayload;

/// An absent nullable payload prints Rock's spelling, not the backend's.
fn null_text() -> Lit {
    Lit::Str(crate::names::ustr("null"))
}

fn source_name(name: Ustr) -> String {
    bone_core::modules::display_name(crate::enum_instance::decl_name(name).as_str()).to_string()
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FmtNode {
    Leaf {
        route: StringifyRoute,
    },
    Nullable(Box<FmtNode>),
    Enum {
        name: Ustr,
        variants: Vec<FmtVariant>,
    },
    Record {
        name: Ustr,
        fields: Vec<(Ustr, FmtNode)>,
    },
    /// `Entity<T>(scene=.., ..)`: the handle's identity integers, unresolved.
    Handle {
        name: Ustr,
        label: String,
        fields: Vec<(&'static str, Ustr)>,
    },
    Newtype {
        name: Ustr,
        inner: Box<FmtNode>,
    },
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
    /// Keyed by the path lowering passes to `dispatched_call` for each call.
    pub fn obligations(&self, path: &mut Vec<u32>, out: &mut Vec<(Vec<u32>, &'static str)>) {
        match self {
            Self::Leaf { route } => {
                out.extend(route.synthesized_callee().map(|n| (path.clone(), n)))
            }
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

/// Enums, value records and handles format structurally; identity structs keep `inspect`.
pub(crate) fn formats_structurally(ws: &Workspace, name: Ustr) -> bool {
    if handle_fields(name).is_some()
        || ws
            .nominals
            .enum_id(crate::enum_instance::decl_name(name))
            .is_some()
    {
        return true;
    }
    super::shape::is_value_struct(ws, name)
}

fn handle_fields(name: Ustr) -> Option<&'static [(&'static str, &'static str)]> {
    use bone_core::modules::is_intrinsic_key;
    if is_intrinsic_key(name, "Entity") {
        Some(&[
            ("scene", "_scene_id"),
            ("arena", "_arena_id"),
            ("slot", "_slot"),
            ("generation", "_generation"),
        ])
    } else if is_intrinsic_key(name, "Arena") {
        Some(&[("scene", "_scene_id"), ("arena", "_arena_id")])
    } else if is_intrinsic_key(name, "Scene") {
        Some(&[("scene", "_scene_id")])
    } else {
        None
    }
}

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
            format!(
                "({})",
                parts.iter().map(type_label).collect::<Vec<_>>().join(", ")
            )
        }
        MonoTy::App {
            ctor: MonoCtor::Array,
            params,
        } if params.len() == 1 => {
            format!("[{}]", type_label(&params[0]))
        }
        MonoTy::App {
            ctor: MonoCtor::Nominal(n),
            params,
        } if params.is_empty() => source_name(*n),
        MonoTy::App {
            ctor: MonoCtor::Nominal(n),
            params,
        } => format!(
            "{}<{}>",
            source_name(*n),
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
        let TypeExpr::Generic(var, _) = crate::monoty::TyEnv::Decl.decl_node(ws, *generic).ok()?
        else {
            return None;
        };
        subst.insert(var, param.clone());
    }
    Some(subst)
}

/// Unsupported kinds are a formatter limit, reported at compile time.
pub(crate) fn format_tree(ws: &Workspace, ty: &MonoTy) -> Result<FmtNode, String> {
    format_tree_in(ws, ty, &mut Vec::new())
}

fn format_tree_in(
    ws: &Workspace,
    ty: &MonoTy,
    visiting: &mut Vec<Ustr>,
) -> Result<FmtNode, String> {
    Ok(match ty {
        MonoTy::Int => FmtNode::Leaf {
            route: StringifyRoute::IntText,
        },
        MonoTy::Uint | MonoTy::Bool => FmtNode::Leaf {
            route: StringifyRoute::ToString,
        },
        MonoTy::Float => FmtNode::Leaf {
            route: StringifyRoute::FormatFloat,
        },
        MonoTy::String => FmtNode::Leaf {
            route: StringifyRoute::EscapedString,
        },
        MonoTy::Nullable(inner) => {
            FmtNode::Nullable(Box::new(format_tree_in(ws, inner, visiting)?))
        }
        MonoTy::App {
            ctor: MonoCtor::Nominal(name),
            params,
        } => {
            let name = *name;
            if let Some(fields) = handle_fields(name) {
                let label = type_label(ty);
                return Ok(FmtNode::Handle {
                    name,
                    label,
                    fields: fields
                        .iter()
                        .map(|(l, f)| (*l, crate::names::ustr(f)))
                        .collect(),
                });
            }
            if visiting.contains(&name) {
                return Err(format!("`{}` is recursive", source_name(name)));
            }
            visiting.push(name);
            let node = nominal_tree(ws, name, params, visiting);
            visiting.pop();
            node?
        }
        MonoTy::App {
            ctor: MonoCtor::Array | MonoCtor::Map,
            ..
        }
        | MonoTy::Tuple(_) => {
            return Err(format!("`{}` is a container", type_label(ty)));
        }
        MonoTy::App {
            ctor: MonoCtor::Ref,
            ..
        } => {
            return Err(format!("`{}` is a raw reference", type_label(ty)));
        }
        MonoTy::Function { .. } => return Err(format!("`{}` is a callback", type_label(ty))),
        other => {
            return Err(format!(
                "`{}` has no structural rendering",
                type_label(other)
            ));
        }
    })
}

fn nominal_tree(
    ws: &Workspace,
    name: Ustr,
    params: &[MonoTy],
    visiting: &mut Vec<Ustr>,
) -> Result<FmtNode, String> {
    let field_tree = |ty: TyId, subst: &MonoSubst, visiting: &mut Vec<Ustr>| {
        let ty = convert_decl(ws, ty, subst)
            .map_err(|r| format!("a field of `{}` has no type ({r:?})", source_name(name)))?;
        format_tree_in(ws, &ty, visiting)
    };
    if let Some(nid) = ws.nominals.newtype_id(name) {
        let def = &ws.module(nid.module).newtypes[nid.def];
        let subst = nominal_subst(ws, &def.generics, params)
            .ok_or_else(|| format!("`{}` arity", source_name(name)))?;
        return Ok(FmtNode::Newtype {
            name,
            inner: Box::new(field_tree(def.wrapped, &subst, visiting)?),
        });
    }
    if let Some(eid) = ws.nominals.enum_id(crate::enum_instance::decl_name(name)) {
        if is_payload_free_enum(ws, name) {
            return Ok(FmtNode::Leaf {
                route: StringifyRoute::EnumNames(name),
            });
        }
        let def = &ws.module(eid.module).enums[eid.def];
        let subst = nominal_subst(ws, &def.generics, params)
            .ok_or_else(|| format!("`{}` arity", source_name(name)))?;
        let mut variants = Vec::with_capacity(def.variants.len());
        for v in &def.variants {
            let payload = match &v.payload {
                VariantPayload::Unit => FmtPayload::Unit,
                VariantPayload::Tuple(tys) => FmtPayload::Tuple(
                    tys.iter()
                        .map(|ty| field_tree(*ty, &subst, visiting))
                        .collect::<Result<_, _>>()?,
                ),
                VariantPayload::Record(fields) => FmtPayload::Record(
                    fields
                        .iter()
                        .map(|f| Ok((f.name, field_tree(f.ty, &subst, visiting)?)))
                        .collect::<Result<_, String>>()?,
                ),
            };
            variants.push(FmtVariant {
                name: v.name,
                payload,
            });
        }
        return Ok(FmtNode::Enum { name, variants });
    }
    if let Some(sid) = ws.nominals.struct_id(name) {
        let def = &ws.module(sid.module).structs[sid.def];
        if !def.is_value {
            return Err(format!("`{}` is an identity struct", source_name(name)));
        }
        let subst = nominal_subst(ws, &def.generics, params)
            .ok_or_else(|| format!("`{}` arity", source_name(name)))?;
        let fields = def
            .fields
            .iter()
            .map(|f| Ok((f.name, field_tree(f.ty, &subst, visiting)?)))
            .collect::<Result<_, String>>()?;
        return Ok(FmtNode::Record { name, fields });
    }
    Err(format!(
        "`{}` has no structural rendering",
        source_name(name)
    ))
}

impl<'a> Builder<'a> {
    pub(crate) fn render_fmt(
        &mut self,
        node: &FmtNode,
        path: Vec<u32>,
        repr: ValueRepr,
    ) -> Result<Idx<Inst>, CompilerError> {
        match node {
            FmtNode::Leaf { route } => {
                let v = self.pack(repr)?;
                self.render_stringify(route, path, v)
            }
            FmtNode::Nullable(inner) => match repr {
                ValueRepr::Group {
                    name,
                    parts,
                    present: Some(present),
                } => {
                    let group = ValueRepr::Group {
                        name,
                        parts,
                        present: None,
                    };
                    self.absent_or_join(None, present, true, null_text(), |s| {
                        s.render_fmt(inner, path, group)
                    })
                }
                ValueRepr::Scalar(v) => {
                    let is_nil = self.is_nil(v);
                    self.absent_or_join(None, is_nil, false, null_text(), |s| {
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
                let display_name = source_name(*name);
                let parts = self.record_parts(*name, fields.iter().map(|(f, _)| *f), repr)?;
                let mut pieces = Vec::with_capacity(fields.len());
                for (i, ((field, child), part)) in fields.iter().zip(parts).enumerate() {
                    let mut child_path = path.clone();
                    child_path.push(i as u32);
                    let rendered = self.render_fmt(child, child_path, part)?;
                    pieces.push((format!("{field}: "), rendered));
                }
                if pieces.is_empty() {
                    return Ok(self.emit_cur(Inst::Const(Lit::Str(crate::names::ustr(
                        format!("{display_name} {{}}").as_str(),
                    )))));
                }
                Ok(self.join_pieces(&format!("{display_name} {{ "), pieces, " }"))
            }
            FmtNode::Handle {
                name,
                label,
                fields,
            } => {
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
                let display_name = source_name(*name);
                let mut child_path = path;
                child_path.push(0);
                let rendered = self.render_fmt(inner, child_path, repr)?;
                Ok(self.join_pieces(
                    &format!("{display_name}("),
                    vec![(String::new(), rendered)],
                    ")",
                ))
            }
        }
    }

    fn join_pieces(
        &mut self,
        open: &str,
        pieces: Vec<(String, Idx<Inst>)>,
        close: &str,
    ) -> Idx<Inst> {
        let mut out = self.emit_cur(Inst::Const(Lit::Str(crate::names::ustr(open))));
        for (i, (label, piece)) in pieces.into_iter().enumerate() {
            let sep = if i > 0 { format!(", {label}") } else { label };
            if !sep.is_empty() {
                let s = self.emit_cur(Inst::Const(Lit::Str(crate::names::ustr(sep.as_str()))));
                out = self.emit_cur(Inst::StringConcat(out, s));
            }
            out = self.emit_cur(Inst::StringConcat(out, piece));
        }
        let c = self.emit_cur(Inst::Const(Lit::Str(crate::names::ustr(close))));
        self.emit_cur(Inst::StringConcat(out, c))
    }

    /// A scalarized group holds the fields in declaration order; a materialized
    /// value (cdata or table) is read by field name.
    fn record_parts(
        &mut self,
        name: Ustr,
        wanted: impl Iterator<Item = Ustr>,
        repr: ValueRepr,
    ) -> Result<Vec<ValueRepr>, CompilerError> {
        let sid = self.ws.nominals.struct_id(name).ok_or_else(|| {
            gap(format!(
                "formatted record `{}` has no definition",
                source_name(name)
            ))
        })?;
        let declared: Vec<Ustr> = self.ws.module(sid.module).structs[sid.def]
            .fields
            .iter()
            .map(|f| f.name)
            .collect();
        let repr = match repr {
            ValueRepr::Scalar(v)
                if super::shape::planned_value(self.ws, name)
                    .is_some_and(|plan| plan.storage == ValueStorage::LeafTable) =>
            {
                let shape = self.nominal_rep_shape_required(name, false)?;
                let leaves = self.cdata_leaves(v, &shape)?;
                Self::rebuild_from_leaves(&shape, &leaves)
            }
            repr => repr,
        };
        match repr {
            ValueRepr::Group {
                parts,
                present: None,
                ..
            } => {
                if parts.len() != declared.len() {
                    return Err(gap(format!(
                        "formatted record `{}` has a mismatched group",
                        source_name(name)
                    )));
                }
                wanted
                    .map(|w| {
                        declared
                            .iter()
                            .position(|d| *d == w)
                            .map(|i| parts[i].clone())
                            .ok_or_else(|| {
                                gap(format!("`{}` has no field `{w}`", source_name(name)))
                            })
                    })
                    .collect()
            }
            other => {
                let v = self.pack(other)?;
                Ok(wanted
                    .map(|w| ValueRepr::Scalar(self.read_field(v, w)))
                    .collect())
            }
        }
    }

    /// Payload reads follow `match_pattern`'s, so every layout agrees.
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
        let plan = EnumPlan::for_name(self.ws, name).ok_or_else(|| {
            gap(format!(
                "formatted enum `{}` has no plan",
                source_name(name)
            ))
        })?;
        let cdata = plan.cdata().is_some();
        let flat_shape = plan.flat();
        // A value constructed in place has a constant tag. Only its arm is reachable;
        // the others would read slots holding nil or another variant's payload.
        let known = match scrut {
            Scrut::Cells { cells, .. } => match self.f.insts[cells.tag()] {
                Inst::Const(Lit::Str(variant)) => Some(variant),
                _ => None,
            },
            _ => None,
        };
        let result = self.f.new_cell(None);
        let join = self.f.new_block();
        for (vi, variant) in variants.iter().enumerate() {
            if known.is_some_and(|k| k != variant.name) {
                continue;
            }
            let fail = self.f.new_block();
            self.test_tag(scrut, variant.name, Some(name), fail);
            let head = format!("{}::{}", source_name(name), variant.name);
            let rendered = match &variant.payload {
                FmtPayload::Unit => {
                    self.emit_cur(Inst::Const(Lit::Str(crate::names::ustr(head.as_str()))))
                }
                FmtPayload::Tuple(nodes) => {
                    let mut pieces = Vec::with_capacity(nodes.len());
                    for (i, child) in nodes.iter().enumerate() {
                        let child_path = [path.as_slice(), &[vi as u32, i as u32]].concat();
                        let slot = flat_shape.as_ref().map_or(i, |s| s.slot_offset(i));
                        let piece = match flat_shape
                            .and_then(|shape| shape.payload_shape(self.ws, variant.name, i))
                        {
                            Some(vs) => {
                                if let Some(inner) = self.nested_flat_enum_scrut(scrut, slot, vs)? {
                                    self.render_enum(child, vs.name, child_path, &inner)?
                                } else {
                                    let leaves = self.scrut_payload_leaves(scrut, slot, vs);
                                    let repr = Self::rebuild_from_leaves(vs, &leaves);
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
                        let payload = if let Some(payload) =
                            flat_shape.and_then(|flat| flat.named_payload(variant.name, *field))
                        {
                            let leaves = self.scrut_named_payload_leaves(scrut, &payload.keys)?;
                            Self::rebuild_from_leaves(&payload.shape, &leaves)
                        } else {
                            ValueRepr::Scalar(self.scrut_named(scrut, *field, cdata)?)
                        };
                        let piece = self.render_fmt(child, child_path, payload)?;
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
