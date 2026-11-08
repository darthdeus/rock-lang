use skull_parser::{
    Ast, Dialect, FunctionBody, GlobalPolicy, ItemKind, MacroRegistry, PackageTag,
    check_macro_call_labels, check_macro_call_labels_in, check_macro_name_uniqueness,
    collect_registry, cook_one,
};

use bone_core::modules::ScopeProbe;
use bone_semantic::{
    hir::{Function, Module, TypeCtor, TypeExpr},
    prelude::*,
    semantic_passes::{FragmentContribution, contribute},
};

use crate::span_shift;

/// Re-exported because it is part of [`Session::new`]'s signature: a caller
/// that builds a session needs the alias table type without depending on
/// `bone-core` directly.
pub use bone_core::modules::NsAliases;

/// Unique identifier of a Body across all modules.
pub type BodyId = (ModuleId, BodyOwner);

/// The source-coordinate change carried with one incremental body update.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextEditDelta {
    pub file: Ustr,
    pub at: usize,
    pub byte_delta: isize,
    pub line_delta: isize,
}

/// The semantic part of a body-only update, kept separate from coordinate
/// relocation so graph owners are not re-walked merely because spans moved.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BodyUpdate {
    pub edit: Option<TextEditDelta>,
    pub semantic_bodies: Vec<BodyId>,
    pub span_only: bool,
}

/// Represents a kind of update necessary after change detection
///
/// - NoChange: nothing really changed (e.g. watcher detected a file save without any changes)
/// - BodyOnly: body of some items changed, but their interface remains, meaning we can do a local
///   recompile of the bodies only
/// - Interface: a "public API" of an item changed, all of its users/callers need to be invalidated
///   and recompiled as well
/// - Scope: the namespace tree's inputs changed (a `mod` header, a `use`, a v1 definition's name,
///   kind or visibility, or a legacy definition a compatibility view also names). The tree is
///   re-assembled, and the edited file plus every file whose lowering read a changed fact re-lower,
///   then the interface cascade runs (scope epochs)
/// - FullUser: a full recompile of every user file
///
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum UpdateTier {
    NoChange,
    BodyOnly,
    Interface,
    Scope,
    FullUser,
}

/// Result of a session update. Stores the type of update, and all the bodies
/// that were reinferred.
#[derive(Clone, Debug)]
pub struct UpdateResult {
    pub tier: UpdateTier,
    pub reinferred: Vec<BodyId>,
    pub relowered: Vec<BodyId>,
    pub body_update: BodyUpdate,
}

impl Default for UpdateResult {
    fn default() -> Self {
        Self {
            tier: UpdateTier::NoChange,
            reinferred: Vec::new(),
            relowered: Vec::new(),
            body_update: BodyUpdate::default(),
        }
    }
}

/// The namespace facts one edit changed, in the terms a lowering records
/// them ([`ScopeProbe`]): a module's member of one name, every member of a
/// module (its presence or its glob export bindings changed), a module entered
/// as a path segment (its presence or visibility), and a legacy bare name.
/// Plus the files whose own fragment scope changed.
#[derive(Clone, Debug, Default)]
struct ScopeDelta {
    members: FxHashSet<(u32, Ustr, Ustr)>,
    all_members: FxHashSet<(u32, Ustr)>,
    nodes: FxHashSet<(u32, Ustr)>,
    legacy: FxHashSet<Ustr>,
    fragments: FxHashSet<Ustr>,
}

impl ScopeDelta {
    fn is_empty(&self) -> bool {
        self.members.is_empty()
            && self.all_members.is_empty()
            && self.nodes.is_empty()
            && self.legacy.is_empty()
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
            ScopeProbe::Legacy { name } => self.legacy.contains(&name),
        }
    }

    /// Diff two assembled trees (`old` and `new` hold `ns.packages`, `defs`,
    /// `fragment_scopes`, `legacy_projections` and `legacy_imports`).
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

        let def_keys: FxHashSet<Ustr> = old.defs.keys().chain(new.defs.keys()).copied().collect();
        for key in def_keys {
            let (o, n) = (old.defs.get(&key), new.defs.get(&key));
            let same = match (o, n) {
                (Some(o), Some(n)) => {
                    o.package == n.package
                        && o.module == n.module
                        && o.kind == n.kind
                        && o.name == n.name
                        && o.vis == n.vis
                }
                _ => false,
            };
            if !same {
                for meta in o.into_iter().chain(n) {
                    delta.members.insert((meta.package, meta.node, meta.name));
                }
            }
        }

        let files: FxHashSet<Ustr> = old
            .legacy_projections
            .keys()
            .chain(new.legacy_projections.keys())
            .chain(old.legacy_imports.keys())
            .chain(new.legacy_imports.keys())
            .copied()
            .collect();
        let empty_projection = bone_core::modules::LegacyProjection::default();
        let empty_imports = bone_core::modules::LegacyImports::default();
        for file in files {
            let (old_view, new_view) = (
                old.legacy_projections
                    .get(&file)
                    .unwrap_or(&empty_projection),
                new.legacy_projections
                    .get(&file)
                    .unwrap_or(&empty_projection),
            );
            let (old_imports, new_imports) = (
                old.legacy_imports.get(&file).unwrap_or(&empty_imports),
                new.legacy_imports.get(&file).unwrap_or(&empty_imports),
            );
            delta
                .legacy
                .extend(changed_keys(&old_view.by_name, &new_view.by_name));
            delta.legacy.extend(changed_keys(
                &old_imports.named.by_name,
                &new_imports.named.by_name,
            ));
            delta.legacy.extend(changed_keys(
                &old_imports.globs.by_name,
                &new_imports.globs.by_name,
            ));
            delta
                .legacy
                .extend(changed_keys(&old_imports.renamed, &new_imports.renamed));
        }

        let scoped: FxHashSet<Ustr> = old
            .fragment_scopes
            .keys()
            .chain(new.fragment_scopes.keys())
            .copied()
            .collect();
        for file in scoped {
            let same = match (
                old.fragment_scopes.get(&file),
                new.fragment_scopes.get(&file),
            ) {
                (Some(o), Some(n)) => {
                    o.package == n.package
                        && o.package_name == n.package_name
                        && o.module_path == n.module_path
                        && o.named == n.named
                        && o.globs == n.globs
                }
                _ => false,
            };
            if !same {
                delta.fragments.insert(file);
            }
        }

        delta
    }
}

/// The keys whose entries differ between two maps, a missing entry included.
fn changed_keys<V: PartialEq>(old: &FxHashMap<Ustr, V>, new: &FxHashMap<Ustr, V>) -> Vec<Ustr> {
    old.iter()
        .filter(|(k, v)| new.get(*k) != Some(*v))
        .map(|(k, _)| *k)
        .chain(new.keys().filter(|k| !old.contains_key(*k)).copied())
        .collect()
}

/// Whether the warm modules can be lowered and inferred once, alone, and
/// reused by every build. Warm ASTs are immutable for the session's lifetime
/// and a stdlib file consults no user SCOPE (`legacy_name` answers `Bare` for
/// it, and it has no fragment scope). But warm and legacy user files share
/// the bare-name maps a lowering and an inference read -- types and their
/// generic arity, globals, overload families, decorators, macros -- so a
/// user definition under a warm name changes a fresh build (a user
/// `struct Box` is a cross-module duplicate of the warm `Box<T>`; a user
/// overload joins its family), and a warm body that names something only a
/// user file defines resolves only in a combined build. A build reuses the
/// warm-alone state only when
/// neither can happen: no user definition or macro takes a warm name
/// (`Session::apart_from_warm`) and the warm-alone build is clean.
/// Otherwise it lowers warm and user files together, as a fresh build does.
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
    pub warm: Vec<Ast>,
    pub warm_mids: FxHashSet<ModuleId>,
    pub paths: Vec<String>,
    pub files: FxHashMap<String, UserFile>,
    /// Reverse dep index: name -> modules whose bodies reference it. Used by
    /// `rebuild_interface` to find which modules to re-infer on an interface edit.
    pub dependents: FxHashMap<Ustr, FxHashSet<ModuleId>>,
    /// Forward dep index: module -> the names its bodies reference. The inverse
    /// of `dependents`, kept so a body edit can update just the edited module's
    /// entry in O(its deps) instead of rescanning the whole workspace.
    pub module_dep_names: FxHashMap<ModuleId, FxHashSet<Ustr>>,
    /// Reverse index for INTERFACE-position type references: type-name -> modules
    /// whose signatures / globals / fields name it. The interface twin of
    /// `dependents` (which tracks body references). An interface change re-LOWERS
    /// these dependents (their signatures must re-resolve the name); a body change
    /// only re-infers `dependents`. Rebuilt from cached lowered-interface names
    /// in `rebuild_dependents`.
    pub dependents_iface: FxHashMap<Ustr, FxHashSet<ModuleId>>,
    /// Independent of inference: even a module with body errors has interface
    /// deps. Invalidated when its lowered interface is replaced or purged.
    module_iface_dep_names: FxHashMap<ModuleId, FxHashSet<Ustr>>,
    pub registry: MacroRegistry,
    /// Macros defined by the WARM (stdlib) modules, paired with their defining
    /// file. `files` only covers user macros, so without this the stdlib ones
    /// (`log_once`, `log_throttled`) are invisible to every consumer that reads
    /// macro defs. Warm ASTs are fixed for the session's lifetime, so this is
    /// computed once at construction rather than walked per query.
    pub warm_macros: Vec<(Ustr, MacroDef)>,
    /// The package graph's alias tables, kept because every O(workspace)
    /// rebuild runs `build_namespaces` again and needs them.
    pub aliases: bone_core::modules::NsAliases,
    /// The warm modules lowered and inferred alone, before any user file or
    /// namespace tree entered the workspace, which a build starts from
    /// instead of re-lowering the stdlib -- when that equals lowering them
    /// with the user files (see [`WarmBase`]).
    warm_base: WarmBase,
    /// Every bare name a warm module defines, plus the intrinsic type names:
    /// the names through which a user file can reach a warm lowering.
    warm_names: FxHashSet<Ustr>,
    /// The warm modules' namespace contributions (the `std` root), computed
    /// once: warm ASTs are immutable. User files keep theirs on `UserFile`.
    warm_contributions: Vec<FragmentContribution>,
    /// The dialect and owning package a host declared for a file that was
    /// not in the initial set ([`Session::declare_file_facts`]). A held file
    /// keeps its facts on its `UserFile`; these are read when the file enters
    /// through `update_file` or `with_file_variant`, and survive a refused
    /// entry, so the next attempt parses the same way.
    declared_facts: FxHashMap<String, (Dialect, Option<PackageTag>, GlobalPolicy)>,
}

