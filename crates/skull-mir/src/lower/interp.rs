//! String interpolation and `format`: rendering a hole, the nil-guarded and
//! non-null render paths, width/precision padding, and the piece emitter.
use super::*;

impl Builder<'_> {
    /// Lower an interpolated string to a left-folded `..` chain. A `Literal` part
    /// is a string const; a `{x}` hole stringifies by its static type (D8: the
    /// dispatch is decided HERE, not at the backend) — scalar -> [`Inst::ToString`]
    /// (`tostring`), float -> a `__rock_format_float` call, enum / value record /
    /// handle -> the structural rendering (`format.rs`), other composite ->
    /// `inspect`, transparent newtype -> `Name(<tostring inner>)`. A `{x:spec}` hole gaps
    /// (format-spec lowering is its own follow-up; skull refuses rather than
    /// mis-format). An empty interpolation is the `""` const.
    pub(crate) fn lower_interp(
        &mut self,
        parts: IdxRange<bone_semantic::hir::InterpPart>,
    ) -> Result<Idx<Inst>, CompilerError> {
        use bone_semantic::hir::InterpPart;
        let mut acc: Option<Idx<Inst>> = None;
        for p in parts {
            let s = match self.body.interp_part_arena[p] {
                InterpPart::Literal(lit) => self.emit_cur(Inst::Const(Lit::Str(lit))),
                InterpPart::Value { expr, spec } => {
                    // Every call a hole synthesizes belongs to the HOLE, not to
                    // the enclosing `Interp`: two float holes in one literal
                    // would otherwise be one site.
                    self.with_site_expr(expr, |s| s.lower_interp_hole(expr, spec))?
                }
            };
            acc = Some(match acc {
                None => s,
                Some(lhs) => self.emit_cur(Inst::StringConcat(lhs, s)),
            });
        }
        Ok(acc.unwrap_or_else(|| self.emit_cur(Inst::Const(Lit::Str(Ustr::from(""))))))
    }

    fn lower_interp_hole(
        &mut self,
        expr: Idx<Expr>,
        spec: Option<bone_semantic::hir::InterpSpec>,
    ) -> Result<Idx<Inst>, CompilerError> {
        match spec {
            None if self.dispatch().structural_route(expr).is_some() => {
                self.stringify_structural(expr)
            }
            // A tuple whose box would WIDEN is packed narrow here -- one slot
            // per element, the nobox part materialized. The widening exists for
            // readers that derive the layout from the declared type; `inspect`
            // walks the table at runtime and would render a wider tuple.
            //
            // Packing rather than rejecting on the spot is deliberate: the
            // resulting `<name>_ct` is what the post-transform residue check
            // already reports, so this boundary keeps the attribution (`while
            // emitting <fn>`) and the per-body sibling accumulation that a fresh
            // lowering-time error would lose. Pinned by `multi_rejection` and
            // `nobox_diagnostic_attribution`.
            None if self.tuple_box_shape(expr)?.is_some() => {
                let r = self.lower_expr_repr(expr)?.into_value();
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

    /// A structural hole renders from the value's own representation, so a
    /// scalarized or flat value is never packed just to be printed.
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
        // A nullable scalarized hole: the same null text as any other hole.
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

    /// Default `{x}` stringify for value `v` of expression `expr`, dispatched on
    /// `expr`'s static type.
    pub(crate) fn stringify(
        &mut self,
        expr: Idx<Expr>,
        v: Idx<Inst>,
    ) -> Result<Idx<Inst>, CompilerError> {
        // A literal `{null}` hole renders `tostring(nil)` = "nil".
        if matches!(self.body.expr_arena[expr], Expr::Null) {
            return Ok(self.emit_cur(Inst::ToString(v)));
        }
        // `expr_ty` PEELS `Nullable`, so every arm below is picked from the
        // NON-NULL shape while the value can still arrive nil. Guard once here
        // instead of per arm: the float and payload-free-enum renderings fault on
        // nil and a newtype renders `Id(nil)`, and the arms that already render
        // "nil" are unaffected by the branch.
        if self.expr_is_nullable(expr) {
            return self.nil_guarded_render(v, |s| s.stringify_non_null(expr, v));
        }
        self.stringify_non_null(expr, v)
    }

    /// `if v == nil then "nil" else <render(v)>` -- the guard every interpolation
    /// path needs on a nullable hole, since `expr_ty` peels `Nullable` and the
    /// renderer is chosen from the non-null shape.
    pub(crate) fn nil_guarded_render(
        &mut self,
        v: Idx<Inst>,
        render: impl FnOnce(&mut Self) -> Result<Idx<Inst>, CompilerError>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let result = self.f.new_cell(None);
        let nil = self.emit_cur(Inst::Const(Lit::Nil));
        let is_nil = self.emit_cur(Inst::Eq(v, nil));
        let nil_bb = self.f.new_block();
        let val_bb = self.f.new_block();
        let join = self.f.new_block();
        self.f.set_term(
            self.cur,
            Term::Branch {
                cond: is_nil,
                then_bb: nil_bb,
                else_bb: val_bb,
            },
        );

        self.cur = nil_bb;
        let n = self.emit_cur(Inst::Const(Lit::Str(Ustr::from("nil"))));
        self.store_and_join(result, n, join);

        self.cur = val_bb;
        let rendered = render(self)?;
        self.store_and_join(result, rendered, join);

        self.cur = join;
        Ok(self.emit_cur(Inst::Load(result)))
    }

    /// `stringify` for a value known to be present. Split out so the nullable
    /// guard above can reuse every arm on its non-null path.
    pub(crate) fn stringify_non_null(
        &mut self,
        expr: Idx<Expr>,
        v: Idx<Inst>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let route = self.dispatch().stringify_route(expr);
        self.render_stringify(&route, Vec::new(), v)
    }

    /// Render ONE [`StringifyRoute`]. The route is decided by
    /// `skull_mir::lower::dispatch`, which the instance walk reads too; this is
    /// the rendering half and makes no type decision of its own.
    ///
    /// `path` identifies the obligation within its hole -- empty for the hole,
    /// a field-index chain for a nobox value's leaf.
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
            // A transparent newtype renders `Name(<inner>)`; its runtime value IS
            // the raw inner, so `tostring` it directly.
            StringifyRoute::Newtype(name) => {
                let open = self.emit_cur(Inst::Const(Lit::Str(Ustr::from(
                    format!("{name}(").as_str(),
                ))));
                let inner = self.emit_cur(Inst::ToString(v));
                let close = self.emit_cur(Inst::Const(Lit::Str(Ustr::from(")"))));
                let a = self.emit_cur(Inst::StringConcat(open, inner));
                Ok(self.emit_cur(Inst::StringConcat(a, close)))
            }
            // A payload-free enum value is its ordinal int -> render the variant
            // name statically from `__rock_enum_<E>_names[tag]` (0-indexed, the
            // table holds the full `<E>::<V>` display string, matching rock's
            // inspect output -- verified by skull-diff). No `inspect` round-trip;
            // the value carries no type tag at runtime, so this is the only point
            // the static type is known. The `inspect(any)` escape (a payload-free
            // enum passed to `inspect`/stored in `any`/inside a composite) is the
            // deferred box-on-escape-into-`any` work.
            StringifyRoute::EnumNames(name) => {
                let names = self.emit_cur(Inst::Global(Ustr::from(
                    format!(
                        "__rock_enum_{}_names",
                        crate::mangle::type_symbol(self.ws, *name)
                    )
                    .as_str(),
                )));
                Ok(self.emit_cur(Inst::Index {
                    base: names,
                    index: v,
                    kind: ContainerKind::Map,
                }))
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

    /// Format value `v` of `expr` per `spec` (`{x:.2f}` etc.). A static spec
    /// emits one `string.format("<fmt>", v)`; dynamic width/precision assemble
    /// the conversion at runtime (`FormatDyn`); grouping/center/fill wrap with
    /// the stdlib `__rock_fmt_grouping`/`__rock_fmt_pad`; a composite with `.N`
    /// precision routes through `inspect_formatted`. The `%`/`b` conversions
    /// (raw prelude globals, not FuncRef-resolvable) gap for now.
    pub(crate) fn lower_format(
        &mut self,
        expr: Idx<Expr>,
        v: Idx<Inst>,
        spec: &bone_semantic::hir::InterpSpec,
    ) -> Result<Idx<Inst>, CompilerError> {
        // Same `Nullable`-peel guard as `stringify`: a null with a spec would reach
        // `string.format("%.2f", nil)`. A null has nothing to pad or round, so it
        // renders bare "nil" whatever the spec asks for.
        if self.expr_is_nullable(expr) {
            let spec = *spec;
            return self.nil_guarded_render(v, move |s| s.lower_format_non_null(expr, v, &spec));
        }
        self.lower_format_non_null(expr, v, spec)
    }

    /// `lower_format` for a value known to be present.
    pub(crate) fn lower_format_non_null(
        &mut self,
        expr: Idx<Expr>,
        v: Idx<Inst>,
        spec: &bone_semantic::hir::InterpSpec,
    ) -> Result<Idx<Inst>, CompilerError> {
        // Every decision below -- which helper is called, and the flags the
        // printf rendering reads -- comes from the ONE plan the instance walk
        // also reads. This function renders it.
        let plan = self.dispatch().format_plan(expr, spec);

        // A composite has no printf conversion: render it structurally (at
        // `.N` precision when asked, via `inspect_formatted`) and then run the
        // same pad path a conversion would.
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

        // `%` (percent) and `b` (binary) have no printf equivalent -> the stdlib
        // helpers; width/align deferred to the pad helper (skip_width set above).
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
                // printf path. An integer-class conversion on a float floors first
                // (Lua `%d` truncates toward zero; rock floors toward -inf).
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
            let sep_v = self.emit_cur(Inst::Const(Lit::Str(Ustr::from(sep.to_string().as_str()))));
            let grp_v = self.emit_cur(Inst::Const(Lit::Int(group)));
            out =
                self.dispatched_call("__rock_fmt_grouping", Vec::new(), vec![out, sep_v, grp_v])?;
        }
        if plan.needs_pad {
            // Pad AFTER grouping (separators count toward the field width).
            out = self.pad_to_width(out, spec)?;
        }
        Ok(out)
    }

    /// Pad an already-rendered string to the spec's field width. A width of 0
    /// (no width in the spec) leaves the string untouched, so this is safe to
    /// call unconditionally.
    pub(crate) fn pad_to_width(
        &mut self,
        out: Idx<Inst>,
        spec: &bone_semantic::hir::InterpSpec,
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
        let align_v = self.emit_cur(Inst::Const(Lit::Str(Ustr::from(align.as_str()))));
        let fill = spec.fill.unwrap_or(' ').to_string();
        let fill_v = self.emit_cur(Inst::Const(Lit::Str(Ustr::from(fill.as_str()))));
        self.dispatched_call("__rock_fmt_pad", Vec::new(), vec![out, w, align_v, fill_v])
    }

    /// A spec's precision as a value: static `Int`, dynamic `{.{p}}` expression,
    /// or `-1` (none).
    pub(crate) fn lower_precision(
        &mut self,
        spec: &bone_semantic::hir::InterpSpec,
    ) -> Result<Idx<Inst>, CompilerError> {
        match (spec.precision, spec.precision_expr) {
            (Some(p), _) => Ok(self.emit_cur(Inst::Const(Lit::Int(p as i64)))),
            (None, Some(pe)) => self.lower_expr(pe),
            (None, None) => Ok(self.emit_cur(Inst::Const(Lit::Int(-1)))),
        }
    }

    /// Emit `string.format` from decomposed [`FmtPiece`]s: a single static
    /// conversion ([`Inst::Format`]) when all-literal, else assemble the
    /// conversion string at runtime and [`Inst::FormatDyn`].
    pub(crate) fn emit_format_pieces(
        &mut self,
        v: Idx<Inst>,
        pieces: Vec<FmtPiece>,
    ) -> Result<Idx<Inst>, CompilerError> {
        if let [FmtPiece::Lit(s)] = &pieces[..] {
            return Ok(self.emit_cur(Inst::Format {
                value: v,
                fmt: Ustr::from(s.as_str()),
            }));
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
}
