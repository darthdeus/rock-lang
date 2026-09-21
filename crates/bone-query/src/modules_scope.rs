//! What a modules-v1 fragment can name, for completion (spec 15.3). Every
//! answer comes from the resolver (`bone_lower::resolve`): the tiers decide
//! what a bare name binds, the path walks decide what a qualified prefix
//! names, and a member is offered only if the resolver would let this
//! fragment name it. Nothing here matches names against definitions itself.

use bone_core::modules::{AccessSite, DefKind, DefMeta, FragmentScope, UsePathRef};
use bone_semantic::lower::resolve::{self, CalleeFamily, ResolvedKind, Viewer};
use bone_semantic::prelude::*;
use bone_semantic::semantic_passes::{FragmentContribution, ScopeItem};

/// The fragment scope of the v1 file `file`, `None` for a legacy file.
pub fn fragment(ws: &Workspace, file: Ustr) -> Option<&FragmentScope> {
    ws.fragment_scopes.get(&file)
}

/// Whether one callable family member is visible from a v1 query fragment.
/// Query readers use the same per-member table as lowering; the family key's
/// `DefMeta::vis` is only a compatibility anchor for non-callable definitions.
pub fn callable_member_admitted(
    ws: &Workspace,
    frag: &FragmentScope,
    key: Ustr,
    mid: ModuleId,
    fid: Idx<bone_semantic::hir::Function>,
) -> bool {
    ws.callable_member_admits(key, mid, fid, AccessSite::new(Some(frag.package), &frag.module_path))
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

/// Names that a v1 fragment can write for each canonical definition of one
/// kind. Completion indexes are workspace-wide and therefore store DefKeys;
/// this map is the request-site view that turns those keys back into names the
/// fragment's resolver actually accepts. Callable families are resolved in
/// callee position so composed imports contribute every member key.
pub fn spellings_by_key(
    ws: &Workspace,
    frag: &FragmentScope,
    kind: DefKind,
) -> std::collections::HashMap<Ustr, Vec<Ustr>> {
    let mut out: std::collections::HashMap<Ustr, Vec<Ustr>> = std::collections::HashMap::new();
    for name in tier_names(ws, frag) {
        if kind == DefKind::Callable {
            let Ok(family) = resolve::resolve_v1_callee(ws, frag, name) else { continue };
            match family {
                CalleeFamily::Single(resolved) if resolved.kind == ResolvedKind::Callable => {
                    out.entry(resolved.key).or_default().push(name);
                }
                CalleeFamily::Merged { members, .. } => {
                    for member in members {
                        out.entry(member.key).or_default().push(name);
                    }
                }
                _ => {}
            }
            continue;
        }
        let Ok(resolved) = resolve::resolve_bare(ws, Some(frag), name) else { continue };
        let matches = matches!(
            (kind, resolved.kind),
            (DefKind::Type, ResolvedKind::Type)
                | (DefKind::Global, ResolvedKind::Global)
                | (DefKind::Decorator, ResolvedKind::Decorator)
        );
        if matches {
            out.entry(resolved.key).or_default().push(name);
        }
    }
    for names in out.values_mut() {
        names.sort_by(|a, b| a.as_str().len().cmp(&b.as_str().len()).then_with(|| a.as_str().cmp(b.as_str())));
        names.dedup();
    }
    out
}

/// A global root together with the spelling and type that a fragment can use.
/// The canonical key remains available for the cached reachability index.
#[derive(Clone, Debug)]
pub struct GlobalBinding {
    pub name: Ustr,
    pub key: Ustr,
    pub ty: String,
}

/// Enumerate globals that the fragment can actually bind. This is used for
/// reach roots as well as for the cached call-through argument source; walking
/// Module::global_by_name directly would leak private globals and DefKeys.
pub fn visible_globals(ws: &Workspace, frag: &FragmentScope) -> Vec<GlobalBinding> {
    let names = spellings_by_key(ws, frag, DefKind::Global);
    let mut out = Vec::new();
    for (key, spellings) in names {
        let Some(global) = ws.modules.iter().find_map(|m| {
            m.global_by_name
                .get(&key)
                .map(|idx| m.globals[*idx])
        }) else {
            continue;
        };
        for name in spellings {
            out.push(GlobalBinding {
                name,
                key,
                ty: ws.render(None, global.ty),
            });
        }
    }
    out.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()).then_with(|| a.key.as_str().cmp(b.key.as_str())));
    out.dedup_by(|a, b| a.name == b.name && a.key == b.key);
    out
}

