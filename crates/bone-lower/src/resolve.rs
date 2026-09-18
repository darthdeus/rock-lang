//! Modules-v1 name resolution (plan M2; spec 4.3, 8.3, 8.6).
//!
//! The one lookup authority over the namespace tree in `Workspace.ns`.
//! Legacy files have no fragment scope: their bare names go through
//! `resolve_legacy_bare`, which keeps the bare key unless a compatibility
//! view names a v1 definition (spec 3.2).
//!
//! Tier policy for a BARE name in a v1 fragment (spec 8.3):
//!   1. lexical locals — handled by `lookup_in_scope` before this runs;
//!   2. the current logical module's own definitions and named export
//!      bindings;
//!   3. the fragment's explicit imports;
//!   4. glob bindings visible to the fragment, including what the globbed
//!      modules re-export;
//!   5. the workspace's bare-name maps (compiler stdlib surface).
//! There is no fallthrough: the first tier that binds the name decides.
//! Losers at earlier tiers are recorded as `hidden` so a failing overload
//! match can name what it would have found (the hidden-origin diagnostic).

use bone_core::modules::{
    legacy_name, FragmentScope, LegacyName, NsPackage, NsVisibility, ScopeProbe, UsePathRef, STD_PACKAGE_ID,
};
use bone_core::workspace::Workspace;
use ustr::Ustr;

std::thread_local! {
    /// The probes of the lookup running on this thread, when one is being
    /// recorded ([`with_probes`]). Thread-local rather than a parameter
    /// because every resolver function takes `&Workspace` and a lookup is
    /// one sequential call on one thread: the frame opens and closes around
    /// it, so no parallel work can interleave with it.
    static PROBES: std::cell::RefCell<Option<Vec<ScopeProbe>>> = const { std::cell::RefCell::new(None) };
}

/// Run `lookup` and return the namespace facts it read (plan 2.6): each
/// module member asked for, hit or miss, and each module node entered as a
/// path segment. A tier-5 lookup of the std root is not probed: the stdlib
/// is immutable for a session's lifetime.
pub fn with_probes<R>(lookup: impl FnOnce() -> R) -> (R, Vec<ScopeProbe>) {
    /// Puts the enclosing frame back even if `lookup` unwinds.
    struct Restore(Option<Option<Vec<ScopeProbe>>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            if let Some(outer) = self.0.take() {
                PROBES.with(|slot| slot.replace(outer));
            }
        }
    }
    let mut restore = Restore(Some(PROBES.with(|slot| slot.replace(Some(Vec::new())))));
    let result = lookup();
    let outer = restore.0.take().expect("the frame is restored once");
    let probes = PROBES.with(|slot| slot.replace(outer)).unwrap_or_default();
    // A nested recording's facts are the enclosing lookup's too.
    PROBES.with(|slot| {
        if let Some(outer) = slot.borrow_mut().as_mut() {
            outer.extend(probes.iter().copied());
        }
    });
    (result, probes)
}

fn probe(fact: ScopeProbe) {
    PROBES.with(|slot| {
        if let Some(probes) = slot.borrow_mut().as_mut() {
            probes.push(fact);
        }
    });
}

fn probe_member(package: u32, path: &[Ustr], name: Ustr) {
    probe(ScopeProbe::Member { package, node: NsPackage::path_key(path), name });
}

fn probe_node(package: u32, path: &[Ustr]) {
    probe(ScopeProbe::Node { package, node: NsPackage::path_key(path) });
}

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

/// Resolve a bare name in a legacy source file (spec 3.2). Legacy names keep
/// their bare keys; a name reached through the file's compatibility imports
/// or its root projection resolves to the v1 definition's canonical key.
/// When a legacy file of the compilation unit also defines the name, the
/// reference is ambiguous: neither side wins. A merged callable family
/// (`LegacyName::Family`) is one only as a CALLEE (`resolve_legacy_callee`);
/// anywhere else it names several definitions at once and is ambiguous too.
/// Every input exists before any body lowers, so a fresh build and an
/// incremental re-lower decide alike.
pub fn resolve_legacy_bare(ws: &Workspace, file: Ustr, name: Ustr) -> Result<Resolved, String> {
    match legacy_name(ws, file, name) {
        LegacyName::Bare => Ok(plain(name)),
        LegacyName::Projected(entries) => projected(name, &entries),
        LegacyName::Family(keys) => {
            // The legacy definitions take part exactly when the bare key is
            // a member; named imports compose without them.
            if keys.contains(&name) {
                let owners = ws.ns.legacy_owned.files_defining(name);
                let foreign: Vec<Ustr> = keys.into_iter().filter(|k| *k != name).collect();
                Err(legacy_collision(name, &owners, &foreign))
            } else {
                Err(legacy_candidates(name, &keys))
            }
        }
        LegacyName::Collision { owners, foreign } => Err(legacy_collision(name, &owners, &foreign)),
    }
}

