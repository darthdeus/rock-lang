//! Backend-agnostic `@deco` application-argument SELECTION, shared by both
//! codegen backends (skull-codegen-lua's `decorator_call_args`,
//! skull-codegen-js's `decorator_call_args_js`). The selection rules (named arg
//! wins, else the param's default, else a hard error for a required param with
//! no arg) live HERE once; each backend only maps a chosen [`DecoArg`] to its
//! own literal syntax + default token.
//!
//! [`DecoratorTable`] holds the other backend-neutral decorator decisions:
//! which annotation names which definition, which definitions a program emits,
//! and which applications it registers at load.

use bone_semantic::hir::{AnnoValue, Annotation, BodyOwner, DecoratorDef, FnBackend, Function};
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::ModuleId;
use comfy_la_arena::Idx;
use rustc_hash::{FxHashMap, FxHashSet};
use skull_parser::CompilerError;
use ustr::Ustr;

use crate::code_identity::{CodeArtifact, CodeIdentity};
use crate::instance::Instance;
use crate::mir::FuncRef;

/// The value chosen for one `deco` param on one `@deco` application.
pub enum DecoArg<'a> {
    /// A named annotation arg supplied this param's value.
    Value(&'a AnnoValue),
    /// The param was omitted; the backend fills its default token
    /// (`nil`/`null`), and the callee prologue substitutes the real default.
    Default,
    /// The param was omitted and its explicit `null` overrides its default
    /// (EXPLICIT_NULL_DEFAULT): the backend passes its `Lit::Omitted` marker,
    /// the only value the callee prologue fills.
    Omitted,
}

