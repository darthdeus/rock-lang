use bone_core::hir::{
    BindingInfo, Body, BodyOwner, Expr, FnBackend, Function, Statement, TypeCtor, TypeExpr,
    VariantPayload,
};
use bone_core::prelude::*;
use bone_core::workspace::{BodyInference, Workspace};

use crate::lower::backend_variant;
use crate::mir::FuncRef;
use crate::monoty::{MonoCtor, MonoTy};

fn published(function: &Function) -> bool {
    function.backend != FnBackend::Js
        && function
            .annotations
            .iter()
            .any(|a| a.is_builtin("nomangle"))
}

fn opaque(function: &Function) -> bool {
    if function.annotations.iter().any(|a| {
        a.is_builtin("uniform_values")
            || a.is_builtin("artifact_catalog")
            || (a.is_builtin("intrinsic") && function.inline_body.is_none())
    }) {
        return false;
    }
    function.is_extern && function.backend != FnBackend::Js
        || function
            .inline_body
            .is_some_and(|(backend, _)| backend == FnBackend::Lua)
}

struct Boundary<'a> {
    ws: &'a Workspace,
    span: Span,
    seen: FxHashSet<Ustr>,
    errors: &'a mut Vec<CompilerError>,
}

impl Boundary<'_> {
    fn ty(&mut self, inference: Option<&BodyInference>, ty: TyId) {
        let Some((ty, node)) = self.ws.resolve_ty(inference, ty) else {
            return;
        };
        if let TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            ..
        } = node
        {
            self.nominal(name);
            if self.phantom_value(name) {
                return;
            }
        }
        if let Some(shape) = node.shape() {
            for child in shape.params.chain(shape.tail) {
                self.ty(inference, ty.with_idx(child));
            }
        }
    }

    fn phantom_value(&self, name: Ustr) -> bool {
        self.ws.nominals.struct_id(name).is_some_and(|id| {
            let def = &self.ws.module(id.module).structs[id.def];
            def.is_value
                && def.fields.iter().all(|field| {
                    !self.ws.type_any(None, field.ty, &mut |node| {
                        matches!(node, TypeExpr::Generic(..))
                    })
                })
        })
    }

    fn mono(&mut self, ty: &MonoTy) {
        let args = match ty {
            MonoTy::App { ctor, params } => {
                if let MonoCtor::Nominal(name) = ctor {
                    self.nominal(*name);
                    if self.phantom_value(*name) {
                        return;
                    }
                }
                params
            }
            MonoTy::Function { params, ret } => {
                self.mono(ret);
                params
            }
            MonoTy::Tuple(items) => items,
            MonoTy::Nullable(inner) => return self.mono(inner),
            _ => return,
        };
        for ty in args {
            self.mono(ty);
        }
    }

    fn nominal(&mut self, name: Ustr) {
        if !self.seen.insert(name) {
            return;
        }
        if let Some(id) = self.ws.nominals.struct_id(name) {
            let def = &self.ws.module(id.module).structs[id.def];
            if !def.is_value && !def.is_nobox && !def.lua_named {
                let name = crate::mangle::declared_name(name.as_str());
                self.errors.push(
                    CompilerError::new_with_label(
                        self.span,
                        format!("Lua interop with struct `{name}` requires `@lua_named`"),
                    )
                    .with_label(def.name_span, format!("add `@lua_named` to `{name}` here")),
                );
            }
            for field in &def.fields {
                self.ty(None, field.ty);
            }
        } else if let Some(id) = self
            .ws
            .nominals
            .enum_id(crate::enum_instance::decl_name(name))
        {
            for variant in &self.ws.module(id.module).enums[id.def].variants {
                match &variant.payload {
                    VariantPayload::Unit => {}
                    VariantPayload::Tuple(items) => {
                        for &ty in items {
                            self.ty(None, ty);
                        }
                    }
                    VariantPayload::Record(fields) => {
                        for field in fields {
                            self.ty(None, field.ty);
                        }
                    }
                }
            }
        } else if let Some(id) = self.ws.nominals.newtype_id(name) {
            self.ty(None, self.ws.module(id.module).newtypes[id.def].wrapped);
        }
    }

    fn expression(
        &mut self,
        body: &Body,
        inference: &BodyInference,
        root: Idx<Expr>,
        seen: &mut FxHashSet<Idx<Expr>>,
    ) {
        if !seen.insert(root) {
            return;
        }
        let span = body.expr_spans[root];
        if let Some(&ty) = inference.expr_types.get(root) {
            self.ty(Some(inference), ty);
            if !self.ws.type_any(Some(inference), ty, &mut |node| {
                matches!(node, TypeExpr::Builtin(bone_core::hir::BuiltinTy::Any))
            }) {
                return;
            }
        }
        for &(site, ty) in &inference.any_erasures {
            if span == site {
                self.ty(Some(inference), ty);
            }
        }
        match body.expr_arena[root] {
            Expr::As { value, .. } => self.expression(body, inference, value, seen),
            Expr::Call { args, .. }
                if let Some(&(mid, function)) = inference.resolved_overload_calls.get(&root)
                    && let Some(index) = forwarded_parameter(self.ws, mid, function) =>
            {
                if let Some(arg) = args.into_iter().nth(index) {
                    self.expression(body, inference, arg, seen);
                }
            }
            Expr::Block {
                tail: Some(value), ..
            } => self.expression(body, inference, value, seen),
            Expr::If {
                then_branch,
                else_branch,
                ..
            } => {
                self.expression(body, inference, then_branch, seen);
                if let Some(other) = else_branch {
                    self.expression(body, inference, other, seen);
                }
            }
            Expr::Var(binding) => {
                for value in binding_values(body, binding) {
                    self.expression(body, inference, value, seen);
                }
            }
            _ => {}
        }
    }
}

