use std::sync::Arc;

use skull_parser::stdlib::ModuleSet;
use skull_parser::{
    Ast, FunctionBody, GlobalPolicy, ItemKind, MacroRegistry, PackageTag,
    check_macro_name_uniqueness, collect_registry, cook_one,
};

use bone_core::modules::ScopeProbe;
use bone_semantic::{
    hir::{Function, Module, TypeCtor, TypeExpr},
    prelude::*,
    semantic_passes::{FragmentContribution, TypeArenaSeed, contribute, lower_modules_seeded},
};

use crate::span_shift;

/// Re-exported because it is part of [`Session::new`]'s signature: a caller
/// that builds a session needs the alias table type without depending on
/// `bone-core` directly.
pub use bone_core::modules::NsAliases;

/// Unique identifier of a Body across all modules.
pub type BodyId = (ModuleId, BodyOwner);

/// The source-coordinate change carried with one incremental body update.
pub use bone_core::text_edit::TextEditDelta;

/// The semantic part of a body-only update, kept separate from coordinate
/// relocation so graph owners are not re-walked merely because spans moved.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BodyUpdate {
    pub edit: Option<TextEditDelta>,
    pub semantic_bodies: Vec<BodyId>,
    pub span_only: bool,
}

/// The broadest rebuild needed after an edit.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum UpdateTier {
    #[default]
    NoChange,
    BodyOnly,
    Interface,
    Scope,
    FullUser,
}

/// Result of a session update. Stores the type of update, and all the bodies
/// that were reinferred.
#[derive(Clone, Debug, Default)]
pub struct UpdateResult {
    pub tier: UpdateTier,
    pub reinferred: Vec<BodyId>,
    pub relowered: Vec<BodyId>,
    pub body_update: BodyUpdate,
}

/// Namespace changes keyed by the facts recorded as scope probes.
#[derive(Clone, Debug, Default)]
struct ScopeDelta {
    members: FxHashSet<(u32, Ustr, Ustr)>,
    all_members: FxHashSet<(u32, Ustr)>,
    nodes: FxHashSet<(u32, Ustr)>,
    fragments: FxHashSet<Ustr>,
}

impl ScopeDelta {
    fn is_empty(&self) -> bool {
        self.members.is_empty()
            && self.all_members.is_empty()
            && self.nodes.is_empty()
            && self.fragments.is_empty()
    }

    fn hits(&self, probe: &ScopeProbe) -> bool {
        match *probe {
            ScopeProbe::Member {
                package,
                node,
                name,
            } => {
                self.members.contains(&(package, node, name))
                    || self.all_members.contains(&(package, node))
            }
            ScopeProbe::Node { package, node } => self.nodes.contains(&(package, node)),
        }
    }

    /// Diff two assembled trees (`old` and `new` hold `ns.packages`, `defs`
    /// and `fragment_scopes`).
    fn between(old: &Workspace, new: &Workspace) -> Self {
        let mut delta = ScopeDelta::default();

        let package_ids: FxHashSet<u32> = old
            .ns
            .packages
            .keys()
            .chain(new.ns.packages.keys())
            .copied()
            .collect();
        for package in package_ids {
            let (o, n) = (old.ns.packages.get(&package), new.ns.packages.get(&package));
            let node_keys: FxHashSet<Ustr> = o
                .into_iter()
                .flat_map(|p| p.nodes.keys())
                .chain(n.into_iter().flat_map(|p| p.nodes.keys()))
                .copied()
                .collect();
            for key in node_keys {
                match (
                    o.and_then(|p| p.nodes.get(&key)),
                    n.and_then(|p| p.nodes.get(&key)),
                ) {
                    (Some(on), Some(nn)) => {
                        if on.vis != nn.vis {
                            delta.nodes.insert((package, key));
                        }
                        if on.glob_exports != nn.glob_exports {
                            delta.all_members.insert((package, key));
                        }
                        for (a, b) in [
                            (&on.owned_types, &nn.owned_types),
                            (&on.owned_globals, &nn.owned_globals),
                            (&on.owned_callables, &nn.owned_callables),
                            (&on.owned_decorators, &nn.owned_decorators),
                            (&on.children, &nn.children),
                        ] {
                            for name in changed_keys(a, b) {
                                delta.members.insert((package, key, name));
                            }
                        }
                        for name in changed_keys(&on.callable_members, &nn.callable_members) {
                            delta.members.insert((package, key, name));
                        }
                        for name in changed_keys(&on.exports, &nn.exports) {
                            delta.members.insert((package, key, name));
                        }
                    }
                    (None, None) => {}
                    _ => {
                        delta.nodes.insert((package, key));
                        delta.all_members.insert((package, key));
                    }
                }
            }
        }

        for key in changed_keys_by(&old.defs, &new.defs, |o, n| {
            (o.package, &o.module, o.kind, o.name, &o.vis)
                == (n.package, &n.module, n.kind, n.name, &n.vis)
        }) {
            for meta in old.defs.get(&key).into_iter().chain(new.defs.get(&key)) {
                delta.members.insert((meta.package, meta.node, meta.name));
            }
        }
        delta.fragments.extend(changed_keys_by(
            &old.fragment_scopes,
            &new.fragment_scopes,
            |o, n| {
                (
                    o.package,
                    &o.package_name,
                    &o.module_path,
                    &o.named,
                    &o.globs,
                ) == (
                    n.package,
                    &n.package_name,
                    &n.module_path,
                    &n.named,
                    &n.globs,
                )
            },
        ));

        delta
    }
}

/// The keys whose entries differ between two maps, a missing entry included.
fn changed_keys<V: PartialEq>(old: &FxHashMap<Ustr, V>, new: &FxHashMap<Ustr, V>) -> Vec<Ustr> {
    changed_keys_by(old, new, PartialEq::eq)
}

fn changed_keys_by<V>(
    old: &FxHashMap<Ustr, V>,
    new: &FxHashMap<Ustr, V>,
    same: impl Fn(&V, &V) -> bool,
) -> Vec<Ustr> {
    old.iter()
        .filter(|(k, value)| !new.get(*k).is_some_and(|next| same(value, next)))
        .map(|(k, _)| *k)
        .chain(new.keys().filter(|k| !old.contains_key(*k)).copied())
        .collect()
}

/// How many times its live size a module's type arena may grow by keeping
/// earlier lowerings' entries.
const MAX_KEPT_ARENA_GROWTH: usize = 16;

/// Reuse the warm-only workspace when no user macro can affect warm expansion.
#[derive(Clone, Debug)]
enum WarmBase {
    /// Not built yet: every build so far had a collision.
    Unbuilt,
    Clean(Box<Workspace>),
    /// The warm modules alone were refused or report a diagnostic, so they
    /// may name a user definition; never reused.
    Unusable,
}

#[derive(Clone, Debug)]
pub struct Session {
    pub ws: Workspace,
    /// Shared, never edited: a build that must expand them copies them.
    pub warm: Arc<[Ast]>,
    pub warm_mids: FxHashSet<ModuleId>,
    pub paths: Vec<String>,
    pub files: FxHashMap<String, UserFile>,
    /// Reverse dep index: name -> modules whose bodies reference it. Used by
    /// `rebuild_interface` to find which modules to re-infer on an interface edit.
    pub dependents: FxHashMap<Ustr, FxHashSet<ModuleId>>,
    /// Forward dep index: module -> the names its bodies reference. The inverse
    /// of `dependents`, kept so a body edit can update just the edited module's
    /// entry in O(its deps) instead of rescanning the whole workspace.
    module_dep_names: FxHashMap<ModuleId, FxHashSet<Ustr>>,
    /// Reverse dependencies from type names to interfaces that mention them.
    dependents_iface: FxHashMap<Ustr, FxHashSet<ModuleId>>,
    /// Independent of inference: even a module with body errors has interface
    /// deps. Invalidated when its lowered interface is replaced or purged.
    module_iface_dep_names: FxHashMap<ModuleId, FxHashSet<Ustr>>,
    pub registry: MacroRegistry,
    /// Warm macro signatures, captured before expansion removes their definitions.
    warm_macros: Vec<(Ustr, MacroDef)>,
    /// The package graph's alias tables, kept because every O(workspace)
    /// rebuild runs `build_namespaces` again and needs them.
    pub aliases: bone_core::modules::NsAliases,
    /// The warm modules lowered and inferred alone, before any user file or
    /// namespace tree entered the workspace, which a build starts from
    /// instead of re-lowering the stdlib -- when that equals lowering them
    /// with the user files (see [`WarmBase`]).
    warm_base: WarmBase,
    /// The names through which a user file can reach a warm module.
    warm_names: bone_semantic::stdlib_base::WarmNames,
    /// The warm modules' namespace contributions (the `std` root), computed
    /// once: warm ASTs are immutable. User files keep theirs on `UserFile`.
    warm_contributions: Vec<FragmentContribution>,
    /// Keep host package facts even when a new file fails to enter the session.
    declared_facts: FxHashMap<String, (Option<PackageTag>, GlobalPolicy)>,
}

/// A file entering a session, carrying the facts its package supplies.
#[derive(Clone, Debug)]
pub struct SessionFile {
    pub path: String,
    pub text: String,
    pub package: Option<PackageTag>,
    /// The owning package's mutable-global policy, which every re-parse keeps.
    pub global_policy: GlobalPolicy,
}