/// A bare name in callee position (direct call, UFCS, safe-nav call or an
/// operator), in either dialect: a single definition, or one overload family
/// spanning several definitions, among which overload resolution picks by
/// argument types. Legacy merges legacy callables with projected or imported
/// v1 ones (spec 3.2); v1 composes explicit imports of different callables
/// (spec 8.3) and keeps the built-in operator family beside a declared or
/// imported operator (spec 8.5).
pub enum CalleeFamily {
    Single(Resolved),
    /// Every member key, deduplicated, plus the losing tiers' candidates.
    Merged { keys: Vec<Ustr>, hidden: Vec<Ustr> },
}

pub fn resolve_legacy_callee(ws: &Workspace, file: Ustr, name: Ustr) -> Result<CalleeFamily, String> {
    match legacy_name(ws, file, name) {
        LegacyName::Family(keys) => Ok(CalleeFamily::Merged { keys, hidden: Vec::new() }),
        LegacyName::Bare => Ok(CalleeFamily::Single(plain(name))),
        LegacyName::Projected(entries) => projected(name, &entries).map(CalleeFamily::Single),
        LegacyName::Collision { owners, foreign } => Err(legacy_collision(name, &owners, &foreign)),
    }
}

fn projected(name: Ustr, entries: &[bone_core::modules::LegacyProjectionEntry]) -> Result<Resolved, String> {
    if let [entry] = entries {
        return Ok(Resolved { key: entry.key, kind: legacy_resolved_kind(entry.kind), hidden: Vec::new() });
    }
    Err(legacy_candidates(name, &entries.iter().map(|e| e.key).collect::<Vec<_>>()))
}

fn legacy_candidates(name: Ustr, keys: &[Ustr]) -> String {
    let candidates = keys.iter().map(|k| k.as_str()).collect::<Vec<_>>().join(", ");
    format!("ambiguous legacy reference to `{name}`: candidates {candidates}")
}

fn legacy_collision(name: Ustr, owners: &[Ustr], foreign: &[Ustr]) -> String {
    let owners = owners.iter().map(|f| f.as_str()).collect::<Vec<_>>().join(", ");
    let foreign = foreign.iter().map(|k| k.as_str()).collect::<Vec<_>>().join(", ");
    format!("ambiguous legacy reference to `{name}`: legacy definition in {owners} and v1 definition {foreign}")
}

fn legacy_resolved_kind(kind: bone_core::modules::DefKind) -> ResolvedKind {
    match kind {
        bone_core::modules::DefKind::Type => ResolvedKind::Type,
        bone_core::modules::DefKind::Global => ResolvedKind::Global,
        bone_core::modules::DefKind::Callable => ResolvedKind::Callable,
        bone_core::modules::DefKind::Decorator => ResolvedKind::Decorator,
        bone_core::modules::DefKind::Module => ResolvedKind::Module,
    }
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
    bind_bare(ws, frag, name).map(|(resolved, _)| resolved)
}

/// A bare name in callee position in a v1 fragment (spec 8.5). A declared,
/// imported or globbed operator keeps the std prelude's operator family
/// beside it, so a module's own `operator +` does not hide built-in
/// arithmetic.
pub fn resolve_v1_callee(ws: &Workspace, frag: &FragmentScope, name: Ustr) -> Result<CalleeFamily, String> {
    let operator = bone_core::hir::operator_entry(name.as_str()).is_some_and(|(_, overloadable)| overloadable);
    let (mut keys, hidden) = match bind_bare(ws, frag, name)? {
        (resolved, Tier::Std) => return Ok(CalleeFamily::Single(resolved)),
        (resolved, Tier::Scope) if !operator => return Ok(CalleeFamily::Single(resolved)),
        (resolved, Tier::Scope) => (vec![resolved.key], resolved.hidden),
    };
    if operator {
        if let Some((key, bone_core::modules::DefKind::Callable)) = ws.ns.std_definition(name) {
            keys.push(key);
        }
    }
    keys.sort_by_key(|k| k.as_str().to_string());
    keys.dedup();
    if let [key] = keys[..] {
        return Ok(CalleeFamily::Single(Resolved { key, kind: ResolvedKind::Callable, hidden }));
    }
    Ok(CalleeFamily::Merged { keys, hidden })
}

