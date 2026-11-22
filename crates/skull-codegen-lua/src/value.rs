use super::*;

impl Emitter<'_> {
    pub(crate) fn write_value(&self, out: &mut String, id: Idx<Inst>, inst: &Inst) {
        let instrument = self.allocation.and_then(|allocation| {
            if allocation.pc_sources.is_some()
                && matches!(
                    inst,
                    Inst::Field { .. } | Inst::Index { .. } | Inst::TupleIndex { .. }
                )
            {
                self.pc_source(out.len(), id, "projection");
            }
            self.record_allocation_site(id, emitted_value_allocation_kind(inst), out.len())
        });
        if let Some(ref site) = instrument {
            out.push_str("(__rock_alloc_site[\"");
            write!(out, "{:016x}", site.id).unwrap();
            out.push_str("\"] and ");
        }
        self.write_value_inner(out, id, inst);
        if instrument.is_some() {
            out.push(')');
        }
    }

    fn write_value_inner(&self, out: &mut String, id: Idx<Inst>, inst: &Inst) {
        if let Some(lit) = constant_bits(self.func, self.inlinable, id) {
            write_lit(out, &lit);
            return;
        }
        match inst {
            Inst::Const(l) => write_lit(out, l),
            Inst::Global(g) => {
                if let Some(identity) = self.func.code_value_identities.get(&id) {
                    self.write_code_slot_ref(out, identity);
                } else if let Some(path) = self
                    .func
                    .fn_value_symbols
                    .get(&id)
                    .and_then(|&f| func_of(self.ws, f).lua_extern_path)
                {
                    self.write_lua_extern_path(out, path.as_str());
                } else {
                    self.write_global(out, *g);
                }
            }
            Inst::Load(c) => write_cellname(out, self.func, self.frame, self.cell_colors, *c),

            Inst::IntAdd(a, b) | Inst::FloatAdd(a, b) => self.write_bin(out, "+", a, b),
            Inst::IntSub(a, b) | Inst::FloatSub(a, b) => self.write_bin(out, "-", a, b),
            Inst::IntMul(a, b) | Inst::FloatMul(a, b) => self.write_bin(out, "*", a, b),
            Inst::UintAdd(a, b) => self.write_u32_bin(out, "+", a, b),
            Inst::UintSub(a, b) => self.write_u32_bin(out, "-", a, b),
            Inst::UintMul(a, b) => self.write_u32_mul(out, a, b),
            Inst::IntDiv(a, b) | Inst::UintDiv(a, b) => {
                out.push_str("math.floor(");
                self.write_bin(out, "/", a, b);
                out.push(')');
            }
            Inst::FloatDiv(a, b) => self.write_bin(out, "/", a, b),
            Inst::IntMod(a, b) | Inst::FloatMod(a, b) => self.write_bin(out, "%", a, b),
            Inst::UintMod(a, b) => self.write_u32_bin(out, "%", a, b),
            Inst::VecStrideNilToFalse(value) => {
                out.push('(');
                self.write_opnd(out, *value);
                out.push_str(" or false)");
            }
            Inst::VecIndexSlot(index) => self.write_vec_slot(out, *index),
            Inst::NbIndexSlot(index) => self.write_opnd(out, *index),
            Inst::VecStrideFalseToNil(value) => {
                out.push('(');
                self.write_opnd(out, *value);
                out.push_str(" or nil)");
            }
            Inst::StringConcat(a, b) => {
                // A flat concat run lets LuaJIT allocate the result once with BC_CAT.
                let mut parts = Vec::new();
                concat_run(self.func, self.inlinable, &mut parts, *a);
                concat_run(self.func, self.inlinable, &mut parts, *b);
                for (i, p) in parts.iter().enumerate() {
                    if i > 0 {
                        out.push_str(" .. ");
                    }
                    self.write_opnd(out, *p);
                }
            }
            Inst::ToString(x) => self.write_builtin_call(out, "tostring", &[*x]),
            Inst::Format { value, fmt } => {
                out.push_str("string.format(");
                write_lua_string(out, fmt.as_str());
                out.push_str(", ");
                self.write_format_arg(out, inst, *value);
                out.push(')');
            }
            Inst::FormatDyn { value, fmt } => {
                out.push_str("string.format(");
                self.write_opnd(out, *fmt);
                out.push_str(", ");
                self.write_format_arg(out, inst, *value);
                out.push(')');
            }
            Inst::FloatToInt(x) => self.write_builtin_call(out, "math.floor", &[*x]),
            // These LuaJIT identity conversions must reference their operand:
            // rendering it would evaluate a producer twice.
            Inst::IntToFloat(x) | Inst::UintToFloat(x) | Inst::UintToInt(x) => {
                self.write_opnd(out, *x)
            }
            Inst::FloatToF32(x) => self.write_builtin_call(out, "__rock_f32", &[*x]),
            Inst::IntToUint(x) | Inst::FloatToUint(x) => self.write_checked_u32(out, *x),
            Inst::UintBitcastInt(x) => {
                out.push_str("(((");
                self.write_opnd(out, *x);
                out.push_str(" + 2147483648) % 4294967296) - 2147483648)");
            }
            Inst::IntBitcastUint(x) => {
                out.push_str("((");
                self.write_opnd(out, *x);
                out.push_str(") % 4294967296)");
            }

            Inst::IntLt(a, b) | Inst::UintLt(a, b) | Inst::FloatLt(a, b) => {
                self.write_bin(out, "<", a, b)
            }
            Inst::IntLeq(a, b) | Inst::UintLeq(a, b) | Inst::FloatLeq(a, b) => {
                self.write_bin(out, "<=", a, b)
            }
            Inst::IntGt(a, b) | Inst::UintGt(a, b) | Inst::FloatGt(a, b) => {
                self.write_bin(out, ">", a, b)
            }
            Inst::IntGeq(a, b) | Inst::UintGeq(a, b) | Inst::FloatGeq(a, b) => {
                self.write_bin(out, ">=", a, b)
            }
            Inst::Eq(a, b) => self.write_bin(out, "==", a, b),
            Inst::NotEq(a, b) => self.write_bin(out, "~=", a, b),

            Inst::Neg(x) => {
                out.push('-');
                self.write_opnd(out, *x);
            }
            Inst::Not(x) => {
                out.push_str("not ");
                self.write_opnd(out, *x);
            }
            Inst::BitNot(x) => self.write_builtin_call(out, "bit.bnot", &[*x]),
            Inst::UintBitNot(x) => {
                out.push_str("(bit.bnot(");
                self.write_opnd(out, *x);
                out.push_str(")) % 4294967296");
            }

            Inst::BitAnd(a, b) => self.write_builtin_call(out, "bit.band", &[*a, *b]),
            Inst::BitOr(a, b) => self.write_builtin_call(out, "bit.bor", &[*a, *b]),
            Inst::BitXor(a, b) => self.write_builtin_call(out, "bit.bxor", &[*a, *b]),
            Inst::Shl(a, b) => self.write_builtin_call(out, "bit.lshift", &[*a, *b]),
            Inst::Shr(a, b) => self.write_builtin_call(out, "bit.rshift", &[*a, *b]),
            Inst::UintBitAnd(a, b) => self.write_u32_bitfn(out, "bit.band", a, b),
            Inst::UintBitOr(a, b) => self.write_u32_bitfn(out, "bit.bor", a, b),
            Inst::UintBitXor(a, b) => self.write_u32_bitfn(out, "bit.bxor", a, b),
            Inst::UintShl(a, b) => self.write_u32_shift(out, "bit.lshift", a, b),
            Inst::UintShr(a, b) => {
                out.push_str("math.floor(");
                self.write_opnd(out, *a);
                out.push_str(" / (2 ^ ");
                self.write_u32_shift_count(out, *b);
                out.push_str("))");
            }

            Inst::Call { .. } | Inst::CallValue { .. } => self.write_call(out, id),
            Inst::Lambda { .. } => self.reject_pass_residue("a closure", "closure conversion"),

            Inst::MakeTuple(vs) => {
                out.push('{');
                self.write_csv(out, vs);
                out.push('}');
            }
            Inst::MakeVec(vs) => {
                crate::write_vec(out, vs.len(), |out| self.write_csv(out, vs));
            }
            Inst::MakeFlatVec(vec) => self.write_flat_vec(out, vec),
            Inst::MakeStruct { ty, fields } => {
                // Column and strided Vecs already carry a storage header.
                let nominal = ty.filter(|name| name.as_str() != "Vec");
                if nominal.is_some() {
                    out.push_str("setmetatable(");
                }
                out.push('{');
                for (i, (n, id)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    write_table_key(out, n.as_str());
                    self.write_opnd(out, *id);
                }
                out.push('}');
                if let Some(name) = nominal {
                    self.write_box_metatable(out, name);
                }
            }
            Inst::MakeValueStruct {
                name,
                representation,
                form,
                fields,
            }
            | Inst::MaterializeValueStruct {
                name,
                representation,
                form,
                fields,
            } => {
                let Some(suffix) = ctor_suffix(self.func, *representation, *form) else {
                    self.ctx.reject(
                        name.to_string(),
                        CompilerError::new_no_location(format!(
                            "`{name}` value constructor has no valid supplied representation plan"
                        )),
                    );
                    return;
                };
                let _ = write!(
                    out,
                    "{}{suffix}(",
                    skull_mir::mangle::type_symbol(self.ws, *name)
                );
                for (i, (_, id)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    self.write_opnd(out, *id);
                }
                out.push(')');
            }
            Inst::MakeNullableValue { present, value } => {
                out.push('(');
                self.write_opnd(out, *present);
                out.push_str(" and ");
                self.write_opnd(out, *value);
                out.push_str(" or nil)");
            }
            Inst::MakeEnum {
                tag,
                ty,
                positional,
                named,
            } => self.write_enum_box(out, *ty, positional, named, |out| {
                write_lua_string(out, tag.as_str());
            }),
            Inst::MakeEnumDyn {
                tag,
                ty,
                positional,
                named,
            } => {
                self.write_dynamic_enum(out, *tag, *ty, positional, named);
            }

            Inst::FfiNewArray { ctype, count, .. } => {
                // The prelude's allocator: large buffers leave the Lua heap.
                let _ = write!(
                    out,
                    "__rock_nb_alloc(\"{}\", ",
                    skull_mir::mangle::type_symbol(self.ws, *ctype)
                );
                self.write_opnd(out, *count);
                out.push(')');
            }
            Inst::FfiViewCast { cty, ptr } => {
                let _ = write!(
                    out,
                    "ffi.cast(\"{}*\", ",
                    skull_mir::mangle::type_symbol(self.ws, *cty)
                );
                self.write_opnd(out, *ptr);
                out.push(')');
            }
            Inst::Field { base, field, .. } => {
                self.write_opnd(out, *base);
                write_field_key(out, field.as_str());
            }
            Inst::TupleIndex { base, index } => {
                self.write_opnd(out, *base);
                out.push('[');
                push_uint(out, (index + 1) as u32);
                out.push(']');
            }

            Inst::Index { base, index, kind } => self.write_index(out, id, *base, *index, *kind),
            Inst::VecCheckedIndex { vec, index } => {
                // The live length guards a logical element, independently of
                // false or nil leaves and stale physical storage.
                let fast =
                    self.repeatable(*vec, 2) && self.repeatable(*index, 2) && self.used_as_expr(id);
                if fast {
                    self.write_opnd(out, *index);
                    out.push_str(" >= 0 and ");
                    self.write_opnd(out, *index);
                    out.push_str(" < ");
                    self.write_opnd(out, *vec);
                    out.push_str(".len and (");
                    self.write_opnd(out, *index);
                    out.push_str(" + 1) or ");
                }
                self.write_builtin_call(out, "__vec_checked_index", &[*vec, *index]);
            }
            Inst::Len { base, kind } => match kind {
                ContainerKind::Vec
                | ContainerKind::VecInBounds
                | ContainerKind::VecNonNull
                | ContainerKind::VecNonNullInBounds => {
                    self.write_opnd(out, *base);
                    out.push_str("[0]");
                }
                ContainerKind::VecValueColumns(_)
                | ContainerKind::VecValueStrided(_)
                | ContainerKind::NativeBuffer
                | ContainerKind::NativeBufferInBounds
                | ContainerKind::NativeBufferValue(_) => {
                    self.write_opnd(out, *base);
                    out.push_str(".len");
                }
                ContainerKind::NativeBufferLeaf => {
                    unreachable!("`len` of a leaf view: lowering must ask the owning buffer")
                }
                ContainerKind::VecColumn | ContainerKind::VecStrideSlot | ContainerKind::Map => {
                    out.push('#');
                    self.write_opnd(out, *base);
                }
                ContainerKind::Record(_) => unreachable!("len of a heap record"),
            },

            Inst::Store { .. }
            | Inst::StoreField { .. }
            | Inst::StoreIndex { .. }
            | Inst::StoreGlobal { .. }
            | Inst::MapNext { .. }
            | Inst::CallMulti { .. }
            | Inst::CallValueMulti { .. }
            | Inst::CoroSetInner { .. }
            | Inst::CoroHook { .. }
            | Inst::ArrayPush { .. } => unreachable!("effect has no value"),

            Inst::CallBoundMulti { .. } => {
                unreachable!("placeholder bound-call must be substituted before emit")
            }

            Inst::MakeEnv { fn_ref, fields } => {
                let callable = fn_ref.is_some();
                if callable {
                    out.push_str("setmetatable({ __slot = ");
                    let identity = self
                        .func
                        .code_value_identities
                        .get(&id)
                        .expect("Rock closure MakeEnv missing compiler-owned CodeRef identity");
                    self.write_code_slot_ref(out, identity);
                } else {
                    out.push('{');
                }
                for (i, (field, value)) in fields.iter().enumerate() {
                    if callable || i > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(field.as_str());
                    out.push_str(" = ");
                    self.write_opnd(out, *value);
                }
                out.push_str(if callable { " }, __rock_fn_mt)" } else { "}" });
            }
        }
    }

    fn write_code_slot_ref(
        &self,
        out: &mut String,
        identity: &skull_mir::code_identity::CodeIdentity,
    ) {
        out.push_str("__rock_code_slot_ref(");
        write_lua_string(out, &identity.logical_key(self.ws, self.interner));
        out.push(')');
    }

    /// LuaJIT's `%f` prints garbage digits below about 1e-152. Every valid
    /// precision (at most 99) rounds a magnitude under 1e-100 to zero, so the
    /// argument becomes a zero of the same sign.
    fn write_format_arg(&self, out: &mut String, format: &Inst, value: Idx<Inst>) {
        if fixed_point_format(self.func, format) {
            self.write_builtin_call(out, crate::binding::FIXED_POINT, &[value]);
        } else {
            self.write_opnd(out, value);
        }
    }

    fn write_bin(&self, out: &mut String, op: &str, a: &Idx<Inst>, b: &Idx<Inst>) {
        self.write_opnd(out, *a);
        out.push(' ');
        out.push_str(op);
        out.push(' ');
        self.write_opnd(out, *b);
    }

    pub(crate) fn write_builtin_call(&self, out: &mut String, name: &str, args: &[Idx<Inst>]) {
        out.push_str(name);
        out.push('(');
        self.write_csv(out, args);
        out.push(')');
    }

    fn write_u32_bin(&self, out: &mut String, op: &str, a: &Idx<Inst>, b: &Idx<Inst>) {
        out.push('(');
        self.write_bin(out, op, a, b);
        out.push_str(") % 4294967296");
    }

    fn write_u32_bitfn(&self, out: &mut String, f: &str, a: &Idx<Inst>, b: &Idx<Inst>) {
        out.push('(');
        self.write_builtin_call(out, f, &[*a, *b]);
        out.push_str(" % 4294967296)");
    }

    fn write_u32_shift(&self, out: &mut String, f: &str, a: &Idx<Inst>, b: &Idx<Inst>) {
        out.push_str(f);
        out.push('(');
        self.write_opnd(out, *a);
        out.push_str(", ");
        self.write_u32_shift_count(out, *b);
        out.push_str(") % 4294967296");
    }

    fn write_u32_shift_count(&self, out: &mut String, count: Idx<Inst>) {
        if valid_constant_shift_count(self.func, count) {
            self.write_opnd(out, count);
        } else {
            self.write_builtin_call(out, "__rock_u32_shift_count", &[count]);
        }
    }

    fn write_u32_mul(&self, out: &mut String, a: &Idx<Inst>, b: &Idx<Inst>) {
        // Every product here is below 2^53. Splitting at 16 bits avoids the
        // double-rounding that a direct Lua multiplication would incur.
        out.push_str("((");
        self.write_opnd(out, *a);
        out.push_str(" % 65536) * (");
        self.write_opnd(out, *b);
        out.push_str(" % 65536) + ((math.floor(");
        self.write_opnd(out, *a);
        out.push_str(" / 65536) * (");
        self.write_opnd(out, *b);
        out.push_str(" % 65536) + (");
        self.write_opnd(out, *a);
        out.push_str(" % 65536) * math.floor(");
        self.write_opnd(out, *b);
        out.push_str(" / 65536)) % 65536) * 65536) % 4294967296");
    }

    fn write_checked_u32(&self, out: &mut String, x: Idx<Inst>) {
        if let Some(value) = checked_u32_constant(self.func, x) {
            if value == 0.0 && value.is_sign_negative() {
                out.push_str("-0.0");
            } else {
                let _ = write!(out, "{value:.0}");
            }
            return;
        }
        self.write_builtin_call(out, "__rock_checked_u32", &[x]);
    }

    /// Whether `id` may be written a second time on a path that is rarely
    /// taken: it renders as a bare local or a literal, or as field reads or
    /// one integer sum over those, which have no effect to repeat.
    fn repeatable(&self, id: Idx<Inst>, depth: u8) -> bool {
        if self.opnd_is_plain(id) {
            return true;
        }
        match self.func.insts[id] {
            Inst::Field { base, .. } | Inst::TupleIndex { base, .. } if depth > 0 => {
                self.repeatable(base, depth - 1)
            }
            Inst::IntAdd(a, b) | Inst::IntSub(a, b) if depth > 0 => {
                self.repeatable(a, 0) && self.repeatable(b, 0)
            }
            _ => false,
        }
    }

    fn used_as_expr(&self, id: Idx<Inst>) -> bool {
        self.inlinable[raw(id) as usize] || self.has_value_local(id)
    }

    fn write_vec_slot(&self, out: &mut String, index: Idx<Inst>) {
        match self.func.insts[index] {
            Inst::Const(Lit::Int(i))
                if (0..i64::from(i32::MAX)).contains(&i)
                    && self.inlinable[raw(index) as usize]
                    && !self.hoist.contains_key(&index) =>
            {
                let _ = write!(out, "{}", i + 1);
            }
            _ => {
                self.write_opnd(out, index);
                out.push_str(" + 1");
            }
        }
    }

    fn write_index(
        &self,
        out: &mut String,
        id: Idx<Inst>,
        base: Idx<Inst>,
        index: Idx<Inst>,
        kind: ContainerKind,
    ) {
        match kind {
            // Check negative indices before reading the slot, since logical
            // index -1 addresses the length. False elements need no helper.
            ContainerKind::Vec | ContainerKind::VecNonNull
                if self.repeatable(base, 2)
                    && self.repeatable(index, 3)
                    && self.used_as_expr(id) =>
            {
                let check_sign =
                    !matches!(self.func.insts[index], Inst::Const(Lit::Int(i)) if i >= 0);
                if check_sign {
                    self.write_opnd(out, index);
                    out.push_str(" >= 0 and ");
                }
                self.write_index_place(out, base, index, ContainerKind::VecInBounds);
                out.push_str(" or (");
                if check_sign {
                    out.push('(');
                    self.write_opnd(out, index);
                    out.push_str(" < 0 or ");
                }
                self.write_index_place(out, base, index, ContainerKind::VecInBounds);
                out.push_str(" == nil");
                if check_sign {
                    out.push(')');
                }
                out.push_str(" and ");
                self.write_builtin_call(out, "__vec_checked_get", &[base, index]);
                out.push(')');
            }
            ContainerKind::Vec | ContainerKind::VecNonNull => {
                self.write_builtin_call(out, "__vec_checked_get", &[base, index]);
            }
            // Lowering decomposes whole-element reads of column storage into
            // per-column reads; any Lua emitted here would be wrong code.
            ContainerKind::VecValueColumns(elem) => {
                unreachable!(
                    "whole-element index of column-stored [{elem}] reached emit, in `{}`",
                    self.func.name
                )
            }
            ContainerKind::VecValueStrided(elem) => {
                unreachable!(
                    "whole-element index of strided [{elem}] reached emit, in `{}`",
                    self.func.name
                )
            }
            ContainerKind::NativeBuffer | ContainerKind::NativeBufferValue(_) => {
                self.write_builtin_call(out, "__nb_checked_get", &[base, index]);
            }
            _ => self.write_index_place(out, base, index, kind),
        }
    }

    pub(crate) fn write_index_place(
        &self,
        out: &mut String,
        base: Idx<Inst>,
        index: Idx<Inst>,
        kind: ContainerKind,
    ) {
        self.write_opnd(out, base);
        match kind {
            // The index was proven within `[0, len)`, so the slot holds the
            // element, nil and false included: no fallback.
            ContainerKind::VecInBounds | ContainerKind::VecNonNullInBounds => {
                out.push('[');
                self.write_vec_slot(out, index);
                out.push(']');
                return;
            }
            ContainerKind::NativeBufferInBounds => {
                out.push_str(".ptr");
            }
            ContainerKind::Record(_) => {
                if let Inst::Const(Lit::Str(field)) = &self.func.insts[index] {
                    write_field_key(out, field.as_str());
                    return;
                }
            }
            // `index` is already 1-based and checked (`VecCheckedIndex`).
            ContainerKind::VecColumn
            | ContainerKind::VecStrideSlot
            | ContainerKind::NativeBufferLeaf
            | ContainerKind::Map => {}
            _ => unreachable!("{kind:?} has no directly indexable storage"),
        }
        out.push('[');
        self.write_opnd(out, index);
        out.push(']');
    }

    fn write_flat_vec(&self, out: &mut String, vec: &FlatVecCtor) {
        let header = |out: &mut String| {
            self.write_vec_header(
                out,
                vec.contents.repr_name(),
                vec.elem.name,
                vec.contents.stride(),
                vec.capacity,
            );
        };
        let header_key = |out: &mut String| {
            out.push('[');
            write_lua_string(out, VEC_COLUMN_HEADER_FIELD);
            out.push_str("] = ");
            header(out);
        };
        match &vec.contents {
            FlatVecContents::Header(_) => header(out),
            FlatVecContents::Columns { columns, len } => {
                out.push('{');
                header_key(out);
                for (name, values) in columns {
                    out.push_str(", [");
                    write_lua_string(out, name.as_str());
                    out.push_str("] = {");
                    self.write_csv(out, values);
                    out.push('}');
                }
                let _ = write!(out, ", len = {len}}}");
            }
            FlatVecContents::Rows { values, len, .. } => {
                out.push_str("{_data = {");
                self.write_csv(out, values);
                let _ = write!(out, "}}, len = {len}, ");
                header_key(out);
                out.push('}');
            }
        }
    }

    /// A flat Vec's header. Only a reservation (`cap > 0`) ever writes one, so
    /// a header with a constant zero capacity is shared per element type.
    fn write_vec_header(
        &self,
        out: &mut String,
        repr: &str,
        elem: ustr::Ustr,
        stride: Option<usize>,
        capacity: Idx<Inst>,
    ) {
        let elem = skull_mir::mangle::type_symbol(self.ws, elem);
        if matches!(self.func.insts[capacity], Inst::Const(Lit::Int(0))) {
            out.push_str("__rock_vec_header(");
            write_lua_string(out, repr);
            out.push_str(", ");
            write_lua_string(out, elem.as_str());
            let _ = write!(out, ", {})", stride.unwrap_or(0));
            return;
        }
        out.push_str("{repr = ");
        write_lua_string(out, repr);
        out.push_str(", elem = ");
        write_lua_string(out, elem.as_str());
        if let Some(stride) = stride {
            let _ = write!(out, ", stride = {stride}");
        }
        out.push_str(", cap = ");
        self.write_opnd(out, capacity);
        out.push('}');
    }

    pub(crate) fn cell_is_frame_field(&self, cell: Idx<Cell>) -> bool {
        self.frame.is_some_and(|frame| frame.fields.contains(&cell))
    }

    pub(crate) fn write_single_frame_assignment(
        &self,
        out: &mut String,
        target: &str,
        value: Idx<Inst>,
    ) {
        if self.never_nil(value) {
            let _ = write!(out, " {target}");
            self.write_assignment_rhs(out, value);
            return;
        }
        if !self.opnd_is_plain(value) {
            return self.write_frame_assignment(out, &[(target.to_owned(), true)], |out| {
                self.write_opnd(out, value)
            });
        }
        out.push_str(" if ");
        self.write_non_nil_disjunct(out, value);
        let _ = write!(out, "{target} ~= nil then {target} = ");
        self.write_opnd(out, value);
        out.push_str(" end\n");
    }

    // Nil writes into absent Lua hash slots can repeatedly rehash a full frame.
    // Only compiler-owned plain frame fields take this path; user stores keep
    // their assignment and metamethod behavior. Scoped vN temporaries evaluate
    // every RHS once before any destination changes.
    fn write_frame_assignment(
        &self,
        out: &mut String,
        targets: &[(String, bool)],
        rhs: impl FnOnce(&mut String),
    ) {
        out.push_str(" do local ");
        for i in 0..targets.len() {
            if i > 0 {
                out.push_str(", ");
            }
            write!(out, "v{i}").unwrap();
        }
        out.push_str(" = ");
        rhs(out);
        out.push('\n');
        for (i, (target, frame)) in targets.iter().enumerate() {
            if *frame {
                writeln!(
                    out,
                    "  if v{i} ~= nil or {target} ~= nil then {target} = v{i} end"
                )
                .unwrap();
            } else {
                writeln!(out, "  {target} = v{i}").unwrap();
            }
        }
        out.push_str(" end\n");
    }

    pub(crate) fn write_cells_assignment(
        &self,
        out: &mut String,
        dests: &[Idx<Cell>],
        rhs: impl FnOnce(&mut String),
    ) {
        if dests.iter().any(|c| self.cell_is_frame_field(*c)) {
            let targets: Vec<_> = dests
                .iter()
                .map(|c| {
                    (
                        cellname(self.func, self.frame, self.cell_colors, *c),
                        self.cell_is_frame_field(*c),
                    )
                })
                .collect();
            return self.write_frame_assignment(out, &targets, rhs);
        }
        out.push(' ');
        for (i, c) in dests.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            write_cellname(out, self.func, self.frame, self.cell_colors, *c);
        }
        if !dests.is_empty() {
            out.push_str(" = ");
        }
        rhs(out);
        out.push('\n');
    }

    /// A store of literal nil skips its statically-false `nil ~= nil`
    /// disjunct; the field test still decides.
    pub(crate) fn write_non_nil_disjunct(&self, out: &mut String, value: Idx<Inst>) {
        if !matches!(self.func.insts[value], Inst::Const(Lit::Nil)) {
            self.write_opnd(out, value);
            out.push_str(" ~= nil or ");
        }
    }
}

