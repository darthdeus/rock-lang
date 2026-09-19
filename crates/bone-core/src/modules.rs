//! Modules-v1 namespace data (plan M2; spec 2, 6, 8, 9).
//!
//! The namespace tree is built per workspace by the semantic pass from the
//! v1 fragments' headers, inline mod blocks, and use declarations. Definition
//! identity uses canonical DefKeys (`<package>::<module path>::<name>`,
//! plan 2.1): the workspace `*_by_name` maps keep their types, but v1 items
//! register under their DefKey while legacy items keep today's bare names.
//! The tree itself is only the SCOPE map — resolution lives in
//! `bone-lower/src/resolve.rs`.

use crate::prelude::*;

/// Dense package id from the host's package graph.
pub type PackageId = u32;

/// Reserved package id for the compiler-owned virtual standard library.
/// Package graph ids are dense from zero, so the maximum value cannot be
/// assigned by a host graph.
pub const STD_PACKAGE_ID: PackageId = u32::MAX;
pub const STD_PACKAGE_NAME: &str = "std";

/// Compiler-known type formers that have no source declaration of their own.
/// Their canonical keys are the bare spellings, just like legacy definitions.
pub const STD_INTRINSIC_TYPES: &[&str] = &["FieldRef", "ReadFieldRef", "WriteFieldRef"];

/// Whether a source file id uses an established stdlib file-id spelling.
/// The loader owns these ids; do not infer stdlib ownership from linked files.
pub fn is_stdlib_module_file(file: &str) -> bool {
    file.starts_with("<bone-stdlib:")
        || file.starts_with("<stdlib:")
        || file.starts_with("<stdlib>/")
}

/// Intrinsic recognition is keyed by the canonical DefKey, not by a path's
/// display tail. A v1 definition such as `other::FieldRef` therefore cannot
/// enter an intrinsic branch merely because it has the same final segment.
pub fn is_intrinsic_key(key: Ustr, spelling: &str) -> bool {
    key == Ustr::from(spelling)
}

pub fn intrinsic_type_name(key: Ustr) -> Option<&'static str> {
    STD_INTRINSIC_TYPES
        .iter()
        .copied()
        .find(|name| is_intrinsic_key(key, name))
}

pub fn is_intrinsic_key_in(key: Ustr, spellings: &[&str]) -> bool {
    spellings.iter().any(|spelling| is_intrinsic_key(key, spelling))
}

/// Package-root-relative module path (`combat::casting`).
pub type ModulePath = Vec<Ustr>;

/// The source site performing an access check (spec 10.2; plan 2.3).
/// None package indicates legacy callers (spec 3.3).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct AccessSite<'a> {
    pub package: Option<PackageId>,
    pub module_path: &'a [Ustr],
}

impl<'a> AccessSite<'a> {
    pub fn new(package: Option<PackageId>, module_path: &'a [Ustr]) -> Self {
        Self { package, module_path }
    }
}

/// Visibility of a namespace node or export binding (spec 9).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NsVisibility {
    /// Defining module + descendants (the default).
    Private,
    /// `pub(super)`.
    Super,
    /// `pub(in a::b)` — named ancestor module, package-root-relative.
    In(ModulePath),
    /// `pub(package)`.
    Package,
    /// `pub`.
    Pub,
}

impl NsVisibility {
    pub fn from_parser(vis: &skull_parser::Visibility) -> Self {
        match vis {
            skull_parser::Visibility::Private => NsVisibility::Private,
            skull_parser::Visibility::Pub => NsVisibility::Pub,
            skull_parser::Visibility::PubSuper => NsVisibility::Super,
            skull_parser::Visibility::PubPackage => NsVisibility::Package,
            skull_parser::Visibility::PubIn(path) => {
                NsVisibility::In(path.iter().map(|i| i.name).collect())
            }
        }
    }
}

