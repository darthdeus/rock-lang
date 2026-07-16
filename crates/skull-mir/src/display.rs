//! Textual dump of a [`MirFunc`] — the test oracle and how we eyeball lowering.
//!
//! Value-producing instructions are numbered `v0..vN` in block/append order;
//! effects ([`Inst::Store`]) bind no value, so they print without a number.
//! Cells print as `cN` (their arena index), blocks as `bbN`.

use std::collections::HashMap;
use std::fmt;

use comfy_la_arena::Idx;

use crate::mir::{Cell, Inst, Lit, MirFunc, Term};

fn raw<T>(idx: Idx<T>) -> u32 {
    idx.into_raw().into()
}

fn fmt_lit(l: &Lit) -> String {
    match l {
        Lit::Int(i) => format!("{i}"),
        Lit::Float(x) => format!("{x:?}"),
        Lit::Bool(b) => format!("{b}"),
        Lit::Str(s) => format!("{:?}", s.as_str()),
        Lit::Nil => "nil".to_string(),
    }
}

impl fmt::Display for MirFunc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Number value-producing instructions sequentially across all blocks.
        let mut num: HashMap<Idx<Inst>, u32> = HashMap::new();
        let mut next = 0u32;
        for (_, block) in self.blocks.iter() {
            for &id in &block.insts {
                if self.insts[id].produces_value() {
                    num.insert(id, next);
                    next += 1;
                }
            }
        }

        let params = self
            .params
            .iter()
            .map(|&c| format!("c{}", raw(c)))
            .collect::<Vec<_>>()
            .join(", ");
        write!(f, "fn {}({}) {{", self.name, params)?;
        for (bid, block) in self.blocks.iter() {
            write!(f, "\n  bb{}:", raw(bid))?;
            for &id in &block.insts {
                let line = fmt_inst(&self.insts[id], &num, &self.cells);
                match num.get(&id) {
                    Some(v) => write!(f, "\n    v{v} = {line}")?,
                    None => write!(f, "\n    {line}")?,
                }
            }
            write!(f, "\n    {}", fmt_term(&block.term, &num))?;
        }
        write!(f, "\n}}")
    }
}