/// Render a nominal type at a v1 query site. Primitive and generic renderings
/// are already source spellings; a DefKey nominal gets the shortest visible
/// bare spelling when one exists.
pub fn display_type(ws: &Workspace, frag: &FragmentScope, rendered: &str) -> String {
    let spellings = spellings_by_key(ws, frag, DefKind::Type);
    display_type_with_names(ws, rendered, &spellings)
}

pub(crate) fn display_type_with_names(
    ws: &Workspace,
    rendered: &str,
    spellings: &std::collections::HashMap<Ustr, Vec<Ustr>>,
) -> String {
    let mut out = String::with_capacity(rendered.len());
    let bytes = rendered.as_bytes();
    let mut start = 0;
    while start < bytes.len() {
        if !(bytes[start].is_ascii_alphabetic() || bytes[start] == b'_') {
            out.push(bytes[start] as char);
            start += 1;
            continue;
        }
        let mut end = start + 1;
        while end < bytes.len()
            && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_' || bytes[end] == b':')
        {
            end += 1;
        }
        let token = &rendered[start..end];
        let key = Ustr::from(token);
        let spelling = spellings
            .get(&key)
            .and_then(|names| names.first())
            .map(Ustr::as_str)
            .or_else(|| ws.defs.get(&key).map(|meta| meta.name.as_str()))
            .unwrap_or_else(|| token.rsplit("::").next().unwrap_or(token));
        out.push_str(spelling);
        start = end;
    }
    out
}

/// Rewrite canonical method names in a reachability suffix into names admitted
/// by frag. A missing spelling means the path cannot be written at this site,
/// so the caller should omit it rather than emit a DefKey.
pub fn display_reach_suffix(
    ws: &Workspace,
    frag: &FragmentScope,
    path: &crate::reachability::ReachPath,
) -> Option<String> {
    let callable_names = spellings_by_key(ws, frag, DefKind::Callable);
    display_method_suffix_with_names(
        path.suffix.as_str(),
        path.is_method,
        &path.method_members,
        &callable_names,
    )
}

