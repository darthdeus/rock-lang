//! Instruction expressions, value-struct forms and calls.

use super::*;

impl Emitter<'_> {
    /// Render an inst's own JS expression (its operands routed through
    /// [`Self::render_val`], so a non-inlinable operand reads its temp). JS is
    /// untyped, so the int/float split collapses to one operator and
    /// `IntToFloat` is identity.
    pub(super) fn render_inst_expr(&self, iid: Idx<Inst>) -> Result<String, CompilerError> {
        let bin =
            |a: &Idx<Inst>, op: &str, b: &Idx<Inst>, s: &Self| -> Result<String, CompilerError> {
                Ok(format!(
                    "({} {} {})",
                    s.render_val(*a)?,
                    op,
                    s.render_val(*b)?
                ))
            };
        Ok(match &self.mf.insts[iid] {
            Inst::Const(Lit::Int(i)) => i.to_string(),
            Inst::Const(Lit::Uint(i)) => i.to_string(),
            Inst::Const(Lit::Float(f)) => js_number(*f)?,
            Inst::Const(Lit::Bool(b)) => b.to_string(),
            Inst::Const(Lit::Nil) => "null".to_string(),
            Inst::Const(Lit::Omitted) => OMITTED_ARG.to_string(),
            Inst::Const(Lit::Str(s)) => js_string_lit(s.as_str())?,
            Inst::Load(c) => cell_name(self.mf, *c),
            Inst::IntAdd(a, b) | Inst::FloatAdd(a, b) => bin(a, "+", b, self)?,
            Inst::IntSub(a, b) | Inst::FloatSub(a, b) => bin(a, "-", b, self)?,
            Inst::IntMul(a, b) | Inst::FloatMul(a, b) => bin(a, "*", b, self)?,
            Inst::UintAdd(a, b) => format!(
                "(({} + {}) >>> 0)",
                self.render_val(*a)?,
                self.render_val(*b)?
            ),
            Inst::UintSub(a, b) => format!(
                "(({} - {}) >>> 0)",
                self.render_val(*a)?,
                self.render_val(*b)?
            ),
            Inst::UintMul(a, b) => format!(
                "(Math.imul({}, {}) >>> 0)",
                self.render_val(*a)?,
                self.render_val(*b)?
            ),
            // Both operands are already in uint range, so only a zero divisor
            // can leave it, and LuaJIT yields inf/nan there rather than wrapping.
            Inst::UintDiv(a, b) => format!(
                "Math.floor({} / {})",
                self.render_val(*a)?,
                self.render_val(*b)?
            ),
            Inst::UintMod(a, b) => bin(a, "%", b, self)?,
            Inst::FloatDiv(a, b) => bin(a, "/", b, self)?,
            // Rock int `/` floors toward -inf (Lua); JS `/` is real division.
            Inst::IntDiv(a, b) => format!(
                "Math.floor({} / {})",
                self.render_val(*a)?,
                self.render_val(*b)?
            ),
            Inst::IntLt(a, b) | Inst::FloatLt(a, b) => bin(a, "<", b, self)?,
            Inst::IntLeq(a, b) | Inst::FloatLeq(a, b) => bin(a, "<=", b, self)?,
            Inst::IntGt(a, b) | Inst::FloatGt(a, b) => bin(a, ">", b, self)?,
            Inst::IntGeq(a, b) | Inst::FloatGeq(a, b) => bin(a, ">=", b, self)?,
            Inst::UintLt(a, b) => bin(a, "<", b, self)?,
            Inst::UintLeq(a, b) => bin(a, "<=", b, self)?,
            Inst::UintGt(a, b) => bin(a, ">", b, self)?,
            Inst::UintGeq(a, b) => bin(a, ">=", b, self)?,
            // Loose `== null` / `!= null` when either side is `nil`: it matches
            // JS `undefined` too, so an OMITTED defaulted arg (JS passes nothing
            // -> `undefined`, not `null`) still trips the callee's `x == nil`
            // default-fill guard, exactly as Lua's absent-arg `nil` does.
            Inst::Eq(a, b) if self.is_nil(*a) || self.is_nil(*b) => bin(a, "==", b, self)?,
            Inst::NotEq(a, b) if self.is_nil(*a) || self.is_nil(*b) => bin(a, "!=", b, self)?,
            Inst::Eq(a, b) => bin(a, "===", b, self)?,
            Inst::NotEq(a, b) => bin(a, "!==", b, self)?,
            // Space after `-`: `Neg` of a negative literal would otherwise emit
            // `(--1.5)`, which JS lexes as the `--` decrement operator.
            Inst::Neg(x) => format!("(- {})", self.render_val(*x)?),
            Inst::Not(x) => format!("(!{})", self.render_val(*x)?),
            // Bitwise: luajit lowers these to `bit.*`, whose results are SIGNED
            // 32-bit; JS `& | ^ ~ <<` are signed 32-bit too, and both sides
            // coerce to int32 and mask the shift count to 5 bits, so those map
            // straight across. `bit.rshift` is the LOGICAL shift, which is JS
            // `>>>` -- but `>>>` yields an UNSIGNED 32-bit value, so `| 0`
            // brings it back to the signed range luajit reports
            // (`bit.rshift(-1, 0)` is -1, not 4294967295).
            Inst::BitAnd(a, b) => bin(a, "&", b, self)?,
            Inst::BitOr(a, b) => bin(a, "|", b, self)?,
            Inst::BitXor(a, b) => bin(a, "^", b, self)?,
            Inst::Shl(a, b) => bin(a, "<<", b, self)?,
            Inst::Shr(a, b) => {
                format!(
                    "(({} >>> {}) | 0)",
                    self.render_val(*a)?,
                    self.render_val(*b)?
                )
            }
            Inst::BitNot(x) => format!("(~{})", self.render_val(*x)?),
            Inst::UintBitNot(x) => format!("((~{}) >>> 0)", self.render_val(*x)?),
            Inst::UintBitAnd(a, b) => format!(
                "(({} & {}) >>> 0)",
                self.render_val(*a)?,
                self.render_val(*b)?
            ),
            Inst::UintBitOr(a, b) => format!(
                "(({} | {}) >>> 0)",
                self.render_val(*a)?,
                self.render_val(*b)?
            ),
            Inst::UintBitXor(a, b) => format!(
                "(({} ^ {}) >>> 0)",
                self.render_val(*a)?,
                self.render_val(*b)?
            ),
            Inst::UintShl(a, b) => self.checked_uint_shift(*a, *b, "<<")?,
            Inst::UintShr(a, b) => self.checked_uint_shift(*a, *b, ">>>")?,
            // Identity on JS (all doubles), like codegen-lua on luajit.
            Inst::IntToFloat(x) => self.render_val(*x)?,
            Inst::UintToFloat(x) => self.render_val(*x)?,
            Inst::FloatToF32(x) => format!("Math.fround({})", self.render_val(*x)?),
            Inst::IntToUint(x) | Inst::FloatToUint(x) => {
                let value = self.render_val(*x)?;
                format!(
                    "((value) => (value >= 0 && value <= 4294967295 ? Math.trunc(value) : (() => {{ throw new Error(\"uint conversion out of range\"); }})()))({value})"
                )
            }
            Inst::UintToInt(x) => self.render_val(*x)?,
            Inst::UintBitcastInt(x) => format!("({} | 0)", self.render_val(*x)?),
            Inst::IntBitcastUint(x) => format!("({} >>> 0)", self.render_val(*x)?),
            Inst::VecStrideNilToFalse(x) => {
                let value = self.render_val(*x)?;
                format!("((value) => value == null ? false : value)({value})")
            }
            Inst::VecStrideFalseToNil(x) => {
                let value = self.render_val(*x)?;
                format!("((value) => value === false ? null : value)({value})")
            }
            // Rock float->int truncates toward -inf == floor.
            Inst::FloatToInt(x) => format!("Math.floor({})", self.render_val(*x)?),
            // Lua `..` coerces each operand via `tostring` (number formatting is
            // `%.14g`); JS `+` does not, so every leaf routes through
            // `lua_tostring` for parity. Emitted as one flat `+` run over the
            // whole concat tree (mirrors the Lua backend's flat `..`).
            Inst::StringConcat(a, b) => {
                let mut parts = Vec::new();
                self.collect_concat(&mut parts, *a);
                self.collect_concat(&mut parts, *b);
                let rendered: Result<Vec<String>, CompilerError> = parts
                    .iter()
                    .map(|p| Ok(format!("lua_tostring({})", self.render_val(*p)?)))
                    .collect();
                format!("({})", rendered?.join(" + "))
            }
            // The scalar `{x}` interpolation stringify (int/bool/string) -- same
            // `%.14g`-parity path as a concat leaf.
            Inst::ToString(x) => format!("lua_tostring({})", self.render_val(*x)?),
            // Rock `%` is LuaJIT's `a - floor(a / b) * b`, computed the same way
            // so rounding and the sign of a zero agree; JS `%` truncates. Each
            // operand appears twice, so an inlined expression binds once.
            Inst::IntMod(a, b) | Inst::FloatMod(a, b) => {
                let (ra, rb) = (self.render_val(*a)?, self.render_val(*b)?);
                if self.is_plain_operand(*a) && self.is_plain_operand(*b) {
                    format!("({ra} - Math.floor({ra} / {rb}) * {rb})")
                } else {
                    format!("((a, b) => a - Math.floor(a / b) * b)({ra}, {rb})")
                }
            }
            Inst::Call { func, args } => self.render_call(iid, *func, args)?,
            // A capture-free hoisted closure resolves its reusable logical slot.
            // Any OTHER global read (module `let`, enum name table, named fn as a
            // value) is not emitted, so it stays out of subset. `error` is the
            // one runtime global: the coroutine transform's layout raise and
            // `await`'s re-raise read it as a value (`JS_CORO_RUNTIME` defines it).
            Inst::Global(name) if self.mf.code_value_identities.contains_key(&iid) => {
                let identity = &self.mf.code_value_identities[&iid];
                let key = self
                    .globals
                    .code_slots
                    .get(identity)
                    .ok_or_else(|| err("JS: Rock function value has no CodeRef slot"))?;
                format!("__rock_code_slot_ref({})", js_string_lit(key)?)
            }
            Inst::Global(name)
                if self.hoisted.contains(name)
                    || name.as_str() == "error"
                    || is_runtime_global(name) =>
            {
                name.to_string()
            }
            // A boundary function referenced as a value: its co-emitted JS name.
            Inst::Global(name) if self.globals.fns.contains_key(name) => {
                self.globals.fns[name].clone()
            }
            Inst::Global(name) if self.globals.const_globals.contains(name) => {
                js_member("__G", name.as_str())?
            }
            // Module-level `let`/`global` state: scoped so no body local can
            // collide with or shadow it.
            Inst::Global(name) => match self.globals.state.get(&symbol_of_key(*name)) {
                Some(slot) => {
                    self.globals.uses_storage.set(true);
                    format!("__rock_globals[{}]", js_string_lit(slot)?)
                }
                None if self.globals.enum_globals.contains(name) => name.to_string(),
                None => {
                    return Err(err(format!(
                        "JS subset: global read `{name}` not supported"
                    )));
                }
            },
            // A CodeRef env record `{ __slot, c<A>.. }` (capturing lambda) or a
            // box `{ v }` (a shared mutable capture) -- built by `convert_closures`.
            Inst::MakeEnv { fn_ref, fields } => {
                let mut parts: Vec<String> = Vec::new();
                if let Some(_name) = fn_ref {
                    if let Some(identity) = self.mf.code_value_identities.get(&iid) {
                        let key = self
                            .globals
                            .code_slots
                            .get(identity)
                            .ok_or_else(|| err("JS: Rock closure has no CodeRef slot"))?;
                        parts.push(format!(
                            "__slot: __rock_code_slot_ref({})",
                            js_string_lit(key)?
                        ));
                    } else {
                        return Err(err(
                            "JS: Rock closure missing compiler-owned CodeRef identity",
                        ));
                    }
                }
                for (f, v) in fields {
                    parts.push(format!("{f}: {}", self.render_val(*v)?));
                }
                format!("({{{}}})", parts.join(", "))
            }
            // The runtime is plain JS: no CodeRef dispatch.
            Inst::CallValue { callee, args } if self.is_runtime_value(*callee) => {
                let rendered = args
                    .iter()
                    .map(|a| self.render_val(*a))
                    .collect::<Result<Vec<_>, _>>()?;
                format!("{}({})", self.render_val(*callee)?, rendered.join(", "))
            }
            // An indirect call: foreign raw functions call directly; Rock values
            // resolve their slot through `__rock_call`.
            Inst::CallValue { callee, args } => {
                let mut rendered = vec![self.render_val(*callee)?];
                for a in args {
                    rendered.push(self.render_val(*a)?);
                }
                format!("__rock_call({})", rendered.join(", "))
            }
            // NativeBuffer variants share the equality runtime's buffer tag.
            Inst::MakeStruct { ty, fields } => {
                let name = ty.map(|t| t.to_string()).unwrap_or_default();
                let mut parts: Vec<String> = Vec::new();
                if !name.is_empty() {
                    let tag = if is_native_buffer(&name) {
                        "NativeBuffer"
                    } else {
                        &name
                    };
                    parts.push(format!("__ty: {}", js_string_lit(tag)?));
                }
                for (f, v) in fields {
                    parts.push(format!("{}: {}", js_key(f)?, self.render_val(*v)?));
                }
                format!("({{{}}})", parts.join(", "))
            }
            // A `value` struct is byte-value cdata on luajit; on JS it is a plain
            // `{__ty, ...}` object, same shape as a reference struct. An all-scalar
            // (GC-free) one compares via the generic structural deep-eq
            // (`JS_STRUCTURAL_EQ`), matching luajit's byte-value eq. A GC-carrying
            // one compares its pointer fields BY REFERENCE on luajit, which the
            // generic contents-eq cannot reproduce; the per-type `__rock_vt_eq`
            // registry (`build_vt_eq_defs`) supplies that split and `__op_eq`
            // consults it before the generic walk -- mirroring luajit's `_vt.__eq`
            // metatable-authority rule. Construction is identical for both tiers.
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
                let arena = self.mf.rep_arena.as_ref().ok_or_else(|| {
                    err(format!(
                        "JS subset: `{name}` value constructor has no representation arena"
                    ))
                })?;
                let plan = arena.try_get(*representation).ok_or_else(|| {
                    err(format!(
                        "JS subset: `{name}` value constructor has a foreign representation plan"
                    ))
                })?;
                if matches!(form, skull_mir::mir::ValueCtorForm::Leaves) {
                    let canonical = plan.leaf_paths().ok_or_else(|| {
                        err(format!(
                            "JS subset: `{name}` leaf-form constructor has no leaf paths"
                        ))
                    })?;
                    let got: Vec<ustr::Ustr> = fields.iter().map(|(f, _)| *f).collect();
                    if got != canonical {
                        return Err(err(format!(
                            "`{name}` leaf-form constructor does not match its supplied plan: {got:?} vs {canonical:?}"
                        )));
                    }
                    return self.render_leaf_form_value_struct(*name, *representation, fields);
                }
                let mut parts = vec![format!("__ty: {}", js_string_lit(name.as_ref())?)];
                for (f, v) in fields {
                    parts.push(format!("{}: {}", js_key(f)?, self.render_val(*v)?));
                }
                format!("({{{}}})", parts.join(", "))
            }
            // A non-cdata enum boxes as `{__variant, __ty, 0:.., 1:.., <name>:..}`
            // via the nil-dropping ctor (`JS_ENUM_RUNTIME`). Positional payloads take
            // native 0-based keys (luajit's 1-based table keys are Lua table-ctor
            // semantics; `TupleIndex` reads `base[index]` uniformly here). A unit
            // variant of a mixed enum is its interned singleton (an `Inst::Global`),
            // so a bare `MakeEnum` here always carries payload.
            Inst::MakeEnum {
                tag,
                ty,
                positional,
                named,
            } => self.render_make_enum(&js_string_lit(tag.as_str())?, ty, positional, named)?,
            Inst::MakeEnumDyn {
                tag,
                ty,
                positional,
                named,
            } => self.render_make_enum(&self.render_val(*tag)?, ty, positional, named)?,
            // A tuple is a plain JS array; `.N` reads index N. (Most tuples are
            // scalarized by SROA before emission; these cover the residue.)
            Inst::MakeTuple(elems) => {
                let rendered: Result<Vec<String>, CompilerError> =
                    elems.iter().map(|e| self.render_val(*e)).collect();
                format!("[{}]", rendered?.join(", "))
            }
            Inst::TupleIndex { base, index } => {
                format!("{}[{index}]", self.render_val(*base)?)
            }
            // A Vec literal: `{_data, len, __ty}` with `_data` a 0-based dense
            // array (the JS-native repr; the base-agnostic `__vec_data_*`
            // accessors read `_data[i]`).
            Inst::MakeVec(elems) => {
                let rendered: Result<Vec<String>, CompilerError> =
                    elems.iter().map(|e| self.render_val(*e)).collect();
                let rendered = rendered?;
                let data = format!("[{}]", rendered.join(", "));
                format!("({{_data: {data}, len: {}, __ty: \"Vec\"}})", elems.len())
            }
            Inst::Index { base, index, kind } => {
                let (b, i) = (self.render_val(*base)?, self.render_val(*index)?);
                match kind {
                    ContainerKind::NativeBuffer => format!("__nb_checked_get({b}, {i})"),
                    // Lowering decomposes every whole-element read into per-leaf
                    // reads (`nb_leaf_element_repr`), so reaching emit means the
                    // element has no C leaf layout to address.
                    ContainerKind::NativeBufferValue(elem) => {
                        return Err(err(format!(
                            "JS subset: whole-element read of `NativeBuffer<{elem}>` reached \
                             emit -- lowering must decompose it into leaf reads"
                        )));
                    }
                    ContainerKind::Vec => format!("__vec_checked_get({b}, {i})"),
                    // A whole-element read has no single slot to name: column
                    // storage has no element object. Lowering decomposes every
                    // such read into per-column reads (`column_element_repr`),
                    // so reaching emit means that decomposition was bypassed.
                    ContainerKind::VecValueColumns(elem) => {
                        return Err(err(format!(
                            "JS subset: whole-element read of column-stored `[{elem}]` reached \
                             emit -- lowering must decompose it into per-column reads"
                        )));
                    }
                    ContainerKind::VecValueStrided(elem) => {
                        return Err(err(format!(
                            "JS subset: whole-element read of strided `[{elem}]` reached emit"
                        )));
                    }
                    // One leaf column: the index is already checked
                    // (`VecCheckedIndex`) and already 0-based, so this is raw.
                    ContainerKind::VecColumn | ContainerKind::VecStrideSlot => format!("{b}[{i}]"),
                    // One typed leaf view. The index is absolute and already
                    // bounds-checked once per ELEMENT, so this indexes raw. A
                    // `bool` view is a Uint8Array holding 0/1, which the language
                    // wants back as a boolean.
                    ContainerKind::NativeBufferLeaf => {
                        if self.leaf_view_is_bool(*base) {
                            format!("({b}[{i}] !== 0)")
                        } else {
                            format!("{b}[{i}]")
                        }
                    }
                    // Keyed MIR reads also address reflected struct leaves.
                    // Evaluate the owner and selector once in either case.
                    ContainerKind::Map => format!(
                        "(function(t, k) {{ return t instanceof Map ? t.get(k) : t[k]; }})({b}, {i})"
                    ),
                }
            }
            Inst::Len { base, kind } => match kind {
                ContainerKind::NativeBuffer
                | ContainerKind::NativeBufferValue(_)
                | ContainerKind::Vec
                | ContainerKind::VecValueColumns(_)
                | ContainerKind::VecValueStrided(_) => {
                    format!("{}.len", self.render_val(*base)?)
                }
                ContainerKind::VecColumn | ContainerKind::VecStrideSlot => {
                    format!("{}.length", self.render_val(*base)?)
                }
                // A view has no length of its own: it spans the whole buffer, and
                // only the owning NativeBuffer knows the element count. Reaching
                // here means a `len` was taken on the wrong base.
                ContainerKind::NativeBufferLeaf => {
                    return Err(err("JS subset: `len` of a NativeBuffer leaf view"));
                }
                ContainerKind::Map => format!("{}.size", self.render_val(*base)?),
            },
            // The `native_buffer_alloc<T>` primitive lowers to this MIR node
            // (skull-mir intercepts the call and resolves the element ctype).
            // `Ptr<T>` IS a TypedArray on JS, matching the luajit `ffi.new` cdata;
            // value-struct elements use the shared flat C layout below.
            Inst::FfiNewArray {
                ctype,
                count,
                representation,
            } => match ctype.as_str() {
                "float" => format!("new Float32Array({})", self.render_val(*count)?),
                "int" => format!("new Int32Array({})", self.render_val(*count)?),
                "uint32_t" => format!("new Uint32Array({})", self.render_val(*count)?),
                "bool" => format!("new Uint8Array({})", self.render_val(*count)?),
                // A value struct has no per-struct ctype on JS: back it onto one
                // flat `Float32Array(n * size/4)` laid out in the SAME C bytes
                // luajit uses, which the per-leaf views (`FfiViewCast`) then alias
                // through `.buffer`. For an all-float element `size/4` IS the float
                // leaf count, so an all-float buffer's storage and its `.ptr`
                // handed to a host API are unchanged.
                other => {
                    let representation = representation.ok_or_else(|| {
                        err(format!(
                            "JS subset: native_buffer_alloc element ctype `{other}` has no supplied representation plan"
                        ))
                    })?;
                    let arena = self.mf.rep_arena.as_ref().ok_or_else(|| {
                        err(format!(
                            "JS subset: native_buffer_alloc element ctype `{other}` has no representation arena"
                        ))
                    })?;
                    let plan = arena.try_get(representation).ok_or_else(|| {
                        err(format!(
                            "JS subset: native_buffer_alloc element ctype `{other}` has a foreign representation plan"
                        ))
                    })?;
                    let size = arena.leaf_size(self.ws, representation).map_err(|e| {
                        err(format!(
                            "JS subset: native_buffer_alloc element ctype `{other}` has no C \
                             leaf layout to back a TypedArray with ({})",
                            e.message
                        ))
                    })?;
                    if plan.value_storage() != Some(skull_mir::lower::ValueStorage::CData) {
                        return Err(err(format!(
                            "JS subset: native_buffer_alloc element ctype `{other}` is not a CData representation"
                        )));
                    }
                    // A 4-byte-backed array needs a 4-byte multiple. C gives that
                    // whenever any leaf is 4-wide; an all-`bool` element would not,
                    // and rejects rather than silently reinterpreting its bytes.
                    if size % 4 != 0 {
                        return Err(err(format!(
                            "JS subset: native_buffer_alloc element ctype `{other}` is {size} \
                             bytes, not a multiple of 4 -- no aligned TypedArray backing"
                        )));
                    }
                    // The backing view uses float32 slots regardless of the
                    // element's C width. Preserve that width for byte equality.
                    format!(
                        "Object.assign(new Float32Array(({}) * {}), {{__rock_elem_size: {size}}})",
                        self.render_val(*count)?,
                        size / 4
                    )
                }
            },
            // One typed view over the buffer's storage. luajit casts the pointer;
            // JS makes a TypedArray over the same `ArrayBuffer`, so the views and
            // `.ptr` alias the same bytes -- which is what lets leaf-addressed and
            // whole-`.ptr` (host upload) access to one buffer stay coherent.
            Inst::FfiViewCast { cty, ptr } => {
                let p = self.render_val(*ptr)?;
                let array = match cty.as_str() {
                    "double" => "Float64Array",
                    "float" => "Float32Array",
                    "int" => "Int32Array",
                    "uint32_t" => "Uint32Array",
                    // C `bool` is one byte. Reads coerce back to a JS boolean at
                    // the leaf `Index` (`!== 0`); writes coerce implicitly.
                    "bool" => "Uint8Array",
                    other => {
                        return Err(err(format!(
                            "JS subset: no TypedArray for leaf view C type `{other}`"
                        )));
                    }
                };
                format!("new {array}({p}.buffer, {p}.byteOffset)")
            }
            // A general field read is JS property access. NativeBuffer `.ptr`/
            // `.len`, a leaf VIEW field and Vec `._data`/`.len` are all the same
            // shape; a read THROUGH a buffer element is decomposed by lowering.
            Inst::Field { base, field } => js_member(&self.render_val(*base)?, field)?,
            // Presence is a bool on the scalar ABI, so JS truthiness agrees with
            // luajit's `present and value or nil`.
            Inst::MakeNullableValue { present, value } => format!(
                "({} ? {} : null)",
                self.render_val(*present)?,
                self.render_val(*value)?
            ),
            // `{x:spec}` interpolation: `string.format(fmt, value)` for a single
            // value (Lua path). `__rock_sprintf` is the single-value C printf port
            // (d/i/u/x/X/o/f/F/e/E/g/G/s/c, flags, width, precision; half-even
            // rounding). Format's fmt is a static string, FormatDyn's is computed.
            Inst::Format { value, fmt } => {
                format!(
                    "__rock_sprintf({}, {})",
                    js_string_lit(fmt.as_str())?,
                    self.render_val(*value)?
                )
            }
            Inst::FormatDyn { value, fmt } => {
                format!(
                    "__rock_sprintf({}, {})",
                    self.render_val(*fmt)?,
                    self.render_val(*value)?
                )
            }
            // Column storage: one plain array per leaf, sharing the single
            // `len`. Plain arrays rather than TypedArrays on purpose -- V8's
            // packed-double arrays beat `Float64Array` for the strided reads
            // this layout exists to make fast.
            // A nested leaf path is dotted (`min.x`), so keys are quoted.
            Inst::MakeVecColumns {
                elem,
                columns,
                len,
                capacity,
                ..
            } => {
                let mut s = format!(
                    "{{{}: {{repr: {}, elem: {}, cap: {}}}",
                    js_string_lit(skull_mir::mir::VEC_COLUMN_HEADER_FIELD)?,
                    js_string_lit(skull_mir::mir::VEC_COLUMN_REPR)?,
                    js_string_lit(elem.as_str())?,
                    self.render_val(*capacity)?,
                );
                for (name, vs) in columns {
                    let vals = vs
                        .iter()
                        .map(|v| self.render_val(*v))
                        .collect::<Result<Vec<_>, _>>()?;
                    s.push_str(&format!(
                        ", {}: [{}]",
                        js_string_lit(name.as_str())?,
                        vals.join(", ")
                    ));
                }
                s.push_str(&format!(", len: {len}, __ty: \"Vec\"}}"));
                s
            }
            Inst::MakeVecColumnHeader { elem, capacity, .. } => format!(
                "{{repr: {}, elem: {}, cap: {}}}",
                js_string_lit(skull_mir::mir::VEC_COLUMN_REPR)?,
                js_string_lit(elem.as_str())?,
                self.render_val(*capacity)?,
            ),
            Inst::MakeVecStrided {
                elem,
                stride,
                values,
                len,
                capacity,
                ..
            } => {
                let values = values
                    .iter()
                    .map(|value| self.render_val(*value))
                    .collect::<Result<Vec<_>, _>>()?;
                format!(
                    "{{_data: [{}], len: {len}, __ty: \"Vec\", {}: {{repr: {}, elem: {}, stride: {stride}, cap: {}}}}}",
                    values.join(", "),
                    js_string_lit(skull_mir::mir::VEC_COLUMN_HEADER_FIELD)?,
                    js_string_lit(skull_mir::mir::VEC_STRIDED_REPR)?,
                    js_string_lit(elem.as_str())?,
                    self.render_val(*capacity)?,
                )
            }
            Inst::MakeVecStridedHeader {
                elem,
                stride,
                capacity,
                ..
            } => format!(
                "{{repr: {}, elem: {}, stride: {stride}, cap: {}}}",
                js_string_lit(skull_mir::mir::VEC_STRIDED_REPR)?,
                js_string_lit(elem.as_str())?,
                self.render_val(*capacity)?,
            ),
            // The element index is checked ONCE here; each per-leaf access then
            // indexes its column raw.
            Inst::VecCheckedIndex { vec, index } => format!(
                "__vec_checked_index({}, {})",
                self.render_val(*vec)?,
                self.render_val(*index)?
            ),
            other => {
                return Err(err(format!(
                    "JS subset: cannot render {other:?} as a JS expression"
                )));
            }
        })
    }

    /// Flatten a `StringConcat` tree into its leaf operands, descending only into
    /// child `StringConcat`s folded into this expression. A materialized child
    /// stays a leaf: its operands may be `let`s of another switch case.
    fn collect_concat(&self, parts: &mut Vec<Idx<Inst>>, id: Idx<Inst>) {
        if self.inlinable[raw(id) as usize] {
            if let Inst::StringConcat(a, b) = &self.mf.insts[id] {
                self.collect_concat(parts, *a);
                self.collect_concat(parts, *b);
                return;
            }
        }
        parts.push(id);
    }

    /// Box an enum: `__rock_make_enum(<tag>, "<ty>", [0, p0, 1, p1, "name", v, ..])`.
    /// `tag_expr` is a rendered JS expression (a string literal for `MakeEnum`, the
    /// dynamic tag operand for `MakeEnumDyn`). The ctor drops nil `kv` slots so a
    /// union-wide rebox owns exactly the active variant's keys.
    /// Rebuild the declared nested-object shape from a flattened leaf field list
    /// (`("pos.x", ..), ("pos.y", ..)` -> `{pos: {__ty:"NV2", x:.., y:..}}`).
    /// The leaf form exists only between the end of `cleanup` and emit; luajit
    /// consumes it via a generated leaf ctor, JS has to reassemble.
    /// The struct type of `name`'s field `head`, when that field is itself a
    /// value struct -- so a rebuilt sub-object carries the `__ty` the non-leaf
    /// path would give it, and the inspect renderer can recurse into a
    /// flattened group. Fails closed: a non-struct field yields `None`.
    fn nested_field_struct_name(
        &self,
        name: ustr::Ustr,
        head: &str,
    ) -> Result<Option<ustr::Ustr>, CompilerError> {
        let sid = self
            .ws
            .struct_by_name
            .get(&name)
            .ok_or_else(|| err(format!("leaf-form value `{name}` has no declared struct")))?;
        let sd = &self.ws.modules[sid.module.index()].structs[sid.def];
        let Some(f) = sd.fields.iter().find(|f| f.name.as_str() == head) else {
            return Err(err(format!("struct `{name}` has no field `{head}`")));
        };
        let plan = skull_mir::lower::struct_field_plan(
            self.ws,
            bone_core::hir::FnBackend::Js,
            name,
            f.name,
            false,
        )
        .map_err(|e| err(e.message))?;
        Ok(plan
            .child()
            .filter(|child| self.ws.struct_by_name.contains_key(child)))
    }

    /// Split a possibly-FLATTENED field list into exact fields and per-head
    /// groups of dotted leaves. Shared by the leaf-form object renderer and the
    /// inspect renderer so the two read the same shape the same way.
    fn split_leaf_fields(
        fields: &[(ustr::Ustr, Idx<Inst>)],
    ) -> (
        HashMap<ustr::Ustr, Idx<Inst>>,
        Vec<(String, Vec<(ustr::Ustr, Idx<Inst>)>)>,
    ) {
        let mut exact: HashMap<ustr::Ustr, Idx<Inst>> = HashMap::new();
        let mut groups: Vec<(String, Vec<(ustr::Ustr, Idx<Inst>)>)> = Vec::new();
        for (f, v) in fields {
            match f.as_str().split_once('.') {
                None => {
                    exact.insert(*f, *v);
                }
                Some((head, rest)) => {
                    let rest = ustr::Ustr::from(rest);
                    match groups.iter_mut().find(|(h, _)| h == head) {
                        Some((_, sub)) => sub.push((rest, *v)),
                        None => groups.push((head.to_string(), vec![(rest, *v)])),
                    }
                }
            }
        }
        (exact, groups)
    }

    /// A value constructor that arrives as its leaf paths (`pos.x`, `pos.y`, a
    /// presence tag at an inline child's own name). A CData value's box is a
    /// plain nested JS object -- its dotted paths are only the luajit byte
    /// layout -- so a child's leaves are rebuilt as its own box under the field
    /// name. A TABLE value's box keys the leaf paths themselves, so the same
    /// leaves stay flat: that is where the lowering reads `pos.x` from, and a
    /// nested child under `pos` leaves every read through it undefined.
    pub(super) fn render_leaf_form_value_struct(
        &self,
        name: ustr::Ustr,
        representation: skull_mir::lower::RepId,
        fields: &[(ustr::Ustr, Idx<Inst>)],
    ) -> Result<String, CompilerError> {
        let arena = self.mf.rep_arena.as_ref().ok_or_else(|| {
            err(format!(
                "JS subset: `{name}` value constructor has no representation arena"
            ))
        })?;
        let cdata = arena
            .try_get(representation)
            .and_then(|plan| plan.value_storage())
            == Some(skull_mir::lower::ValueStorage::CData);
        if !cdata {
            let mut parts = vec![format!("__ty: {}", js_string_lit(name.as_ref())?)];
            for (key, value) in fields {
                parts.push(format!("{}: {}", js_key(key)?, self.render_val(*value)?));
            }
            return Ok(format!("({{{}}})", parts.join(", ")));
        }
        // Nested groups in first-seen order, so the emitted object matches the
        // declared field order the non-leaf path produces.
        let mut top: Vec<(ustr::Ustr, String)> = Vec::new();
        let mut groups: Vec<(ustr::Ustr, Vec<(ustr::Ustr, Idx<Inst>)>)> = Vec::new();
        for (key, value) in fields {
            match key.as_str().split_once('.') {
                None => top.push((*key, self.render_val(*value)?)),
                Some((head, rest)) => {
                    let head = ustr::Ustr::from(head);
                    let rest = ustr::Ustr::from(rest);
                    match groups.iter_mut().find(|(h, _)| *h == head) {
                        Some((_, sub)) => sub.push((rest, *value)),
                        None => groups.push((head, vec![(rest, *value)])),
                    }
                }
            }
        }
        let mut parts = vec![format!("__ty: {}", js_string_lit(name.as_ref())?)];
        for (key, rendered) in &top {
            parts.push(format!("{}: {rendered}", js_key(key)?));
        }
        for (head, sub) in &groups {
            let child = self
                .nested_field_struct_name(name, head.as_str())?
                .ok_or_else(|| {
                    CompilerError::new_no_location(format!(
                        "leaf-form ctor for `{name}`: field `{head}` is not a value struct"
                    ))
                })?;
            let child_rep = arena
                .plan_nominal_value(self.ws, bone_core::hir::FnBackend::Js, child)
                .map_err(|e| err(e.message))?;
            let inner = self.render_leaf_form_value_struct(child, child_rep, sub)?;
            parts.push(format!("{}: {inner}", js_key(head)?));
        }
        Ok(format!("({{{}}})", parts.join(", ")))
    }

    fn render_make_enum(
        &self,
        tag_expr: &str,
        ty: &ustr::Ustr,
        positional: &[Idx<Inst>],
        named: &[(ustr::Ustr, Idx<Inst>)],
    ) -> Result<String, CompilerError> {
        let mut kv: Vec<String> = Vec::new();
        for (i, id) in positional.iter().enumerate() {
            kv.push(i.to_string());
            kv.push(self.render_val(*id)?);
        }
        for (name, id) in named {
            kv.push(js_string_lit(name.as_str())?);
            kv.push(self.render_val(*id)?);
        }
        Ok(format!(
            "__rock_make_enum({tag_expr}, {}, [{}])",
            js_string_lit(ty.as_str())?,
            kv.join(", ")
        ))
    }

    /// Whether `base` is the `bool` leaf view (`buf.__vb`) -- a `Uint8Array`
    /// holding 0/1 where the language wants a boolean. The view field name is the
    /// discriminant because `LEAF_VIEWS` is the single source that mints it.
    fn leaf_view_is_bool(&self, base: Idx<Inst>) -> bool {
        match &self.mf.insts[base] {
            Inst::Field { field, .. } => {
                skull_mir::lower::leaf_view_cty(field.as_str()) == Some("bool")
            }
            Inst::FfiViewCast { cty, .. } => cty.as_str() == "bool",
            _ => false,
        }
    }

    /// Colon-format render of a GC-free value struct `inspect` arg, matching the
    /// luajit cdata `__tostring` (`Name { f: v }`, declaration field order). Nested
    /// value-struct fields recurse; primitive leaves defer to the generic `inspect`.
    fn render_value_struct_inspect(
        &self,
        id: Idx<Inst>,
        cyc: &str,
        prec: &str,
    ) -> Result<String, CompilerError> {
        // `MaterializeValueStruct` is the same box, explicitly authorized -- it
        // inspects identically.
        if let Inst::MakeValueStruct { name, fields, .. }
        | Inst::MaterializeValueStruct { name, fields, .. } = &self.mf.insts[id]
        {
            if skull_mir::lower::gc_free_value_struct_field_order(self.ws, *name).is_some() {
                return self.render_value_struct_inspect_fields(*name, fields, cyc, prec);
            }
        }
        Ok(format!("inspect({}, {cyc}, {prec})", self.render_val(id)?))
    }

    /// `Name { f: v, .. }` in DECLARED field order, from a field list that may be
    /// the flattened leaf form. A nested `nobox` field arrives as dotted leaves
    /// (`pos.x`, `pos.y`) with no `pos` entry, so a lookup by declared name alone
    /// misses it -- and a missed lookup here is an emit REJECTION, which turns
    /// every print of such a struct into a skip rather than a diagnosis.
    fn render_value_struct_inspect_fields(
        &self,
        name: ustr::Ustr,
        fields: &[(ustr::Ustr, Idx<Inst>)],
        cyc: &str,
        prec: &str,
    ) -> Result<String, CompilerError> {
        let order = skull_mir::lower::gc_free_value_struct_field_order(self.ws, name)
            .ok_or_else(|| err(format!("`{name}` is not a GC-free value struct")))?;
        let (exact, groups) = Self::split_leaf_fields(fields);
        let mut expr = js_string_lit(&format!("{name} {{ "))?;
        for (i, fname) in order.iter().enumerate() {
            if i > 0 {
                expr.push_str(&format!(" + {}", js_string_lit(", ")?));
            }
            expr.push_str(&format!(" + {}", js_string_lit(&format!("{fname}: "))?));
            let rendered = if let Some(vid) = exact.get(fname) {
                self.render_value_struct_inspect(*vid, cyc, prec)?
            } else if let Some((_, sub)) = groups.iter().find(|(h, _)| h == fname.as_str()) {
                let sub_name = self
                    .nested_field_struct_name(name, fname)?
                    .ok_or_else(|| {
                        err(format!(
                            "value struct `{name}` field `{fname}` arrives as leaves but is not a value struct"
                        ))
                    })?;
                self.render_value_struct_inspect_fields(sub_name, sub, cyc, prec)?
            } else {
                return Err(err(format!(
                    "value struct `{name}` field `{fname}` absent at inspect boundary"
                )));
            };
            expr.push_str(&format!(" + {rendered}"));
        }
        expr.push_str(&format!(" + {}", js_string_lit(" }")?));
        Ok(format!("({expr})"))
    }

    /// The JS name this call site targets. A `spec_bindings` entry means the
    /// lowerer specialized this instantiation, and the shared body cannot serve
    /// it -- so an unemitted specialization resolves to `None` and rejects,
    /// rather than silently reaching the boxed shared body. Mirrors the luajit
    /// emitter's `callee_name`.
    pub(super) fn callee_name(&self, iid: Idx<Inst>, func: FuncRef) -> Option<&String> {
        // The call-site join is the identity oracle. The instruction's FuncRef
        // is only the backend declaration selected for the call; the graph owns
        // the target-neutral instance and the artifact view owns its symbol.
        let body = self
            .callees
            .instance_for_call(self.ws, self.mf, iid, func)?;
        self.callees.get(&body)
    }

    pub(super) fn render_call(
        &self,
        iid: Idx<Inst>,
        func: FuncRef,
        args: &[Idx<Inst>],
    ) -> Result<String, CompilerError> {
        let cf = &self.ws.module(func.module).functions[func.func];
        // Resolve once so typed inspection also obeys declaration supply.
        let supply = js_supply(self.ws, cf);
        // `inspect`/`inspect_formatted` of a GC-FREE value struct still diverges:
        // it is cdata on luajit, rendered `Name { f: v }` (colon, via the cdata
        // `__tostring`) where the JS `{__ty,..}`-object walker emits `f = v`
        // (equals) -- reject at the box boundary. A GC-CARRYING value struct is a
        // `_vt` TABLE on luajit; `inspect` walks it via the SAME generic
        // equals-format walker as the JS object (NOT the `_vt.__tostring`, which it
        // never calls -- oracle-verified: luajit renders `Bag { n = 5, xs = Vec {..} }`),
        // so it inspects faithfully. A boxed ENUM likewise renders faithfully (T8e).
        // `inspect` of a GC-FREE value struct: luajit renders it via the cdata
        // `__tostring` as `Name { f: v }` (COLON, declaration field order), where the
        // generic `{__ty,..}`-object walker would emit `f = v` (equals, sorted keys).
        // A GC-free value struct's fields are bounded to primitives + nested value
        // structs (no strings/GC by the C-layout rule), so an emit-time recursive
        // colon renderer is exact -- primitive leaves go through the generic
        // `inspect` (lua_tostring / precision parity), nested value structs recurse.
        if supply.is_some() && matches!(cf.name.as_str(), "inspect" | "inspect_formatted") {
            if let Some(&arg0) = args.first() {
                if let Inst::MakeValueStruct { name, .. } = &self.mf.insts[arg0] {
                    if skull_mir::lower::gc_free_value_struct_field_order(self.ws, *name).is_some()
                    {
                        let (cyc, prec) = if cf.name.as_str() == "inspect_formatted" {
                            ("true".to_string(), self.render_val(args[1])?)
                        } else {
                            (
                                args.get(1)
                                    .map(|a| self.render_val(*a))
                                    .transpose()?
                                    .unwrap_or_else(|| "true".to_string()),
                                args.get(2)
                                    .map(|a| self.render_val(*a))
                                    .transpose()?
                                    .unwrap_or_else(|| "(-1)".to_string()),
                            )
                        };
                        return self.render_value_struct_inspect(arg0, &cyc, &prec);
                    }
                }
            }
        }
        let rargs: Result<Vec<String>, CompilerError> =
            args.iter().map(|a| self.render_val(*a)).collect();
        let rargs = rargs?;
        if let Some(supply) = supply {
            return Ok(render_supply(supply, &rargs, cf.param_decls.len()));
        }
        match self.callee_name(iid, func) {
            Some(name) => Ok(format!("{name}({})", rargs.join(", "))),
            None if self.mf.spec_bindings.contains_key(&iid) => Err(err(format!(
                "JS subset: no specialized body emitted for the `{}` instantiation at this call",
                cf.name
            ))),
            None => Err(err(format!(
                "JS subset: call to `{}` is not a mapped JS intrinsic",
                cf.name
            ))),
        }
    }
}

