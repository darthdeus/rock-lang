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
pub fn resolve_bare(ws: &Workspace, frag: Option<&FragmentScope>, name: Ustr) -> Resolved {
    let Some(frag) = frag else {
        return plain(name);
    };
    let Some(pkg) = ws.ns.package(frag.package) else {
        return plain(name);
    };
    let here = pkg.key_of(&frag.module_path);
    let mut hidden = Vec::new();

    // Tier 2: the current logical module's own definitions.
    if let Some(node) = pkg.node(here) {
        if let Some(kind) = owned_kind(node, name) {
            let key = first_owned(node, name).expect("kind implies key");
            return Resolved { key, kind, hidden };
        }
    }

    // Tier 3: the fragment's explicit imports.
    if let Some(target) = frag.named.get(&name) {
        match walk_path(ws, frag, target, WalkMode::Definition, &mut Vec::new()) {
            Ok(Landing::Def(resolved)) => {
                return Resolved { key: resolved.key, kind: resolved.kind, hidden }
            }
            Ok(Landing::Module(..)) => {}
            Err(_) => {}
        }
    }

    // Tier 4: glob bindings — fragment globs, then the module's shared glob
    // exports. First glob that binds the name wins.
    let module_node = pkg.node(here);
    let mut glob_targets: Vec<UsePathRef> = frag.globs.clone();
    if let Some(node) = module_node {
        for binding in node.exports.values() {
            if binding.is_glob {
                glob_targets.push(binding.target.clone());
            }
        }
    }
    for glob in &glob_targets {
        if let Ok(Landing::Module(pkg_id, path)) =
            walk_path(ws, frag, glob, WalkMode::Module, &mut Vec::new())
        {
            if let Some(target_pkg) = ws.ns.package(pkg_id) {
                let node_key = target_pkg.key_of(&path);
                if let Some(node) = target_pkg.node(node_key) {
                    if let Some(kind) = owned_kind(node, name) {
                        let key = first_owned(node, name).expect("kind implies key");
                        return Resolved { key, kind, hidden };
                    }
                }
            }
        }
    }

    // Tier 5: the workspace's bare-name maps (compiler stdlib surface).
    plain(name)
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
    if segments.is_empty() {
        return Err("empty path".to_string());
    }

    // Resolve the anchor (root / self / super / alias / lexical).
    let first = segments[0];
    let mut pkg_id = frag.package;
    let mut cur: Vec<Ustr> = Vec::new();
    let mut i = 0usize;
    if path.from_root {
        i = 0;
    } else if first.as_str() == "self" {
        cur = frag.module_path.clone();
        i = 1;
    } else if first.as_str() == "super" {
        if frag.module_path.is_empty() {
            return Err("`super::` at the package root has no parent".to_string());
        }
        cur = frag.module_path[..frag.module_path.len() - 1].to_vec();
        i = 1;
    } else {
        let pkg = ws.ns.package(frag.package).ok_or("unknown package")?;
        if first == pkg.self_name {
            i = 1;
        } else if let Some(&target) = pkg.aliases.get(&first) {
            pkg_id = target;
            i = 1;
        }
    }

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
                        return Ok(MemberTarget::EnumVariant { enum_key: key, variant });
                    }
                    return Err(format!("`{}` cannot be traversed further", key));
                }
                _ => {
                    if !is_last {
                        return Err(format!("`{}` cannot be traversed", key));
                    }
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

fn walk_path(
    ws: &Workspace,
    frag: &FragmentScope,
    path: &UsePathRef,
    mode: WalkMode,
    visited: &mut Vec<(u32, Ustr)>,
) -> Result<Landing, String> {
    let segments = &path.segments;
    if segments.is_empty() {
        return Err("empty path".to_string());
    }
    // Determine the anchor from the first segment.
    let first = segments[0];
    let mut pkg_id = frag.package;
    let mut cur: Vec<Ustr>;
    let rest_start;
    if path.from_root {
        // `::a::b` — the anchor is this package's root; `a` is treated like a
        // root-anchored first segment (self name, alias, or root item).
        cur = Vec::new();
        rest_start = 0;
    } else if first.as_str() == "self" {
        cur = frag.module_path.clone();
        rest_start = 1;
    } else if first.as_str() == "super" {
        if frag.module_path.is_empty() {
            return Err("`super::` at the package root has no parent".to_string());
        }
        cur = frag.module_path[..frag.module_path.len() - 1].to_vec();
        rest_start = 1;
    } else {
        // Alias, self name, or a reference relative to the current module.
        let pkg = ws
            .ns
            .package(frag.package)
            .ok_or_else(|| "unknown package".to_string())?;
        if first == pkg.self_name {
            cur = Vec::new();
            rest_start = 1;
        } else if let Some(&target) = pkg.aliases.get(&first) {
            pkg_id = target;
            cur = Vec::new();
            rest_start = 1;
        } else {
            cur = frag.module_path.clone();
            rest_start = 0;
        }
    }

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
