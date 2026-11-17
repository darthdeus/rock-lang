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
    let mut json_name_seen = false;
    for anno in annos {
        validate_one(pos, anno, deco_names)?;
        if anno.is_builtin("json_name") {
            if json_name_seen {
                comp_error!(anno.span, "duplicate @json_name on field");
            }
            json_name_seen = true;
        }
    }
    Ok(())
}

pub fn json_field_name(annos: &[Annotation]) -> Option<Ustr> {
    annos.iter().find_map(|anno| {
        if anno.is_builtin("json_name") {
            if let Some(arg) = anno.args.first() {
                if let AnnoValue::String(name) = &arg.value {
                    return Some(*name);
                }
            }
        }
        None
    })
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
    use AnnoPos::*;
    let args = anno.args.as_slice();
    let positional = |len, value_matches: fn(&AnnoValue) -> bool| {
        args.len() == len
            && args
                .iter()
                .all(|arg| arg.name.is_none() && value_matches(&arg.value))
    };
    let string = |value: &AnnoValue| matches!(value, AnnoValue::String(_));
    let nonempty_string =
        |value: &AnnoValue| matches!(value, AnnoValue::String(name) if !name.is_empty());
    let ident = |value: &AnnoValue| matches!(value, AnnoValue::Ident(_));
    let check = |allowed, valid: bool, message: &str| {
        at(anno, pos, allowed)?;
        if !valid {
            comp_error!(anno.span, "{message}");
        }
        Ok(())
    };
    let no_args_at = |allowed| {
        at(anno, pos, allowed)?;
        no_args(anno)
    };
    match name.as_str() {
        "deprecated" => check(
            FnItem,
            args.is_empty() || positional(1, string),
            "`@deprecated` takes an optional replacement message string",
        ),
        "test" => {
            at(anno, pos, FnItem)?;
            test_args(anno)
        }
        "json_omit_if" => check(
            StructField,
            positional(1, ident),
            "`@json_omit_if` requires one predicate function name",
        ),
        "json_name" => check(
            StructField,
            positional(1, nonempty_string),
            "`@json_name` requires one nonempty string argument",
        ),
        // The author asserts equivalence to tail(prepare(receiver), args).
        // Lowering only checks whether args can be constructed after prepare.
        "preflight" => check(
            FnItem,
            positional(2, nonempty_string),
            "`@preflight` requires prepare and tail function name strings",
        ),
        "nomangle" => {
            if !matches!(pos, FnItem | StructItem) {
                comp_error!(
                    anno.span,
                    "annotation `@nomangle` is not allowed on {} -- it applies to a fn or a struct",
                    pos.describe()
                );
            }
            no_args(anno)
        }
        "pure" | "ignore" | "debug_impure" | "intrinsic" | "native_required" | "nowrite"
        | "writes_elems" | "returns_nonneg" | "frame_ref_source" | "identity_erasure"
        | "relocates" | "struct_expand" | "shader_expand" | "vertex_expand" | "uniform_expand"
        | "build_var_expand" | "embed_expand" | "uniform_values" | "scene_type_key"
        | "materialize" | "sort_key_wide" | "sort_key_lt" | "gpu_only" | "scalar_abi"
        | "inline" | "sim" | "sim_entry" | "sim_render" | "shadow_root" => no_args_at(FnItem),
        // The result is below the place's live length at return.
        "returns_index_below" => check(
            FnItem,
            positional(1, string),
            "`@returns_index_below` takes one string: a parameter name, optionally followed by `.field`s",
        ),
        "sim_env" => check(
            FnItem,
            positional(2, string),
            "`@sim_env` takes exactly two string arguments: @sim_env(\"KEY\", \"VALUE\")",
        ),
        "state" => comp_error!(
            anno.span,
            "`@state` was removed; delete it: globals have no ownership/world/realization metadata"
        ),
        "host_resource" => comp_error!(
            anno.span,
            "`@host_resource` was removed; delete it: type declarations no longer carry host-projection metadata"
        ),
        "shader" => check(
            FnItem,
            positional(
                1,
                |value| matches!(value, AnnoValue::Ident(id) if matches!(id.name.as_str(), "vertex" | "fragment")),
            ),
            "shader stage must be vertex or fragment",
        ),
        "allow" => check(
            FnItem,
            args.first()
                .is_some_and(|a| a.name.is_none() && ident(&a.value))
                && match args {
                    [_] => true,
                    [_, a] => a.name.is_none() && string(&a.value),
                    _ => false,
                },
            "`@allow` takes a lint name and an optional justification string: @allow(long_inline_lua, \"why\")",
        ),
        "size" => check(
            Param,
            positional(1, |value| matches!(value, AnnoValue::Int(_))),
            "`@size` takes exactly one int argument: @size(32)",
        ),
        "consume" => no_args_at(StructItem),
        "instance" | "unwraps" | "attr" | "uniform" | "textures" | "engine" => no_args_at(Param),
        "json_default" | "json_skip" | "flat" | "identity_storage" => no_args_at(StructField),
        // Opaque boundaries make no assumption about the parameter's representation.
        "repr" => {
            at(anno, pos, Param)?;
            match args {
                [arg] if arg.name.is_none() => match &arg.value {
                    AnnoValue::Ident(k) if k.name.as_str() == "opaque" => return Ok(()),
                    AnnoValue::Ident(k) => comp_error!(
                        anno.span,
                        "unknown `@repr` kind `{}` -- the only kind is `opaque`",
                        k.name
                    ),
                    _ => {}
                },
                _ => {}
            }
            comp_error!(
                anno.span,
                "`@repr` takes exactly one bare kind identifier: @repr(opaque)"
            );
        }
        "lifecycle" | "artifact_definition" => {
            at(anno, pos, DecoItem)?;
            validate_decorator_args(anno)
        }
        "artifact_catalog" => check(
            FnItem,
            matches!(args, [a] if a.name.as_ref().is_some_and(|n| n.name.as_str() == "catalog") && string(&a.value)),
            "`@artifact_catalog` requires one named string argument: catalog=",
        ),
        _ if deco_names.contains(&name) => at(anno, pos, FnItem),
        _ => comp_error!(anno.span, "unknown annotation `@{name}`"),
    }
}

