use super::*;
use skull_mir::mir::{FlatVecContents, FlatVecCtor, VEC_COLUMN_HEADER_FIELD};

impl Emitter<'_> {
    fn render_flat_vec(&self, vec: &FlatVecCtor) -> Result<String, CompilerError> {
        let header = || {
            let stride = vec
                .contents
                .stride()
                .map_or_else(String::new, |stride| format!(", stride: {stride}"));
            Ok::<_, CompilerError>(format!(
                "{{repr: {}, elem: {}{stride}, cap: {}}}",
                js_string_lit(vec.contents.repr_name()),
                js_string_lit(skull_mir::mangle::type_symbol(self.ws, vec.elem.name).as_str()),
                self.render_val(vec.capacity)?,
            ))
        };
        let render_values = |values: &[Idx<Inst>]| {
            values
                .iter()
                .map(|&value| self.render_val(value))
                .collect::<Result<Vec<_>, _>>()
        };
        let key = || js_string_lit(VEC_COLUMN_HEADER_FIELD);
        Ok(match &vec.contents {
            FlatVecContents::Header(_) => header()?,
            FlatVecContents::Columns { columns, len } => {
                let mut out = format!("{{{}: {}", key(), header()?);
                for (name, values) in columns {
                    let values = render_values(values)?.join(", ");
                    write!(out, ", {}: [{values}]", js_string_lit(name.as_str())).unwrap();
                }
                write!(out, ", len: {len}, __ty: \"Vec\"}}").unwrap();
                out
            }
            FlatVecContents::Rows { values, len, .. } => format!(
                "{{_data: [{}], len: {len}, __ty: \"Vec\", {}: {}}}",
                render_values(values)?.join(", "),
                key(),
                header()?,
            ),
        })
    }

    /// Render an inst's own JS expression; JS is untyped, so int/float collapse to one operator.
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
            Inst::Const(Lit::Str(s)) => js_string_lit(s.as_str()),
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
            // A zero divisor is the only way out of uint range, and LuaJIT yields inf/nan there;
            // Rock int `/` floors, JS `/` does not.
            Inst::UintDiv(a, b) | Inst::IntDiv(a, b) => format!(
                "Math.floor({} / {})",
                self.render_val(*a)?,
                self.render_val(*b)?
            ),
            Inst::UintMod(a, b) => bin(a, "%", b, self)?,
            Inst::FloatDiv(a, b) => bin(a, "/", b, self)?,
            Inst::IntLt(a, b) | Inst::UintLt(a, b) | Inst::FloatLt(a, b) => bin(a, "<", b, self)?,
            Inst::IntLeq(a, b) | Inst::UintLeq(a, b) | Inst::FloatLeq(a, b) => {
                bin(a, "<=", b, self)?
            }
            Inst::IntGt(a, b) | Inst::UintGt(a, b) | Inst::FloatGt(a, b) => bin(a, ">", b, self)?,
            Inst::IntGeq(a, b) | Inst::UintGeq(a, b) | Inst::FloatGeq(a, b) => {
                bin(a, ">=", b, self)?
            }
            // Loose `== null` also matches `undefined`, so an omitted defaulted arg still trips the
            // callee's default-fill guard.
            Inst::Eq(a, b) if self.is_nil(*a) || self.is_nil(*b) => bin(a, "==", b, self)?,
            Inst::NotEq(a, b) if self.is_nil(*a) || self.is_nil(*b) => bin(a, "!=", b, self)?,
            Inst::Eq(a, b) => bin(a, "===", b, self)?,
            Inst::NotEq(a, b) => bin(a, "!==", b, self)?,
            // Space after `-`: a negative literal would otherwise emit `(--1.5)`, JS's decrement
            // operator.
            Inst::Neg(x) => format!("(- {})", self.render_val(*x)?),
            Inst::Not(x) => format!("(!{})", self.render_val(*x)?),
            // `bit.rshift` is logical (`>>>`), which yields unsigned; `| 0` restores luajit's
            // signed result (`bit.rshift(-1, 0)` is -1).
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
            Inst::VecIndexSlot(index) | Inst::NbIndexSlot(index) => self.render_val(*index)?,
            Inst::VecStrideNilToFalse(x) => {
                let value = self.render_val(*x)?;
                format!("((value) => value == null ? false : value)({value})")
            }
            Inst::VecStrideFalseToNil(x) => {
                let value = self.render_val(*x)?;
                format!("((value) => value === false ? null : value)({value})")
            }
            Inst::FloatToInt(x) => format!("Math.floor({})", self.render_val(*x)?),
            // Lua `..` stringifies operands (`%.14g` numbers); JS `+` does not, so every leaf goes
            // through `lua_tostring`.
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
            Inst::ToString(x) => format!("lua_tostring({})", self.render_val(*x)?),
            // Rock `%` is `a - floor(a / b) * b` (JS `%` truncates); operands appear twice, so an
            // inlined expression binds once.
            Inst::IntMod(a, b) | Inst::FloatMod(a, b) => {
                let (ra, rb) = (self.render_val(*a)?, self.render_val(*b)?);
                if self.is_plain_operand(*a) && self.is_plain_operand(*b) {
                    format!("({ra} - Math.floor({ra} / {rb}) * {rb})")
                } else {
                    format!("((a, b) => a - Math.floor(a / b) * b)({ra}, {rb})")
                }
            }
            Inst::Call { func, args } => self.render_call(iid, *func, args)?,
            // Only capture-free hoisted closures and the `error` runtime global are emittable
            // global reads.
            Inst::Global(name) if self.mf.code_value_identities.contains_key(&iid) => {
                let identity = &self.mf.code_value_identities[&iid];
                let key = self
                    .globals
                    .code_slots
                    .get(identity)
                    .ok_or_else(|| err("JS: Rock function value has no CodeRef slot"))?;
                format!("__rock_code_slot_ref({})", js_string_lit(key))
            }
            Inst::Global(name)
                if self.hoisted.contains(name)
                    || name.as_str() == "error"
                    || is_runtime_global(name) =>
            {
                name.to_string()
            }
            Inst::Global(name) if self.globals.fns.contains_key(name) => {
                self.globals.fns[name].clone()
            }
            Inst::Global(name) if self.globals.const_globals.contains(name) => {
                js_member("__G", name.as_str())
            }
            Inst::Global(name) => match self.globals.state.get(&symbol_of_key(*name)) {
                Some(slot) => {
                    self.globals.uses_storage.set(true);
                    format!("__rock_globals[{}]", js_string_lit(slot))
                }
                None if self.globals.enum_globals.contains(name) => name.to_string(),
                None => {
                    return Err(err(format!(
                        "JS subset: global read `{name}` not supported"
                    )));
                }
            },
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
                            js_string_lit(key)
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
            Inst::CallValue { callee, args } if self.is_runtime_value(*callee) => {
                let rendered = args
                    .iter()
                    .map(|a| self.render_val(*a))
                    .collect::<Result<Vec<_>, _>>()?;
                format!("{}({})", self.render_val(*callee)?, rendered.join(", "))
            }
            Inst::CallValue { callee, args } => self.indirect_call(*callee, args)?,
            Inst::MakeStruct { ty, fields } => {
                let name = ty
                    .map(|t| skull_mir::mangle::type_symbol(self.ws, t).to_string())
                    .unwrap_or_default();
                let mut parts: Vec<String> = Vec::new();
                if !name.is_empty() {
                    let tag = if is_native_buffer(&name) {
                        "NativeBuffer"
                    } else {
                        &name
                    };
                    parts.push(format!("__ty: {}", js_string_lit(tag)));
                }
                for (f, v) in fields {
                    parts.push(format!("{}: {}", js_key(f), self.render_val(*v)?));
                }
                format!("({{{}}})", parts.join(", "))
            }
            // A value struct is a plain `{__ty, ...}` object on JS. A GC-carrying one compares
            // pointer fields by reference like luajit's `_vt.__eq`, via the per-type `__rock_vt_eq`
            // registry consulted before the generic walk.
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
                let mut parts = vec![format!(
                    "__ty: {}",
                    js_string_lit(skull_mir::mangle::type_symbol(self.ws, *name).as_str())
                )];
                for (f, v) in fields {
                    parts.push(format!("{}: {}", js_key(f), self.render_val(*v)?));
                }
                format!("({{{}}})", parts.join(", "))
            }
            // A non-cdata enum boxes with 0-based positional keys through the nil-dropping ctor; a
            // unit variant of a mixed enum is an interned singleton, so a bare `MakeEnum` carries
            // payload.
            Inst::MakeEnum {
                tag,
                ty,
                positional,
                named,
            } => self.render_make_enum(&js_string_lit(tag.as_str()), ty, positional, named)?,
            Inst::MakeEnumDyn {
                tag,
                ty,
                positional,
                named,
            } => self.render_make_enum(&self.render_val(*tag)?, ty, positional, named)?,
            Inst::MakeTuple(elems) => {
                let rendered: Result<Vec<String>, CompilerError> =
                    elems.iter().map(|e| self.render_val(*e)).collect();
                format!("[{}]", rendered?.join(", "))
            }
            Inst::TupleIndex { base, index } => {
                format!("{}[{index}]", self.render_val(*base)?)
            }
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
                    ContainerKind::NativeBufferInBounds => format!("{b}.ptr[{i}]"),
                    // Lowering decomposes whole-element reads (`nb_leaf_element_repr`), so one
                    // reaching emit has no C leaf layout.
                    ContainerKind::NativeBufferValue(elem) => {
                        return Err(err(format!(
                            "JS subset: whole-element read of `NativeBuffer<{elem}>` reached \
                             emit -- lowering must decompose it into leaf reads"
                        )));
                    }
                    ContainerKind::Vec | ContainerKind::VecNonNull => {
                        format!("__vec_checked_get({b}, {i})")
                    }
                    ContainerKind::VecInBounds | ContainerKind::VecNonNullInBounds => {
                        format!("{b}._data[{i}]")
                    }
                    // Column storage has no element object; lowering decomposes whole-element reads
                    // (`column_element_repr`).
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
                    ContainerKind::VecColumn | ContainerKind::VecStrideSlot => format!("{b}[{i}]"),
                    // The index is bounds-checked once per element; a `bool` view is a Uint8Array
                    // of 0/1 that reads back as a boolean.
                    ContainerKind::NativeBufferLeaf => {
                        if self.leaf_view_is_bool(*base) {
                            format!("({b}[{i}] !== 0)")
                        } else {
                            format!("{b}[{i}]")
                        }
                    }
                    ContainerKind::Map => format!(
                        "(function(t, k) {{ return t instanceof Map ? t.get(k) : t[k]; }})({b}, {i})"
                    ),
                    ContainerKind::Record(_) => {
                        if let Inst::Const(Lit::Str(field)) = &self.mf.insts[*index] {
                            js_member(&b, field.as_str())
                        } else {
                            format!("{b}[{i}]")
                        }
                    }
                }
            }
            Inst::Len { base, kind } => match kind {
                ContainerKind::NativeBuffer
                | ContainerKind::NativeBufferInBounds
                | ContainerKind::NativeBufferValue(_)
                | ContainerKind::Vec
                | ContainerKind::VecInBounds
                | ContainerKind::VecNonNull
                | ContainerKind::VecNonNullInBounds
                | ContainerKind::VecValueColumns(_)
                | ContainerKind::VecValueStrided(_) => {
                    format!("{}.len", self.render_val(*base)?)
                }
                ContainerKind::VecColumn | ContainerKind::VecStrideSlot => {
                    format!("{}.length", self.render_val(*base)?)
                }
                // A view has no length of its own; only the owning NativeBuffer knows the element
                // count.
                ContainerKind::NativeBufferLeaf => {
                    return Err(err("JS subset: `len` of a NativeBuffer leaf view"));
                }
                ContainerKind::Map => format!("{}.size", self.render_val(*base)?),
                ContainerKind::Record(_) => unreachable!("len of a heap record"),
            },
            Inst::FfiNewArray {
                ctype,
                count,
                representation,
            } => match ctype.as_str() {
                "float" => format!("new Float32Array({})", self.render_val(*count)?),
                "int" => format!("new Int32Array({})", self.render_val(*count)?),
                "uint32_t" => format!("new Uint32Array({})", self.render_val(*count)?),
                "bool" => format!("new Uint8Array({})", self.render_val(*count)?),
                // A value struct backs onto one flat `Float32Array(n * size/4)` in luajit's C
                // layout, which the per-leaf views alias through `.buffer`.
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
                    // A 4-byte-backed array needs a 4-byte multiple; an all-`bool` element would
                    // not and rejects.
                    if size % 4 != 0 {
                        return Err(err(format!(
                            "JS subset: native_buffer_alloc element ctype `{other}` is {size} \
                             bytes, not a multiple of 4 -- no aligned TypedArray backing"
                        )));
                    }
                    // The backing view uses float32 slots regardless of C width; keep that width
                    // for byte equality.
                    format!(
                        "Object.assign(new Float32Array(({}) * {}), {{__rock_elem_size: {size}}})",
                        self.render_val(*count)?,
                        size / 4
                    )
                }
            },
            // The views and `.ptr` alias one `ArrayBuffer`, keeping leaf-addressed and whole-`.ptr`
            // (host upload) access coherent.
            Inst::FfiViewCast { cty, ptr } => {
                let p = self.render_val(*ptr)?;
                let array = match cty.as_str() {
                    "double" => "Float64Array",
                    "float" => "Float32Array",
                    "int" => "Int32Array",
                    "uint32_t" => "Uint32Array",
                    "bool" => "Uint8Array",
                    other => {
                        return Err(err(format!(
                            "JS subset: no TypedArray for leaf view C type `{other}`"
                        )));
                    }
                };
                format!("new {array}({p}.buffer, {p}.byteOffset)")
            }
            Inst::Field { base, field, .. } => js_member(&self.render_val(*base)?, field),
            // Presence is a bool on the scalar ABI, so JS truthiness agrees with luajit's `present
            // and value or nil`.
            Inst::MakeNullableValue { present, value } => format!(
                "({} ? {} : null)",
                self.render_val(*present)?,
                self.render_val(*value)?
            ),
            Inst::Format { value, fmt } => {
                format!(
                    "__rock_sprintf({}, {})",
                    js_string_lit(fmt.as_str()),
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
            Inst::MakeFlatVec(vec) => self.render_flat_vec(vec)?,
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

    /// Flatten a `StringConcat` tree; a materialized child stays a leaf because its operands may be
    /// `let`s of another switch case.
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

    /// A value constructor arriving as leaf paths. A CData box is a nested object, so a child's
    /// leaves are rebuilt under its field name; a TABLE box keys the leaf paths themselves, so they
    /// stay flat.
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
            // An absent tag canonicalizes to `null`; the scalar ABI spells absence `false`.
            let presence = arena
                .nominal_value_leaf_presence_flags_required(
                    self.ws,
                    bone_core::hir::FnBackend::Js,
                    name,
                )
                .map_err(|e| err(e.message))?;
            if presence.len() != fields.len() {
                return Err(err(format!(
                    "`{name}` leaf-form constructor has {} leaves for {} presence flags",
                    fields.len(),
                    presence.len()
                )));
            }
            let mut parts = vec![format!(
                "__ty: {}",
                js_string_lit(skull_mir::mangle::type_symbol(self.ws, name).as_str())
            )];
            for ((key, value), tag) in fields.iter().zip(presence) {
                let rendered = self.render_val(*value)?;
                let rendered = if tag {
                    format!("({rendered} ? true : null)")
                } else {
                    rendered
                };
                parts.push(format!("{}: {rendered}", js_key(key)));
            }
            return Ok(format!("({{{}}})", parts.join(", ")));
        }
        let (top, groups) = split_leaf_paths(fields.iter().copied());
        let mut parts = vec![format!(
            "__ty: {}",
            js_string_lit(skull_mir::mangle::type_symbol(self.ws, name).as_str())
        )];
        for (key, value) in &top {
            parts.push(format!("{}: {}", js_key(key), self.render_val(*value)?));
        }
        for (head, sub) in &groups {
            let child =
                nested_field_struct_name(self.ws, name, head.as_str())?.ok_or_else(|| {
                    CompilerError::new_no_location(format!(
                        "leaf-form ctor for `{name}`: field `{head}` is not a value struct"
                    ))
                })?;
            let child_rep = arena
                .plan_nominal_value(self.ws, bone_core::hir::FnBackend::Js, child)
                .map_err(|e| err(e.message))?;
            let inner = self.render_leaf_form_value_struct(child, child_rep, sub)?;
            parts.push(format!("{}: {inner}", js_key(head)));
        }
        Ok(format!("({{{}}})", parts.join(", ")))
    }

    /// Box an enum: `__rock_make_enum(<tag>, "<ty>", [0, p0, 1, p1, "name", v, ..])`; the ctor
    /// drops nil `kv` slots.
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
            kv.push(js_string_lit(name.as_str()));
            kv.push(self.render_val(*id)?);
        }
        Ok(format!(
            "__rock_make_enum({tag_expr}, {}, [{}])",
            js_string_lit(skull_mir::mangle::type_symbol(self.ws, *ty).as_str()),
            kv.join(", ")
        ))
    }

    fn leaf_view_is_bool(&self, base: Idx<Inst>) -> bool {
        match &self.mf.insts[base] {
            Inst::Field { field, .. } => {
                skull_mir::lower::leaf_view_cty(field.as_str()) == Some("bool")
            }
            Inst::FfiViewCast { cty, .. } => cty.as_str() == "bool",
            _ => false,
        }
    }

    fn render_value_struct_inspect(
        &self,
        name: ustr::Ustr,
        id: Idx<Inst>,
        cyc: &str,
        prec: &str,
        indent: &str,
    ) -> Result<String, CompilerError> {
        if let Inst::MakeValueStruct {
            name: actual,
            fields,
            ..
        }
        | Inst::MaterializeValueStruct {
            name: actual,
            fields,
            ..
        } = &self.mf.insts[id]
        {
            if *actual == name
                && skull_mir::lower::gc_free_value_struct_field_order(self.ws, *actual).is_some()
            {
                return self.render_value_struct_inspect_fields(*actual, fields, cyc, prec, indent);
            }
        }
        let value = self.render_val(id)?;
        Ok(format!(
            "__rock_inspect_value({value}, {}, \"root\", null, {prec})",
            js_string_lit(indent)
        ))
    }

    fn render_value_struct_inspect_fields(
        &self,
        name: ustr::Ustr,
        fields: &[(ustr::Ustr, Idx<Inst>)],
        cyc: &str,
        prec: &str,
        indent: &str,
    ) -> Result<String, CompilerError> {
        let mut order = skull_mir::lower::gc_free_value_struct_field_order(self.ws, name)
            .ok_or_else(|| err(format!("`{name}` is not a GC-free value struct")))?;
        order.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        let (exact, groups) = split_leaf_paths(fields.iter().copied());
        let display_name = skull_mir::layout_instance::display_name(name);
        let multiline = order.len() > 2;
        let mut expr = if multiline {
            js_string_lit(&format!("{display_name} {{\n"))
        } else {
            js_string_lit(&format!("{display_name} {{ "))
        };
        let child_indent = if multiline {
            format!("{indent}  ")
        } else {
            indent.to_string()
        };
        for (i, fname) in order.iter().enumerate() {
            if i > 0 {
                expr.push_str(&format!(
                    " + {}",
                    js_string_lit(if multiline { ",\n" } else { ", " })
                ));
            }
            let label = if multiline {
                format!("{child_indent}{fname} = ")
            } else {
                format!("{fname} = ")
            };
            expr.push_str(&format!(" + {}", js_string_lit(&label)));
            let presence = exact.iter().find(|(f, _)| f == fname).map(|(_, vid)| *vid);
            let leaves = groups.iter().find(|(h, _)| h == fname);
            let enum_view = match leaves {
                Some((_, sub)) => match nested_field_nominal_name(self.ws, name, fname)? {
                    Some(child) => {
                        let rendered = sub
                            .iter()
                            .map(|(leaf, vid)| Ok((leaf.as_str(), self.render_val(*vid)?)))
                            .collect::<Result<HashMap<_, _>, CompilerError>>()?;
                        enum_leaf_view(self.ws, child, |leaf| {
                            rendered.get(leaf).cloned().unwrap_or_else(|| "null".into())
                        })
                    }
                    None => None,
                },
                None => None,
            };
            let rendered = if let Some(view) = enum_view {
                format!("inspect({view}, {cyc}, {prec})")
            } else if let Some((_, sub)) = leaves {
                // A nullable inline child arrives as its presence under the field
                // name plus its dotted leaves.
                let sub_name = nested_field_struct_name(self.ws, name, fname)?.ok_or_else(|| {
                    err(format!(
                        "value struct `{name}` field `{fname}` arrives as leaves but is not a value struct"
                    ))
                })?;
                let fields = self.render_value_struct_inspect_fields(
                    sub_name,
                    sub,
                    cyc,
                    prec,
                    &child_indent,
                )?;
                match presence {
                    Some(present) => {
                        format!("({} ? {fields} : \"nil\")", self.render_val(present)?)
                    }
                    None => fields,
                }
            } else if let Some(vid) = presence {
                if let Some(child_name) = nested_field_struct_name(self.ws, name, fname)? {
                    self.render_value_struct_inspect(child_name, vid, cyc, prec, &child_indent)?
                } else {
                    format!("inspect({}, {cyc}, {prec})", self.render_val(vid)?)
                }
            } else {
                return Err(err(format!(
                    "value struct `{name}` field `{fname}` absent at inspect boundary"
                )));
            };
            expr.push_str(&format!(" + {rendered}"));
        }
        let close = if multiline {
            format!("\n{indent}}}")
        } else {
            " }".to_string()
        };
        expr.push_str(&format!(" + {}", js_string_lit(&close)));
        Ok(format!("({expr})"))
    }

    /// The JS name this call site targets; an unemitted specialization resolves to `None` and
    /// rejects instead of reaching the boxed shared body.
    pub(super) fn callee_name(&self, iid: Idx<Inst>, func: FuncRef) -> Option<&String> {
        let body = self
            .callees
            .instance_for_call(self.ws, self.mf, iid, func)?;
        self.callees.get(&body)
    }

    pub(super) fn indirect_call(
        &self,
        callee: Idx<Inst>,
        args: &[Idx<Inst>],
    ) -> Result<String, CompilerError> {
        let operands = std::iter::once(callee)
            .chain(args.iter().copied())
            .map(|value| self.render_val(value))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(format!("__rock_call({})", operands.join(", ")))
    }

    pub(super) fn render_call(
        &self,
        iid: Idx<Inst>,
        func: FuncRef,
        args: &[Idx<Inst>],
    ) -> Result<String, CompilerError> {
        let cf = &self.ws.module(func.module).functions[func.func];
        let supply = js_supply(self.ws, cf);
        if supply.is_some() && matches!(cf.name.as_str(), "inspect" | "inspect_formatted") {
            if let Some(&arg0) = args.first() {
                if let Inst::MakeValueStruct { name, .. }
                | Inst::MaterializeValueStruct { name, .. } = &self.mf.insts[arg0]
                {
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
                        return self.render_value_struct_inspect(*name, arg0, &cyc, &prec, "");
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

/// The minimum argument count a co-emitted callee accepts: MIR param count minus defaulted params,
/// which arrive as `undefined` and fill callee-side.
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
    // A defaulted value-struct param explodes to N leaf slots, all omittable together; widths come
    // from the emitted body's own plan, since a specialization's leaf width can differ from the
    // declaration's.
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JsSupply {
    Template(&'static str),
    Global(&'static str),
}

/// The one JS realization oracle for rendering, callee discovery and the artifact view;
/// declaration-keyed mappings win before any declared body.
pub fn js_supply(ws: &Workspace, cf: &bone_core::hir::Function) -> Option<JsSupply> {
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

/// Render one supplied call. A template is padded to the declared arity with `undefined`, since
/// defaulted trailing params are not in the call args.
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