impl SessionFile {
    /// Retain the source loader's package facts across reparses.
    pub fn from_source(source: &rock_core::SourceFile) -> Self {
        Self {
            path: source.file.to_id_path(),
            text: source.contents.clone(),
            package: source.package.clone(),
            global_policy: source.global_policy,
        }
    }

    /// A source with no owning manifest and no explicit package.
    pub fn standalone(path: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            text: text.into(),
            package: None,
            global_policy: GlobalPolicy::Any,
        }
    }
}

#[derive(Clone, Debug)]
pub struct UserFile {
    text_hash: u64,
    item_prints: Vec<ItemPrint>,
    pub text: String,
    /// Keep macro signatures for editor queries after expansion.
    pub macros: Vec<MacroDef>,
    /// The package that owns this file, from the package graph.
    package: Option<PackageTag>,
    /// Retained across full and single-item reparses.
    global_policy: GlobalPolicy,
    /// Recovery keeps the partial tree queryable while diagnostics retain its syntax errors.
    syntax_errors: Vec<CompilerError>,
    /// Hash of what this file contributes to the namespace tree: the `mod` header, every `use`,
    /// and each definition's name, kind and visibility (see [`scope_hash`]).
    scope_hash: u64,
    /// What this file contributes to the namespace tree, kept so a scope
    /// edit re-assembles the tree without re-parsing any other file.
    pub contribution: FragmentContribution,
}

/// One parameter of a macro definition, rendered at parse time (the AST is
/// transient, so the type is stored as surface text rather than an arena index).
#[derive(Clone, Debug)]
pub struct MacroParam {
    pub name: Ustr,
    pub is_variadic: bool,
    pub ty: String,
}

/// A macro definition (`macro foo(...) { ... }`) recovered at parse time before
/// expansion drops it from the cooked module. Holds what navigation/ctags need:
/// name, definition name-span, parameter signature, and return type.
#[derive(Clone, Debug)]
pub struct MacroDef {
    pub name: Ustr,
    /// The macro NAME's span -- shifted incrementally on fast body edits.
    pub span: Span,
    pub params: Vec<MacroParam>,
    pub ret: Option<String>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ItemKindTag {
    Fn,
    Struct,
    Enum,
    Newtype,
    Other,
}

#[derive(Clone, Debug)]
struct ItemPrint {
    kind: ItemKindTag,
    name: Ustr,
    iface_hash: u64,
    body_hash: Option<u64>,
    offset: usize,
    /// Full item span (covers leading annotations) -- the incremental body-edit fast path
    /// slices `text[item_span]` to re-parse just this item, and reads its line/col as the
    /// `parse_one_item` base.
    item_span: Span,
    /// Byte range of the function's body BLOCK (Rock-bodied fns only) -- an edit
    /// strictly inside it is a pure body edit. `None` for non-fn / lua / extern /
    /// macro items, which the fast path never takes.
    body_range: Option<(usize, usize)>,
}

enum FileDiff {
    BodiesOnly(Vec<usize>),
    Interface,
}

/// Reject declaration collisions before expansion removes macro definitions.
fn reject_macro_name_collisions(modules: &ModuleSet) -> Result<(), CompilerError> {
    check_macro_name_uniqueness(modules.iter())
        .into_iter()
        .next()
        .map_or(Ok(()), Err)
}

/// See [`UserFile::scope_hash`].
fn scope_hash(contribution: &FragmentContribution) -> u64 {
    use std::hash::{Hash, Hasher};

    let mut h = rustc_hash::FxHasher::default();
    if let Some(header) = &contribution.header {
        header
            .path
            .iter()
            .map(|ident| ident.name.as_str())
            .collect::<Vec<_>>()
            .hash(&mut h);
        hash_visibility(header.vis.as_ref(), &mut h);
    }
    scope_items(&contribution.items, 0, &mut h);
    h.finish()
}

/// What namespace assembly reads from a cooked contribution. Signatures,
/// fields and bodies are left out -- an edit to them keeps every DefKey and
/// stays on the interface path.
fn scope_items(
    items: &[bone_semantic::semantic_passes::ScopeItem],
    depth: usize,
    h: &mut rustc_hash::FxHasher,
) {
    use bone_semantic::semantic_passes::ScopeItem;
    use std::hash::Hash;

    for item in items {
        match item {
            ScopeItem::Module {
                name, vis, items, ..
            } => {
                (depth, "mod", name.as_str()).hash(h);
                hash_visibility(vis.as_ref(), h);
                scope_items(items, depth + 1, h);
            }
            ScopeItem::Use(decl) => {
                (depth, "use").hash(h);
                hash_visibility(decl.vis.as_ref(), h);
                hash_use_tree(&decl.tree, h);
            }
            ScopeItem::Def {
                name, kind, vis, ..
            } => {
                (depth, *kind, name.as_str()).hash(h);
                hash_visibility(vis.as_ref(), h);
            }
        }
    }
}

fn hash_use_tree(tree: &skull_parser::UseTree, h: &mut rustc_hash::FxHasher) {
    use skull_parser::{UseSuffix, UseTree};
    use std::hash::Hash;

    match tree {
        UseTree::Path {
            from_root,
            segments,
            kind,
        } => {
            from_root.hash(h);
            segments
                .iter()
                .map(|ident| ident.name.as_str())
                .collect::<Vec<_>>()
                .hash(h);
            match kind {
                UseSuffix::Bind => 0u8.hash(h),
                UseSuffix::BindAs(alias) => {
                    1u8.hash(h);
                    alias.name.as_str().hash(h);
                }
                UseSuffix::Glob => 2u8.hash(h),
                UseSuffix::Operator { op, alias } => {
                    3u8.hash(h);
                    op.name.as_str().hash(h);
                    alias.as_ref().map(|ident| ident.name.as_str()).hash(h);
                }
                UseSuffix::Nested(children) => {
                    4u8.hash(h);
                    children.len().hash(h);
                    for child in children {
                        hash_use_tree(child, h);
                    }
                }
            }
        }
    }
}

fn hash_visibility(vis: Option<&skull_parser::Visibility>, h: &mut rustc_hash::FxHasher) {
    use skull_parser::Visibility;
    use std::hash::Hash;

    match vis {
        None => 0u8.hash(h),
        Some(Visibility::Private) => 1u8.hash(h),
        Some(Visibility::Pub) => 2u8.hash(h),
        Some(Visibility::PubSuper) => 3u8.hash(h),
        Some(Visibility::PubPackage) => 4u8.hash(h),
        Some(Visibility::PubIn(path)) => {
            5u8.hash(h);
            for seg in path {
                seg.name.as_str().hash(h);
            }
        }
    }
}

fn item_prints(a: &Ast, text: &str) -> Vec<ItemPrint> {
    use std::hash::{Hash, Hasher};

    a.items
        .iter()
        .filter_map(|item| {
            let (start, stop) = item.span.offset_range();
            debug_assert!(stop <= text.len(), "item span out of bounds");
            let mut h = rustc_hash::FxHasher::default();
            for ann in &item.annotations {
                let (a, b) = ann.span.offset_range();
                text[a..b].hash(&mut h);
            }
            let (kind, name, body_range) = match &item.kind {
                ItemKind::Function(f) => (
                    ItemKindTag::Fn,
                    f.name.name,
                    match f.body {
                        FunctionBody::Rock(body) if !f.is_macro => {
                            Some(a.b(&body).span.offset_range())
                        }
                        _ => None,
                    },
                ),
                ItemKind::Struct(s) => (ItemKindTag::Struct, s.name.name, None),
                ItemKind::Enum(e) => (ItemKindTag::Enum, e.name.name, None),
                ItemKind::Newtype(n) => (ItemKindTag::Newtype, n.name.name, None),
                ItemKind::Decorator(d) => (ItemKindTag::Newtype, d.name.name, None),
                // Const values participate in hot reload; mutable globals keep their
                // live value, so their entire declaration remains interface input.
                ItemKind::Global(g) => (
                    ItemKindTag::Other,
                    g.name.name,
                    g.value
                        .filter(|_| g.is_const)
                        .map(|value| a.e(&value).span.offset_range()),
                ),
                ItemKind::MacroInvocation { name, .. } => (ItemKindTag::Other, name.name, None),
                ItemKind::CompilerDirective(_) | ItemKind::Use(_) | ItemKind::Module(_) => {
                    (ItemKindTag::Other, Ustr::from(""), None)
                }
                ItemKind::Comment(_) => return None,
            };
            let body_hash = if let Some((bs, be)) = body_range {
                debug_assert!(
                    start <= bs && be <= stop,
                    "{}",
                    if kind == ItemKindTag::Fn {
                        "block escapes item span"
                    } else {
                        "const init escapes item span"
                    }
                );
                text[start..bs].hash(&mut h);
                text[be..stop].hash(&mut h);
                Some(fingerprint(&text[bs..be]))
            } else {
                text[start..stop].hash(&mut h);
                None
            };
            Some(ItemPrint {
                kind,
                name,
                iface_hash: h.finish(),
                body_hash,
                offset: start,
                item_span: item.span,
                body_range,
            })
        })
        .collect()
}

fn classify(old: &[ItemPrint], new: &[ItemPrint]) -> FileDiff {
    if old.len() != new.len() {
        return FileDiff::Interface;
    }

    let mut changed = Vec::new();

    for (i, (o, n)) in old.iter().zip(new).enumerate() {
        if o.kind != n.kind || o.name != n.name || o.iface_hash != n.iface_hash {
            return FileDiff::Interface;
        }

        if o.body_hash != n.body_hash {
            changed.push(i);
        }
    }

    FileDiff::BodiesOnly(changed)
}

fn name_shapes(prints: &[ItemPrint]) -> FxHashMap<Ustr, u64> {
    let mut by_name: FxHashMap<Ustr, Vec<u64>> = FxHashMap::default();

    for p in prints {
        if p.name.is_empty() {
            continue;
        }

        by_name.entry(p.name).or_default().push(p.iface_hash);
    }

    by_name
        .into_iter()
        .map(|(n, mut hs)| {
            hs.sort();
            (n, fingerprint(&hs))
        })
        .collect()
}

/// Parse `text` and tag the module with its owning package. Every session parse
/// goes through here (or [`Session::parse_stored`], which reads the facts back
/// off the file).
fn parse_with_facts(
    text: &str,
    file: Ustr,
    package: Option<PackageTag>,
    global_policy: GlobalPolicy,
) -> Result<Ast, CompilerError> {
    let mut module = skull_parser::parse_module(text, file)?;
    module.package = package;
    module.global_policy = global_policy;
    Ok(module)
}

fn cook_and_contribute(
    modules: &mut [Ast],
    registry: &MacroRegistry,
) -> Result<Vec<FragmentContribution>, CompilerError> {
    modules
        .par_iter_mut()
        .map(|ast| cook_one(ast, registry))
        .collect::<Vec<_>>()
        .into_iter()
        .collect::<Result<(), _>>()?;
    Ok(modules.iter().map(contribute).collect())
}

impl Session {
    /// Expansion erases macro declarations; editor queries read user and warm signatures here.
    pub fn all_macro_defs(&self) -> impl Iterator<Item = (Ustr, &MacroDef)> {
        self.files
            .iter()
            .flat_map(|(path, uf)| uf.macros.iter().map(move |d| (Ustr::from(path), d)))
            .chain(self.warm_macros.iter().map(|(f, d)| (*f, d)))
    }

