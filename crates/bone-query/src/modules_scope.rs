//! What a modules-v1 fragment can name, for completion. Every
//! answer comes from the resolver (`bone_lower::resolve`): the tiers decide
//! what a bare name binds, the path walks decide what a qualified prefix
//! names, and a member is offered only if the resolver would let this
//! fragment name it. Nothing here matches names against definitions itself.

use bone_core::modules::{AccessSite, DefKind, DefMeta, FragmentScope, UsePathRef};
use bone_semantic::lower::resolve::{self, CalleeFamily, ResolvedKind};
use bone_semantic::prelude::*;
use bone_semantic::semantic_passes::{FragmentContribution, ScopeItem};

/// The fragment scope of the v1 file `file`, `None` for a synthetic module.
pub fn fragment(ws: &Workspace, file: Ustr) -> Option<&FragmentScope> {
    ws.fragment_scopes.get(&file)
}

/// Whether one callable family member is visible from a v1 query fragment.
/// Query readers use the same per-member table as lowering; the family key's
/// `DefMeta::vis` is only a compatibility anchor for non-callable definitions.
pub(crate) fn callable_member_admitted(
    ws: &Workspace,
    frag: &FragmentScope,
    key: Ustr,
    mid: ModuleId,
    fid: Idx<bone_semantic::hir::Function>,
) -> bool {
    ws.callable_member_admits(
        key,
        mid,
        fid,
        AccessSite::new(Some(frag.package), &frag.module_path),
    )
}

/// Every overload of the callable spelling `name`, in module order, that
/// `frag` admits (all indexed overloads for a synthetic module).
pub(crate) fn admitted_overloads<'w>(
    ws: &'w Workspace,
    frag: Option<&'w FragmentScope>,
    name: Ustr,
) -> impl Iterator<
    Item = (
        ModuleId,
        Idx<bone_semantic::hir::Function>,
        &'w bone_semantic::hir::Function,
    ),
> + 'w {
    let keys: Vec<Ustr> = match frag {
        Some(_)
            if {
                ws.functions_by_name.contains_key(&name)
                    && ws
                        .defs
                        .get(&name)
                        .is_some_and(|meta| meta.kind == DefKind::Callable)
            } =>
        {
            vec![name]
        }
        Some(frag) => match resolve::resolve_v1_callee(ws, frag, name) {
            Ok(CalleeFamily::Single(resolved)) if resolved.kind == ResolvedKind::Callable => {
                vec![resolved.key]
            }
            Ok(CalleeFamily::Merged { members }) => {
                members.into_iter().map(|member| member.key).collect()
            }
            _ => Vec::new(),
        },
        None => vec![name],
    };
    let mut out = Vec::new();
    for m in ws.modules.iter() {
        for &key in &keys {
            let Some(overloads) = m.functions_by_name.get(&key) else {
                continue;
            };
            for &(mid, fid) in overloads {
                if frag.is_none_or(|frag| callable_member_admitted(ws, frag, key, mid, fid)) {
                    out.push((mid, fid, &ws.modules[mid.index()].functions[fid]));
                }
            }
        }
    }
    out.into_iter()
}

/// Resolve a type spelling from a query source to the key stored in workspace
/// registries. Query syntax carries source spellings while those registries
/// use canonical definition keys.
pub(crate) fn resolve_type_key(
    ws: &Workspace,
    frag: Option<&FragmentScope>,
    name: &str,
) -> Option<Ustr> {
    let resolved = if let Some(frag) = frag {
        let path = name.strip_prefix("::").unwrap_or(name);
        if path.contains("::") || name.starts_with("::") {
            resolve::resolve_path(
                ws,
                frag,
                &UsePathRef {
                    from_root: name.starts_with("::"),
                    segments: path.split("::").map(Ustr::from).collect(),
                },
            )
        } else {
            resolve::resolve_bare(ws, frag, Ustr::from(name))
        }
        .ok()?
    } else {
        let key = Ustr::from(name);
        if ws
            .defs
            .get(&key)
            .is_some_and(|meta| meta.kind == DefKind::Type)
        {
            return Some(key);
        }
        return ws.defs.iter().find_map(|(&key, meta)| {
            (meta.kind == DefKind::Type && meta.name.as_str() == name).then_some(key)
        });
    };
    let is_type = resolved.kind == ResolvedKind::Type
        || (resolved.kind == ResolvedKind::Binding
            && ws
                .defs
                .get(&resolved.key)
                .is_some_and(|meta| meta.kind == DefKind::Type));
    is_type.then_some(resolved.key)
}

