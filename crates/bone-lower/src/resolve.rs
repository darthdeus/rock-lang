//! Modules-v1 name resolution (plan M2; spec 4.3, 8.3, 8.6).
//!
//! The one lookup authority over the namespace tree in `Workspace.ns`.
//! Legacy workspaces have an empty tree and no fragment scopes: every
//! helper here short-circuits to the bare name, which is exactly today's
//! behavior, so the v1 layer cannot move legacy resolution.
//!
//! Tier policy for a BARE name in a v1 fragment (spec 8.3):
//!   1. lexical locals — handled by `lookup_in_scope` before this runs;
//!   2. the current logical module's own definitions;
//!   3. the fragment's explicit imports;
//!   4. glob bindings visible to the fragment;
//!   5. the workspace's bare-name maps (compiler stdlib surface).
//! There is no fallthrough: the first tier that binds the name decides.
//! Losers at earlier tiers are recorded as `hidden` so a failing overload
//! match can name what it would have found (the hidden-origin diagnostic).

use bone_core::modules::{FragmentScope, UsePathRef};
use bone_core::workspace::Workspace;
use ustr::Ustr;

/// What a resolved name turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedKind {
    Type,
    Global,
    Callable,
    Decorator,
    /// A module name (usable in a longer path).
    Module,
    /// Bound by an import whose target kind the key itself decides.
    Binding,
}

/// A resolution result: the canonical key every workspace map is keyed by,
/// plus the origin tier's hidden candidates (spec 8.3 hidden-origin).
#[derive(Debug, Clone)]
pub struct Resolved {
    pub key: Ustr,
    pub kind: ResolvedKind,
    pub hidden: Vec<Ustr>,
}

/// Where a path walk landed.
enum Landing {
    /// A definition (or an unresolved binding that resolved to one).
    Def(Resolved),
    /// A module node: (package, path).
    Module(u32, Vec<Ustr>),
}

/// Resolve a bare name in a v1 fragment through the tier policy. Legacy
/// files (no fragment scope) return the bare name unchanged.
/// The first tier that binds the name decides; a bound-but-broken target
/// is a diagnostic, never a fallthrough (spec 8.3).
pub fn resolve_bare(
    ws: &Workspace,
    frag: Option<&FragmentScope>,
    name: Ustr,
) -> Result<Resolved, String> {
    let Some(frag) = frag else {
        return Ok(plain(name));
    };
    let Some(pkg) = ws.ns.package(frag.package) else {
        return Ok(plain(name));
    };
    let here = pkg.key_of(&frag.module_path);
    let glob_hits = glob_candidates(ws, frag, pkg, here, name);

    // Tier 2: the current logical module's own definitions.
    if let Some(node) = pkg.node(here) {
        if let Some(kind) = owned_kind(node, name) {
            let key = first_owned(node, name).expect("kind implies key");
            check_key_access(ws, frag, key)?;
            let hidden: Vec<Ustr> = glob_hits.iter().map(|(k, _)| *k).collect();
            return Ok(Resolved { key, kind, hidden });
        }
    }

    // Tier 3: the fragment's explicit imports. A bound name does not fall
    // through, even if the target is missing or a module.
    if let Some(target) = frag.named.get(&name) {
        let hidden: Vec<Ustr> = glob_hits.iter().map(|(k, _)| *k).collect();
        return match walk_path(ws, frag, target, WalkMode::Definition, &mut Vec::new()) {
            Ok(Landing::Def(resolved)) => {
                check_key_access(ws, frag, resolved.key)?;
                Ok(Resolved { key: resolved.key, kind: resolved.kind, hidden })
            }
            Ok(Landing::Module(..)) => {
                Err(format!("`{name}` names a module, not a definition"))
            }
            Err(msg) => Err(msg),
        };
    }

    // Tier 4: glob bindings. Ambiguous globs diagnose rather than picking
    // HashMap iteration order (spec 8.3 / 8.7).
    if !glob_hits.is_empty() {
        let mut keys: Vec<Ustr> = glob_hits.iter().map(|(k, _)| *k).collect();
        keys.sort_by_key(|k| k.as_str().to_string());
        keys.dedup();
        if keys.len() > 1 {
            return Err(format!(
                "`{name}` is ambiguous between glob imports {}",
                keys.iter().map(|k| k.as_str()).collect::<Vec<_>>().join(", ")
            ));
        }
        let (key, kind) = glob_hits[0];
        check_key_access(ws, frag, key)?;
        return Ok(Resolved { key, kind, hidden: Vec::new() });
    }

    // Tier 5: the workspace's bare-name maps (compiler stdlib surface).
    Ok(plain(name))
}

