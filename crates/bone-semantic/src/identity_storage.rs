//! Identity-storage checks for declarations and function bodies.
//! A package's manifest declares its migration exemption; the compiler
//! otherwise enforces the same rules for every package.

use bone_core::escape::lambda_is_nonescaping;
use bone_core::identity_storage::{
    IDENTITY_ERASURE_ANNOTATION, IDENTITY_STORAGE_ANNOTATION, IdentityStorageVia,
    identity_body_storage_violation, identity_body_violation, identity_local_storage_violation,
    identity_root_storage_violation, identity_storage_field_violation, identity_storage_violation,
    is_identity_storage_edge, is_identity_storage_field,
};
pub use bone_core::identity_storage::{
    IdentityNode, IdentityPosition, IdentityStorageViolation, IdentityTypes, identity_violation_in,
};
use bone_core::modules::stdlib_file_name;
use bone_core::suspension::{SuspensionPolicy, uses_after_suspension};
use bone_core::workspace::BodyInference;

use crate::hir::{
    BindingInfo, BindingKind, Callee, Expr, Function, Module, TypeCtor, TypeExpr, VariantArgs,
    VariantPayload,
};
use crate::prelude::*;

pub use bone_core::identity_storage::{
    IdentityStorageExemption, RUST_EMBEDDED_PACKAGES, STDLIB_PACKAGE, UNPACKAGED_PROGRAMS,
    identity_storage_exemption, identity_storage_package,
};

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum IdentityStorageKind {
    /// A struct or value field holding an identity directly or nullable.
    Field,
    /// An identity inside a Vec, Map, tuple or stored type argument, in any
    /// declaration position.
    Container,
    /// An enum payload holding an identity directly or nullable.
    Payload,
    /// A newtype wrapping an identity directly or nullable.
    Newtype,
    /// In a body: a local, parameter, Vec literal or constructor whose type
    /// stores an identity (a Vec, Map or stored type argument).
    LocalStorage,
    /// An identity captured by a closure not proven nonescaping.
    Capture,
    /// A `co fn` parameter holding an identity directly or nullable.
    CoParam,
    /// An identity borrow read after a coroutine suspension.
    AcrossYield,
    /// An identity coerced to `any`.
    AnyErasure,
}

impl IdentityStorageKind {
    pub const ALL: [IdentityStorageKind; 9] = [
        IdentityStorageKind::Field,
        IdentityStorageKind::Container,
        IdentityStorageKind::Payload,
        IdentityStorageKind::Newtype,
        IdentityStorageKind::LocalStorage,
        IdentityStorageKind::Capture,
        IdentityStorageKind::CoParam,
        IdentityStorageKind::AcrossYield,
        IdentityStorageKind::AnyErasure,
    ];

    pub fn label(self) -> &'static str {
        match self {
            IdentityStorageKind::Field => "field",
            IdentityStorageKind::Container => "container",
            IdentityStorageKind::Payload => "payload",
            IdentityStorageKind::Newtype => "newtype",
            IdentityStorageKind::LocalStorage => "local-storage",
            IdentityStorageKind::Capture => "capture",
            IdentityStorageKind::CoParam => "co-param",
            IdentityStorageKind::AcrossYield => "across-yield",
            IdentityStorageKind::AnyErasure => "any-erasure",
        }
    }
}

#[derive(Clone, Debug)]
pub struct IdentityStorageSite {
    pub kind: IdentityStorageKind,
    pub identity: Ustr,
    pub span: Span,
    pub message: String,
}

/// Whether `module`'s sites are counted by the census instead of reported.
pub fn identity_storage_exempt(ws: &Workspace, module: &Module) -> bool {
    identity_storage_exemption(ws, module).is_some()
}

