use crate::prelude::*;

pub type PackageId = u32;

/// Reserved package id for the compiler-owned virtual standard library.
/// Package graph ids are dense from zero, so a host never assigns this one.
pub const STD_PACKAGE_ID: PackageId = u32::MAX;
pub const STD_PACKAGE_NAME: &str = "std";

pub use rock_core::source_code::{STANDALONE_PACKAGE_PREFIX, artifact_name, display_name};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntrinsicType {
    /// Members of the std export view, so they resolve only while the stdlib is
    /// loaded.
    FieldRef,
    /// Builtin formers with their own lowering; they resolve bare at the std
    /// tier but are not std exports.
    Former,
}

/// Compiler-known type formers with no source declaration. Their keys are the
/// bare spellings, so a package type with the same tail (`geo::Map`) is never
/// one of them.
pub(crate) const STD_INTRINSIC_TYPES: &[(&str, IntrinsicType)] = &[
    ("FieldRef", IntrinsicType::FieldRef),
    ("ReadFieldRef", IntrinsicType::FieldRef),
    ("WriteFieldRef", IntrinsicType::FieldRef),
    ("Map", IntrinsicType::Former),
    ("Coroutine", IntrinsicType::Former),
    ("Iterator", IntrinsicType::Former),
];

pub use rock_core::{is_stdlib_module_file, stdlib_file_name};

/// Compares the whole canonical key, so `other::FieldRef` is not an intrinsic.
/// Interning `spelling` instead would take the shared interner lock on hot
/// inference paths.
pub fn is_intrinsic_key(key: Ustr, spelling: &str) -> bool {
    key.as_str() == spelling
}

/// The component indices of a valid swizzle read on `struct_name`; invalid
/// shapes return None and are diagnosed on the read path.
pub fn vec_swizzle_components_of(struct_name: Ustr, field: &str) -> Option<Vec<usize>> {
    let arity = vec_swizzle_arity(struct_name)?;
    if field.is_empty() || field.len() > 4 {
        return None;
    }
    let xyzw: Option<Vec<usize>> = field.chars().map(|ch| "xyzw".find(ch)).collect();
    let rgba: Option<Vec<usize>> = field.chars().map(|ch| "rgba".find(ch)).collect();
    let idxs = xyzw.or(rgba)?;
    idxs.iter().all(|&i| i < arity).then_some(idxs)
}

pub fn vec_swizzle_arity(struct_name: Ustr) -> Option<usize> {
    ["Vec2", "Vec3", "Vec4"]
        .iter()
        .position(|spelling| is_intrinsic_key(struct_name, spelling))
        .map(|i| i + 2)
}

pub fn intrinsic_type(key: Ustr) -> Option<IntrinsicType> {
    STD_INTRINSIC_TYPES
        .iter()
        .find(|(name, _)| is_intrinsic_key(key, name))
        .map(|&(_, kind)| kind)
}

pub fn is_field_ref_former(key: Ustr) -> bool {
    intrinsic_type(key) == Some(IntrinsicType::FieldRef)
}

pub fn std_exported_intrinsic_types() -> impl Iterator<Item = &'static str> {
    STD_INTRINSIC_TYPES
        .iter()
        .filter(|(_, kind)| *kind == IntrinsicType::FieldRef)
        .map(|&(name, _)| name)
}

/// Stdlib `value` descriptors whose Lua representation is one host slot, so
/// copying one shares the referent.
pub(crate) const STD_DESCRIPTOR_TYPES: &[&str] = &[
    "Ptr",
    "PtrU8",
    "PtrI8",
    "PtrU16",
    "PtrI16",
    "PtrU32",
    "NativeBuffer",
    "NativeBufferU8",
    "NativeBufferI8",
    "NativeBufferU16",
    "NativeBufferI16",
    "NativeBufferU32",
];

pub fn is_descriptor_key(key: Ustr) -> bool {
    is_intrinsic_key_in(key, STD_DESCRIPTOR_TYPES)
}

pub fn is_intrinsic_key_in(key: Ustr, spellings: &[&str]) -> bool {
    spellings
        .iter()
        .any(|spelling| is_intrinsic_key(key, spelling))
}

/// Package-root-relative module path (`combat::casting`).
pub(crate) type ModulePath = Vec<Ustr>;

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct AccessSite<'a> {
    pub package: Option<PackageId>,
    pub module_path: &'a [Ustr],
}

