//! The process stdlib lowered and inferred once, alone, and reused by every
//! batch semantic pass whose user modules cannot change it.
//!
//! A stdlib file consults no user scope, so its lowering and inference read
//! user files only through the bare-name maps they share (types and generic
//! arity, globals, overload families, decorators), and its expansion reads
//! them only through the macro registry. A user module that defines nothing
//! under a stdlib name and no macro under a name a stdlib expansion can look
//! up therefore leaves every stdlib module exactly as a combined pass would
//! build it, and the pass can start from a copy of the stdlib-alone workspace
//! and lower only the user modules. The incremental session reuses its warm
//! modules on the same condition ([`apart_from`]).

use std::borrow::Cow;
use std::sync::OnceLock;

use skull_parser::stdlib::ModuleSet;
use skull_parser::{
    Ast, ItemKind, MacroRegistry, collect_registry, cook_one, is_hygiene_name, macro_reach,
};

use crate::prelude::*;
use crate::semantic_passes::{FragmentContribution, ScopeItem, contribute};

/// The names through which a user file can reach the warm modules.
#[derive(Clone, Debug, Default)]
pub struct WarmNames {
    /// Every bare name a warm module defines, plus the intrinsic type names:
    /// the names through which a user definition reaches a warm lowering.
    defs: FxHashSet<Ustr>,
    /// Every name a warm module's expansion can look up
    /// ([`skull_parser::macro_reach`]): a user macro under one could rewrite
    /// a warm module.
    macro_reach: FxHashSet<Ustr>,
    /// Whether a user macro under no reached name leaves the warm modules'
    /// expansion unchanged: false only when they invoke a macro at the top
    /// level but define none, which expansion against a registry holding a
    /// user macro alone would report.
    admits_macros: bool,
}

/// [`WarmNames`] of the `warm` modules, whose namespace contributions are
/// `contributions`.
pub fn warm_names<'a>(
    warm: &[Ast],
    contributions: impl IntoIterator<Item = &'a FragmentContribution>,
) -> WarmNames {
    let defs = contributions
        .into_iter()
        .flat_map(|c| c.items.iter())
        .filter_map(|item| match item {
            ScopeItem::Def { name, .. } => Some(*name),
            _ => None,
        })
        .chain(bone_core::modules::std_exported_intrinsic_types().map(ustr))
        .collect();
    let invokes = warm
        .iter()
        .flat_map(|ast| &ast.items)
        .any(|item| matches!(item.kind, ItemKind::MacroInvocation { .. }));
    WarmNames {
        defs,
        macro_reach: warm.iter().flat_map(macro_reach).collect(),
        admits_macros: !invokes || !collect_registry(warm).is_empty(),
    }
}

/// Whether the user file `ast` can reach no warm module ([`warm_names`]): it
/// defines nothing under a warm name (a legacy file's items, a v1 file's
/// bare-keyed operator functions, any file's macros; a v1 file's other
/// definitions are keyed by DefKey and cannot collide), and no macro a warm
/// expansion could look up.
pub fn apart_from(warm: &WarmNames, ast: &Ast, contribution: &FragmentContribution) -> bool {
    fn apart(items: &[ScopeItem], legacy: bool, warm: &FxHashSet<Ustr>) -> bool {
        items.iter().all(|item| match item {
            ScopeItem::Def { name, kind, .. } => {
                let bare = legacy || (*kind == "callable" && name.as_str().starts_with("__op_"));
                !(bare && warm.contains(name))
            }
            ScopeItem::Module { items, .. } => apart(items, legacy, warm),
            ScopeItem::Use(_) => true,
        })
    }
    apart(
        &contribution.items,
        ast.dialect == skull_parser::Dialect::Legacy,
        &warm.defs,
    ) && macro_names(ast).all(|name| {
        warm.admits_macros
            && !warm.defs.contains(&name)
            && !warm.macro_reach.contains(&name)
            && !is_hygiene_name(name.as_str())
    })
}

/// The names of the macros `ast` defines (the items the registry collects).
fn macro_names(ast: &Ast) -> impl Iterator<Item = Ustr> + '_ {
    ast.items.iter().filter_map(|item| match &item.kind {
        ItemKind::Function(f) if f.is_macro => Some(f.name.name),
        _ => None,
    })
}

struct StdlibBase {
    /// The stdlib modules expanded, lowered and inferred alone, with no
    /// error anywhere.
    ws: Workspace,
    /// The stdlib's own macros, which a qualifying pass extends with its
    /// user modules' macros.
    registry: MacroRegistry,
    names: WarmNames,
    files: FxHashSet<Ustr>,
}

static BASE: OnceLock<Option<StdlibBase>> = OnceLock::new();

