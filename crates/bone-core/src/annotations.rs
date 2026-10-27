//! Annotation registry + validation.
//!
//! One table decides which annotation names are legal at which declaration
//! positions and what arguments they take. Fail closed: a position with no
//! registered names rejects every annotation, and unknown names reject
//! everywhere -- the compiler refuses what it does not understand instead
//! of silently ignoring it. Names backed by a user `deco` declaration are
//! legal on fns; their args are the deco's own params, checked at
//! decorator application, not here.

use crate::hir::{AnnoValue, Annotation};
use crate::prelude::*;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AnnoPos {
    FnItem,
    StructItem,
    EnumItem,
    NewtypeItem,
    GlobalItem,
    DecoItem,
    Param,
    StructField,
    EnumVariantField,
    DecoParam,
}

impl AnnoPos {
    pub fn describe(self) -> &'static str {
        match self {
            AnnoPos::FnItem => "a fn",
            AnnoPos::StructItem => "a struct",
            AnnoPos::EnumItem => "an enum",
            AnnoPos::NewtypeItem => "a newtype",
            AnnoPos::GlobalItem => "a global",
            AnnoPos::DecoItem => "a deco declaration",
            AnnoPos::Param => "a fn parameter",
            AnnoPos::StructField => "a struct field",
            AnnoPos::EnumVariantField => "an enum record-variant field",
            AnnoPos::DecoParam => "a deco parameter",
        }
    }
}

pub fn validate_annotations(
    pos: AnnoPos,
    annos: &[Annotation],
    deco_names: &FxHashSet<Ustr>,
) -> Result<(), CompilerError> {
    for anno in annos {
        validate_one(pos, anno, deco_names)?;
    }
    Ok(())
}