fn validate_decorator_args(anno: &Annotation) -> Result<(), CompilerError> {
    let (required, optional, unnamed, invalid, missing) = if anno.is_builtin("lifecycle") {
        (
            ["phase", "owner"],
            Some("stage_param"),
            "`@lifecycle` arguments must be named: phase=, owner=, stage_param=",
            "`@lifecycle` accepts string arguments phase=, owner=, optional stage_param=",
            "`@lifecycle` requires phase= and owner=",
        )
    } else {
        (
            ["catalog", "key_param"],
            None,
            "`@artifact_definition` arguments must be named: catalog=, key_param=",
            "`@artifact_definition` accepts string arguments catalog= and key_param=",
            "`@artifact_definition` requires catalog= and key_param=",
        )
    };
    for arg in &anno.args {
        let Some(key) = &arg.name else {
            comp_error!(anno.span, "{unnamed}");
        };
        let key = key.name.as_str();
        if !(Some(key) == optional || required.contains(&key))
            || !matches!(arg.value, AnnoValue::String(_))
        {
            comp_error!(anno.span, "{invalid}");
        }
    }
    if !required.into_iter().all(|key| {
        anno.args
            .iter()
            .any(|a| a.name.as_ref().is_some_and(|n| n.name.as_str() == key))
    }) {
        comp_error!(anno.span, "{missing}");
    }
    Ok(())
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
        comp_error!(
            anno.span,
            "annotation `@{}` takes no arguments",
            anno.name.name
        );
    }
    Ok(())
}

/// A backend the `@test` runners execute on. `@test(backends = "lua")` limits
/// a test to the listed backends; the others neither compile nor run it and
/// count it apart from their skips.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum TestBackend {
    Lua,
    Js,
    Glsl,
}

impl TestBackend {
    pub const ALL: [TestBackend; 3] = [TestBackend::Lua, TestBackend::Js, TestBackend::Glsl];

    pub fn name(self) -> &'static str {
        match self {
            TestBackend::Lua => "lua",
            TestBackend::Js => "js",
            TestBackend::Glsl => "glsl",
        }
    }
}