fn glob_candidates(
    ws: &Workspace,
    frag: &FragmentScope,
    pkg: &bone_core::modules::NsPackage,
    here: Ustr,
    name: Ustr,
) -> Vec<(Ustr, ResolvedKind)> {
    let mut glob_targets: Vec<UsePathRef> = frag.globs.clone();
    if let Some(node) = pkg.node(here) {
        let mut exports: Vec<_> = node.exports.iter().filter(|(_, b)| b.is_glob).collect();
        exports.sort_by_key(|(k, _)| k.as_str().to_string());
        for (_, binding) in exports {
            glob_targets.push(binding.target.clone());
        }
    }
    glob_targets.sort_by(|a, b| {
        (a.from_root, a.segments.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("::")).cmp(&(
            b.from_root,
            b.segments.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("::"),
        ))
    });
    let mut hits = Vec::new();
    for glob in &glob_targets {
        if let Ok(Landing::Module(pkg_id, path)) =
            walk_path(ws, frag, glob, WalkMode::Module, &mut Vec::new())
        {
            if let Some(target_pkg) = ws.ns.package(pkg_id) {
                let node_key = target_pkg.key_of(&path);
                if let Some(node) = target_pkg.node(node_key) {
                    if let Some(kind) = owned_kind(node, name) {
                        let key = first_owned(node, name).expect("kind implies key");
                        hits.push((key, kind));
                    }
                }
            }
        }
    }
    hits
}

fn check_key_access(ws: &Workspace, frag: &FragmentScope, key: Ustr) -> Result<(), String> {
    let Some(meta) = ws.defs.get(&key) else {
        return Ok(());
    };
    if ws.ns.can_access(
        &meta.vis,
        meta.package,
        &meta.module,
        frag.package,
        &frag.module_path,
    ) {
        Ok(())
    } else {
        Err(format!("`{}` is not accessible from this module", meta.name))
    }
}

/// Resolve an explicit path (a use target or a qualified reference) in a v1
/// fragment. Legacy callers never reach this.
pub fn resolve_path(
    ws: &Workspace,
    frag: &FragmentScope,
    path: &UsePathRef,
) -> Result<Resolved, String> {
    match walk_path(ws, frag, path, WalkMode::Definition, &mut Vec::new())? {
        Landing::Def(resolved) => Ok(resolved),
        Landing::Module(..) => Err("path names a module, not a definition".to_string()),
    }
}

/// What a member-wise value path (`p::E::V`, `p::f`, `p::Type`) resolved to.
pub enum MemberTarget {
    /// A global/constant/callable family: the DefKey.
    Global(Ustr),
    /// `path::Type::Variant` — the enum's DefKey and the variant name.
    EnumVariant { enum_key: Ustr, variant: Ustr },
    /// A bare type name (for qualified struct literals).
    Type(Ustr),
}