/// Whether a restricted setter audience (`write_vis`) is strictly within (a proper subset of)
/// the getter audience (`read_vis`).
pub fn is_setter_audience_within_getter(write_vis: &NsVisibility, read_vis: &NsVisibility) -> bool {
    match (write_vis, read_vis) {
        (NsVisibility::Private, NsVisibility::Pub) => true,
        (NsVisibility::Private, NsVisibility::Package) => true,
        (NsVisibility::Private, NsVisibility::Super) => true,
        (NsVisibility::Private, NsVisibility::In(_)) => true,
        (NsVisibility::Private, NsVisibility::Private) => false,
        _ => false,
    }
}

/// One callable declaration in an overload family. The family key alone is
/// not enough to identify a member because multiple files may contribute to
/// one logical module; the source file and per-file overload ordinal identify
/// the member without depending on source offsets that body edits can shift.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallableMember {
    pub file: Ustr,
    pub ordinal: usize,
    pub vis: NsVisibility,
}

/// One namespace node: a logical module within a package. Multi-file
/// contributions coalesce here; the node owns visibility and the items
/// declared in it (callables stay families in the workspace maps — the node
/// records their presence and the DefKey prefix). Callable visibility stays
/// on each family member rather than on the shared family key.
#[derive(Clone, Debug, Default)]
pub struct NsNode {
    /// `::`-joined module path ("" for the package root).
    pub path_key: Ustr,
    pub segments: ModulePath,
    /// Explicit visibility declarations (`pub mod x;` headers / blocks).
    /// Conflicting declarations are a build error at tree construction.
    pub vis: Option<NsVisibility>,
    /// Ancestor node keys, root first. Filled at construction.
    pub ancestors: Vec<Ustr>,
    /// Types owned here: bare name -> DefKey.
    pub owned_types: FxHashMap<Ustr, Ustr>,
    /// Globals/constants owned here: bare name -> DefKey.
    pub owned_globals: FxHashMap<Ustr, Ustr>,
    /// Callable families owned here: bare name -> family DefKey prefix (the
    /// family key is the DefKey itself; overloads share it).
    pub owned_callables: FxHashMap<Ustr, Ustr>,
    /// The declarations contributing each callable family, in source order.
    /// This is the authoritative per-member visibility table; `DefMeta::vis`
    /// is not meaningful for callable families.
    pub callable_members: FxHashMap<Ustr, Vec<CallableMember>>,
    /// Decorators owned here.
    pub owned_decorators: FxHashMap<Ustr, Ustr>,
    /// Child modules by segment.
    pub children: FxHashMap<Ustr, Ustr>,
    /// Shared named export bindings from `pub use` / `private use` at module
    /// scope, by bound name — spec 8.2 distinguishes them from fragment-local
    /// imports, which live on the fragment (file) in `FragmentScope`.
    pub exports: FxHashMap<Ustr, ExportBinding>,
    /// Shared glob export bindings (`pub use a::*;`). A module may hold any
    /// number; they sit below owned names and named exports (spec 8.3, 8.6).
    pub glob_exports: Vec<ExportBinding>,
}

/// A shared export binding (spec 8.2): `pub use` / `private use` at module
/// scope. The target stays UNRESOLVED — the resolver chases it lazily from
/// the exporting module (with cycle rejection), so declaration order never
/// matters and pure re-export cycles diagnose at use (spec 8.6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportBinding {
    pub target: UsePathRef,
    pub vis: NsVisibility,
}

/// Kind of a canonical definition (plan 2.1). Callables are families;
/// everything else is unique in the module's one item-name domain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DefKind {
    Type,
    Global,
    Callable,
    Decorator,
    Module,
}

/// Metadata for one canonical definition (plan 2.1). Diagnostics and the
/// LSP render `name`/the shortest accessible path, never the raw key.
#[derive(Clone, Debug)]
pub struct DefMeta {
    pub package: PackageId,
    pub module: ModulePath,
    pub kind: DefKind,
    pub name: Ustr,
    pub vis: NsVisibility,
    pub span: crate::prelude::Span,
    pub dialect: skull_parser::Dialect,
}

