//! Resolve module definitions, explicit imports, globs, and standard-library
//! names in that order. A callable first tier admits lower-tier overloads.

use bone_core::hir::{FamilyMember, FamilyTier, unary_operator_entry};
use bone_core::modules::{
    DefKind, FragmentScope, NsPackage, NsVisibility, STD_PACKAGE_ID, ScopeProbe, UsePathRef,
};
use bone_core::workspace::Workspace;
use ustr::Ustr;

std::thread_local! {
    static PROBES: std::cell::RefCell<Option<Vec<ScopeProbe>>> = const { std::cell::RefCell::new(None) };
}

/// Run a lookup and return the namespace facts it read. The immutable std root
/// is not recorded.
pub fn with_probes<R>(lookup: impl FnOnce() -> R) -> (R, Vec<ScopeProbe>) {
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

pub(crate) fn namespace_key(ws: &Workspace, package: u32, path: &[Ustr]) -> Ustr {
    match path {
        [] => bone_core::modules::root_key(),
        [key] => *key,
        _ => {
            let mut key = path[0];
            if let Some(pkg) = ws.ns.package(package) {
                for segment in &path[1..] {
                    match pkg.node(key).and_then(|node| node.children.get(segment)) {
                        Some(child) => key = *child,
                        // Missing paths still need canonical keys for negative probes.
                        None => return NsPackage::path_key(path),
                    }
                }
                return key;
            }
            NsPackage::path_key(path)
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct ModuleKey(u32, Ustr);

impl ModuleKey {
    fn root(package: u32) -> Self {
        Self(package, bone_core::modules::root_key())
    }

    fn at(ws: &Workspace, package: u32, path: &[Ustr]) -> Self {
        Self(package, namespace_key(ws, package, path))
    }

    fn parent(self) -> Result<Self, String> {
        if self.1.is_empty() {
            return Err("`super::` at the package root has no parent".to_string());
        }
        Ok(Self(
            self.0,
            self.1
                .rsplit_once("::")
                .map_or_else(bone_core::modules::root_key, |(parent, _)| {
                    Ustr::from(parent)
                }),
        ))
    }

    fn member_probe(self, name: Ustr) {
        let Self(package, node) = self;
        probe(ScopeProbe::Member {
            package,
            node,
            name,
        });
    }

    fn node_probe(self) {
        let Self(package, node) = self;
        probe(ScopeProbe::Node { package, node });
    }

    fn node(self, ws: &Workspace) -> Option<&bone_core::modules::NsNode> {
        ws.ns.package(self.0)?.node(self.1)
    }

    fn require(self, ws: &Workspace, segment: Ustr) -> Result<&bone_core::modules::NsNode, String> {
        self.node_probe();
        ws.ns
            .package(self.0)
            .ok_or("unknown package")?
            .node(self.1)
            .ok_or_else(|| format!("no module `{segment}` in path"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedKind {
    Type,
    Global,
    Callable,
    Decorator,
    Binding,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolved {
    pub key: Ustr,
    pub kind: ResolvedKind,
}

impl Resolved {
    fn member(&self, tier: FamilyTier) -> FamilyMember {
        FamilyMember {
            key: self.key,
            tier,
        }
    }
}

fn hit_display(ws: &Workspace, hit: &Resolved) -> String {
    if hit.kind == ResolvedKind::Callable && !ws.defs.contains_key(&hit.key) {
        format!("std::{}", hit.key)
    } else {
        hit.key.to_string()
    }
}

#[derive(Debug, Clone)]
pub enum CalleeFamily {
    Single(Resolved),
    Merged { members: Vec<FamilyMember> },
}

fn resolved_kind(kind: DefKind) -> ResolvedKind {
    match kind {
        DefKind::Type => ResolvedKind::Type,
        DefKind::Global => ResolvedKind::Global,
        DefKind::Callable => ResolvedKind::Callable,
        DefKind::Decorator => ResolvedKind::Decorator,
    }
}

/// The first binding tier decides even when its target fails to resolve.
pub fn resolve_bare(ws: &Workspace, frag: &FragmentScope, name: Ustr) -> Result<Resolved, String> {
    bare_binding(ws, frag, name)?.ok_or_else(|| unbound(name))
}

// An absent name may still anchor a module path; a broken binding carries
// a diagnostic that the path lookup must retain if no module binds it.
pub(crate) fn bare_binding(
    ws: &Workspace,
    frag: &FragmentScope,
    name: Ustr,
) -> Result<Option<Resolved>, String> {
    single_binding(ws, frag, name, |_| true)
}

/// Annotation spellings are claimed only by decorators at the selected tier.
pub(crate) fn decorator_binding(
    ws: &Workspace,
    frag: &FragmentScope,
    name: Ustr,
) -> Result<Option<Resolved>, String> {
    single_binding(ws, frag, name, |hit| hit.kind == ResolvedKind::Decorator)
}

fn single_binding(
    ws: &Workspace,
    frag: &FragmentScope,
    name: Ustr,
    keep: impl Fn(&Resolved) -> bool,
) -> Result<Option<Resolved>, String> {
    let Some(mut tier) = bind_tiers(ws, frag, name, false)?.first else {
        return Ok(None);
    };
    tier.hits.retain(|hit| keep(hit));
    match tier.hits.as_slice() {
        [] => Ok(None),
        [resolved] => Ok(Some(*resolved)),
        _ => Err(several(ws, name, &tier.hits, tier.tier)),
    }
}

/// A callable can collect candidates from lower-priority tiers.
pub fn resolve_v1_callee(
    ws: &Workspace,
    frag: &FragmentScope,
    name: Ustr,
) -> Result<CalleeFamily, String> {
    resolve_v1_optional_callee(ws, frag, name)?.ok_or_else(|| unbound(name))
}

/// [`resolve_v1_callee`], or `None` when no tier binds the name. A unary
/// operator's function (`-x` -> `__unm`) or the iterator protocol is optional:
/// a scalar operand or a builtin iterable never consults the family.
pub(crate) fn resolve_v1_optional_callee(
    ws: &Workspace,
    frag: &FragmentScope,
    name: Ustr,
) -> Result<Option<CalleeFamily>, String> {
    let Tiers { first, lower } = bind_tiers(ws, frag, name, true)?;
    let Some(tier) = first else {
        return Ok(None);
    };
    if let [resolved] = tier.hits.as_slice()
        && lower.is_empty()
    {
        return Ok(Some(CalleeFamily::Single(*resolved)));
    }
    if !tier.all_callables() {
        return Err(several(ws, name, &tier.hits, tier.tier));
    }
    let mut members: Vec<_> = tier.hits.into_iter().map(|hit| (hit, tier.tier)).collect();
    members.extend(lower);
    // One definition reached through several tiers is one member, at the
    // nearest of them.
    members.sort_by(|(a, a_tier), (b, b_tier)| {
        a.key.as_str().cmp(b.key.as_str()).then(a_tier.cmp(b_tier))
    });
    members.dedup_by_key(|(hit, _)| hit.key);
    if let [(hit, _)] = members[..] {
        return Ok(Some(CalleeFamily::Single(hit)));
    }
    Ok(Some(CalleeFamily::Merged {
        members: members
            .iter()
            .map(|(hit, tier)| hit.member(*tier))
            .collect(),
    }))
}

struct BoundTier {
    hits: bone_core::prelude::SmallVec<[Resolved; 1]>,
    tier: FamilyTier,
}

impl BoundTier {
    fn all_callables(&self) -> bool {
        self.hits
            .iter()
            .all(|hit| hit.kind == ResolvedKind::Callable)
    }
}

#[derive(Default)]
struct Tiers {
    first: Option<BoundTier>,
    lower: Vec<(Resolved, FamilyTier)>,
}

impl Tiers {
    fn wants_more(&self, callee: bool) -> bool {
        self.first
            .as_ref()
            .is_none_or(|first| callee && first.all_callables())
    }

    fn bind(&mut self, hits: impl IntoIterator<Item = Resolved>, tier: FamilyTier) {
        if self.first.is_none() {
            self.first = Some(BoundTier {
                hits: hits.into_iter().collect(),
                tier,
            });
        } else {
            self.lower.extend(
                hits.into_iter()
                    .filter(|hit| hit.kind == ResolvedKind::Callable)
                    .map(|hit| (hit, tier)),
            );
        }
    }
}

fn bind_tiers(
    ws: &Workspace,
    frag: &FragmentScope,
    name: Ustr,
    callee: bool,
) -> Result<Tiers, String> {
    let mut tiers = Tiers::default();
    // Trusted stdlib fragments share the compiler-owned root and have no
    // imports; looking through both the own and std tiers repeats one lookup.
    if frag.package == STD_PACKAGE_ID || ws.ns.package(frag.package).is_none() {
        if let Some(std) = std_definition(ws, name) {
            tiers.bind([std], FamilyTier::Std);
        }
        return Ok(tiers);
    }
    let viewers = [frag];

    // Tier 2: the current logical module's own definitions and explicit
    // export bindings.
    let own = Lookup::new(ws).explicit_members(
        &viewers,
        ModuleKey::at(ws, frag.package, &frag.module_path),
        name,
    )?;
    if let Some(own) = own {
        let hit = collapse(ws, own, name, || unbound(name), "definitions")?;
        tiers.bind([hit], FamilyTier::Own);
    }

    // Tier 3: an explicit import failure blocks lower tiers.
    if tiers.wants_more(callee) {
        if let Some(hits) = import_tier(ws, frag, name)? {
            tiers.bind(hits, FamilyTier::Imports);
        }
    }

    // Tier 4: glob bindings. Several candidates are one family only in
    // callee position; anywhere else they are ambiguous at use, never a
    // HashMap-order pick.
    if tiers.wants_more(callee) {
        let glob_hits = glob_candidates(ws, frag, name)?;
        if !glob_hits.is_empty() {
            tiers.bind(glob_hits, FamilyTier::Globs);
        }
    }

    // Tier 5: the compiler-owned std export view.
    if tiers.wants_more(callee) {
        if let Some(std) = std_definition(ws, name) {
            tiers.bind([std], FamilyTier::Std);
        }
    }
    Ok(tiers)
}

fn import_tier(
    ws: &Workspace,
    frag: &FragmentScope,
    name: Ustr,
) -> Result<Option<Vec<Resolved>>, String> {
    if let Some(targets) = frag.named.get(&name) {
        let mut hits: Vec<Resolved> = Vec::new();
        for target in targets {
            hits.extend(resolve_import_target(ws, frag, name, target)?);
        }
        hits.sort_by(|a, b| a.key.as_str().cmp(b.key.as_str()));
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
                let Some(unary) = unary_selector_path(target) else {
                    continue;
                };
                if let Some(resolved) = walk_optional_definition(ws, frag, &unary)? {
                    hits.push(resolved);
                }
            }
            hits.sort_by(|a, b| a.key.as_str().cmp(b.key.as_str()));
            hits.dedup();
            if !hits.is_empty() {
                return Ok(Some(hits));
            }
        }
    }
    Ok(None)
}

/// The prefix family selected alongside subtraction by an `operator(-)` import.
fn unary_selector_path(path: &UsePathRef) -> Option<UsePathRef> {
    if path.segments.last().copied()? != Ustr::from("__op_sub") {
        return None;
    }
    let mut unary = path.clone();
    *unary.segments.last_mut().unwrap() = Ustr::from("__unm");
    Some(unary)
}

fn resolve_import_target(
    ws: &Workspace,
    frag: &FragmentScope,
    name: Ustr,
    path: &UsePathRef,
) -> Result<Vec<Resolved>, String> {
    let Some(unary) = unary_selector_path(path) else {
        return resolve_path(ws, frag, path).map(|hit| vec![hit]);
    };

    let mut hits = Vec::new();
    for candidate in [path, &unary] {
        if let Some(hit) = walk_optional_definition(ws, frag, candidate)? {
            hits.push(hit);
        }
    }
    if hits.is_empty() {
        Err(unbound(name))
    } else {
        Ok(hits)
    }
}

fn unbound(name: impl std::fmt::Display) -> String {
    format!("`{name}` did not resolve to any definition")
}

/// Several imports or globs of `name` where one definition is wanted: a
/// composed callable family used as a value, or definitions that cannot
/// compose.
fn several(ws: &Workspace, name: Ustr, hits: &[Resolved], tier: FamilyTier) -> String {
    let keys = hits
        .iter()
        .map(|hit| hit_display(ws, hit))
        .collect::<Vec<_>>()
        .join(", ");
    let callables = hits.iter().all(|hit| hit.kind == ResolvedKind::Callable);
    match (tier, callables) {
        (FamilyTier::Imports, true) => {
            format!(
                "`{name}` names an overload family of several imports ({keys}), not a single definition"
            )
        }
        (FamilyTier::Imports, false) => {
            format!(
                "`{name}` is imported from several definitions that do not form one overload family: {keys}"
            )
        }
        (FamilyTier::Globs, true) => {
            format!(
                "`{name}` names an overload family of several glob imports ({keys}), not a single definition"
            )
        }
        (FamilyTier::Globs, false) => format!("`{name}` is ambiguous between glob imports {keys}"),
        _ => unreachable!("only imports and globs can bind several definitions"),
    }
}

fn std_definition(ws: &Workspace, name: Ustr) -> Option<Resolved> {
    // The std exports among the intrinsics (the FieldRef family) live in the
    // std view below; the other formers resolve bare even with no stdlib.
    if bone_core::modules::intrinsic_type(name) == Some(bone_core::modules::IntrinsicType::Former) {
        return Some(Resolved {
            key: name,
            kind: ResolvedKind::Type,
        });
    }
    let (key, kind) = ws.ns.std_definition(name)?;
    let kind = resolved_kind(kind);
    Some(Resolved { key, kind })
}

fn glob_candidates(
    ws: &Workspace,
    frag: &FragmentScope,
    name: Ustr,
) -> Result<Vec<Resolved>, String> {
    let viewers = [frag];
    let mut lookup = Lookup::new(ws);
    let mut hits = Lookup::new(ws)
        .glob_export_members(
            &viewers,
            ModuleKey::at(ws, frag.package, &frag.module_path),
            name,
        )?
        .found;
    // `frag.globs` is kept in path order (`UsePathRef::insert_sorted`).
    for glob in &frag.globs {
        // Every fragment glob was checked where it is declared
        // (`check_use_target`), so a failure here is a real diagnostic.
        let module = walk_modules(ws, frag, glob)?;
        hits.extend(lookup.members_of(&viewers, module, name)?.found);
    }
    hits.sort_by(|a, b| a.key.as_str().cmp(b.key.as_str()));
    hits.dedup();
    Ok(hits)
}

/// Check visibility for the requesting fragment, including export-chain viewers.
fn admits(
    ws: &Workspace,
    frag: &FragmentScope,
    vis: &NsVisibility,
    owner_pkg: u32,
    owner_path: &[Ustr],
) -> bool {
    ws.ns
        .can_access(vis, owner_pkg, owner_path, frag.package, &frag.module_path)
}

fn all_admit_key(ws: &Workspace, viewers: &[&FragmentScope], key: Ustr) -> bool {
    let Some(meta) = ws.defs.get(&key) else {
        return true;
    };
    // One overload must admit all viewers along a re-export chain.
    let admitted = |vis| all_admit(ws, viewers, vis, meta.package, &meta.module);
    if meta.kind == DefKind::Callable
        && let Some(members) = ws.callable_family_members(key)
    {
        members.iter().any(|member| admitted(&member.vis))
    } else {
        admitted(&meta.vis)
    }
}

fn all_admit(
    ws: &Workspace,
    viewers: &[&FragmentScope],
    vis: &NsVisibility,
    pkg: u32,
    path: &[Ustr],
) -> bool {
    viewers.iter().all(|v| admits(ws, v, vis, pkg, path))
}

#[derive(Debug, Default)]
pub struct Members {
    /// Distinct definitions the viewers may name, sorted by key.
    pub found: Vec<Resolved>,
    /// Some binding of the name exists that a viewer may not name.
    pub denied: bool,
    /// An export chain came back to a binding it was already following.
    pub cycle: bool,
    /// A named export binding binds the name to a module: a path
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
        self.found
            .sort_by(|a, b| a.key.as_str().cmp(b.key.as_str()));
        self.found.dedup();
        self
    }
}

/// Active export lookups, distinguished by the kind of export.
type Visit = (ModuleKey, Ustr, Guard);

struct Lookup<'w> {
    ws: &'w Workspace,
    visiting: Vec<Visit>,
}

impl<'w> Lookup<'w> {
    fn new(ws: &'w Workspace) -> Self {
        Self {
            ws,
            visiting: Vec::new(),
        }
    }

    fn follow<T>(&mut self, visit: Visit, f: impl FnOnce(&mut Self) -> T) -> T {
        self.visiting.push(visit);
        let result = f(self);
        self.visiting.pop();
        result
    }

    fn members_of(
        &mut self,
        viewers: &[&FragmentScope],
        module: ModuleKey,
        name: Ustr,
    ) -> Result<Members, String> {
        if let Some(explicit) = self.explicit_members(viewers, module, name)? {
            return Ok(explicit);
        }
        self.glob_export_members(viewers, module, name)
    }

    /// The module's explicit binding of `name` -- an owned definition or a named
    /// export binding -- or `None` when the module binds `name` only through
    /// globs, if at all.
    fn explicit_members(
        &mut self,
        viewers: &[&FragmentScope],
        module: ModuleKey,
        name: Ustr,
    ) -> Result<Option<Members>, String> {
        let ModuleKey(pkg_id, node_key) = module;
        module.member_probe(name);
        let Some(pkg) = self.ws.ns.package(pkg_id) else {
            return Ok(None);
        };
        let Some(node) = pkg.node(node_key) else {
            return Ok(None);
        };
        let path = &node.segments;
        let mut out = Members::default();
        if let Some((key, kind)) = node.owned(name) {
            let kind = resolved_kind(kind);
            if all_admit_key(self.ws, viewers, key) {
                out.found.push(Resolved { key, kind });
            } else {
                out.denied = true;
            }
            return Ok(Some(out));
        }
        let Some(binding) = node.exports.get(&name) else {
            return Ok(None);
        };
        if !all_admit(self.ws, viewers, &binding.vis, pkg_id, path) {
            out.denied = true;
            return Ok(Some(out));
        }
        let visit = (module, name, Guard::ExportBinding);
        if self.visiting.contains(&visit) {
            out.cycle = true;
            return Ok(Some(out));
        }
        let exporter = exporter_scope(pkg, path);
        let followed = self.follow(visit, |lookup| {
            lookup.walk_members(&exporter, viewers, &binding.target)
        });
        let followed = followed?;
        if followed.found.is_empty() && !followed.cycle {
            let target = binding
                .target
                .segments
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join("::");
            if !followed.denied {
                let module = self.follow(visit, |lookup| {
                    lookup.walk_module_path(
                        &exporter,
                        &binding.target,
                        binding.target.segments.len(),
                    )
                });
                if module?.is_some() {
                    out.module = true;
                    return Ok(Some(out));
                }
                return Err(unbound(target));
            }
            // A viewer's denial is an omission; the exporter's own is a broken
            // binding (the exporter must be permitted to name it).
            let for_exporter = self.follow(visit, |lookup| {
                lookup.walk_members(&exporter, &[], &binding.target)
            });
            if for_exporter?.found.is_empty() {
                return Err(format!(
                    "`{target}` is not accessible from the module that re-exports it"
                ));
            }
        }
        out.absorb(followed);
        Ok(Some(out.finish()))
    }

    /// What the module's glob export bindings offer under `name`.
    fn glob_export_members(
        &mut self,
        viewers: &[&FragmentScope],
        module: ModuleKey,
        name: Ustr,
    ) -> Result<Members, String> {
        let ModuleKey(pkg_id, node_key) = module;
        module.member_probe(name);
        let mut out = Members::default();
        let Some(pkg) = self.ws.ns.package(pkg_id) else {
            return Ok(out);
        };
        let Some(node) = pkg.node(node_key) else {
            return Ok(out);
        };
        if node.glob_exports.is_empty() {
            return Ok(out);
        }
        let path = &node.segments;
        let exporter = exporter_scope(pkg, path);
        let mut chain = viewers.to_vec();
        chain.push(&exporter);
        // Kept in target path order (`UsePathRef::insert_sorted`).
        for binding in &node.glob_exports {
            if !all_admit(self.ws, viewers, &binding.vis, pkg_id, path) {
                continue;
            }
            let target = walk_modules(self.ws, &exporter, &binding.target)?;
            // A glob chain that comes back to a module it is already opening
            // adds nothing new; that module's members are being collected.
            let visit = (target, name, Guard::GlobOpen);
            if self.visiting.contains(&visit) {
                continue;
            }
            let followed = self.follow(visit, |lookup| lookup.members_of(&chain, target, name));
            out.absorb(followed?);
        }
        Ok(out.finish())
    }

    fn resolve_anchor(
        &mut self,
        frag: &FragmentScope,
        path: &UsePathRef,
    ) -> Result<(ModuleKey, usize), String> {
        let &first = path.segments.first().ok_or("empty path")?;
        if path.from_root {
            return Ok((ModuleKey::root(frag.package), 0));
        }
        if first.as_str() == "std" {
            return Ok((ModuleKey::root(STD_PACKAGE_ID), 1));
        }
        let relative = ModuleKey::at(self.ws, frag.package, &frag.module_path);
        match first.as_str() {
            "self" => return Ok((relative, 1)),
            "super" => return Ok((relative.parent()?, 1)),
            _ => {}
        }
        let pkg = self.ws.ns.package(frag.package).ok_or("unknown package")?;
        if first == pkg.self_name {
            return Ok((ModuleKey::root(frag.package), 1));
        }
        if let Some(&target) = pkg.aliases.get(&first) {
            return Ok((ModuleKey::root(target), 1));
        }
        relative.member_probe(first);
        if pkg.node(relative.1).is_some_and(|node| {
            node.children.contains_key(&first) || node.exports.contains_key(&first)
        }) {
            return Ok((relative, 0));
        }
        Ok(self
            .imported_module(frag, relative, first)?
            .map_or((relative, 0), |module| (module, 1)))
    }

    fn imported_module(
        &mut self,
        frag: &FragmentScope,
        module: ModuleKey,
        name: Ustr,
    ) -> Result<Option<ModuleKey>, String> {
        let Some(targets) = frag.named.get(&name) else {
            return Ok(None);
        };
        let visit = (module, name, Guard::Import);
        if self.visiting.contains(&visit) {
            return Ok(None);
        }
        let mut modules = self.follow(visit, |lookup| {
            let mut modules = Vec::new();
            for target in targets {
                modules.extend(lookup.walk_module_path(frag, target, target.segments.len())?);
            }
            Ok::<_, String>(modules)
        })?;
        modules.sort_by(|a, b| {
            a.0.cmp(&b.0).then_with(|| {
                a.node(self.ws)
                    .map(|node| &node.segments)
                    .cmp(&b.node(self.ws).map(|node| &node.segments))
            })
        });
        modules.dedup();
        match modules.len() {
            0 => Ok(None),
            1 => Ok(modules.pop()),
            _ => Err(format!(
                "`{name}` is imported from several modules: {}",
                modules
                    .iter()
                    .map(|&m| module_display(self.ws, m))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    fn step_module(
        &mut self,
        frag: &FragmentScope,
        current: ModuleKey,
        seg: Ustr,
    ) -> Result<Option<ModuleKey>, String> {
        let ModuleKey(pkg_id, node_key) = current;
        current.member_probe(seg);
        let Some(pkg) = self.ws.ns.package(pkg_id) else {
            return Ok(None);
        };
        let Some(node) = pkg.node(node_key) else {
            return Ok(None);
        };
        let denied = || format!("module `{seg}` is not accessible from this module");
        if let Some(&child) = node.children.get(&seg) {
            let child = ModuleKey(pkg_id, child);
            let decl = child.require(self.ws, seg)?;
            let vis = decl.vis.as_ref().unwrap_or(&NsVisibility::Private);
            return admits(self.ws, frag, vis, pkg_id, &node.segments)
                .then_some(Some(child))
                .ok_or_else(denied);
        }
        let Some(binding) = node.exports.get(&seg) else {
            return Ok(None);
        };
        let visit = (current, seg, Guard::ModuleExport);
        if self.visiting.contains(&visit) {
            return Err("circular `use` binding with no defining target".to_string());
        }
        let exporter = exporter_scope(pkg, &node.segments);
        let target = self.follow(visit, |lookup| {
            lookup.walk_module_path(&exporter, &binding.target, binding.target.segments.len())
        });
        if target.as_ref().is_ok_and(Option::is_some)
            && !admits(self.ws, frag, &binding.vis, pkg_id, &node.segments)
        {
            return Err(denied());
        }
        target
    }

    fn walk_members(
        &mut self,
        frag: &FragmentScope,
        outer: &[&FragmentScope],
        path: &UsePathRef,
    ) -> Result<Members, String> {
        let segments = &path.segments;
        let module = self
            .walk_module_path(frag, path, segments.len() - 1)?
            .ok_or_else(|| format!("no module `{}` in path", segments[segments.len() - 2]))?;
        let mut viewers = vec![frag];
        viewers.extend_from_slice(outer);
        self.members_of(&viewers, module, last_of(segments))
    }

    /// Walk a module prefix; `None` means only its final segment is not a module.
    fn walk_module_path(
        &mut self,
        frag: &FragmentScope,
        path: &UsePathRef,
        stop: usize,
    ) -> Result<Option<ModuleKey>, String> {
        let (module, index) = self.walk_prefix::<false>(frag, path, stop)?;
        if index == stop {
            module.require(self.ws, last_of(&path.segments))?;
            Ok(Some(module))
        } else if index + 1 == stop {
            Ok(None)
        } else {
            Err(format!("no module `{}` in path", path.segments[index]))
        }
    }

    fn walk_prefix<const VALUE: bool>(
        &mut self,
        frag: &FragmentScope,
        path: &UsePathRef,
        stop: usize,
    ) -> Result<(ModuleKey, usize), String> {
        let (mut module, start) = self.resolve_anchor(frag, path)?;
        for (index, &segment) in path.segments[..stop].iter().enumerate().skip(start) {
            match segment.as_str() {
                "self" => continue,
                "super" => {
                    module = module.parent()?;
                    continue;
                }
                _ => {}
            }
            if VALUE {
                module.require(self.ws, segment)?;
            }
            let Some(next) = self.step_module(frag, module, segment)? else {
                return Ok((module, index));
            };
            module = next;
        }
        Ok((module, stop))
    }
}

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

/// Explicit members shadow glob exports, including inaccessible members.
pub fn module_members(
    ws: &Workspace,
    viewers: &[&FragmentScope],
    pkg_id: u32,
    path: &[Ustr],
    name: Ustr,
) -> Result<Members, String> {
    Lookup::new(ws).members_of(viewers, ModuleKey::at(ws, pkg_id, path), name)
}

/// An exporting module standing in as the site its export targets resolve
/// from.
fn exporter_scope(pkg: &bone_core::modules::NsPackage, path: &[Ustr]) -> FragmentScope {
    // Spelled out: `..Default::default()` would intern an empty package
    // name only to discard it, once per glob step of every lookup.
    FragmentScope {
        package: pkg.id,
        package_name: pkg.self_name,
        module_path: path.to_vec(),
        named: Default::default(),
        globs: Vec::new(),
    }
}

/// Every name module `path` offers to `viewers`, sorted: owned names, named
/// export bindings, and the members of admitted glob export targets. Callers
/// decide each name through `module_members`; this only enumerates.
pub fn module_member_names(
    ws: &Workspace,
    viewers: &[&FragmentScope],
    pkg_id: u32,
    path: &[Ustr],
) -> Result<Vec<Ustr>, String> {
    let mut names = Vec::new();
    collect_member_names(
        ws,
        viewers,
        ModuleKey::at(ws, pkg_id, path),
        &mut names,
        &mut Vec::new(),
    )?;
    names.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    names.dedup();
    Ok(names)
}

fn collect_member_names(
    ws: &Workspace,
    viewers: &[&FragmentScope],
    module: ModuleKey,
    names: &mut Vec<Ustr>,
    seen: &mut Vec<ModuleKey>,
) -> Result<(), String> {
    let ModuleKey(pkg_id, node_key) = module;
    let Some(pkg) = ws.ns.package(pkg_id) else {
        return Ok(());
    };
    if seen.contains(&module) {
        return Ok(());
    }
    seen.push(module);
    let Some(node) = pkg.node(node_key) else {
        return Ok(());
    };
    names.extend(node.owned_types.keys());
    names.extend(node.owned_globals.keys());
    names.extend(node.owned_callables.keys());
    names.extend(node.owned_decorators.keys());
    names.extend(node.exports.keys());
    let path = &node.segments;
    let exporter = exporter_scope(pkg, path);
    for binding in &node.glob_exports {
        if !all_admit(ws, viewers, &binding.vis, pkg_id, path) {
            continue;
        }
        let target = walk_modules(ws, &exporter, &binding.target)?;
        collect_member_names(ws, viewers, target, names, seen)?;
    }
    Ok(())
}

fn collapse(
    ws: &Workspace,
    members: Members,
    name: Ustr,
    not_found: impl Fn() -> String,
    what: &str,
) -> Result<Resolved, String> {
    pick_member(ws, members, || format!("`{name}`"), what)?.ok_or_else(not_found)
}

fn pick_member(
    ws: &Workspace,
    members: Members,
    name: impl FnOnce() -> String,
    what: &str,
) -> Result<Option<Resolved>, String> {
    match members.found.as_slice() {
        [single] => Ok(Some(*single)),
        [] if members.cycle => Err("circular `use` binding with no defining target".to_string()),
        [] if members.denied => Err(format!("{} is not accessible from this module", name())),
        [] if members.module => Err(format!("{} names a module, not a definition", name())),
        [] => Ok(None),
        many => Err(format!(
            "{} is ambiguous between {what} {}",
            name(),
            many.iter()
                .map(|hit| hit_display(ws, hit))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn check_key_access(ws: &Workspace, frag: &FragmentScope, key: Ustr) -> Result<(), String> {
    if all_admit_key(ws, &[frag], key) {
        return Ok(());
    }
    let name = ws.defs.get(&key).map_or(key, |meta| meta.name);
    Err(format!("`{name}` is not accessible from this module"))
}

pub fn resolve_path(
    ws: &Workspace,
    frag: &FragmentScope,
    path: &UsePathRef,
) -> Result<Resolved, String> {
    collapse(
        ws,
        Lookup::new(ws).walk_members(frag, &[], path)?,
        last_of(&path.segments),
        || unbound(joined(&path.segments)),
        "glob re-exports",
    )
}

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
/// module, which must itself be permitted to name it.
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
    if !glob
        && result.is_err()
        && let Some(unary) = unary_selector_path(path)
        && check_target(ws, frag, &unary, false, export).is_ok()
    {
        return Ok(());
    }
    result
}

fn check_target(
    ws: &Workspace,
    frag: &FragmentScope,
    path: &UsePathRef,
    glob: bool,
    export: bool,
) -> Result<(), String> {
    if glob {
        return walk_modules(ws, frag, path).map(|_| ());
    }
    let members = Lookup::new(ws).walk_members(frag, &[], path);
    if matches!(&members, Ok(m) if !m.found.is_empty() || m.cycle || m.module) {
        return Ok(());
    }
    let module = Lookup::new(ws).walk_module_path(frag, path, path.segments.len());
    if matches!(module, Ok(Some(_))) {
        return Ok(());
    }
    // Denials read as the lookup's own do: a fragment names the member, an
    // exporter the whole target it re-exports.
    let target = joined(&path.segments);
    match members? {
        m if m.denied && export => Err(format!(
            "`{target}` is not accessible from the module that re-exports it"
        )),
        m if m.denied => Err(format!(
            "`{}` is not accessible from this module",
            last_of(&path.segments)
        )),
        // The last segment is a module this site may not enter.
        _ => {
            module?;
            Err(format!(
                "`{target}` did not resolve to any definition or module"
            ))
        }
    }
}

pub fn check_use_binding(
    ws: &Workspace,
    site: &FragmentScope,
    local: Ustr,
    path: &UsePathRef,
) -> Result<(), String> {
    if matches!(local.as_str(), "self" | "super" | "std") {
        return Err(format!(
            "`{local}` is a reserved path root and cannot name an import"
        ));
    }
    let Some(landing) = Lookup::new(ws).walk_module_path(site, path, path.segments.len())? else {
        return Ok(());
    };
    let pkg = ws.ns.package(site.package).ok_or("unknown package")?;
    let current = ModuleKey::at(ws, site.package, &site.module_path);
    let child = current.node(ws).and_then(|node| node.children.get(&local));
    let clash = if child.is_some_and(|key| landing != ModuleKey(site.package, *key)) {
        Some("a child module of this module")
    } else if pkg
        .aliases
        .get(&local)
        .is_some_and(|&target| landing != ModuleKey::root(target))
    {
        Some("a dependency alias")
    } else if local == pkg.self_name && landing != ModuleKey::root(site.package) {
        Some("this package's name")
    } else {
        None
    };
    match clash {
        Some(what) => Err(format!(
            "`use` binds `{local}` to module `{}`, but `{local}` is also {what}",
            module_display(ws, landing)
        )),
        None => Ok(()),
    }
}

fn module_display(ws: &Workspace, module: ModuleKey) -> String {
    let ModuleKey(package, path) = module;
    let root = ws
        .ns
        .package(package)
        .map_or_else(|| package.to_string(), |pkg| pkg.self_name.to_string());
    if root.starts_with(bone_core::modules::STANDALONE_PACKAGE_PREFIX) {
        format!("::{path}")
    } else if path.is_empty() {
        root
    } else {
        format!("{root}::{path}")
    }
}

pub(crate) enum MemberTarget {
    Global(Ustr),
    Callable(Ustr),
    EnumVariant { enum_key: Ustr, variant: Ustr },
    Type(Ustr),
}

/// Member-wise resolution: at every node a segment may be a child module OR
/// an owned definition; `Type::Variant` consumes the type and its variant
/// (the resolver decides, not the lexer).
pub(crate) fn resolve_member_path(
    ws: &Workspace,
    frag: &FragmentScope,
    path: &UsePathRef,
) -> Result<MemberTarget, String> {
    let segments = &path.segments;
    if !path.from_root
        && let [name] = segments.as_slice()
    {
        let resolved = resolve_bare(ws, frag, *name)?;
        return match resolved.kind {
            ResolvedKind::Type => Ok(MemberTarget::Type(resolved.key)),
            ResolvedKind::Callable => Ok(MemberTarget::Callable(resolved.key)),
            ResolvedKind::Global | ResolvedKind::Binding => Ok(MemberTarget::Global(resolved.key)),
            ResolvedKind::Decorator => Err(format!("`{name}` names a decorator, not a value")),
        };
    }

    let mut bare_error = None;
    // `E::V` whose `E` binds nothing: the error names `E`, the one name the
    // author has to bring into scope.
    let mut first_unbound = false;
    if !path.from_root && segments.len() == 2 {
        let first = segments[0];
        match bare_binding(ws, frag, first) {
            Ok(Some(resolved)) => {
                if resolved.kind == ResolvedKind::Type {
                    let key = resolved.key;
                    let variant = segments[1];
                    if ws.nominals.enum_id(key).is_some() {
                        check_key_access(ws, frag, key)?;
                        return Ok(MemberTarget::EnumVariant {
                            enum_key: key,
                            variant,
                        });
                    }
                }
            }
            Ok(None) => first_unbound = true,
            // Module anchors need the path walk; if that also finds no
            // binding, retain the bare lookup's ambiguity or other error.
            Err(error) => bare_error = Some(error),
        }
    }
    let (module, index) = Lookup::new(ws).walk_prefix::<true>(frag, path, segments.len())?;
    if index == segments.len() {
        return Err("path names a module, not a definition".to_string());
    }
    let seg = segments[index];
    let members = Lookup::new(ws).members_of(&[frag], module, seg)?;
    let not_found = || {
        if index == 0 {
            if let Some(error) = &bare_error {
                return error.clone();
            }
            if first_unbound {
                return unbound(seg);
            }
        }
        unbound(joined(segments))
    };
    let Resolved { key, kind } = collapse(ws, members, seg, not_found, "glob re-exports")?;
    match (kind, &segments[index + 1..]) {
        (ResolvedKind::Type, []) => Ok(MemberTarget::Type(key)),
        (ResolvedKind::Type, [variant]) if ws.nominals.enum_id(key).is_some() => {
            Ok(MemberTarget::EnumVariant {
                enum_key: key,
                variant: *variant,
            })
        }
        (ResolvedKind::Type, [variant]) => Err(format!(
            "`{key}` is not an enum; `{variant}` cannot be selected from it"
        )),
        (ResolvedKind::Type, _) => Err(format!("`{key}` cannot be traversed further")),
        (ResolvedKind::Callable, []) => Ok(MemberTarget::Callable(key)),
        (_, []) => Ok(MemberTarget::Global(key)),
        _ => Err(format!("`{key}` cannot be traversed")),
    }
}

fn joined(segments: &[Ustr]) -> String {
    segments
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join("::")
}

pub fn resolve_path_to_module(
    ws: &Workspace,
    frag: &FragmentScope,
    path: &UsePathRef,
) -> Result<(u32, Vec<Ustr>), String> {
    let module = walk_modules(ws, frag, path)?;
    Ok((
        module.0,
        module.node(ws).expect("resolved module").segments.clone(),
    ))
}

/// Resolve a definition while distinguishing a missing final member from errors.
fn walk_optional_definition(
    ws: &Workspace,
    frag: &FragmentScope,
    path: &UsePathRef,
) -> Result<Option<Resolved>, String> {
    let members = Lookup::new(ws).walk_members(frag, &[], path)?;
    let name = last_of(&path.segments);
    pick_member(
        ws,
        members,
        || match unary_operator_entry(name.as_str()) {
            Some(symbol) => format!("operator `{symbol}`"),
            None => format!("`{name}`"),
        },
        "glob re-exports",
    )
}

fn walk_modules(
    ws: &Workspace,
    frag: &FragmentScope,
    path: &UsePathRef,
) -> Result<ModuleKey, String> {
    Lookup::new(ws)
        .walk_module_path(frag, path, path.segments.len())?
        .ok_or_else(|| format!("no module `{}` in path", last_of(&path.segments)))
}

// Value paths distinguish an absent module (an error) from an absent child
// module (try a definition in the current module).
/// Resolve a variant path with the same tier lookup used for value paths.
pub(crate) fn resolve_variant_pattern(
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
        MemberTarget::Global(key) | MemberTarget::Callable(key) => {
            Err(format!("`{key}` is not an enum variant"))
        }
    }
}

fn last_of(segments: &[Ustr]) -> Ustr {
    segments[segments.len() - 1]
}

pub(crate) fn plain(name: Ustr) -> Resolved {
    Resolved {
        key: name,
        kind: ResolvedKind::Binding,
    }
}