/// Member-wise resolution: at every node a segment may be a child module OR
/// an owned definition; `Type::Variant` consumes the type and its variant
/// (spec 4.3 — the resolver decides, not the lexer).
pub fn resolve_member_path(
    ws: &Workspace,
    frag: &FragmentScope,
    path: &UsePathRef,
) -> Result<MemberTarget, String> {
    let segments = &path.segments;
    let (pkg_id, mut cur, mut i) = resolve_anchor(ws, frag, path)?;

    while i < segments.len() {
        let seg = segments[i];
        let is_last = i == segments.len() - 1;
        if seg.as_str() == "self" {
            i += 1;
            continue;
        }
        if seg.as_str() == "super" {
            if cur.is_empty() {
                return Err("`super::` at the package root has no parent".to_string());
            }
            cur.pop();
            i += 1;
            continue;
        }

        let pkg = ws.ns.package(pkg_id).ok_or("unknown package")?;
        let node_key = pkg.key_of(&cur);
        let Some(node) = pkg.node(node_key) else {
            return Err(format!("no module `{}` in path", seg));
        };
        // Child module wins (it may shadow nothing: definitions and modules
        // share one item-name domain, so a child module and an owned name
        // cannot collide — the tree builder rejects that).
        if node.children.contains_key(&seg) {
            cur.push(seg);
            i += 1;
            continue;
        }
        if let Some(kind) = owned_kind(node, seg) {
            let key = first_owned(node, seg).expect("kind implies key");
            match kind {
                ResolvedKind::Type => {
                    if is_last {
                        check_key_access(ws, frag, key)?;
                        return Ok(MemberTarget::Type(key));
                    }
                    // `Type::Variant` — exactly one segment must remain.
                    if i + 2 == segments.len() {
                        let variant = segments[i + 1];
                        if !ws.enums_by_name.contains_key(&key) {
                            return Err(format!(
                                "`{}` is not an enum; `{}` cannot be selected from it",
                                key, variant
                            ));
                        }
                        check_key_access(ws, frag, key)?;
                        return Ok(MemberTarget::EnumVariant { enum_key: key, variant });
                    }
                    return Err(format!("`{}` cannot be traversed further", key));
                }
                _ => {
                    if !is_last {
                        return Err(format!("`{}` cannot be traversed", key));
                    }
                    check_key_access(ws, frag, key)?;
                    return Ok(MemberTarget::Global(key));
                }
            }
        }
        // Not owned here: an export binding, or unknown.
        if let Some(binding) = node.exports.get(&seg) {
            return match walk_path(ws, frag, &binding.target, WalkMode::Definition, &mut Vec::new())? {
                Landing::Def(resolved) => match resolved.kind {
                    ResolvedKind::Type => Ok(MemberTarget::Type(resolved.key)),
                    _ => Ok(MemberTarget::Global(resolved.key)),
                },
                Landing::Module(..) => Err("use binding points at a module".to_string()),
            };
        }
        return Err(format!(
            "`{}` did not resolve to any definition",
            segments
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join("::")
        ));
    }

    // Walked to a module without hitting a definition.
    Err("path names a module, not a definition".to_string())
}

/// Resolve a path whose FINAL segment is a module (glob targets, `use p;`).
pub fn resolve_path_to_module(
    ws: &Workspace,
    frag: &FragmentScope,
    path: &UsePathRef,
) -> Result<(u32, Vec<Ustr>), String> {
    match walk_path(ws, frag, path, WalkMode::Module, &mut Vec::new())? {
        Landing::Module(pkg_id, path) => Ok((pkg_id, path)),
        Landing::Def(_) => Err("expected a module in use path".to_string()),
    }
}

enum WalkMode {
    /// The last segment names a definition.
    Definition,
    /// Every segment names a module; land on the final node.
    Module,
}

/// Shared first-segment anchor for `walk_path` and `resolve_member_path`.
/// `::x` is root-anchored; `self`/`super`/package self-name/alias bind;
/// any other first segment is relative to the current module (spec 4.3).
fn resolve_anchor(
    ws: &Workspace,
    frag: &FragmentScope,
    path: &UsePathRef,
) -> Result<(u32, Vec<Ustr>, usize), String> {
    let segments = &path.segments;
    if segments.is_empty() {
        return Err("empty path".to_string());
    }
    let first = segments[0];
    if path.from_root {
        return Ok((frag.package, Vec::new(), 0));
    }
    if first.as_str() == "self" {
        return Ok((frag.package, frag.module_path.clone(), 1));
    }
    if first.as_str() == "super" {
        if frag.module_path.is_empty() {
            return Err("`super::` at the package root has no parent".to_string());
        }
        return Ok((
            frag.package,
            frag.module_path[..frag.module_path.len() - 1].to_vec(),
            1,
        ));
    }
    let pkg = ws
        .ns
        .package(frag.package)
        .ok_or_else(|| "unknown package".to_string())?;
    if first == pkg.self_name {
        return Ok((frag.package, Vec::new(), 1));
    }
    if let Some(&target) = pkg.aliases.get(&first) {
        return Ok((target, Vec::new(), 1));
    }
    Ok((frag.package, frag.module_path.clone(), 0))
}

