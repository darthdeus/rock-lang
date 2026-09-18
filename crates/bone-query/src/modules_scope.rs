//! What a modules-v1 fragment can name, for completion (spec 15.3). Every
//! answer comes from the resolver (`bone_lower::resolve`): the tiers decide
//! what a bare name binds, the path walks decide what a qualified prefix
//! names, and a member is offered only if the resolver would let this
//! fragment name it. Nothing here matches names against definitions itself.

use bone_core::modules::{DefKind, DefMeta, FragmentScope, UsePathRef};
use bone_semantic::lower::resolve::{self, CalleeFamily, ResolvedKind, Viewer};
use bone_semantic::prelude::*;

/// The fragment scope of the v1 file `file`, `None` for a legacy file.
pub fn fragment(ws: &Workspace, file: Ustr) -> Option<&FragmentScope> {
    ws.fragment_scopes.get(&file)
}

/// Every name some tier of `frag` offers (spec 8.3), bound cleanly or not:
/// the module's own members, the fragment's named imports (by their local
/// names), its glob imports' members, and the std prelude. Sorted.
fn tier_names(ws: &Workspace, frag: &FragmentScope) -> Vec<Ustr> {
    let viewers = [Viewer::Fragment(frag)];
    let mut names: Vec<Ustr> =
        resolve::module_member_names(ws, &viewers, frag.package, &frag.module_path).unwrap_or_default();
    names.extend(frag.named.keys().copied());
    for glob in &frag.globs {
        if let Ok((package, path)) = resolve::resolve_path_to_module(ws, frag, glob) {
            names.extend(resolve::module_member_names(ws, &viewers, package, &path).unwrap_or_default());
        }
    }
    if let Some(root) = ws.ns.std_root() {
        names.extend(root.owned_types.keys().copied());
        names.extend(root.owned_globals.keys().copied());
        names.extend(root.owned_callables.keys().copied());
    }
    names.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    names.dedup();
    names
}

/// Every bare name the fragment's tiers bind that `admit` accepts, with the
/// definition the resolver binds it to. A name whose binding is broken (an
/// ambiguous glob, imports that do not compose) is left out; a composed
/// callable family is offered once, by its first member. Sorted by name.
pub fn bound_names(ws: &Workspace, frag: &FragmentScope, admit: impl Fn(&str) -> bool) -> Vec<(Ustr, Ustr)> {
    tier_names(ws, frag)
        .into_iter()
        .filter(|name| admit(name.as_str()))
        .filter_map(|name| bound_key(ws, frag, name).map(|key| (name, key)))
        .collect()
}

/// The definition the bare `name` binds in `frag`, if a tier binds it to one.
fn bound_key(ws: &Workspace, frag: &FragmentScope, name: Ustr) -> Option<Ustr> {
    match resolve::resolve_bare(ws, Some(frag), name) {
        Ok(r) if r.kind != ResolvedKind::Module => Some(r.key),
        Ok(_) => None,
        Err(_) => match resolve::resolve_v1_callee(ws, frag, name) {
            Ok(CalleeFamily::Merged { keys, .. }) => keys.first().copied(),
            Ok(CalleeFamily::Single(r)) => Some(r.key),
            Err(_) => None,
        },
    }
}

/// One member a qualified prefix offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Member {
    Def(Ustr),
    Module,
    Variant,
}