/// Fragment facts for one v1 file: which package/module it contributes to
/// and its own imports (spec 8.2 fragment-local scope).
#[derive(Clone, Debug, Default)]
pub struct FragmentScope {
    pub package: PackageId,
    pub package_name: Ustr,
    pub module_path: ModulePath,
    /// Plain `use` bindings local to this fragment: local name -> every
    /// (unresolved) use path bound under it, in source order. Resolution is
    /// lazy so declaration order never matters; pure cycles diagnose at use,
    /// and several paths compose or conflict there (spec 8.3).
    pub named: FxHashMap<Ustr, Vec<UsePathRef>>,
    /// Module scopes this fragment globs (`use a::b::*;`), unresolved.
    pub globs: Vec<UsePathRef>,
}

/// An unresolved use path: `::`-root flag plus the segments (contextual
/// `self`/`super` stay as segment spellings for the resolver).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsePathRef {
    pub from_root: bool,
    pub segments: Vec<Ustr>,
}

/// Fragment-local imports (plain `use` in one file), spec 8.2.
#[derive(Clone, Debug, Default)]
pub struct FragmentImports {
    /// Plain (tier-3) bindings: local name -> DefKey.
    pub named: FxHashMap<Ustr, Ustr>,
    /// Glob (tier-4) bindings: the module node keys globbed.
    pub globs: Vec<Ustr>,
}

/// Host-supplied alias tables (one entry per package), so cross-package
/// resolution uses the graph's dependency names rather than guessing. The
/// CLI derives this from the PackageGraph; tests construct it directly.
#[derive(Clone, Debug, Default)]
pub struct NsAliases {
    pub packages: Vec<PackageAliases>,
}

#[derive(Clone, Debug, Default)]
pub struct PackageAliases {
    pub id: PackageId,
    pub self_name: Ustr,
    /// alias -> target package id.
    pub aliases: Vec<(Ustr, PackageId)>,
}

/// One v1 definition made visible to a legacy compilation unit. The key stays
/// canonical so calls and type lookups retain the definition site's identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyProjectionEntry {
    pub key: Ustr,
    pub kind: DefKind,
}

/// The compatibility view built once for one legacy source file. A name can
/// have several entries when participating v1 packages export distinct
/// definitions with the same spelling; identical DefKeys are collapsed.
#[derive(Clone, Debug, Default)]
pub struct LegacyProjection {
    pub by_name: FxHashMap<Ustr, Vec<LegacyProjectionEntry>>,
}

impl LegacyProjection {
    pub fn insert(&mut self, name: Ustr, key: Ustr, kind: DefKind) {
        let entries = self.by_name.entry(name).or_default();
        if entries.iter().any(|entry| entry.key == key) {
            return;
        }
        entries.push(LegacyProjectionEntry { key, kind });
        entries.sort_by_key(|entry| entry.key.as_str().to_string());
    }
}

/// A legacy file's item-level `use` declarations (spec 3.2). A named import
/// binds its local name explicitly, which replaces every other candidate for
/// that name in the file; a glob import stands where the root projection
/// would. `renamed` records what an import binds under a name other than
/// the one it was exported as (`use p::x as y` is keyed `x`): that
/// definition leaves the file's bare `x`.
#[derive(Clone, Debug, Default)]
pub struct LegacyImports {
    pub named: LegacyProjection,
    pub globs: LegacyProjection,
    pub renamed: FxHashMap<Ustr, Vec<Ustr>>,
}

impl LegacyImports {
    pub fn is_empty(&self) -> bool {
        self.named.by_name.is_empty() && self.globs.by_name.is_empty()
    }

    /// Whether any import of this file can bear on the bare name `name`.
    pub fn mentions(&self, name: Ustr) -> bool {
        self.named.by_name.contains_key(&name)
            || self.globs.by_name.contains_key(&name)
            || self.renamed.contains_key(&name)
    }
}