    /// `aliases` are the host package graph's dependency-alias tables, the same
    /// input `semantic_pass_with_aliases` takes. A standalone-only session
    /// passes the default (empty) table.
    pub fn new(
        warm: impl Into<Arc<[Ast]>>,
        files: Vec<SessionFile>,
        aliases: &bone_core::modules::NsAliases,
    ) -> Result<Self, CompilerError> {
        let warm: Arc<[Ast]> = warm.into();
        let mut ws = Workspace::default();

        let warm_mids: FxHashSet<ModuleId> =
            warm.iter().map(|m| ws.intern_module(m.file)).collect();

        let warm_macros: Vec<(Ustr, MacroDef)> = warm
            .iter()
            .flat_map(|m| macro_defs(m).into_iter().map(|d| (m.file, d)))
            .collect();

        let mut session = Session {
            ws,
            warm,
            warm_mids,
            paths: Vec::new(),
            files: FxHashMap::default(),
            dependents: Default::default(),
            module_dep_names: Default::default(),
            dependents_iface: Default::default(),
            module_iface_dep_names: Default::default(),
            registry: Default::default(),
            warm_macros,
            aliases: aliases.clone(),
            warm_base: WarmBase::Unbuilt,
            warm_names: Default::default(),
            warm_contributions: Vec::new(),
            declared_facts: FxHashMap::default(),
        };
        let mut cooked_warm = session.warm.to_vec();
        let warm_registry = collect_registry(cooked_warm.iter());
        session.warm_contributions = cook_and_contribute(&mut cooked_warm, &warm_registry)
            .unwrap_or_else(|_| session.warm.iter().map(contribute).collect());
        session.warm_names =
            bone_semantic::stdlib_base::warm_names(&session.warm, &session.warm_contributions);

        // Parse each file once for its prints/macros AND keep the module to feed
        // lowering directly -- avoids a second parse via `combined_unexpanded`,
        // which re-parses (the AST is transient and not persisted).
        let mut user_modules: Vec<Ast> = Vec::new();
        for SessionFile {
            path,
            text,
            package,
            global_policy,
        } in files
        {
            if session.files.contains_key(&path) {
                comp_error!(
                    Span::unknown(),
                    "bone session: duplicate file `{}` in initial set",
                    path
                );
            }

            let module =
                parse_with_facts(&text, ustr(path.as_str()), package.clone(), global_policy)?;

            // Looked up, not interned: user modules take their ids when they
            // lower, after the warm modules, so a warm-alone build
            // (`warm_base`) interns the warm modules alone.
            if session
                .ws
                .module_ids
                .get(&module.file)
                .is_some_and(|mid| session.warm_mids.contains(mid))
            {
                // A stdlib file present in the initial set IS the warm module --
                // skip the user copy (adding it would self-collide on every
                // stdlib type); the warm build already covers it.
                continue;
            }

            let prints = item_prints(&module, &text);
            let macros = macro_defs(&module);
            let contribution = contribute(&module);
            let scope = scope_hash(&contribution);

            session.files.insert(
                path.clone(),
                UserFile {
                    text_hash: fingerprint(&text),
                    item_prints: prints,
                    text,
                    macros,
                    package,
                    global_policy,
                    syntax_errors: module.recovery_errors.clone(),
                    scope_hash: scope,
                    contribution,
                },
            );

            session.paths.push(path);
            user_modules.push(module);
        }

        let unexpanded = ModuleSet::new(session.warm.clone(), user_modules);
        session.registry = collect_registry(unexpanded.iter());
        reject_macro_name_collisions(&unexpanded)?;

        let registry = session.registry.clone();
        let (ws, _) = session.build(unexpanded, &registry)?;
        session.ws = ws;
        session.rebuild_dependents();

        Ok(session)
    }

    /// Re-parse every user file from its stored text and prepend the (immutable, pre-parsed)
    /// warm modules -- the un-expanded module set the full-rebuild path (`rebuild_users`) feeds
    /// to `expand_and_desugar`.
    pub fn combined_unexpanded(&self) -> Result<ModuleSet, CompilerError> {
        let users = self
            .paths
            .iter()
            .map(|path| self.parse_stored(ustr(path)))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ModuleSet::new(self.warm.clone(), users))
    }

    /// The package facts recorded for `path`: a held file's own, else what the host declared
    /// for it.
    fn facts_of(&self, path: &str) -> (Option<PackageTag>, GlobalPolicy) {
        match self.files.get(path) {
            Some(f) => (f.package.clone(), f.global_policy),
            None => self
                .declared_facts
                .get(path)
                .cloned()
                .unwrap_or((None, GlobalPolicy::Any)),
        }
    }

    /// Parse `text` as a buffer of `path`, with the package facts a
    /// stored copy of it carries. A recovery reparse of an editor buffer goes
    /// through here: parsed with the same package facts as the stored file.
    pub fn parse_buffer(&self, path: &str, text: &str) -> Result<Ast, CompilerError> {
        let (package, global_policy) = self.facts_of(path);
        parse_with_facts(text, ustr(path), package, global_policy)
    }

    /// Declare the owning package of a file that enters the session after [`Session::new`] --
    /// the editor opening a file the batch load did not see.
    pub fn declare_file_facts(
        &mut self,
        path: &str,
        package: Option<PackageTag>,
        global_policy: GlobalPolicy,
    ) -> Result<(), CompilerError> {
        if let Some(held) = self.files.get(path)
            && (held.package != package || held.global_policy != global_policy)
        {
            comp_error!(
                Span::unknown(),
                "bone session: `{}` is held as {:?}; it cannot become {:?} without a new session",
                path,
                held.package,
                package
            );
        }
        self.declared_facts
            .insert(path.to_string(), (package, global_policy));
        Ok(())
    }

    /// Re-parse `path` from its stored text with its own facts.
    fn parse_stored(&self, path: Ustr) -> Result<Ast, CompilerError> {
        let (package, global_policy) = self.facts_of(path.as_str());
        parse_with_facts(
            &self.files[path.as_str()].text,
            path,
            package,
            global_policy,
        )
    }

    pub fn workspace(&self) -> &Workspace {
        &self.ws
    }

    /// Query a temporary file variant through the normal update tiers, then restore
    /// its original text or remove the added file. The callback must not unwind.
    pub fn with_file_variant<R>(
        &mut self,
        path: &str,
        variant: &str,
        f: impl FnOnce(&Workspace) -> R,
    ) -> Result<R, CompilerError> {
        let original = self.files.get(path).map(|uf| uf.text.clone());
        let result = self.update_file(path, variant).map(|_| f(&self.ws));
        if let Some(text) = original {
            self.update_file(path, &text)?;
        } else if result.is_ok() {
            self.remove_file(path)?;
        }
        result
    }

