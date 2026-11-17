//! Value expressions, integer and u32 arithmetic, indexing and frame stores.

use super::*;

impl Emitter<'_> {
    pub(crate) fn write_value(&self, out: &mut String, id: Idx<Inst>, inst: &Inst) {
        let instrument =
            self.record_allocation_site(id, emitted_value_allocation_kind(inst), out.len());
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

    /// The RHS expression for a value-producing instruction.
    fn write_value_inner(&self, out: &mut String, id: Idx<Inst>, inst: &Inst) {
        if let Some(lit) = constant_bits(self.func, self.inlinable, id) {
            write_lit(out, &lit);
            return;
        }
        match inst {
            Inst::Const(l) => write_lit(out, l),
            Inst::Global(g) => {
                if let Some(identity) = self.func.code_value_identities.get(&id) {
                    out.push_str("__rock_code_slot_ref(");
                    out.push_str(&lua_string(&identity.logical_key(self.ws, self.interner)));
                    out.push(')');
                } else if self.has_state_slot(*g) {
                    out.push_str(self.ctx.state_table());
                    out.push('[');
                    self.write_state_slot(out, *g);
                    out.push(']');
                } else {
                    out.push_str(
                        self.bound_name(skull_mir::mangle::symbol_of_key(*g))
                            .as_str(),
                    );
                }
            }
            Inst::Load(c) => write_cellname(out, self.func, self.frame, self.cell_colors, *c),

            Inst::IntAdd(a, b) | Inst::FloatAdd(a, b) => self.write_bin(out, "+", a, b),
            Inst::IntSub(a, b) | Inst::FloatSub(a, b) => self.write_bin(out, "-", a, b),
            Inst::IntMul(a, b) | Inst::FloatMul(a, b) => self.write_bin(out, "*", a, b),
            Inst::UintAdd(a, b) => self.write_u32_bin(out, "+", a, b),
            Inst::UintSub(a, b) => self.write_u32_bin(out, "-", a, b),
            Inst::UintMul(a, b) => self.write_u32_mul(out, a, b),
            // Integer `/` is floor-division (rock semantics); float `/` is real.
            Inst::IntDiv(a, b) | Inst::UintDiv(a, b) => {
                out.push_str("math.floor(");
                self.write_opnd(out, *a);
                out.push_str(" / ");
                self.write_opnd(out, *b);
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
            // A proven element index: the 1-based column slot, no check. A
            // folded operand is parenthesized by its consumer.
            Inst::VecIndexSlot(index) => self.write_vec_slot(out, *index),
            // A proven NativeBuffer index is the index itself.
            Inst::NbIndexSlot(index) => self.write_opnd(out, *index),
            Inst::VecStrideFalseToNil(value) => {
                out.push('(');
                self.write_opnd(out, *value);
                out.push_str(" or nil)");
            }
            Inst::StringConcat(a, b) => {
                // A flat concat run lets LuaJIT allocate the result once with BC_CAT.
                let mut parts = Vec::new();
                self.collect_concat(&mut parts, *a);
                self.collect_concat(&mut parts, *b);
                for (i, p) in parts.iter().enumerate() {
                    if i > 0 {
                        out.push_str(" .. ");
                    }
                    self.write_opnd(out, *p);
                }
            }
            Inst::ToString(x) => {
                out.push_str("tostring(");
                self.write_opnd(out, *x);
                out.push(')');
            }
            Inst::Format { value, fmt } => {
                out.push_str("string.format(");
                write_lua_string(out, fmt.as_str());
                out.push_str(", ");
                self.write_opnd(out, *value);
                out.push(')');
            }
            Inst::FormatDyn { value, fmt } => {
                out.push_str("string.format(");
                self.write_opnd(out, *fmt);
                out.push_str(", ");
                self.write_opnd(out, *value);
                out.push(')');
            }
            Inst::FloatToInt(x) => {
                out.push_str("math.floor(");
                self.write_opnd(out, *x);
                out.push(')');
            }
            // These LuaJIT identity conversions must reference their operand:
            // rendering it would evaluate a producer twice.
            Inst::IntToFloat(x) | Inst::UintToFloat(x) | Inst::UintToInt(x) => {
                self.write_opnd(out, *x)
            }
            Inst::FloatToF32(x) => {
                out.push_str("__rock_f32(");
                self.write_opnd(out, *x);
                out.push(')');
            }
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
            Inst::BitNot(x) => {
                out.push_str("bit.bnot(");
                self.write_opnd(out, *x);
                out.push(')');
            }
            Inst::UintBitNot(x) => {
                out.push_str("(bit.bnot(");
                self.write_opnd(out, *x);
                out.push_str(")) % 4294967296");
            }

            Inst::BitAnd(a, b) => self.write_bitfn(out, "bit.band", a, b),
            Inst::BitOr(a, b) => self.write_bitfn(out, "bit.bor", a, b),
            Inst::BitXor(a, b) => self.write_bitfn(out, "bit.bxor", a, b),
            Inst::Shl(a, b) => self.write_bitfn(out, "bit.lshift", a, b),
            Inst::Shr(a, b) => self.write_bitfn(out, "bit.rshift", a, b),
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
                out.push_str("{_data = {");
                self.write_csv(out, vs);
                let _ = write!(out, "}}, len = {}, __ty = \"Vec\"}}", vs.len());
            }
            // Column storage: one array per leaf, sharing the single `len`.
            // Keys are written in brackets because a nested leaf path is dotted
            // (`min.x`), which is not a bare Lua identifier.
            Inst::MakeVecColumns {
                elem,
                columns,
                len,
                capacity,
                ..
            } => {
                out.push_str("{[");
                write_lua_string(out, skull_mir::mir::VEC_COLUMN_HEADER_FIELD);
                out.push_str("] = ");
                self.write_vec_header(out, skull_mir::mir::VEC_COLUMN_REPR, *elem, None, *capacity);
                out.push_str(", ");
                for (name, vs) in columns {
                    out.push('[');
                    write_lua_string(out, name.as_str());
                    out.push_str("] = {");
                    self.write_csv(out, vs);
                    out.push_str("}, ");
                }
                let _ = write!(out, "len = {len}, __ty = \"Vec\"}}");
            }
            Inst::MakeVecColumnHeader { elem, capacity, .. } => {
                self.write_vec_header(out, skull_mir::mir::VEC_COLUMN_REPR, *elem, None, *capacity);
            }
            Inst::MakeVecStrided {
                elem,
                stride,
                values,
                len,
                capacity,
                ..
            } => {
                out.push_str("{_data = {");
                self.write_csv(out, values);
                out.push_str("}, len = ");
                let _ = write!(out, "{len}");
                out.push_str(", __ty = \"Vec\", [");
                write_lua_string(out, skull_mir::mir::VEC_COLUMN_HEADER_FIELD);
                out.push_str("] = ");
                self.write_vec_header(
                    out,
                    skull_mir::mir::VEC_STRIDED_REPR,
                    *elem,
                    Some(*stride),
                    *capacity,
                );
                out.push('}');
            }
            Inst::MakeVecStridedHeader {
                elem,
                stride,
                capacity,
                ..
            } => {
                self.write_vec_header(
                    out,
                    skull_mir::mir::VEC_STRIDED_REPR,
                    *elem,
                    Some(*stride),
                    *capacity,
                );
            }
            Inst::MakeStruct { ty, fields } => {
                out.push('{');
                let mut first = true;
                if let Some(name) = ty {
                    out.push_str("__ty = ");
                    write_lua_string(out, skull_mir::mangle::type_symbol(self.ws, *name).as_str());
                    first = false;
                }
                for (n, id) in fields {
                    if !first {
                        out.push_str(", ");
                    }
                    first = false;
                    write_table_key(out, n.as_str());
                    self.write_opnd(out, *id);
                }
                out.push('}');
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
            } => {
                out.push_str("{__variant = ");
                write_lua_string(out, tag.as_str());
                out.push_str(", __ty = ");
                write_lua_string(out, skull_mir::mangle::type_symbol(self.ws, *ty).as_str());
                for id in positional {
                    out.push_str(", ");
                    self.write_opnd(out, *id);
                }
                for (n, id) in named {
                    let _ = write!(out, ", {n} = ");
                    self.write_opnd(out, *id);
                }
                out.push('}');
            }
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
                    self.repeatable(*vec, 2) && self.repeatable(*index, 2) && self.has_home(id);
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
                out.push_str("__vec_checked_index(");
                self.write_opnd(out, *vec);
                out.push_str(", ");
                self.write_opnd(out, *index);
                out.push(')');
            }
            Inst::Len { base, kind } => match kind {
                ContainerKind::Vec
                | ContainerKind::VecInBounds
                | ContainerKind::VecNonNull
                | ContainerKind::VecNonNullInBounds
                | ContainerKind::VecValueColumns(_)
                | ContainerKind::VecValueStrided(_)
                | ContainerKind::NativeBuffer
                | ContainerKind::NativeBufferInBounds
                | ContainerKind::NativeBufferValue(_) => {
                    self.write_opnd(out, *base);
                    out.push_str(".len");
                }
                // Neither a bare leaf column nor a leaf VIEW has a `len` of its
                // own -- the owning container holds the single length, so
                // lowering asks it there.
                ContainerKind::NativeBufferLeaf => {
                    unreachable!("`len` of a leaf view: lowering must ask the owning buffer")
                }
                ContainerKind::VecColumn | ContainerKind::VecStrideSlot => {
                    out.push('#');
                    self.write_opnd(out, *base);
                }
                ContainerKind::Map => {
                    // No O(1) length on a Map; lowering shouldn't emit this kind.
                    out.push('#');
                    self.write_opnd(out, *base);
                }
                ContainerKind::Record => unreachable!("len of a heap record"),
            },

            // Effects don't produce values.
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

            // Substituted by the inliner (or rejected by mir_emit's residual
            // check) long before codegen.
            Inst::CallBoundMulti { .. } => {
                unreachable!("placeholder bound-call must be substituted before emit")
            }

            // Closure environments hold a CodeRef. Shared mutable capture boxes hold
            // only the capture fields.
            Inst::MakeEnv { fn_ref, fields } => match fn_ref {
                Some(_name) => {
                    out.push_str("setmetatable({ ");
                    let identity = self
                        .func
                        .code_value_identities
                        .get(&id)
                        .expect("Rock closure MakeEnv missing compiler-owned CodeRef identity");
                    out.push_str("__slot = __rock_code_slot_ref(");
                    out.push_str(&lua_string(&identity.logical_key(self.ws, self.interner)));
                    out.push(')');
                    for (f, v) in fields {
                        out.push_str(", ");
                        out.push_str(f.as_str());
                        out.push_str(" = ");
                        self.write_opnd(out, *v);
                    }
                    out.push_str(" }, __rock_fn_mt)");
                }
                None => {
                    out.push('{');
                    for (i, (f, v)) in fields.iter().enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        out.push_str(f.as_str());
                        out.push_str(" = ");
                        self.write_opnd(out, *v);
                    }
                    out.push('}');
                }
            },
        }
    }

    /// `a <op> b` written directly into `out` (no per-operand allocation).
    fn write_bin(&self, out: &mut String, op: &str, a: &Idx<Inst>, b: &Idx<Inst>) {
        self.write_opnd(out, *a);
        out.push(' ');
        out.push_str(op);
        out.push(' ');
        self.write_opnd(out, *b);
    }

    /// `f(a, b)` (bit-library call shape) written directly into `out`.
    fn write_bitfn(&self, out: &mut String, f: &str, a: &Idx<Inst>, b: &Idx<Inst>) {
        out.push_str(f);
        out.push('(');
        self.write_opnd(out, *a);
        out.push_str(", ");
        self.write_opnd(out, *b);
        out.push(')');
    }

    fn write_u32_bin(&self, out: &mut String, op: &str, a: &Idx<Inst>, b: &Idx<Inst>) {
        out.push('(');
        self.write_bin(out, op, a, b);
        out.push_str(") % 4294967296");
    }

    fn write_u32_bitfn(&self, out: &mut String, f: &str, a: &Idx<Inst>, b: &Idx<Inst>) {
        out.push('(');
        self.write_bitfn(out, f, a, b);
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
            out.push_str("__rock_u32_shift_count(");
            self.write_opnd(out, count);
            out.push(')');
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
        out.push_str("__rock_checked_u32(");
        self.write_opnd(out, x);
        out.push(')');
    }

    /// Whether `id` may be written a second time on a path that is rarely
    /// taken: it renders as a bare local or a literal, or as field reads or
    /// one integer sum over those, which have no effect to repeat.
    fn repeatable(&self, id: Idx<Inst>, depth: u8) -> bool {
        if !self.inlinable[raw(id) as usize] || self.hoist.contains_key(&id) {
            return true;
        }
        match self.func.insts[id] {
            Inst::Load(_) | Inst::Const(_) => true,
            Inst::Field { base, .. } | Inst::TupleIndex { base, .. } if depth > 0 => {
                self.repeatable(base, depth - 1)
            }
            Inst::IntAdd(a, b) | Inst::IntSub(a, b) if depth > 0 => {
                self.repeatable(a, 0) && self.repeatable(b, 0)
            }
            _ => false,
        }
    }

    /// Whether value `id` is folded into its use or assigned to a local,
    /// rather than written as a bare statement.
    fn has_home(&self, id: Idx<Inst>) -> bool {
        self.inlinable[raw(id) as usize]
            || self.colors[raw(id) as usize] != u32::MAX
            || regalloc::alias_at(self.alias, id).is_some()
    }

    /// The 1-based backing slot of a 0-based Vec index: a literal is folded,
    /// anything else prints as `<index> + 1`.
    pub(crate) fn write_vec_slot(&self, out: &mut String, index: Idx<Inst>) {
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

    /// READ index `base[index]`. NativeBuffer and Vec go through the
    /// debug-mode bounds-checked stdlib helpers (`__nb_checked_get` /
    /// `__vec_checked_get`) — skull runs debug; release stripping is future
    /// release-mode work. Map and Record index raw (miss returns nil; `.get` is
    /// the checked Map form).
    fn write_index(
        &self,
        out: &mut String,
        id: Idx<Inst>,
        base: Idx<Inst>,
        index: Idx<Inst>,
        kind: ContainerKind,
    ) {
        match kind {
            // A Vec keeps every slot outside `[0, len)` nil, so a slot that
            // reads as non-nil is in bounds and is the answer: no call per
            // element, `false` included. A nil slot is either a nil element or
            // out of bounds, and the helper tells which. A read whose result
            // is discarded is a bare statement, which only the call can be.
            ContainerKind::Vec | ContainerKind::VecNonNull
                if self.repeatable(base, 2) && self.repeatable(index, 2) && self.has_home(id) =>
            {
                for prefix in ["", " or ("] {
                    out.push_str(prefix);
                    self.write_opnd(out, base);
                    out.push_str("._data[");
                    self.write_vec_slot(out, index);
                    out.push(']');
                }
                out.push_str(" == nil and __vec_checked_get(");
                self.write_opnd(out, base);
                out.push_str(", ");
                self.write_opnd(out, index);
                out.push_str("))");
            }
            ContainerKind::Vec | ContainerKind::VecNonNull => {
                out.push_str("__vec_checked_get(");
                self.write_opnd(out, base);
                out.push_str(", ");
                self.write_opnd(out, index);
                out.push(')');
            }
            // The index was proven within `[0, len)`, so the slot holds the
            // element, nil and false included: no fallback.
            ContainerKind::VecInBounds | ContainerKind::VecNonNullInBounds => {
                self.write_opnd(out, base);
                out.push_str("._data[");
                self.write_vec_slot(out, index);
                out.push(']');
            }
            // A whole-element read has no single slot to name -- column storage
            // has no element table. Lowering decomposes every such read into
            // per-column reads (`column_element_repr`), so reaching emit means
            // that decomposition was bypassed. Fail loudly: emitting ANY Lua
            // here would be silently wrong code.
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
                out.push_str("__nb_checked_get(");
                self.write_opnd(out, base);
                out.push_str(", ");
                self.write_opnd(out, index);
                out.push(')');
            }
            // The index was proven within `[0, len)`: the C array, raw.
            ContainerKind::NativeBufferInBounds => {
                self.write_opnd(out, base);
                out.push_str(".ptr[");
                self.write_opnd(out, index);
                out.push(']');
            }
            // A leaf column or stride slot: `index` is already 1-based and
            // checked (`VecCheckedIndex`), so this is a raw array read. A leaf
            // view is a raw C pointer whose ELEMENT access was bounds-checked
            // once already.
            ContainerKind::VecColumn
            | ContainerKind::VecStrideSlot
            | ContainerKind::NativeBufferLeaf
            | ContainerKind::Map => {
                self.write_opnd(out, base);
                out.push('[');
                self.write_opnd(out, index);
                out.push(']');
            }
            ContainerKind::Record => {
                self.write_opnd(out, base);
                if let Inst::Const(Lit::Str(field)) = &self.func.insts[index] {
                    write_field_key(out, field.as_str());
                } else {
                    out.push('[');
                    self.write_opnd(out, index);
                    out.push(']');
                }
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

    /// A single frame-field store. The nil-write guard goes when the value
    /// cannot be nil; a guarded plain operand reads twice instead of paying a
    /// `do local v0` evaluation scope; only a guarded non-plain RHS keeps the
    /// temporary so it evaluates once. A literal nil drops its statically
    /// false `v ~= nil` disjunct.
    pub(crate) fn write_single_frame_assignment(
        &self,
        out: &mut String,
        target: &str,
        value: Idx<Inst>,
    ) {
        if self.never_nil(value) {
            let _ = write!(out, " {target} = ");
            self.write_opnd(out, value);
            out.push('\n');
            return;
        }
        if !self.opnd_is_plain(value) {
            return self.write_frame_assignment(out, &[(target.to_owned(), true)], |out| {
                self.write_opnd(out, value)
            });
        }
        out.push_str(" if ");
        if !matches!(self.func.insts[value], Inst::Const(Lit::Nil)) {
            self.write_opnd(out, value);
            out.push_str(" ~= nil or ");
        }
        let _ = write!(out, "{target} ~= nil then {target} = ");
        self.write_opnd(out, value);
        out.push_str(" end\n");
    }

    // Nil writes into absent Lua hash slots can repeatedly rehash a full frame.
    // Only compiler-owned plain frame fields use this path; user stores retain
    // their ordinary assignment and metamethod behavior. Scoped vN temporaries
    // evaluate every RHS once before any destination changes.
    pub(crate) fn write_frame_assignment(
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

    /// `d0, d1, ... = <rhs>` into cells, through [`Self::write_frame_assignment`]
    /// when any destination is a frame field. No destinations leave a bare
    /// `<rhs>` statement.
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