/// What a bare name in a legacy source file names (spec 3.2). The one
/// decision behind every legacy lookup: lowering reads it to resolve a name
/// and to record a merged overload family's members. Its inputs -- the
/// per-file views and the legacy owned-name facts -- exist before any body
/// lowers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LegacyName {
    /// Nothing but (possibly) the legacy definition: the bare key.
    Bare,
    /// v1 definitions reached through the file's imports or projection, with
    /// no legacy definition in the way. More than one entry is ambiguous.
    Projected(Vec<LegacyProjectionEntry>),
    /// Callables under one name forming one overload family, exactly as
    /// legacy overloading merges them: every member key. The legacy bare key
    /// comes first when the legacy definitions take part; the v1 keys follow
    /// sorted.
    Family(Vec<Ustr>),
    /// A legacy definition and a v1 definition that cannot merge (not both
    /// callable): ambiguous at use.
    Collision { owners: Vec<Ustr>, foreign: Vec<Ustr> },
}

pub fn legacy_name(ws: &crate::workspace::Workspace, file: Ustr, name: Ustr) -> LegacyName {
    // The compiler-owned stdlib is lowered in the same workspace as user
    // packages, but its legacy bodies must not see user declarations.
    if is_stdlib_module_file(file.as_str()) {
        return LegacyName::Bare;
    }
    let imports = ws.legacy_imports.get(&file);
    // An explicit named import wins in its file (spec 3.2: explicit
    // compatibility imports shadow the root projection, and they are how a
    // legacy file resolves a collision). Named imports of several callables
    // compose into one family of those imports alone (spec 8.3).
    if let Some(entries) = imports.and_then(|i| i.named.by_name.get(&name)) {
        if entries.len() > 1 && entries.iter().all(|e| e.kind == DefKind::Callable) {
            return LegacyName::Family(entries.iter().map(|e| e.key).collect());
        }
        return LegacyName::Projected(entries.clone());
    }
    let Some(entries) = imports
        .and_then(|i| i.globs.by_name.get(&name))
        .or_else(|| ws.legacy_projections.get(&file).and_then(|view| view.by_name.get(&name)))
    else {
        return LegacyName::Bare;
    };
    let renamed = imports.and_then(|i| i.renamed.get(&name));
    let entries: Vec<LegacyProjectionEntry> = entries
        .iter()
        .filter(|entry| !renamed.is_some_and(|keys| keys.contains(&entry.key)))
        .cloned()
        .collect();
    if entries.is_empty() {
        return LegacyName::Bare;
    }
    // A v1 re-export of a legacy definition keeps the bare key: it is the
    // legacy definition itself, not a second candidate.
    let foreign: Vec<Ustr> = entries.iter().filter(|e| e.key != name).map(|e| e.key).collect();
    let foreign_callable = entries.iter().all(|e| e.key == name || e.kind == DefKind::Callable);
    let owners = ws.ns.legacy_owned.files_defining(name);
    // Legacy overloading merges every same-named callable, so several
    // projected v1 callables are one family even with no legacy member.
    if owners.is_empty() {
        if foreign.len() > 1 && foreign_callable {
            return LegacyName::Family(foreign);
        }
        return LegacyName::Projected(entries);
    }
    if foreign.is_empty() {
        return LegacyName::Projected(entries);
    }
    if ws.ns.legacy_owned.all_callable(name) && foreign_callable {
        return LegacyName::Family(std::iter::once(name).chain(foreign).collect());
    }
    LegacyName::Collision { owners, foreign }
}

/// One namespace fact a lowering read (plan 2.6, scope epochs). A module's
/// lowering records every probe its lookups made, hit or miss, so an edit to
/// the namespace tree re-lowers exactly the modules that read a changed fact.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ScopeProbe {
    /// What module `node` of `package` offers under `name`: its owned entry,
    /// named export binding and child module of that name, the owned
    /// definition's `DefMeta`, and -- through the node's glob export
    /// bindings -- whatever they open. A glob target is probed in turn.
    Member { package: PackageId, node: Ustr, name: Ustr },
    /// Module `node` of `package` entered as a path segment: its presence and
    /// its declared visibility.
    Node { package: PackageId, node: Ustr },
    /// A legacy file's lookup of the bare `name` (`legacy_name`): the file's
    /// compatibility views and the legacy-owned facts for that name.
    Legacy { name: Ustr },
}