    pub fn update_file(&mut self, path: &str, text: &str) -> Result<UpdateResult, CompilerError> {
        // Reset the work counters so callers can read EXACTLY how many bodies
        // this update re-inferred (check_module_incremental bumps it) and
        // re-lowered (the lowering passes bump it).
        self.ws
            .reinferred_body_count
            .store(0, std::sync::atomic::Ordering::Relaxed);
        self.ws
            .relowered_body_count
            .store(0, std::sync::atomic::Ordering::Relaxed);
        let hash = fingerprint(text);

        if let Some(existing) = self.files.get(path) {
            if existing.text_hash == hash {
                return Ok(UpdateResult::default());
            }
        }

        if let Some(result) = self.try_incremental_body_edit(path, text, hash)? {
            return Ok(result);
        }

        let (package, global_policy) = self.facts_of(path);
        let module = parse_with_facts(text, ustr(path), package.clone(), global_policy)?;
        let mid = self.ws.intern_module(module.file);
        if self.warm_mids.contains(&mid) {
            // Opening a warm (stdlib) file: it IS the warm module, not a second copy.
            return Ok(UpdateResult::default());
        }

        let new_prints = item_prints(&module, text);
        let new_macros = macro_defs(&module);
        let tracked = self.files.contains_key(path);
        let only_functions =
            tracked && only_functions_changed(&self.files[path].item_prints, &new_prints);

        let changed: FxHashSet<Ustr> = match self.files.get(path) {
            Some(old) => {
                let old = name_shapes(&old.item_prints);
                let new = name_shapes(&new_prints);
                old.keys()
                    .chain(new.keys())
                    .filter(|name| old.get(*name) != new.get(*name))
                    .copied()
                    .collect()
            }
            None => FxHashSet::default(),
        };
        let touches_macros = tracked
            && (touches_macro(&self.files[path].macros, &new_macros, &changed)
                || self.defines_a_function_named_like_a_macro(&module));
        let new_contribution = if tracked && !touches_macros {
            let mut cooked = module.clone();
            cook_one(&mut cooked, &self.registry)?;
            contribute(&cooked)
        } else {
            contribute(&module)
        };

        let new_scope = scope_hash(&new_contribution);

        // An edit that changes what the namespace tree or the fragment scopes
        // are built from (`UserFile::scope_hash`) takes the scope path.
        let scope_changed = self
            .files
            .get(path)
            .is_some_and(|old| old.scope_hash != new_scope);

        let diff = (tracked && !module.items.is_empty() && !scope_changed)
            .then(|| classify(&self.files[path].item_prints, &new_prints));

        let next = UserFile {
            text_hash: hash,
            item_prints: new_prints,
            text: text.to_string(),
            macros: new_macros,
            package,
            global_policy,
            syntax_errors: module.recovery_errors.clone(),
            scope_hash: new_scope,
            contribution: new_contribution,
        };
        let span_only = matches!(&diff, Some(FileDiff::BodiesOnly(items)) if items.is_empty());
        let scope_path = tracked && scope_changed && !module.items.is_empty() && !touches_macros;
        self.apply_file(path, Some(next), span_only, |s| match diff {
            Some(FileDiff::BodiesOnly(changed)) => match changed.as_slice() {
                [idx] => s.rebuild_body_incremental(path, module, *idx, false),
                _ => s.rebuild_module(path, module, &changed),
            },
            Some(FileDiff::Interface) if !touches_macros => s.rebuild_interface(
                module,
                &changed,
                ScopeDelta::default(),
                UpdateTier::Interface,
                only_functions,
            ),
            _ if scope_path => s.rebuild_scope(module, &changed),
            _ => s.rebuild_users(),
        })
    }

    /// Whether `module` defines a top-level function under a name some macro
    /// of the session also has: the whole-program `check_macro_name_uniqueness`
    /// refuses that, and only the full rebuild runs it.
    fn defines_a_function_named_like_a_macro(&self, module: &Ast) -> bool {
        let macro_names: FxHashSet<Ustr> = self.all_macro_defs().map(|(_, d)| d.name).collect();
        if macro_names.is_empty() {
            return false;
        }
        module.items.iter().any(|item| {
            matches!(&item.kind, ItemKind::Function(f) if !f.is_macro && macro_names.contains(&f.name.name))
        })
    }

    /// Build from unexpanded inputs. Namespace assembly must precede expansion
    /// and lowering so files can resolve module paths and uses.
    fn build(
        &mut self,
        unexpanded: ModuleSet,
        registry: &MacroRegistry,
    ) -> Result<(Workspace, Vec<ModuleId>), CompilerError> {
        debug_assert!(Arc::ptr_eq(unexpanded.shared(), &self.warm));
        let warm_len = self.warm.len();
        // Read from the ASTs being built, not the stored `UserFile`s, so the
        // build is a function of the texts alone.
        let independent = unexpanded.rest().iter().all(|ast| {
            bone_semantic::stdlib_base::apart_from(&self.warm_names, ast, &contribute(ast))
        });
        let mut cooked_warm = self.warm.to_vec();
        let warm_contributions = cook_and_contribute(&mut cooked_warm, registry)?;
        let mut cooked_users = unexpanded.into_parts().1;
        let user_contributions = cook_and_contribute(&mut cooked_users, registry)?;
        let base = independent
            .then(|| self.warm_alone(&cooked_warm, &warm_contributions).cloned())
            .flatten();
        let (mut ws, lowered, first_user) = match base {
            Some(base) => (base, cooked_users, 0),
            None => {
                cooked_warm.extend(cooked_users);
                (Workspace::default(), cooked_warm, warm_len)
            }
        };
        let contributions: Vec<_> = warm_contributions[..first_user]
            .iter()
            .chain(&user_contributions)
            .collect();
        bone_semantic::semantic_passes::assemble_namespaces(
            &mut ws,
            &contributions,
            &self.aliases,
        )?;
        let mids = lower_and_infer(&mut ws, &lowered)?;
        // The same whole-program gate the batch path runs, so an export whose type closure is
        // not public fails in a session too rather than only under `rock compile`.
        bone_semantic::semantic_passes::pass_validate_api_closure(&ws)?;
        ws.phase_timings.clear();
        self.set_cooked_contributions(user_contributions, warm_contributions);
        Ok((ws, mids[first_user..].to_vec()))
    }

    /// The warm-alone workspace, built on first need; `None` when the warm
    /// modules alone are refused or report a diagnostic (see [`WarmBase`]).
    fn warm_alone(
        &mut self,
        cooked: &[Ast],
        contributions: &[FragmentContribution],
    ) -> Option<&Workspace> {
        if matches!(self.warm_base, WarmBase::Unbuilt) {
            let base = bone_semantic::stdlib_base::checked_alone(&self.warm).or_else(|| {
                let mut ws = Workspace::default();
                // The combined build decides whether a warm-only refusal is fatal.
                bone_semantic::semantic_passes::assemble_namespaces(
                    &mut ws,
                    &contributions.iter().collect::<Vec<_>>(),
                    &self.aliases,
                )
                .ok()?;
                lower_and_infer(&mut ws, cooked).ok()?;
                ws.phase_timings.clear();
                body_errors(&ws).is_empty().then_some(ws)
            });
            self.warm_base = base.map_or(WarmBase::Unusable, |ws| WarmBase::Clean(Box::new(ws)));
        }
        match &self.warm_base {
            WarmBase::Clean(ws) => Some(ws),
            _ => None,
        }
    }

    /// Every file's namespace contribution in module order: the warm modules,
    /// then the user files in tracking order -- the order `build_namespaces`
    /// reads a fresh build's modules in, so a rejection names the same file.
    fn contributions(&self) -> Vec<&FragmentContribution> {
        self.warm_contributions
            .iter()
            .chain(
                self.paths
                    .iter()
                    .map(|p| &self.files[p.as_str()].contribution),
            )
            .collect()
    }

    fn set_cooked_contributions(
        &mut self,
        user_contributions: Vec<FragmentContribution>,
        warm_contributions: Vec<FragmentContribution>,
    ) {
        for contribution in user_contributions {
            if let Some(user_file) = self.files.get_mut(contribution.file.as_str()) {
                user_file.scope_hash = scope_hash(&contribution);
                user_file.contribution = contribution;
            }
        }
        self.warm_contributions = warm_contributions;
        self.warm_names =
            bone_semantic::stdlib_base::warm_names(&self.warm, &self.warm_contributions);
    }

    /// Reassemble the namespace and use changed scope probes to find affected files.
    fn rebuild_scope(
        &mut self,
        module: Ast,
        changed: &FxHashSet<Ustr>,
    ) -> Result<UpdateResult, CompilerError> {
        let mut next = Workspace::default();
        bone_semantic::semantic_passes::assemble_namespaces(
            &mut next,
            &self.contributions(),
            &self.aliases,
        )?;

        let delta = ScopeDelta::between(&self.ws, &next);

        self.ws.ns.packages = next.ns.packages;
        self.ws.defs = next.defs;
        self.ws.fragment_scopes = next.fragment_scopes;

        self.rebuild_interface(module, changed, delta, UpdateTier::Scope, false)
    }

    /// User modules other than `except` whose lowering read a fact in `delta`.
    fn modules_reading(&self, delta: &ScopeDelta, except: ModuleId) -> FxHashSet<ModuleId> {
        if delta.is_empty() {
            return FxHashSet::default();
        }
        self.ws
            .user_modules(&self.warm_mids)
            .filter(|m| m.id != except)
            .filter(|m| {
                delta.fragments.contains(&m.file) || m.scope_probes.iter().any(|p| delta.hits(p))
            })
            .map(|m| m.id)
            .collect()
    }