fn validate_one(
    pos: AnnoPos,
    anno: &Annotation,
    deco_names: &FxHashSet<Ustr>,
) -> Result<(), CompilerError> {
    let name = anno.name.name;
    if let Some(resolved_name) = anno.resolved_name {
        if deco_names.contains(&resolved_name) {
            return at(anno, pos, AnnoPos::FnItem);
        }
        comp_error!(anno.span, "unknown annotation `@{name}`");
    }
    match name.as_str() {
        "test" => {
            at(anno, pos, AnnoPos::FnItem)?;
            test_args(anno)
        }
        "nomangle" => {
            match pos {
                AnnoPos::FnItem | AnnoPos::StructItem => {}
                _ => comp_error!(
                    anno.span,
                    "annotation `@nomangle` is not allowed on {} -- it applies to a fn or a struct",
                    pos.describe()
                ),
            }
            no_args(anno)
        }
        "pure" | "ignore" | "debug_impure" | "intrinsic" | "native_required"
        | "frame_ref_source" | "identity_erasure" | "relocates"
        | "struct_expand" | "shader_expand" | "vertex_expand" | "uniform_expand"
        | "build_var_expand" | "embed_expand"
        | "uniform_values" | "scene_type_key"
        | "materialize" | "gpu_only"
        | "scalar_abi" | "inline" | "sim" | "sim_entry"
        | "sim_render" | "shadow_root" => {
            at(anno, pos, AnnoPos::FnItem)?;
            no_args(anno)
        }
        "sim_env" => {
            at(anno, pos, AnnoPos::FnItem)?;
            let ok = anno.args.len() == 2
                && anno
                    .args
                    .iter()
                    .all(|a| a.name.is_none() && matches!(a.value, AnnoValue::String(_)));
            if !ok {
                comp_error!(
                    anno.span,
                    "`@sim_env` takes exactly two string arguments: @sim_env(\"KEY\", \"VALUE\")"
                );
            }
            Ok(())
        }
        "state" => comp_error!(
            anno.span,
            "`@state` was removed; delete it: globals have no ownership/world/realization metadata"
        ),
        "shader" => {
            at(anno, pos, AnnoPos::FnItem)?;
            let ok = anno.args.len() == 1
                && anno.args[0].name.is_none()
                && matches!(anno.args[0].value, AnnoValue::Ident(_));
            if !ok {
                comp_error!(anno.span, "`@shader` takes exactly one stage argument: @shader(compute)");
            }
            Ok(())
        }
        "workgroup" => {
            at(anno, pos, AnnoPos::FnItem)?;
            let ok = anno.args.len() == 3
                && anno.args[0].name.is_none()
                && matches!(anno.args[0].value, AnnoValue::Int(_))
                && anno.args[1].name.is_none()
                && matches!(anno.args[1].value, AnnoValue::Int(_))
                && anno.args[2].name.is_none()
                && matches!(anno.args[2].value, AnnoValue::Int(_));
            if !ok {
                comp_error!(anno.span, "`@workgroup` takes exactly three integer sizes: @workgroup(64, 1, 1)");
            }
            Ok(())
        }
        "dispatch" => {
            at(anno, pos, AnnoPos::FnItem)?;
            let ok = anno.args.len() == 1
                && anno.args[0].name.is_none()
                && matches!(anno.args[0].value, AnnoValue::Int(_));
            if !ok {
                comp_error!(anno.span, "`@dispatch` takes exactly one integer dimension: @dispatch(1)");
            }
            Ok(())
        }
        "allow" => {
            at(anno, pos, AnnoPos::FnItem)?;
            let ok = match anno.args.as_slice() {
                [l] => l.name.is_none() && matches!(l.value, AnnoValue::Ident(_)),
                [l, j] => {
                    l.name.is_none()
                        && matches!(l.value, AnnoValue::Ident(_))
                        && j.name.is_none()
                        && matches!(j.value, AnnoValue::String(_))
                }
                _ => false,
            };
            if !ok {
                comp_error!(
                    anno.span,
                    "`@allow` takes a lint name and an optional justification string: @allow(long_inline_lua, \"why\")"
                );
            }
            Ok(())
        }
        "consume" => {
            at(anno, pos, AnnoPos::StructItem)?;
            no_args(anno)
        }
        // `@host_resource` marks a TYPE as a name for something the host owns:
        // a GL object, a canvas, a shader, a mesh upload, a playing voice, an
        // FFI pointer. The runtime host-projection classification reads it
        // out of `__ROCK_TYPE_TABLE` instead of keeping its own list of names.
        //
        // The optional bare identifier is the NATIVE LEDGER class
        // (the host's native ledger), for the types the ledger tracks. A
        // host resource the ledger does not track -- an FFI pointer, a
        // `NativeBuffer` -- carries the annotation with no class, and the
        // runtime has nothing to ask about it.
        "host_resource" => {
            at_type_item(anno, pos)?;
            match &anno.args[..] {
                [] => Ok(()),
                [a] if a.name.is_none() && matches!(a.value, AnnoValue::Ident(_)) => Ok(()),
                _ => comp_error!(
                    anno.span,
                    "`@host_resource` takes nothing, or one bare native ledger class: \
                     @host_resource(voice)"
                ),
            }
        }
        "size" => {
            at(anno, pos, AnnoPos::Param)?;
            let ok = anno.args.len() == 1
                && anno.args[0].name.is_none()
                && matches!(anno.args[0].value, AnnoValue::Int(_));
            if !ok {
                comp_error!(anno.span, "`@size` takes exactly one int argument: @size(32)");
            }
            Ok(())
        }
        "instance" => {
            at(anno, pos, AnnoPos::Param)?;
            no_args(anno)
        }
        // The parameter-level REPRESENTATION contract.
        // `opaque` states that the body
        // makes no assumption about this parameter's physical form, so the
        // column-container rejection skips it. It names a KIND rather than
        // being nullary because the same slot is where a future `boxed` /
        // `columns` contract belongs; an unrecognized kind rejects rather than
        // reading as "some contract is in force".
        //
        // Whether the fn is an opaque boundary at all is checked where the
        // param is built (`bone-lower`), which has the fn in hand; this table
        // sees the annotation alone.
        "repr" => {
            at(anno, pos, AnnoPos::Param)?;
            let kind = match &anno.args[..] {
                [a] if a.name.is_none() => match &a.value {
                    AnnoValue::Ident(k) => Some(k.name),
                    _ => None,
                },
                _ => None,
            };
            match kind {
                Some(k) if k.as_str() == "opaque" => Ok(()),
                Some(k) => comp_error!(
                    anno.span,
                    "unknown `@repr` kind `{k}` -- the only kind is `opaque`"
                ),
                None => comp_error!(
                    anno.span,
                    "`@repr` takes exactly one bare kind identifier: @repr(opaque)"
                ),
            }
        }
        // GLSL entry input roles: the value-struct param they
        // mark is per-vertex attribute data (`@attr`) or a uniform block
        // (`@uniform`). `@instance` is the per-instance attribute divisor.
        // `@textures` groups sampler params into one struct so the texture unit
        // is compiler-assigned by field order, not hand-numbered at the bind
        // site where two entries can silently share a number. `@engine` is the
        // engine-owned frame/pass block at binding 0 -- a separate ROLE from
        // `@uniform` because it differs in producer and lifetime, not in layout.
        "attr" | "uniform" | "textures" | "engine" => {
            at(anno, pos, AnnoPos::Param)?;
            no_args(anno)
        }
        "storage" => {
            at(anno, pos, AnnoPos::Param)?;
            let ok = anno.args.len() == 1
                && anno.args[0].name.is_none()
                && matches!(anno.args[0].value, AnnoValue::Ident(_));
            if !ok {
                comp_error!(anno.span, "`@storage` takes one access argument: @storage(read_write)");
            }
            Ok(())
        }
        "flat" | "identity_storage" => {
            at(anno, pos, AnnoPos::StructField)?;
            no_args(anno)
        }
        "lifecycle" => {
            at(anno, pos, AnnoPos::DecoItem)?;
            const KEYS: &[&str] = &["phase", "owner", "stage_param"];
            for arg in &anno.args {
                let Some(key) = arg.name.as_ref() else {
                    comp_error!(anno.span, "`@lifecycle` arguments must be named: phase=, owner=, stage_param=");
                };
                if !KEYS.contains(&key.name.as_str()) || !matches!(arg.value, AnnoValue::String(_)) {
                    comp_error!(anno.span, "`@lifecycle` accepts string arguments phase=, owner=, optional stage_param=");
                }
            }
            let has = |name: &str| anno.args.iter().any(|a| a.name.as_ref().is_some_and(|n| n.name.as_str() == name));
            if !has("phase") || !has("owner") {
                comp_error!(anno.span, "`@lifecycle` requires phase= and owner=");
            }
            Ok(())
        }
        "artifact_definition" => {
            at(anno, pos, AnnoPos::DecoItem)?;
            const KEYS: &[&str] = &["catalog", "key_param"];
            for arg in &anno.args {
                let Some(key) = arg.name.as_ref() else {
                    comp_error!(
                        anno.span,
                        "`@artifact_definition` arguments must be named: catalog=, key_param="
                    );
                };
                if !KEYS.contains(&key.name.as_str())
                    || !matches!(arg.value, AnnoValue::String(_))
                {
                    comp_error!(
                        anno.span,
                        "`@artifact_definition` accepts string arguments catalog= and key_param="
                    );
                }
            }
            let has = |name: &str| {
                anno.args
                    .iter()
                    .any(|a| a.name.as_ref().is_some_and(|n| n.name.as_str() == name))
            };
            if !has("catalog") || !has("key_param") {
                comp_error!(
                    anno.span,
                    "`@artifact_definition` requires catalog= and key_param="
                );
            }
            Ok(())
        }
        "artifact_catalog" => {
            at(anno, pos, AnnoPos::FnItem)?;
            if anno.args.len() != 1
                || anno.args[0].name.as_ref().is_none_or(|name| name.name.as_str() != "catalog")
                || !matches!(anno.args[0].value, AnnoValue::String(_))
            {
                comp_error!(
                    anno.span,
                    "`@artifact_catalog` requires one named string argument: catalog="
                );
            }
            Ok(())
        }
        _ if deco_names.contains(&name) => at(anno, pos, AnnoPos::FnItem),
        _ => {
            comp_error!(anno.span, "unknown annotation `@{name}`");
        }
    }
}

