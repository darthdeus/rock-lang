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
//!   5. the compiler-owned std view (the standard library's own members).
//! Values shadow; callables accumulate: the first tier that binds the name
//! decides, except that in callee position a first tier that binds callables
//! opens one family with the callables of every lower tier, which overload
//! resolution ranks by fit with the tier as the tie-breaker. Lower-tier
//! non-callables neither join nor block.

use bone_core::hir::{FamilyMember, FamilyTier};
use bone_core::modules::{
    legacy_name, DefKind, FragmentScope, LegacyName, NsPackage, NsVisibility, PackageId, ScopeProbe,
    UsePathRef,
    STD_PACKAGE_ID,
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

/// A resolution result, or one definition a lookup found: the canonical key
/// every workspace map is keyed by, its kind, and for a callable of the std
/// view or a legacy provider's view, the view's package
/// ([`FamilyMember::owner`]). A legacy key is the bare name every legacy file
/// defining it shares, so the key alone does not say whose overloads a v1
/// fragment reached; two views offering one bare name are two definitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolved {
    pub key: Ustr,
    pub kind: ResolvedKind,
    pub owner: Option<PackageId>,
}

impl Resolved {
    fn member(&self, tier: FamilyTier) -> FamilyMember {
        FamilyMember { key: self.key, owner: self.owner, tier }
    }

    fn sort_key(&self) -> (String, Option<PackageId>) {
        (self.key.as_str().to_string(), self.owner)
    }
}

/// How a diagnostic names a hit: a view member by its view's package, since
/// its bare key alone does not say which.
fn hit_display(ws: &Workspace, hit: &Resolved) -> String {
    match hit.owner.and_then(|owner| ws.ns.package(owner)) {
        Some(pkg) => format!("{}::{}", pkg.self_name, hit.key),
        None if hit.owner == Some(STD_PACKAGE_ID) => format!("std::{}", hit.key),
        None => hit.key.to_string(),
    }
}