/// Every declaration position in `module` that stores an identity struct.
pub fn identity_storage_sites(ws: &Workspace, module: &Module) -> Vec<IdentityStorageSite> {
    let mut sites = Vec::new();
    let mut push = |violation: Option<IdentityStorageViolation>,
                    direct: IdentityStorageKind,
                    span: Span,
                    what: String| {
        let Some(violation) = violation else { return };
        let kind = match violation.via {
            IdentityStorageVia::Direct => direct,
            IdentityStorageVia::Container => IdentityStorageKind::Container,
        };
        let identity = violation.identity;
        sites.push(IdentityStorageSite {
            kind,
            identity,
            span,
            message: format!(
                "{what} cannot store identity struct `{identity}`; store an `Entity<{identity}>` (Arena row) or make `{identity}` a `value`"
            ),
        });
    };
    for (_, def) in module.structs.iter() {
        for field in &def.fields {
            let violation = if is_identity_storage_field(field) {
                identity_storage_field_violation(ws, field.ty)
            } else {
                identity_storage_violation(ws, field.ty)
            };
            push(
                violation,
                IdentityStorageKind::Field,
                field.span,
                format!("field `{}`", field.name),
            );
        }
    }
    for (_, def) in module.enums.iter() {
        for variant in &def.variants {
            match &variant.payload {
                VariantPayload::Unit => {}
                VariantPayload::Tuple(types) => {
                    let violation = types
                        .iter()
                        .find_map(|ty| identity_storage_violation(ws, *ty));
                    push(
                        violation,
                        IdentityStorageKind::Payload,
                        variant.name_span,
                        format!("enum variant `{}`", variant.name),
                    );
                }
                VariantPayload::Record(fields) => {
                    for field in fields {
                        push(
                            identity_storage_violation(ws, field.ty),
                            IdentityStorageKind::Payload,
                            field.span,
                            format!("enum field `{}`", field.name),
                        );
                    }
                }
            }
        }
    }
    for (_, def) in module.newtypes.iter() {
        push(
            identity_storage_violation(ws, def.wrapped),
            IdentityStorageKind::Newtype,
            def.span,
            format!("newtype `{}`", def.name),
        );
    }
    for (_, global) in module.globals.iter() {
        // A root admits a direct or nullable identity, so what a global
        // stores is always below a container.
        push(
            identity_root_storage_violation(ws, global.ty),
            IdentityStorageKind::Container,
            global.span,
            format!("global `{}`", global.name),
        );
    }
    sites
}

fn storage_message(what: &str, identity: Ustr) -> String {
    format!(
        "{what} cannot store identity struct `{identity}`; store an `Entity<{identity}>` (Arena row) or make `{identity}` a `value`"
    )
}

/// How the body check reads a slot's type: as inferred, or under an
/// instance's substitution (the instance graph's substitution-only check).
pub trait BodySlotTypes {
    /// A local, parameter or temporary of type `ty`: an identity at the top is
    /// a borrow.
    fn local(&self, ty: TyId) -> Option<IdentityStorageViolation>;
    /// A value the closure or coroutine frame keeps: the identity itself is
    /// storage.
    fn stored(&self, ty: TyId) -> Option<IdentityStorageViolation>;
    /// A Vec literal written to an `@identity_storage` field: its direct
    /// elements are rows.
    fn row(&self, ty: TyId) -> Option<IdentityStorageViolation>;
}

struct InferredSlotTypes<'a> {
    ws: &'a Workspace,
    bi: &'a BodyInference,
}

impl BodySlotTypes for InferredSlotTypes<'_> {
    fn local(&self, ty: TyId) -> Option<IdentityStorageViolation> {
        identity_local_storage_violation(self.ws, self.bi, ty)
    }
    fn stored(&self, ty: TyId) -> Option<IdentityStorageViolation> {
        identity_body_storage_violation(self.ws, Some(self.bi), ty)
    }
    fn row(&self, ty: TyId) -> Option<IdentityStorageViolation> {
        identity_body_violation(self.ws, self.bi, ty, IdentityPosition::Row)
    }
}

/// The Vec literals written directly to an `@identity_storage` field, by a
/// constructor or an assignment: the one edge whose elements may be rows.
fn row_edge_literals(ws: &Workspace, body: &Body, bi: &BodyInference) -> FxHashSet<Idx<Expr>> {
    let mut rows = FxHashSet::default();
    let mut edge = |owner: Idx<Expr>, field: Ustr, value: Idx<Expr>| {
        if matches!(body.expr_arena[value], Expr::Array(_))
            && let Some(ty) = bi.expr_types.get(owner).copied()
            && is_identity_storage_edge(ws, bi, ty, field)
        {
            rows.insert(value);
        }
    };
    for (id, expr) in body.expr_arena.iter() {
        match expr {
            Expr::StructLiteral { fields, .. } => {
                for field in *fields {
                    let field = &body.struct_lit_field_arena[field];
                    edge(id, field.name, field.value);
                }
            }
            Expr::Assign { target, value } => {
                if let Expr::FieldAccess { base, field } = body.expr_arena[*target] {
                    edge(base, field, *value);
                }
            }
            _ => {}
        }
    }
    rows
}

/// Every position in one function body that stores an identity struct: the
/// S0b body check. `bi` is the body's inference; types it left unresolved are
/// not reported.
pub fn identity_body_sites(
    ws: &Workspace,
    function: &Function,
    body: &Body,
    bi: &BodyInference,
) -> Vec<IdentityStorageSite> {
    identity_body_sites_with(ws, function, body, bi, &InferredSlotTypes { ws, bi })
}