    /// Re-lower modules whose scopes changed and re-infer callers of changed definitions.
    fn rebuild_interface(
        &mut self,
        module: Ast,
        changed: &FxHashSet<Ustr>,
        mut delta: ScopeDelta,
        tier: UpdateTier,
        only_functions: bool,
    ) -> Result<UpdateResult, CompilerError> {
        let file = module.file;
        let mid = self.ws.module_ids[&file];

        // Shape changes need inference; resolution, flavor, arity and defaults
        // can change how dependent signatures lower.
        let old_types = {
            use bone_semantic::semantic_passes::{NominalFacts, NominalFlavor};
            self.ws
                .struct_by_name
                .iter()
                .map(|(name, id)| (name, id.module, NominalFlavor::Struct))
                .chain(
                    self.ws
                        .enums_by_name
                        .iter()
                        .map(|(name, id)| (name, id.module, NominalFlavor::Enum)),
                )
                .chain(
                    self.ws
                        .newtypes_by_name
                        .iter()
                        .map(|(name, id)| (name, id.module, NominalFlavor::Newtype)),
                )
                .filter(|(_, owner, _)| *owner == mid)
                .map(|(name, _, flavor)| {
                    let generics = self.ws.type_generics.get(name);
                    (
                        *name,
                        NominalFacts {
                            flavor,
                            arity: generics.map_or(0, |g| g.arity),
                            defaults: generics.map(|g| g.defaults.clone()).unwrap_or_default(),
                        },
                    )
                })
                .collect()
        };
        let new_types = bone_semantic::semantic_passes::nominal_type_facts(
            &self.ws.fragment_scopes[&file],
            &module,
        );
        let changed_types = changed_keys(&old_types, &new_types);

        let mut to_relower: FxHashSet<ModuleId> = FxHashSet::default();
        for key in &changed_types {
            if let Some(mods) = self.dependents_iface.get(key) {
                to_relower.extend(mods.iter().copied().filter(|m| *m != mid));
            }
            // A body reads the same facts when it lowers a path through the
            // type (`E::V`, a newtype call, a type annotation), and records
            // the member it looked up.
            if let Some(meta) = self.ws.defs.get(key) {
                delta.members.insert((meta.package, meta.node, meta.name));
            }
        }
        to_relower.extend(self.modules_reading(&delta, mid));
        // Plus every module currently carrying an interface error, so a type that (re)appears
        // with this edit re-lowers its previously-dangling users and clears them.
        for m in self.ws.user_modules(&self.warm_mids) {
            if m.id != mid && !m.interface_errors.is_empty() {
                to_relower.insert(m.id);
            }
        }

        // Re-parse each closure module from its stored text (the edited module
        // uses the provided AST); cook + lower them together with the edited
        // module so intra-closure and closure->kept references resolve.
        let mut edited = module;
        cook_one(&mut edited, &self.registry)?;
        let mut asts: Vec<Ast> = Vec::with_capacity(to_relower.len() + 1);
        asts.push(edited);
        for &m in &to_relower {
            let path = self.ws.module(m).file;
            let mut a = self.parse_stored(path)?;
            cook_one(&mut a, &self.registry)?;
            asts.push(a);
        }

        // Callers keep type and item ids. Seed the retained type arena and check
        // that the item indexes stay stable before reusing their inference.
        let kept = (only_functions
            && to_relower.is_empty()
            && delta.is_empty()
            && changed_types.is_empty())
        .then(|| self.kept_type_arena(mid))
        .flatten()
        .map(|seed| (seed, item_names(self.ws.module(mid))));

        self.retain_modules(|m| m != mid && !to_relower.contains(&m));
        self.ws.phase_timings.clear();

        let (seeds, old_items) = match kept {
            Some((seed, items)) => (FxHashMap::from_iter([(mid, seed)]), Some(items)),
            None => (FxHashMap::default(), None),
        };
        let f_mids = lower_modules_seeded(&mut self.ws, &asts, seeds)?;
        let narrow = old_items.is_some_and(|items| items == item_names(self.ws.module(mid)));

        // Re-lowering can change any exported definition, so invalidate all its users.
        let mut reinfer_names: FxHashSet<Ustr> = changed.clone();
        for &m in &f_mids {
            let module = self.ws.module(m);
            if narrow {
                // A v1 fragment's callers record the DefKey, not the bare name.
                let bare: FxHashSet<&str> = changed.iter().map(|n| n.as_str()).collect();
                reinfer_names.extend(
                    module
                        .functions_by_name
                        .keys()
                        .filter(|key| bare.contains(key.rsplit("::").next().unwrap_or(key)))
                        .copied(),
                );
                continue;
            }
            reinfer_names.extend(exported_type_names(module));
            reinfer_names.extend(module.functions_by_name.keys().copied());
            reinfer_names.extend(module.global_by_name.keys().copied());
        }

        // Re-infer: body-dependents of `reinfer_names` (their TEXT is unchanged, so body-
        // granular reuse applies), every module with a stale inference error, AND every re-
        // lowered module (its arena shifted, so its cached inference -- already cleared by
        // `retain_modules` -- can't be reused).
        let mut to_reinfer: FxHashSet<ModuleId> = reinfer_names
            .iter()
            .flat_map(|n| self.dependents.get(n).into_iter().flatten().copied())
            .collect();
        for (m, inf) in self.ws.inferences_iter() {
            if !inf.errors.is_empty() {
                to_reinfer.insert(m);
            }
        }
        to_reinfer.extend(f_mids.iter().copied());

        // `ws.modules` is append-only (`retain_modules` only nulls inference
        // slots / prunes name maps, never shrinks the vec), and `ModuleId` is its
        // dense index -- so existence is an O(1) bounds check, not an O(modules)
        // scan per dependent.
        let ids: Vec<ModuleId> = to_reinfer
            .iter()
            .copied()
            .filter(|m| m.index() < self.ws.modules.len())
            .collect();

        // Reuse successful function checks whose dependencies did not change.
        let mut reused: FxHashSet<BodyId> = FxHashSet::default();
        let prevs: Vec<(ModuleId, InferenceResult)> = ids
            .iter()
            .map(|id| {
                let cached = self
                    .ws
                    .inferences
                    .get_mut(id.index())
                    .and_then(|o| o.take())
                    .unwrap_or_default();
                let errored: FxHashSet<BodyOwner> = cached.errors.keys().copied().collect();
                let mut prev = InferenceResult::default();
                for (owner, bi) in cached.per_fn {
                    if !errored.contains(&owner) && bi.deps.is_disjoint(&reinfer_names) {
                        // Globals, decorators and struct defaults re-check
                        // regardless of `prev`, so only a function is reused.
                        if matches!(owner, BodyOwner::Function(_)) {
                            reused.insert((*id, owner));
                        }
                        prev.per_fn.insert(owner, bi);
                    }
                }
                (*id, prev)
            })
            .collect();
        // check_module_incremental reads ws immutably (declared signatures from
        // the HIR, not inference results), so dependents re-check in parallel --
        // mirroring the full pipeline's `infer_modules`.
        let results: Vec<_> = prevs
            .into_par_iter()
            .map(|(id, prev)| {
                Ok::<_, CompilerError>((
                    id,
                    check_module_incremental(&self.ws, id, self.ws.module(id), prev)?,
                ))
            })
            .collect::<Result<_, _>>()?;

        for (id, inf) in results {
            self.ws.set_inference(id, inf);
        }

        // The same whole-program gate `rebuild_users` runs: a signature edit
        // alone can export a type whose closure is not public. It reads only
        // definitions and their signatures, and is a no-op with no v1 package.
        bone_semantic::semantic_passes::pass_validate_api_closure(&self.ws)?;

        self.rebuild_dependents();

        // `ids` already contains `f_mids`, so it covers `reinferred`, minus the bodies reused
        // above.
        let mut reinferred = self.collect_reinferred(&ids);
        reinferred.retain(|body| !reused.contains(body));
        let relowered = self.collect_reinferred(&f_mids);

        Ok(UpdateResult {
            tier,
            reinferred,
            relowered,
            ..UpdateResult::default()
        })
    }

    pub fn remove_file(&mut self, path: &str) -> Result<UpdateResult, CompilerError> {
        self.apply_file(path, None, false, Self::rebuild_users)
    }

    pub fn diagnostics(&self) -> Vec<CompilerError> {
        let mut errs = body_errors(&self.ws);
        // Recovered syntax errors from each file's total parse. The compile path
        // rejects on these; the interactive path keeps the partial tree AND shows
        // them here, so a broken file gets both best-effort features and a squiggle.
        errs.extend(
            self.files
                .values()
                .flat_map(|f| f.syntax_errors.iter().cloned()),
        );
        errs
    }

    pub fn is_tracked(&self, path: &str) -> bool {
        self.files.contains_key(path)
    }

