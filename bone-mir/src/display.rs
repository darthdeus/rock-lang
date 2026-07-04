//! Textual dump of a `MirFunc` (the test oracle + how we eyeball lowering).

use std::collections::HashMap;
use std::fmt;

use comfy_la_arena::Idx;

use crate::mir::{Inst, MirFunc, Term};

fn n<T>(idx: Idx<T>) -> u32 {
    idx.into_raw().into()
}

fn produces_value(inst: &Inst) -> bool {
    !matches!(inst, Inst::Store { .. })
}

impl fmt::Display for MirFunc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Number value-producing instructions sequentially; effects (Store)
        // don't bind a value, so they get no number.
        let mut num: HashMap<Idx<Inst>, u32> = HashMap::new();
        let mut next = 0u32;
        for (_, block) in self.blocks.iter() {
            for &id in &block.insts {
                if produces_value(&self.insts[id]) {
                    num.insert(id, next);
                    next += 1;
                }
            }
        }

        write!(f, "fn {}() {{", self.name)?;
        for (bid, block) in self.blocks.iter() {
            write!(f, "\n  bb{}:", n(bid))?;
            for &id in &block.insts {
                let line = fmt_inst(&self.insts[id], &num);
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

fn fmt_inst(inst: &Inst, num: &HashMap<Idx<Inst>, u32>) -> String {
    let v = |id: Idx<Inst>| num[&id];
    match inst {
        Inst::Const(c) => format!("const {c}"),
        Inst::IntAdd(a, b) => format!("int.add v{}, v{}", v(*a), v(*b)),
        Inst::Load(c) => format!("load c{}", n(*c)),
        Inst::Store { cell, value } => format!("store c{}, v{}", n(*cell), v(*value)),
    }
}

fn fmt_term(t: &Term, num: &HashMap<Idx<Inst>, u32>) -> String {
    match t {
        Term::Return(Some(v)) => format!("return v{}", num[v]),
        Term::Return(None) => "return".to_string(),
        Term::Unreachable => "unreachable".to_string(),
    }
}
