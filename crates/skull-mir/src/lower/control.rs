//! Control flow that produces a VALUE: arithmetic and comparison lowering,
//! `if` as expression and as statement, the store-and-join merge helpers, and
//! the short-circuiting `&&` / `||` / `??`.
use super::*;

impl Builder<'_> {
    pub(crate) fn lower_native_arith(
        &mut self,
        op: &str,
        lt: ArithTy,
        rt: ArithTy,
        a: Idx<Expr>,
        b: Idx<Expr>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let lv = self.lower_expr(a)?;
        let rv = self.lower_expr(b)?;
        // Promote an int operand exactly when `arith_inst` picks a Float* variant
        // (`x + 1`, `f * i`, and `/` which is always float division): the op is
        // typed float, so an int operand is used-as-float. String concat and pure
        // int ops leave their operands untouched.
        let str_op = op == "__op_add" && (lt == ArithTy::Str || rt == ArithTy::Str);
        let to_float = !str_op && (op == "__op_div" || lt == ArithTy::Float || rt == ArithTy::Float);
        let lv = self.coerce_i2f(lv, lt == ArithTy::Int, to_float);
        let rv = self.coerce_i2f(rv, rt == ArithTy::Int, to_float);
        let inst = arith_inst(op, lt, rt, lv, rv)?;
        Ok(self.emit_cur(inst))
    }

    pub(crate) fn lower_binop(
        &mut self,
        op: BinOp,
        lhs: Idx<Expr>,
        rhs: Idx<Expr>,
    ) -> Result<Idx<Inst>, CompilerError> {
        match op {
            BinOp::Lt | BinOp::Leq | BinOp::Gt | BinOp::Geq => {
                // Lua `<`/`>`/... orders numbers AND strings (lexicographic), and
                // the typed Int*/Float* variants render the SAME operator — so a
                // string or generic-`T` comparison uses the non-float variant.
                // bone has type-checked both sides to the same orderable type
                // (rock emits a native compare here too). A genuinely unorderable
                // operand would be a type error upstream.
                let lf = self.arith_ty(lhs);
                let rf = self.arith_ty(rhs);
                let float = lf == Some(ArithTy::Float) || rf == Some(ArithTy::Float);
                let lv = self.lower_expr(lhs)?;
                let rv = self.lower_expr(rhs)?;
                // A mixed float compare (`x > 0`, `i < f`) promotes its int side --
                // both Float* operands must arrive float (GLSL ES has no implicit
                // int->float in a compare).
                let lv = self.coerce_i2f(lv, lf == Some(ArithTy::Int), float);
                let rv = self.coerce_i2f(rv, rf == Some(ArithTy::Int), float);
                let inst = match (op, float) {
                    (BinOp::Lt, false) => Inst::IntLt(lv, rv),
                    (BinOp::Lt, true) => Inst::FloatLt(lv, rv),
                    (BinOp::Leq, false) => Inst::IntLeq(lv, rv),
                    (BinOp::Leq, true) => Inst::FloatLeq(lv, rv),
                    (BinOp::Gt, false) => Inst::IntGt(lv, rv),
                    (BinOp::Gt, true) => Inst::FloatGt(lv, rv),
                    (BinOp::Geq, false) => Inst::IntGeq(lv, rv),
                    (BinOp::Geq, true) => Inst::FloatGeq(lv, rv),
                    _ => unreachable!(),
                };
                Ok(self.emit_cur(inst))
            }
            BinOp::Eq | BinOp::NotEq => {
                // `x == null` / `x != null` on a flat enum: the tag slot is the null
                // discriminator (nil tag == null). A threaded value has no box to
                // compare, and materializing one would compare a table to nil
                // (always false) -- so compare the tag directly.
                let null_side = if self.is_null_lit(rhs) {
                    Some(lhs)
                } else if self.is_null_lit(lhs) {
                    Some(rhs)
                } else {
                    None
                };
                if let Some(v) = null_side {
                    // A `V?` place read here is read for its PRESENCE: the copy
                    // such a read otherwise takes would allocate a cdata per nil
                    // test (`mouse_screen`'s `sim_mouse_override`, every frame).
                    if self.nobox_value_name(v).is_none()
                        && !self.expr_is_flat_enum(v)
                        && self.nullable_place_value_of(v).is_some()
                    {
                        let outer = self.presence_only.replace(v);
                        let value = self.lower_expr(v);
                        self.presence_only = outer;
                        let value = value?;
                        let nil = self.emit_cur(Inst::Const(Lit::Nil));
                        return Ok(self.emit_cur(if matches!(op, BinOp::Eq) {
                            Inst::Eq(value, nil)
                        } else {
                            Inst::NotEq(value, nil)
                        }));
                    }
                    if self.nobox_value_name(v).is_some() {
                        let repr = self.lower_expr_repr(v)?;
                        let present = match repr {
                            ValueRepr::Group {
                                present: Some(value),
                                ..
                            } => value,
                            ValueRepr::Group { present: None, .. } => {
                                self.emit_cur(Inst::Const(Lit::Bool(true)))
                            }
                            other => {
                                let value = self.pack(other);
                                let nil = self.emit_cur(Inst::Const(Lit::Nil));
                                self.emit_cur(Inst::NotEq(value, nil))
                            }
                        };
                        return Ok(if matches!(op, BinOp::Eq) {
                            self.emit_cur(Inst::Not(present))
                        } else {
                            present
                        });
                    }
                    // The tuple twin of the nobox-value case above: a `(..)?`
                    // whose element splats carries its own presence SLOT, and
                    // that slot is the answer. Packing instead would build a
                    // `MakeTuple` -- a table, never nil -- so `== null` would be
                    // false even on the absent arm.
                    if self.nullable_tuple_wire_shape(v)?.is_some() {
                        let repr = self.lower_expr_repr(v)?;
                        let present = match repr {
                            ValueRepr::Tuple {
                                present: Some(value),
                                ..
                            } => value,
                            other => {
                                let value = self.pack(other);
                                let nil = self.emit_cur(Inst::Const(Lit::Nil));
                                self.emit_cur(Inst::NotEq(value, nil))
                            }
                        };
                        return Ok(if matches!(op, BinOp::Eq) {
                            self.emit_cur(Inst::Not(present))
                        } else {
                            present
                        });
                    }
                    if self.expr_is_flat_enum(v) {
                        let tag = match self.lower_expr_repr(v)? {
                            ValueRepr::Group { parts, .. } => {
                                self.pack(parts.into_iter().next().expect("flat enum group has a tag leaf"))
                            }
                            // a boxed flat enum: nil-or-table, compare directly.
                            other => self.pack(other),
                        };
                        let nil = self.emit_cur(Inst::Const(Lit::Nil));
                        return Ok(self.emit_cur(if matches!(op, BinOp::Eq) {
                            Inst::Eq(tag, nil)
                        } else {
                            Inst::NotEq(tag, nil)
                        }));
                    }
                }
                if let (Some(ln), Some(rn)) =
                    (self.nobox_value_name(lhs), self.nobox_value_name(rhs))
                {
                    if ln == rn {
                        let mut shape = value_shape_of_nominal(self.ws, ln)
                            .ok_or_else(|| gap(format!("nobox value `{ln}` has no scalar shape")))?;
                        shape.nullable =
                            self.expr_is_nullable(lhs) || self.expr_is_nullable(rhs);
                        let lhs_repr = self.lower_expr_repr(lhs)?;
                        let lhs_leaves = self.repr_to_leaves(&lhs_repr, &shape);
                        let rhs_repr = self.lower_expr_repr(rhs)?;
                        let rhs_leaves = self.repr_to_leaves(&rhs_repr, &shape);
                        let equal = self.emit_all_equal(&lhs_leaves, &rhs_leaves);
                        return Ok(if matches!(op, BinOp::Eq) {
                            equal
                        } else {
                            self.emit_cur(Inst::Not(equal))
                        });
                    }
                }
                let scalar = self.eq_is_scalar(lhs)
                    || self.eq_is_scalar(rhs)
                    || self.is_null_lit(lhs)
                    || self.is_null_lit(rhs);
                let lv = self.lower_expr(lhs)?;
                let rv = self.lower_expr(rhs)?;
                if !scalar {
                    // Non-scalar equality is STRUCTURAL, not Lua reference
                    // equality: route through the stdlib `__op_eq`/`__op_ne`,
                    // which walk fields structurally via `pairs()` (cdata value
                    // structs hit the `==` primitive fast path = byte-value
                    // compare).
                    let name = if matches!(op, BinOp::Eq) { "__op_eq" } else { "__op_ne" };
                    let func = self.resolve_op_helper(name)?;
                    return Ok(self.emit_cur(Inst::Call { func, args: vec![lv, rv] }));
                }
                // Mixed `float == int` promotes the int side (untyped `Eq` renders a
                // single `==`; both operands must arrive same-typed for GLSL).
                let lf = self.arith_ty(lhs);
                let rf = self.arith_ty(rhs);
                let float = lf == Some(ArithTy::Float) || rf == Some(ArithTy::Float);
                let lv = self.coerce_i2f(lv, lf == Some(ArithTy::Int), float);
                let rv = self.coerce_i2f(rv, rf == Some(ArithTy::Int), float);
                Ok(self.emit_cur(if matches!(op, BinOp::Eq) {
                    Inst::Eq(lv, rv)
                } else {
                    Inst::NotEq(lv, rv)
                }))
            }
            BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr => {
                let lv = self.lower_expr(lhs)?;
                let rv = self.lower_expr(rhs)?;
                let inst = match op {
                    BinOp::BitAnd => Inst::BitAnd(lv, rv),
                    BinOp::BitOr => Inst::BitOr(lv, rv),
                    BinOp::BitXor => Inst::BitXor(lv, rv),
                    BinOp::Shl => Inst::Shl(lv, rv),
                    BinOp::Shr => Inst::Shr(lv, rv),
                    _ => unreachable!(),
                };
                Ok(self.emit_cur(inst))
            }
            // Short-circuit operators: the rhs may not evaluate, so they lower
            // to branches with the result flowing through a cell.
            BinOp::And => self.lower_and(lhs, rhs),
            BinOp::Or => self.lower_or(lhs, rhs),
            BinOp::NullCoalesce => self.lower_nullcoalesce(lhs, rhs),
        }
    }

    /// `if c { a } else { b }` as a value: branch, store each arm's value into a
    /// shared result **cell**, load it in the join. No block params (P1). A
    /// no-`else` arm yields unit. An arm that diverges (returns/breaks) skips its
    /// store + join-goto.
    pub(crate) fn lower_if(
        &mut self,
        cond: Idx<Expr>,
        then_branch: Idx<Expr>,
        else_branch: Option<Idx<Expr>>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let repr =
            self.lower_if_repr(cond, then_branch, else_branch, RetSlot::Opaque)?;
        Ok(self.pack(repr))
    }

    pub(crate) fn lower_if_repr(
        &mut self,
        cond: Idx<Expr>,
        then_branch: Idx<Expr>,
        else_branch: Option<Idx<Expr>>,
        shape: RetSlot,
    ) -> Result<ValueRepr, CompilerError> {
        let results: Vec<Idx<Cell>> =
            (0..shape.slot_count()).map(|_| self.f.new_cell(None)).collect();
        let cond_v = self.lower_expr(cond)?;
        let then_bb = self.f.new_block();
        let else_bb = self.f.new_block();
        let join = self.f.new_block();
        self.f.set_term(
            self.cur,
            Term::Branch {
                cond: cond_v,
                then_bb,
                else_bb,
            },
        );

        self.cur = then_bb;
        let tv = self.lower_expr_repr(then_branch)?;
        self.store_repr_and_join(&results, &shape, tv, join)?;

        self.cur = else_bb;
        let ev = match else_branch {
            Some(eb) => self.lower_expr_repr(eb)?,
            None => ValueRepr::Scalar(self.emit_cur(Inst::Const(Lit::Nil))),
        };
        self.store_repr_and_join(&results, &shape, ev, join)?;

        self.cur = join;
        let slots: Vec<Idx<Inst>> =
            results.iter().map(|&cell| self.emit_cur(Inst::Load(cell))).collect();
        let mut idx = 0;
        Ok(self.slot_repr(&shape, &slots, &mut idx))
    }

    /// `if` in STATEMENT position (value discarded): branch + arms-as-statements
    /// + join, with NO result cell / store / load. Arms recurse statement-position
    /// so nested if/block/match also skip their result cells.
    pub(crate) fn lower_if_stmt(
        &mut self,
        cond: Idx<Expr>,
        then_branch: Idx<Expr>,
        else_branch: Option<Idx<Expr>>,
    ) -> Result<(), CompilerError> {
        let cond_v = self.lower_expr(cond)?;
        let then_bb = self.f.new_block();
        let else_bb = self.f.new_block();
        let join = self.f.new_block();
        self.f.set_term(self.cur, Term::Branch { cond: cond_v, then_bb, else_bb });

        self.cur = then_bb;
        self.lower_stmt_expr(then_branch)?;
        if self.is_open() {
            self.f.set_term(self.cur, Term::Goto(join));
        }

        self.cur = else_bb;
        if let Some(eb) = else_branch {
            self.lower_stmt_expr(eb)?;
        }
        if self.is_open() {
            self.f.set_term(self.cur, Term::Goto(join));
        }

        self.cur = join;
        Ok(())
    }

    /// The nil-guarded join every `?.` form shares, storing PER SLOT.
    ///
    /// The three safe-nav lowerings differ only in what the non-nil arm computes,
    /// and each joined its arms through ONE result cell. A `nobox` value is leaves
    /// plus a presence tag and does not fit one cell, so it packed to a box --
    /// the materialization the mark forbids. This is the same multi-cell join
    /// `if`/`match` as values already use ([`Self::store_repr_and_join`]); safe-nav
    /// was simply never dispatched through it.
    ///
    /// The nil arm needs no special case: `repr_to_leaves` already folds a
    /// `Const(Nil)` at a nullable shape to `false` plus one nil per leaf.
    ///
    /// `value_arm` receives the receiver VALUE, lowered once before the test --
    /// re-lowering the receiver expression on the non-nil arm would run its side
    /// effects twice (`none()?.v` would call `none` again).
    pub(crate) fn safe_nav_repr(
        &mut self,
        receiver: Idx<Expr>,
        shape: RetSlot,
        value_arm: impl FnOnce(&mut Self, Idx<Inst>) -> Result<ValueRepr, CompilerError>,
    ) -> Result<ValueRepr, CompilerError> {
        let recv = self.lower_safe_nav_receiver(receiver)?;
        let is_nil = self.safe_nav_absent_cond(&recv);
        let results: Vec<Idx<Cell>> =
            (0..shape.slot_count()).map(|_| self.f.new_cell(None)).collect();
        let nil_bb = self.f.new_block();
        let val_bb = self.f.new_block();
        let join = self.f.new_block();
        self.f.set_term(self.cur, Term::Branch { cond: is_nil, then_bb: nil_bb, else_bb: val_bb });

        self.cur = nil_bb;
        let n = self.emit_cur(Inst::Const(Lit::Nil));
        self.store_repr_and_join(&results, &shape, ValueRepr::Scalar(n), join)?;

        self.cur = val_bb;
        let recv_v = self.safe_nav_present_value(recv);
        let v = value_arm(self, recv_v)?;
        self.store_repr_and_join(&results, &shape, v, join)?;

        self.cur = join;
        let slots: Vec<Idx<Inst>> =
            results.iter().map(|&cell| self.emit_cur(Inst::Load(cell))).collect();
        let mut idx = 0;
        Ok(self.slot_repr(&shape, &slots, &mut idx))
    }

    /// If the current block is still open, store `value` into `result` and jump
    /// to `join` (the fall-through merge). A diverged arm leaves it untouched.
    pub(crate) fn store_and_join(&mut self, result: Idx<Cell>, value: Idx<Inst>, join: Idx<Block>) {
        if self.is_open() {
            self.emit_cur(Inst::Store {
                cell: result,
                value,
            });
            self.f.set_term(self.cur, Term::Goto(join));
        }
    }

    pub(crate) fn store_repr_and_join(
        &mut self,
        results: &[Idx<Cell>],
        shape: &RetSlot,
        value: ValueRepr,
        join: Idx<Block>,
    ) -> Result<(), CompilerError> {
        if !self.is_open() {
            return Ok(());
        }
        let slots = self.repr_to_slot_values(value, shape);
        if results.len() != slots.len() {
            return Err(gap(format!(
                "aggregate join produced {} slots for {} result cells",
                slots.len(),
                results.len()
            )));
        }
        for (&cell, value) in results.iter().zip(slots) {
            self.emit_cur(Inst::Store { cell, value });
        }
        self.f.set_term(self.cur, Term::Goto(join));
        Ok(())
    }

    /// `a && b` == `if a then b else false`.
    pub(crate) fn lower_and(&mut self, lhs: Idx<Expr>, rhs: Idx<Expr>) -> Result<Idx<Inst>, CompilerError> {
        let result = self.f.new_cell(None);
        let lv = self.lower_expr(lhs)?;
        let rhs_bb = self.f.new_block();
        let else_bb = self.f.new_block();
        let join = self.f.new_block();
        self.f.set_term(
            self.cur,
            Term::Branch {
                cond: lv,
                then_bb: rhs_bb,
                else_bb,
            },
        );
        self.cur = rhs_bb;
        let rv = self.lower_expr(rhs)?;
        self.store_and_join(result, rv, join);
        self.cur = else_bb;
        let f = self.emit_cur(Inst::Const(Lit::Bool(false)));
        self.store_and_join(result, f, join);
        self.cur = join;
        Ok(self.emit_cur(Inst::Load(result)))
    }

    /// `a || b` == `if a then true else b`.
    pub(crate) fn lower_or(&mut self, lhs: Idx<Expr>, rhs: Idx<Expr>) -> Result<Idx<Inst>, CompilerError> {
        let result = self.f.new_cell(None);
        let lv = self.lower_expr(lhs)?;
        let then_bb = self.f.new_block();
        let rhs_bb = self.f.new_block();
        let join = self.f.new_block();
        self.f.set_term(
            self.cur,
            Term::Branch {
                cond: lv,
                then_bb,
                else_bb: rhs_bb,
            },
        );
        self.cur = then_bb;
        let t = self.emit_cur(Inst::Const(Lit::Bool(true)));
        self.store_and_join(result, t, join);
        self.cur = rhs_bb;
        let rv = self.lower_expr(rhs)?;
        self.store_and_join(result, rv, join);
        self.cur = join;
        Ok(self.emit_cur(Inst::Load(result)))
    }

    /// `a ?? b` == `if a == nil then b else a` — `a` evaluated once.
    pub(crate) fn lower_nullcoalesce(
        &mut self,
        lhs: Idx<Expr>,
        rhs: Idx<Expr>,
    ) -> Result<Idx<Inst>, CompilerError> {
        let result = self.f.new_cell(None);
        let lv = self.lower_expr(lhs)?;
        let nil = self.emit_cur(Inst::Const(Lit::Nil));
        let is_nil = self.emit_cur(Inst::Eq(lv, nil));
        let rhs_bb = self.f.new_block();
        let lhs_bb = self.f.new_block();
        let join = self.f.new_block();
        self.f.set_term(
            self.cur,
            Term::Branch {
                cond: is_nil,
                then_bb: rhs_bb,
                else_bb: lhs_bb,
            },
        );
        self.cur = rhs_bb;
        let rv = self.lower_expr(rhs)?;
        self.store_and_join(result, rv, join);
        // lhs non-nil: reuse `lv` (its defining block dominates this one).
        self.cur = lhs_bb;
        self.store_and_join(result, lv, join);
        self.cur = join;
        Ok(self.emit_cur(Inst::Load(result)))
    }
}