/// The owner of a callable found in module node `pkg_id`: a definition with
/// no `DefMeta` there is the std view's or a legacy provider's, keyed by its
/// bare legacy name.
fn view_owner(ws: &Workspace, pkg_id: PackageId, key: Ustr, kind: ResolvedKind) -> Option<PackageId> {
    (kind == ResolvedKind::Callable && !ws.defs.contains_key(&key)).then_some(pkg_id)
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
/// v1 ones (spec 3.2); v1 spans the lookup tiers (spec 8.3, 8.5).
pub enum CalleeFamily {
    Single(Resolved),
    /// Every member, deduplicated, with the tier that contributed it.
    Merged { members: Vec<FamilyMember> },
}

pub fn resolve_legacy_callee(ws: &Workspace, file: Ustr, name: Ustr) -> Result<CalleeFamily, String> {
    match legacy_name(ws, file, name) {
        LegacyName::Family(keys) => Ok(CalleeFamily::Merged {
            members: keys
                .into_iter()
                .map(|key| FamilyMember { key, owner: None, tier: FamilyTier::Legacy })
                .collect(),
        }),
        LegacyName::Bare => Ok(CalleeFamily::Single(plain(name))),
        LegacyName::Projected(entries) => projected(name, &entries).map(CalleeFamily::Single),
        LegacyName::Collision { owners, foreign } => Err(legacy_collision(name, &owners, &foreign)),
    }
}

fn projected(name: Ustr, entries: &[bone_core::modules::LegacyProjectionEntry]) -> Result<Resolved, String> {
    if let [entry] = entries {
        return Ok(Resolved { key: entry.key, kind: resolved_kind(entry.kind), owner: None });
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

fn resolved_kind(kind: bone_core::modules::DefKind) -> ResolvedKind {
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
/// is a diagnostic, never a fallthrough (spec 8.3). Several explicit imports
/// (or glob candidates) of one name are one family only in callee position
/// (`resolve_v1_callee`); anywhere else they are a diagnostic.
pub fn resolve_bare(
    ws: &Workspace,
    frag: Option<&FragmentScope>,
    name: Ustr,
) -> Result<Resolved, String> {
    let Some(frag) = frag else {
        return Ok(plain(name));
    };
    match bind_tiers(ws, frag, name, false)?.first {
        Bound::One(resolved, _) => Ok(resolved),
        Bound::Several(hits, via, _) => Err(several(ws, name, &hits, via)),
        Bound::Unbound => Err(unbound(name)),
    }
}

/// A bare name in callee position in a v1 fragment (spec 8.3, 8.5). When the
/// first tier that binds the name binds callables, the family is those and
/// the callables of every lower tier, each member tagged with its tier for
/// the exact-tie break; that tier's composition must be all callables. A
/// first tier that binds a non-callable wins alone. Operators are no
/// different: a module's own `operator +` and std's are one family.
pub fn resolve_v1_callee(ws: &Workspace, frag: &FragmentScope, name: Ustr) -> Result<CalleeFamily, String> {
    v1_callee(ws, frag, name)?.ok_or_else(|| unbound(name))
}

/// A unary operator's function (`-x` -> `__unm`, `!x` -> `__not`) in a v1
/// fragment: its family, or `None` when no tier binds the name. That is not
/// an error here: a scalar operand never consults the family.
pub fn resolve_v1_unary(ws: &Workspace, frag: &FragmentScope, name: Ustr) -> Result<Option<CalleeFamily>, String> {
    v1_callee(ws, frag, name)
}

fn v1_callee(ws: &Workspace, frag: &FragmentScope, name: Ustr) -> Result<Option<CalleeFamily>, String> {
    let Tiers { first, lower } = bind_tiers(ws, frag, name, true)?;
    let mut members: Vec<(Resolved, FamilyTier)> = match first {
        Bound::Unbound => return Ok(None),
        Bound::One(resolved, _) if resolved.kind != ResolvedKind::Callable => {
            return Ok(Some(CalleeFamily::Single(resolved)))
        }
        Bound::One(resolved, tier) => vec![(resolved, tier)],
        Bound::Several(hits, via, _) if hits.iter().any(|hit| hit.kind != ResolvedKind::Callable) => {
            return Err(several(ws, name, &hits, via))
        }
        Bound::Several(hits, _, tier) => hits.into_iter().map(|hit| (hit, tier)).collect(),
    };
    members.extend(lower);
    // One definition reached through several tiers is one member, at the
    // nearest of them.
    members.sort_by_key(|(hit, tier)| (hit.sort_key(), *tier));
    members.dedup_by_key(|(hit, _)| (hit.key, hit.owner));
    if let [(hit, _)] = members[..] {
        return Ok(Some(CalleeFamily::Single(hit)));
    }
    Ok(Some(CalleeFamily::Merged { members: members.iter().map(|(hit, tier)| hit.member(*tier)).collect() }))
}

/// What the tiers bind a bare name to in a v1 fragment.
enum Bound {
    One(Resolved, FamilyTier),
    /// Several distinct definitions at one tier, sorted by key, and which
    /// tier bound them.
    Several(Vec<Resolved>, Via, FamilyTier),
    /// No tier binds the name.
    Unbound,
}

/// The tier that bound several definitions under one name.
#[derive(Clone, Copy)]
enum Via {
    Imports,
    Globs,
}

/// The first tier's binding of a name, and in callee position, when that
/// binding is all callables, the callables every lower tier offers.
struct Tiers {
    first: Bound,
    lower: Vec<(Resolved, FamilyTier)>,
}

impl Tiers {
    /// Whether the next tier is consulted: until one binds the name, and
    /// past it only to accumulate callables behind a callable binding.
    fn wants_more(&self, callee: bool) -> bool {
        match &self.first {
            Bound::Unbound => true,
            Bound::One(resolved, _) => callee && resolved.kind == ResolvedKind::Callable,
            Bound::Several(hits, ..) => callee && hits.iter().all(|hit| hit.kind == ResolvedKind::Callable),
        }
    }

    fn bind(&mut self, mut hits: Vec<Resolved>, tier: FamilyTier, via: Via) {
        if let Bound::Unbound = self.first {
            self.first = match hits[..] {
                [hit] => Bound::One(hit, tier),
                _ => Bound::Several(hits, via, tier),
            };
            return;
        }
        hits.retain(|hit| hit.kind == ResolvedKind::Callable);
        self.lower.extend(hits.into_iter().map(|hit| (hit, tier)));
    }
}

fn bind_tiers(ws: &Workspace, frag: &FragmentScope, name: Ustr, callee: bool) -> Result<Tiers, String> {
    let mut tiers = Tiers { first: Bound::Unbound, lower: Vec::new() };
    if ws.ns.package(frag.package).is_none() {
        if let Some(std) = std_definition(ws, name) {
            tiers.bind(vec![std], FamilyTier::Std, Via::Imports);
        }
        return Ok(tiers);
    }
    let viewers = [Viewer::Fragment(frag)];

    // Tier 2: the current logical module's own definitions and explicit
    // export bindings.
    let own = explicit_members(ws, &viewers, frag.package, &frag.module_path, name, &mut Vec::new())?;
    if let Some(own) = own {
        let not_found = || format!("`{name}` did not resolve to any definition");
        let hit = collapse(ws, own, name, not_found, "definitions")?;
        tiers.bind(vec![hit], FamilyTier::Own, Via::Imports);
    }

    // Tier 3: the fragment's explicit imports. A bound name does not fall
    // through, even if a target is missing or a module; below a callable
    // binding, an import that lands on a module is a non-callable and adds
    // nothing.
    if tiers.wants_more(callee) {
        let below = !matches!(tiers.first, Bound::Unbound);
        if let Some(hits) = import_tier(ws, frag, name, below)? {
            tiers.bind(hits, FamilyTier::Imports, Via::Imports);
        }
    }

    // Tier 4: glob bindings. Several candidates are one family only in
    // callee position; anywhere else they are ambiguous at use, never a
    // HashMap-order pick (spec 8.3 / 8.7).
    if tiers.wants_more(callee) {
        let glob_hits = glob_candidates(ws, frag, name)?;
        if !glob_hits.is_empty() {
            tiers.bind(glob_hits, FamilyTier::Globs, Via::Globs);
        }
    }

    // Tier 5: the compiler-owned std export view. There is no legacy
    // workspace-map fall-through for v1 names.
    if tiers.wants_more(callee) {
        if let Some(std) = std_definition(ws, name) {
            tiers.bind(vec![std], FamilyTier::Std, Via::Imports);
        }
    }
    Ok(tiers)
}

/// The definitions the fragment's explicit imports bind `name` to, sorted by
/// key and deduplicated (several routes to one definition are that
/// definition once), or `None` when no import binds it. `below` is set when a
/// nearer tier already bound callables: an import landing on a module is
/// then left out instead of rejected.
fn import_tier(ws: &Workspace, frag: &FragmentScope, name: Ustr, below: bool) -> Result<Option<Vec<Resolved>>, String> {
    if let Some(targets) = frag.named.get(&name) {
        let mut hits: Vec<Resolved> = Vec::new();
        for target in targets {
            match resolve_import_target(ws, frag, name, target)? {
                Some(found) => hits.extend(found),
                None if below => {}
                None => return Err(format!("`{name}` names a module, not a definition")),
            }
        }
        hits.sort_by_key(Resolved::sort_key);
        hits.dedup();
        return Ok((!hits.is_empty()).then_some(hits));
    }

    // An unaliased `operator(-)` selector binds the binary spelling
    // (`__op_sub`) because that is the name a binary expression uses. Prefix
    // lowering asks for `__unm`, so follow that selector to the sibling unary
    // member without making a second import spelling or a second family.
    if name.as_str() == "__unm" {
        let binary = Ustr::from("__op_sub");
        if let Some(targets) = frag.named.get(&binary) {
            let mut hits = Vec::new();
            for target in targets {
                let Some(unary) = operator_family_paths(target).nth(1) else {
                    continue;
                };
                match walk_optional_definition(ws, frag, &[], &unary, &mut Vec::new()) {
                    Ok(Some(resolved)) => hits.push(resolved),
                    Ok(None) => {}
                    Err(message) => return Err(operator_family_error(&unary, message)),
                }
            }
            hits.sort_by_key(Resolved::sort_key);
            hits.dedup();
            if !hits.is_empty() {
                return Ok(Some(hits));
            }
        }
    }
    Ok(None)
}

/// `operator(-)` is recorded under the binary family name because binary
/// expressions use `__op_sub`. Its sibling prefix family is part of the same
/// selector, including when the selector has an alias.
fn operator_family_paths(path: &UsePathRef) -> impl Iterator<Item = UsePathRef> {
    let mut paths = vec![path.clone()];
    if path.segments.last().copied() == Some(Ustr::from("__op_sub")) {
        let mut unary = path.clone();
        if let Some(last) = unary.segments.last_mut() {
            *last = Ustr::from("__unm");
        }
        paths.push(unary);
    }
    paths.into_iter()
}

/// What one explicit import of `name` binds: its definitions, or `None` when
/// the target is a module.
fn resolve_import_target(
    ws: &Workspace,
    frag: &FragmentScope,
    name: Ustr,
    path: &UsePathRef,
) -> Result<Option<Vec<Resolved>>, String> {
    let candidates = operator_family_paths(path).collect::<Vec<_>>();
    let mut hits = Vec::new();
    if path.segments.last().copied() == Some(Ustr::from("__op_sub")) {
        for candidate in &candidates {
            match walk_optional_definition(ws, frag, &[], candidate, &mut Vec::new()) {
                Ok(Some(resolved)) => hits.push(resolved),
                Ok(None) => {}
                Err(message) => return Err(operator_family_error(candidate, message)),
            }
        }
    } else {
        match walk_path(ws, frag, &[], path, WalkMode::Definition, &mut Vec::new())? {
            Landing::Def(r) => hits.push(r),
            Landing::Module(..) => return Ok(None),
        }
    }
    if hits.is_empty() {
        Err(format!("`{name}` did not resolve to any definition"))
    } else {
        Ok(Some(hits))
    }
}

fn operator_family_error(path: &UsePathRef, message: String) -> String {
    let Some(symbol) = path.segments.last().and_then(|name| match name.as_str() {
        "__unm" => Some("-"),
        "__not" => Some("!"),
        _ => None,
    }) else {
        return message;
    };
    let internal = path.segments.last().expect("path has a last segment").as_str();
    let quoted = format!("`{internal}`");
    let operator = format!("operator `{symbol}`");
    if message.contains(&quoted) {
        message.replace(&quoted, &operator)
    } else {
        message.replace(internal, symbol)
    }
}

fn unbound(name: Ustr) -> String {
    format!("`{name}` did not resolve to any definition")
}

/// Several imports or globs of `name` where one definition is wanted: a
/// composed callable family used as a value, or definitions that cannot
/// compose (spec 8.3).
fn several(ws: &Workspace, name: Ustr, hits: &[Resolved], via: Via) -> String {
    let keys = hits.iter().map(|hit| hit_display(ws, hit)).collect::<Vec<_>>().join(", ");
    let callables = hits.iter().all(|hit| hit.kind == ResolvedKind::Callable);
    match (via, callables) {
        (Via::Imports, true) => {
            format!("`{name}` names an overload family of several imports ({keys}), not a single definition")
        }
        (Via::Imports, false) => {
            format!("`{name}` is imported from several definitions that do not form one overload family: {keys}")
        }
        (Via::Globs, true) => {
            format!("`{name}` names an overload family of several glob imports ({keys}), not a single definition")
        }
        (Via::Globs, false) => format!("`{name}` is ambiguous between glob imports {keys}"),
    }
}

/// A member of the std view. A std callable's key is the bare name legacy
/// files share, so it is owned by the std view: only the standard library's
/// own overloads belong to it (spec 5).
fn std_definition(ws: &Workspace, name: Ustr) -> Option<Resolved> {
    if bone_core::modules::is_intrinsic_key_in(name, &["Map", "Coroutine", "Iterator"]) {
        return Some(Resolved { key: name, kind: ResolvedKind::Type, owner: None });
    }
    let (key, kind) = ws.ns.std_definition(name)?;
    let kind = resolved_kind(kind);
    Some(Resolved { key, kind, owner: view_owner(ws, STD_PACKAGE_ID, key, kind) })
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
) -> Result<Vec<Resolved>, String> {
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
    hits.sort_by_key(Resolved::sort_key);
    hits.dedup();
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
            Some(meta) if meta.kind == DefKind::Callable => ws
                .callable_family_members(key)
                .map(|members| {
                    members
                        .iter()
                        .any(|member| self.admits(ws, &member.vis, meta.package, &meta.module))
                })
                .unwrap_or_else(|| self.admits(ws, &meta.vis, meta.package, &meta.module)),
            Some(meta) => self.admits(ws, &meta.vis, meta.package, &meta.module),
            None => matches!(self, Viewer::Fragment(_)),
        }
    }
}

fn all_admit_key(ws: &Workspace, viewers: &[Viewer], key: Ustr) -> bool {
    if let Some(meta) = ws.defs.get(&key).filter(|meta| meta.kind == DefKind::Callable) {
        if let Some(members) = ws.callable_family_members(key) {
            // A re-export chain has several viewers. One member must survive
            // every viewer; combining each viewer's independent `any` would
            // incorrectly join two different overloads into one family.
            return members.iter().any(|member| {
                viewers
                    .iter()
                    .all(|viewer| viewer.admits(ws, &member.vis, meta.package, &meta.module))
            });
        }
    }
    viewers.iter().all(|v| v.admits_key(ws, key))
}

fn all_admit(ws: &Workspace, viewers: &[Viewer], vis: &NsVisibility, pkg: u32, path: &[Ustr]) -> bool {
    viewers.iter().all(|v| v.admits(ws, vis, pkg, path))
}

/// What one module offers under one name to a chain of viewers.
#[derive(Debug, Default)]
pub struct Members {
    /// Distinct definitions the viewers may name, sorted by key.
    pub found: Vec<Resolved>,
    /// Some binding of the name exists that a viewer may not name.
    pub denied: bool,
    /// An export chain came back to a binding it was already following.
    pub cycle: bool,
    /// A named export binding binds the name to a module (spec 8.6): a path
    /// segment, not a definition.
    pub module: bool,
}

impl Members {
    fn absorb(&mut self, other: Members) {
        self.found.extend(other.found);
        self.denied |= other.denied;
        self.cycle |= other.cycle;
        self.module |= other.module;
    }

    fn finish(mut self) -> Self {
        self.found.sort_by_key(Resolved::sort_key);
        self.found.dedup();
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
    /// A named export binding followed as a module segment.
    ModuleExport,
    /// A fragment import followed as a path anchor, keyed by the
    /// fragment's module: imports resolve from one fragment per lookup.
    Import,
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
            out.found.push(Resolved { key, kind, owner: view_owner(ws, pkg_id, key, kind) });
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
            visiting.push(visit);
            let module = walk_module_path(ws, &exporter, &binding.target, binding.target.segments.len(), visiting);
            visiting.pop();
            if module?.is_some() {
                out.module = true;
                return Ok(Some(out));
            }
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
    ws: &Workspace,
    members: Members,
    name: Ustr,
    not_found: impl Fn() -> String,
    what: &str,
) -> Result<Resolved, String> {
    match members.found.as_slice() {
        [single] => Ok(*single),
        [] if members.cycle => Err("circular `use` binding with no defining target".to_string()),
        [] if members.denied => Err(format!("`{name}` is not accessible from this module")),
        [] if members.module => Err(format!("`{name}` names a module, not a definition")),
        [] => Err(not_found()),
        many => Err(format!(
            "`{name}` is ambiguous between {what} {}",
            many.iter().map(|hit| hit_display(ws, hit)).collect::<Vec<_>>().join(", ")
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
    check_operator_selector_target(ws, frag, path, glob, false)
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
    check_operator_selector_target(ws, &exporter, path, glob, true)
}

fn check_operator_selector_target(
    ws: &Workspace,
    frag: &FragmentScope,
    path: &UsePathRef,
    glob: bool,
    export: bool,
) -> Result<(), String> {
    let result = check_target(ws, frag, path, glob, export);
    if result.is_ok() || glob || path.segments.last().copied() != Some(Ustr::from("__op_sub")) {
        return result;
    }
    let mut unary = path.clone();
    if let Some(last) = unary.segments.last_mut() {
        *last = Ustr::from("__unm");
    }
    if check_target(ws, frag, &unary, false, export).is_ok() {
        Ok(())
    } else {
        result
    }
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
    if matches!(&members, Ok(m) if !m.found.is_empty() || m.cycle || m.module) {
        return Ok(());
    }
    let module = walk_module_path(ws, frag, path, path.segments.len(), &mut Vec::new());
    if matches!(module, Ok(Some(_))) {
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
        // The last segment is a module this site may not enter.
        _ => {
            module?;
            Err(format!("`{target}` did not resolve to any definition or module"))
        }
    }
}

/// Check the name a named `use` leaf binds (spec 4.3, 8.1). `self`, `super`
/// and `std` are reserved path roots. A binding that lands on a module is a
/// path anchor for `site` (a fragment, or the exporting module for a shared
/// binding), so it may not take the name of a child module of `site`'s
/// module, a dependency alias or the package's own name unless it is that
/// same module: two anchors under one name are never picked between.
pub fn check_use_binding(ws: &Workspace, site: &FragmentScope, local: Ustr, path: &UsePathRef) -> Result<(), String> {
    if matches!(local.as_str(), "self" | "super" | "std") {
        return Err(format!("`{local}` is a reserved path root and cannot name an import"));
    }
    let Some(landing) = walk_module_path(ws, site, path, path.segments.len(), &mut Vec::new())? else {
        return Ok(());
    };
    let pkg = ws.ns.package(site.package).ok_or("unknown package")?;
    let mut child = site.module_path.clone();
    child.push(local);
    let child_module = pkg.node(pkg.key_of(&site.module_path)).is_some_and(|node| node.children.contains_key(&local));
    let clash = if child_module && landing != (site.package, child) {
        Some("a child module of this module")
    } else if pkg.aliases.get(&local).is_some_and(|&target| landing != (target, Vec::new())) {
        Some("a dependency alias")
    } else if local == pkg.self_name && landing != (site.package, Vec::new()) {
        Some("this package's name")
    } else {
        None
    };
    match clash {
        Some(what) => Err(format!(
            "`use` binds `{local}` to module `{}`, but `{local}` is also {what}",
            module_display(ws, &landing)
        )),
        None => Ok(()),
    }
}

fn module_display(ws: &Workspace, (package, path): &(u32, Vec<Ustr>)) -> String {
    let root = ws.ns.package(*package).map_or_else(|| package.to_string(), |pkg| pkg.self_name.to_string());
    std::iter::once(root).chain(path.iter().map(|s| s.to_string())).collect::<Vec<_>>().join("::")
}

/// What a member-wise value path (`p::E::V`, `p::f`, `p::Type`) resolved to.
pub enum MemberTarget {
    /// A global or constant: the DefKey.
    Global(Ustr),
    /// A callable family: the key the family is registered under and the
    /// view that owns a legacy key ([`Resolved::owner`]). A std family has no
    /// `DefMeta`, so the kind has to come from the member lookup rather than
    /// `ws.defs`.
    Callable { key: Ustr, owner: Option<PackageId> },
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
    let mut visiting = Vec::new();
    let (mut pkg_id, mut cur, mut i) = resolve_anchor(ws, frag, path, &mut visiting)?;

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
        let pkg = ws.ns.package(pkg_id).ok_or("unknown package")?;
        if pkg.node(pkg.key_of(&cur)).is_none() {
            return Err(format!("no module `{}` in path", seg));
        }
        // A module segment wins (it may shadow nothing: definitions and
        // modules share one item-name domain, so a child module and an owned
        // name cannot collide — the tree builder rejects that).
        if let Some((next_pkg, next)) = step_module(ws, frag, pkg_id, &cur, seg, &mut visiting)? {
            pkg_id = next_pkg;
            cur = next;
            i += 1;
            continue;
        }
        let members = module_members(ws, &[Viewer::Fragment(frag)], pkg_id, &cur, seg)?;
        let not_found = || unresolved_value_path(ws, frag, segments);
        let Resolved { key, kind, owner } = collapse(ws, members, seg, not_found, "glob re-exports")?;
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
        if kind == ResolvedKind::Callable {
            return Ok(MemberTarget::Callable { key, owner });
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
/// `::x` is root-anchored; `self`/`super`/`std`/package self-name/alias
/// bind; a first segment the current module binds as a module (a child or
/// a named export binding, spec 8.3 tier 2) is relative to it; otherwise a
/// fragment import that lands on a module anchors the path there (tier 3,
/// spec 8.1); anything else is relative to the current module (spec 4.3).
fn resolve_anchor(
    ws: &Workspace,
    frag: &FragmentScope,
    path: &UsePathRef,
    visiting: &mut Visiting,
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
    let relative = (frag.package, frag.module_path.clone(), 0);
    probe_member(frag.package, &frag.module_path, first);
    let own = pkg.node(pkg.key_of(&frag.module_path));
    if own.is_some_and(|node| node.children.contains_key(&first) || node.exports.contains_key(&first)) {
        return Ok(relative);
    }
    if let Some((package, module)) = imported_module(ws, frag, first, visiting)? {
        return Ok((package, module, 1));
    }
    Ok(relative)
}

/// The module a fragment's explicit import of `name` lands on (spec 8.1:
/// `use p::m;`, `use p::m as n;`, `use p::m::{self}`), if it lands on one.
/// Each import path resolves from the fragment like any path it writes; an
/// import being followed is skipped while its own path resolves, so
/// `use inner;` names the child `inner` and an alias cycle names nothing.
fn imported_module(
    ws: &Workspace,
    frag: &FragmentScope,
    name: Ustr,
    visiting: &mut Visiting,
) -> Result<Option<(u32, Vec<Ustr>)>, String> {
    let Some(targets) = frag.named.get(&name) else { return Ok(None) };
    let visit = (frag.package, NsPackage::path_key(&frag.module_path), name, Guard::Import);
    if visiting.contains(&visit) {
        return Ok(None);
    }
    visiting.push(visit);
    let mut modules = Vec::new();
    let mut failure = None;
    for target in targets {
        match walk_module_path(ws, frag, target, target.segments.len(), visiting) {
            Ok(Some(module)) => modules.push(module),
            Ok(None) => {}
            Err(message) => {
                failure = Some(message);
                break;
            }
        }
    }
    visiting.pop();
    if let Some(message) = failure {
        return Err(message);
    }
    modules.sort();
    modules.dedup();
    match modules.len() {
        0 => Ok(None),
        1 => Ok(modules.pop()),
        _ => Err(format!(
            "`{name}` is imported from several modules: {}",
            modules.iter().map(|m| module_display(ws, m)).collect::<Vec<_>>().join(", ")
        )),
    }
}

/// Where the module segment `seg` of module `cur` leads: a child module, or
/// a named export binding of a module (spec 8.6: a re-export is another path
/// to the same module). `None` when `seg` names no module there. A child is
/// an access to that module (spec 9); an export binding is checked as the
/// facade path `frag` wrote -- the binding's own visibility -- while its
/// target resolves from the exporting module, which must be permitted to
/// name it.
fn step_module(
    ws: &Workspace,
    frag: &FragmentScope,
    pkg_id: u32,
    cur: &[Ustr],
    seg: Ustr,
    visiting: &mut Visiting,
) -> Result<Option<(u32, Vec<Ustr>)>, String> {
    probe_member(pkg_id, cur, seg);
    let Some(pkg) = ws.ns.package(pkg_id) else { return Ok(None) };
    let node_key = pkg.key_of(cur);
    let Some(node) = pkg.node(node_key) else { return Ok(None) };
    if node.children.contains_key(&seg) {
        let mut next = cur.to_vec();
        next.push(seg);
        probe_node(pkg_id, &next);
        if pkg.node(pkg.key_of(&next)).is_none() {
            return Err(format!("no module `{}` in path", seg));
        }
        check_module_access(ws, frag, pkg_id, &next)?;
        return Ok(Some((pkg_id, next)));
    }
    let Some(binding) = node.exports.get(&seg) else { return Ok(None) };
    let visit = (pkg_id, node_key, seg, Guard::ModuleExport);
    if visiting.contains(&visit) {
        return Err("circular `use` binding with no defining target".to_string());
    }
    visiting.push(visit);
    let exporter = exporter_scope(pkg, cur);
    let target = walk_module_path(ws, &exporter, &binding.target, binding.target.segments.len(), visiting);
    visiting.pop();
    let Some(target) = target? else { return Ok(None) };
    if !Viewer::Fragment(frag).admits(ws, &binding.vis, pkg_id, cur) {
        return Err(format!("module `{}` is not accessible from this module", seg));
    }
    Ok(Some(target))
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
            let hit = collapse(ws, members, last_of(&path.segments), not_found, "glob re-exports")?;
            Ok(Landing::Def(hit))
        }
    }
}

/// Resolve a definition path while keeping a missing final member distinct
/// from access and path errors. Operator-family selectors may omit the unary
/// sibling, but must surface every other failure.
fn walk_optional_definition(
    ws: &Workspace,
    frag: &FragmentScope,
    outer: &[Viewer],
    path: &UsePathRef,
    visiting: &mut Visiting,
) -> Result<Option<Resolved>, String> {
    let members = walk_members(ws, frag, outer, path, visiting)?;
    let name = last_of(&path.segments);
    match members.found.as_slice() {
        [single] => Ok(Some(*single)),
        [] if members.cycle => Err("circular `use` binding with no defining target".to_string()),
        [] if members.denied => Err(format!("`{name}` is not accessible from this module")),
        [] if members.module => Err(format!("`{name}` names a module, not a definition")),
        [] => Ok(None),
        many => Err(format!(
            "`{name}` is ambiguous between glob re-exports {}",
            many.iter().map(|hit| hit_display(ws, hit)).collect::<Vec<_>>().join(", ")
        )),
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
    let (pkg_id, cur) = walk_module_path(ws, frag, path, segments.len() - 1, visiting)?
        .ok_or_else(|| format!("no module `{}` in path", segments[segments.len() - 2]))?;
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
    walk_module_path(ws, frag, path, stop, &mut Vec::new())?
        .ok_or_else(|| format!("no module `{}` in path", path.segments[stop - 1]))
}

/// [`walk_modules`], answering `Ok(None)` when every segment before the
/// last walked reaches a module but the last, `segments[stop - 1]`, names
/// none there (it may name a definition).
fn walk_module_path(
    ws: &Workspace,
    frag: &FragmentScope,
    path: &UsePathRef,
    stop: usize,
    visiting: &mut Visiting,
) -> Result<Option<(u32, Vec<Ustr>)>, String> {
    let segments = &path.segments;
    let (mut pkg_id, mut cur, rest_start) = resolve_anchor(ws, frag, path, visiting)?;
    let start = rest_start.min(stop);
    for (i, seg) in segments[start..stop].iter().enumerate() {
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
        match step_module(ws, frag, pkg_id, &cur, *seg, visiting)? {
            Some((next_pkg, next)) => {
                pkg_id = next_pkg;
                cur = next;
            }
            None if start + i + 1 == stop => return Ok(None),
            None => return Err(format!("no module `{}` in path", seg)),
        }
    }
    probe_node(pkg_id, &cur);
    let pkg = ws
        .ns
        .package(pkg_id)
        .ok_or_else(|| "unknown package".to_string())?;
    if pkg.node(pkg.key_of(&cur)).is_none() {
        return Err(format!("no module `{}` in path", last_of(segments)));
    }
    Ok(Some((pkg_id, cur)))
}

/// Resolve a pattern path `prefix::Variant` where the LAST segment is the
/// variant and the prefix lands on an enum type. The whole path resolves
/// exactly like the value `prefix::Variant`, so a bare enum prefix binds
/// through every scope tier (imports, globs, std), not only own members.
pub fn resolve_variant_pattern(
    ws: &Workspace,
    frag: &FragmentScope,
    path: &UsePathRef,
) -> Result<(Ustr, Ustr), String> {
    if path.segments.len() < 2 {
        return Err("a qualified pattern needs a type and a variant".to_string());
    }
    match resolve_member_path(ws, frag, path)? {
        MemberTarget::EnumVariant { enum_key, variant } => Ok((enum_key, variant)),
        MemberTarget::Type(key) => Err(format!("`{key}` names a type, not an enum variant")),
        MemberTarget::Global(key) | MemberTarget::Callable { key, .. } => {
            Err(format!("`{key}` is not an enum variant"))
        }
    }
}

fn last_of(segments: &[Ustr]) -> Ustr {
    segments[segments.len() - 1]
}

fn plain(name: Ustr) -> Resolved {
    Resolved { key: name, kind: ResolvedKind::Binding, owner: None }
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
