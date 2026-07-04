use crate::prelude::*;

#[derive(Clone, Debug)]
pub enum Expr {
    Var(Ustr),
    FuncDecl {
        input_variable: Ustr,
        body: Box<Expr>,
    },

    FunApp {
        e1: Box<Expr>,
        e2: Box<Expr>,
    },

    Ann {
        expr: Box<Expr>,
        ty: Type,
    },

    Integer(i64),
    Boolean(bool),

    If {
        condition: Box<Expr>,
        then_expr: Box<Expr>,
        else_expr: Box<Expr>,
    },

    PredOp {
        op: PredOp,
        e1: Box<Expr>,
        e2: Box<Expr>,
    },

    BinOp {
        op: BinOp,
        e1: Box<Expr>,
        e2: Box<Expr>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Type {
    Integer,
    Boolean,
    Fun { param: Box<Type>, ret: Box<Type> },
}

#[derive(Clone, Debug)]
pub enum PredOp {
    And,
    Or,
    Eq,
    Neq,
    Lt,
    Gt,
    Leq,
    Geq,
}

#[derive(Clone, Debug)]
pub enum BinOp {
    Plus,
    Minus,
    Mult,
    Div,
}

pub fn expr_int(x: i64) -> Box<Expr> {
    Box::new(Expr::Integer(x))
}

pub fn expr_bool(x: bool) -> Box<Expr> {
    Box::new(Expr::Boolean(x))
}

pub fn expr_var(name: impl Into<Ustr>) -> Box<Expr> {
    Box::new(Expr::Var(name.into()))
}

pub fn expr_ann(x: Expr, ty: Type) -> Expr {
    Expr::Ann {
        expr: Box::new(x),
        ty,
    }
}

pub fn expr_pred_op(op: PredOp, e1: Box<Expr>, e2: Box<Expr>) -> Box<Expr> {
    Box::new(Expr::PredOp { op, e1, e2 })
}

pub fn expr_bin_op(op: BinOp, e1: Box<Expr>, e2: Box<Expr>) -> Box<Expr> {
    Box::new(Expr::BinOp { op, e1, e2 })
}

pub fn add(left: u64, right: u64) -> u64 {
    left + right
}

pub struct Gamma {
    pub vars: FxHashMap<Ustr, Type>,
}

// Takes an expr and infers its type given a context.
pub fn infer_type(c: &Gamma, expr: &Expr) -> Option<Type> {
    match expr {
        Expr::Integer(_) => Some(Type::Integer),
        Expr::Boolean(_) => Some(Type::Boolean),

        //  (x : t) \in C
        //  -------------  BT-VAR
        //   C |- x => t
        Expr::Var(name) => c.vars.get(name).cloned(),

        //    C |- e <= t
        // ---------------  BT-ANN
        // C |- e : t => t
        Expr::Ann { expr, ty } => check_type(c, expr, ty),

        // C |- e1 => t1 -> t2    C |- e2 <= t1
        // -----------------------------------  BT-APP
        //          C |- e1 e2 => t2
        Expr::FunApp { e1, e2 } => {
            // TODO: use t1?
            if let Some(Type::Fun {
                param: _t1,
                ret: t2,
            }) = infer_type(c, e1)
            {
                check_type(c, e2, &t2)
            } else {
                None
            }
        }

        // Expr::FunApp { e1, e2 } => match (infer_type(c, e1), infer_type(c, e2)) {
        //     (
        //         Some(Type::Fun {
        //             param: t_param,
        //             ret: t_ret,
        //         }),
        //         Some(t_arg),
        //     ) => {
        //         if *t_param == t_arg {
        //             Some(*t_ret)
        //         } else {
        //             None
        //         }
        //     }
        //     _ => None,
        // },
        _ => None,
    }
}

// Takes a context, an expression and its type, and returns the same type
// on success.
pub fn check_type(c: &Gamma, expr: &Expr, ty: &Type) -> Option<Type> {
    match expr {
        //   C, x : t1 |- e <= t2
        // ------------------------  BT-FUNDECL
        // C |- \x -> e <= t1 -> t2
        Expr::FuncDecl {
            // TODO: insert x into c
            input_variable: _x,
            body: e,
        } => match ty {
            Type::Fun { param: t1, ret: t2 } => {
                check_type(c, e, t2)?;
                Some(*t1.clone())
            }

            _ => None,
        },

        // C |- e => t
        // -----------  BT-CHECKINFER
        // C |- e <= t
        _ => {
            let inferred = infer_type(c, expr)?;
            if inferred == *ty {
                Some(inferred)
            } else {
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gamma() -> Gamma {
        Gamma {
            vars: Default::default(),
        }
    }

    fn assert_infer(expr: &Expr, ty: &Type) {
        match infer_type(&gamma(), expr) {
            Some(exp_ty) => assert_eq!(ty, &exp_ty),
            None => panic!(
                "Expr {:?} was expected to infer to {:?}, but failed",
                expr, ty
            ),
        }
    }

    fn assert_infer_fails(expr: &Expr) {
        match infer_type(&gamma(), expr) {
            Some(exp_ty) => panic!(
                "Expr {:?} was expected to fail to infer, but inferred to {:?}",
                expr, exp_ty
            ),
            None => { /* expected */ }
        }
    }

    #[test]
    fn simple_infer() {
        assert_infer(&expr_int(5), &Type::Integer);
        assert_infer(&expr_bool(true), &Type::Boolean);
        assert_infer(&expr_ann(*expr_int(5), Type::Integer), &Type::Integer);
        assert_infer_fails(&expr_ann(*expr_bool(true), Type::Integer));
    }

    #[test]
    fn it_works() {
        let if_expr = Expr::If {
            condition: Box::new(Expr::PredOp {
                op: PredOp::Lt,
                e1: expr_int(5),
                e2: expr_int(10),
            }),
            then_expr: expr_int(2),
            else_expr: expr_int(1),
        };

        let _if_expr = expr_ann(if_expr, Type::Integer);

        let _bool_expr = expr_ann(
            Expr::PredOp {
                op: PredOp::And,
                e1: expr_bool(true),
                e2: expr_bool(false),
            },
            Type::Boolean,
        );

        let _fun_expr: Expr = Expr::FuncDecl {
            input_variable: "x".into(),
            body: Box::new(Expr::If {
                condition: expr_pred_op(PredOp::Lt, expr_var("x"), expr_int(2)),
                then_expr: expr_bin_op(BinOp::Plus, expr_var("x"), expr_int(1)),
                else_expr: expr_bin_op(BinOp::Plus, expr_var("x"), expr_int(2)),
            }),
        };

        let result = add(2, 2);
        assert_eq!(result, 4);
    }
}
