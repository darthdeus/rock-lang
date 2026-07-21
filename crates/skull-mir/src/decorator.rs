//! Backend-agnostic `@deco` application-argument SELECTION, shared by both
//! codegen backends (skull-codegen-lua's `decorator_call_args`,
//! skull-codegen-js's `decorator_call_args_js`). The selection rules (named arg
//! wins, else the param's default, else a hard error for a required param with
//! no arg) live HERE once; each backend only maps a chosen [`DecoArg`] to its
//! own literal syntax + default token.

use bone_semantic::hir::{Annotation, AnnoValue, DecoratorDef};
use skull_parser::CompilerError;

/// The value chosen for one `deco` param on one `@deco` application.
pub enum DecoArg<'a> {
    /// A named annotation arg supplied this param's value.
    Value(&'a AnnoValue),
    /// The param was omitted; the backend fills its default token
    /// (`nil`/`null`), and the callee prologue substitutes the real default.
    Default,
}

/// One [`DecoArg`] per `deco` param, in declaration order: the named annotation
/// arg if present, else `Default` if the param has one, else a hard error for a
/// required param with no arg.
pub fn select_decorator_args<'a>(
    d: &'a DecoratorDef,
    ann: &'a Annotation,
) -> Result<Vec<DecoArg<'a>>, CompilerError> {
    let mut out = Vec::with_capacity(d.param_decls.len());
    for pd in d.param_decls.iter() {
        let pname = pd.name;
        let named = ann
            .args
            .iter()
            .find(|a| a.name.as_ref().map_or(false, |n| n.name == pname));
        if let Some(a) = named {
            out.push(DecoArg::Value(&a.value));
        } else if pd.has_default {
            out.push(DecoArg::Default);
        } else {
            return Err(CompilerError::new_no_location(format!(
                "skull: decorator `{}` is missing a value for required param `{}`",
                d.name, pname
            )));
        }
    }
    Ok(out)
}