fn walk_path(
    ws: &Workspace,
    frag: &FragmentScope,
    path: &UsePathRef,
    mode: WalkMode,
    visited: &mut Vec<(u32, Ustr)>,
) -> Result<Landing, String> {
    let segments = &path.segments;
    let (pkg_id, mut cur, rest_start) = resolve_anchor(ws, frag, path)?;

    // Traverse module-by-module up to (and for Module mode, including) the
    // last segment.
    let def_stop = match mode {
        WalkMode::Definition => segments.len() - 1,
        WalkMode::Module => segments.len(),
    };
    for seg in &segments[rest_start..def_stop] {
        if seg.as_str() == "self" {
            continue;
        }
        if seg.as_str() == "super" {
            if cur.is_empty() {
                return Err("`super::` at the package root has no parent".to_string());
            }
            cur.pop();
            continue;
        }
        cur.push(*seg);
        let pkg = ws
            .ns
            .package(pkg_id)
            .ok_or_else(|| "unknown package".to_string())?;
        let key = pkg.key_of(&cur);
        if pkg.node(key).is_none() {
            return Err(format!("no module `{}` in path", seg));
        }
    }

    if let WalkMode::Module = mode {
        let pkg = ws
            .ns
            .package(pkg_id)
            .ok_or_else(|| "unknown package".to_string())?;
        if pkg.node(pkg.key_of(&cur)).is_none() {
            return Err(format!("no module `{}` in path", last_of(segments)));
        }
        return Ok(Landing::Module(pkg_id, cur));
    }

    // The last segment names the definition: look in the node, then chase
    // export bindings (cycle-guarded; a pure cycle is an error, spec 8.6).
    let pkg = ws
        .ns
        .package(pkg_id)
        .ok_or_else(|| "unknown package".to_string())?;
    let node_key = pkg.key_of(&cur);
    let node = pkg
        .node(node_key)
        .ok_or_else(|| format!("no module `{}` in path", last_of(segments)))?;
    let name = last_of(segments);
    if let Some(kind) = owned_kind(node, name) {
        let key = first_owned(node, name).expect("kind implies key");
        check_key_access(ws, frag, key)?;
        return Ok(Landing::Def(Resolved { key, kind, hidden: Vec::new() }));
    }
    if let Some(binding) = node.exports.get(&name) {
        let visit = (pkg_id, node_key);
        if visited.contains(&visit) {
            return Err("circular `use` binding with no defining target".to_string());
        }
        visited.push(visit);
        return match walk_path(ws, frag, &binding.target, WalkMode::Definition, visited)? {
            Landing::Def(resolved) => Ok(Landing::Def(resolved)),
            Landing::Module(..) => Err("use binding points at a module".to_string()),
        };
    }
    Err(format!(
        "`{}` did not resolve to any definition",
        segments.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("::")
    ))
}

/// Resolve a pattern path `prefix::Variant` where the LAST segment is the
/// variant and the prefix lands on an enum type.
pub fn resolve_variant_pattern(
    ws: &Workspace,
    frag: &FragmentScope,
    path: &UsePathRef,
) -> Result<(Ustr, Ustr), String> {
    if path.segments.len() < 2 {
        return Err("a qualified pattern needs a type and a variant".to_string());
    }
    let variant = last_of(&path.segments);
    let prefix = UsePathRef {
        from_root: path.from_root,
        segments: path.segments[..path.segments.len() - 1].to_vec(),
    };
    match resolve_member_path(ws, frag, &prefix)? {
        MemberTarget::Type(enum_key) => {
            if !ws.enums_by_name.contains_key(&enum_key) {
                return Err(format!("`{enum_key}` is not an enum"));
            }
            Ok((enum_key, variant))
        }
        MemberTarget::EnumVariant { enum_key, variant: v } => {
            if v == variant {
                Ok((enum_key, variant))
            } else {
                Err("mismatched variant path".to_string())
            }
        }
        MemberTarget::Global(key) => Err(format!("`{key}` is not an enum type")),
    }
}

fn last_of(segments: &[Ustr]) -> Ustr {
    segments[segments.len() - 1]
}

fn plain(name: Ustr) -> Resolved {
    Resolved { key: name, kind: ResolvedKind::Binding, hidden: Vec::new() }
}

fn owned_kind(node: &bone_core::modules::NsNode, name: Ustr) -> Option<ResolvedKind> {
    if node.owned_types.contains_key(&name) {
        Some(ResolvedKind::Type)
    } else if node.owned_globals.contains_key(&name) {
        Some(ResolvedKind::Global)
    } else if node.owned_callables.contains_key(&name) {
        Some(ResolvedKind::Callable)
    } else if node.owned_decorators.contains_key(&name) {
        Some(ResolvedKind::Decorator)
    } else {
        None
    }
}

fn first_owned(node: &bone_core::modules::NsNode, name: Ustr) -> Option<Ustr> {
    node.owned_types
        .get(&name)
        .or_else(|| node.owned_globals.get(&name))
        .or_else(|| node.owned_callables.get(&name))
        .or_else(|| node.owned_decorators.get(&name))
        .copied()
}
