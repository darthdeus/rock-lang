use std::sync::Arc;

use skull_parser::stdlib::ModuleSet;
use skull_parser::{
    Ast, FunctionBody, GlobalPolicy, ItemKind, MacroRegistry, PackageTag,
    check_macro_name_uniqueness, collect_registry, cook_one,
};

use bone_core::modules::ScopeProbe;
use bone_core::prelude::Nominal;
use bone_semantic::{
    hir::{Function, Module, TypeCtor, TypeExpr},
    prelude::*,
    semantic_passes::{FragmentContribution, TypeArenaSeed, contribute, lower_modules_seeded},
};

use crate::span_shift;

pub use bone_core::modules::NsAliases;

pub type BodyId = (ModuleId, BodyOwner);

pub use bone_core::text_edit::TextEditDelta;

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum UpdateTier {
    #[default]
    NoChange,
    BodyOnly,
    Interface,
    Scope,
    FullUser,
}

#[derive(Clone, Debug, Default)]
pub struct UpdateResult {
    pub tier: UpdateTier,
    pub reinferred: Vec<BodyId>,
    pub relowered: Vec<BodyId>,
    pub text_edit: Option<TextEditDelta>,
}

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
                        let changed = [
                            (&on.owned_types, &nn.owned_types),
                            (&on.owned_globals, &nn.owned_globals),
                            (&on.owned_callables, &nn.owned_callables),
                            (&on.owned_decorators, &nn.owned_decorators),
                            (&on.children, &nn.children),
                        ]
                        .into_iter()
                        .flat_map(|(old, new)| changed_keys(old, new))
                        .chain(changed_keys(&on.callable_members, &nn.callable_members))
                        .chain(changed_keys(&on.exports, &nn.exports));
                        delta
                            .members
                            .extend(changed.map(|name| (package, key, name)));
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

fn changed_keys<V: PartialEq>(
    old: &FxHashMap<Ustr, V>,
    new: &FxHashMap<Ustr, V>,
) -> impl Iterator<Item = Ustr> {
    changed_keys_by(old, new, PartialEq::eq)
}

fn changed_keys_by<V>(
    old: &FxHashMap<Ustr, V>,
    new: &FxHashMap<Ustr, V>,
    same: impl Fn(&V, &V) -> bool,
) -> impl Iterator<Item = Ustr> {
    old.iter()
        .filter(move |(k, value)| !new.get(*k).is_some_and(|next| same(value, next)))
        .map(|(k, _)| *k)
        .chain(new.keys().filter(move |k| !old.contains_key(*k)).copied())
}

/// How many times its live size a module's type arena may grow by keeping
/// earlier lowerings' entries.
const MAX_KEPT_ARENA_GROWTH: usize = 16;

/// Reuse the warm-only workspace when no user macro can affect warm expansion.
#[derive(Clone, Debug)]
enum WarmBase {
    Unbuilt,
    Clean(Box<Workspace>),
    /// The warm modules alone were refused or report a diagnostic; never reused.
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
    pub dependents: FxHashMap<Ustr, FxHashSet<ModuleId>>,
    module_dep_names: FxHashMap<ModuleId, FxHashSet<Ustr>>,
    dependents_iface: FxHashMap<Ustr, FxHashSet<ModuleId>>,
    /// Independent of inference: even a module with body errors has interface
    /// deps. Invalidated when its lowered interface is replaced or purged.
    module_iface_dep_names: FxHashMap<ModuleId, FxHashSet<Ustr>>,
    pub registry: MacroRegistry,
    /// Warm macro signatures, captured before expansion removes their definitions.
    warm_macros: Vec<(Ustr, MacroDef)>,
    pub aliases: bone_core::modules::NsAliases,
    /// The warm modules lowered alone, reused only when that equals lowering them
    /// with the user files (see [`WarmBase`]).
    warm_base: WarmBase,
    warm_names: bone_semantic::stdlib_base::WarmNames,
    /// Computed once: warm ASTs are immutable.
    warm_contributions: Vec<FragmentContribution>,
    declared_facts: FxHashMap<String, (Option<PackageTag>, GlobalPolicy)>,
}

#[derive(Clone, Debug)]
pub struct SessionFile {
    pub path: String,
    pub text: String,
    pub package: Option<PackageTag>,
    pub global_policy: GlobalPolicy,
}