/// Which tier bound a name: the fragment's own scope (tiers 2-4) or the std
/// prelude (tier 5).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tier {
    Scope,
    Std,
}

/// What the tiers bind a bare name to in a v1 fragment, and which tier.
fn bind_bare(ws: &Workspace, frag: &FragmentScope, name: Ustr) -> Result<(Resolved, Tier), String> {
    if ws.ns.package(frag.package).is_none() {
        return std_bare(ws, name).map(|r| (r, Tier::Std));
    }
    let viewers = [Viewer::Fragment(frag)];
    let glob_hits = glob_candidates(ws, frag, name)?;

    // Tier 2: the current logical module's own definitions and explicit
    // export bindings.
    let own = explicit_members(ws, &viewers, frag.package, &frag.module_path, name, &mut Vec::new())?;
    if let Some(own) = own {
        let not_found = || format!("`{name}` did not resolve to any definition");
        let (key, kind) = collapse(own, name, not_found, "definitions")?;
        let hidden: Vec<Ustr> = glob_hits.iter().map(|(k, _)| *k).collect();
        return Ok((Resolved { key, kind, hidden }, Tier::Scope));
    }

    // Tier 3: the fragment's explicit imports. A bound name does not fall
    // through, even if the target is missing or a module.
    if let Some(target) = frag.named.get(&name) {
        let hidden: Vec<Ustr> = glob_hits.iter().map(|(k, _)| *k).collect();
        return match walk_path(ws, frag, &[], target, WalkMode::Definition, &mut Vec::new())? {
            Landing::Def(resolved) => Ok((Resolved { key: resolved.key, kind: resolved.kind, hidden }, Tier::Scope)),
            Landing::Module(..) => Err(format!("`{name}` names a module, not a definition")),
        };
    }

    // Tier 4: glob bindings. Ambiguous globs diagnose rather than picking
    // HashMap iteration order (spec 8.3 / 8.7).
    if !glob_hits.is_empty() {
        if glob_hits.len() > 1 {
            return Err(format!(
                "`{name}` is ambiguous between glob imports {}",
                glob_hits.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(", ")
            ));
        }
        let (key, kind) = glob_hits[0];
        return Ok((Resolved { key, kind, hidden: Vec::new() }, Tier::Scope));
    }

    // Tier 5: the compiler-owned std export view. There is no legacy
    // workspace-map fall-through for v1 names.
    std_bare(ws, name).map(|r| (r, Tier::Std))
}

fn std_bare(ws: &Workspace, name: Ustr) -> Result<Resolved, String> {
    if bone_core::modules::is_intrinsic_key_in(name, &["Map", "Coroutine", "Iterator"]) {
        return Ok(Resolved {
            key: name,
            kind: ResolvedKind::Type,
            hidden: Vec::new(),
        });
    }
    let Some((key, kind)) = ws.ns.std_definition(name) else {
        return Err(format!("`{name}` did not resolve to any definition"));
    };
    let kind = match kind {
        bone_core::modules::DefKind::Type => ResolvedKind::Type,
        bone_core::modules::DefKind::Global => ResolvedKind::Global,
        bone_core::modules::DefKind::Callable => ResolvedKind::Callable,
        bone_core::modules::DefKind::Decorator => ResolvedKind::Decorator,
        bone_core::modules::DefKind::Module => ResolvedKind::Module,
    };
    Ok(Resolved { key, kind, hidden: Vec::new() })
}

/// Tier 4 candidates for `name`, deduplicated and sorted by key: what every
/// module the fragment globs offers under `name`, plus the current module's
/// own glob export bindings. A glob opens only members accessible from
/// `frag` (spec 8.1), so an inaccessible one never shadows a lower tier or
/// makes a glob ambiguous (spec 8.3); it opens the target's export bindings
/// as well as its owned names (spec 8.3 tier 4, 8.6).
fn glob_candidates(
    ws: &Workspace,
    frag: &FragmentScope,
    name: Ustr,
) -> Result<Vec<(Ustr, ResolvedKind)>, String> {
    let viewers = [Viewer::Fragment(frag)];
    let mut visiting = Vec::new();
    let mut hits =
        glob_export_members(ws, &viewers, frag.package, &frag.module_path, name, &mut visiting)?.found;
    let mut globs = frag.globs.clone();
    globs.sort_by_key(path_sort_key);
    for glob in &globs {
        // Every fragment glob was checked where it is declared
        // (`check_use_target`), so a failure here is a real diagnostic.
        if let Landing::Module(pkg_id, path) = walk_path(ws, frag, &[], glob, WalkMode::Module, &mut visiting)? {
            hits.extend(members_of(ws, &viewers, pkg_id, &path, name, &mut visiting)?.found);
        }
    }
    hits.sort_by_key(|(k, _)| k.as_str().to_string());
    hits.dedup_by_key(|(k, _)| *k);
    Ok(hits)
}