/// Parse the value of `@test(backends = "...")`: a comma-separated list of
/// backend names. `Err` carries the complete diagnostic text.
pub fn parse_test_backends(list: &str) -> std::result::Result<Vec<TestBackend>, String> {
    let known = || {
        TestBackend::ALL
            .iter()
            .map(|b| b.name())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut out = Vec::new();
    for raw in list.split(',') {
        let name = raw.trim();
        if name.is_empty() {
            return Err(format!(
                "`@test(backends = ...)` needs a comma-separated list of backend names (known: {})",
                known()
            ));
        }
        let Some(backend) = TestBackend::ALL.into_iter().find(|b| b.name() == name) else {
            return Err(format!(
                "`@test(backends = ...)`: unknown backend `{name}` (known: {})",
                known()
            ));
        };
        if out.contains(&backend) {
            return Err(format!(
                "`@test(backends = ...)` lists backend `{name}` twice"
            ));
        }
        out.push(backend);
    }
    Ok(out)
}

fn test_args(anno: &Annotation) -> Result<(), CompilerError> {
    const TEST_KEYS: &[&str] = &[
        "output",
        "returns",
        "expect_failure",
        "compile_error",
        "type_holes",
        "backends",
    ];
    for arg in &anno.args {
        match &arg.name {
            Some(key) if key.name.as_str() == "backends" => match &arg.value {
                AnnoValue::String(list) => {
                    if let Err(message) = parse_test_backends(list.as_str()) {
                        comp_error!(anno.span, "{}", message);
                    }
                }
                _ => comp_error!(
                    anno.span,
                    "`@test(backends = ...)` takes a string such as \"lua\" or \"lua, js\""
                ),
            },
            Some(key) if TEST_KEYS.contains(&key.name.as_str()) => {}
            Some(key) => {
                comp_error!(
                    anno.span,
                    "`@test` has no `{}` argument (known: output, returns, expect_failure, compile_error, type_holes, backends)",
                    key.name
                );
            }
            None => {
                comp_error!(
                    anno.span,
                    "`@test` arguments must be named: output=, returns=, expect_failure=, compile_error=, type_holes=, backends="
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{TestBackend, parse_test_backends};

    #[test]
    fn deprecated_accepts_only_an_optional_positional_string_on_functions() {
        use super::*;
        use crate::hir::AnnoArg;
        use skull_parser::Ident;

        let span = Span::new(Ustr::from("<annotation>"), (0, 0), (0, 0), (0, 0));
        let ident = |name| Ident {
            name: Ustr::from(name),
            span,
        };
        let mut annotation = Annotation {
            name: ident("deprecated"),
            span,
            args: Vec::new(),
            resolved_name: None,
            resolved_ident_list: None,
        };
        let names = FxHashSet::default();
        assert!(
            validate_annotations(AnnoPos::FnItem, std::slice::from_ref(&annotation), &names)
                .is_ok()
        );
        annotation.args.push(AnnoArg {
            name: None,
            value: AnnoValue::String(Ustr::from("use fresh")),
        });
        assert!(
            validate_annotations(AnnoPos::FnItem, std::slice::from_ref(&annotation), &names)
                .is_ok()
        );
        annotation.args[0].name = Some(ident("message"));
        assert!(
            validate_annotations(AnnoPos::FnItem, std::slice::from_ref(&annotation), &names)
                .is_err()
        );
        annotation.args[0].name = None;
        annotation.args[0].value = AnnoValue::Int(1);
        assert!(
            validate_annotations(AnnoPos::FnItem, std::slice::from_ref(&annotation), &names)
                .is_err()
        );
        annotation.args[0].value = AnnoValue::String(Ustr::from("use fresh"));
        annotation.args.push(annotation.args[0].clone());
        assert!(
            validate_annotations(AnnoPos::FnItem, std::slice::from_ref(&annotation), &names)
                .is_err()
        );
        annotation.args.clear();
        assert!(validate_annotations(AnnoPos::Param, &[annotation], &names).is_err());
    }

    #[test]
    fn backends_list_parses_single_and_comma_separated_names() {
        assert_eq!(parse_test_backends("lua"), Ok(vec![TestBackend::Lua]));
        assert_eq!(
            parse_test_backends(" lua ,js,glsl"),
            Ok(vec![TestBackend::Lua, TestBackend::Js, TestBackend::Glsl])
        );
    }

    #[test]
    fn backends_list_rejects_unknown_empty_and_repeated_names() {
        let unknown = parse_test_backends("lua, luajit").unwrap_err();
        assert!(
            unknown.contains("unknown backend `luajit`")
                && unknown.contains("known: lua, js, glsl"),
            "{unknown}"
        );
        assert!(
            parse_test_backends("")
                .unwrap_err()
                .contains("comma-separated")
        );
        assert!(
            parse_test_backends("lua,")
                .unwrap_err()
                .contains("comma-separated")
        );
        assert!(parse_test_backends("js, js").unwrap_err().contains("twice"));
    }
}
