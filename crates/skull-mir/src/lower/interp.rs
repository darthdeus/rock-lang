//! String interpolation and `format`: rendering a hole, the nil-guarded and
//! non-null render paths, width/precision padding, and the piece emitter.
use super::*;

impl Builder<'_> {
    /// Lower an interpolated string to a left-folded `..` chain. A `Literal` part
    /// is a string const; a `{x}` hole stringifies by its static type (D8: the
    /// dispatch is decided HERE, not at the backend) — scalar -> [`Inst::ToString`]
    /// (`tostring`), float -> a `__rock_format_float` call, composite -> `inspect`,
    /// transparent newtype -> `Name(<tostring inner>)`. A `{x:spec}` hole gaps
    /// (format-spec lowering is its own follow-up; skull refuses rather than
    /// mis-format). An empty interpolation is the `""` const.
    pub(crate) fn lower_interp(
        &mut self,
        parts: IdxRange<bone_semantic::hir::InterpPart>,
    ) -> Result<Idx<Inst>, CompilerError> {
        use bone_semantic::hir::InterpPart;
        let mut acc: Option<Idx<Inst>> = None;
        for p in parts {
            let s = match self.body.interp_part_arena[p].clone() {
                InterpPart::Literal(lit) => self.emit_cur(Inst::Const(Lit::Str(lit))),
                InterpPart::Value { expr, spec } => {
                    match spec {
                        None if self.nobox_value_name(expr).is_some() => {
                            self.stringify_nobox_expr(expr)?
                        }
                        None => {
                            let v = self.lower_expr(expr)?;
                            self.stringify(expr, v)?
                        }
                        Some(spec) => {
                            let v = self.lower_expr(expr)?;
                            self.lower_format(expr, v, &spec)?
                        }
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

    pub(crate) fn stringify_nobox_expr(&mut self, expr: Idx<Expr>) -> Result<Idx<Inst>, CompilerError> {
        let name = self
            .nobox_value_name(expr)
            .ok_or_else(|| self.gap_here("nobox interpolation type"))?;
        let repr = self.lower_expr_repr(expr)?;
        self.stringify_nobox_repr(name, repr)
    }

    pub(crate) fn stringify_nobox_repr(
        &mut self,
        name: Ustr,
        repr: ValueRepr,
    ) -> Result<Idx<Inst>, CompilerError> {
        let ValueRepr::Group {
            parts, present, ..
        } = repr
        else {
            return Err(self.gap_here(format!(
                "nobox value `{name}` reached interpolation boxed"
            )));
        };
        if let Some(present) = present {
            let result = self.f.new_cell(None);
            let value_bb = self.f.new_block();
            let nil_bb = self.f.new_block();
            let join = self.f.new_block();
            self.f.set_term(
                self.cur,
                Term::Branch {
                    cond: present,
                    then_bb: value_bb,
                    else_bb: nil_bb,
                },
            );

            self.cur = nil_bb;
            let nil = self.emit_cur(Inst::Const(Lit::Str(Ustr::from("nil"))));
            self.store_and_join(result, nil, join);

            self.cur = value_bb;
            let rendered = self.stringify_nobox_repr(
                name,
                ValueRepr::Group {
                    name,
                    parts,
                    present: None,
                },
            )?;
            self.store_and_join(result, rendered, join);

            self.cur = join;
            return Ok(self.emit_cur(Inst::Load(result)));
        }
        let sid = *self
            .ws
            .struct_by_name
            .get(&name)
            .ok_or_else(|| gap(format!("nobox value `{name}` has no definition")))?;
        let fields = self.ws.modules[sid.module.index()].structs[sid.def]
            .fields
            .clone();
        if fields.len() != parts.len() {
            return Err(gap(format!(
                "nobox value `{name}` interpolation shape mismatch"
            )));
        }

        let mut out = self.emit_cur(Inst::Const(Lit::Str(Ustr::from(
            format!("{name} {{ ").as_str(),
        ))));
        for (i, (field, part)) in fields.into_iter().zip(parts).enumerate() {
            if i > 0 {
                let comma = self.emit_cur(Inst::Const(Lit::Str(Ustr::from(", "))));
                out = self.emit_cur(Inst::StringConcat(out, comma));
            }
            let label = self.emit_cur(Inst::Const(Lit::Str(Ustr::from(
                format!("{} = ", field.name).as_str(),
            ))));
            out = self.emit_cur(Inst::StringConcat(out, label));
            let rendered = match part {
                ValueRepr::Group {
                    name: child_name,
                    parts,
                    ..
                } => self.stringify_nobox_repr(
                    child_name,
                    ValueRepr::Group {
                        name: child_name,
                        parts,
                        present: None,
                    },
                )?,
                ValueRepr::Scalar(value) => self.stringify_struct_leaf(field.ty, value)?,
                part @ ValueRepr::Tuple { .. } => {
                    let value = self.pack(part);
                    self.stringify_struct_leaf(field.ty, value)?
                }
            };
            out = self.emit_cur(Inst::StringConcat(out, rendered));
        }
        let close = self.emit_cur(Inst::Const(Lit::Str(Ustr::from(" }"))));
        Ok(self.emit_cur(Inst::StringConcat(out, close)))
    }

    pub(crate) fn stringify_struct_leaf(
        &mut self,
        ty: TyId,
        value: Idx<Inst>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let ty = self
            .resolve_tyid(ty)
            .ok_or_else(|| gap("nobox interpolation field type"))?;
        if matches!(self.ty_node(ty), Some(TypeExpr::Nullable(_))) {
            let inspect = self.resolve_global_fn("inspect")?;
            return Ok(self.emit_cur(Inst::Call {
                func: inspect,
                args: vec![value],
            }));
        }
        match self.resolve_ty(ty) {
            Some(TypeExpr::Builtin(BuiltinTy::Int | BuiltinTy::Bool)) => {
                Ok(self.emit_cur(Inst::ToString(value)))
            }
            Some(TypeExpr::Builtin(BuiltinTy::String)) => {
                let quote = self.emit_cur(Inst::Const(Lit::Str(Ustr::from("\""))));
                let open = self.emit_cur(Inst::StringConcat(quote, value));
                Ok(self.emit_cur(Inst::StringConcat(open, quote)))
            }
            Some(TypeExpr::Builtin(BuiltinTy::Float)) => {
                let format = self.resolve_global_fn("__rock_format_float")?;
                Ok(self.emit_cur(Inst::Call {
                    func: format,
                    args: vec![value],
                }))
            }
            Some(TypeExpr::App {
                ctor: TypeCtor::Nominal(name),
                ..
            }) if self.ws.newtypes_by_name.contains_key(&name) => {
                let open = self.emit_cur(Inst::Const(Lit::Str(Ustr::from(
                    format!("{name}(").as_str(),
                ))));
                let inner = self.emit_cur(Inst::ToString(value));
                let close = self.emit_cur(Inst::Const(Lit::Str(Ustr::from(")"))));
                let out = self.emit_cur(Inst::StringConcat(open, inner));
                Ok(self.emit_cur(Inst::StringConcat(out, close)))
            }
            Some(TypeExpr::App {
                ctor: TypeCtor::Nominal(name),
                ..
            }) if payload_free_enum_variants(self.ws, name).is_some() => {
                let names = self.emit_cur(Inst::Global(Ustr::from(
                    format!("__rock_enum_{name}_names").as_str(),
                )));
                Ok(self.emit_cur(Inst::Index {
                    base: names,
                    index: value,
                    kind: ContainerKind::Map,
                }))
            }
            Some(_) => {
                let inspect = self.resolve_global_fn("inspect")?;
                Ok(self.emit_cur(Inst::Call {
                    func: inspect,
                    args: vec![value],
                }))
            }
            None => Err(gap("nobox interpolation field type")),
        }
    }

    /// Default `{x}` stringify for value `v` of expression `expr`, dispatched on
    /// `expr`'s static type (mirrors rock-codegen's interp dispatch).
    pub(crate) fn stringify(&mut self, expr: Idx<Expr>, v: Idx<Inst>) -> Result<Idx<Inst>, CompilerError> {
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
        self.f.set_term(self.cur, Term::Branch { cond: is_nil, then_bb: nil_bb, else_bb: val_bb });

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
            let spec = spec.clone();
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
        // Effective conversion letter when the spec omits one (`{x:5}`).
        let inferred = match self.expr_ty(expr) {
            Some(TypeExpr::Builtin(BuiltinTy::Int)) => Some('d'),
            Some(TypeExpr::Builtin(BuiltinTy::Float)) => Some('f'),
            Some(TypeExpr::Builtin(BuiltinTy::String | BuiltinTy::Bool)) => Some('s'),
            _ => None,
        };
        // A composite has no printf conversion: render it structurally (at
        // `.N` precision when asked, via `inspect_formatted`) and then run the
        // same pad path a conversion would. The numeric-presentation flags
        // below have no meaning on a structural rendering, so a spec carrying
        // one is left to gap rather than silently ignored.
        let has_precision = spec.precision.is_some() || spec.precision_expr.is_some();
        if spec.format_type.is_none()
            && inferred.is_none()
            && spec.grouping.is_none()
            && spec.sign.is_none()
            && !spec.alternate
            && !spec.zero_pad
            && self.is_composite_expr(expr)
        {
            let rendered = if has_precision {
                let prec = self.lower_precision(spec)?;
                let func = self.resolve_global_fn("inspect_formatted")?;
                self.emit_cur(Inst::Call { func, args: vec![v, prec] })
            } else {
                self.stringify_non_null(expr, v)?
            };
            return self.pad_to_width(rendered, spec);
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
        let align = spec.alignment.map(|c| c.to_string()).unwrap_or_else(|| ">".to_string());
        let align_v = self.emit_cur(Inst::Const(Lit::Str(Ustr::from(align.as_str()))));
        let fill = spec.fill.unwrap_or(' ').to_string();
        let fill_v = self.emit_cur(Inst::Const(Lit::Str(Ustr::from(fill.as_str()))));
        let func = self.resolve_global_fn("__rock_fmt_pad")?;
        Ok(self.emit_cur(Inst::Call { func, args: vec![out, w, align_v, fill_v] }))
    }

    /// A spec's precision as a value: static `Int`, dynamic `{.{p}}` expression,
    /// or `-1` (none).
    pub(crate) fn lower_precision(&mut self, spec: &bone_semantic::hir::InterpSpec) -> Result<Idx<Inst>, CompilerError> {
        match (spec.precision, spec.precision_expr) {
            (Some(p), _) => Ok(self.emit_cur(Inst::Const(Lit::Int(p as i64)))),
            (None, Some(pe)) => self.lower_expr(pe),
            (None, None) => Ok(self.emit_cur(Inst::Const(Lit::Int(-1)))),
        }
    }

    /// Whether `e`'s type is a composite (tuple/array/map/struct/enum) — what
    /// `inspect`/`inspect_formatted` render structurally.
    pub(crate) fn is_composite_expr(&self, e: Idx<Expr>) -> bool {
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
    pub(crate) fn emit_format_pieces(
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
}