fn binding_values(body: &Body, binding: Idx<BindingInfo>) -> impl Iterator<Item = Idx<Expr>> + '_ {
    let initializers = body
        .statement_arena
        .values()
        .filter_map(move |stmt| match *stmt {
            Statement::Let { pattern, value, .. }
                if body.pattern_single_bind(pattern) == Some(binding) =>
            {
                Some(value)
            }
            Statement::Static { name, value } if name == binding => Some(value),
            _ => None,
        });
    let assignments = body.expr_arena.values().filter_map(move |expr| match *expr {
        Expr::Assign { target, value }
            if matches!(body.expr_arena[target], Expr::Var(b) if b == binding) => Some(value),
        _ => None,
    });
    initializers.chain(assignments)
}

fn forwarded_parameter(ws: &Workspace, mid: ModuleId, function: Idx<Function>) -> Option<usize> {
    let body = ws.module(mid).bodies.get(&BodyOwner::Function(function))?;
    let mut expr = body.root?;
    loop {
        match body.expr_arena[expr] {
            Expr::As { value, .. }
            | Expr::Block {
                tail: Some(value), ..
            } => expr = value,
            Expr::Block {
                statements,
                tail: None,
            } => {
                let Statement::Return(Some(value)) =
                    body.statement_arena[statements.into_iter().next_back()?]
                else {
                    return None;
                };
                expr = value;
            }
            Expr::Var(binding) => {
                return body.param_bindings.into_iter().position(|p| p == binding);
            }
            _ => return None,
        }
    }
}

fn lua_words(source: &str) -> impl Iterator<Item = &str> {
    source
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|s| !s.is_empty())
}

fn opaque_parameter(param: &bone_core::hir::ParamDecl) -> bool {
    param
        .annotations
        .iter()
        .any(|annotation| annotation.is_builtin("repr"))
}

fn stdlib(ws: &Workspace, module: ModuleId) -> bool {
    bone_core::modules::stdlib_file_name(ws.module(module).file.as_str()).is_some()
}

#[derive(Clone, Copy)]
enum CallBoundary {
    Ordinary,
    Opaque(FuncRef),
    Cast,
}