fn path_sort_key(path: &UsePathRef) -> (bool, String) {
    (path.from_root, path.segments.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("::"))
}

/// Who names a module member. A lookup through an export binding is seen by
/// every viewer along the chain: the path's author and each exporting module
/// it passed through, so a re-export never widens what it re-exports
/// (spec 8.6).
#[derive(Clone, Copy)]
pub enum Viewer<'a> {
    /// A v1 fragment (or an exporting module standing in for one).
    Fragment(&'a FragmentScope),
    /// Any consumer at all: the legacy compatibility projection, which sees
    /// only `pub` v1 definitions (spec 3.2).
    Public,
}

impl Viewer<'_> {
    fn admits(&self, ws: &Workspace, vis: &NsVisibility, owner_pkg: u32, owner_path: &[Ustr]) -> bool {
        match self {
            Viewer::Fragment(frag) => {
                ws.ns.can_access(vis, owner_pkg, owner_path, frag.package, &frag.module_path)
            }
            Viewer::Public => *vis == NsVisibility::Pub,
        }
    }

    /// A key with no recorded metadata is the stdlib or an intrinsic: public
    /// to a fragment, and never part of the legacy projection, which carries
    /// only v1 definitions.
    fn admits_key(&self, ws: &Workspace, key: Ustr) -> bool {
        match ws.defs.get(&key) {
            Some(meta) => self.admits(ws, &meta.vis, meta.package, &meta.module),
            None => matches!(self, Viewer::Fragment(_)),
        }
    }
}

fn all_admit_key(ws: &Workspace, viewers: &[Viewer], key: Ustr) -> bool {
    viewers.iter().all(|v| v.admits_key(ws, key))
}

fn all_admit(ws: &Workspace, viewers: &[Viewer], vis: &NsVisibility, pkg: u32, path: &[Ustr]) -> bool {
    viewers.iter().all(|v| v.admits(ws, vis, pkg, path))
}

/// What one module offers under one name to a chain of viewers.
#[derive(Debug, Default)]
pub struct Members {
    /// Distinct definitions the viewers may name, sorted by key.
    pub found: Vec<(Ustr, ResolvedKind)>,
    /// Some binding of the name exists that a viewer may not name.
    pub denied: bool,
    /// An export chain came back to a binding it was already following.
    pub cycle: bool,
}

impl Members {
    fn absorb(&mut self, other: Members) {
        self.found.extend(other.found);
        self.denied |= other.denied;
        self.cycle |= other.cycle;
    }

    fn finish(mut self) -> Self {
        self.found.sort_by_key(|(k, _)| k.as_str().to_string());
        self.found.dedup_by_key(|(k, _)| *k);
        self
    }
}

/// Bindings being followed, for cycle detection: (package, module key, name)
/// plus which guard pushed it. Opening a module's glob exports and following
/// a module's named export binding of the same name are different steps, so
/// they keep separate keys: a glob of `hub` that opens `facade` must still
/// follow `facade`'s own `pub use a::bump`.
type Visiting = Vec<(u32, Ustr, Ustr, Guard)>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Guard {
    ExportBinding,
    GlobOpen,
}

/// The one member lookup: what module `path` of `pkg_id` offers under `name`
/// to `viewers`. Owned definitions and named export bindings bind the name
/// explicitly and hide the module's glob export bindings; otherwise every
/// glob export contributes (spec 8.6: overlapping globs are ambiguous at use
/// unless the exporting module disambiguates). Export targets resolve from
/// the exporting module, which must itself be permitted to name them, and are
/// followed to a fixed point with canonical keys deduplicated.
pub fn module_members(
    ws: &Workspace,
    viewers: &[Viewer],
    pkg_id: u32,
    path: &[Ustr],
    name: Ustr,
) -> Result<Members, String> {
    members_of(ws, viewers, pkg_id, path, name, &mut Vec::new())
}