impl SessionFile {
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
    pub macros: Vec<MacroDef>,
    package: Option<PackageTag>,
    global_policy: GlobalPolicy,
    /// Recovery keeps the partial tree queryable while diagnostics retain its syntax errors.
    syntax_errors: Vec<CompilerError>,
    pub contribution: FragmentContribution,
}

/// The type is stored as surface text because the AST is transient.
#[derive(Clone, Debug)]
pub struct MacroParam {
    pub name: Ustr,
    pub is_variadic: bool,
    pub ty: String,
}

/// Expansion drops macro definitions, so retain their parsed signatures.
#[derive(Clone, Debug)]
pub struct MacroDef {
    pub name: Ustr,
    /// Shifted incrementally on fast body edits.
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
    /// Covers leading annotations; the body-edit fast path re-parses `text[item_span]`.
    item_span: Span,
    /// Body block of a Rock-bodied fn: an edit strictly inside it is a pure body edit.
    body_range: Option<(usize, usize)>,
}

impl ItemPrint {
    fn same_interface(&self, other: &Self) -> bool {
        self.kind == other.kind && self.name == other.name && self.iface_hash == other.iface_hash
    }
}

enum FileDiff {
    BodiesOnly(Vec<usize>),
    Interface { only_functions: bool },
    Scope,
}

fn reject_macro_name_collisions(modules: &ModuleSet) -> Result<(), CompilerError> {
    check_macro_name_uniqueness(modules.iter())
        .into_iter()
        .next()
        .map_or(Ok(()), Err)
}

fn same_scope(left: &FragmentContribution, right: &FragmentContribution) -> bool {
    let header_matches = match (&left.header, &right.header) {
        (Some(a), Some(b)) => a.vis == b.vis && same_names(&a.path, &b.path),
        (None, None) => true,
        _ => false,
    };
    header_matches && same_scope_items(&left.items, &right.items)
}

fn same_names(left: &[skull_parser::Ident], right: &[skull_parser::Ident]) -> bool {
    left.iter()
        .map(|ident| ident.name)
        .eq(right.iter().map(|ident| ident.name))
}

fn same_scope_items(
    left: &[bone_semantic::semantic_passes::ScopeItem],
    right: &[bone_semantic::semantic_passes::ScopeItem],
) -> bool {
    use bone_semantic::semantic_passes::ScopeItem;

    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| match (left, right) {
                (
                    ScopeItem::Def {
                        name: a,
                        kind: ka,
                        vis: va,
                        ..
                    },
                    ScopeItem::Def {
                        name: b,
                        kind: kb,
                        vis: vb,
                        ..
                    },
                ) => (a, ka, va) == (b, kb, vb),
                (ScopeItem::Use(a), ScopeItem::Use(b)) => same_use(a, b),
                _ => false,
            })
}