/// A file entering a session, carrying the facts its package supplies.
///
/// A session RE-PARSES from stored text on every edit, so it has to keep the
/// dialect and the owning package rather than take them stamped once from the
/// caller: dialect and package facts reach the parser and bone through the
/// host, on the incremental route too. A standalone source with no
/// manifest is legacy and unowned, which is what [`SessionFile::legacy`]
/// spells (the loader supplies an explicit dialect, or a synthetic
/// package, for a standalone source).
#[derive(Clone, Debug)]
pub struct SessionFile {
    pub path: String,
    pub text: String,
    pub dialect: Dialect,
    pub package: Option<PackageTag>,
    /// The owning package's mutable-global policy, which every re-parse keeps.
    pub global_policy: GlobalPolicy,
}

impl SessionFile {
    /// Retain the source loader's parse and package facts across reparses.
    pub fn from_source(source: &rock_core::SourceFile) -> Self {
        Self {
            path: source.file.to_id_path(),
            text: source.contents.clone(),
            dialect: source.dialect,
            package: source.package.clone(),
            global_policy: source.global_policy,
        }
    }

    /// A source with no owning manifest: legacy dialect, no package.
    pub fn legacy(path: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            text: text.into(),
            dialect: Dialect::Legacy,
            package: None,
            global_policy: GlobalPolicy::Any,
        }
    }
}