fn members_of(
    ws: &Workspace,
    viewers: &[Viewer],
    pkg_id: u32,
    path: &[Ustr],
    name: Ustr,
    visiting: &mut Visiting,
) -> Result<Members, String> {
    if let Some(explicit) = explicit_members(ws, viewers, pkg_id, path, name, visiting)? {
        return Ok(explicit);
    }
    glob_export_members(ws, viewers, pkg_id, path, name, visiting)
}

/// The module's explicit binding of `name` -- an owned definition or a named
/// export binding -- or `None` when the module binds `name` only through
/// globs, if at all.
fn explicit_members(
    ws: &Workspace,
    viewers: &[Viewer],
    pkg_id: u32,
    path: &[Ustr],
    name: Ustr,
    visiting: &mut Visiting,
) -> Result<Option<Members>, String> {
    probe_member(pkg_id, path, name);
    let Some(pkg) = ws.ns.package(pkg_id) else { return Ok(None) };
    let node_key = pkg.key_of(path);
    let Some(node) = pkg.node(node_key) else { return Ok(None) };
    let mut out = Members::default();
    if let Some(kind) = owned_kind(node, name) {
        let key = first_owned(node, name).expect("kind implies key");
        if all_admit_key(ws, viewers, key) {
            out.found.push((key, kind));
        } else {
            out.denied = true;
        }
        return Ok(Some(out));
    }
    let Some(binding) = node.exports.get(&name) else { return Ok(None) };
    if !all_admit(ws, viewers, &binding.vis, pkg_id, path) {
        out.denied = true;
        return Ok(Some(out));
    }
    let visit = (pkg_id, node_key, name, Guard::ExportBinding);
    if visiting.contains(&visit) {
        out.cycle = true;
        return Ok(Some(out));
    }
    visiting.push(visit);
    let exporter = exporter_scope(pkg, path);
    let followed = walk_members(ws, &exporter, viewers, &binding.target, visiting);
    visiting.pop();
    let followed = followed?;
    if followed.found.is_empty() && !followed.cycle {
        let target = binding.target.segments.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("::");
        if !followed.denied {
            return Err(format!("`{target}` did not resolve to any definition"));
        }
        // A viewer's denial is an omission; the exporter's own is a broken
        // binding (spec 8.6: the exporter must be permitted to name it).
        visiting.push(visit);
        let for_exporter = walk_members(ws, &exporter, &[], &binding.target, visiting);
        visiting.pop();
        if for_exporter?.found.is_empty() {
            return Err(format!("`{target}` is not accessible from the module that re-exports it"));
        }
    }
    out.absorb(followed);
    Ok(Some(out.finish()))
}

/// What the module's glob export bindings offer under `name`.
fn glob_export_members(
    ws: &Workspace,
    viewers: &[Viewer],
    pkg_id: u32,
    path: &[Ustr],
    name: Ustr,
    visiting: &mut Visiting,
) -> Result<Members, String> {
    probe_member(pkg_id, path, name);
    let mut out = Members::default();
    let Some(pkg) = ws.ns.package(pkg_id) else { return Ok(out) };
    let node_key = pkg.key_of(path);
    let Some(node) = pkg.node(node_key) else { return Ok(out) };
    if node.glob_exports.is_empty() {
        return Ok(out);
    }
    let exporter = exporter_scope(pkg, path);
    let mut chain = viewers.to_vec();
    chain.push(Viewer::Fragment(&exporter));
    let mut bindings: Vec<_> = node.glob_exports.iter().collect();
    bindings.sort_by_key(|b| path_sort_key(&b.target));
    for binding in bindings {
        if !all_admit(ws, viewers, &binding.vis, pkg_id, path) {
            continue;
        }
        let (target_pkg, target_path) =
            match walk_path(ws, &exporter, &[], &binding.target, WalkMode::Module, visiting)? {
                Landing::Module(target_pkg, target_path) => (target_pkg, target_path),
                Landing::Def(_) => return Err("expected a module in use path".to_string()),
            };
        let Some(target) = ws.ns.package(target_pkg) else { continue };
        // A glob chain that comes back to a module it is already opening
        // adds nothing new; that module's members are being collected.
        let visit = (target_pkg, target.key_of(&target_path), name, Guard::GlobOpen);
        if visiting.contains(&visit) {
            continue;
        }
        visiting.push(visit);
        let followed = members_of(ws, &chain, target_pkg, &target_path, name, visiting);
        visiting.pop();
        out.absorb(followed?);
    }
    Ok(out.finish())
}