fn call_boundary(ws: &Workspace, rep: FuncRef) -> CallBoundary {
    let function = &ws.module(rep.module).functions[rep.func];
    if stdlib(ws, rep.module) {
        return if function.name == "cast" {
            CallBoundary::Cast
        } else {
            CallBoundary::Ordinary
        };
    }
    if let Ok(callee) = backend_variant(ws, function.name, rep, FnBackend::Lua)
        && opaque(&ws.module(callee.module).functions[callee.func])
    {
        CallBoundary::Opaque(callee)
    } else {
        CallBoundary::Ordinary
    }
}

fn constructs_raw_table(
    ws: &Workspace,
    body: &Body,
    inference: &BodyInference,
    expr: Idx<Expr>,
    seen: &mut FxHashSet<Idx<Expr>>,
) -> bool {
    if !seen.insert(expr) {
        return false;
    }
    let mut raw = |value| constructs_raw_table(ws, body, inference, value, seen);
    match body.expr_arena[expr] {
        Expr::Var(binding) => binding_values(body, binding).any(raw),
        Expr::As { value, .. }
        | Expr::Block {
            tail: Some(value), ..
        } => raw(value),
        Expr::If {
            then_branch,
            else_branch,
            ..
        } => raw(then_branch) || else_branch.is_some_and(raw),
        Expr::Call { args, .. } => {
            let Some(&(mid, func)) = inference.resolved_overload_calls.get(&expr) else {
                return false;
            };
            let function = &ws.module(mid).functions[func];
            function.inline_body.is_some_and(|(backend, source)| {
                backend == FnBackend::Lua
                    && source
                        .trim_start()
                        .strip_prefix("return")
                        .is_some_and(|tail| tail.trim_start().starts_with('{'))
            }) || forwarded_parameter(ws, mid, func)
                .and_then(|index| args.into_iter().nth(index))
                .is_some_and(raw)
        }
        _ => false,
    }
}

fn check_body(
    ws: &Workspace,
    mid: ModuleId,
    owner: BodyOwner,
    boundaries: &mut FxHashMap<FuncRef, CallBoundary>,
    errors: &mut Vec<CompilerError>,
) {
    let module = ws.module(mid);
    if matches!(owner, BodyOwner::Function(id) if module.functions[id].backend == FnBackend::Js) {
        return;
    }
    let Some(body) = module.bodies.get(&owner) else {
        return;
    };
    let Some(inference) = ws.inference(mid).and_then(|m| m.per_fn.get(&owner)) else {
        return;
    };
    for (&expr, &(module, func)) in &inference.resolved_overload_calls {
        let rep = FuncRef { module, func };
        let callee = match *boundaries
            .entry(rep)
            .or_insert_with(|| call_boundary(ws, rep))
        {
            CallBoundary::Ordinary => continue,
            CallBoundary::Opaque(callee) => callee,
            CallBoundary::Cast => {
                if let Expr::Call { args, .. } = body.expr_arena[expr]
                    && let Some(arg) = args.into_iter().next()
                    && constructs_raw_table(ws, body, inference, arg, &mut FxHashSet::default())
                    && let Some(&ty) = inference.expr_types.get(expr)
                {
                    Boundary {
                        ws,
                        span: body.expr_spans[expr],
                        seen: FxHashSet::default(),
                        errors,
                    }
                    .ty(Some(inference), ty);
                }
                continue;
            }
        };
        let target = &ws.module(callee.module).functions[callee.func];
        let (receiver, args) = match body.expr_arena[expr] {
            Expr::Call { args, .. } => (None, args),
            Expr::SafeNavCall { receiver, args, .. } => (Some(receiver), args),
            _ => continue,
        };
        let mut boundary = Boundary {
            ws,
            span: body.expr_spans[expr],
            seen: FxHashSet::default(),
            errors,
        };
        if let Some(&ty) = inference.expr_types.get(expr) {
            boundary.ty(Some(inference), ty);
        }
        let mut seen = FxHashSet::default();
        let mut positional = 0;
        let global_lookup = target.inline_body.is_some_and(|(_, source)| {
            source.contains("__rock_global_entry") || source.contains("_G[")
        });
        for arg in receiver.into_iter().chain(args) {
            if global_lookup && let Expr::String(name) = body.expr_arena[arg] {
                for (&global, &ty) in &ws.globals_by_name {
                    if crate::mangle::declared_name(global.as_str()) == name.as_str()
                        || crate::mangle::symbol_of_key(global) == name
                    {
                        boundary.ty(None, ty);
                    }
                }
            }
            let parameter = if let Some(name) = body.arg_names.get(&arg) {
                target.param_decls.iter().find(|param| param.name == *name)
            } else {
                let param = target.param_decls.get(positional);
                positional += 1;
                param
            };
            if !parameter.is_some_and(opaque_parameter) {
                boundary.expression(body, inference, arg, &mut seen);
            }
        }
    }
}