pub(crate) fn display_method_suffix_with_names(
    suffix: &str,
    final_method: bool,
    methods: &[crate::reachability::MethodRef],
    callable_names: &std::collections::HashMap<Ustr, Vec<Ustr>>,
) -> Option<String> {
    let segments: Vec<&str> = suffix.split('.').collect();
    let mut methods = methods.iter();
    let mut rendered = Vec::with_capacity(segments.len());
    for (i, segment) in segments.iter().enumerate() {
        let is_method = segment.ends_with("()") || (final_method && i + 1 == segments.len());
        if !is_method {
            rendered.push((*segment).to_string());
            continue;
        }
        let method = methods.next()?;
        let name = callable_names.get(&method.0)?.first()?;
        let final_hop = final_method && i + 1 == segments.len();
        rendered.push(if final_hop {
            name.to_string()
        } else {
            format!("{name}()")
        });
    }
    if methods.next().is_some() {
        return None;
    }
    Some(rendered.join("."))
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
            Ok(CalleeFamily::Merged { members, .. }) => members.first().map(|m| m.key),
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
                && let [hit] = members.found[..]
            {
                out.push((name, Member::Def(hit.key)));
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
/// that names it from `frag`. `package` owns it: with the key it identifies
/// the definition, since a legacy provider's key is a bare name.
pub struct ImportCandidate {
    pub name: Ustr,
    pub key: Ustr,
    pub package: u32,
    /// The module holding it, as a `use` path prefix (empty at a package
    /// root reached relatively).
    pub module_path: Vec<Ustr>,
    pub from_root: bool,
    pub path: String,
}

/// Definitions `frag` could name through a new `use` (spec 15.3:
/// "completion may suggest out-of-scope public symbols with an import
/// edit"; `rock fix --imports`), for names `admit` accepts. Only a name no
/// tier binds qualifies -- an explicit import of a bound name would rebind
/// every use of it -- and only a definition of this package, of a declared
/// dependency, or in the view of an `allow_legacy` dependency (spec 3.2),
/// through a path the resolver resolves from `frag` to that very
/// definition, which is also the access check. Sorted by name, then path.
pub fn import_candidates(ws: &Workspace, frag: &FragmentScope, admit: impl Fn(&str) -> bool) -> Vec<ImportCandidate> {
    let mut out = Vec::new();
    let mut bound: Option<std::collections::HashSet<Ustr>> = None;
    let mut unbound = |name: Ustr| {
        admit(name.as_str())
            && !bound.get_or_insert_with(|| tier_names(ws, frag).into_iter().collect()).contains(&name)
    };
    let mut offer = |name: Ustr, key: Ustr, package: u32, from_root: bool, segments: Vec<Ustr>| {
        let path = UsePathRef { from_root, segments };
        if !matches!(resolve::resolve_path(ws, frag, &path), Ok(r) if r.key == key) {
            return;
        }
        let spelled = path.segments.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("::");
        let module_path = path.segments[..path.segments.len() - 1].to_vec();
        out.push(ImportCandidate {
            name,
            key,
            package,
            module_path,
            from_root,
            path: if from_root { format!("::{spelled}") } else { spelled },
        });
    };
    for (key, meta) in ws.defs.iter() {
        if !matches!(meta.kind, DefKind::Type | DefKind::Global | DefKind::Callable)
            || (meta.package == frag.package && meta.module == frag.module_path)
            || !unbound(meta.name)
        {
            continue;
        }
        let Some((from_root, segments)) = import_path(ws, frag, meta) else { continue };
        offer(meta.name, *key, meta.package, from_root, segments);
    }
    // A legacy provider's view: its root owns bare legacy keys with no
    // `DefMeta`, reached through the fragment package's alias for it.
    for (target, alias) in dependency_aliases(ws, frag) {
        let Some(root) = ws.ns.package(target).filter(|p| !p.legacy_files.is_empty()).and_then(|p| p.node(Ustr::from(""))) else {
            continue;
        };
        let owned = root.owned_types.iter().chain(root.owned_globals.iter()).chain(root.owned_callables.iter());
        for (&name, &key) in owned {
            if unbound(name) {
                offer(name, key, target, false, vec![alias, name]);
            }
        }
    }
    out.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()).then(a.path.cmp(&b.path)));
    out
}

/// Each package the fragment's package names as a dependency, with the
/// alphabetically first alias it uses for it.
fn dependency_aliases(ws: &Workspace, frag: &FragmentScope) -> Vec<(u32, Ustr)> {
    let Some(pkg) = ws.ns.package(frag.package) else { return Vec::new() };
    let mut by_target: std::collections::BTreeMap<u32, Ustr> = std::collections::BTreeMap::new();
    for (&alias, &target) in &pkg.aliases {
        let slot = by_target.entry(target).or_insert(alias);
        if alias.as_str() < slot.as_str() {
            *slot = alias;
        }
    }
    by_target.into_iter().collect()
}

/// The text and offset of a new `use <path>;` in a v1 fragment (spec 15.3:
/// "keep import ordering stable and avoid repeated churn"): at the start of
/// the line of the first top-level `use` item whose text sorts after it,
/// else on the line after the last `use` item, else after the `mod` header,
/// else at the top. Deterministic for a given file, so a second run inserts
/// nothing new in a new place.
pub fn use_insertion(contribution: &FragmentContribution, src: &str, path: &str) -> (usize, String) {
    let line = format!("use {path};");
    let line_start = |at: usize| src[..at.min(src.len())].rfind('\n').map_or(0, |nl| nl + 1);
    let uses: Vec<(usize, usize)> = contribution
        .items
        .iter()
        .filter_map(|item| match item {
            ScopeItem::Use(decl) => Some(decl.span.offset_range),
            _ => None,
        })
        .collect();
    let later = uses.iter().find(|(start, end)| src[line_start(*start)..(*end).min(src.len())] > *line.as_str());
    let at = match (later, uses.last().map(|u| u.1).or_else(|| contribution.header.as_ref().map(|h| h.span.offset_range.1))) {
        (Some(&(start, _)), _) => line_start(start),
        (None, Some(end)) => {
            let end = end.min(src.len());
            src[end..].find('\n').map_or(src.len(), |nl| end + nl + 1)
        }
        (None, None) => 0,
    };
    let lead = if at == src.len() && !src.is_empty() && !src.ends_with('\n') { "\n" } else { "" };
    (at, format!("{lead}{line}\n"))
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
