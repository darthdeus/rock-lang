//! What a modules-v1 fragment can name, for completion. Every answer comes
//! from the resolver (`bone_lower::resolve`); nothing here matches names
//! against definitions itself.

use bone_core::modules::{AccessSite, DefKind, DefMeta, FragmentScope, NsPackage, UsePathRef};
use bone_semantic::lower::resolve::{self, CalleeFamily, ResolvedKind};
use bone_semantic::prelude::*;
use bone_semantic::semantic_passes::{FragmentContribution, ScopeItem};
use std::collections::HashMap;

pub fn fragment(ws: &Workspace, file: Ustr) -> Option<&FragmentScope> {
    ws.fragment_scopes.get(&file)
}

/// Query readers use the same per-member table as lowering; the family
/// key's `DefMeta::vis` only anchors non-callable definitions.
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

pub(crate) fn admitted_overloads<'w>(
    ws: &'w Workspace,
    frag: &'w FragmentScope,
    name: Ustr,
) -> impl Iterator<
    Item = (
        ModuleId,
        Idx<bone_semantic::hir::Function>,
        &'w bone_semantic::hir::Function,
    ),
> + 'w {
    let keys: Vec<Ustr> = if ws.functions_by_name.contains_key(&name)
        && ws
            .defs
            .get(&name)
            .is_some_and(|meta| meta.kind == DefKind::Callable)
    {
        vec![name]
    } else {
        match resolve::resolve_v1_callee(ws, frag, name) {
            Ok(CalleeFamily::Single(resolved)) if resolved.kind == ResolvedKind::Callable => {
                vec![resolved.key]
            }
            Ok(CalleeFamily::Merged { members }) => {
                members.into_iter().map(|member| member.key).collect()
            }
            _ => Vec::new(),
        }
    };
    let mut out = Vec::new();
    for m in ws.modules.iter() {
        for &key in &keys {
            let Some(overloads) = m.functions_by_name.get(&key) else {
                continue;
            };
            for &(mid, fid) in overloads {
                if callable_member_admitted(ws, frag, key, mid, fid) {
                    out.push((mid, fid, &ws.modules[mid.index()].functions[fid]));
                }
            }
        }
    }
    out.into_iter()
}