impl<'a> AccessSite<'a> {
    pub fn new(package: Option<PackageId>, module_path: &'a [Ustr]) -> Self {
        Self {
            package,
            module_path,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NsVisibility {
    /// Defining module and its descendants (the default).
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

pub fn is_setter_audience_within_getter(write_vis: &NsVisibility, read_vis: &NsVisibility) -> bool {
    *write_vis == NsVisibility::Private && *read_vis != NsVisibility::Private
}

/// The source file and per-file overload ordinal identify a member: several
/// files may contribute to one module, and offsets shift with body edits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallableMember {
    pub file: Ustr,
    pub ordinal: usize,
    pub vis: NsVisibility,
}

/// A logical module within a package; multi-file contributions coalesce here.
/// Callable visibility lives on each family member, not the family key.
#[derive(Clone, Debug, Default)]
pub struct NsNode {
    /// `::`-joined module path ("" for the package root).
    pub path_key: Ustr,
    pub segments: ModulePath,
    pub vis: Option<NsVisibility>,
    /// Ancestor node keys, root first.
    pub ancestors: Vec<Ustr>,
    /// Bare name to canonical DefKey.
    pub owned_types: FxHashMap<Ustr, Ustr>,
    pub owned_globals: FxHashMap<Ustr, Ustr>,
    pub owned_callables: FxHashMap<Ustr, Ustr>,
    /// The declarations of each callable family, in source order: the
    /// per-member visibility table (`DefMeta::vis` is meaningless for
    /// callables).
    pub callable_members: FxHashMap<Ustr, Vec<CallableMember>>,
    pub owned_decorators: FxHashMap<Ustr, Ustr>,
    pub children: FxHashMap<Ustr, Ustr>,
    /// Named export bindings from module-scope `pub use` / `private use`;
    /// fragment-local imports live in `FragmentScope`.
    pub exports: FxHashMap<Ustr, ExportBinding>,
    /// Glob export bindings (`pub use a::*;`), below owned names and named
    /// exports.
    pub glob_exports: Vec<ExportBinding>,
}

impl NsNode {
    /// The definition this node owns under `name`, checking types, globals,
    /// callables and decorators in that order.
    pub fn owned(&self, name: Ustr) -> Option<(Ustr, DefKind)> {
        [
            (&self.owned_types, DefKind::Type),
            (&self.owned_globals, DefKind::Global),
            (&self.owned_callables, DefKind::Callable),
            (&self.owned_decorators, DefKind::Decorator),
        ]
        .into_iter()
        .find_map(|(table, kind)| table.get(&name).map(|&key| (key, kind)))
    }

    pub fn owned_table_mut(&mut self, kind: DefKind) -> &mut FxHashMap<Ustr, Ustr> {
        match kind {
            DefKind::Type => &mut self.owned_types,
            DefKind::Global => &mut self.owned_globals,
            DefKind::Callable => &mut self.owned_callables,
            DefKind::Decorator => &mut self.owned_decorators,
        }
    }
}

/// A module-scope `pub use` / `private use`. The target stays unresolved: the
/// resolver chases it lazily with cycle rejection, so declaration order never
/// matters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportBinding {
    pub target: UsePathRef,
    pub vis: NsVisibility,
}

/// Kind of a canonical definition. Callables are families;
/// everything else is unique in the module's one item-name domain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DefKind {
    Type,
    Global,
    Callable,
    Decorator,
}

/// Metadata for one canonical definition. Diagnostics render `name` or the
/// shortest accessible path, never the raw key.
#[derive(Clone, Debug)]
pub struct DefMeta {
    pub package: PackageId,
    pub module: ModulePath,
    /// `module`'s namespace node key ([`NsPackage::path_key`]), kept so a
    /// lookup does not join and intern the path again.
    pub node: Ustr,
    pub kind: DefKind,
    pub name: Ustr,
    pub vis: NsVisibility,
    pub span: crate::prelude::Span,
}

#[derive(Clone, Debug, Default)]
pub struct FragmentScope {
    pub package: PackageId,
    pub package_name: Ustr,
    pub module_path: ModulePath,
    /// Fragment-local `use` bindings: local name -> every unresolved path bound
    /// under it, in source order. Resolved lazily at use.
    pub named: FxHashMap<Ustr, Vec<UsePathRef>>,
    /// Unresolved module scopes this fragment globs.
    pub globs: Vec<UsePathRef>,
}

/// An unresolved use path; `self`/`super` stay as segment spellings for the
/// resolver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsePathRef {
    pub from_root: bool,
    pub segments: Vec<Ustr>,
}