/// The minimum argument count a co-emitted callee accepts: its MIR param count
/// minus the params carrying a default (each omitted trailing default arrives as
/// JS `undefined` and is filled callee-side). Defaults live on the fn's HIR body;
/// for the scalar defaults this gate targets (`step: int = 1`) each maps 1:1 to a
/// MIR param slot. A GC value-struct default would break the 1:1 assumption, but
/// value-struct construction rejects at emit independently, so no such call is
/// admitted here.
pub fn required_arg_count(
    ws: &Workspace,
    abi: &skull_mir::lower::BackendFnAbiPlan,
    arity: usize,
) -> usize {
    use bone_core::hir::BodyOwner;
    let fr = abi.decl();
    let defaults: Vec<bool> = ws
        .module(fr.module)
        .bodies
        .get(&BodyOwner::Function(fr.func))
        .map(|b| b.param_defaults.iter().map(|d| d.is_some()).collect())
        .unwrap_or_default();
    // Scalar ABI: a defaulted value-struct param explodes to N leaf slots and
    // all N are omittable together -- subtract per-param LEAF widths, not 1.
    // The widths come from the EMITTED BODY's own plan: a specialization's
    // instantiation is exactly where a param's leaf width differs from the
    // declaration's, so asking the declaration would answer for another ABI.
    if let Some(widths) = abi.param_leaf_widths() {
        let omittable: usize = widths
            .iter()
            .zip(defaults.iter().chain(std::iter::repeat(&false)))
            .filter(|(_, d)| **d)
            .map(|(w, _)| w)
            .sum();
        return arity.saturating_sub(omittable);
    }
    let n_defaults = defaults.iter().filter(|d| **d).count();
    arity.saturating_sub(n_defaults)
}