    pub fn tracked_paths(&self) -> Vec<&str> {
        self.paths.iter().map(|s| s.as_str()).collect()
    }
    /// Evict every module `keep` rejects, except permanent ones: the builtin
    /// module owns the builtin type nodes and outlives every eviction.
    pub fn retain_modules(&mut self, keep: impl Fn(ModuleId) -> bool) {
        let keep = |mid: ModuleId| Workspace::is_permanent_module(mid) || keep(mid);
        self.module_iface_dep_names.retain(|mid, _| keep(*mid));
        for (i, slot) in self.ws.inferences.iter_mut().enumerate() {
            if !keep(ModuleId::new(i as u32)) {
                *slot = None;
            }
        }

        self.ws.struct_by_name.retain(|_, id| keep(id.module));
        self.ws.enums_by_name.retain(|_, id| keep(id.module));
        self.ws.newtypes_by_name.retain(|_, id| keep(id.module));

        // The arity/defaults facts owned by evicted modules go with them; a
        // re-lowered module re-inserts its own. Facts owned by kept modules
        // stay, so a kept first definer still wins over a re-lowered duplicate.
        self.ws.type_generics.retain(|_, entry| keep(entry.owner));

        self.ws.functions_by_name.retain(|_, fns| {
            fns.retain(|(mid, _)| keep(*mid));
            !fns.is_empty()
        });

        self.ws.globals_by_name.clear();
        // Rebuild both global indexes so a removed const stops restricting assignments.
        self.ws.const_globals.clear();

        // Skip evicted
        for module in self.ws.modules.iter().filter(|m| keep(m.id)) {
            for (_, g) in module.globals.iter() {
                self.ws.globals_by_name.insert(g.name, g.ty);
                if g.is_const {
                    self.ws.const_globals.insert(g.name);
                }
            }
        }
    }

    pub fn rebuild_users(&mut self) -> Result<UpdateResult, CompilerError> {
        // Build off to the side. A failed lowering may intern ids without inserting modules.
        let unexpanded = self.combined_unexpanded()?;
        let registry = collect_registry(unexpanded.iter());
        reject_macro_name_collisions(&unexpanded)?;
        let (mut ws, user_mids) = self.build(unexpanded, &registry)?;
        // Host-resolved build variables, set once on the workspace when the
        // session was made; nothing in this rebuild derives them again.
        ws.comptime_inputs = self.ws.comptime_inputs.clone();

        // Commit -- past this point nothing fails, so `self.ws` never observes a
        // partially-interned workspace.
        self.ws = ws;
        self.module_iface_dep_names.clear();
        self.registry = registry;

        let reinferred = self.collect_reinferred(&user_mids);
        let relowered = reinferred.clone();

        self.ws.phase_timings.clear();

        self.rebuild_dependents();

        Ok(UpdateResult {
            tier: UpdateTier::FullUser,
            reinferred,
            relowered,
            ..UpdateResult::default()
        })
    }

    /// Full O(workspace) rebuild of both dep indices. For the full-rebuild paths
    /// (`Session::new`, `rebuild_users`, `rebuild_module`, `rebuild_interface`);
    /// a single-body edit uses `update_dependents_for` instead.
    fn rebuild_dependents(&mut self) {
        self.dependents.clear();
        self.module_dep_names.clear();
        self.dependents_iface.clear();

        for (mid, _) in self.ws.inferences_iter() {
            if !Workspace::is_user_module(mid, &self.warm_mids) {
                continue;
            }

            let names = self.module_deps(mid);
            for name in &names {
                self.dependents.entry(*name).or_default().insert(mid);
            }
            self.module_dep_names.insert(mid, names);
        }

        // Lowered types give interface dependencies even when inference failed.
        for module in self.ws.user_modules(&self.warm_mids) {
            let mid = module.id;
            let names = self
                .module_iface_dep_names
                .entry(mid)
                .or_insert_with(|| iface_dep_names(module));
            for &name in names.iter() {
                self.dependents_iface.entry(name).or_default().insert(mid);
            }
        }
    }

    /// A body edit changes only the edited module's outgoing dependencies.
    fn update_dependents_for(&mut self, mid: ModuleId) {
        if !Workspace::is_user_module(mid, &self.warm_mids) {
            return;
        }

        let new = self.module_deps(mid); // owned -> releases the `ws` borrow
        let old = self.module_dep_names.remove(&mid).unwrap_or_default();

        for name in old.difference(&new) {
            if let Some(set) = self.dependents.get_mut(name) {
                set.remove(&mid);
                if set.is_empty() {
                    self.dependents.remove(name);
                }
            }
        }
        for name in new.difference(&old) {
            self.dependents.entry(*name).or_default().insert(mid);
        }

        self.module_dep_names.insert(mid, new);
    }

    fn module_deps(&self, mid: ModuleId) -> FxHashSet<Ustr> {
        self.ws
            .inference(mid)
            .into_iter()
            .flat_map(|inf| inf.per_fn.values())
            .flat_map(|b| b.deps.iter().copied())
            .collect()
    }

    fn rebuild_module(
        &mut self,
        path: &str,
        ast: Ast,
        changed: &[usize],
    ) -> Result<UpdateResult, CompilerError> {
        let file = ast.file;
        let mid = self.ws.module_ids[&file];

        #[cfg(debug_assertions)]
        let old_iface = interface_snapshot(&self.ws, mid);

        let old_gens: FxHashMap<Idx<Function>, u64> = {
            let m = self.ws.module(mid);
            m.functions
                .iter()
                .filter_map(|(fid, _)| {
                    m.bodies
                        .get(&BodyOwner::Function(fid))
                        .map(|b| (fid, b.generation))
                })
                .collect()
        };

        let old_inf = self
            .ws
            .inferences
            .get_mut(mid.index())
            .and_then(|s| s.take());

        // Same text, same interface: starting from the same arena prefix puts
        // every entry back at the index cached callers hold.
        let seeds = {
            let module = self.ws.module(mid);
            let kept = module.ty_arena_live_start as usize;
            if kept == 0 {
                FxHashMap::default()
            } else {
                let arena = module.ty_arena.values().take(kept).cloned().collect();
                let seed = TypeArenaSeed {
                    arena,
                    var_high_water: module.lower_var_floor,
                };
                FxHashMap::from_iter([(mid, seed)])
            }
        };

        self.retain_modules(|m| m != mid);
        self.ws.phase_timings.clear();

        let mut edited = ast;
        cook_one(&mut edited, &self.registry)?;
        lower_modules_seeded(&mut self.ws, &[edited], seeds)?;

        let module = self.ws.module_mut(mid);
        let prints = &self.files[path].item_prints;
        let reinferred: Vec<_> = changed
            .iter()
            .filter_map(|&i| Some((mid, module.body_at(prints.get(i)?.offset)?)))
            .collect();
        let changed_owners: FxHashSet<_> = reinferred.iter().map(|&(_, owner)| owner).collect();
        for (fid, _) in module.functions.iter() {
            let owner = BodyOwner::Function(fid);
            if let Some(body) = module.bodies.get_mut(&owner) {
                let old = old_gens.get(&fid).copied().unwrap_or(0);
                body.generation = old + u64::from(changed_owners.contains(&owner));
            }
        }

        let timer = Instant::now();
        let result = check_module_incremental(
            &self.ws,
            mid,
            self.ws.module(mid),
            old_inf.unwrap_or_default(),
        )?;
        self.ws.set_inference(mid, result);
        self.ws.log_time("infer functions", timer);

        #[cfg(debug_assertions)]
        debug_assert_eq!(
            old_iface,
            interface_snapshot(&self.ws, mid),
            "relower of `{path}` changed the module interface, classify mis-tiered an interface edit"
        );

        self.rebuild_dependents();
        let relowered = self.collect_reinferred(&[mid]);

        Ok(UpdateResult {
            tier: UpdateTier::BodyOnly,
            reinferred,
            relowered,
            ..UpdateResult::default()
        })
    }

    /// Reparse just one item when the edit stays within its function body.
    fn try_incremental_body_edit(
        &mut self,
        path: &str,
        new_text: &str,
        new_hash: u64,
    ) -> Result<Option<UpdateResult>, CompilerError> {
        let Some(old) = self.files.get(path) else {
            return Ok(None);
        };
        // A partial reparse cannot tell whether an old recovery error was fixed.
        if !old.syntax_errors.is_empty() {
            return Ok(None);
        }
        let (start, end, byte_delta) = byte_diff(&old.text, new_text);
        let Some(idx) = locate_body_edit(&old.item_prints, start, end) else {
            return Ok(None);
        };
        let print = &old.item_prints[idx];
        let span = print.item_span;
        let (so, eo) = span.offset_range();
        let new_end = (eo as isize + byte_delta) as usize;
        if new_end > new_text.len() || so > new_end {
            return Ok(None);
        }
        let Ok(mut mini) = skull_parser::parse_one_item(
            &new_text[so..new_end],
            ustr(path),
            so,
            span.line_range().0,
            span.col_range().0,
        ) else {
            return Ok(None);
        };
        mini.package = old.package.clone();
        mini.global_policy = old.global_policy;
        // The parse is total now; a recovered (partial) item must NOT be installed
        // in place. Bail to the full reparse, which keeps the best-effort tree and
        // records the recovery error as a diagnostic.
        if !mini.recovery_errors.is_empty() {
            return Ok(None);
        }
        cook_one(&mut mini, &self.registry)?;

        // The re-parsed item must be exactly one function with the SAME name and
        // interface; otherwise the "body edit" actually moved the interface and
        // the full path must classify/route it.
        let mut mini_prints = item_prints(&mini, new_text);
        if mini_prints.len() != 1 {
            return Ok(None);
        }
        let np = mini_prints.pop().unwrap();
        if np.kind != ItemKindTag::Fn || np.name != print.name || np.iface_hash != print.iface_hash
        {
            return Ok(None);
        }
        // The item parse stops at the end of the function; text left after it
        // in the edited range is a new item the full path must see.
        if np.item_span.offset_range().1 != new_end {
            return Ok(None);
        }

        // Resolve the HIR body + spans (non-mutating). Bail if anything is off.
        let Some(&mid) = self.ws.module_ids.get(&ustr(path)) else {
            return Ok(None);
        };
        let Some(owner) = self.ws.module(mid).body_at(so) else {
            return Ok(None);
        };
        let BodyOwner::Function(fid) = owner else {
            return Ok(None);
        };
        let old_fn_span = self.ws.module(mid).functions[fid].span;
        let item = &mini.items[0];
        let ItemKind::Function(f) = &item.kind else {
            return Ok(None);
        };
        let item_shift = TextEditDelta {
            file: ustr(path),
            at: eo,
            byte_delta,
            line_delta: f.span.line_range().1 as isize - old_fn_span.line_range().1 as isize,
        };
        let mut macros = old.macros.clone();
        for md in &mut macros {
            item_shift.shift_span(&mut md.span);
        }
        let next = UserFile {
            text_hash: new_hash,
            item_prints: build_shifted_prints(old.item_prints.clone(), idx, np, &item_shift),
            text: new_text.to_string(),
            macros,
            package: old.package.clone(),
            global_policy: old.global_policy,
            syntax_errors: Vec::new(),
            scope_hash: old.scope_hash,
            contribution: old.contribution.clone(),
        };
        self.apply_file(path, Some(next), false, |s| {
            s.rebuild_body_incremental(path, mini, idx, true)
        })
        .map(Some)
    }

