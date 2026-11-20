//! The process stdlib checked once for builds whose user modules cannot change it.

use std::sync::OnceLock;

use skull_parser::stdlib::ModuleSet;
use skull_parser::{Ast, ItemKind, collect_registry, cook_one, is_hygiene_name, macro_reach};

use crate::prelude::*;
use crate::semantic_passes::{FragmentContribution, ScopeItem};
use bone_core::modules::{DefKind, NsAliases, STD_PACKAGE_ID};

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
/// defines no bare-keyed operator or macro under a warm name, and no macro a
/// warm expansion could look up. Ordinary definitions have package keys.
pub fn apart_from(warm: &WarmNames, ast: &Ast, contribution: &FragmentContribution) -> bool {
    fn apart(items: &[ScopeItem], warm: &FxHashSet<Ustr>) -> bool {
        items.iter().all(|item| match item {
            ScopeItem::Def { name, kind, .. } => {
                let bare = *kind == DefKind::Callable && name.as_str().starts_with("__op_");
                !(bare && warm.contains(name))
            }
            ScopeItem::Module { items, .. } => apart(items, warm),
            ScopeItem::Use(_) => true,
        })
    }
    apart(&contribution.items, &warm.defs)
        && macro_names(ast).all(|name| {
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
        .map(|ast| cook_one(ast, &registry))
        .collect::<Vec<_>>()
        .into_iter()
        .collect::<Result<(), _>>()
        .ok()?;
    if cooked.iter().any(|ast| !ast.recovery_errors.is_empty()) {
        return None;
    }
    let mut ws = Workspace::default();
    crate::semantic_passes::build_namespaces(&mut ws, &cooked, &Default::default()).ok()?;
    crate::semantic_passes::lower_and_infer(&mut ws, &cooked).ok()?;
    if has_errors(&ws) {
        return None;
    }
    ws.phase_timings.clear();
    Some(StdlibBase { ws })
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

pub(crate) fn check_users(
    modules: &mut ModuleSet,
    aliases: &NsAliases,
) -> Option<Result<Workspace, CompilerError>> {
    if !skull_parser::stdlib::is_process_stdlib(modules.shared())
        || aliases
            .packages
            .iter()
            .any(|package| package.id == STD_PACKAGE_ID)
        || modules.rest().iter().any(|ast| {
            is_stdlib_module_file(ast.file.as_str())
                || macro_names(ast).next().is_some()
                || ast
                    .package
                    .as_ref()
                    .is_some_and(|package| package.id == STD_PACKAGE_ID)
        })
    {
        return None;
    }
    let base = base()?;
    let registry = collect_registry(modules.iter());
    let cooked = modules
        .rest()
        .par_iter()
        .map(|raw| {
            let mut cooked = raw.clone();
            cook_one(&mut cooked, &registry).ok()?;
            let contribution = crate::semantic_passes::contribute(&cooked);
            (!defines_operator(&contribution.items)).then_some((cooked, contribution))
        })
        .collect::<Vec<_>>()
        .into_iter()
        .collect::<Option<Vec<_>>>()?;
    let (users, contributions): (Vec<_>, Vec<_>) = cooked.into_iter().unzip();
    let mut ws = base.workspace();
    ws.skip_session_facts = true;
    *modules = ModuleSet::new(modules.shared().clone(), users);
    Some((|| {
        crate::semantic_passes::assemble_namespaces(
            &mut ws,
            &contributions.iter().collect::<Vec<_>>(),
            aliases,
        )?;
        crate::semantic_passes::lower_and_infer(&mut ws, modules.rest())?;
        Ok(ws)
    })())
}

fn defines_operator(items: &[ScopeItem]) -> bool {
    items.iter().any(|item| match item {
        ScopeItem::Def { name, kind, .. } => {
            *kind == DefKind::Callable && name.as_str().starts_with("__op_")
        }
        ScopeItem::Module { items, .. } => defines_operator(items),
        ScopeItem::Use(_) => false,
    })
}

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
    /// A copy of the base workspace with its own reuse counters (a clone
    /// would share them with every other copy).
    fn workspace(&self) -> Workspace {
        Workspace {
            reinferred_body_count: Default::default(),
            relowered_body_count: Default::default(),
            vec_effects: Default::default(),
            ..self.ws.clone()
        }
    }
}