/// The shared base, built on first use; `None` when the stdlib alone does not
/// check cleanly (then a combined pass decides what that means).
fn base() -> Option<&'static StdlibBase> {
    if let Some(base) = BASE.get() {
        return base.as_ref();
    }
    // Built outside the lock: the build runs rayon jobs, and a worker waiting
    // on them can steal another pass that needs the base, which would block
    // forever on a lock its own thread holds. Concurrent first builds are
    // identical; the first one stored wins.
    let built = build();
    let _ = BASE.set(built);
    BASE.get().and_then(Option::as_ref)
}

fn build() -> Option<StdlibBase> {
    let stdlib = skull_parser::stdlib::cached().as_ref().ok()?;
    let registry = collect_registry(stdlib.iter());
    let mut cooked = stdlib.to_vec();
    cooked
        .par_iter_mut()
        .for_each(|ast| cook_one(ast, &registry));
    if cooked.iter().any(|ast| !ast.recovery_errors.is_empty()) {
        return None;
    }
    let mut ws = Workspace::default();
    crate::semantic_passes::lower_and_infer(&mut ws, &cooked).ok()?;
    if has_errors(&ws) {
        return None;
    }
    ws.phase_timings.clear();
    let contributions: Vec<FragmentContribution> = stdlib.iter().map(contribute).collect();
    Some(StdlibBase {
        ws,
        registry,
        names: warm_names(stdlib, &contributions),
        files: stdlib.iter().map(|ast| ast.file).collect(),
    })
}

/// The process stdlib expanded, lowered and inferred alone, when `warm` is
/// the process stdlib itself and it checks cleanly: what a build over `warm`
/// alone produces, already built.
pub fn checked_alone(warm: &std::sync::Arc<[Ast]>) -> Option<Workspace> {
    if !skull_parser::stdlib::is_process_stdlib(warm) {
        return None;
    }
    base().map(StdlibBase::workspace)
}

/// Any lowering, interface or body error in `ws`.
fn has_errors(ws: &Workspace) -> bool {
    ws.inferences
        .iter()
        .flatten()
        .any(|inf| inf.errors.values().any(|errs| !errs.is_empty()))
        || ws.modules.iter().any(|m| {
            !m.interface_errors.is_empty() || m.lowering_errors.values().any(|e| !e.is_empty())
        })
}

impl StdlibBase {
    /// Whether `user` leaves the stdlib modules as a combined pass builds
    /// them: not a stdlib file itself, and apart from every stdlib name.
    fn admits(&self, user: &Ast) -> bool {
        !self.files.contains(&user.file) && apart_from(&self.names, user, &contribute(user))
    }

    /// A copy of the base workspace with its own reuse counters (a clone
    /// would share them with every other copy).
    fn workspace(&self) -> Workspace {
        Workspace {
            reinferred_body_count: Default::default(),
            relowered_body_count: Default::default(),
            ..self.ws.clone()
        }
    }
}

/// The semantic pass over `modules` from the shared stdlib base, or `None`
/// when it does not apply: the shared prefix of `modules` must be the process
/// stdlib itself ([`skull_parser::stdlib::is_process_stdlib`]) and every
/// other module must be admitted. The result equals a combined pass from a
/// default workspace. The prefix stays shared and unexpanded; only the user
/// modules are expanded.
pub(crate) fn semantic_pass(
    modules: &mut ModuleSet,
    aliases: &bone_core::modules::NsAliases,
) -> Option<Result<Workspace, CompilerError>> {
    if !skull_parser::stdlib::is_process_stdlib(modules.shared()) {
        return None;
    }
    let base_timer = Instant::now();
    let base = base()?;
    let base_time = base_timer.elapsed();
    if !modules.rest().iter().all(|user| base.admits(user)) {
        return None;
    }
    // A user macro is under no name the stdlib reaches, so the stdlib
    // expands against this registry as against its own.
    let registry = if modules
        .rest()
        .iter()
        .any(|user| macro_names(user).next().is_some())
    {
        Cow::Owned(base.registry.extended(modules.rest()))
    } else {
        Cow::Borrowed(&base.registry)
    };
    let clone_timer = Instant::now();
    let mut ws = base.workspace();
    ws.phase_timings.push(("stdlib base", base_time));
    ws.log_time("clone base", clone_timer);
    Some((|| {
        let timer = Instant::now();
        crate::semantic_passes::build_namespaces(&mut ws, modules.iter(), aliases)?;
        ws.log_time("namespaces", timer);
        let users = modules.rest_mut();
        let timer = Instant::now();
        users
            .par_iter_mut()
            .for_each(|ast| cook_one(ast, &registry));
        ws.log_time("macro_expand", timer);
        crate::semantic_passes::lower_and_infer(&mut ws, users)?;
        Ok(ws)
    })())
}
