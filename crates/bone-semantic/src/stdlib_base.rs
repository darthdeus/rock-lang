//! The process stdlib lowered and inferred once, alone, and reused by every
//! batch semantic pass whose user modules cannot change it.
//!
//! A stdlib file consults no user scope, so its lowering and inference read
//! user files only through the bare-name maps they share (types and generic
//! arity, globals, overload families, decorators, macros). A user module that
//! defines nothing under a stdlib name and no macro therefore leaves every
//! stdlib module exactly as a combined pass would build it, and the pass can
//! start from a copy of the stdlib-alone workspace and lower only the user
//! modules. The incremental session reuses its warm modules on the same
//! condition ([`apart_from`]).

use std::sync::OnceLock;

use skull_parser::stdlib::ModuleSet;
use skull_parser::{Ast, ItemKind, MacroRegistry, collect_registry, cook_one};

use crate::prelude::*;
use crate::semantic_passes::{FragmentContribution, ScopeItem, contribute};

/// Every bare name the `warm` contributions define, plus the intrinsic type
/// names: the names through which a user file can reach a warm lowering.
pub fn warm_names<'a>(warm: impl IntoIterator<Item = &'a FragmentContribution>) -> FxHashSet<Ustr> {
    warm.into_iter()
        .flat_map(|c| c.items.iter())
        .filter_map(|item| match item {
            ScopeItem::Def { name, .. } => Some(*name),
            _ => None,
        })
        .chain(bone_core::modules::std_exported_intrinsic_types().map(ustr))
        .collect()
}

/// Whether the user file `ast` defines nothing, and no macro, under one of
/// `warm` names ([`warm_names`]): a legacy file's items, a v1 file's
/// bare-keyed operator functions, and any file's macros. A v1 file's other
/// definitions are keyed by DefKey and cannot collide.
pub fn apart_from(warm: &FxHashSet<Ustr>, ast: &Ast, contribution: &FragmentContribution) -> bool {
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
        warm,
    ) && macro_names(ast).all(|name| !warm.contains(&name))
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
    /// The stdlib's own macros: every macro a qualifying pass sees.
    registry: MacroRegistry,
    names: FxHashSet<Ustr>,
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
        names: warm_names(&contributions),
        files: stdlib.iter().map(|ast| ast.file).collect(),
    })
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
    /// them: not a stdlib file itself, no macro, and apart from every stdlib
    /// name.
    fn admits(&self, user: &Ast) -> bool {
        !self.files.contains(&user.file)
            && macro_names(user).next().is_none()
            && apart_from(&self.names, user, &contribute(user))
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
    let base = base()?;
    if !modules.rest().iter().all(|user| base.admits(user)) {
        return None;
    }
    let mut ws = base.workspace();
    Some((|| {
        crate::semantic_passes::build_namespaces(&mut ws, modules.iter(), aliases)?;
        let users = modules.rest_mut();
        let timer = Instant::now();
        users
            .par_iter_mut()
            .for_each(|ast| cook_one(ast, &base.registry));
        ws.log_time("macro_expand", timer);
        crate::semantic_passes::lower_and_infer(&mut ws, users)?;
        Ok(ws)
    })())
}