    fn rebuild_body_incremental(
        &mut self,
        path: &str,
        ast: Ast,
        changed_idx: usize,
        cooked: bool,
    ) -> Result<UpdateResult, CompilerError> {
        let file = ast.file;
        let mid = self.ws.module_ids[&file];

        // New start of the edited item: equal to its old start, so bdy_at of old still
        // resolves to the right owner
        let new_start = self.files[path].item_prints[changed_idx].offset;

        let owner = match self.ws.module(mid).body_at(new_start) {
            Some(o) => o,
            None => return self.rebuild_module(path, ast, &[changed_idx]),
        };

        // Only functions take the fast path (for now)
        // TODO: also handle globals
        let BodyOwner::Function(fid) = owner else {
            return self.rebuild_module(path, ast, &[changed_idx]);
        };

        let old_fn_span = self.ws.module(mid).functions[fid].span;

        // Extract the new fn span as an owned value so the borrow into `ast`
        // ends before the fallback may move `ast` into `rebuild_module`.
        let new_fn_span_opt = ast
            .items
            .iter()
            .find(|it| it.span.offset_range().0 == new_start)
            .and_then(|it| match &it.kind {
                ItemKind::Function(nf) => Some(nf.span),
                _ => None,
            });
        let new_fn_span = match new_fn_span_opt {
            Some(s) => s,
            None => return self.rebuild_module(path, ast, &[changed_idx]),
        };

        // The threaded full-module AST is not persisted, so it is cooked in
        // place.
        let mut edited = ast;
        let t_cook = Instant::now();
        if !cooked {
            cook_one(&mut edited, &self.registry)?;
        }
        let cook_dur = t_cook.elapsed();

        // The fallback needs a fresh AST because cooking twice would repeat desugaring.
        let item = edited.items.iter().find_map(|it| match &it.kind {
            ItemKind::Function(f) if it.span.offset_range().0 == new_start => Some((it, f)),
            _ => None,
        });
        let Some((item, f)) = item else {
            let m = self.parse_stored(ustr(path))?;
            return self.rebuild_module(path, m, &[changed_idx]);
        };
        let owner = BodyOwner::Function(fid);
        let old_gen = self
            .ws
            .module(mid)
            .bodies
            .get(&owner)
            .map(|b| b.generation)
            .unwrap_or(0);

        let at = old_fn_span.offset_range().1;
        let byte_delta = new_fn_span.offset_range().1 as isize - at as isize;
        let line_delta = new_fn_span.line_range().1 as isize - old_fn_span.line_range().1 as isize;

        let old_inf = self
            .ws
            .inferences
            .get_mut(mid.index())
            .and_then(|s| s.take());
        self.ws.phase_timings.clear();

        let mut module = std::mem::take(self.ws.module_mut(mid));

        let shift_timer = Instant::now();
        span_shift::shift_module_after_edit(&mut module, owner, at, byte_delta, line_delta);
        self.ws.log_time("shift spans", shift_timer);

        let lower_timer = Instant::now();
        let installed = bone_semantic::lower::relower_function_in_place(
            &self.ws,
            &mut module,
            &edited,
            f,
            &item.annotations,
            fid,
        )?;
        if installed {
            if let Some(b) = module.bodies.get_mut(&owner) {
                b.generation = old_gen + 1;
            }
        }
        *self.ws.module_mut(mid) = module;
        self.ws.index_param_signature(mid, fid);
        self.ws.log_time("lower functions", lower_timer);

        let infer_timer = Instant::now();
        let result = check_module_incremental(
            &self.ws,
            mid,
            self.ws.module(mid),
            old_inf.unwrap_or_default(),
        )?;
        self.ws.set_inference(mid, result);
        self.ws.log_time("infer functions", infer_timer);

        let deps_timer = Instant::now();
        self.update_dependents_for(mid);
        self.ws.log_time("rebuild dependents", deps_timer);

        if !cooked {
            self.ws.phase_timings.push(("cook one", cook_dur));
        }
        let body = vec![(mid, owner)];
        Ok(UpdateResult {
            tier: UpdateTier::BodyOnly,
            reinferred: body.clone(),
            relowered: body,
            ..UpdateResult::default()
        })
    }

    /// The module's whole type arena as a seed for its next lowering, or `None`
    /// once earlier lowerings have piled up enough dead entries that the edit
    /// should re-lower from empty (and re-infer every caller) to reclaim them.
    fn kept_type_arena(&self, mid: ModuleId) -> Option<TypeArenaSeed> {
        let module = self.ws.module(mid);
        let total = module.ty_arena.len();
        let live = total - module.ty_arena_live_start as usize;
        (total < MAX_KEPT_ARENA_GROWTH * live.max(256)).then(|| TypeArenaSeed {
            arena: module.ty_arena.clone(),
            var_high_water: module.lower_var_high_water,
        })
    }

    /// Install one file change. Every mutating update tier and removal shares
    /// this rollback, including failures after in-place lowering took HIR away.
    fn apply_file(
        &mut self,
        path: &str,
        next: Option<UserFile>,
        span_only: bool,
        rebuild: impl FnOnce(&mut Self) -> Result<UpdateResult, CompilerError>,
    ) -> Result<UpdateResult, CompilerError> {
        let removing = next.is_none();
        let previous = match next {
            Some(file) => self.files.insert(path.to_string(), file),
            None => self.files.remove(path),
        };
        let position = if removing {
            if previous.is_none() {
                return Ok(UpdateResult::default());
            }
            let pos = self
                .paths
                .iter()
                .position(|p| p == path)
                .expect("tracked file must be in path order");
            self.paths.remove(pos);
            Some(pos)
        } else {
            if previous.is_none() {
                self.paths.push(path.to_string());
            }
            None
        };
        match rebuild(self) {
            Ok(mut result) => {
                if result.tier == UpdateTier::BodyOnly && result.body_update.edit.is_none() {
                    result.body_update = BodyUpdate {
                        edit: previous
                            .as_ref()
                            .map(|file| text_edit_delta(path, &file.text, &self.files[path].text)),
                        semantic_bodies: if span_only {
                            Vec::new()
                        } else {
                            result.reinferred.clone()
                        },
                        span_only,
                    };
                }
                if !removing && result.tier != UpdateTier::NoChange {
                    bone_semantic::relocation::check_relocation(&mut self.ws);
                    bone_semantic::fn_identity::check_generic_fn_eq(&mut self.ws);
                }
                Ok(result)
            }
            Err(error) => {
                if let Some(file) = previous {
                    self.files.insert(path.to_string(), file);
                    if let Some(pos) = position {
                        self.paths.insert(pos, path.to_string());
                    }
                } else {
                    self.files.remove(path);
                    self.paths.retain(|p| p != path);
                }
                let action = if removing { "removal" } else { "update" };
                if let Err(revert_error) = self.rebuild_users() {
                    return Err(CompilerError::new_no_location(format!(
                        "bone session: rebuild of previous good state failed after a failed {action} of `{path}` - session is inconsistent: {} (original {action} error: {})",
                        revert_error.message, error.message
                    )));
                }
                Err(error)
            }
        }
    }

    fn collect_reinferred(&self, mids: &[ModuleId]) -> Vec<BodyId> {
        let mut reinferred = Vec::new();

        for mid in mids {
            if let Some(inf) = self.ws.inference(*mid) {
                reinferred.extend(inf.per_fn.keys().map(|o| (*mid, *o)));
                reinferred.extend(inf.errors.keys().map(|o| (*mid, *o)));
            }
        }

        reinferred
    }
}