fn fmt_inst(
    inst: &Inst,
    num: &HashMap<Idx<Inst>, u32>,
    _cells: &comfy_la_arena::Arena<Cell>,
) -> String {
    let v = |id: Idx<Inst>| num[&id];
    let bin = |name: &str, a: &Idx<Inst>, b: &Idx<Inst>| format!("{name} v{}, v{}", v(*a), v(*b));
    let list = |vs: &[Idx<Inst>]| {
        vs.iter()
            .map(|id| format!("v{}", num[id]))
            .collect::<Vec<_>>()
            .join(", ")
    };
    match inst {
        Inst::Const(l) => format!("const {}", fmt_lit(l)),
        Inst::Global(g) => format!("global {g}"),
        Inst::Load(c) => format!("load c{}", raw(*c)),

        Inst::IntAdd(a, b) => bin("int.add", a, b),
        Inst::IntSub(a, b) => bin("int.sub", a, b),
        Inst::IntMul(a, b) => bin("int.mul", a, b),
        Inst::IntDiv(a, b) => bin("int.div", a, b),
        Inst::IntMod(a, b) => bin("int.mod", a, b),
        Inst::FloatAdd(a, b) => bin("float.add", a, b),
        Inst::FloatSub(a, b) => bin("float.sub", a, b),
        Inst::FloatMul(a, b) => bin("float.mul", a, b),
        Inst::FloatDiv(a, b) => bin("float.div", a, b),
        Inst::FloatMod(a, b) => bin("float.mod", a, b),
        Inst::StringConcat(a, b) => bin("str.concat", a, b),
        Inst::ToString(x) => format!("tostring v{}", v(*x)),
        Inst::Format { value, fmt } => format!("format {:?} v{}", fmt.as_str(), v(*value)),
        Inst::FormatDyn { value, fmt } => format!("format_dyn v{} v{}", v(*fmt), v(*value)),
        Inst::FloatToInt(x) => format!("float_to_int v{}", v(*x)),
        Inst::IntToFloat(x) => format!("int_to_float v{}", v(*x)),

        Inst::IntLt(a, b) => bin("int.lt", a, b),
        Inst::IntLeq(a, b) => bin("int.leq", a, b),
        Inst::IntGt(a, b) => bin("int.gt", a, b),
        Inst::IntGeq(a, b) => bin("int.geq", a, b),
        Inst::FloatLt(a, b) => bin("float.lt", a, b),
        Inst::FloatLeq(a, b) => bin("float.leq", a, b),
        Inst::FloatGt(a, b) => bin("float.gt", a, b),
        Inst::FloatGeq(a, b) => bin("float.geq", a, b),
        Inst::Eq(a, b) => bin("eq", a, b),
        Inst::NotEq(a, b) => bin("neq", a, b),

        Inst::Neg(x) => format!("neg v{}", v(*x)),
        Inst::Not(x) => format!("not v{}", v(*x)),
        Inst::BitNot(x) => format!("bitnot v{}", v(*x)),

        Inst::BitAnd(a, b) => bin("bit.and", a, b),
        Inst::BitOr(a, b) => bin("bit.or", a, b),
        Inst::BitXor(a, b) => bin("bit.xor", a, b),
        Inst::Shl(a, b) => bin("bit.shl", a, b),
        Inst::Shr(a, b) => bin("bit.shr", a, b),

        Inst::CallBoundMulti { dests, generic_idx, args } => {
            let ds = dests
                .iter()
                .map(|c| format!("c{}", raw(*c)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{ds} = call_bound <I{generic_idx}>({})", list(args))
        }
        Inst::Call { func, args } => {
            let a = args.iter().map(|id| format!("v{}", num[id])).collect::<Vec<_>>().join(", ");
            format!("call {}:{}({a})", func.module.0, raw(func.func))
        }
        Inst::CallMulti { dests, func, args } => {
            let d = dests.iter().map(|c| format!("c{}", raw(*c))).collect::<Vec<_>>().join(", ");
            let a = args.iter().map(|id| format!("v{}", num[id])).collect::<Vec<_>>().join(", ");
            format!("{d} = call {}:{}({a})", func.module.0, raw(func.func))
        }
        Inst::CallValue { callee, args } => {
            let a = args.iter().map(|id| format!("v{}", num[id])).collect::<Vec<_>>().join(", ");
            format!("call_value v{}({a})", v(*callee))
        }
        Inst::CallValueMulti { dests, callee, args } => {
            let d = dests.iter().map(|c| format!("c{}", raw(*c))).collect::<Vec<_>>().join(", ");
            let a = args.iter().map(|id| format!("v{}", num[id])).collect::<Vec<_>>().join(", ");
            format!("{d} = call_value v{}({a})", v(*callee))
        }
        Inst::Lambda { params, entry } => {
            let ps = params.iter().map(|c| format!("c{}", raw(*c))).collect::<Vec<_>>().join(", ");
            format!("lambda ({ps}) -> bb{}", raw(*entry))
        }

        Inst::MakeTuple(vs) => format!("tuple [{}]", list(vs)),
        Inst::MakeVec(vs) => format!("vec [{}]", list(vs)),
        Inst::MakeStruct { ty, fields } => {
            let fs = fields
                .iter()
                .map(|(n, id)| format!("{n}=v{}", num[id]))
                .collect::<Vec<_>>()
                .join(", ");
            let tyn = ty.map(|t| format!("{t} ")).unwrap_or_default();
            format!("struct {tyn}{{{fs}}}")
        }
        Inst::MakeEnum {
            tag,
            ty,
            positional,
            named,
        } => {
            let mut parts: Vec<String> = positional.iter().map(|id| format!("v{}", num[id])).collect();
            parts.extend(named.iter().map(|(n, id)| format!("{n}=v{}", num[id])));
            format!("enum {ty}::{tag}({})", parts.join(", "))
        }
        Inst::MakeEnumDyn { tag, ty, positional, named } => {
            let mut parts: Vec<String> = positional.iter().map(|id| format!("v{}", num[id])).collect();
            parts.extend(named.iter().map(|(n, id)| format!("{n}=v{}", num[id])));
            format!("enum_dyn {ty}::[v{}]({})", num[tag], parts.join(", "))
        }

        Inst::Field { base, field } => format!("field v{}.{field}", v(*base)),
        Inst::TupleIndex { base, index } => format!("tuple_index v{}.{index}", v(*base)),

        Inst::Index { base, index, kind } => {
            format!("index.{kind:?} v{}[v{}]", v(*base), v(*index))
        }
        Inst::Len { base, kind } => format!("len.{kind:?} v{}", v(*base)),

        Inst::Store { cell, value } => format!("store c{}, v{}", raw(*cell), v(*value)),
        Inst::StoreField { base, field, value } => {
            format!("store_field v{}.{field}, v{}", v(*base), v(*value))
        }
        Inst::StoreIndex {
            base,
            index,
            value,
            kind,
        } => format!(
            "store_index.{kind:?} v{}[v{}], v{}",
            v(*base),
            v(*index),
            v(*value)
        ),
        Inst::StoreGlobal { name, value } => format!("store_global {name}, v{}", v(*value)),
        Inst::ArrayPush { arr, value, kind } => {
            format!("array_push[{kind:?}] v{}, v{}", v(*arr), v(*value))
        }
        Inst::CoroSetInner(x) => format!("coro_set_inner v{}", v(*x)),
        Inst::MakeValueStruct { name, fields } => {
            let parts: Vec<String> =
                fields.iter().map(|(fname, f)| format!("{fname}: v{}", v(*f))).collect();
            format!("{name}_ct({})", parts.join(", "))
        }
        Inst::CoroHook { kind, lambda } => {
            format!("coro_hook {} v{}", kind.slot(), v(*lambda))
        }
        Inst::MapNext {
            key_cell,
            val_cell,
            table,
        } => format!(
            "c{}, c{} = next(v{})",
            raw(*key_cell),
            raw(*val_cell),
            v(*table)
        ),
    }
}

fn fmt_term(t: &Term, num: &HashMap<Idx<Inst>, u32>) -> String {
    match t {
        Term::Return(vs) if vs.is_empty() => "return".to_string(),
        Term::Return(vs) => {
            format!("return {}", vs.iter().map(|v| format!("v{}", num[v])).collect::<Vec<_>>().join(", "))
        }
        Term::Unreachable => "unreachable".to_string(),
        Term::Goto(b) => format!("goto bb{}", raw(*b)),
        Term::Branch {
            cond,
            then_bb,
            else_bb,
        } => format!(
            "branch v{} -> bb{}, bb{}",
            num[cond],
            raw(*then_bb),
            raw(*else_bb)
        ),
        Term::Switch {
            discr,
            cases,
            default,
        } => {
            let cs = cases
                .iter()
                .map(|(l, b)| format!("{} -> bb{}", fmt_lit(l), raw(*b)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("switch v{} [{cs}] else bb{}", num[discr], raw(*default))
        }
        Term::Yield { values, resume } => {
            if values.is_empty() {
                format!("yield -> bb{}", raw(*resume))
            } else {
                let vs = values.iter().map(|v| format!("v{}", num[v])).collect::<Vec<_>>().join(", ");
                format!("yield {vs} -> bb{}", raw(*resume))
            }
        }
    }
}