/// Interned once: resolver lookups ask for it constantly and interning takes a
/// lock.
pub fn root_key() -> Ustr {
    static ROOT: std::sync::LazyLock<Ustr> = std::sync::LazyLock::new(|| Ustr::from(""));
    *ROOT
}

impl UsePathRef {
    /// Orders like the joined `"::"` string without allocating it, so a segment
    /// that is a prefix of another does not simply sort first.
    pub fn cmp_joined(&self, other: &UsePathRef) -> std::cmp::Ordering {
        fn joined_bytes(path: &UsePathRef) -> impl Iterator<Item = u8> + '_ {
            path.segments.iter().enumerate().flat_map(|(i, s)| {
                let sep: &[u8] = if i == 0 { b"" } else { b"::" };
                sep.iter().copied().chain(s.as_bytes().iter().copied())
            })
        }
        self.from_root
            .cmp(&other.from_root)
            .then_with(|| joined_bytes(self).cmp(joined_bytes(other)))
    }

    /// Insert `item` after any equal path in [`UsePathRef::cmp_joined`] order,
    /// so glob lookups walk a fixed order without sorting.
    pub fn insert_sorted<T>(paths: &mut Vec<T>, item: T, path: impl Fn(&T) -> &UsePathRef) {
        let at = paths.partition_point(|p| path(p).cmp_joined(path(&item)).is_le());
        paths.insert(at, item);
    }
}

pub use rock_core::package_aliases::{NsAliases, PackageAliases};

/// One namespace fact a lowering read, hit or miss, so a tree edit re-lowers
/// exactly the modules that read a changed fact.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ScopeProbe {
    /// Module `node`'s entry under `name`: owned definition and its `DefMeta`,
    /// named export, child module, and whatever its glob exports open.
    Member {
        package: PackageId,
        node: Ustr,
        name: Ustr,
    },
    /// Module `node` entered as a path segment: its presence and visibility.
    Node { package: PackageId, node: Ustr },
}

#[derive(Clone, Debug, Default)]
pub struct NsTree {
    pub packages: FxHashMap<PackageId, NsPackage>,
}

#[derive(Clone, Debug, Default)]
pub struct NsPackage {
    pub id: PackageId,
    pub self_name: Ustr,
    pub nodes: FxHashMap<Ustr, NsNode>,
    pub aliases: FxHashMap<Ustr, PackageId>,
}

impl NsPackage {
    pub fn node(&self, path_key: Ustr) -> Option<&NsNode> {
        self.nodes.get(&path_key)
    }

    pub fn path_key(segments: &[Ustr]) -> Ustr {
        let joined = segments
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("::");
        Ustr::from(&joined)
    }
}

impl NsTree {
    pub fn package(&self, id: PackageId) -> Option<&NsPackage> {
        self.packages.get(&id)
    }

    pub fn std_root(&self) -> Option<&NsNode> {
        self.package(STD_PACKAGE_ID)?.node(root_key())
    }

    pub fn std_definition(&self, name: Ustr) -> Option<(Ustr, DefKind)> {
        self.std_root()?.owned(name)
    }

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
            NsVisibility::Private => owner_pkg == from_pkg && from_path.starts_with(owner_path),
            NsVisibility::Super => {
                owner_pkg == from_pkg
                    && owner_path
                        .split_last()
                        .is_some_and(|(_, parent)| from_path.starts_with(parent))
            }
            NsVisibility::In(ancestor) => owner_pkg == from_pkg && from_path.starts_with(ancestor),
        }
    }
}

/// The canonical DefKey: package self name, module path, then item name, joined
/// by `::`, which no identifier contains.
pub fn def_key(pkg_self_name: &str, module_path: &[Ustr], name: &str) -> Ustr {
    if pkg_self_name == STD_PACKAGE_NAME {
        return Ustr::from(name);
    }
    let mut key = String::from(pkg_self_name);
    for seg in module_path {
        key.push_str("::");
        key.push_str(seg);
    }
    key.push_str("::");
    key.push_str(name);
    Ustr::from(&key)
}