/// `@repr(opaque)` waives a rule that exists ONLY for bodies the compiler
/// cannot see into, so on a Rock body there is no assumption to waive and the
/// annotation would be a claim nothing consults. Rejected there rather than
/// ignored -- a reader must not be able to believe a contract is in force.
///
/// Separate from [`validate_annotations`] because it needs the enclosing fn,
/// which the registry table deliberately does not see.
pub fn validate_param_repr_against_body(
    fn_name: Ustr,
    param_decls: &[crate::hir::ParamDecl],
    is_opaque_boundary: bool,
) -> Result<(), CompilerError> {
    if is_opaque_boundary {
        return Ok(());
    }
    for pd in param_decls {
        for anno in &pd.annotations {
            if anno.is_builtin("repr") {
                comp_error!(
                    anno.span,
                    "`@repr(opaque)` applies to a parameter of an `inline(..)` or \
                     `extern` boundary; `{fn_name}` has a Rock body, so the compiler \
                     already knows the parameter's representation and there is no \
                     assumption to waive"
                );
            }
        }
    }
    Ok(())
}

/// An annotation that decorates any NOMINAL TYPE declaration. Separate from
/// [`at`] because that one names a single position in its message, and
/// "applies to a struct" would be wrong for three.
fn at_type_item(anno: &Annotation, pos: AnnoPos) -> Result<(), CompilerError> {
    match pos {
        AnnoPos::StructItem | AnnoPos::EnumItem | AnnoPos::NewtypeItem => Ok(()),
        _ => comp_error!(
            anno.span,
            "annotation `@{}` is not allowed on {} -- it applies to a struct, an enum or a newtype",
            anno.name.name,
            pos.describe()
        ),
    }
}

fn at(anno: &Annotation, pos: AnnoPos, allowed: AnnoPos) -> Result<(), CompilerError> {
    if pos != allowed {
        comp_error!(
            anno.span,
            "annotation `@{}` is not allowed on {} -- it applies to {}",
            anno.name.name,
            pos.describe(),
            allowed.describe()
        );
    }
    Ok(())
}

fn no_args(anno: &Annotation) -> Result<(), CompilerError> {
    if !anno.args.is_empty() {
        comp_error!(anno.span, "annotation `@{}` takes no arguments", anno.name.name);
    }
    Ok(())
}

fn test_args(anno: &Annotation) -> Result<(), CompilerError> {
    const TEST_KEYS: &[&str] = &["output", "returns", "expect_failure", "compile_error", "type_holes"];
    for arg in &anno.args {
        match &arg.name {
            Some(key) if TEST_KEYS.contains(&key.name.as_str()) => {}
            Some(key) => {
                comp_error!(
                    anno.span,
                    "`@test` has no `{}` argument (known: output, returns, expect_failure, compile_error, type_holes)",
                    key.name
                );
            }
            None => {
                comp_error!(
                    anno.span,
                    "`@test` arguments must be named: output=, returns=, expect_failure=, compile_error=, type_holes="
                );
            }
        }
    }
    Ok(())
}