/// An exporting module standing in as the site its export targets resolve
/// from.
fn exporter_scope(pkg: &bone_core::modules::NsPackage, path: &[Ustr]) -> FragmentScope {
    FragmentScope {
        package: pkg.id,
        package_name: pkg.self_name,
        module_path: path.to_vec(),
        ..Default::default()
    }
}

/// Every name module `path` offers to `viewers`, sorted: owned names, named
/// export bindings, and the members of admitted glob export targets. Callers
/// decide each name through `module_members`; this only enumerates.
pub fn module_member_names(
    ws: &Workspace,
    viewers: &[Viewer],
    pkg_id: u32,
    path: &[Ustr],
) -> Result<Vec<Ustr>, String> {
    let mut names = Vec::new();
    collect_member_names(ws, viewers, pkg_id, path, &mut names, &mut Vec::new())?;
    names.sort_by_key(|n| n.as_str().to_string());
    names.dedup();
    Ok(names)
}

fn collect_member_names(
    ws: &Workspace,
    viewers: &[Viewer],
    pkg_id: u32,
    path: &[Ustr],
    names: &mut Vec<Ustr>,
    seen: &mut Vec<(u32, Ustr)>,
) -> Result<(), String> {
    let Some(pkg) = ws.ns.package(pkg_id) else { return Ok(()) };
    let node_key = pkg.key_of(path);
    if seen.contains(&(pkg_id, node_key)) {
        return Ok(());
    }
    seen.push((pkg_id, node_key));
    let Some(node) = pkg.node(node_key) else { return Ok(()) };
    names.extend(node.owned_types.keys());
    names.extend(node.owned_globals.keys());
    names.extend(node.owned_callables.keys());
    names.extend(node.owned_decorators.keys());
    names.extend(node.exports.keys());
    let exporter = exporter_scope(pkg, path);
    for binding in &node.glob_exports {
        if !all_admit(ws, viewers, &binding.vis, pkg_id, path) {
            continue;
        }
        if let Landing::Module(target_pkg, target_path) =
            walk_path(ws, &exporter, &[], &binding.target, WalkMode::Module, &mut Vec::new())?
        {
            collect_member_names(ws, viewers, target_pkg, &target_path, names, seen)?;
        }
    }
    Ok(())
}

/// One definition out of a lookup of `name`, or the diagnostic for why there
/// is none. `not_found` renders the diagnostic for a name nothing binds.
fn collapse(
    members: Members,
    name: Ustr,
    not_found: impl Fn() -> String,
    what: &str,
) -> Result<(Ustr, ResolvedKind), String> {
    match members.found.as_slice() {
        [single] => Ok(*single),
        [] if members.cycle => Err("circular `use` binding with no defining target".to_string()),
        [] if members.denied => Err(format!("`{name}` is not accessible from this module")),
        [] => Err(not_found()),
        many => Err(format!(
            "`{name}` is ambiguous between {what} {}",
            many.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(", ")
        )),
    }
}

/// The spec 9 access predicate for a definition named from `frag`. A key
/// with no recorded metadata is the stdlib or an intrinsic, which is public.
fn key_accessible(ws: &Workspace, frag: &FragmentScope, key: Ustr) -> bool {
    Viewer::Fragment(frag).admits_key(ws, key)
}

fn check_key_access(ws: &Workspace, frag: &FragmentScope, key: Ustr) -> Result<(), String> {
    if key_accessible(ws, frag, key) {
        return Ok(());
    }
    let name = ws.defs.get(&key).map_or(key, |meta| meta.name);
    Err(format!("`{name}` is not accessible from this module"))
}

/// Spec 9 lists MODULES among the things visibility applies to, so traversing
/// a path segment is an access, exactly like naming the definition at its end.
///
/// A module item is declared in its PARENT, so its audience is measured from
/// the parent's path: an undeclared `mod x;` (or an explicit `private mod x;`)
/// is private to the declaring module's subtree. At the package root that is
/// the whole package and no consumer, which is spec 9's "private root items are
/// effectively accessible throughout the package". The package root node itself
/// has no declaring parent and is implicitly public.
///
/// Both walkers call this; neither carries its own policy. Without it a
/// consumer could write `dep::private_module::pub_item` and reach straight
/// through the closed door, because only the final segment was ever checked.
fn check_module_access(
    ws: &Workspace,
    frag: &FragmentScope,
    pkg_id: u32,
    module_path: &[Ustr],
) -> Result<(), String> {
    let Some((name, declaring)) = module_path.split_last() else {
        return Ok(());
    };
    probe_node(pkg_id, module_path);
    let Some(pkg) = ws.ns.package(pkg_id) else {
        return Ok(());
    };
    let Some(node) = pkg.node(pkg.key_of(module_path)) else {
        return Ok(());
    };
    let default_vis = NsVisibility::Private;
    let vis = node.vis.as_ref().unwrap_or(&default_vis);
    if ws.ns.can_access(vis, pkg_id, declaring, frag.package, &frag.module_path) {
        Ok(())
    } else {
        Err(format!("module `{}` is not accessible from this module", name))
    }
}

