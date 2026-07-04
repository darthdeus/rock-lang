//! MIR: HIR, flattened. Same `Arena` + `Idx` style as the HIR — a function is a
//! graph of blocks (control flow made explicit) instead of a tree of exprs, and
//! variables are cells. An instruction's `Idx<Inst>` *is* its value, exactly
//! like `Idx<Expr>` is a value in HIR. No separate `Value` type.

use comfy_la_arena::{Arena, Idx};
use ustr::Ustr;

/// One instruction. Operands are `Idx<Inst>` (the producing instruction).
#[derive(Clone, Debug)]
pub enum Inst {
    Const(i64),
    IntAdd(Idx<Inst>, Idx<Inst>),
    Load(Idx<Cell>),                             // read a variable
    Store { cell: Idx<Cell>, value: Idx<Inst> }, // write a variable (no value)
}

/// A variable slot — a Lua local. Read with `Load`, written with `Store`.
#[derive(Clone, Debug)]
pub struct Cell {
    pub name: Option<Ustr>,
}

/// How a block ends. Control flow lives only here.
#[derive(Clone, Debug)]
pub enum Term {
    Return(Option<Idx<Inst>>),
    Unreachable,
}

/// A basic block: its instructions in order, then one terminator.
#[derive(Clone, Debug)]
pub struct Block {
    pub insts: Vec<Idx<Inst>>,
    pub term: Term,
}

/// A function — a CFG.
#[derive(Clone, Debug)]
pub struct MirFunc {
    pub name: Ustr,
    pub insts: Arena<Inst>,
    pub cells: Arena<Cell>,
    pub blocks: Arena<Block>,
    pub entry: Idx<Block>,
}

impl MirFunc {
    pub fn new(name: Ustr) -> Self {
        let mut blocks = Arena::default();
        let entry = blocks.alloc(Block {
            insts: Vec::new(),
            term: Term::Unreachable,
        });
        Self {
            name,
            insts: Arena::default(),
            cells: Arena::default(),
            blocks,
            entry,
        }
    }

    pub fn entry(&self) -> Idx<Block> {
        self.entry
    }

    pub fn new_cell(&mut self, name: Option<Ustr>) -> Idx<Cell> {
        self.cells.alloc(Cell { name })
    }

    /// Allocate an instruction and append it to `block`; return its id (= value).
    pub fn emit(&mut self, block: Idx<Block>, inst: Inst) -> Idx<Inst> {
        let id = self.insts.alloc(inst);
        self.blocks[block].insts.push(id);
        id
    }

    pub fn set_term(&mut self, block: Idx<Block>, term: Term) {
        self.blocks[block].term = term;
    }
}
