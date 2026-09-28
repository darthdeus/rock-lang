//! The identity-storage declaration check (docs/identity-storage-plan.md S0a)
//! and its transitional package exemptions.
//!
//! Enforcement is on by default. A package on [`IDENTITY_STORAGE_EXEMPT`]
//! gets no diagnostics; `rock identity-storage-census` counts its sites and
//! the census test pins those counts per kind, so a migration that removes
//! sites updates the pinned table and drops the package here when it reaches
//! zero.

use bone_core::identity_storage::{
    identity_root_storage_violation, identity_storage_field_violation, identity_storage_violation,
    is_identity_storage_field, IdentityStorageVia, IdentityStorageViolation,
    IDENTITY_STORAGE_ANNOTATION,
};
use bone_core::modules::is_stdlib_module_file;

use crate::hir::{Module, TypeCtor, TypeExpr, VariantPayload};
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
    "chess_game",
    "chesslings",
    "sample_game_c",
    "sample_game_c_v0",
    "sample_game_b",
    "decorator_systems",
    "fm_core",
    "sample-game",
    "idle_core",
    "logo",
    "mag",
    "mag_content",
    "mag_encounter_worker",
    "mag_simulation",
    "mag_world",
    "match3",
    "match3_core",
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
    /// A global holding an identity under a nullable wrapper (not a root).
    Global,
}

impl IdentityStorageKind {
    pub fn label(self) -> &'static str {
        match self {
            IdentityStorageKind::Field => "field",
            IdentityStorageKind::Container => "container",
            IdentityStorageKind::Payload => "payload",
            IdentityStorageKind::Newtype => "newtype",
            IdentityStorageKind::Global => "global",
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
        push(
            identity_root_storage_violation(ws, global.ty),
            IdentityStorageKind::Global,
            global.span,
            format!("global `{}`", global.name),
        );
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