/// [`identity_body_sites`] reading slot types through `types`.
pub fn identity_body_sites_with(
    ws: &Workspace,
    function: &Function,
    body: &Body,
    bi: &BodyInference,
    types: &impl BodySlotTypes,
) -> Vec<IdentityStorageSite> {
    let mut sites = Vec::new();
    let fn_params: FxHashSet<Idx<BindingInfo>> = body.param_bindings.into_iter().collect();
    let binding_ty = |binding: Idx<BindingInfo>| bi.binding_types.get(binding).copied();

    for (binding, info) in body.binding_arena.iter() {
        if info.kind == BindingKind::Fn {
            continue;
        }
        let Some(ty) = binding_ty(binding) else {
            continue;
        };
        let what = if info.kind == BindingKind::Param {
            "parameter"
        } else {
            "local"
        };
        if let Some(violation) = types.local(ty) {
            sites.push(IdentityStorageSite {
                kind: IdentityStorageKind::LocalStorage,
                identity: violation.identity,
                span: body.binding_spans[binding],
                message: storage_message(&format!("{what} `{}`", info.name), violation.identity),
            });
        } else if function.is_coroutine
            && fn_params.contains(&binding)
            && let Some(violation) = types.stored(ty)
        {
            let identity = violation.identity;
            sites.push(IdentityStorageSite {
                kind: IdentityStorageKind::CoParam,
                identity,
                span: body.binding_spans[binding],
                message: format!(
                    "coroutine parameter `{}` cannot hold identity struct `{identity}`: the coroutine frame would retain the borrow; pass an `Entity<{identity}>` and resolve it in each resume",
                    info.name
                ),
            });
        }
    }

    let rows = row_edge_literals(ws, body, bi);
    for (id, expr) in body.expr_arena.iter() {
        let what = match expr {
            Expr::Array(_) => "this Vec literal",
            Expr::StructLiteral { .. } => "this constructor",
            Expr::EnumVariant { args, .. } if !matches!(args, VariantArgs::Unit) => {
                "this enum constructor"
            }
            _ => continue,
        };
        let Some(ty) = bi.expr_types.get(id).copied() else {
            continue;
        };
        let violation = if rows.contains(&id) {
            types.row(ty)
        } else {
            types.local(ty)
        };
        if let Some(violation) = violation {
            sites.push(IdentityStorageSite {
                kind: IdentityStorageKind::LocalStorage,
                identity: violation.identity,
                span: body.expr_spans[id],
                message: storage_message(what, violation.identity),
            });
        }
    }

    let erasure_capability = function
        .annotations
        .iter()
        .any(|annotation| annotation.is_builtin(IDENTITY_ERASURE_ANNOTATION));
    for &(span, ty) in bi.any_erasures.iter().filter(|_| !erasure_capability) {
        if let Some(violation) = types.stored(ty) {
            let identity = violation.identity;
            sites.push(IdentityStorageSite {
                kind: IdentityStorageKind::AnyErasure,
                identity,
                span,
                message: format!(
                    "cannot erase identity struct `{identity}` to `any`; erase an `Entity<{identity}>` or make `{identity}` a `value`"
                ),
            });
        }
    }

    for (lambda, expr) in body.expr_arena.iter() {
        if !matches!(expr, Expr::Lambda { .. }) {
            continue;
        }
        let lambda_span = body.expr_spans[lambda];
        let mut captured = FxHashSet::default();
        let mut escapes = None;
        for (use_id, use_expr) in body.expr_arena.iter() {
            let Expr::Var(binding) = *use_expr else {
                continue;
            };
            if !contains(lambda_span, body.expr_spans[use_id])
                || contains(lambda_span, body.binding_spans[binding])
                || !captured.insert(binding)
            {
                continue;
            }
            let Some(violation) = binding_ty(binding).and_then(|ty| types.stored(ty)) else {
                continue;
            };
            if !*escapes.get_or_insert_with(|| !lambda_is_nonescaping(ws, body, bi, lambda)) {
                break;
            }
            let identity = violation.identity;
            sites.push(IdentityStorageSite {
                kind: IdentityStorageKind::Capture,
                identity,
                span: body.expr_spans[use_id],
                message: format!(
                    "closure that may escape cannot capture identity struct `{identity}` (`{}`); capture an `Entity<{identity}>` and resolve it when the closure runs",
                    body.binding_arena[binding].name
                ),
            });
        }
    }

    if function.is_coroutine {
        let policy = IdentityBorrowPolicy { body, bi, types };
        for at in uses_after_suspension(body, &policy) {
            let Some(binding) = used_binding(body, at) else {
                continue;
            };
            let Some(violation) = binding_ty(binding).and_then(|ty| types.stored(ty)) else {
                continue;
            };
            let identity = violation.identity;
            sites.push(IdentityStorageSite {
                kind: IdentityStorageKind::AcrossYield,
                identity,
                span: body.expr_spans[at],
                message: format!(
                    "identity borrow `{}` is used after a coroutine suspension; retain an `Entity<{identity}>` and resolve it after the yield",
                    body.binding_arena[binding].name
                ),
            });
        }
    }

    // A `let xs = [x]` reports its binding and its literal: one site.
    let mut seen = FxHashSet::default();
    sites.retain(|site| {
        seen.insert((
            site.kind,
            site.identity,
            site.span.file,
            site.span.line_range.0,
        ))
    });
    sites
}

