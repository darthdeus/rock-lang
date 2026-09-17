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

/// Package-root-relative module path (`combat::casting`).
pub type ModulePath = Vec<Ustr>;

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

/// One namespace node: a logical module within a package. Multi-file
/// contributions coalesce here; the node owns visibility and the items
/// declared in it (callables stay families in the workspace maps — the node
/// records their presence and the DefKey prefix).
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
    /// Decorators owned here.
    pub owned_decorators: FxHashMap<Ustr, Ustr>,
    /// Child modules by segment.
    pub children: FxHashMap<Ustr, Ustr>,
    /// Shared export bindings from `pub use` / `private use` at module
    /// scope — spec 8.2 distinguishes them from fragment-local imports,
    /// which live on the fragment (file) in `FragmentScope`.
    pub exports: FxHashMap<Ustr, ExportBinding>,
}

/// A shared export binding (spec 8.2): `pub use` / `private use` at module
/// scope. The target stays UNRESOLVED — the resolver chases it lazily (with
/// cycle rejection), so declaration order never matters and pure re-export
/// cycles diagnose at use (spec 8.6). Glob bindings sit at a lower lookup
/// tier than named ones (spec 8.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportBinding {
    pub target: UsePathRef,
    pub is_glob: bool,
    pub vis: NsVisibility,
}

/// Kind of a canonical definition (plan 2.1). Callables are families;
/// everything else is unique in the module's one item-name domain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    /// Plain `use` bindings local to this fragment: local name -> the
    /// (unresolved) use path. Resolution is lazy so declaration order never
    /// matters; pure cycles diagnose at use.
    pub named: FxHashMap<Ustr, UsePathRef>,
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

/// Per-package scope facts the resolver needs, built by the semantic pass
/// from the fragments plus the host's alias tables.
#[derive(Clone, Debug, Default)]
pub struct NsTree {
    pub packages: FxHashMap<PackageId, NsPackage>,
}

#[derive(Clone, Debug, Default)]
pub struct NsPackage {
    pub id: PackageId,
    pub self_name: Ustr,
    pub nodes: FxHashMap<Ustr, NsNode>,
    /// Dependency aliases: alias -> target package id.
    pub aliases: FxHashMap<Ustr, PackageId>,
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
