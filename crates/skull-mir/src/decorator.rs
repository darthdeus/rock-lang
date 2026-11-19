use bone_core::hir::{AnnoValue, Annotation, BodyOwner, DecoratorDef, FnBackend, Function};
use bone_core::prelude::ModuleId;
use bone_core::workspace::Workspace;
use comfy_la_arena::Idx;
use rustc_hash::{FxHashMap, FxHashSet};
use skull_parser::CompilerError;
use ustr::Ustr;

use crate::code_identity::{CodeArtifact, CodeIdentity};
use crate::instance::Instance;
use crate::mir::FuncRef;

pub enum DecoArg<'a> {
    Value(&'a AnnoValue),
    Default,
    Omitted,
}

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

pub(crate) type DecoratorRef = (ModuleId, Idx<DecoratorDef>);

pub fn decorator_symbol(def_key: &str) -> String {
    format!(
        "__deco_{}",
        crate::mangle::symbol_of_key(ustr::Ustr::from(def_key))
    )
}

pub struct DecoratorTable {
    by_key: FxHashMap<Ustr, DecoratorRef>,
    definitions: Vec<DecoratorRef>,
}

pub struct DecoratorApplication<'w> {
    pub func: FuncRef,
    pub function: &'w Function,
    pub decorator: DecoratorRef,
    pub annotation: &'w Annotation,
}

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

    pub fn resolve(&self, ann: &Annotation) -> Option<DecoratorRef> {
        self.by_key
            .get(&ann.resolved_name.unwrap_or(ann.name.name))
            .copied()
    }

    pub fn is_decorated(&self, f: &Function) -> bool {
        f.annotations.iter().any(|ann| self.resolve(ann).is_some())
    }

    pub fn definitions(&self) -> &[DecoratorRef] {
        &self.definitions
    }

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

fn serves(f: &Function, target: FnBackend) -> bool {
    f.backend == FnBackend::Any || f.backend == target
}