/// What the JS backend renders for a call it supplies ITSELF.
///
/// Two shapes, because the surface has two: a fixed-arity expression template,
/// and a variadic host global no template can express.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JsSupply {
    /// A `$1..$n` expression template.
    Template(&'static str),
    /// A bare JS global, called with every rendered argument.
    Global(&'static str),
}

/// The one JS realization oracle, shared by rendering, callee discovery and
/// the artifact view. Declaration-keyed mappings win before any declared body,
/// including the portable `map_new` body. Unmarked same-name user functions
/// remain ordinary callees.
pub fn js_supply(ws: &Workspace, cf: &bone_core::hir::Function) -> Option<JsSupply> {
    // `definition_plan_stage_js` defines the accessor under its own symbol.
    if skull_mir::definition::is_artifact_catalog_accessor(cf) {
        let symbol = ustr::Ustr::from(skull_mir::mangle::mangle_fn(ws, cf).as_str());
        return Some(JsSupply::Global(symbol.as_str()));
    }
    match JsIntrinsics(ws).lookup(cf) {
        Some(Emission::Template(tmpl)) => Some(JsSupply::Template(tmpl)),
        Some(Emission::Global(name)) => Some(JsSupply::Global(name)),
        _ => None,
    }
}

/// Render one supplied call from its arguments.
///
/// A template is padded to the callee's DECLARED arity with `undefined`: a
/// defaulted trailing param (e.g. `string.find`'s `from = 0`) is not
/// materialized into the call args, so a `$N` for it would otherwise stay
/// literal. JS reads an omitted arg as `undefined`, and for the templated fns
/// that carry a default that coincides with the Rock default. A global is
/// variadic and takes the arguments as they came.
fn render_supply(supply: JsSupply, args: &[String], declared_arity: usize) -> String {
    match supply {
        JsSupply::Template(tmpl) => {
            let mut padded: Vec<String> = args.to_vec();
            while padded.len() < declared_arity {
                padded.push("undefined".to_string());
            }
            let refs: Vec<&str> = padded.iter().map(|s| s.as_str()).collect();
            fill_template(tmpl, &refs)
        }
        JsSupply::Global(name) => format!("{name}({})", args.join(", ")),
    }
}