/// One [`DecoArg`] per `deco` param, in declaration order: the named annotation
/// arg if present, else `Default` if the param has one, else a hard error for a
/// required param with no arg.
pub fn select_decorator_args<'a>(
    ws: &Workspace,
    module: ModuleId,
    decorator: Idx<DecoratorDef>,
    ann: &'a Annotation,
) -> Result<Vec<DecoArg<'a>>, CompilerError> {
    let d = &ws.module(module).decorators[decorator];
    let body = ws
        .module(module)
        .bodies
        .get(&BodyOwner::Decorator(decorator));
    let mut out = Vec::with_capacity(d.param_decls.len());
    for (index, pd) in d.param_decls.iter().enumerate() {
        let pname = pd.name;
        let named = ann
            .args
            .iter()
            .find(|a| a.name.as_ref().map_or(false, |n| n.name == pname));
        if let Some(a) = named {
            out.push(DecoArg::Value(&a.value));
        } else if body
            .is_some_and(|body| crate::lower::param_null_overrides_default(ws, body, index, pd.ty))
        {
            out.push(DecoArg::Omitted);
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

/// A `deco` definition: its module and index there.
pub type DecoratorRef = (ModuleId, Idx<DecoratorDef>);

/// The emitted symbol of a `deco` definition, from its definition key.
pub fn decorator_symbol(def_key: &str) -> String {
    format!(
        "__deco_{}",
        crate::mangle::symbol_of_key(ustr::Ustr::from(def_key))
    )
}

/// Every `deco` in the workspace by definition key. An annotation names a
/// decorator through its resolved key, so a package decorator that shares a
/// spelling with another one is still a different definition.
pub struct DecoratorTable {
    by_key: FxHashMap<Ustr, DecoratorRef>,
    definitions: Vec<DecoratorRef>,
}

/// One `@deco` annotation on a function, resolved to its definition.
pub struct DecoratorApplication<'w> {
    pub func: FuncRef,
    pub function: &'w Function,
    pub decorator: DecoratorRef,
    pub annotation: &'w Annotation,
}

/// Which `@sim` targets count as generated CodeRef values.
#[derive(Clone, Copy)]
pub enum SimValues<'a> {
    None,
    All,
    Selected(&'a FxHashSet<Ustr>),
}

impl DecoratorTable {
    pub fn collect(ws: &Workspace) -> Self {
        let mut by_key = FxHashMap::default();
        let mut definitions = Vec::new();
        for module in ws.modules.iter() {
            for (did, d) in module.decorators.iter() {
                by_key.insert(d.def_key, (module.id, did));
                if !crate::definition::is_compiler_owned_decorator(d) {
                    definitions.push((module.id, did));
                }
            }
        }
        Self {
            by_key,
            definitions,
        }
    }

    /// The decorator an annotation applies, if it names one.
    pub fn resolve(&self, ann: &Annotation) -> Option<DecoratorRef> {
        self.by_key
            .get(&ann.resolved_name.unwrap_or(ann.name.name))
            .copied()
    }

    /// Whether any annotation on `f` applies a decorator, compiler-owned ones
    /// included. Generated text names such a function, so it is a root.
    pub fn is_decorated(&self, f: &Function) -> bool {
        f.annotations.iter().any(|ann| self.resolve(ann).is_some())
    }

    /// Every decorator the program defines as a function, in declaration order.
    /// Compiler-owned decorators have no body to emit.
    pub fn definitions(&self) -> &[DecoratorRef] {
        &self.definitions
    }

    /// Every application of a program-defined decorator to a function `target`
    /// serves, in module, function and annotation order. These are the
    /// registration calls a program makes at load.
    pub fn applications<'w>(
        &self,
        ws: &'w Workspace,
        target: FnBackend,
    ) -> Vec<DecoratorApplication<'w>> {
        let mut out = Vec::new();
        for module in ws.source_modules() {
            for (fid, f) in module.functions.iter() {
                if !serves(f, target) {
                    continue;
                }
                for ann in &f.annotations {
                    let Some(decorator) = self.resolve(ann) else {
                        continue;
                    };
                    let d = &ws.module(decorator.0).decorators[decorator.1];
                    if crate::definition::is_compiler_owned_decorator(d) {
                        continue;
                    }
                    out.push(DecoratorApplication {
                        func: FuncRef {
                            module: module.id,
                            func: fid,
                        },
                        function: f,
                        decorator,
                        annotation: ann,
                    });
                }
            }
        }
        out
    }

    /// The [`definitions`](Self::definitions) some function `target` serves
    /// applies, in declaration order.
    pub fn applied_definitions(&self, ws: &Workspace, target: FnBackend) -> Vec<DecoratorRef> {
        let applied: FxHashSet<DecoratorRef> = self
            .applications(ws, target)
            .into_iter()
            .map(|app| app.decorator)
            .collect();
        self.definitions
            .iter()
            .copied()
            .filter(|decorator| applied.contains(decorator))
            .collect()
    }

    /// Mark the functions generated registration glue passes as values:
    /// decorated functions and the selected `@sim` targets. They have no MIR
    /// value site, but they are escaping Rock values and use the same CodeRefs.
    pub fn mark_generated_code_values(
        &self,
        ws: &Workspace,
        target: FnBackend,
        sims: SimValues<'_>,
        artifact: &mut CodeArtifact,
    ) {
        for module in ws.source_modules() {
            for (fid, f) in module.functions.iter() {
                if !serves(f, target) {
                    continue;
                }
                let sim = f.annotations.iter().any(|ann| ann.is_builtin("sim"))
                    && match sims {
                        SimValues::None => false,
                        SimValues::All => true,
                        SimValues::Selected(names) => names.contains(&f.name),
                    };
                if sim || self.is_decorated(f) {
                    artifact.mark_value(CodeIdentity::Named(Instance::base(FuncRef {
                        module: module.id,
                        func: fid,
                    })));
                }
            }
        }
    }
}

/// A function serves `target` unless it is pinned to another backend.
fn serves(f: &Function, target: FnBackend) -> bool {
    f.backend == FnBackend::Any || f.backend == target
}