pub(crate) fn check_concrete_boundary<'a>(
    ws: &Workspace,
    func: FuncRef,
    span: Span,
    params: impl Iterator<Item = &'a MonoTy>,
    result: Option<&'a MonoTy>,
) -> Result<(), CompilerError> {
    let function = &ws.module(func.module).functions[func.func];
    if function.generics.is_empty() || stdlib(ws, func.module) || !opaque(function) {
        return Ok(());
    }
    let mut errors = Vec::new();
    let mut boundary = Boundary {
        ws,
        span,
        seen: FxHashSet::default(),
        errors: &mut errors,
    };
    for (ty, param) in params.zip(&function.param_decls) {
        if !opaque_parameter(param) {
            boundary.mono(ty);
        }
    }
    if let Some(ty) = result {
        boundary.mono(ty);
    }
    order_errors(&mut errors);
    let mut errors = errors.into_iter();
    match errors.next() {
        Some(first) => Err(first.with_siblings(errors)),
        None => Ok(()),
    }
}

pub fn errors(ws: &Workspace) -> Vec<CompilerError> {
    let mut errors = Vec::new();
    let mut boundaries = FxHashMap::default();
    let globals: FxHashMap<_, _> = ws
        .globals_by_name
        .iter()
        .map(|(&name, &ty)| (crate::mangle::symbol_of_key(name).as_str(), ty))
        .collect();
    for module in ws.source_modules().filter(|module| !stdlib(ws, module.id)) {
        for function in module.functions.values() {
            if !opaque(function) && !published(function) {
                continue;
            }
            let mut boundary = Boundary {
                ws,
                span: function.name_span,
                seen: FxHashSet::default(),
                errors: &mut errors,
            };
            for ty in function
                .param_decls
                .iter()
                .filter(|p| !opaque_parameter(p))
                .map(|p| p.ty)
                .chain(function.return_ty)
            {
                boundary.ty(None, ty);
            }
            if let Some((FnBackend::Lua, source)) = function.inline_body
                && !globals.is_empty()
            {
                for word in lua_words(source.as_str()) {
                    if let Some(&ty) = globals.get(word) {
                        boundary.ty(None, ty);
                    }
                }
            }
        }
        for &owner in module.bodies.keys() {
            check_body(ws, module.id, owner, &mut boundaries, &mut errors);
        }
    }
    order_errors(&mut errors);
    errors
}

pub fn edited_errors(ws: &Workspace, edited: &[(ModuleId, BodyOwner)]) -> Vec<CompilerError> {
    let mut errors = Vec::new();
    let mut boundaries = FxHashMap::default();
    for &(mid, owner) in edited {
        if !stdlib(ws, mid) {
            check_body(ws, mid, owner, &mut boundaries, &mut errors);
        }
    }
    order_errors(&mut errors);
    errors
}

fn order_errors(errors: &mut Vec<CompilerError>) {
    errors.sort_by(|a, b| {
        (a.location.file.as_str(), a.location.offset, &a.message).cmp(&(
            b.location.file.as_str(),
            b.location.offset,
            &b.message,
        ))
    });
    errors.dedup_by(|a, b| a.location == b.location && a.message == b.message);
}