/// Query syntax carries source spellings; workspace registries use
/// canonical definition keys.
pub(crate) fn resolve_type_key(ws: &Workspace, frag: &FragmentScope, name: &str) -> Option<Ustr> {
    let path = name.strip_prefix("::").unwrap_or(name);
    let resolved = if path.contains("::") || name.starts_with("::") {
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
    .ok()?;
    let is_type = resolved.kind == ResolvedKind::Type
        || (resolved.kind == ResolvedKind::Binding
            && ws
                .defs
                .get(&resolved.key)
                .is_some_and(|meta| meta.kind == DefKind::Type));
    is_type.then_some(resolved.key)
}

pub(crate) fn resolve_type_key_in_file(ws: &Workspace, file: Ustr, name: &str) -> Option<Ustr> {
    resolve_type_key(ws, fragment(ws, file)?, name)
}

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

pub(crate) type Spellings = HashMap<Ustr, Vec<Ustr>>;

#[derive(Default)]
pub(crate) struct ResolvedNames {
    pub ordinary: Vec<(Ustr, Ustr)>,
    pub globals: Spellings,
    pub types: Spellings,
    pub callables: Spellings,
}

impl ResolvedNames {
    pub(crate) fn build(ws: &Workspace, frag: &FragmentScope) -> Self {
        let mut names = Self::default();
        for name in tier_names(ws, frag) {
            names.resolve(ws, frag, name);
        }
        for spellings in [&mut names.globals, &mut names.types, &mut names.callables] {
            sort_spellings(spellings);
        }
        names
    }

    fn resolve(&mut self, ws: &Workspace, frag: &FragmentScope, name: Ustr) {
        let bare = resolve::resolve_bare(ws, frag, name).ok();
        let ordinary = match bare {
            Some(r) if r.kind != ResolvedKind::Callable => {
                let spellings = match r.kind {
                    ResolvedKind::Global => Some(&mut self.globals),
                    ResolvedKind::Type => Some(&mut self.types),
                    _ => None,
                };
                if let Some(spellings) = spellings {
                    spellings.entry(r.key).or_default().push(name);
                }
                Some(r.key)
            }
            _ => {
                let family = resolve::resolve_v1_callee(ws, frag, name).ok();
                let first = record_callable_spellings(&mut self.callables, name, family);
                bare.map(|r| r.key).or(first)
            }
        };
        self.ordinary.extend(ordinary.map(|key| (name, key)));
    }
}

fn record_callable_spellings(
    spellings: &mut Spellings,
    name: Ustr,
    family: Option<CalleeFamily>,
) -> Option<Ustr> {
    match family? {
        CalleeFamily::Single(r) => {
            if r.kind == ResolvedKind::Callable {
                spellings.entry(r.key).or_default().push(name);
            }
            Some(r.key)
        }
        CalleeFamily::Merged { members } => {
            for member in &members {
                spellings.entry(member.key).or_default().push(name);
            }
            members.first().map(|member| member.key)
        }
    }
}

fn sort_spellings(spellings: &mut Spellings) {
    for names in spellings.values_mut() {
        names.sort_unstable_by_key(|name| (name.len(), name.as_str()));
        names.dedup();
    }
}

pub(crate) fn spellings_by_key(ws: &Workspace, frag: &FragmentScope, kind: DefKind) -> Spellings {
    let mut out = Spellings::new();
    for name in tier_names(ws, frag) {
        if kind == DefKind::Callable {
            record_callable_spellings(
                &mut out,
                name,
                resolve::resolve_v1_callee(ws, frag, name).ok(),
            );
        } else if let Ok(r) = resolve::resolve_bare(ws, frag, name)
            && matches!(
                (kind, r.kind),
                (DefKind::Type, ResolvedKind::Type) | (DefKind::Decorator, ResolvedKind::Decorator)
            )
        {
            out.entry(r.key).or_default().push(name);
        }
    }
    sort_spellings(&mut out);
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Member {
    Def(Ustr),
    Module,
}

pub(crate) fn module_members(
    ws: &Workspace,
    frag: &FragmentScope,
    from_root: bool,
    segments: &[Ustr],
) -> Option<Vec<(Ustr, Member)>> {
    let prefix = UsePathRef {
        from_root,
        segments: segments.to_vec(),
    };
    let (package, path) = resolve::resolve_path_to_module(ws, frag, &prefix).ok()?;
    let viewers = [frag];
    let mut out = Vec::new();
    // Broken re-exports are source diagnostics; still offer accessible child modules.
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
        .and_then(|p| p.node(NsPackage::path_key(&path)))
        .map(|node| node.children.keys().copied().collect::<Vec<_>>())
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
    Some(out)
}

/// An accessible definition no tier of `frag` binds, with the `use` path
/// that names it from `frag`.
pub struct ImportCandidate {
    pub name: Ustr,
    pub key: Ustr,
    pub path: String,
}

/// Definitions `frag` could name through a new `use`. Only names no tier
/// binds qualify: importing a bound name would rebind every use of it.
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

/// A new `use <path>;` placed by sorted order among top-level `use` items,
/// so a second run inserts nothing new in a new place.
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
        let key = Ustr::from(
            format!(
                "{}fixture-dir/types.rock::Kind",
                bone_core::modules::STANDALONE_PACKAGE_PREFIX
            )
            .as_str(),
        );
        let spellings = std::collections::HashMap::from([(key, vec![Ustr::from("Alias")])]);
        let ws = Workspace::new();
        assert_eq!(
            display_type_with_names(&ws, &format!("Vec<{key}?>"), &spellings),
            "Vec<Alias?>"
        );
    }
}