/// Per-package scope facts the resolver needs, built by the semantic pass
/// from the fragments plus the host's alias tables.
#[derive(Clone, Debug, Default)]
pub struct NsTree {
    pub packages: FxHashMap<PackageId, NsPackage>,
    /// Bare names defined by the legacy files of this compilation unit
    /// (stdlib included), recorded before any body lowers.
    pub legacy_owned: LegacyOwnedNames,
}

/// Which legacy files define each bare item name, and as what. Lowering
/// replaces a file's entry before it lowers any body, in a fresh build and in
/// an incremental re-lower alike, so a legacy lookup never depends on which
/// maps have filled so far.
#[derive(Clone, Debug, Default)]
pub struct LegacyOwnedNames {
    by_file: FxHashMap<Ustr, Vec<(Ustr, DefKind)>>,
    by_name: FxHashMap<Ustr, Vec<(Ustr, DefKind)>>,
}

impl LegacyOwnedNames {
    pub fn replace_file(&mut self, file: Ustr, mut names: Vec<(Ustr, DefKind)>) {
        names.sort_unstable_by(|a, b| a.0.as_str().cmp(b.0.as_str()).then(kind_rank(a.1).cmp(&kind_rank(b.1))));
        names.dedup();
        if let Some(old) = self.by_file.remove(&file) {
            for (name, _) in old {
                if let Some(files) = self.by_name.get_mut(&name) {
                    files.retain(|(f, _)| *f != file);
                    if files.is_empty() {
                        self.by_name.remove(&name);
                    }
                }
            }
        }
        for &(name, kind) in &names {
            let files = self.by_name.entry(name).or_default();
            files.push((file, kind));
            files.sort_unstable_by(|a, b| a.0.as_str().cmp(b.0.as_str()).then(kind_rank(a.1).cmp(&kind_rank(b.1))));
        }
        if !names.is_empty() {
            self.by_file.insert(file, names);
        }
    }

    /// The names `file` defines with their kinds, sorted.
    pub fn names_defined_by(&self, file: Ustr) -> &[(Ustr, DefKind)] {
        self.by_file.get(&file).map(Vec::as_slice).unwrap_or(&[])
    }

    /// The legacy files defining `name`, sorted.
    pub fn files_defining(&self, name: Ustr) -> Vec<Ustr> {
        let mut files: Vec<Ustr> =
            self.by_name.get(&name).into_iter().flatten().map(|(file, _)| *file).collect();
        files.dedup();
        files
    }

    /// Whether every legacy definition of `name` is a function.
    pub fn all_callable(&self, name: Ustr) -> bool {
        self.by_name
            .get(&name)
            .is_some_and(|defs| defs.iter().all(|(_, kind)| *kind == DefKind::Callable))
    }
}

fn kind_rank(kind: DefKind) -> u8 {
    match kind {
        DefKind::Type => 0,
        DefKind::Global => 1,
        DefKind::Callable => 2,
        DefKind::Decorator => 3,
        DefKind::Module => 4,
    }
}

#[derive(Clone, Debug, Default)]
pub struct NsPackage {
    pub id: PackageId,
    pub self_name: Ustr,
    pub nodes: FxHashMap<Ustr, NsNode>,
    /// Dependency aliases: alias -> target package id.
    pub aliases: FxHashMap<Ustr, PackageId>,
    /// For the view of a legacy provider (spec 3.2): the provider's own
    /// files, whose definitions are the only members of its view's callable
    /// families. Empty for a v1 package.
    pub legacy_files: FxHashSet<Ustr>,
}

