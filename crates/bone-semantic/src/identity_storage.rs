//! The identity-storage checks (docs/identity-storage-plan.md): declarations
//! (S0a), function bodies (S0b), and their transitional package exemptions.
//!
//! Enforcement is on by default. A package on [`IDENTITY_STORAGE_EXEMPT`]
//! gets no diagnostics; `rock identity-storage-census` counts its sites and
//! the census test pins those counts per kind, so a migration that removes
//! sites updates the pinned table and drops the package here when it reaches
//! zero.

use bone_core::escape::lambda_is_nonescaping;
use bone_core::identity_storage::{
    identity_body_storage_violation, identity_local_storage_violation, identity_root_storage_violation,
    identity_storage_field_violation, identity_storage_violation, is_identity_storage_field, IdentityStorageVia,
    IdentityStorageViolation, IDENTITY_STORAGE_ANNOTATION,
};
use bone_core::modules::is_stdlib_module_file;
use bone_core::suspension::{uses_after_suspension, SuspensionPolicy};
use bone_core::workspace::BodyInference;

use crate::hir::{BindingInfo, BindingKind, Callee, Expr, Function, Module, TypeCtor, TypeExpr, VariantArgs, VariantPayload};
use crate::prelude::*;

/// The census label for files outside any package: `tests/` programs and
/// programs embedded in Rust tests.
pub const UNPACKAGED_PROGRAMS: &str = "unpackaged programs";
/// The census label for stdlib files.
pub const STDLIB_PACKAGE: &str = "std";

/// Programs embedded in Rust tests that declare a package of their own. They
/// belong to [`UNPACKAGED_PROGRAMS`] until S8 rewrites them; the census does
/// not count them.
pub const RUST_EMBEDDED_PACKAGES: &[&str] = &[
    "legacyapp",
    "memclass_handle",
    "memclass_handle_marked",
    "memclass_handle_named",
    "memclass_handle_platform",
    "provider",
    "v1embed",
    "v1spec",
];

/// Packages, by stamped package name, whose identity-storage violations are
/// counted by the census instead of reported. Two packages that share a name
/// (`games/testbed`, `demos/testbed`) share an entry.
pub const IDENTITY_STORAGE_EXEMPT: &[&str] = &[
    STDLIB_PACKAGE,
    UNPACKAGED_PROGRAMS,
    "anvil",
    "bitgun",
    "bopt",
    "chess",
    "chess_core",
    "chess_game",
    "chess_sfx",
    "chesslings",
    "sample_game_c",
    "sample_game_c_v0",
    "sample_game_b",
    "decorator_systems",
    "fm",
    "fm_core",
    "sample-game",
    "idle_core",
    "idle_reference",
    "logo",
    "mag",
    "mag_ai",
    "mag_content",
    "mag_encounter_worker",
    "mag_simulation",
    "mag_world",
    "match3",
    "match3_core",
    "pixel_spirit",
    "spark",
    "stone",
    "stone_core",
    "survivors",
    "testbed",
    "typography",
    "ui_lab",
    "vfxlab",
    "void",
    "wgpu_renderer",
];

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
}