/// The members of what the qualified prefix `segments` names from `frag`:
/// the module the resolver walks it to -- its accessible members and child
/// modules -- or the enum it names, whose variants follow. `None` when the
/// prefix names neither.
pub fn path_members(
    ws: &Workspace,
    frag: &FragmentScope,
    from_root: bool,
    segments: &[Ustr],
) -> Option<Vec<(Ustr, Member)>> {
    let prefix = UsePathRef { from_root, segments: segments.to_vec() };
    if let Ok((package, path)) = resolve::resolve_path_to_module(ws, frag, &prefix) {
        let viewers = [Viewer::Fragment(frag)];
        let mut out = Vec::new();
        for name in resolve::module_member_names(ws, &viewers, package, &path).unwrap_or_default() {
            if let Ok(members) = resolve::module_members(ws, &viewers, package, &path, name)
                && let [(key, _)] = members.found[..]
            {
                out.push((name, Member::Def(key)));
            }
        }
        let children = ws
            .ns
            .package(package)
            .and_then(|p| p.node(p.key_of(&path)))
            .map(|node| node.children.keys().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        for child in children {
            let mut child_path = segments.to_vec();
            child_path.push(child);
            let child_path = UsePathRef { from_root, segments: child_path };
            if resolve::resolve_path_to_module(ws, frag, &child_path).is_ok() {
                out.push((child, Member::Module));
            }
        }
        out.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
        return Some(out);
    }
    // `E::` and `p::E::` name an enum exactly as a value path's
    // `Type::Variant` does (`resolve_member_path`): a one-segment prefix is
    // a bare name, a longer one a path.
    let enum_key = match segments {
        [only] if !from_root => resolve::resolve_bare(ws, Some(frag), *only).ok().map(|r| r.key),
        _ => resolve::resolve_path(ws, frag, &prefix).ok().map(|r| r.key),
    }?;
    let e = ws.enums_by_name.get(&enum_key)?;
    Some(
        ws.module(e.module).enums[e.def]
            .variants
            .iter()
            .map(|v| (v.name, Member::Variant))
            .collect(),
    )
}

/// An accessible definition no tier of `frag` binds, with the `use` path
/// that names it from `frag`.
pub struct ImportCandidate {
    pub name: Ustr,
    pub key: Ustr,
    pub path: String,
}

/// Definitions `frag` could name through a new `use` (spec 15.3:
/// "completion may suggest out-of-scope public symbols with an import
/// edit"), for names `admit` accepts. Only a name no tier binds qualifies --
/// an explicit import of a bound name would rebind every use of it -- and
/// only a definition of this package or a declared dependency, through a
/// path the resolver resolves from `frag` to that very definition, which is
/// also the access check. Sorted by name.
pub fn import_candidates(ws: &Workspace, frag: &FragmentScope, admit: impl Fn(&str) -> bool) -> Vec<ImportCandidate> {
    let mut out = Vec::new();
    let mut bound: Option<std::collections::HashSet<Ustr>> = None;
    for (key, meta) in ws.defs.iter() {
        if !matches!(meta.kind, DefKind::Type | DefKind::Global | DefKind::Callable)
            || (meta.package == frag.package && meta.module == frag.module_path)
            || !admit(meta.name.as_str())
            || bound
                .get_or_insert_with(|| tier_names(ws, frag).into_iter().collect())
                .contains(&meta.name)
        {
            continue;
        }
        let Some((from_root, segments)) = import_path(ws, frag, meta) else { continue };
        let path = UsePathRef { from_root, segments };
        if !matches!(resolve::resolve_path(ws, frag, &path), Ok(r) if r.key == *key) {
            continue;
        }
        let spelled = path.segments.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("::");
        out.push(ImportCandidate {
            name: meta.name,
            key: *key,
            path: if from_root { format!("::{spelled}") } else { spelled },
        });
    }
    out.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()).then(a.path.cmp(&b.path)));
    out
}

/// The path a `use` in `frag` names `meta`'s definition by: relative from
/// the package root, `::`-anchored from any other module of the package, or
/// through the fragment package's own alias for a dependency.
fn import_path(ws: &Workspace, frag: &FragmentScope, meta: &DefMeta) -> Option<(bool, Vec<Ustr>)> {
    let mut segments = Vec::new();
    let from_root = if meta.package == frag.package {
        !frag.module_path.is_empty()
    } else {
        let alias = ws
            .ns
            .package(frag.package)?
            .aliases
            .iter()
            .filter(|(_, target)| **target == meta.package)
            .map(|(alias, _)| *alias)
            .min_by(|a, b| a.as_str().cmp(b.as_str()))?;
        segments.push(alias);
        false
    };
    segments.extend(meta.module.iter().copied());
    segments.push(meta.name);
    Some((from_root, segments))
}