#[derive(Clone, Debug)]
pub struct UserFile {
    pub text_hash: u64,
    pub item_prints: Vec<ItemPrint>,
    /// The file's current source text -- the single source of truth for this
    /// file's parse. A body edit byte-diffs old vs new to localize the change to
    /// one item (`try_incremental_body_edit`); the slow/full paths re-parse from
    /// it on demand. The AST is NOT persisted: the per-body HIR (`Module.bodies`)
    /// is the incremental store, so a stored AST would be a redundant second copy
    /// that drifts.
    pub text: String,
    /// The macros this file defines (Functions with `is_macro`), captured at
    /// parse time. Kept so an interface edit can ask "did a macro change?"
    /// (`touches_macro`) without a retained AST -- and so consumers (ctags) can
    /// read macro signatures, which are dropped from the cooked HIR by expansion.
    /// Each def's `span` is shifted on the fast body-edit path, so its line stays
    /// accurate without a reparse.
    pub macros: Vec<MacroDef>,
    /// The dialect this file parses in and the package that owns it, from the
    /// package graph. Kept here because every re-parse -- a full file reparse,
    /// a one-item fast edit, an O(workspace) rebuild -- needs them again, and
    /// re-deriving them would mean re-reading manifests on the edit path.
    pub dialect: Dialect,
    pub package: Option<PackageTag>,
    /// The owning package's mutable-global policy, kept for the same reason.
    pub global_policy: GlobalPolicy,
    /// Recovered syntax errors from this file's last parse. The parse is total
    /// (best-effort tree + recovery errors), so these are kept here and surfaced
    /// via `diagnostics()` -- a broken file still type-checks its valid parts AND
    /// shows the parse error as a diagnostic (the compile path rejects on them).
    pub syntax_errors: Vec<CompilerError>,
    /// Hash of what this file contributes to the namespace tree: the `mod`
    /// header, every `use`, and each definition's name, kind and visibility
    /// (see [`scope_hash`]). The header is not an item, so `item_prints` alone
    /// cannot see it change.
    pub scope_hash: u64,
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
pub enum ItemKindTag {
    Fn,
    Struct,
    Enum,
    Newtype,
    Global,
    Decorator,
    Other,
}

#[derive(Clone, Debug)]
pub struct ItemPrint {
    pub kind: ItemKindTag,
    pub name: Ustr,
    pub iface_hash: u64,
    pub body_hash: Option<u64>,
    offset: usize,
    /// Full item span (covers leading annotations) -- the incremental body-edit
    /// fast path slices `text[item_span]` to re-parse just this item, and reads
    /// its line/col as the `parse_one_item` base. Maintained across fast edits
    /// (the changed item replaced; following items shifted by the byte delta).
    item_span: Span,
    /// Byte range of the function's body BLOCK (Rock-bodied fns only) -- an edit
    /// strictly inside it is a pure body edit. `None` for non-fn / lua / extern /
    /// macro items, which the fast path never takes.
    body_range: Option<(usize, usize)>,
}

#[derive(Clone, Debug)]
pub enum FileDiff {
    LayoutOnly,
    BodiesOnly(Vec<usize>),
    Interface,
}

/// Reject a macro name colliding with a function name (call-position ambiguity)
/// and a macro call whose argument labels don't fit its parameters, cross-module,
/// on the pre-expansion module set (macros still present). The same gates
/// `check_modules` applies on the CLI compile path, so CLI / LSP rebuilds
/// refuse the same programs. Returns the first offender.
fn reject_macro_name_collisions(modules: &[Ast]) -> Result<(), CompilerError> {
    match check_macro_name_uniqueness(modules)
        .into_iter()
        .chain(check_macro_call_labels(modules))
        .next()
    {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// See [`UserFile::scope_hash`].
pub fn scope_hash(a: &Ast, text: &str) -> u64 {
    use std::hash::{Hash, Hasher};

    let mut h = rustc_hash::FxHasher::default();
    if let Some(header) = &a.module_header {
        let (s, e) = header.span.offset_range;
        text[s..e].hash(&mut h);
    }
    scope_items(&a.items, text, a.dialect == Dialect::V1, 0, &mut h);
    h.finish()
}

/// What `build_namespaces` reads from items: every `use`, and in a v1 file
/// each definition's name, kind and declared visibility and each inline `mod`
/// block's name and visibility. A legacy file's definitions are not in the
/// tree. Signatures, fields and bodies are left out -- an edit to them keeps
/// every DefKey and stays on the interface path.
fn scope_items(
    items: &[skull_parser::Item],
    text: &str,
    v1: bool,
    depth: usize,
    h: &mut rustc_hash::FxHasher,
) {
    use skull_parser::ItemKind;
    use std::hash::Hash;

    for item in items {
        let name = match &item.kind {
            ItemKind::Use(_) => {
                let (s, e) = item.span.offset_range;
                text[s..e].hash(h);
                continue;
            }
            _ if !v1 => continue,
            ItemKind::Module(block) => {
                (depth, "mod", block.name.name.as_str()).hash(h);
                hash_visibility(block.vis.as_ref(), h);
                scope_items(&block.items, text, v1, depth + 1, h);
                continue;
            }
            ItemKind::Struct(s) => (s.name.name, "type"),
            ItemKind::Enum(e) => (e.name.name, "type"),
            ItemKind::Newtype(n) => (n.name.name, "type"),
            ItemKind::Global(g) => (g.name.name, "global"),
            ItemKind::Function(f) => (f.name.name, "callable"),
            ItemKind::Decorator(d) => (d.name.name, "decorator"),
            _ => continue,
        };
        (depth, name.1, name.0.as_str()).hash(h);
        hash_visibility(item.vis.as_ref(), h);
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

pub fn item_prints(a: &Ast, text: &str) -> Vec<ItemPrint> {
    use skull_parser::ItemKind;
    use std::hash::{Hash, Hasher};

    let mut prints = Vec::new();

    for item in a.items.iter() {
        let (start, stop) = item.span.offset_range;
        debug_assert!(stop <= text.len(), "item span out of bounds");

        let mut h = rustc_hash::FxHasher::default();

        for ann in item.annotations.iter() {
            let (a, b) = ann.span.offset_range;
            text[a..b].hash(&mut h);
        }

        let (kind, name, body_hash, body_range) = match &item.kind {
            ItemKind::Function(f) => {
                let mut body_range = None;
                let body_hash = if let FunctionBody::Rock(b) = f.body
                    && !f.is_macro
                {
                    let (bs, be) = a.b(&b).span.offset_range;

                    debug_assert!(start <= bs && be <= stop, "block escapes item span");

                    text[start..bs].hash(&mut h);
                    text[be..stop].hash(&mut h);

                    body_range = Some((bs, be));
                    Some(text_hash(&text[bs..be]))
                } else {
                    // lua/extern/macro, whole body is interface
                    text[start..stop].hash(&mut h);
                    None
                };

                (ItemKindTag::Fn, f.name.name, body_hash, body_range)
            }

            ItemKind::Struct(s) => {
                text[start..stop].hash(&mut h);
                (ItemKindTag::Struct, s.name.name, None, None)
            }

            ItemKind::Enum(e) => {
                text[start..stop].hash(&mut h);
                (ItemKindTag::Enum, e.name.name, None, None)
            }

            ItemKind::Newtype(n) => {
                text[start..stop].hash(&mut h);
                (ItemKindTag::Newtype, n.name.name, None, None)
            }

            ItemKind::Decorator(d) => {
                text[start..stop].hash(&mut h);
                (ItemKindTag::Newtype, d.name.name, None, None)
            }

            ItemKind::Global(g) => {
                // A `const`'s init VALUE is its body; the `const` keyword, name, and
                // type annotation are interface. So a const value-only edit (same
                // type) is a BodyOnly edit -> it hot-reloads through the cheap
                // overlay. A type or keyword change moves the prefix text into the
                // iface_hash -> Interface (full rebuild).
                //
                // A mutable `global` keeps WHOLE-item interface: the hot-reload guard
                // preserves its live value across the re-dofile, so a value edit has
                // no live effect and the overlay deliberately never re-emits it.
                // Whole-item iface keeps such an edit on the full-rebuild path.
                if g.is_const
                    && let Some(value) = g.value
                {
                    let (vs, ve) = a.e(&value).span.offset_range;
                    debug_assert!(start <= vs && ve <= stop, "const init escapes item span");
                    text[start..vs].hash(&mut h);
                    text[ve..stop].hash(&mut h);
                    (
                        ItemKindTag::Other,
                        g.name.name,
                        Some(text_hash(&text[vs..ve])),
                        Some((vs, ve)),
                    )
                } else {
                    text[start..stop].hash(&mut h);
                    (ItemKindTag::Other, g.name.name, None, None)
                }
            }

            ItemKind::CompilerDirective(_) => {
                text[start..stop].hash(&mut h);
                (ItemKindTag::Other, Ustr::from(""), None, None)
            }

            ItemKind::MacroInvocation { name, .. } => {
                text[start..stop].hash(&mut h);
                (ItemKindTag::Other, name.name, None, None)
            }

            // Modules-v1 forms: whole-item interface, no body to isolate.
            // `scope_hash` is what routes their edits to a namespace rebuild.
            ItemKind::Use(_) | ItemKind::Module(_) => {
                text[start..stop].hash(&mut h);
                (ItemKindTag::Other, Ustr::from(""), None, None)
            }

            ItemKind::Comment(_) => continue,
        };

        prints.push(ItemPrint {
            kind,
            name,
            iface_hash: h.finish(),
            body_hash,
            offset: start,
            item_span: item.span,
            body_range,
        });
    }

    prints
}

pub fn classify(old: &[ItemPrint], new: &[ItemPrint]) -> FileDiff {
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

    if changed.is_empty() {
        FileDiff::LayoutOnly
    } else {
        FileDiff::BodiesOnly(changed)
    }
}

pub fn name_shapes(prints: &[ItemPrint]) -> FxHashMap<Ustr, u64> {
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
            (n, hash_u64s(&hs))
        })
        .collect()
}

pub fn changed_names(old: &FxHashMap<Ustr, u64>, new: &FxHashMap<Ustr, u64>) -> FxHashSet<Ustr> {
    old.keys()
        .chain(new.keys())
        .filter(|n| old.get(*n) != new.get(*n))
        .copied()
        .collect()
}

/// Parse `text` in its file's dialect and tag the module with its owning
/// package. Every session parse goes through here (or [`Session::parse_stored`],
/// which reads the facts back off the file): a re-parse that forgot them would
/// read a v1 file as legacy source, which is a parse error on `pub`/`mod` and
/// an unresolved name on everything that still parsed.
fn parse_with_facts(
    text: &str,
    file: Ustr,
    dialect: Dialect,
    package: Option<PackageTag>,
    global_policy: GlobalPolicy,
) -> Result<Ast, CompilerError> {
    let mut module = skull_parser::parse_module_with_dialect(text, file, dialect)?;
    module.package = package;
    module.global_policy = global_policy;
    Ok(module)
}

impl Session {
    /// Every macro visible in this session, user files AND stdlib, paired with
    /// its defining file. Macros are erased by expansion before bone lowering,
    /// so the workspace HIR has no record of them -- this is the only source.
    /// Consumers (completion, ctags, semantic tokens) must read it rather than
    /// `files` alone, or stdlib macros silently go missing.
    pub fn all_macro_defs(&self) -> impl Iterator<Item = (Ustr, &MacroDef)> {
        self.files
            .iter()
            .flat_map(|(path, uf)| uf.macros.iter().map(move |d| (Ustr::from(path), d)))
            .chain(self.warm_macros.iter().map(|(f, d)| (*f, d)))
    }

    /// `aliases` are the host package graph's dependency-alias tables, the same
    /// input `semantic_pass_with_aliases` takes. A legacy-only session passes
    /// the default (empty) table.
    pub fn new(
        warm: Vec<Ast>,
        files: Vec<SessionFile>,
        aliases: &bone_core::modules::NsAliases,
    ) -> Result<Self, CompilerError> {
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
            warm_names: FxHashSet::default(),
            warm_contributions: Vec::new(),
            declared_facts: FxHashMap::default(),
        };
        session.warm_contributions = session.warm.iter().map(contribute).collect();
        session.warm_names = session
            .warm_contributions
            .iter()
            .flat_map(|c| c.items.iter())
            .filter_map(|item| match item {
                bone_semantic::semantic_passes::ScopeItem::Def { name, .. } => Some(*name),
                _ => None,
            })
            .chain(bone_core::modules::std_exported_intrinsic_types().map(ustr))
            .collect();

        // Parse each file once for its prints/macros AND keep the module to feed
        // lowering directly -- avoids a second parse via `combined_unexpanded`,
        // which re-parses (the AST is transient and not persisted).
        let mut user_modules: Vec<Ast> = Vec::new();
        for SessionFile {
            path,
            text,
            dialect,
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

            let module = parse_with_facts(
                &text,
                ustr(path.as_str()),
                dialect,
                package.clone(),
                global_policy,
            )?;

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
            let scope = scope_hash(&module, &text);

            session.files.insert(
                path.clone(),
                UserFile {
                    text_hash: text_hash(&text),
                    item_prints: prints,
                    text,
                    macros,
                    dialect,
                    package,
                    global_policy,
                    syntax_errors: module.recovery_errors.clone(),
                    scope_hash: scope,
                    contribution: contribute(&module),
                },
            );

            session.paths.push(path);
            user_modules.push(module);
        }

        let mut unexpanded = session.warm.clone();
        unexpanded.extend(user_modules);
        session.registry = collect_registry(&unexpanded);
        reject_macro_name_collisions(&unexpanded)?;

        let registry = session.registry.clone();
        let (ws, _) = session.build(unexpanded, &registry)?;
        session.ws = ws;
        session.rebuild_dependents();

        Ok(session)
    }

    /// Re-parse every user file from its stored text and prepend the (immutable,
    /// pre-parsed) warm modules -- the un-expanded module set the full-rebuild
    /// path (`rebuild_users`) feeds to `expand_and_desugar`. The AST is transient
    /// (not persisted), so this re-parses; it runs only on the O(workspace)
    /// fallback, where a parse per file is in line with the from-scratch cost.
    pub fn combined_unexpanded(&self) -> Result<Vec<Ast>, CompilerError> {
        let mut combined = self.warm.clone();

        for path in &self.paths {
            combined.push(self.parse_stored(ustr(path))?);
        }

        Ok(combined)
    }

    /// The dialect and owning package recorded for `path`: a held file's own,
    /// else what the host declared for it. A path nobody declared is a
    /// standalone source: legacy, unowned. A caller that knows better supplies
    /// the facts up front, through [`SessionFile`] or
    /// [`Session::declare_file_facts`].
    fn facts_of(&self, path: &str) -> (Dialect, Option<PackageTag>, GlobalPolicy) {
        match self.files.get(path) {
            Some(f) => (f.dialect, f.package.clone(), f.global_policy),
            None => self.declared_facts.get(path).cloned().unwrap_or((
                Dialect::Legacy,
                None,
                GlobalPolicy::Any,
            )),
        }
    }

    /// Parse `text` as a buffer of `path`, with the dialect and package a
    /// stored copy of it carries. A recovery reparse of an editor buffer goes
    /// through here: parsed as legacy, a v1 file loses its module header and
    /// `use` items.
    pub fn parse_buffer(&self, path: &str, text: &str) -> Result<Ast, CompilerError> {
        let (dialect, package, global_policy) = self.facts_of(path);
        parse_with_facts(text, ustr(path), dialect, package, global_policy)
    }

    /// Declare the dialect and owning package of a file that enters the
    /// session after [`Session::new`] -- the editor opening a file the batch
    /// load did not see. Its manifest decides both, exactly as for a file of
    /// the initial set; without a declaration it would read as a standalone
    /// legacy source. A held file's facts cannot change: its package's
    /// manifest changed, which needs a new session.
    pub fn declare_file_facts(
        &mut self,
        path: &str,
        dialect: Dialect,
        package: Option<PackageTag>,
        global_policy: GlobalPolicy,
    ) -> Result<(), CompilerError> {
        if let Some(held) = self.files.get(path)
            && (held.dialect != dialect
                || held.package != package
                || held.global_policy != global_policy)
        {
            comp_error!(
                Span::unknown(),
                "bone session: `{}` is held as {:?} of {:?}; it cannot become {:?} of {:?} without a new session",
                path,
                held.dialect,
                held.package,
                dialect,
                package
            );
        }
        self.declared_facts
            .insert(path.to_string(), (dialect, package, global_policy));
        Ok(())
    }

    /// Re-parse `path` from its stored text with its own facts.
    fn parse_stored(&self, path: Ustr) -> Result<Ast, CompilerError> {
        let (dialect, package, global_policy) = self.facts_of(path.as_str());
        parse_with_facts(
            &self.files[path.as_str()].text,
            path,
            dialect,
            package,
            global_policy,
        )
    }

    pub fn workspace(&self) -> &Workspace {
        &self.ws
    }

    /// Run `f` against the workspace with `path`'s text temporarily replaced by
    /// `variant`, then restore the original state. The speculative-analysis
    /// primitive for IDE paths (completion munges): the variant goes through the
    /// normal update tiers, so a body-local munge costs one body re-inference --
    /// not the from-scratch project pass `combined_unexpanded` + `semantic_pass`
    /// would be.
    ///
    /// An UNTRACKED `path` (a file this session refused because its live text
    /// never lowered clean, or one it has never seen) is speculatively tracked
    /// for the duration of `f` and removed again -- the session ends as
    /// untracked as it started.
    ///
    /// Not panic-safe: a panicking `f` leaves the variant applied (the IDE
    /// queries this serves don't panic). A failed variant update is restored
    /// before the error propagates; a failed RESTORE propagates itself -- the
    /// original text round-tripped before, so that failure is a real bug and
    /// must not be swallowed.
    pub fn with_file_variant<R>(
        &mut self,
        path: &str,
        variant: &str,
        f: impl FnOnce(&Workspace) -> R,
    ) -> Result<R, CompilerError> {
        let Some(original) = self.files.get(path).map(|uf| uf.text.clone()) else {
            self.update_file(path, variant)?;
            let out = f(&self.ws);
            self.remove_file(path)?;
            return Ok(out);
        };
        if let Err(e) = self.update_file(path, variant) {
            self.update_file(path, &original)?;
            return Err(e);
        }
        let out = f(&self.ws);
        self.update_file(path, &original)?;
        Ok(out)
    }

    pub fn update_file(&mut self, path: &str, text: &str) -> Result<UpdateResult, CompilerError> {
        let result = self.update_file_bodies(path, text)?;
        if result.tier != UpdateTier::NoChange {
            // Whole-program: an edited callee can change an unchanged caller's
            // verdict.
            bone_semantic::relocation::check_relocation(&mut self.ws);
            bone_semantic::fn_identity::check_generic_fn_eq(&mut self.ws);
        }
        Ok(result)
    }

    fn update_file_bodies(
        &mut self,
        path: &str,
        text: &str,
    ) -> Result<UpdateResult, CompilerError> {
        // Reset the work counters so callers can read EXACTLY how many bodies
        // this update re-inferred (check_module_incremental bumps it) and
        // re-lowered (the lowering passes bump it).
        self.ws
            .reinferred_body_count
            .store(0, std::sync::atomic::Ordering::Relaxed);
        self.ws
            .relowered_body_count
            .store(0, std::sync::atomic::Ordering::Relaxed);
        let hash = text_hash(text);
        let previous_text = self.files.get(path).map(|file| file.text.clone());

        if let Some(existing) = self.files.get(path) {
            if existing.text_hash == hash {
                return Ok(UpdateResult {
                    tier: UpdateTier::NoChange,
                    reinferred: Vec::new(),
                    relowered: Vec::new(),
                    ..UpdateResult::default()
                });
            }
        }

        // Fast path: a single edit localized to one Rock-bodied function's
        // interior re-parses ONLY that item (no whole-file reparse/clone). Bails
        // (Ok(None)) to the full parse below for anything non-localized; the
        // re-parsed item's interface hash is re-checked, so a mis-localized
        // interface edit also falls through.
        if let Some(result) = self.try_incremental_body_edit(path, text, hash)? {
            return Ok(result);
        }

        let (dialect, package, global_policy) = self.facts_of(path);
        let module = parse_with_facts(text, ustr(path), dialect, package.clone(), global_policy)?;
        let syntax_errors = module.recovery_errors.clone();
        let mid = self.ws.intern_module(module.file);
        if self.warm_mids.contains(&mid) {
            // Opening a warm (stdlib) file: it IS the warm module, not a second
            // copy. No-op rather than adding a duplicate that would self-collide
            // on every stdlib type. Queries on this path resolve to the warm
            // module; its real diagnostics (if any) come from the warm build.
            return Ok(UpdateResult {
                tier: UpdateTier::NoChange,
                reinferred: Vec::new(),
                relowered: Vec::new(),
                ..UpdateResult::default()
            });
        }

        let new_prints = item_prints(&module, text);
        let new_macros = macro_defs(&module);
        let new_scope = scope_hash(&module, text);
        let new_contribution = contribute(&module);
        let tracked = self.files.contains_key(path);

        // An edit that changes what the namespace tree, the fragment scopes
        // or the legacy projections are built from takes the scope path:
        // `UserFile::scope_hash`, a legacy definition whose name a legacy
        // compatibility view also names (a legacy lookup of that name reads
        // the legacy-owned facts), or a legacy definition of a provider a v1
        // package sees through its namespace view.
        let scope_changed = self
            .files
            .get(path)
            .is_some_and(|old| old.scope_hash != new_scope)
            || (dialect == Dialect::Legacy
                && tracked
                && (self.changes_a_projected_collision(path, &module)
                    || self.changes_a_legacy_view(path, package.as_ref(), &module)));

        let diff = (tracked && !module.items.is_empty() && !scope_changed)
            .then(|| classify(&self.files[path].item_prints, &new_prints));

        let changed: FxHashSet<Ustr> = match self.files.get(path) {
            Some(old) => changed_names(&name_shapes(&old.item_prints), &name_shapes(&new_prints)),
            None => FxHashSet::default(),
        };
        // A macro change rewrites its users' EXPANSION, so they must be
        // re-lowered (re-expanded), which only the full rebuild does; so does
        // a function whose name a macro also has, which the whole-program
        // collision check must refuse.
        let touches_macros = tracked
            && (touches_macro(&self.files[path].macros, &new_macros, &changed)
                || self.defines_a_function_named_like_a_macro(&module));

        let previous = self.files.insert(
            path.to_string(),
            UserFile {
                text_hash: hash,
                item_prints: new_prints,
                text: text.to_string(),
                macros: new_macros,
                // An edit never changes which package owns a file or which
                // dialect it is written in; both come from its manifest.
                dialect,
                package,
                global_policy,
                syntax_errors,
                scope_hash: new_scope,
                contribution: new_contribution,
            },
        );

        if previous.is_none() {
            self.paths.push(path.to_string());
        }

        let restore = move |s: &mut Self| match previous {
            Some(prev) => {
                s.files.insert(path.to_string(), prev);
            }

            None => {
                s.files.remove(path);
                s.paths.retain(|p| p != path);
            }
        };

        // The freshly parsed `module` is threaded into the chosen rebuild path
        // rather than stored: the AST is transient, so the rebuilder cooks+lowers
        // this owned copy directly (no clone-from-storage, no persisted AST).
        let span_only = matches!(&diff, Some(FileDiff::LayoutOnly));
        let scope_path = tracked && scope_changed && !module.items.is_empty() && !touches_macros;
        let mut result = match diff {
            Some(FileDiff::BodiesOnly(changed)) => {
                if changed.len() == 1 {
                    let idx = changed[0];

                    self.rebuild_or_revert(
                        "update",
                        path,
                        move |s| s.rebuild_body_incremental(path, module, idx),
                        restore,
                    )
                } else {
                    self.rebuild_or_revert(
                        "update",
                        path,
                        move |s| s.rebuild_module(path, module, &changed),
                        restore,
                    )
                }
            }

            Some(FileDiff::LayoutOnly) => self.rebuild_or_revert(
                "update",
                path,
                move |s| s.rebuild_module(path, module, &[]),
                restore,
            ),

            Some(FileDiff::Interface) if !touches_macros => self.rebuild_or_revert(
                "update",
                path,
                move |s| {
                    s.rebuild_interface(
                        module,
                        &changed,
                        ScopeDelta::default(),
                        UpdateTier::Interface,
                    )
                },
                restore,
            ),

            _ if scope_path => self.rebuild_or_revert(
                "update",
                path,
                move |s| s.rebuild_scope(module, &changed),
                restore,
            ),

            // Everything else rebuilds every user file, on purpose: a file the
            // session did not hold (it is a new fragment, so every lookup
            // anywhere may now reach it, and nothing recorded a probe on it),
            // a file with no items left, and a macro-touching edit (its users
            // must re-expand).
            _ => self.rebuild_or_revert("update", path, |s| s.rebuild_users(), restore),
        }?;
        if result.tier == UpdateTier::BodyOnly && result.body_update.edit.is_none() {
            result.body_update = BodyUpdate {
                edit: previous_text
                    .as_deref()
                    .map(|old| text_edit_delta(path, old, text)),
                semantic_bodies: if !span_only {
                    result.reinferred.clone()
                } else {
                    Default::default()
                },
                span_only,
            };
        }
        Ok(result)
    }

    /// Whether a legacy file's edit adds or removes a definition -- or
    /// changes its kind -- whose bare name some legacy compatibility view also
    /// names. Legacy resolution of that name is decided when a caller LOWERS
    /// (ambiguous, a merged callable family, or the v1 definition alone), so
    /// the callers that looked it up have to re-lower: the scope path.
    fn changes_a_projected_collision(&self, path: &str, module: &Ast) -> bool {
        !self
            .projected_legacy_owned_changes(ustr(path), module)
            .is_empty()
    }

    /// Whether a legacy file's edit adds, removes or re-kinds a definition of
    /// a provider some v1 package depends on: its namespace view
    /// holds what the provider's files define, and v1 lookups through it are
    /// decided at lowering.
    fn changes_a_legacy_view(
        &self,
        path: &str,
        package: Option<&PackageTag>,
        module: &Ast,
    ) -> bool {
        let Some(tag) = package else { return false };
        let viewed = self
            .ws
            .ns
            .package(tag.id)
            .is_some_and(|pkg| pkg.node(ustr("")).is_some());
        if !viewed {
            return false;
        }
        let old: FxHashSet<(Ustr, bone_core::modules::DefKind)> = self
            .ws
            .ns
            .legacy_owned
            .names_defined_by(ustr(path))
            .iter()
            .copied()
            .collect();
        let new: FxHashSet<(Ustr, bone_core::modules::DefKind)> =
            bone_semantic::semantic_passes::legacy_owned_names(module)
                .into_iter()
                .collect();
        old != new
    }

    /// The bare names the legacy file `file` starts or stops defining (or
    /// re-kinds) by becoming `module` that some legacy compatibility view
    /// also names.
    fn projected_legacy_owned_changes(&self, file: Ustr, module: &Ast) -> FxHashSet<Ustr> {
        if self.ws.legacy_projections.is_empty() && self.ws.legacy_imports.is_empty() {
            return FxHashSet::default();
        }
        let old: FxHashSet<(Ustr, bone_core::modules::DefKind)> = self
            .ws
            .ns
            .legacy_owned
            .names_defined_by(file)
            .iter()
            .copied()
            .collect();
        let new: FxHashSet<(Ustr, bone_core::modules::DefKind)> =
            bone_semantic::semantic_passes::legacy_owned_names(module)
                .into_iter()
                .collect();
        old.symmetric_difference(&new)
            .map(|(name, _)| *name)
            .filter(|name| {
                self.ws
                    .legacy_projections
                    .values()
                    .any(|view| view.by_name.contains_key(name))
                    || self
                        .ws
                        .legacy_imports
                        .values()
                        .any(|imports| imports.mentions(*name))
            })
            .collect()
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

    /// Build every module into a new workspace from `unexpanded` (the warm
    /// modules followed by the user files, un-expanded): the namespace tree
    /// from every file's contribution, expansion against `registry`, lowering
    /// and inference, and the whole-program API-closure gate -- the pass
    /// order `Session::new` and `rebuild_users` share with the batch
    /// `semantic_pass`. The tree comes FIRST, because it turns v1 headers,
    /// inline `mod` blocks and `use` declarations into the scopes lowering
    /// reads. When the warm modules are independent of the user files
    /// ([`WarmBase`]) the build starts from their warm-alone state and lowers
    /// only the user files. Returns the workspace and the user modules' ids.
    fn build(
        &mut self,
        mut unexpanded: Vec<Ast>,
        registry: &MacroRegistry,
    ) -> Result<(Workspace, Vec<ModuleId>), CompilerError> {
        let warm_len = self.warm.len();
        // Read from the ASTs being built, not the stored `UserFile`s, so the
        // build is a function of the texts alone.
        let users = &unexpanded[warm_len..];
        let user_contributions: Vec<FragmentContribution> = users.iter().map(contribute).collect();
        let independent = users
            .iter()
            .zip(&user_contributions)
            .all(|(ast, contribution)| self.apart_from_warm(ast, contribution));
        let base = if independent {
            self.warm_alone(registry).cloned()
        } else {
            None
        };
        let (mut ws, mut lowered, first_user) = match base {
            Some(base) => (base, unexpanded.split_off(warm_len), 0),
            None => (Workspace::default(), unexpanded, warm_len),
        };
        let contributions: Vec<&FragmentContribution> = self
            .warm_contributions
            .iter()
            .chain(&user_contributions)
            .collect();
        bone_semantic::semantic_passes::assemble_namespaces(
            &mut ws,
            &contributions,
            &self.aliases,
        )?;
        lowered
            .par_iter_mut()
            .for_each(|ast| cook_one(ast, registry));
        let mids = lower_and_infer(&mut ws, &lowered)?;
        // The same whole-program gate the batch path runs, so an export whose
        // type closure is not public fails in a session too rather than only
        // under `rock compile`. It is a no-op for a workspace with no v1
        // package.
        bone_semantic::semantic_passes::pass_validate_api_closure(&ws)?;
        ws.phase_timings.clear();
        Ok((ws, mids[first_user..].to_vec()))
    }

    /// The warm-alone workspace, built on first need; `None` when the warm
    /// modules alone are refused or report a diagnostic (see [`WarmBase`]).
    fn warm_alone(&mut self, registry: &MacroRegistry) -> Option<&Workspace> {
        if matches!(self.warm_base, WarmBase::Unbuilt) {
            let mut ws = Workspace::default();
            let mut cooked = self.warm.clone();
            cooked
                .par_iter_mut()
                .for_each(|ast| cook_one(ast, registry));
            // A refusal here is not the build's verdict: the warm modules may
            // name a user definition, and the combined build decides.
            let built = lower_and_infer(&mut ws, &cooked).is_ok() && body_errors(&ws).is_empty();
            ws.phase_timings.clear();
            self.warm_base = if built {
                WarmBase::Clean(Box::new(ws))
            } else {
                WarmBase::Unusable
            };
        }
        match &self.warm_base {
            WarmBase::Clean(ws) => Some(ws),
            WarmBase::Unbuilt | WarmBase::Unusable => None,
        }
    }

    /// Whether the user file `ast` defines nothing, and no macro, under a
    /// name a warm module defines (or an intrinsic type name): a legacy
    /// file's items, a v1 file's bare-keyed operator functions, and any
    /// file's macros. A v1 file's other definitions are keyed by DefKey and
    /// cannot collide.
    fn apart_from_warm(&self, ast: &Ast, contribution: &FragmentContribution) -> bool {
        use bone_semantic::semantic_passes::ScopeItem;
        fn apart(items: &[ScopeItem], legacy: bool, warm: &FxHashSet<Ustr>) -> bool {
            items.iter().all(|item| match item {
                ScopeItem::Def { name, kind, .. } => {
                    let bare =
                        legacy || (*kind == "callable" && name.as_str().starts_with("__op_"));
                    !(bare && warm.contains(name))
                }
                ScopeItem::Module { items, .. } => apart(items, legacy, warm),
                ScopeItem::Use(_) => true,
            })
        }
        apart(
            &contribution.items,
            ast.dialect == Dialect::Legacy,
            &self.warm_names,
        ) && macro_defs(ast)
            .iter()
            .all(|m| !self.warm_names.contains(&m.name))
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

    /// The namespace facts `file`'s last lowering read, or `None`
    /// for a file the session does not hold.
    pub fn scope_probes(&self, file: &str) -> Option<&FxHashSet<ScopeProbe>> {
        let mid = self.ws.module_ids.get(&ustr(file))?;
        self.ws.modules.get(mid.index()).map(|m| &m.scope_probes)
    }

    /// The scope path: re-assemble the namespace tree from the
    /// stored contributions (the edited file's is already the new one), diff
    /// it against the current tree, commit it, and run the interface cascade
    /// with every module whose lowering read a changed fact added to the
    /// re-lower set. The warm modules never read a user fact.
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

        let mut delta = ScopeDelta::between(&self.ws, &next);
        if module.dialect == Dialect::Legacy {
            delta
                .legacy
                .extend(self.projected_legacy_owned_changes(module.file, &module));
        }

        self.ws.ns.packages = next.ns.packages;
        self.ws.defs = next.defs;
        self.ws.fragment_scopes = next.fragment_scopes;
        self.ws.legacy_projections = next.legacy_projections;
        self.ws.legacy_imports = next.legacy_imports;

        self.rebuild_interface(module, changed, delta, UpdateTier::Scope)
    }

    /// User modules other than `except` whose lowering read a fact in `delta`.
    fn modules_reading(&self, delta: &ScopeDelta, except: ModuleId) -> FxHashSet<ModuleId> {
        if delta.is_empty() {
            return FxHashSet::default();
        }
        self.ws
            .modules
            .iter()
            .filter(|m| {
                m.id != except && m.id != ModuleId::builtin() && !self.warm_mids.contains(&m.id)
            })
            .filter(|m| {
                delta.fragments.contains(&m.file) || m.scope_probes.iter().any(|p| delta.hits(p))
            })
            .map(|m| m.id)
            .collect()
    }

    /// The interface cascade: re-lower the edited module and the modules its
    /// edit can re-resolve, re-infer the body-dependents of what changed.
    /// `delta` holds namespace facts that changed already (the scope path's
    /// tree diff); this adds the edited module's own nominal-type facts, and
    /// every module whose lowering read one of them re-lowers too.
    fn rebuild_interface(
        &mut self,
        module: Ast,
        changed: &FxHashSet<Ustr>,
        mut delta: ScopeDelta,
        tier: UpdateTier,
    ) -> Result<UpdateResult, CompilerError> {
        let file = module.file;
        let mid = self.ws.module_ids[&file];

        // --- Interface-dependent closure to RE-LOWER ---
        // A module needs RE-LOWERING (re-resolving its signatures) only when a
        // type it references in INTERFACE position stops resolving the same
        // way: a type this edit removed or renamed, or one whose generic arity
        // or trailing defaults changed (checked at LOWER time, in
        // `lower_type_expr`), or whose flavor changed. A type whose *shape*
        // changed (field add/retype) keeps all of that, so dependents'
        // signatures still resolve it; those need only re-inference, handled
        // below. Since a dependent's TEXT is unchanged, re-lowering it never
        // removes a name, so the re-lower set is one level deep.
        //
        // Both sides are keyed the way the workspace maps key a type: the
        // DefKey in a v1 fragment, the bare name in a legacy file. The old
        // facts are the maps' (still pre-edit here; `lower_modules` below
        // repopulates them), the new ones the fresh AST's.
        let old_types: FxHashMap<Ustr, bone_semantic::semantic_passes::NominalFacts> = {
            use bone_semantic::semantic_passes::{NominalFacts, NominalFlavor};
            let facts = |key: &Ustr, flavor| NominalFacts {
                flavor,
                arity: self.ws.type_generic_arity.get(key).copied().unwrap_or(0),
                defaults: self
                    .ws
                    .type_generic_defaults
                    .get(key)
                    .cloned()
                    .unwrap_or_default(),
            };
            self.ws
                .struct_by_name
                .iter()
                .filter(|(_, id)| id.module == mid)
                .map(|(n, _)| (*n, facts(n, NominalFlavor::Struct)))
                .chain(
                    self.ws
                        .enums_by_name
                        .iter()
                        .filter(|(_, id)| id.module == mid)
                        .map(|(n, _)| (*n, facts(n, NominalFlavor::Enum))),
                )
                .chain(
                    self.ws
                        .newtypes_by_name
                        .iter()
                        .filter(|(_, id)| id.module == mid)
                        .map(|(n, _)| (*n, facts(n, NominalFlavor::Newtype))),
                )
                .collect()
        };
        let new_types = bone_semantic::semantic_passes::nominal_type_facts(
            self.ws.fragment_scopes.get(&file),
            &module,
        );
        let changed_types: Vec<Ustr> = old_types
            .iter()
            .filter(|(key, facts)| new_types.get(*key) != Some(*facts))
            .map(|(key, _)| *key)
            .chain(
                new_types
                    .keys()
                    .filter(|key| !old_types.contains_key(*key))
                    .copied(),
            )
            .collect();

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
        // Plus every module currently carrying an interface error, so a type that
        // (re)appears with this edit re-lowers its previously-dangling users and
        // clears them. User modules only: warm/builtin are never keyed in
        // `self.files`, so a warm module reaching the re-parse loop would panic.
        for m in self.ws.modules.iter() {
            if m.id != mid
                && m.id != ModuleId::builtin()
                && !self.warm_mids.contains(&m.id)
                && !m.interface_errors.is_empty()
            {
                to_relower.insert(m.id);
            }
        }

        // Re-parse each closure module from its stored text (the edited module
        // uses the provided AST); cook + lower them together with the edited
        // module so intra-closure and closure->kept references resolve.
        let mut edited = module;
        self.cook_edited(&mut edited)?;
        let mut asts: Vec<Ast> = Vec::with_capacity(to_relower.len() + 1);
        asts.push(edited);
        for &m in &to_relower {
            let path = self.ws.module(m).file;
            let mut a = self.parse_stored(path)?;
            cook_one(&mut a, &self.registry);
            asts.push(a);
        }

        self.retain_modules(|m| m != mid && !to_relower.contains(&m));
        self.ws.phase_timings.clear();

        let f_mids = lower_modules(&mut self.ws, &asts)?;

        // Names whose definition may have shifted: the edited module's interface
        // diff PLUS every name exported by a RE-LOWERED module (re-lowering may
        // have changed any of its signatures / fields -- e.g. `Wrap { inner: T }`
        // becoming `inner: Error` when `T` was removed). Body-dependents of any of
        // these must re-infer. Re-INFERRED-only modules don't propagate: their
        // interface is unchanged, so their exported definitions are stable.
        let mut reinfer_names: FxHashSet<Ustr> = changed.clone();
        // A legacy name whose lookup outcome may have moved: a body reading it
        // at inference (a merged family's members) re-infers.
        reinfer_names.extend(delta.legacy.iter().copied());
        for &m in &f_mids {
            let module = self.ws.module(m);
            reinfer_names.extend(exported_type_names(module));
            reinfer_names.extend(module.functions_by_name.keys().copied());
            reinfer_names.extend(module.global_by_name.keys().copied());
        }

        // Re-infer: body-dependents of `reinfer_names` (their TEXT is unchanged, so
        // body-granular reuse applies), every module with a stale inference error,
        // AND every re-lowered module (its arena shifted, so its cached inference --
        // already cleared by `retain_modules` -- can't be reused).
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

        // Body-granular re-inference. A dependent module's TEXT is unchanged, so
        // only its bodies whose `deps` reference a changed name can have changed
        // inference -- the rest reuse their cached BodyInference (their generation
        // still matches). Take each module's cached inference and keep only the
        // reusable bodies as `prev`; `check_module_incremental` then re-runs
        // check_function for exactly the dropped (dep-touching or previously-
        // errored) bodies and reuses the rest. This is sound by the same relation
        // the module-level `dependents` index already trusts: a body with deps
        // disjoint from `changed` is no more affected than one in a module that
        // wasn't selected at all (which is never re-inferred). Globals/decorators/
        // struct-defaults always re-check inside check_module_incremental.
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

        // `ids` is the full re-checked set and already contains `f_mids`, so it
        // covers `reinferred`. `relowered` is precisely the re-LOWERED modules
        // (`f_mids`) -- re-inferred-only dependents kept their arenas, so listing
        // them as relowered would be wrong.
        let reinferred = self.collect_reinferred(&ids);
        let relowered = self.collect_reinferred(&f_mids);

        Ok(UpdateResult {
            tier,
            reinferred,
            relowered,
            ..UpdateResult::default()
        })
    }

    pub fn remove_file(&mut self, path: &str) -> Result<UpdateResult, CompilerError> {
        let Some(previous) = self.files.remove(path) else {
            return Ok(UpdateResult {
                tier: UpdateTier::NoChange,
                reinferred: Vec::new(),
                relowered: Vec::new(),
                ..UpdateResult::default()
            });
        };

        let pos = self
            .paths
            .iter()
            .position(|p| p == path)
            .expect("tracked file must be in path order");

        self.paths.remove(pos);

        let restore = move |s: &mut Self| {
            s.files.insert(path.to_string(), previous);
            s.paths.insert(pos, path.to_string());
        };

        self.rebuild_or_revert("removal", path, |s| s.rebuild_users(), restore)
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
        let doomed: Vec<Ustr> = self
            .ws
            .type_generic_owner
            .iter()
            .filter(|(_, owner)| !keep(**owner))
            .map(|(name, _)| *name)
            .collect();
        for name in doomed {
            self.ws.type_generic_owner.remove(&name);
            self.ws.type_generic_arity.remove(&name);
            self.ws.type_generic_defaults.remove(&name);
        }

        self.ws.functions_by_name.retain(|_, fns| {
            fns.retain(|(mid, _)| keep(*mid));
            !fns.is_empty()
        });

        self.ws.globals_by_name.clear();
        // `const_globals` accumulates the same way and must be rebuilt in lockstep:
        // it's an insert-only set (`lower_modules` never removes), so without this a
        // global flipped `const` -> mutable (or a deleted const) would stay in the
        // set and keep producing a phantom "cannot assign to const" diagnostic until
        // an LSP restart. `lower_modules` re-adds the edited/relowered modules' consts;
        // this re-adds the KEPT ones, matching `globals_by_name`.
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
        // Build into a LOCAL workspace and commit to `self.ws` only once lowering
        // and inference have BOTH succeeded. `expand_and_desugar` / `lower_and_infer`
        // intern modules (bumping `module_ids`) well before `insert_module` grows the
        // `modules` vec, so a mid-build `Err` used to leave `self.ws` half-built --
        // `module_ids` running ahead of `modules` -- and the next `ws.module(mid)`
        // panicked with `len is 1 but index is N`. This bit during fast typing: a
        // broken `global x =` fails the rebuild, `rebuild_or_revert` reverts to the
        // PREVIOUS keystroke's text (also broken mid-word), so the recovery rebuild
        // failed too and the corrupted ws stuck around for every later query. Keeping
        // the last-good ws on any failure makes the session queryable after a failed
        // rebuild instead of crash-on-next-touch.
        let unexpanded = self.combined_unexpanded()?;
        let registry = collect_registry(&unexpanded);
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
    pub fn rebuild_dependents(&mut self) {
        self.dependents.clear();
        self.module_dep_names.clear();
        self.dependents_iface.clear();

        for (mid, inf) in self.ws.inferences_iter() {
            if mid == ModuleId::builtin() || self.warm_mids.contains(&mid) {
                continue;
            }

            let names: FxHashSet<Ustr> = inf
                .per_fn
                .values()
                .flat_map(|b| b.deps.iter().copied())
                .collect();

            for name in &names {
                self.dependents.entry(*name).or_default().insert(mid);
            }
            self.module_dep_names.insert(mid, names);
        }

        // Interface-position references: each nominal type name a module's
        // INTERFACE arena mentions (signatures / globals / fields / enum payloads).
        // Cache from the lowered HIR, not inference -- a module with no inference
        // (e.g. a body-error-only module) still has interface deps. Kept interfaces
        // need no arena walk; `retain_modules` invalidates the replaced ones.
        for module in self.ws.modules.iter() {
            let mid = module.id;
            if mid == ModuleId::builtin() || self.warm_mids.contains(&mid) {
                continue;
            }
            let names = self
                .module_iface_dep_names
                .entry(mid)
                .or_insert_with(|| iface_dep_names(module));
            for &name in names.iter() {
                self.dependents_iface.entry(name).or_default().insert(mid);
            }
        }
    }

    /// Incremental dep-index update for ONE re-inferred module: diff its new dep
    /// names against the cached forward index and patch only the changed reverse
    /// entries. O(mid's deps), not O(workspace) -- the body-edit fast path. Sound
    /// because a body edit changes ONLY the edited module's outgoing deps (its
    /// interface is unchanged, so no other module's deps move).
    pub fn update_dependents_for(&mut self, mid: ModuleId) {
        if mid == ModuleId::builtin() || self.warm_mids.contains(&mid) {
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

    pub fn module_deps(&self, mid: ModuleId) -> FxHashSet<Ustr> {
        self.ws
            .inference(mid)
            .into_iter()
            .flat_map(|inf| inf.per_fn.values())
            .flat_map(|b| b.deps.iter().copied())
            .collect()
    }

    pub fn rebuild_module(
        &mut self,
        path: &str,
        ast: Ast,
        changed: &[usize],
    ) -> Result<UpdateResult, CompilerError> {
        let file = ast.file;
        let mid = self.ws.module_ids[&file];

        #[cfg(debug_assertions)]
        let old_iface = Some(interface_snapshot(&self.ws, mid));

        // TODO: maybe we can come up with an overload sig id that's derived from name + types and
        // not Idx<Function> for persistent ids, but maybe not needed for now
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

        self.retain_modules(|m| m != mid);
        self.ws.phase_timings.clear();

        let mut edited = ast;
        self.cook_edited(&mut edited)?;
        lower_modules(&mut self.ws, &[edited])?;

        let changed_owners: FxHashSet<BodyOwner> = {
            let prints = &self.files[path].item_prints;
            let module = self.ws.module(mid);

            changed
                .iter()
                .filter_map(|&i| prints.get(i).and_then(|p| module.body_at(p.offset)))
                .collect()
        };

        {
            let module = self.ws.module_mut(mid);

            let fids: Vec<Idx<Function>> = module.functions.iter().map(|(fid, _)| fid).collect();

            for fid in fids {
                let owner = BodyOwner::Function(fid);

                if let Some(body) = module.bodies.get_mut(&owner) {
                    let old = old_gens.get(&fid).copied().unwrap_or(0);
                    body.generation = if changed_owners.contains(&owner) {
                        old + 1
                    } else {
                        old
                    };
                }
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
        if let Some(old) = old_iface {
            debug_assert_eq!(
                old,
                interface_snapshot(&self.ws, mid),
                "relower of `{path}` changed the module interface, classify mis-tiered an interface edit"
            )
        }

        self.rebuild_dependents();

        let prints = &self.files[path].item_prints;
        let module = self.ws.module(mid);

        let reinferred: Vec<BodyId> = changed
            .iter()
            .filter_map(|&i| {
                prints
                    .get(i)
                    .and_then(|p| module.body_at(p.offset))
                    .map(|owner| (mid, owner))
            })
            .collect();

        let relowered = self.collect_reinferred(&[mid]);

        Ok(UpdateResult {
            tier: UpdateTier::BodyOnly,
            reinferred,
            relowered,
            ..UpdateResult::default()
        })
    }

    /// Fast path for a single body edit: re-parse ONLY the edited item (no
    /// whole-file reparse), feed it to the per-body lowerer, and patch the kept
    /// state. Returns `Ok(None)` to bail to the full-parse path for ANY edit that
    /// is not a clean single-Rock-body-interior change -- so this never produces a
    /// wrong result, only ever skips work.
    fn try_incremental_body_edit(
        &mut self,
        path: &str,
        new_text: &str,
        new_hash: u64,
    ) -> Result<Option<UpdateResult>, CompilerError> {
        // The fast path carries the file's syntax errors forward UNCHANGED (it
        // re-parses only one clean body). That's wrong when the old state had a
        // recovery error -- this very edit may be FIXING it, so the stale error
        // would survive (a `... as in` -> `... as int` keystroke left a phantom
        // "unsupported type" diagnostic until LSP restart). Whenever the file
        // currently has syntax errors, take the full path, which recomputes them.
        if self
            .files
            .get(path)
            .is_some_and(|f| !f.syntax_errors.is_empty())
        {
            return Ok(None);
        }
        // --- locate (non-mutating; releases the files borrow before any change) ---
        let (
            es,
            oe,
            delta,
            idx,
            so,
            eo,
            line,
            col,
            old_iface,
            old_name,
            old_text,
            old_prints,
            old_macros,
            old_syntax_errors,
            old_dialect,
            old_package,
            old_global_policy,
            old_scope_hash,
            old_contribution,
        ) = {
            let Some(old) = self.files.get(path) else {
                return Ok(None);
            };
            let (es, oe, delta) = byte_diff(&old.text, new_text);
            let Some(idx) = locate_body_edit(&old.item_prints, es, oe) else {
                return Ok(None);
            };
            let p = &old.item_prints[idx];
            (
                es,
                oe,
                delta,
                idx,
                p.item_span.offset_range.0,
                p.item_span.offset_range.1,
                p.item_span.line_range.0,
                p.item_span.col_range.0,
                p.iface_hash,
                p.name,
                old.text.clone(),
                old.item_prints.clone(),
                old.macros.clone(),
                old.syntax_errors.clone(),
                old.dialect,
                old.package.clone(),
                old.global_policy,
                old.scope_hash,
                old.contribution.clone(),
            )
        };
        let _ = (es, oe);

        // Re-parse just this item with absolute spans (transient mini-Ast).
        let new_end = (eo as isize + delta) as usize;
        if new_end > new_text.len() || so > new_end {
            return Ok(None);
        }
        let sub = &new_text[so..new_end];
        let Ok(mut mini) =
            skull_parser::parse_one_item(sub, ustr(path), so, line, col, self.facts_of(path).0)
        else {
            return Ok(None); // a hard tokenizer error -> let the full path surface it
        };
        // The mini-Ast lowers with the same unit facts as a full reparse would.
        // This path bails unless the item is exactly one function, so no global
        // is re-lowered here -- but a mini-Ast that silently claimed `game`
        // would be a trap the day that changes.
        mini.package = old_package.clone();
        mini.global_policy = old_global_policy;
        // The parse is total now; a recovered (partial) item must NOT be installed
        // in place. Bail to the full reparse, which keeps the best-effort tree and
        // records the recovery error as a diagnostic.
        if !mini.recovery_errors.is_empty() {
            return Ok(None);
        }
        self.cook_edited(&mut mini)?;

        // The re-parsed item must be exactly one function with the SAME name and
        // interface; otherwise the "body edit" actually moved the interface and
        // the full path must classify/route it.
        let mut mini_prints = item_prints(&mini, new_text);
        if mini_prints.len() != 1 {
            return Ok(None);
        }
        let np = mini_prints.pop().unwrap();
        if np.kind != ItemKindTag::Fn || np.name != old_name || np.iface_hash != old_iface {
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
        let new_fn_span = match &mini.items[0].kind {
            ItemKind::Function(f) => f.span,
            _ => return Ok(None),
        };
        let at = old_fn_span.offset_range.1;
        let byte_delta = new_fn_span.offset_range.1 as isize - old_fn_span.offset_range.1 as isize;
        let line_delta = new_fn_span.line_range.1 as isize - old_fn_span.line_range.1 as isize;

        // --- commit: install the new prints, then relower under rebuild_or_revert
        // (on failure it restores the old file + full-rebuilds from text). ---
        let new_prints = build_shifted_prints(old_prints, idx, np, delta, line_delta);
        // Keep macro def-lines accurate: shift those declared AFTER the edited
        // item by the same delta the prints got (the edited item is a non-macro
        // fn, so a macro is wholly before or wholly after its `eo` end offset).
        let mut new_macros = old_macros;
        for md in new_macros.iter_mut() {
            if md.span.offset_range.0 >= eo {
                shift_span(&mut md.span, delta, line_delta);
            }
        }
        let previous = self.files.insert(
            path.to_string(),
            UserFile {
                text_hash: new_hash,
                item_prints: new_prints,
                text: new_text.to_string(),
                macros: new_macros,
                dialect: old_dialect,
                package: old_package,
                global_policy: old_global_policy,
                // The fast path only re-parses ONE clean body (it bails to the
                // full path on any recovery error), so the file's syntax errors
                // are unchanged by this localized edit.
                syntax_errors: old_syntax_errors,
                scope_hash: old_scope_hash,
                // The edit is inside one function body, which contributes
                // nothing to the namespace tree.
                contribution: old_contribution,
            },
        );
        let restore = move |s: &mut Self| {
            if let Some(prev) = previous {
                s.files.insert(path.to_string(), prev);
            }
        };
        let mut result = self.rebuild_or_revert(
            "update",
            path,
            move |s| s.commit_body_relower(mid, owner, fid, mini, at, byte_delta, line_delta),
            restore,
        )?;
        result.body_update = BodyUpdate {
            edit: Some(text_edit_delta(path, &old_text, new_text)),
            semantic_bodies: result.reinferred.clone(),
            span_only: false,
        };
        Ok(Some(result))
    }

    /// The mutating half of a fast body edit: span-shift the kept bodies, relower
    /// the one edited function from `mini` (a single-item Ast), re-infer, patch
    /// deps. Mirrors `rebuild_body_incremental`'s commit half but fed the mini-Ast
    /// rather than a cooked whole-module clone.
    fn commit_body_relower(
        &mut self,
        mid: ModuleId,
        owner: BodyOwner,
        fid: Idx<Function>,
        mini: Ast,
        at: usize,
        byte_delta: isize,
        line_delta: isize,
    ) -> Result<UpdateResult, CompilerError> {
        let old_gen = self
            .ws
            .module(mid)
            .bodies
            .get(&owner)
            .map(|b| b.generation)
            .unwrap_or(0);

        let item = &mini.items[0];
        let ItemKind::Function(f) = &item.kind else {
            comp_error!(
                Span::unknown(),
                "skull incremental: mini-Ast item is not a function"
            );
        };
        let annotations = item.annotations.as_slice();

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
            &mini,
            f,
            annotations,
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

        let body = vec![(mid, owner)];
        Ok(UpdateResult {
            tier: UpdateTier::BodyOnly,
            reinferred: body.clone(),
            relowered: body,
            ..UpdateResult::default()
        })
    }

    fn rebuild_body_incremental(
        &mut self,
        path: &str,
        ast: Ast,
        changed_idx: usize,
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
        let old_gen = self
            .ws
            .module(mid)
            .bodies
            .get(&owner)
            .map(|b| b.generation)
            .unwrap_or(0);

        // Extract the new fn span as an owned value so the borrow into `ast`
        // ends before the fallback may move `ast` into `rebuild_module`.
        let new_fn_span_opt = ast
            .items
            .iter()
            .find(|it| it.span.offset_range.0 == new_start)
            .and_then(|it| match &it.kind {
                ItemKind::Function(nf) => Some(nf.span),
                _ => None,
            });
        let new_fn_span = match new_fn_span_opt {
            Some(s) => s,
            None => return self.rebuild_module(path, ast, &[changed_idx]),
        };

        // The threaded full-module AST is cooked in place -- no clone (the AST is
        // not persisted, so this owned copy is ours to consume). Timed as before.
        let t_clone = Instant::now();
        let mut edited = ast;
        let clone_dur = t_clone.elapsed();

        let t_cook = Instant::now();
        self.cook_edited(&mut edited)?;
        let cook_dur = t_cook.elapsed();

        // Defensive: the edited item should survive cooking unchanged. If it
        // doesn't, `edited` is already cooked (re-cooking would double-desugar),
        // so re-parse the stored text for a fresh un-cooked module to hand to the
        // whole-module path. Effectively unreachable for a classify-confirmed
        // body edit.
        let item = match edited
            .items
            .iter()
            .find(|it| it.span.offset_range.0 == new_start)
        {
            Some(it) => it,
            None => {
                let m = self.parse_stored(ustr(path))?;
                return self.rebuild_module(path, m, &[changed_idx]);
            }
        };

        let ItemKind::Function(f) = &item.kind else {
            let m = self.parse_stored(ustr(path))?;
            return self.rebuild_module(path, m, &[changed_idx]);
        };

        let annotations = item.annotations.as_slice();

        let at = old_fn_span.offset_range.1;
        let byte_delta = new_fn_span.offset_range.1 as isize - old_fn_span.offset_range.1 as isize;
        let line_delta = new_fn_span.line_range.1 as isize - old_fn_span.line_range.1 as isize;

        let old_inf = self
            .ws
            .inferences
            .get_mut(mid.index())
            .and_then(|s| s.take());
        self.ws.phase_timings.clear();
        self.ws.phase_timings.push(("clone ast", clone_dur));
        self.ws.phase_timings.push(("cook one", cook_dur));

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
            annotations,
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

        let body = vec![(mid, owner)];

        Ok(UpdateResult {
            tier: UpdateTier::BodyOnly,
            reinferred: body.clone(),
            relowered: body,
            ..UpdateResult::default()
        })
    }

    /// Cook the edited file against the cached registry, first refusing a
    /// macro call whose argument labels do not fit (`check_macro_call_labels`,
    /// which `Session::new` and `rebuild_users` run over every file). The
    /// narrow paths that call this are taken only when no macro definition
    /// changed, so the cached registry is current and every other file's
    /// calls were checked against it already.
    fn cook_edited(&self, ast: &mut Ast) -> Result<(), CompilerError> {
        if let Some(e) = check_macro_call_labels_in(ast, &self.registry)
            .into_iter()
            .next()
        {
            return Err(e);
        }
        cook_one(ast, &self.registry);
        Ok(())
    }

    fn rebuild_or_revert(
        &mut self,
        action: &str,
        path: &str,
        rebuild: impl FnOnce(&mut Self) -> Result<UpdateResult, CompilerError>,
        restore: impl FnOnce(&mut Self),
    ) -> Result<UpdateResult, CompilerError> {
        match rebuild(self) {
            Ok(result) => Ok(result),
            Err(e) => {
                restore(self);

                if let Err(revert_err) = self.rebuild_users() {
                    return Err(CompilerError::new_no_location(format!(
                        "bone session: rebuild of previous good state failed after a failed {action} of `{path}` - session is inconsistent: {} (original {action} error: {})",
                        revert_err.message, e.message
                    )));
                }

                Err(e)
            }
        }
    }

    pub fn collect_reinferred(&self, mids: &[ModuleId]) -> Vec<BodyId> {
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

/// Does any macro defined by the old or new version appear in the changed-name
/// set? (Both versions: a removed macro is gone from `new` but its users still
/// need re-expansion; an added one isn't in `old`.) Takes the OLD names from the
/// stored `UserFile.macros` (no retained AST) and the NEW names from the freshly
/// parsed module.
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

// fn module_id_of(module: &Ast, path: &str) -> ModuleId {
//     module
//         .items
//         .first()
//         .map(|i| ModuleId(i.span.file))
//         .unwrap_or_else(|| ModuleId(Ustr::from(path)))
// }

/// Minimal changed byte span between two texts: the longest common prefix `p`
/// and suffix, capped so they don't overlap. Returns `(p, old_end, delta)` where
/// the OLD text's changed region is `[p, old_end)` and `delta = new.len() -
/// old.len()`. Byte-level (UTF-8 split mid-char is harmless -- the region is only
/// used to find the containing item, and item boundaries are char-aligned).
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

/// Shift a span's byte offsets + line numbers by the deltas (columns are left
/// alone -- a kept top-level item after a body edit starts on a later line but
/// the same column, mirroring `span_shift`'s reasoning).
fn shift_span(s: &mut Span, byte_delta: isize, line_delta: isize) {
    s.offset_range = (
        (s.offset_range.0 as isize + byte_delta) as usize,
        (s.offset_range.1 as isize + byte_delta) as usize,
    );
    s.line_range = (
        (s.line_range.0 as isize + line_delta) as usize,
        (s.line_range.1 as isize + line_delta) as usize,
    );
}

/// New per-item prints after a body edit: items before `idx` are unchanged, `idx`
/// is replaced with the re-parsed `np` (already absolute), and every item after
/// `idx` shifts by `(byte_delta, line_delta)` -- so the next edit still locates
/// correctly without a whole-file reparse.
fn build_shifted_prints(
    mut prints: Vec<ItemPrint>,
    idx: usize,
    np: ItemPrint,
    byte_delta: isize,
    line_delta: isize,
) -> Vec<ItemPrint> {
    for (i, p) in prints.iter_mut().enumerate() {
        if i > idx {
            p.offset = (p.offset as isize + byte_delta) as usize;
            shift_span(&mut p.item_span, byte_delta, line_delta);
            if let Some((a, b)) = p.body_range {
                p.body_range = Some((
                    (a as isize + byte_delta) as usize,
                    (b as isize + byte_delta) as usize,
                ));
            }
        }
    }
    prints[idx] = np;
    prints
}

fn text_hash(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = rustc_hash::FxHasher::default();
    text.hash(&mut h);
    h.finish()
}

fn hash_u64s(xs: &[u64]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = rustc_hash::FxHasher::default();
    xs.hash(&mut h);
    h.finish()
}

/// The nominal type names a module's INTERFACE references. The interface arena
/// (`module.ty_arena`, `TyPos::Iface`) holds exactly the signature / global /
/// field / payload types -- body-local types live in `body.lowered_tys` -- so
/// every `App { Nominal(name) }` node in it is an interface-position reference.
/// An unresolved reference is `Error` (no name); the cascade's "re-lower every
/// module with an interface error" rule covers that case, so name erasure here
/// is safe.
/// The nominal TYPE names a module defines (struct / enum / newtype). These are
/// the names a cross-module signature resolves, so they drive the re-lower
/// cascade. Functions / globals are referenced from bodies (not type position),
/// so the re-infer set adds those separately.
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
        .ty_arena
        .iter()
        .filter_map(|(_, te)| match te {
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
/// A module's public interface, rendered so it is INVARIANT under the
/// `ty_arena` index drift that a body-only relower legitimately introduces.
///
/// Used only by the `rebuild_module` debug self-check: a body-only edit must
/// not change the module interface (else `classify` mis-tiered it). The naive
/// `format!("{:?}", &m.ty_arena ...)` is wrong here because the body-edit fast
/// path (`relower_function_in_place`) re-lowers the edited fn's SIGNATURE,
/// appending fresh `ty_arena` entries and orphaning the old ones -- so the
/// arena grows and a re-lowered fn's param/return `Idx<TypeExpr>` shifts even
/// though the signature is textually identical. Comparing raw indices + the
/// whole arena false-positives on that benign drift (it's exactly why the
/// `incremental.rs` oracle compares via `type_at`, not Debug). So we resolve
/// every interface `TyId` to its structural SHAPE and drop the arena. `VarId`s
/// are elided for the same reason: the fast path continues the var counter, so
/// a re-lowered fn's `Infer`/`Generic` carry different ids for the same type.
///
/// Functions and globals are resolved structurally (functions are the fast
/// path's only target today; globals are its planned next one -- see
/// `rebuild_body_incremental`). Structs/enums/newtypes/decorators are never
/// fast-path-relowered, so their arena indices are assigned in stable
/// traversal order and Debug-compare cleanly.
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