/// The macro definitions in a freshly-parsed module (Functions with `is_macro`),
/// with their parameter/return types rendered to surface text while the AST (and
/// its type arena) is still alive -- expansion will drop them from the module.
fn macro_defs(module: &Ast) -> Vec<MacroDef> {
    module
        .items
        .iter()
        .filter_map(|i| {
            let ItemKind::Function(f) = &i.kind else {
                return None;
            };
            if !f.is_macro {
                return None;
            }
            let params = f
                .params
                .iter()
                .map(|p| MacroParam {
                    name: p.name.name,
                    is_variadic: p.is_variadic,
                    ty: module.type_surface(p.ty),
                })
                .collect();
            Some(MacroDef {
                name: f.name.name,
                span: f.name.span,
                params,
                ret: f.return_ty.map(|t| module.type_surface(t)),
            })
        })
        .collect()
}

/// Does any macro defined by the old or new version appear in the changed-name set?
fn touches_macro(
    old_macros: &[MacroDef],
    new_macros: &[MacroDef],
    changed: &FxHashSet<Ustr>,
) -> bool {
    old_macros
        .iter()
        .chain(new_macros)
        .any(|m| changed.contains(&m.name))
}

/// A byte diff may split UTF-8; callers compare offsets or slice byte arrays.
fn byte_diff(old: &str, new: &str) -> (usize, usize, isize) {
    let o = old.as_bytes();
    let n = new.as_bytes();
    let max_pre = o.len().min(n.len());
    let mut p = 0;
    while p < max_pre && o[p] == n[p] {
        p += 1;
    }
    let max_suf = (o.len() - p).min(n.len() - p);
    let mut s = 0;
    while s < max_suf && o[o.len() - 1 - s] == n[n.len() - 1 - s] {
        s += 1;
    }
    (p, o.len() - s, new.len() as isize - old.len() as isize)
}

fn text_edit_delta(path: &str, old: &str, new: &str) -> TextEditDelta {
    let (at, old_end, byte_delta) = byte_diff(old, new);
    let new_end = (old_end as isize + byte_delta) as usize;
    let old_lines = old.as_bytes()[at..old_end]
        .iter()
        .filter(|b| **b == b'\n')
        .count();
    let new_lines = new.as_bytes()[at..new_end]
        .iter()
        .filter(|b| **b == b'\n')
        .count();
    TextEditDelta {
        file: Ustr::from(path),
        at,
        byte_delta,
        line_delta: new_lines as isize - old_lines as isize,
    }
}

/// Whether `old` and `new` list the same items and only function items differ
/// in interface. A non-function item's body hash (a `const` value) counts too.
fn only_functions_changed(old: &[ItemPrint], new: &[ItemPrint]) -> bool {
    old.len() == new.len()
        && old.iter().zip(new).all(|(o, n)| {
            o.kind == n.kind
                && o.name == n.name
                && (o.kind == ItemKindTag::Fn
                    || (o.iface_hash == n.iface_hash && o.body_hash == n.body_hash))
        })
}

/// Every definition a cached caller can address by arena index, in index order.
fn item_names(module: &Module) -> Vec<Ustr> {
    let mut names = Vec::new();
    names.extend(module.functions.values().map(|f| f.name));
    names.extend(module.structs.values().map(|s| s.name));
    names.extend(module.enums.values().map(|e| e.name));
    names.extend(module.newtypes.values().map(|n| n.name));
    names.extend(module.globals.values().map(|g| g.name));
    names.extend(module.decorators.values().map(|d| d.name));
    names
}

/// The index of the single item whose function BODY block strictly contains the
/// edit region `[es, oe)`, or `None` if the edit touches a signature, spans
/// items, or lands between them (body ranges are disjoint, so at most one matches
/// -- the extra guard is defensive).
fn locate_body_edit(prints: &[ItemPrint], es: usize, oe: usize) -> Option<usize> {
    let mut found = None;
    for (i, p) in prints.iter().enumerate() {
        if let Some((bs, be)) = p.body_range {
            if bs <= es && oe <= be {
                if found.is_some() {
                    return None;
                }
                found = Some(i);
            }
        }
    }
    found
}

/// New per-item prints after a body edit: items before `idx` are unchanged, `idx`
/// is replaced with the re-parsed `np` (already absolute), and every item after
/// `idx` (all past `shift.at`) moves by `shift` -- so the next edit still
/// locates correctly without a whole-file reparse.
fn build_shifted_prints(
    mut prints: Vec<ItemPrint>,
    idx: usize,
    np: ItemPrint,
    shift: &TextEditDelta,
) -> Vec<ItemPrint> {
    for p in prints.iter_mut().skip(idx + 1) {
        p.offset = shift.shift_offset(p.offset);
        shift.shift_span(&mut p.item_span);
        if let Some((a, b)) = p.body_range {
            p.body_range = Some((shift.shift_offset(a), shift.shift_offset(b)));
        }
    }
    prints[idx] = np;
    prints
}

fn fingerprint(value: impl std::hash::Hash) -> u64 {
    use std::hash::Hasher;
    let mut h = rustc_hash::FxHasher::default();
    value.hash(&mut h);
    h.finish()
}

/// Nominal definitions whose users must see a re-lowered interface.
fn exported_type_names(module: &Module) -> impl Iterator<Item = Ustr> + '_ {
    module
        .struct_by_name
        .keys()
        .chain(module.enums_by_name.keys())
        .chain(module.newtypes_by_name.keys())
        .copied()
}

fn iface_dep_names(module: &Module) -> FxHashSet<Ustr> {
    module
        .live_type_exprs()
        .filter_map(|te| match te {
            TypeExpr::App {
                ctor: TypeCtor::Nominal(n),
                ..
            } => Some(*n),
            _ => None,
        })
        .collect()
}

fn body_errors(ws: &Workspace) -> Vec<CompilerError> {
    let mut errs: Vec<CompilerError> = ws
        .inferences
        .iter()
        .flatten()
        .flat_map(|inf| inf.errors.values().flatten().cloned())
        .chain(
            ws.modules
                .iter()
                .flat_map(|m| m.lowering_errors.values().flatten().cloned()),
        )
        .chain(
            ws.modules
                .iter()
                .flat_map(|m| m.interface_errors.iter().cloned()),
        )
        .collect();

    errs.sort_by(|a, b| {
        (a.location.file.as_str(), a.location.offset)
            .cmp(&(b.location.file.as_str(), b.location.offset))
    });
    errs
}

#[cfg(debug_assertions)]
/// A module's public interface, rendered so it is INVARIANT under the `ty_arena` index drift
/// that a body-only relower legitimately introduces.
fn interface_snapshot(ws: &Workspace, mid: ModuleId) -> String {
    use std::fmt::Write;

    let m = ws.module(mid);
    let mut out = String::new();

    for (_, f) in m.functions.iter() {
        let params = f
            .param_decls
            .iter()
            .map(|p| {
                format!(
                    "{}:{}{}",
                    p.name,
                    render_ty(ws, &p.ty),
                    if p.has_default { "=?" } else { "" }
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let generics = f
            .generics
            .iter()
            .map(|g| render_ty(ws, g))
            .collect::<Vec<_>>()
            .join(",");
        let ret = f
            .return_ty
            .as_ref()
            .map(|t| render_ty(ws, t))
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "fn {} co={} var={} gen=[{generics}] params=[{params}] req={} ret={ret} inline={:?} backend={:?} extern={} anns={:?} ffi={:?}",
            f.name,
            f.is_coroutine,
            f.is_variadic,
            f.num_required,
            f.inline_body,
            f.backend,
            f.is_extern,
            f.annotations,
            f.ffi_lib,
        );
    }

    for (_, g) in m.globals.iter() {
        let _ = writeln!(out, "global {}:{}", g.name, render_ty(ws, &g.ty));
    }

    let _ = write!(
        out,
        "{:?}",
        (&m.structs, &m.enums, &m.newtypes, &m.decorators)
    );
    out
}

/// Resolve a `TyId` to a canonical, `ty_arena`-index-independent shape string.
#[cfg(debug_assertions)]
fn render_ty(ws: &Workspace, ty: &bone_semantic::hir::TyId) -> String {
    use bone_semantic::hir::TyId;
    match ty {
        TyId::Static(mid, idx) => render_ty_node(ws, *mid, *idx),
        // Body-local types never appear in interface position; render defensively.
        TyId::Local(idx) => format!("local#{idx:?}"),
    }
}

#[cfg(debug_assertions)]
fn render_ty_node(ws: &Workspace, mid: ModuleId, idx: Idx<bone_semantic::hir::TypeExpr>) -> String {
    use bone_semantic::hir::TypeExpr;

    let render_range = |range: IdxRange<TypeExpr>| {
        range
            .map(|i| render_ty_node(ws, mid, i))
            .collect::<Vec<_>>()
            .join(",")
    };

    match &ws.module(mid).ty_arena[idx] {
        TypeExpr::Builtin(b) => format!("{b:?}"),
        // VarId elided -- the fast path continues the var counter, so the same
        // type carries different ids when re-lowered vs freshly lowered.
        TypeExpr::Generic(_, n) => format!("Generic({n})"),
        TypeExpr::Infer(_, n) => format!("Infer({n})"),
        TypeExpr::Nullable(i) => format!("?{}", render_ty_node(ws, mid, *i)),
        TypeExpr::App { ctor, params } => format!("App({ctor:?},[{}])", render_range(*params)),
        TypeExpr::Function { params, ret } => {
            format!(
                "Fn([{}]=>{})",
                render_range(*params),
                render_ty_node(ws, mid, *ret)
            )
        }
        TypeExpr::Tuple { elements } => format!("Tuple([{}])", render_range(*elements)),
    }
}