/// A local borrow of an identity is frame data for the suspension walk.
/// Parameters are left to the `co fn` parameter rule, and locals whose type
/// stores an identity to the local-storage rule.
struct IdentityBorrowPolicy<'a, T> {
    body: &'a Body,
    bi: &'a BodyInference,
    types: &'a T,
}

impl<T: BodySlotTypes> SuspensionPolicy for IdentityBorrowPolicy<'_, T> {
    fn binding_holds_frame_data(
        &self,
        binding: Idx<BindingInfo>,
        _value: Option<Idx<Expr>>,
    ) -> bool {
        if matches!(
            self.body.binding_arena[binding].kind,
            BindingKind::Param | BindingKind::Fn
        ) {
            return false;
        }
        let Some(ty) = self.bi.binding_types.get(binding).copied() else {
            return false;
        };
        self.types.local(ty).is_none() && self.types.stored(ty).is_some()
    }
}

fn contains(outer: Span, inner: Span) -> bool {
    outer.file == inner.file
        && outer.offset_range.0 <= inner.offset_range.0
        && inner.offset_range.1 <= outer.offset_range.1
}

fn used_binding(body: &Body, at: Idx<Expr>) -> Option<Idx<BindingInfo>> {
    match body.expr_arena[at] {
        Expr::Var(binding) => Some(binding),
        Expr::Call {
            callee: Callee::Local(binding),
            ..
        } => Some(binding),
        _ => None,
    }
}

fn is_stdlib_file_named(module: &Module, stem: &str) -> bool {
    stdlib_file_name(module.file.as_str())
        .and_then(|name| name.strip_suffix(".rock"))
        .is_some_and(|name| name == stem)
}

/// The stdlib arena file: the one module that implements the privileged row
/// store, with the trust boundary of `@frame_ref_source`.
pub fn is_identity_store_module(module: &Module) -> bool {
    is_stdlib_file_named(module, "arena")
}

/// The stdlib files that implement the Arena and Scene registries, which hold
/// identities behind `@identity_erasure`.
fn is_identity_registry_module(module: &Module) -> bool {
    is_identity_store_module(module) || is_stdlib_file_named(module, "scene")
}

/// `@identity_storage` is the Arena store's privilege: only
/// [`is_identity_store_module`] may use it, and only on a Vec field.
/// `@identity_erasure` is the Arena and Scene registries' privilege.
fn capability_errors(module: &Module) -> Vec<CompilerError> {
    let is_arena_stdlib = is_identity_store_module(module);
    let mut errs = Vec::new();
    if !is_identity_registry_module(module) {
        for (_, function) in module.functions.iter() {
            if let Some(annotation) = function
                .annotations
                .iter()
                .find(|annotation| annotation.is_builtin(IDENTITY_ERASURE_ANNOTATION))
            {
                errs.push(CompilerError::new_with_label(
                    annotation.span,
                    "`@identity_erasure` is reserved for the standard Arena and Scene registries"
                        .to_string(),
                ));
            }
        }
    }
    for (_, def) in module.structs.iter() {
        for field in &def.fields {
            let Some(annotation) = field
                .annotations
                .iter()
                .find(|annotation| annotation.is_builtin(IDENTITY_STORAGE_ANNOTATION))
            else {
                continue;
            };
            if !is_arena_stdlib {
                errs.push(CompilerError::new_with_label(
                    annotation.span,
                    "`@identity_storage` is reserved for the standard arena row store".to_string(),
                ));
            } else if !matches!(
                field.ty,
                TyId::Static(_, idx) if matches!(
                    module.ty_arena[idx],
                    TypeExpr::App { ctor: TypeCtor::Array, .. }
                )
            ) {
                errs.push(CompilerError::new_with_label(
                    annotation.span,
                    "`@identity_storage` marks the Vec field that holds Arena rows".to_string(),
                ));
            }
        }
    }
    errs
}

pub(crate) fn check_identity_storage(ws: &mut Workspace, fresh: &[ModuleId]) {
    let mut errs = Vec::new();
    for module in ws.modules.iter() {
        if !fresh.contains(&module.id) {
            continue;
        }
        errs.extend(
            capability_errors(module)
                .into_iter()
                .map(|e| (module.id, e)),
        );
        if identity_storage_exempt(ws, module) {
            continue;
        }
        errs.extend(identity_storage_sites(ws, module).into_iter().map(|site| {
            (
                module.id,
                CompilerError::new_with_label(site.span, site.message),
            )
        }));
    }
    for (mid, err) in errs {
        ws.module_mut(mid).interface_errors.push(err);
    }
}
