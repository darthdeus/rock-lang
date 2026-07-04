//! HIR -> MIR lowering. Walk the (func, body) HIR; emit instructions into
//! blocks; variables become cells.

use bone_semantic::{
    defs::{ModuleId, Workspace},
    hir::{self, BindingInfo, BodyOwner, Expr, Function, Statement},
    or_fail,
};
use comfy_la_arena::{ArenaMap, Idx};
use skull_parser::CompilerError;

use crate::mir::{Block, Cell, Inst, MirFunc, Term};

fn gap(msg: impl std::fmt::Display) -> CompilerError {
    CompilerError::new_no_location(format!("bone-mir: {msg}"))
}

struct Lower<'a> {
    mir: MirFunc,
    body: &'a hir::Body,
    cells: ArenaMap<Idx<BindingInfo>, Idx<Cell>>,
}

impl Lower<'_> {
    /// Lower one expression into `block`, returning the instruction (= value) it
    /// produces.
    fn lower_expr(&mut self, block: Idx<Block>, expr: Idx<Expr>) -> Result<Idx<Inst>, CompilerError> {
        match self.body.expr_arena[expr] {
            Expr::Int(n) => Ok(self.mir.emit(block, Inst::Const(n))),

            Expr::Var(b) => {
                let cell = *self
                    .cells
                    .get(b)
                    .ok_or_else(|| gap("read of unbound variable"))?;
                Ok(self.mir.emit(block, Inst::Load(cell)))
            }

            Expr::Block { statements, tail } => {
                for s in statements {
                    self.lower_stmt(block, s)?;
                }
                let tail = tail.ok_or_else(|| gap("value block has no tail"))?;
                self.lower_expr(block, tail)
            }

            other => Err(gap(format!("unsupported expr: {other:?}"))),
        }
    }

    fn lower_stmt(&mut self, block: Idx<Block>, stmt: Idx<Statement>) -> Result<(), CompilerError> {
        // Statement isn't Copy; copy the field idxs out of the borrow first.
        let body = self.body;
        let (name, value) = match &body.statement_arena[stmt] {
            Statement::Let { name, value } => (*name, *value),
            other => return Err(gap(format!("unsupported statement: {other:?}"))),
        };

        let v = self.lower_expr(block, value)?;
        let cell = self.mir.new_cell(Some(body.binding_arena[name].name));
        self.mir.emit(block, Inst::Store { cell, value: v });
        self.cells.insert(name, cell);
        Ok(())
    }
}

pub fn lower_function(
    ws: &Workspace,
    mid: ModuleId,
    fid: Idx<Function>,
) -> Result<MirFunc, CompilerError> {
    let module = ws.module(mid);
    let func = &module.functions[fid];
    let body = &module.bodies[&BodyOwner::Function(fid)];

    let mut lower = Lower {
        mir: MirFunc::new(func.name),
        body,
        cells: ArenaMap::default(),
    };

    let entry = lower.mir.entry();
    let root = or_fail!(body.root, func.span, "a non-extern fn needs a body root")?;
    let result = lower.lower_expr(entry, root)?;
    lower.mir.set_term(entry, Term::Return(Some(result)));

    Ok(lower.mir)
}
