use super::*;

impl<'a> Builder<'a> {
    pub(crate) fn lower_interp(
        &mut self,
        parts: IdxRange<bone_core::hir::InterpPart>,
    ) -> Result<Idx<Inst>, CompilerError> {
        use bone_core::hir::InterpPart;
        let mut acc: Option<Idx<Inst>> = None;
        for p in parts {
            let s = match self.body.interp_part_arena[p] {
                InterpPart::Literal(lit) => self.emit_cur(Inst::Const(Lit::Str(lit))),
                InterpPart::Value { expr, spec } => {
                    // Calls a hole synthesizes belong to the hole, not the enclosing `Interp`, so
                    // two holes in one literal are distinct sites.
                    self.with_site_expr(expr, |s| s.lower_interp_hole(expr, spec))?
                }
            };
            acc = Some(match acc {
                None => s,
                Some(lhs) => self.emit_cur(Inst::StringConcat(lhs, s)),
            });
        }
        Ok(acc.unwrap_or_else(|| self.emit_cur(Inst::Const(Lit::Str(crate::names::ustr(""))))))
    }

    fn lower_interp_hole(
        &mut self,
        expr: Idx<Expr>,
        spec: Option<bone_core::hir::InterpSpec>,
    ) -> Result<Idx<Inst>, CompilerError> {
        match spec {
            None if self.dispatch().structural_route(expr).is_some() => {
                self.stringify_structural(expr)
            }
            None if self.is_explicit_materialize_call(expr) => {
                self.boundary_materializes.insert(expr);
                let v = self.lower_expr(expr)?;
                self.stringify(expr, v)
            }
            // A boxed tuple is rebuilt into its parts first so it packs narrow and reaches
            // the same rejection as an unpacked one.
            None if let Some(shape) = self.tuple_box_shape(expr)? => {
                let r = match self.lower_expr_repr(expr)?.into_value() {
                    boxed @ ValueRepr::Scalar(_) => {
                        let slots = self.tuple_repr_to_slots(boxed, &shape)?;
                        self.tuple_from_slots(&shape, &slots, &mut 0)
                    }
                    parts => parts,
                };
                let v = self.pack(r)?;
                self.stringify(expr, v)
            }
            None => {
                let v = self.lower_expr(expr)?;
                self.stringify(expr, v)
            }
            Some(spec) => {
                let v = self.lower_expr(expr)?;
                self.lower_format(expr, v, &spec)
            }
        }
    }