impl IdentityStorageKind {
    pub const ALL: [IdentityStorageKind; 8] = [
        IdentityStorageKind::Field,
        IdentityStorageKind::Container,
        IdentityStorageKind::Payload,
        IdentityStorageKind::Newtype,
        IdentityStorageKind::LocalStorage,
        IdentityStorageKind::Capture,
        IdentityStorageKind::CoParam,
        IdentityStorageKind::AcrossYield,
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

/// The census label of the package that owns `module`.
pub fn identity_storage_package(module: &Module) -> &str {
    if is_stdlib_module_file(module.file.as_str()) {
        STDLIB_PACKAGE
    } else {
        match module.package {
            Some(p) if !RUST_EMBEDDED_PACKAGES.contains(&p.as_str()) => p.as_str(),
            _ => UNPACKAGED_PROGRAMS,
        }
    }
}

/// Whether `module`'s sites are counted by the census instead of reported.
pub fn identity_storage_exempt(ws: &Workspace, module: &Module) -> bool {
    let package = identity_storage_package(module);
    if package == UNPACKAGED_PROGRAMS && ws.identity_storage_enforce_unpackaged {
        return false;
    }
    IDENTITY_STORAGE_EXEMPT.contains(&package)
}

/// Every declaration position in `module` that stores an identity struct.
pub fn identity_storage_sites(ws: &Workspace, module: &Module) -> Vec<IdentityStorageSite> {
    let mut sites = Vec::new();
    let mut push = |violation: Option<IdentityStorageViolation>, direct: IdentityStorageKind, span: Span, what: String| {
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
            push(violation, IdentityStorageKind::Field, field.span, format!("field `{}`", field.name));
        }
    }
    for (_, def) in module.enums.iter() {
        for variant in &def.variants {
            match &variant.payload {
                VariantPayload::Unit => {}
                VariantPayload::Tuple(types) => {
                    let violation = types.iter().find_map(|ty| identity_storage_violation(ws, *ty));
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

/// Every position in one function body that stores an identity struct: the
/// S0b body check. `bi` is the body's inference; types it left unresolved are
/// not reported.
pub fn identity_body_sites(ws: &Workspace, function: &Function, body: &Body, bi: &BodyInference) -> Vec<IdentityStorageSite> {
    let mut sites = Vec::new();
    let fn_params: FxHashSet<Idx<BindingInfo>> = body.param_bindings.into_iter().collect();
    let binding_ty = |binding: Idx<BindingInfo>| bi.binding_types.get(binding).copied();

    for (binding, info) in body.binding_arena.iter() {
        if info.kind == BindingKind::Fn {
            continue;
        }
        let Some(ty) = binding_ty(binding) else { continue };
        let what = if info.kind == BindingKind::Param { "parameter" } else { "local" };
        if let Some(violation) = identity_local_storage_violation(ws, bi, ty) {
            sites.push(IdentityStorageSite {
                kind: IdentityStorageKind::LocalStorage,
                identity: violation.identity,
                span: body.binding_spans[binding],
                message: storage_message(&format!("{what} `{}`", info.name), violation.identity),
            });
        } else if function.is_coroutine
            && fn_params.contains(&binding)
            && let Some(violation) = identity_body_storage_violation(ws, Some(bi), ty)
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

    for (id, expr) in body.expr_arena.iter() {
        let what = match expr {
            Expr::Array(_) => "this Vec literal",
            Expr::StructLiteral { .. } => "this constructor",
            Expr::EnumVariant { args, .. } if !matches!(args, VariantArgs::Unit) => "this enum constructor",
            _ => continue,
        };
        let Some(ty) = bi.expr_types.get(id).copied() else { continue };
        if let Some(violation) = identity_local_storage_violation(ws, bi, ty) {
            sites.push(IdentityStorageSite {
                kind: IdentityStorageKind::LocalStorage,
                identity: violation.identity,
                span: body.expr_spans[id],
                message: storage_message(what, violation.identity),
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
            let Expr::Var(binding) = *use_expr else { continue };
            if !contains(lambda_span, body.expr_spans[use_id])
                || contains(lambda_span, body.binding_spans[binding])
                || !captured.insert(binding)
            {
                continue;
            }
            let Some(violation) = binding_ty(binding).and_then(|ty| identity_body_storage_violation(ws, Some(bi), ty))
            else {
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
        let policy = IdentityBorrowPolicy { ws, body, bi };
        for at in uses_after_suspension(body, &policy) {
            let binding = match body.expr_arena[at] {
                Expr::Var(binding) => Some(binding),
                Expr::Call {
                    callee: Callee::Local(binding),
                    ..
                } => Some(binding),
                _ => None,
            };
            let Some(binding) = binding else { continue };
            let Some(violation) = binding_ty(binding).and_then(|ty| identity_body_storage_violation(ws, Some(bi), ty))
            else {
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
    sites.retain(|site| seen.insert((site.kind, site.identity, site.span.file, site.span.line_range.0)));
    sites
}

/// A local borrow of an identity is frame data for the suspension walk.
/// Parameters are left to the `co fn` parameter rule, and locals whose type
/// stores an identity to the local-storage rule.
struct IdentityBorrowPolicy<'a> {
    ws: &'a Workspace,
    body: &'a Body,
    bi: &'a BodyInference,
}

impl SuspensionPolicy for IdentityBorrowPolicy<'_> {
    fn binding_holds_frame_data(&self, binding: Idx<BindingInfo>, _value: Option<Idx<Expr>>) -> bool {
        if matches!(self.body.binding_arena[binding].kind, BindingKind::Param | BindingKind::Fn) {
            return false;
        }
        let Some(ty) = self.bi.binding_types.get(binding).copied() else {
            return false;
        };
        identity_local_storage_violation(self.ws, self.bi, ty).is_none()
            && identity_body_storage_violation(self.ws, Some(self.bi), ty).is_some()
    }
}

fn contains(outer: Span, inner: Span) -> bool {
    outer.file == inner.file
        && outer.offset_range.0 <= inner.offset_range.0
        && inner.offset_range.1 <= outer.offset_range.1
}

/// [`identity_body_sites`] for every inferred function body of `module`.
pub fn identity_module_body_sites(ws: &Workspace, module: &Module) -> Vec<IdentityStorageSite> {
    let Some(inference) = ws.inference(module.id) else {
        return Vec::new();
    };
    let mut sites = Vec::new();
    for (f_id, function) in module.functions.iter() {
        let owner = BodyOwner::Function(f_id);
        let (Some(body), Some(bi)) = (module.bodies.get(&owner), inference.per_fn.get(&owner)) else {
            continue;
        };
        sites.extend(identity_body_sites(ws, function, body, bi));
    }
    sites
}

/// `@identity_storage` is the Arena store's privilege, with the trust
/// boundary of `@frame_ref_source`: only the stdlib arena file may use it, and
/// only on a Vec field.
fn capability_errors(module: &Module) -> Vec<CompilerError> {
    let file = module.file.as_str();
    let is_arena_stdlib =
        is_stdlib_module_file(file) && (file.ends_with("/arena.rock") || file.ends_with(":arena.rock>"));
    let mut errs = Vec::new();
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
        errs.extend(capability_errors(module).into_iter().map(|e| (module.id, e)));
        if identity_storage_exempt(ws, module) {
            continue;
        }
        errs.extend(
            identity_storage_sites(ws, module)
                .into_iter()
                .map(|site| (module.id, CompilerError::new_with_label(site.span, site.message))),
        );
    }
    for (mid, err) in errs {
        ws.module_mut(mid).interface_errors.push(err);
    }
}