/// Resolve an explicit path (a use target or a qualified reference) in a v1
/// fragment. Legacy callers never reach this.
pub fn resolve_path(
    ws: &Workspace,
    frag: &FragmentScope,
    path: &UsePathRef,
) -> Result<Resolved, String> {
    match walk_path(ws, frag, &[], path, WalkMode::Definition, &mut Vec::new())? {
        Landing::Def(resolved) => Ok(resolved),
        Landing::Module(..) => Err("path names a module, not a definition".to_string()),
    }
}

/// Check a fragment's own `use` target where it is declared (spec 4.3: an
/// unresolved path is a diagnostic, never a silent no-op). A glob must land
/// on a module; a named import must bind a definition, an export binding or a
/// module (`use p;`, `use a::b as c;`). Ambiguity and pure export cycles stay
/// diagnostics at use, where they are decided (spec 8.3, 8.6).
pub fn check_use_target(
    ws: &Workspace,
    frag: &FragmentScope,
    path: &UsePathRef,
    glob: bool,
) -> Result<(), String> {
    check_target(ws, frag, path, glob, false)
}

/// `check_use_target` for a shared export binding (`pub use`, `private use`)
/// recorded on module `module_path`: its target resolves from the exporting
/// module, which must itself be permitted to name it (spec 8.6).
pub fn check_export_target(
    ws: &Workspace,
    package: u32,
    module_path: &[Ustr],
    path: &UsePathRef,
    glob: bool,
) -> Result<(), String> {
    let pkg = ws.ns.package(package).ok_or("unknown package")?;
    let exporter = exporter_scope(pkg, module_path);
    check_target(ws, &exporter, path, glob, true)
}

fn check_target(
    ws: &Workspace,
    frag: &FragmentScope,
    path: &UsePathRef,
    glob: bool,
    export: bool,
) -> Result<(), String> {
    if glob {
        return walk_modules(ws, frag, path, path.segments.len()).map(|_| ());
    }
    let members = walk_members(ws, frag, &[], path, &mut Vec::new());
    if matches!(&members, Ok(m) if !m.found.is_empty() || m.cycle) {
        return Ok(());
    }
    if walk_modules(ws, frag, path, path.segments.len()).is_ok() {
        return Ok(());
    }
    // Denials read as the lookup's own do: a fragment names the member, an
    // exporter the whole target it re-exports.
    let target = joined(&path.segments);
    match members? {
        m if m.denied && export => {
            Err(format!("`{target}` is not accessible from the module that re-exports it"))
        }
        m if m.denied => Err(format!("`{}` is not accessible from this module", last_of(&path.segments))),
        _ => Err(format!("`{target}` did not resolve to any definition or module")),
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
    if !path.from_root && segments.len() == 2 {
        let first = segments[0];
        if let Ok(resolved) = resolve_bare(ws, Some(frag), first) {
            if resolved.kind == ResolvedKind::Type {
                let key = resolved.key;
                let variant = segments[1];
                if ws.enums_by_name.contains_key(&key) {
                    check_key_access(ws, frag, key)?;
                    return Ok(MemberTarget::EnumVariant { enum_key: key, variant });
                }
            }
        }
    }
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

        probe_node(pkg_id, &cur);
        probe_member(pkg_id, &cur, seg);
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
            check_module_access(ws, frag, pkg_id, &cur)?;
            i += 1;
            continue;
        }
        let members = module_members(ws, &[Viewer::Fragment(frag)], pkg_id, &cur, seg)?;
        let not_found = || unresolved_value_path(ws, frag, segments);
        let (key, kind) = collapse(members, seg, not_found, "glob re-exports")?;
        if kind == ResolvedKind::Type {
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
        if !is_last {
            return Err(format!("`{}` cannot be traversed", key));
        }
        return Ok(MemberTarget::Global(key));
    }

    // Walked to a module without hitting a definition.
    Err("path names a module, not a definition".to_string())
}