fn constant_bits(func: &MirFunc, inline: &[bool], id: Idx<Inst>) -> Option<Lit> {
    let bits = |id| {
        if !inline[raw(id) as usize] {
            return None;
        }
        let lit = match func.insts[id] {
            // Wider int literals retain LuaJIT's runtime coercion.
            Inst::Const(Lit::Int(n)) => return i32::try_from(n).ok().map(|n| n as u32),
            Inst::Const(Lit::Uint(n)) => return Some(n),
            _ => constant_bits(func, inline, id)?,
        };
        match lit {
            Lit::Int(n) => Some(n as u32),
            Lit::Uint(n) => Some(n),
            _ => unreachable!("a bitwise constant is an integer"),
        }
    };
    let inst = &func.insts[id];
    let value = match *inst {
        Inst::BitAnd(a, b) | Inst::UintBitAnd(a, b) => bits(a)? & bits(b)?,
        Inst::BitOr(a, b) | Inst::UintBitOr(a, b) => bits(a)? | bits(b)?,
        Inst::BitXor(a, b) | Inst::UintBitXor(a, b) => bits(a)? ^ bits(b)?,
        Inst::BitNot(a) | Inst::UintBitNot(a) => !bits(a)?,
        _ => return None,
    };
    Some(
        if matches!(
            inst,
            Inst::UintBitAnd(..)
                | Inst::UintBitOr(..)
                | Inst::UintBitXor(..)
                | Inst::UintBitNot(..)
        ) {
            Lit::Uint(value)
        } else {
            Lit::Int(i64::from(value as i32))
        },
    )
}

/// Whether a `Format`/`FormatDyn` ends in an `f`/`F` conversion; a dynamic
/// specifier concatenates its pieces with the conversion last.
pub(crate) fn fixed_point_format(func: &MirFunc, format: &Inst) -> bool {
    let (Inst::Format { value, .. } | Inst::FormatDyn { value, .. }) = format else {
        return false;
    };
    match func.insts[*value] {
        Inst::Const(Lit::Int(_) | Lit::Uint(_)) => return false,
        Inst::Const(Lit::Float(x)) if x.abs() >= 1e-100 || x.is_nan() => return false,
        _ => {}
    }
    let mut fmt = match format {
        Inst::Format { fmt, .. } => return fmt.ends_with(['f', 'F']),
        Inst::FormatDyn { fmt, .. } => *fmt,
        _ => return false,
    };
    loop {
        match &func.insts[fmt] {
            Inst::StringConcat(_, last) => fmt = *last,
            Inst::Const(Lit::Str(s)) => return s.ends_with(['f', 'F']),
            _ => return false,
        }
    }
}