    /// Renders from the value's own representation, so a scalarized or flat value
    /// is never packed just to be printed.
    pub(crate) fn stringify_structural(
        &mut self,
        expr: Idx<Expr>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let route = self
            .dispatch()
            .structural_route(expr)
            .ok_or_else(|| self.gap_here("structural hole lost its route"))?;
        let repr = self.lower_expr_repr(expr)?.into_value();
        match repr {
            ValueRepr::Scalar(v) if self.expr_is_nullable(expr) => {
                self.nil_guarded_render(v, |s| s.render_stringify(&route, Vec::new(), v))
            }
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
                self.render_structural_route(&route, group, Some(present))
            }
            // A flat enum has no presence leaf: a nil tag is its null.
            ValueRepr::Group {
                name,
                parts,
                present: None,
            } if self.expr_is_nullable(expr)
                && EnumPlan::for_name(self.ws, name).is_some_and(|plan| plan.flat().is_some()) =>
            {
                let Some(&ValueRepr::Scalar(tag)) = parts.first() else {
                    return Err(self.gap_here("nullable flat enum hole lost its tag"));
                };
                let group = ValueRepr::Group {
                    name,
                    parts,
                    present: None,
                };
                self.nil_guarded_render(tag, |s| s.render_structural_route(&route, group, None))
            }
            repr => self.render_structural_route(&route, repr, None),
        }
    }

    fn render_structural_route(
        &mut self,
        route: &StringifyRoute,
        repr: ValueRepr,
        present: Option<Idx<Inst>>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let tree = match route {
            StringifyRoute::Structural(tree) => tree.as_ref().clone(),
            StringifyRoute::Unprintable(reason) => {
                return Err(self.unprintable_error(reason));
            }
            _ => return Err(self.gap_here("structural hole with a non-structural route")),
        };
        let Some(present) = present else {
            return self.render_fmt(&tree, Vec::new(), repr);
        };
        self.render_present_or_null(present, true, "nil", |s| {
            s.render_fmt(&tree, Vec::new(), repr)
        })
    }

    pub(crate) fn unprintable_error(&self, reason: &str) -> CompilerError {
        let msg = format!(
            "cannot format this value: {reason}; the structural formatter prints scalars, \
             strings, handles, enums and value records composed of those"
        );
        self.error_at_hole(msg)
    }

    /// `cur_expr` is the hole while it lowers (`with_site_expr`).
    fn error_at_hole(&self, msg: String) -> CompilerError {
        match self.cur_expr {
            Some(e) => CompilerError::new_with_label(self.body.expr_spans[e], msg),
            None => CompilerError::new_no_location(msg),
        }
    }

    pub(crate) fn stringify(
        &mut self,
        expr: Idx<Expr>,
        v: Idx<Inst>,
    ) -> Result<Idx<Inst>, CompilerError> {
        // A literal `{null}` hole renders `tostring(nil)` = "nil".
        if matches!(self.body.expr_arena[expr], Expr::Null) {
            return Ok(self.emit_cur(Inst::ToString(v)));
        }
        if self.expr_is_nullable(expr) {
            return self.nil_guarded_render(v, |s| s.stringify_non_null(expr, v));
        }
        self.stringify_non_null(expr, v)
    }

    /// Every nullable hole needs this guard: `expr_ty` peels `Nullable` and the
    /// renderer is chosen from the non-null shape.
    pub(crate) fn nil_guarded_render(
        &mut self,
        v: Idx<Inst>,
        render: impl FnOnce(&mut Self) -> Result<Idx<Inst>, CompilerError>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let result = self.f.new_cell(None);
        let nil_bb = self.f.new_block();
        let val_bb = self.f.new_block();
        let join = self.f.new_block();
        self.branch_on_nil(v, nil_bb, val_bb);

        self.cur = nil_bb;
        let n = self.emit_cur(Inst::Const(Lit::Str(crate::names::ustr("nil"))));
        self.store_and_join(result, n, join);

        self.cur = val_bb;
        let rendered = render(self)?;
        self.store_and_join(result, rendered, join);

        self.cur = join;
        Ok(self.emit_cur(Inst::Load(result)))
    }

    pub(crate) fn stringify_non_null(
        &mut self,
        expr: Idx<Expr>,
        v: Idx<Inst>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let route = self.dispatch().stringify_route(expr);
        self.render_stringify(&route, Vec::new(), v)
    }

    /// Shares route selection with the instance graph.
    pub(crate) fn render_stringify(
        &mut self,
        route: &StringifyRoute,
        path: Vec<u32>,
        v: Idx<Inst>,
    ) -> Result<Idx<Inst>, CompilerError> {
        match route {
            StringifyRoute::ToString => Ok(self.emit_cur(Inst::ToString(v))),
            StringifyRoute::FormatFloat => {
                self.dispatched_call("__rock_format_float", path, vec![v])
            }
            // A transparent newtype's runtime value is the raw inner.
            StringifyRoute::Newtype(name) => {
                let name = bone_core::modules::display_name(name.as_str());
                let open = self.emit_cur(Inst::Const(Lit::Str(crate::names::ustr(
                    format!("{name}(").as_str(),
                ))));
                let inner = self.emit_cur(Inst::ToString(v));
                let close = self.emit_cur(Inst::Const(Lit::Str(crate::names::ustr(")"))));
                let a = self.emit_cur(Inst::StringConcat(open, inner));
                Ok(self.emit_cur(Inst::StringConcat(a, close)))
            }
            // Enum tags are zero-based integers.
            StringifyRoute::EnumNames(name) => {
                let names = self.emit_cur(Inst::Global(crate::mangle::enum_names_global(
                    self.ws, *name,
                )));
                Ok(self.read_index(names, v, ContainerKind::Map))
            }
            StringifyRoute::Inspect => self.dispatched_call("inspect", path, vec![v]),
            StringifyRoute::EscapedString => {
                self.dispatched_call("__rock_quote_string", path, vec![v])
            }
            StringifyRoute::Structural(tree) => {
                let tree = tree.as_ref().clone();
                self.render_fmt(&tree, path, ValueRepr::Scalar(v))
            }
            StringifyRoute::Unprintable(reason) => Err(self.unprintable_error(reason)),
            StringifyRoute::Unsupported => Err(self.error_at_hole(
                "skull-mir: lowering gap: interpolation hole of unknown/unsupported type"
                    .to_string(),
            )),
        }
    }

    pub(crate) fn lower_format(
        &mut self,
        expr: Idx<Expr>,
        v: Idx<Inst>,
        spec: &bone_core::hir::InterpSpec,
    ) -> Result<Idx<Inst>, CompilerError> {
        // A null with a spec would reach `string.format("%.2f", nil)`; it renders bare
        // "nil" whatever the spec asks for.
        if self.expr_is_nullable(expr) {
            let spec = *spec;
            return self.nil_guarded_render(v, move |s| s.lower_format_non_null(expr, v, &spec));
        }
        self.lower_format_non_null(expr, v, spec)
    }

    pub(crate) fn lower_format_non_null(
        &mut self,
        expr: Idx<Expr>,
        v: Idx<Inst>,
        spec: &bone_core::hir::InterpSpec,
    ) -> Result<Idx<Inst>, CompilerError> {
        // The instance walk reads this same plan; render exactly what it decided.
        let plan = self.dispatch().format_plan(expr, spec);

        if let Some(structural) = plan.structural.clone() {
            let rendered = match structural {
                StructuralRender::Formatted => {
                    let prec = self.lower_precision(spec)?;
                    self.dispatched_call("inspect_formatted", Vec::new(), vec![v, prec])?
                }
                StructuralRender::Plain(route) => self.render_stringify(&route, Vec::new(), v)?,
            };
            return self.pad_to_width(rendered, spec);
        }

        let mut out = match plan.special {
            Some("__rock_fmt_percent") => {
                let prec = self.lower_precision(spec)?;
                self.dispatched_call("__rock_fmt_percent", Vec::new(), vec![v, prec])?
            }
            Some("__rock_fmt_binary") => {
                self.dispatched_call("__rock_fmt_binary", Vec::new(), vec![v])?
            }
            Some(other) => return Err(gap(format!("unplanned format helper `{other}`"))),
            None => {
                // Lua `%d` truncates toward zero; Rock floors toward -inf.
                let v = if plan.val_is_float && matches!(plan.ty, Some('d' | 'x' | 'X' | 'o')) {
                    self.emit_cur(Inst::FloatToInt(v))
                } else {
                    v
                };
                let pieces = fmt_pieces(spec, plan.inferred, plan.skip_width, plan.needs_pad)
                    .ok_or_else(|| {
                        gap("interpolation format specifier (grouping / fill / center)")
                    })?;
                self.emit_format_pieces(v, pieces)?
            }
        };

        if let Some((sep, group)) = plan.grouping {
            let sep_v = self.emit_cur(Inst::Const(Lit::Str(crate::names::ustr(
                sep.to_string().as_str(),
            ))));
            let grp_v = self.emit_cur(Inst::Const(Lit::Int(group)));
            out =
                self.dispatched_call("__rock_fmt_grouping", Vec::new(), vec![out, sep_v, grp_v])?;
        }
        if plan.needs_pad {
            // Pad after grouping: separators count toward the field width.
            out = self.pad_to_width(out, spec)?;
        }
        Ok(out)
    }

    /// A width of 0 leaves the string untouched, so this is safe to call
    /// unconditionally.
    pub(crate) fn pad_to_width(
        &mut self,
        out: Idx<Inst>,
        spec: &bone_core::hir::InterpSpec,
    ) -> Result<Idx<Inst>, CompilerError> {
        let w = match (spec.width, spec.width_expr) {
            (Some(w), _) => self.emit_cur(Inst::Const(Lit::Int(w as i64))),
            (None, Some(we)) => self.lower_expr(we)?,
            (None, None) => self.emit_cur(Inst::Const(Lit::Int(0))),
        };
        let align = spec
            .alignment
            .map(|c| c.to_string())
            .unwrap_or_else(|| ">".to_string());
        let align_v = self.emit_cur(Inst::Const(Lit::Str(crate::names::ustr(align.as_str()))));
        let fill = spec.fill.unwrap_or(' ').to_string();
        let fill_v = self.emit_cur(Inst::Const(Lit::Str(crate::names::ustr(fill.as_str()))));
        self.dispatched_call("__rock_fmt_pad", Vec::new(), vec![out, w, align_v, fill_v])
    }

    /// Static `Int`, dynamic `{.{p}}` expression, or `-1` for none.
    pub(crate) fn lower_precision(
        &mut self,
        spec: &bone_core::hir::InterpSpec,
    ) -> Result<Idx<Inst>, CompilerError> {
        match (spec.precision, spec.precision_expr) {
            (Some(p), _) => Ok(self.emit_cur(Inst::Const(Lit::Int(p as i64)))),
            (None, Some(pe)) => self.lower_expr(pe),
            (None, None) => Ok(self.emit_cur(Inst::Const(Lit::Int(-1)))),
        }
    }

    pub(crate) fn emit_format_pieces(
        &mut self,
        v: Idx<Inst>,
        pieces: Vec<FmtPiece>,
    ) -> Result<Idx<Inst>, CompilerError> {
        if let [FmtPiece::Lit(s)] = &pieces[..] {
            return Ok(self.emit_cur(Inst::Format {
                value: v,
                fmt: crate::names::ustr(s.as_str()),
            }));
        }
        let mut fmt: Option<Idx<Inst>> = None;
        for piece in pieces {
            let part = match piece {
                FmtPiece::Lit(s) => {
                    self.emit_cur(Inst::Const(Lit::Str(crate::names::ustr(s.as_str()))))
                }
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
        let fmt =
            fmt.unwrap_or_else(|| self.emit_cur(Inst::Const(Lit::Str(crate::names::ustr("")))));
        Ok(self.emit_cur(Inst::FormatDyn { value: v, fmt }))
    }
}