/// The diagnostic for a value path nothing binds. A v1 `x::T` whose last
/// segment names a type is most likely a legacy infix ascription, which v1
/// spells `(x : T)` (spec 7); name the replacement then, and only then.
fn unresolved_value_path(ws: &Workspace, frag: &FragmentScope, segments: &[Ustr]) -> String {
    let message = format!("`{}` did not resolve to any definition", joined(segments));
    let Some((ty, prefix)) = segments.split_last() else { return message };
    let names_a_type = BUILTIN_TYPE_NAMES.contains(&ty.as_str())
        || resolve_bare(ws, Some(frag), *ty).is_ok_and(|r| r.kind == ResolvedKind::Type);
    if prefix.is_empty() || !names_a_type {
        return message;
    }
    format!(
        "{message}; `::` is a path separator in modules-v1, so a type ascription is written `({} : {ty})`",
        joined(prefix)
    )
}

/// Type names the parser reads as builtins rather than as paths.
const BUILTIN_TYPE_NAMES: &[&str] = &["any", "bool", "float", "int", "string", "uint"];

fn joined(segments: &[Ustr]) -> String {
    segments.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("::")
}

/// Resolve a path whose FINAL segment is a module (glob targets, `use p;`).
pub fn resolve_path_to_module(
    ws: &Workspace,
    frag: &FragmentScope,
    path: &UsePathRef,
) -> Result<(u32, Vec<Ustr>), String> {
    match walk_path(ws, frag, &[], path, WalkMode::Module, &mut Vec::new())? {
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
    if first.as_str() == "std" {
        return Ok((STD_PACKAGE_ID, Vec::new(), 1));
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

/// Walk `path` as written in `frag`: anchor and module segments are
/// checked from `frag` alone (spec 8.6: a consumer checks the facade path it
/// wrote, not the canonical parent path a second time), and the final
/// member is seen by `frag` and every `outer` viewer.
fn walk_path(
    ws: &Workspace,
    frag: &FragmentScope,
    outer: &[Viewer],
    path: &UsePathRef,
    mode: WalkMode,
    visiting: &mut Visiting,
) -> Result<Landing, String> {
    match mode {
        WalkMode::Module => {
            let (pkg_id, cur) = walk_modules(ws, frag, path, path.segments.len())?;
            Ok(Landing::Module(pkg_id, cur))
        }
        WalkMode::Definition => {
            let members = walk_members(ws, frag, outer, path, visiting)?;
            let not_found = || format!("`{}` did not resolve to any definition", joined(&path.segments));
            let (key, kind) = collapse(members, last_of(&path.segments), not_found, "glob re-exports")?;
            Ok(Landing::Def(Resolved { key, kind, hidden: Vec::new() }))
        }
    }
}

/// What the last segment of `path` names in the module the rest of it
/// reaches, without collapsing to one definition.
fn walk_members(
    ws: &Workspace,
    frag: &FragmentScope,
    outer: &[Viewer],
    path: &UsePathRef,
    visiting: &mut Visiting,
) -> Result<Members, String> {
    let segments = &path.segments;
    let (pkg_id, cur) = walk_modules(ws, frag, path, segments.len() - 1)?;
    let mut viewers = vec![Viewer::Fragment(frag)];
    viewers.extend_from_slice(outer);
    members_of(ws, &viewers, pkg_id, &cur, last_of(segments), visiting)
}

/// Walk the anchor and then `segments[..stop]` as modules, checking each
/// module segment's access from `frag`.
fn walk_modules(
    ws: &Workspace,
    frag: &FragmentScope,
    path: &UsePathRef,
    stop: usize,
) -> Result<(u32, Vec<Ustr>), String> {
    let segments = &path.segments;
    let (pkg_id, mut cur, rest_start) = resolve_anchor(ws, frag, path)?;
    let pkg = ws
        .ns
        .package(pkg_id)
        .ok_or_else(|| "unknown package".to_string())?;
    for seg in &segments[rest_start.min(stop)..stop] {
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
        probe_node(pkg_id, &cur);
        if pkg.node(pkg.key_of(&cur)).is_none() {
            return Err(format!("no module `{}` in path", seg));
        }
        check_module_access(ws, frag, pkg_id, &cur)?;
    }
    probe_node(pkg_id, &cur);
    if pkg.node(pkg.key_of(&cur)).is_none() {
        return Err(format!("no module `{}` in path", last_of(segments)));
    }
    Ok((pkg_id, cur))
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