/// Every name some tier of `frag` offers, bound cleanly or not:
/// the module's own members, the fragment's named imports (by their local
/// names), its glob imports' members, and the std prelude. Sorted.
fn tier_names(ws: &Workspace, frag: &FragmentScope) -> Vec<Ustr> {
    let viewers = [frag];
    // Broken export paths are source diagnostics; omit that tier while offering other names.
    let mut names: Vec<Ustr> =
        resolve::module_member_names(ws, &viewers, frag.package, &frag.module_path)
            .unwrap_or_default();
    names.extend(frag.named.keys().copied());
    for glob in &frag.globs {
        // Unresolved/inaccessible glob imports are source errors, not query failures.
        if let Ok((package, path)) = resolve::resolve_path_to_module(ws, frag, glob) {
            // A broken re-export contributes no names; other import tiers remain usable.
            names.extend(
                resolve::module_member_names(ws, &viewers, package, &path).unwrap_or_default(),
            );
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
pub(crate) fn spellings_by_key(
    ws: &Workspace,
    frag: &FragmentScope,
    kind: DefKind,
) -> std::collections::HashMap<Ustr, Vec<Ustr>> {
    let mut out: std::collections::HashMap<Ustr, Vec<Ustr>> = std::collections::HashMap::new();
    for name in tier_names(ws, frag) {
        if kind == DefKind::Callable {
            // A tier name need not resolve as a callable (or may have a diagnosed broken import).
            let Ok(family) = resolve::resolve_v1_callee(ws, frag, name) else {
                continue;
            };
            match family {
                CalleeFamily::Single(resolved) if resolved.kind == ResolvedKind::Callable => {
                    out.entry(resolved.key).or_default().push(name);
                }
                CalleeFamily::Merged { members } => {
                    for member in members {
                        out.entry(member.key).or_default().push(name);
                    }
                }
                _ => {}
            }
            continue;
        }
        // Ambiguous or broken bindings cannot supply a usable spelling.
        let Ok(resolved) = resolve::resolve_bare(ws, frag, name) else {
            continue;
        };
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
        names.sort_by(|a, b| {
            a.as_str()
                .len()
                .cmp(&b.as_str().len())
                .then_with(|| a.as_str().cmp(b.as_str()))
        });
        names.dedup();
    }
    out
}

/// A global root together with the spelling and type that a fragment can use.
/// The canonical key remains available for the cached reachability index.
#[derive(Clone, Debug)]
pub(crate) struct GlobalBinding {
    pub name: Ustr,
    pub key: Ustr,
    pub node_ty: String,
}

/// Enumerate globals that the fragment can actually bind. This is used for
/// reach roots as well as for the cached call-through argument source; walking
/// Module::global_by_name directly would leak private globals and DefKeys.
pub(crate) fn visible_globals(ws: &Workspace, frag: &FragmentScope) -> Vec<GlobalBinding> {
    let names = spellings_by_key(ws, frag, DefKind::Global);
    let mut out = Vec::new();
    for (key, spellings) in names {
        let Some(global) = ws
            .modules
            .iter()
            .find_map(|m| m.global_by_name.get(&key).map(|idx| m.globals[*idx]))
        else {
            continue;
        };
        for name in spellings {
            out.push(GlobalBinding {
                name,
                key,
                node_ty: crate::reachability::type_key(ws, global.ty).to_string(),
            });
        }
    }
    out.sort_by(|a, b| {
        a.name
            .as_str()
            .cmp(b.name.as_str())
            .then_with(|| a.key.as_str().cmp(b.key.as_str()))
    });
    out.dedup_by(|a, b| a.name == b.name && a.key == b.key);
    out
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
        let identifier_start = if let Some(rest) =
            rendered[start..].strip_prefix(bone_core::modules::STANDALONE_PACKAGE_PREFIX)
        {
            rest.find("::").map(|offset| {
                start + bone_core::modules::STANDALONE_PACKAGE_PREFIX.len() + offset + 2
            })
        } else {
            None
        }
        .unwrap_or(start);
        if identifier_start >= bytes.len()
            || !(bytes[identifier_start].is_ascii_alphabetic() || bytes[identifier_start] == b'_')
        {
            out.push(bytes[start] as char);
            start += 1;
            continue;
        }
        let mut end = identifier_start + 1;
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

pub(crate) fn display_method_suffix_with_names(
    suffix: &str,
    _final_method: bool,
    methods: &[crate::reachability::MethodRef],
    callable_names: &std::collections::HashMap<Ustr, Vec<Ustr>>,
) -> Option<String> {
    let mut rendered = suffix.to_string();
    let mut search_from = 0;
    for method in methods {
        let key = method.0;
        let label = bone_core::modules::display_name(key.as_str());
        let start = rendered.get(search_from..)?.find(label)? + search_from;
        let end = start + label.len();
        let name = callable_names.get(&method.0)?.first()?;
        rendered.replace_range(start..end, name.as_str());
        search_from = start + name.as_str().len();
    }
    Some(rendered)
}

/// Every bare name the fragment's tiers bind that `admit` accepts, with the
/// definition the resolver binds it to. A name whose binding is broken (an
/// ambiguous glob, imports that do not compose) is left out; a composed
/// callable family is offered once, by its first member. Sorted by name.
pub(crate) fn bound_names(
    ws: &Workspace,
    frag: &FragmentScope,
    admit: impl Fn(&str) -> bool,
) -> Vec<(Ustr, Ustr)> {
    tier_names(ws, frag)
        .into_iter()
        .filter(|name| admit(name.as_str()))
        .filter_map(|name| bound_key(ws, frag, name).map(|key| (name, key)))
        .collect()
}

/// The definition the bare `name` binds in `frag`, if a tier binds it to one.
fn bound_key(ws: &Workspace, frag: &FragmentScope, name: Ustr) -> Option<Ustr> {
    match resolve::resolve_bare(ws, frag, name) {
        Ok(r) if r.kind != ResolvedKind::Module => Some(r.key),
        Ok(_) => None,
        // A callable family can be composed even when bare-value resolution rejects it.
        Err(_) => match resolve::resolve_v1_callee(ws, frag, name) {
            Ok(CalleeFamily::Merged { members }) => members.first().map(|m| m.key),
            Ok(CalleeFamily::Single(r)) => Some(r.key),
            // Neither resolver binds this name; omit the unusable source binding.
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
pub(crate) fn path_members(
    ws: &Workspace,
    frag: &FragmentScope,
    from_root: bool,
    segments: &[Ustr],
) -> Option<Vec<(Ustr, Member)>> {
    let prefix = UsePathRef {
        from_root,
        segments: segments.to_vec(),
    };
    // A prefix may name an enum instead of a module; probe that below on a module miss.
    if let Ok((package, path)) = resolve::resolve_path_to_module(ws, frag, &prefix) {
        let viewers = [frag];
        let mut out = Vec::new();
        // Broken re-exports are source diagnostics; still offer accessible child modules.
        for name in resolve::module_member_names(ws, &viewers, package, &path).unwrap_or_default() {
            // Ambiguous/inaccessible/broken bindings are not usable completion candidates.
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
            // No namespace node means no child modules to offer.
            .unwrap_or_default();
        for child in children {
            let mut child_path = segments.to_vec();
            child_path.push(child);
            let child_path = UsePathRef {
                from_root,
                segments: child_path,
            };
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
        // An unresolved bare prefix offers neither enum variants nor module members.
        [only] if !from_root => resolve::resolve_bare(ws, frag, *only).ok().map(|r| r.key),
        // An unresolved qualified prefix is an ordinary completion non-match.
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

/// Definitions `frag` could name through a new `use` (completion may
/// suggest out-of-scope public symbols with an import edit), for names
/// `admit` accepts. Only a name no
/// tier binds qualifies -- an explicit import of a bound name would rebind
/// every use of it -- and only a definition of this package or of a declared
/// dependency, through a path the resolver resolves from `frag` to that very
/// definition, which is also the access check. Sorted by name, then path.
pub fn import_candidates(
    ws: &Workspace,
    frag: &FragmentScope,
    admit: impl Fn(&str) -> bool,
) -> Vec<ImportCandidate> {
    let mut out = Vec::new();
    let mut bound: Option<std::collections::HashSet<Ustr>> = None;
    let mut unbound = |name: Ustr| {
        admit(name.as_str())
            && !bound
                .get_or_insert_with(|| tier_names(ws, frag).into_iter().collect())
                .contains(&name)
    };
    let mut offer = |name: Ustr, key: Ustr, from_root: bool, segments: Vec<Ustr>| {
        let path = UsePathRef {
            from_root,
            segments,
        };
        if !matches!(resolve::resolve_path(ws, frag, &path), Ok(r) if r.key == key) {
            return;
        }
        let spelled = path
            .segments
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("::");
        out.push(ImportCandidate {
            name,
            key,
            path: if from_root {
                format!("::{spelled}")
            } else {
                spelled
            },
        });
    };
    for (key, meta) in ws.defs.iter() {
        if !matches!(
            meta.kind,
            DefKind::Type | DefKind::Global | DefKind::Callable
        ) || (meta.package == frag.package && meta.module == frag.module_path)
            || !unbound(meta.name)
        {
            continue;
        }
        let Some((from_root, segments)) = import_path(ws, frag, meta) else {
            continue;
        };
        offer(meta.name, *key, from_root, segments);
    }
    out.sort_by(|a, b| {
        a.name
            .as_str()
            .cmp(b.name.as_str())
            .then(a.path.cmp(&b.path))
    });
    out
}

/// The text and offset of a new `use <path>;` in a v1 fragment, placed to
/// keep import ordering stable and avoid repeated churn: at the start of
/// the line of the first top-level `use` item whose text sorts after it,
/// else on the line after the last `use` item, else after the `mod` header,
/// else at the top. Deterministic for a given file, so a second run inserts
/// nothing new in a new place.
pub fn use_insertion(
    contribution: &FragmentContribution,
    src: &str,
    path: &str,
) -> (usize, String) {
    let line = format!("use {path};");
    let line_start = |at: usize| src[..at.min(src.len())].rfind('\n').map_or(0, |nl| nl + 1);
    let uses: Vec<(usize, usize)> = contribution
        .items
        .iter()
        .filter_map(|item| match item {
            ScopeItem::Use(decl) => Some(decl.span.offset_range()),
            _ => None,
        })
        .collect();
    let later = uses
        .iter()
        .find(|(start, end)| src[line_start(*start)..(*end).min(src.len())] > *line.as_str());
    let at = match (
        later,
        uses.last().map(|u| u.1).or_else(|| {
            contribution
                .header
                .as_ref()
                .map(|h| h.span.offset_range().1)
        }),
    ) {
        (Some(&(start, _)), _) => line_start(start),
        (None, Some(end)) => {
            let end = end.min(src.len());
            src[end..].find('\n').map_or(src.len(), |nl| end + nl + 1)
        }
        (None, None) => 0,
    };
    let lead = if at == src.len() && !src.is_empty() && !src.ends_with('\n') {
        "\n"
    } else {
        ""
    };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn displayed_types_resolve_standalone_keys_with_file_punctuation() {
        let key = Ustr::from("$standalone$fixture-dir/types.rock::Kind");
        let spellings = std::collections::HashMap::from([(key, vec![Ustr::from("Alias")])]);
        let ws = Workspace::new();
        assert_eq!(
            display_type_with_names(&ws, &format!("Vec<{key}?>"), &spellings),
            "Vec<Alias?>"
        );
    }
}
