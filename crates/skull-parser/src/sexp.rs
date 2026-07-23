//! S-expression rendering, byte-for-byte compatible with rock's
//! `Expr::to_sexp` / `Block::to_sexp` / `Statement::to_sexp` for the
//! subset skull-parser covers. This is the correctness oracle: the
//! A/B guard asserts skull's s-expr equals rock's, file by file.

use crate::ast::*;

impl Ast {
    /// `(module item item ...)` over all items in source order — the
    /// whole-module oracle. Mirrors rock (harness joins `Item::to_sexp`).
    pub fn module_sexp(&self) -> String {
        let items = self
            .items
            .iter()
            .map(|it| self.item_sexp(it))
            .collect::<Vec<_>>()
            .join(" ");
        format!("(module {})", items)
    }

    /// Id-free structural s-expr for one top-level item. Byte-compatible with
    /// rock's `Item::to_sexp`, including a leading annotation prefix.
    pub fn item_sexp(&self, item: &Item) -> String {
        let prefix = if item.annotations.is_empty() {
            String::new()
        } else {
            let anns = item
                .annotations
                .iter()
                .map(|a| {
                    if a.args.is_empty() {
                        format!("(@{})", a.name.name)
                    } else {
                        let args = a
                            .args
                            .iter()
                            .map(|arg| match &arg.name {
                                Some(n) => format!("{}={}", n.name, self.expr_sexp(arg.value)),
                                None => self.expr_sexp(arg.value),
                            })
                            .collect::<Vec<_>>()
                            .join(" ");
                        format!("(@{} {})", a.name.name, args)
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            format!("{} ", anns)
        };
        let body = match &item.kind {
            ItemKind::Function(f) => {
                let tag = if f.is_macro {
                    "macro"
                } else if f.is_coroutine {
                    "co-fn"
                } else {
                    "fn"
                };
                let ret = f
                    .return_ty
                    .map(|t| self.type_sexp(t))
                    .unwrap_or_else(|| "()".to_string());
                let body = match &f.body {
                    FunctionBody::Rock(b) => self.block_sexp(*b),
                    FunctionBody::Lua(_) => "(lua)".to_string(),
                    FunctionBody::ExternC => "(extern)".to_string(),
                    FunctionBody::ExternLua => "(extern-lua)".to_string(),
                    FunctionBody::Ffi(lib) => format!("(ffi {:?})", lib.as_str()),
                };
                format!(
                    "({} {}{} {} {} {})",
                    tag,
                    f.name.name,
                    self.generics_sexp(&f.generics),
                    self.params_sexp(&f.params),
                    ret,
                    body
                )
            }
            ItemKind::Struct(s) => {
                let tag = if s.is_value { "value-struct" } else { "struct" };
                let fields = s
                    .fields
                    .iter()
                    .map(|f| self.field_sexp(f))
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("({} {}{} {})", tag, s.name.name, self.generics_sexp(&s.generics), fields)
            }
            ItemKind::Enum(e) => {
                let variants = e
                    .variants
                    .iter()
                    .map(|v| match &v.kind {
                        EnumVariantKind::Unit => format!("(variant {})", v.name.name),
                        EnumVariantKind::Tuple(tys) => format!(
                            "(variant {} (tuple {}))",
                            v.name.name,
                            tys.iter().map(|&t| self.type_sexp(t)).collect::<Vec<_>>().join(" ")
                        ),
                        EnumVariantKind::Record(fs) => format!(
                            "(variant {} (record {}))",
                            v.name.name,
                            fs.iter().map(|f| self.field_sexp(f)).collect::<Vec<_>>().join(" ")
                        ),
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("(enum {}{} {})", e.name.name, self.generics_sexp(&e.generics), variants)
            }
            ItemKind::Newtype(n) => format!(
                "(newtype {}{} {})",
                n.name.name,
                self.generics_sexp(&n.generics),
                self.type_sexp(n.wrapped)
            ),
            ItemKind::Decorator(d) => format!(
                "(decorator {} {} {})",
                d.name.name,
                self.params_sexp(&d.params),
                self.block_sexp(d.body)
            ),
            ItemKind::Global(g) => {
                let tag = if g.is_const { "const" } else { "global" };
                let ty = g.ty.map(|t| self.type_sexp(t)).unwrap_or_else(|| "()".to_string());
                let val = g.value.map(|v| self.expr_sexp(v)).unwrap_or_else(|| "null".to_string());
                format!("({} {} {} {})", tag, g.name.name, ty, val)
            }
            ItemKind::CompilerDirective(d) => match d {
                CompilerDirective::If(c) => format!("(#if {:?})", c.as_str()),
                CompilerDirective::ElseIf(c) => format!("(#elseif {:?})", c.as_str()),
                CompilerDirective::Else => "(#else)".to_string(),
                CompilerDirective::End => "(#end)".to_string(),
            },
            ItemKind::Comment(c) => format!("(comment {:?})", c.as_str()),
            // Only appears pre-expansion; cooked modules never contain one.
            ItemKind::MacroInvocation { name, args, sigil } => format!(
                "(macro-call {}{} {})",
                name.name,
                if *sigil { "!" } else { "" },
                args.iter().map(|&a| self.expr_sexp(a)).collect::<Vec<_>>().join(" "),
            ),
        };
        format!("{}{}", prefix, body)
    }

    fn generics_sexp(&self, gs: &[Generic]) -> String {
        if gs.is_empty() {
            return String::new();
        }
        let inner = gs
            .iter()
            .map(|g| {
                let mut s = g.name.name.to_string();
                if let Some(b) = g.bound {
                    s = format!("{}:{}", s, self.type_sexp(b));
                }
                if let Some(d) = g.default {
                    s = format!("{}={}", s, self.type_sexp(d));
                }
                s
            })
            .collect::<Vec<_>>()
            .join(" ");
        format!(" <{}>", inner)
    }

    fn params_sexp(&self, ps: &[Param]) -> String {
        let inner = ps
            .iter()
            .map(|p| {
                let mut s = format!("({} {}", p.name.name, self.type_sexp(p.ty));
                if p.is_variadic {
                    s.push_str(" ...");
                }
                if let Some(d) = p.default {
                    s.push_str(&format!(" = {}", self.expr_sexp(d)));
                }
                s.push(')');
                s
            })
            .collect::<Vec<_>>()
            .join(" ");
        format!("({})", inner)
    }

    fn field_sexp(&self, f: &Field) -> String {
        match f.default {
            Some(d) => format!("(field {} {} {})", f.name.name, self.type_sexp(f.ty), self.expr_sexp(d)),
            None => format!("(field {} {})", f.name.name, self.type_sexp(f.ty)),
        }
    }

    /// `(block s1 s2 ...)` — matches rock's `Block::to_sexp`. Used for
    /// function bodies and `if`/`else` branch bodies.
    pub fn block_sexp(&self, b: Idx<BlockData>) -> String {
        let stmts = self.blocks[b]
            .statements
            .iter()
            .map(|&s| self.stmt_sexp(s))
            .collect::<Vec<_>>()
            .join(" ");
        format!("(block {})", stmts)
    }

    pub fn stmt_sexp(&self, s: Idx<StatementData>) -> String {
        match &self.stmts[s].kind {
            StatementKind::Expression(e) => self.expr_sexp(*e),
            StatementKind::Return { value, .. } => format!(
                "(return {})",
                value.map(|e| self.expr_sexp(e)).unwrap_or_else(|| "()".to_string())
            ),
            StatementKind::Let(l) => format!(
                "(let {} {} {})",
                binding_sexp(&l.binding),
                l.ty
                    .map(|t| self.type_sexp(t))
                    .unwrap_or_else(|| "()".to_string()),
                l.value
                    .map(|e| self.expr_sexp(e))
                    .unwrap_or_else(|| "null".to_string()),
            ),
            StatementKind::Static(s) => format!(
                "(static {} {} {})",
                s.name.name,
                s.ty.map(|t| self.type_sexp(t)).unwrap_or_else(|| "()".to_string()),
                self.expr_sexp(s.value),
            ),
            StatementKind::NestedFn(f) => format!(
                "(fn {} {} {} {})",
                f.name.name,
                self.params_sexp(&f.params),
                f.return_ty.map(|t| self.type_sexp(t)).unwrap_or_else(|| "()".to_string()),
                self.block_sexp(f.body),
            ),
            StatementKind::Break { .. } => "(break)".to_string(),
            StatementKind::Continue { .. } => "(continue)".to_string(),
            StatementKind::For(f) => format!(
                "(for {} {} {})",
                binding_sexp(&f.var),
                self.expr_sexp(f.iter),
                self.block_sexp(f.body)
            ),
            StatementKind::While(w) => {
                format!("(while {} {})", self.expr_sexp(w.condition), self.block_sexp(w.body))
            }
            StatementKind::WhileLet(w) => format!(
                "(while-let {} {} {})",
                self.pattern_sexp(&w.pattern),
                self.expr_sexp(w.value),
                self.block_sexp(w.body)
            ),
            StatementKind::Defer(d) => match d {
                DeferKind::Block(b) => format!("(defer {})", self.block_sexp(*b)),
                DeferKind::Expression(e) => format!("(defer {})", self.expr_sexp(*e)),
            },
            StatementKind::LifecycleHook { name, body, .. } => {
                format!("({} {})", name, self.block_sexp(*body))
            }
            StatementKind::Comment(text) => format!("(comment {:?})", text.as_str()),
        }
    }

    pub fn expr_sexp(&self, e: Idx<ExprData>) -> String {
        match &self.exprs[e].kind {
            ExprKind::Null => "null".to_string(),
            ExprKind::Unit => "unit".to_string(),
            // Matches rock: anonymous holes render `?{id}`, named `?{name}`.
            ExprKind::Hole(id, None) => format!("?{}", id),
            ExprKind::Hole(_, Some(name)) => format!("?{}", name),
            ExprKind::Ident(id) => id.name.to_string(),
            ExprKind::QualifiedIdent { base, member } => {
                format!("{}::{}", base.name, member.name)
            }
            ExprKind::Int(num, _) => num.to_string(),
            ExprKind::Float(num) => num.to_string(),
            ExprKind::String(s) => format!("{:?}", s.to_string()),
            ExprKind::InterpolatedString(parts) => {
                let parts_str = parts
                    .iter()
                    .map(|part| match part {
                        InterpolatedPart::Literal(lit) => format!("{:?}", lit),
                        // Spec dropped to match rock's `to_sexp` (the embedded
                        // expr renders, the format spec does not).
                        InterpolatedPart::Expr(e, _) => self.expr_sexp(*e),
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("\"{}\"", parts_str)
            }
            ExprKind::Bool(b) => b.to_string(),

            ExprKind::Not(inner) => format!("(! {})", self.expr_sexp(*inner)),
            ExprKind::BitNot(inner) => format!("(~ {})", self.expr_sexp(*inner)),
            ExprKind::UnaryMinus(inner) => format!("(- {})", self.expr_sexp(*inner)),
            ExprKind::Paren(inner) => format!("(paren {})", self.expr_sexp(*inner)),
            ExprKind::As(inner, ty) => format!("(as {} {})", self.expr_sexp(*inner), self.type_sexp(*ty)),
            ExprKind::TypeAscription(inner, ty) => {
                format!("(:: {} {})", self.expr_sexp(*inner), self.type_sexp(*ty))
            }

            ExprKind::Block(b) => {
                let block = &self.blocks[*b];
                if block.statements.is_empty() {
                    "{}".to_string()
                } else {
                    let stmts = block
                        .statements
                        .iter()
                        .map(|&s| self.stmt_sexp(s))
                        .collect::<Vec<_>>()
                        .join(" ");
                    format!("{{{}}}", stmts)
                }
            }

            ExprKind::ArrayLiteral(items) => {
                format!("[{}]", self.array_items_sexp(items))
            }

            ExprKind::FieldAccess(obj, field) => {
                format!("(. {} {})", self.expr_sexp(*obj), field.name)
            }
            ExprKind::SafeNavigation(obj, field) => {
                format!("(?. {} {})", self.expr_sexp(*obj), field.name)
            }
            ExprKind::SafeNavigationLet { receiver, binding, .. } => {
                format!("(?.let {} {})", self.expr_sexp(*receiver), binding.name)
            }
            ExprKind::QuestionMark(e) => format!("(? {})", self.expr_sexp(*e)),
            ExprKind::PreIncrement(e) => format!("(pre++ {})", self.expr_sexp(*e)),
            ExprKind::PostIncrement(e) => format!("(post++ {})", self.expr_sexp(*e)),
            ExprKind::PreDecrement(e) => format!("(pre-- {})", self.expr_sexp(*e)),
            ExprKind::PostDecrement(e) => format!("(post-- {})", self.expr_sexp(*e)),
            ExprKind::Spread(e) => format!("(spread {})", self.expr_sexp(*e)),
            ExprKind::Index(obj, idx) => {
                format!("(index {} {})", self.expr_sexp(*obj), self.expr_sexp(*idx))
            }

            ExprKind::FunctionCall { func, args, syntax } => {
                // Operator-origin calls render in infix form: `a + b` is
                // `__op_add(a, b)` but shows as `(+ a b)`.
                if let CallSyntax::Binary(op) = syntax {
                    if args.len() == 2 {
                        return format!(
                            "({} {} {})",
                            op.to_code(),
                            self.expr_sexp(args[0].value),
                            self.expr_sexp(args[1].value)
                        );
                    }
                }
                if args.is_empty() {
                    format!("(call {})", self.expr_sexp(*func))
                } else {
                    let args_str = args
                        .iter()
                        .map(|arg| {
                            if arg.is_spread {
                                return format!("..{}", self.expr_sexp(arg.value));
                            }
                            match &arg.name {
                                Some(name) => format!("{}: {}", name.name, self.expr_sexp(arg.value)),
                                None => self.expr_sexp(arg.value),
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                    format!("(call {} {})", self.expr_sexp(*func), args_str)
                }
            }

            ExprKind::BinOp { left, op, right } => format!(
                "({} {} {})",
                op.to_code(),
                self.expr_sexp(*left),
                self.expr_sexp(*right)
            ),

            ExprKind::Assign { target, value, .. } => {
                format!("(= {} {})", self.expr_sexp(*target), self.expr_sexp(*value))
            }
            ExprKind::ReturnExpr(None) => "(return)".to_string(),
            ExprKind::ReturnExpr(Some(e)) => format!("(return {})", self.expr_sexp(*e)),
            ExprKind::BreakExpr => "(break)".to_string(),
            ExprKind::ContinueExpr => "(continue)".to_string(),
            ExprKind::Yield(None) => "(yield)".to_string(),
            ExprKind::Yield(Some(e)) => format!("(yield {})", self.expr_sexp(*e)),
            ExprKind::YieldDelegate(e) => format!("(await {})", self.expr_sexp(*e)),
            ExprKind::SiblingFieldRef(id) => format!(".{}", id.name),
            ExprKind::FunctionRef(id) => format!("&{}", id.name),
            ExprKind::AnaphoricIdent(id) => id.name.to_string(),
            ExprKind::Lambda { params, return_ty, body } => {
                // Clean, id-free type rendering to match rock's lambda to_sexp.
                let opt_ty = |ty: &Option<Idx<TypeExprData>>| match ty {
                    Some(t) => format!("Some({})", self.type_sexp(*t)),
                    None => "None".to_string(),
                };
                let params_str = params
                    .iter()
                    .map(|p| opt_ty(&p.ty))
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("(lambda ({}) {} {})", params_str, opt_ty(return_ty), self.block_sexp(*body))
            }

            ExprKind::Tuple(items) => {
                format!(
                    "(tuple {})",
                    items.iter().map(|&i| self.expr_sexp(i)).collect::<Vec<_>>().join(" ")
                )
            }

            ExprKind::StructLiteral { name, bases, fields } => {
                if bases.is_empty() {
                    format!("(struct {} (data {}))", name.name, self.flat_fields_sexp(fields))
                } else {
                    let bases_str = bases
                        .iter()
                        .map(|b| format!("..{}", self.expr_sexp(*b)))
                        .collect::<Vec<_>>()
                        .join(" ");
                    format!(
                        "(struct {} (bases {}) (data {}))",
                        name.name,
                        bases_str,
                        self.flat_fields_sexp(fields)
                    )
                }
            }

            ExprKind::EnumVariantRecord { enum_name, variant_name, fields } => {
                let fields_str = self.flat_fields_sexp(fields);
                format!("(enum-variant {}::{} {{ {} }})", enum_name.name, variant_name.name, fields_str)
            }

            ExprKind::Match { scrutinee, arms } => {
                let arms_str = arms
                    .iter()
                    .map(|arm| self.arm_sexp(arm))
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("(match {} {})", self.expr_sexp(*scrutinee), arms_str)
            }

            ExprKind::If { branches, else_branch } => {
                let branches_str = branches
                    .iter()
                    .map(|(cond, body)| {
                        format!("(cond {} {})", self.expr_sexp(*cond), self.block_sexp(*body))
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                let else_str = match else_branch {
                    Some(b) => format!("(else {})", self.block_sexp(*b)),
                    None => String::new(),
                };
                format!("(if {} {})", branches_str, else_str)
            }

            ExprKind::IfLet { pattern, value, then_block, else_block } => {
                let else_str = match else_block {
                    Some(b) => format!(" (else {})", self.block_sexp(*b)),
                    None => String::new(),
                };
                format!(
                    "(if-let {} {} {}{})",
                    self.pattern_sexp(pattern),
                    self.expr_sexp(*value),
                    self.block_sexp(*then_block),
                    else_str
                )
            }

            ExprKind::IfLetChain { clauses, then_block, else_block } => {
                let clauses_str = clauses
                    .iter()
                    .map(|c| match c {
                        IfLetClause::Binding { pattern, value } => format!(
                            "(let {} {})",
                            self.pattern_sexp(pattern),
                            self.expr_sexp(*value)
                        ),
                        IfLetClause::Condition(cond) => self.expr_sexp(*cond),
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                let else_str = match else_block {
                    Some(b) => format!(" (else {})", self.block_sexp(*b)),
                    None => String::new(),
                };
                format!("(if-let-chain [{}] {}{})", clauses_str, self.block_sexp(*then_block), else_str)
            }
        }
    }

    /// Array-literal element rendering: rock strips the outer parens of a
    /// `(call ...)` element so `[f(x)]` reads `[call f x]`.
    fn array_items_sexp(&self, items: &[Idx<ExprData>]) -> String {
        items
            .iter()
            .map(|&item| {
                let sexp = self.expr_sexp(item);
                if sexp.starts_with("(call ") && sexp.ends_with(')') {
                    sexp[1..sexp.len() - 1].to_string()
                } else {
                    sexp
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// `name expr name expr ...` — struct / enum-record field rendering,
    /// expanding multi-name groups (`a + b: v`), matching rock.
    fn flat_fields_sexp(&self, fields: &[(Vec<Ident>, Idx<ExprData>)]) -> String {
        let mut out = Vec::new();
        for (names, value) in fields {
            let v = self.expr_sexp(*value);
            for n in names {
                out.push(format!("{} {}", n.name, v));
            }
        }
        out.join(" ")
    }

    /// Clean, id-free type s-expression, byte-identical to rock's
    /// `TypeExpr::to_sexp` (representation-normalizing: `Map<K,V>` parsed as
    /// an `App` renders the same as rock's dedicated `Map` variant).
    pub fn type_sexp(&self, t: Idx<TypeExprData>) -> String {
        let td = &self.types[t];
        let inner = match &td.kind {
            TypeExprKind::Any => "any".to_string(),
            TypeExprKind::Unit => "unit".to_string(),
            TypeExprKind::String => "string".to_string(),
            TypeExprKind::Int => "int".to_string(),
            TypeExprKind::Float => "float".to_string(),
            TypeExprKind::Bool => "bool".to_string(),
            TypeExprKind::Ident(id) => id.name.to_string(),
            TypeExprKind::Array(inner) => format!("[{}]", self.type_sexp(*inner)),
            TypeExprKind::App { ctor, args } => {
                // `Map<K,V>` renders `(Map K V)` to match rock's Map variant.
                format!(
                    "({} {})",
                    ctor.name,
                    args.iter().map(|&a| self.type_sexp(a)).collect::<Vec<_>>().join(" ")
                )
            }
            TypeExprKind::Tuple(ts) => format!(
                "(tuple {})",
                ts.iter().map(|&t| self.type_sexp(t)).collect::<Vec<_>>().join(" ")
            ),
            TypeExprKind::Function { params, ret } => format!(
                "(fn ({}) {})",
                params.iter().map(|&p| self.type_sexp(p)).collect::<Vec<_>>().join(" "),
                self.type_sexp(*ret)
            ),
            TypeExprKind::Hole(..) => "?".to_string(),
        };
        if td.is_nullable {
            format!("{}?", inner)
        } else {
            inner
        }
    }

    /// Render a type as SURFACE syntax (`Coroutine<int, (), int>`, `[T]`,
    /// `(a, b) -> c`, `T?`) for human-facing output (ctags, hover). Unlike
    /// `type_sexp`, `App`/`Tuple`/`Function` use surface delimiters rather than
    /// s-expr parens -- this is NOT the oracle form, so it must not be fed to the
    /// A/B comparison.
    pub fn type_surface(&self, t: Idx<TypeExprData>) -> String {
        let td = &self.types[t];
        let base = match &td.kind {
            TypeExprKind::Any => "any".to_string(),
            TypeExprKind::Unit => "()".to_string(),
            TypeExprKind::String => "string".to_string(),
            TypeExprKind::Int => "int".to_string(),
            TypeExprKind::Float => "float".to_string(),
            TypeExprKind::Bool => "bool".to_string(),
            TypeExprKind::Hole(..) => "?".to_string(),
            TypeExprKind::Ident(id) => id.name.to_string(),
            TypeExprKind::Array(inner) => format!("[{}]", self.type_surface(*inner)),
            TypeExprKind::Tuple(ts) => format!(
                "({})",
                ts.iter().map(|&t| self.type_surface(t)).collect::<Vec<_>>().join(", ")
            ),
            TypeExprKind::App { ctor, args } if args.is_empty() => ctor.name.to_string(),
            TypeExprKind::App { ctor, args } => format!(
                "{}<{}>",
                ctor.name,
                args.iter().map(|&a| self.type_surface(a)).collect::<Vec<_>>().join(", ")
            ),
            TypeExprKind::Function { params, ret } => format!(
                "({}) -> {}",
                params.iter().map(|&p| self.type_surface(p)).collect::<Vec<_>>().join(", "),
                self.type_surface(*ret)
            ),
        };
        if td.is_nullable {
            format!("{base}?")
        } else {
            base
        }
    }

    fn arm_sexp(&self, arm: &MatchArm) -> String {
        let guard_str = match arm.guard {
            Some(g) => format!(" (if {})", self.expr_sexp(g)),
            None => String::new(),
        };
        format!("(arm {}{} {})", self.pattern_sexp(&arm.pattern), guard_str, self.block_sexp(arm.body))
    }

    fn pattern_sexp(&self, p: &Pattern) -> String {
        match p {
            Pattern::Wildcard => "_".to_string(),
            Pattern::Ident(id) => id.name.to_string(),
            Pattern::Literal(e) => format!("(lit {})", self.expr_sexp(*e)),
            Pattern::Tuple(ps) => {
                format!("({})", ps.iter().map(|p| self.pattern_sexp(p)).collect::<Vec<_>>().join(" "))
            }
            Pattern::EnumVariant { enum_name, variant_name, args } => {
                let name_str = match enum_name {
                    Some(e) => format!("{}::{}", e.name, variant_name.name),
                    None => variant_name.name.to_string(),
                };
                match args {
                    Some(args) => format!(
                        "({} {})",
                        name_str,
                        args.iter().map(|p| self.pattern_sexp(p)).collect::<Vec<_>>().join(" ")
                    ),
                    None => name_str,
                }
            }
            Pattern::Record { enum_name, name, fields, rest } => {
                let name_str = match (enum_name, name) {
                    (Some(e), Some(n)) => format!("{}::{}", e.name, n.name),
                    (None, Some(n)) => n.name.to_string(),
                    _ => String::new(),
                };
                let fields_str = fields
                    .iter()
                    .map(|(f, p)| format!("{}: {}", f.name, self.pattern_sexp(p)))
                    .collect::<Vec<_>>()
                    .join(", ");
                let rest_str = if *rest { ", .." } else { "" };
                format!("{} {{ {}{} }}", name_str, fields_str, rest_str)
            }
            Pattern::Range { start, end, inclusive } => {
                let s = start.map(|e| self.expr_sexp(e)).unwrap_or_default();
                let en = end.map(|e| self.expr_sexp(e)).unwrap_or_default();
                let op = if *inclusive { "..=" } else { ".." };
                format!("(range {} {} {})", s, op, en)
            }
            Pattern::Or(alts) => {
                format!("(or {})", alts.iter().map(|p| self.pattern_sexp(p)).collect::<Vec<_>>().join(" | "))
            }
        }
    }
}

/// Mirror of rock's `VariableBinding::to_sexp`.
pub fn binding_sexp(b: &Binding) -> String {
    match b {
        Binding::Ident(id) => id.name.to_string(),
        Binding::Tuple(parts) => {
            format!("(tuple {})", parts.iter().map(binding_sexp).collect::<Vec<_>>().join(" "))
        }
        Binding::Newtype { name, binding } => format!("(newtype {} {})", name.name, binding.name),
    }
}
