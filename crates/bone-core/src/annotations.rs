//! Annotation registry + validation (docs/annotations-design.md).
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
    match name.as_str() {
        "test" => {
            at(anno, pos, AnnoPos::FnItem)?;
            test_args(anno)
        }
        "pure" | "nomangle" | "ignore" | "debug_impure" | "intrinsic" | "native_required"
        | "struct_expand" | "shader_expand" | "vertex_expand" | "uniform_expand" | "uniform_values"
        | "materialize" | "gpu_only"
        | "scalar_abi" | "inline" | "sim"
        | "sim_render" => {
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
        // The parameter-level REPRESENTATION contract
        // (`docs/boundary-contracts.md` 2.4). `opaque` states that the body
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
                    "unknown `@repr` kind `{k}` -- the only kind is `opaque` \
                     (docs/boundary-contracts.md 2.4)"
                ),
                None => comp_error!(
                    anno.span,
                    "`@repr` takes exactly one bare kind identifier: @repr(opaque)"
                ),
            }
        }
        // GLSL entry input roles (docs/shaders.md): the value-struct param they
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
        "flat" => {
            at(anno, pos, AnnoPos::StructField)?;
            no_args(anno)
        }
        _ if deco_names.contains(&name) => at(anno, pos, AnnoPos::FnItem),
        _ => {
            comp_error!(anno.span, "unknown annotation `@{name}` (registry: docs/annotations-design.md)");
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
            if anno.name.name == "repr" {
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