fn same_use(left: &skull_parser::UseDecl, right: &skull_parser::UseDecl) -> bool {
    use skull_parser::UseSuffix;

    let name = |alias: &Option<skull_parser::Ident>| alias.as_ref().map(|ident| ident.name);
    left.from_root == right.from_root
        && left.vis == right.vis
        && left.paths().len() == right.paths().len()
        && left.paths().iter().zip(right.paths()).all(|(a, b)| {
            a.parent == b.parent
                && same_names(&a.segments, &b.segments)
                && match (&a.suffix, &b.suffix) {
                    (UseSuffix::Bind(a), UseSuffix::Bind(b))
                    | (UseSuffix::Operator(a), UseSuffix::Operator(b)) => name(a) == name(b),
                    (UseSuffix::Glob, UseSuffix::Glob) | (UseSuffix::Group, UseSuffix::Group) => {
                        true
                    }
                    _ => false,
                }
        })
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
                ItemKind::CompilerDirective(_) | ItemKind::Use(_) => {
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
        return FileDiff::Interface {
            only_functions: false,
        };
    }

    let mut changed = Vec::new();

    for (i, (o, n)) in old.iter().zip(new).enumerate() {
        if !o.same_interface(n) {
            return FileDiff::Interface {
                only_functions: only_functions_changed(old, new),
            };
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

        // Keep each parsed module for lowering: `combined_unexpanded` would parse again.
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

            // Looked up, not interned: user modules take their ids after the warm
            // modules, so a warm-alone build interns the warm modules alone.
            if session
                .ws
                .module_ids
                .get(&module.file)
                .is_some_and(|mid| session.warm_mids.contains(mid))
            {
                // A stdlib file in the initial set is the warm module; a user copy would self-collide.
                continue;
            }

            let prints = item_prints(&module, &text);
            let macros = macro_defs(&module);
            let contribution = contribute(&module);

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

    pub fn combined_unexpanded(&self) -> Result<ModuleSet, CompilerError> {
        let users = self
            .paths
            .iter()
            .map(|path| self.parse_stored(ustr(path)))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ModuleSet::new(self.warm.clone(), users))
    }

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

    fn parse_stored(&self, path: Ustr) -> Result<Ast, CompilerError> {
        let file = &self.files[path.as_str()];
        parse_with_facts(&file.text, path, file.package.clone(), file.global_policy)
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
        self.ws
            .reinferred_body_count
            .store(0, std::sync::atomic::Ordering::Relaxed);
        self.ws
            .relowered_body_count
            .store(0, std::sync::atomic::Ordering::Relaxed);
        let hash = fingerprint(text);

        if self
            .files
            .get(path)
            .is_some_and(|file| file.text_hash == hash)
        {
            return Ok(UpdateResult::default());
        }

        if let Some(result) = self.try_incremental_body_edit(path, text, hash)? {
            return Ok(result);
        }

        let module = self.parse_buffer(path, text)?;
        let mid = self.ws.intern_module(module.file);
        if self.warm_mids.contains(&mid) {
            // Opening a warm (stdlib) file: it IS the warm module, not a second copy.
            return Ok(UpdateResult::default());
        }

        let old = self.files.get(path);
        let new_prints = item_prints(&module, text);
        let new_macros = macro_defs(&module);
        let changed: FxHashSet<Ustr> = old
            .map(|old| {
                changed_keys(&name_shapes(&old.item_prints), &name_shapes(&new_prints)).collect()
            })
            .unwrap_or_default();
        let touches_macros = old.is_some_and(|old| {
            touches_macro(&old.macros, &new_macros, &changed)
                || self.defines_a_function_named_like_a_macro(&module)
        });
        let contribution = if old.is_some() && !touches_macros {
            let mut cooked = module.clone();
            cook_one(&mut cooked, &self.registry)?;
            contribute(&cooked)
        } else {
            contribute(&module)
        };
        let diff = old.filter(|_| !module.items.is_empty()).map(|old| {
            if !same_scope(&old.contribution, &contribution) {
                FileDiff::Scope
            } else {
                classify(&old.item_prints, &new_prints)
            }
        });
        // The in-place body path keeps every other item's spans, so text
        // outside that body (comments, recovered junk) must be unchanged.
        let body_edit = old.and_then(|old| {
            let (start, end, _) = byte_diff(&old.text, text);
            let idx = locate_body_edit(&old.item_prints, start, end)?;
            Some((idx, text_edit_delta(path, &old.text, text)))
        });
        let next = UserFile {
            text_hash: hash,
            item_prints: new_prints,
            text: text.to_string(),
            macros: new_macros,
            package: module.package.clone(),
            global_policy: module.global_policy,
            syntax_errors: module.recovery_errors.clone(),
            contribution,
        };
        self.apply_file(path, Some(next), |s| match diff {
            Some(FileDiff::BodiesOnly(changed)) => match changed.as_slice() {
                [idx] if body_edit.as_ref().is_some_and(|(at, _)| at == idx) => {
                    let (_, edit) = body_edit.expect("located body edit");
                    s.rebuild_body_incremental(path, module, *idx, false, &edit)
                }
                _ => s.rebuild_module(path, module, &changed),
            },
            Some(FileDiff::Interface { only_functions }) if !touches_macros => s.rebuild_interface(
                module,
                &changed,
                ScopeDelta::default(),
                UpdateTier::Interface,
                only_functions,
            ),
            Some(FileDiff::Scope) if !touches_macros => s.rebuild_scope(module, &changed),
            _ => s.rebuild_users(),
        })
    }

    /// `check_macro_name_uniqueness` refuses a function named like a macro, and only
    /// the full rebuild runs it.
    fn defines_a_function_named_like_a_macro(&self, module: &Ast) -> bool {
        let macro_names: FxHashSet<Ustr> = self.all_macro_defs().map(|(_, d)| d.name).collect();
        if macro_names.is_empty() {
            return false;
        }
        module.items.iter().any(|item| {
            matches!(&item.kind, ItemKind::Function(f) if !f.is_macro && macro_names.contains(&f.name.name))
        })
    }

    /// Namespace assembly must precede expansion and lowering.
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
        // The batch path's whole-program gate: a non-public export type closure must
        // fail in a session too.
        bone_semantic::semantic_passes::pass_validate_api_closure(&ws)?;
        ws.phase_timings.clear();
        self.set_cooked_contributions(user_contributions, warm_contributions);
        Ok((ws, mids[first_user..].to_vec()))
    }

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

    /// In the order `build_namespaces` reads a fresh build's modules, so a rejection
    /// names the same file.
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
                user_file.contribution = contribution;
            }
        }
        self.warm_contributions = warm_contributions;
        self.warm_names =
            bone_semantic::stdlib_base::warm_names(&self.warm, &self.warm_contributions);
    }

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
                .nominals
                .iter()
                .filter(|(_, def)| def.module() == mid)
                .map(|(name, def)| {
                    let flavor = match def {
                        Nominal::Struct(_) => NominalFlavor::Struct,
                        Nominal::Enum(_) => NominalFlavor::Enum,
                        Nominal::Newtype(_) => NominalFlavor::Newtype,
                    };
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
        let changed_types: Vec<_> = changed_keys(&old_types, &new_types).collect();

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
        if !only_functions || !changed_types.is_empty() || !delta.is_empty() {
            to_relower.extend(self.ws.user_modules(&self.warm_mids).filter_map(|module| {
                (module.id != mid && !module.interface_errors.is_empty()).then_some(module.id)
            }));
        }

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

        // Re-lowering shifts the arena, invalidating cached inference.
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

        // `ws.modules` is append-only and `ModuleId` is its dense index.
        let ids: Vec<ModuleId> = to_reinfer
            .iter()
            .copied()
            .filter(|m| m.index() < self.ws.modules.len())
            .collect();

        let mut reused: FxHashSet<BodyId> = FxHashSet::default();
        let prevs: Vec<(ModuleId, InferenceResult)> = ids
            .iter()
            .map(|id| {
                let mut prev = self.take_inference(*id);
                prev.per_fn.retain(|&owner, bi| {
                    // Globals, decorators and struct defaults re-check
                    // regardless of `prev`, so only a function is reused.
                    let reusable = matches!(owner, BodyOwner::Function(_))
                        && !prev.errors.contains_key(&owner)
                        && bi.deps.is_disjoint(&reinfer_names);
                    if reusable {
                        reused.insert((*id, owner));
                    }
                    reusable
                });
                prev.errors.clear();
                (*id, prev)
            })
            .collect();
        // check_module_incremental reads ws immutably, so dependents re-check in parallel.
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

        // A signature edit alone can export a type whose closure is not public.
        bone_semantic::semantic_passes::pass_validate_api_closure(&self.ws)?;

        self.rebuild_dependents();

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
        self.apply_file(path, None, Self::rebuild_users)
    }

    pub fn diagnostics(&self) -> Vec<CompilerError> {
        let mut errs = body_errors(&self.ws);
        // The interactive path keeps the partial tree and reports its recovered syntax errors.
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

        self.ws.nominals.retain(|_, def| keep(def.module()));

        // Facts owned by kept modules stay, so a kept first definer still wins over a
        // re-lowered duplicate.
        self.ws.type_generics.retain(|_, entry| keep(entry.owner));

        self.ws.functions_by_name.retain(|_, fns| {
            fns.retain(|(mid, _)| keep(*mid));
            !fns.is_empty()
        });

        self.ws.globals_by_name.clear();
        // Rebuild both global indexes so a removed const stops restricting assignments.
        self.ws.const_globals.clear();

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
        // A failed lowering may intern ids without inserting modules.
        let unexpanded = self.combined_unexpanded()?;
        let registry = collect_registry(unexpanded.iter());
        reject_macro_name_collisions(&unexpanded)?;
        let (mut ws, user_mids) = self.build(unexpanded, &registry)?;
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

    fn update_dependents_for(&mut self, mid: ModuleId) {
        if !Workspace::is_user_module(mid, &self.warm_mids) {
            return;
        }

        let new = self.module_deps(mid);
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

        let old_inf = self.take_inference(mid);

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

        self.reinfer_body_edit(mid, old_inf)?;

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
        // A recovered (partial) item must not be installed in place.
        if !mini.recovery_errors.is_empty() {
            return Ok(None);
        }
        cook_one(&mut mini, &self.registry)?;

        let Ok([np]) = <[ItemPrint; 1]>::try_from(item_prints(&mini, new_text)) else {
            return Ok(None);
        };
        // A changed name or interface must go through the full path's classification.
        // The item parse stops at the end of the function; text left after it
        // in the edited range is a new item the full path must see.
        if np.kind != ItemKindTag::Fn
            || np.name != print.name
            || np.iface_hash != print.iface_hash
            || np.item_span.offset_range().1 != new_end
        {
            return Ok(None);
        }

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
            contribution: old.contribution.clone(),
        };
        self.apply_file(path, Some(next), |s| {
            s.rebuild_body_incremental(path, mini, idx, true, &item_shift)
        })
        .map(Some)
    }

    fn rebuild_body_incremental(
        &mut self,
        path: &str,
        ast: Ast,
        changed_idx: usize,
        cooked: bool,
        edit: &TextEditDelta,
    ) -> Result<UpdateResult, CompilerError> {
        let file = ast.file;
        let mid = self.ws.module_ids[&file];

        // Equal to the old start, so bdy_at of old still resolves to the right owner.
        let new_start = self.files[path].item_prints[changed_idx].offset;

        let owner = match self.ws.module(mid).body_at(new_start) {
            Some(o) => o,
            None => return self.rebuild_module(path, ast, &[changed_idx]),
        };

        let BodyOwner::Function(fid) = owner else {
            return self.rebuild_module(path, ast, &[changed_idx]);
        };

        let old_fn_span = self.ws.module(mid).functions[fid].span;

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

        // The text after the edited body moves by the edit's size, not by the
        // change in the function's span: recovery may have ended the old
        // function early.
        let at = old_fn_span.offset_range().1;
        let (byte_delta, line_delta) = (edit.byte_delta, edit.line_delta);

        let old_inf = self.take_inference(mid);
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

        self.reinfer_body_edit(mid, old_inf)?;

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

    fn take_inference(&mut self, mid: ModuleId) -> InferenceResult {
        self.ws
            .inferences
            .get_mut(mid.index())
            .and_then(Option::take)
            .unwrap_or_default()
    }

    fn reinfer_body_edit(
        &mut self,
        mid: ModuleId,
        previous: InferenceResult,
    ) -> Result<(), CompilerError> {
        let timer = Instant::now();
        let inf = check_module_incremental(&self.ws, mid, self.ws.module(mid), previous)?;
        self.ws.set_inference(mid, inf);
        self.ws.log_time("infer functions", timer);
        Ok(())
    }

    /// `None` once dead entries piled up enough that the edit should re-lower from empty.
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
        rebuild: impl FnOnce(&mut Self) -> Result<UpdateResult, CompilerError>,
    ) -> Result<UpdateResult, CompilerError> {
        let (previous, position) = if let Some(file) = next {
            let previous = self.files.insert(path.to_string(), file);
            if previous.is_none() {
                self.paths.push(path.to_string());
            }
            (previous, None)
        } else {
            let Some(previous) = self.files.remove(path) else {
                return Ok(UpdateResult::default());
            };
            let position = self
                .paths
                .iter()
                .position(|p| p == path)
                .expect("tracked file must be in path order");
            self.paths.remove(position);
            (Some(previous), Some(position))
        };
        match rebuild(self) {
            Ok(mut result) => {
                if result.tier == UpdateTier::BodyOnly {
                    result.text_edit = previous
                        .as_ref()
                        .map(|file| text_edit_delta(path, &file.text, &self.files[path].text));
                }
                if position.is_none() && result.tier != UpdateTier::NoChange {
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
                let action = if position.is_some() {
                    "removal"
                } else {
                    "update"
                };
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

/// Types are rendered while the AST is alive; expansion drops macros from the module.
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
            (o.kind == ItemKindTag::Fn && n.kind == ItemKindTag::Fn && o.name == n.name)
                || (o.same_interface(n) && o.body_hash == n.body_hash)
        })
}

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

/// The single item whose fn body block strictly contains `[es, oe)`.
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

fn exported_type_names(module: &Module) -> impl Iterator<Item = Ustr> + '_ {
    module.nominals.keys().copied()
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
            "fn {} co={} var={} gen=[{generics}] params=[{params}] req={} ret={ret} inline={:?} backend={:?} extern={} anns={:?} ffi={:?} lua_path={:?}",
            f.name,
            f.is_coroutine,
            f.is_variadic,
            f.num_required,
            f.inline_body,
            f.backend,
            f.is_extern,
            f.annotations,
            f.ffi_lib,
            f.lua_extern_path,
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