impl NsPackage {
    pub fn node(&self, path_key: Ustr) -> Option<&NsNode> {
        self.nodes.get(&path_key)
    }

    /// `::`-join module segments into the node key.
    pub fn path_key(segments: &[Ustr]) -> Ustr {
        let joined = segments.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("::");
        Ustr::from(&joined)
    }

    /// [`NsPackage::path_key`] as a method form for readability at call sites.
    pub fn key_of(&self, segments: &[Ustr]) -> Ustr {
        Self::path_key(segments)
    }
}

impl NsTree {
    pub fn package(&self, id: PackageId) -> Option<&NsPackage> {
        self.packages.get(&id)
    }

    pub fn std_root(&self) -> Option<&NsNode> {
        self.package(STD_PACKAGE_ID)?.node(Ustr::from(""))
    }

    /// Whether a function lowered from `file` belongs to a family member
    /// owned by `owner` ([`crate::hir::FamilyMember`]): a stdlib file for the
    /// std view, one of the provider's own files for a legacy view.
    pub fn owns_file(&self, owner: PackageId, file: Ustr) -> bool {
        if owner == STD_PACKAGE_ID {
            return is_stdlib_module_file(file.as_str());
        }
        self.package(owner).is_some_and(|pkg| pkg.legacy_files.contains(&file))
    }

    /// Look up one member of the compiler-owned default export view.
    pub fn std_definition(&self, name: Ustr) -> Option<(Ustr, DefKind)> {
        let root = self.std_root()?;
        if let Some(&key) = root.owned_types.get(&name) {
            return Some((key, DefKind::Type));
        }
        if let Some(&key) = root.owned_globals.get(&name) {
            return Some((key, DefKind::Global));
        }
        if let Some(&key) = root.owned_callables.get(&name) {
            return Some((key, DefKind::Callable));
        }
        root.owned_decorators
            .get(&name)
            .copied()
            .map(|key| (key, DefKind::Decorator))
    }

    /// Whether `vis` on a node in `owner_pkg` admits access FROM
    /// (from_pkg, from_path) — spec 9's audiences. `from_path` is the
    /// accessing module's path within its own package; cross-package access
    /// only ever sees `Pub`.
    pub fn can_access(
        &self,
        vis: &NsVisibility,
        owner_pkg: PackageId,
        owner_path: &[Ustr],
        from_pkg: PackageId,
        from_path: &[Ustr],
    ) -> bool {
        match vis {
            NsVisibility::Pub => true,
            NsVisibility::Package => owner_pkg == from_pkg,
            NsVisibility::Private => {
                // The defining module and its descendants.
                owner_pkg == from_pkg && from_path_is_ancestor_or_self(owner_path, from_path)
            }
            NsVisibility::Super => {
                // Parent module and its descendants.
                owner_pkg == from_pkg
                    && owner_path.len() >= 1
                    && from_path_is_ancestor_or_self(&owner_path[..owner_path.len() - 1], from_path)
            }
            NsVisibility::In(ancestor) => {
                owner_pkg == from_pkg && from_path_is_ancestor_or_self(ancestor, from_path)
            }
        }
    }
}

/// Is `from` the same module as, or a descendant of, `ancestor`?
fn from_path_is_ancestor_or_self(ancestor: &[Ustr], from: &[Ustr]) -> bool {
    from.len() >= ancestor.len() && from[..ancestor.len()] == *ancestor
}

/// Build the canonical DefKey for a definition (plan 2.1): the package's
/// self name, the `::`-joined module path, then the item name. Encoding is
/// injective because `::` cannot occur inside an identifier.
pub fn def_key(pkg_self_name: &str, module_path: &[Ustr], name: &str) -> Ustr {
    let mut key = String::from(pkg_self_name);
    for seg in module_path {
        key.push_str("::");
        key.push_str(seg);
    }
    key.push_str("::");
    key.push_str(name);
    Ustr::from(&key)
}
