use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

pub mod build_vars;
pub mod graph;

/// The `[language]` manifest table. Absent, or `modules = "legacy"`, keeps
/// today's flat compilation behavior; `modules = "v1"` opts the package into
/// the modules-v1 grammar and scoping rules. Any other value is a load error
/// rather than a silent legacy fallback.
///
/// `globals` declares whether the package may own mutable global storage.
/// Absent = `"any"`; `"none"` is the simulation-package boundary from R5.
///
/// `identity_storage` declares the package's identity-storage migration
/// state: absent or `"enforced"`, `"exempt"` for an unmigrated package, or
/// `{ exempt_modules = ["nav"] }` for unmigrated module path prefixes inside
/// an enforced package.
#[derive(Debug, Clone)]
pub struct LanguageConfig {
    pub modules: rock_core::source_code::Dialect,
    pub globals: rock_core::source_code::GlobalPolicy,
    pub identity_storage: rock_core::source_code::IdentityStorage,
}

impl Default for LanguageConfig {
    fn default() -> Self {
        Self {
            modules: rock_core::source_code::Dialect::Legacy,
            globals: rock_core::source_code::GlobalPolicy::Any,
            identity_storage: rock_core::source_code::IdentityStorage::Enforced,
        }
    }
}

/// `[language] identity_storage`: a state name or an `exempt_modules` table.
struct IdentityStorageDeclaration(rock_core::source_code::IdentityStorage);

impl<'de> serde::Deserialize<'de> for IdentityStorageDeclaration {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use rock_core::source_code::IdentityStorage;
        struct Vis;
        impl<'de> serde::de::Visitor<'de> for Vis {
            type Value = IdentityStorageDeclaration;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(
                    f,
                    "\"enforced\", \"exempt\" or a table with an exempt_modules list"
                )
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                IdentityStorage::parse(value).map(IdentityStorageDeclaration).ok_or_else(|| {
                    E::custom(format!(
                        "unknown `[language] identity_storage` value `{value}`; known values: {}, or {{ exempt_modules = [..] }}",
                        IdentityStorage::NAMES.join(", ")
                    ))
                })
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut modules = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "exempt_modules" => modules = Some(map.next_value::<Vec<String>>()?),
                        other => {
                            return Err(serde::de::Error::custom(format!(
                                "unknown key `{other}` in `[language] identity_storage`; known keys: exempt_modules"
                            )));
                        }
                    }
                }
                let modules = modules.ok_or_else(|| {
                    serde::de::Error::custom(
                        "`[language] identity_storage` table needs `exempt_modules`",
                    )
                })?;
                if let Some(bad) = modules
                    .iter()
                    .find(|m| m.is_empty() || m.split("::").any(str::is_empty))
                {
                    return Err(serde::de::Error::custom(format!(
                        "`[language] identity_storage` exempt module `{bad}` is not a `::`-joined module path"
                    )));
                }
                Ok(IdentityStorageDeclaration(IdentityStorage::ExemptModules(
                    modules,
                )))
            }
        }
        d.deserialize_any(Vis)
    }
}

impl<'de> serde::Deserialize<'de> for LanguageConfig {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Vis;
        impl<'de> serde::de::Visitor<'de> for Vis {
            type Value = LanguageConfig;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(
                    f,
                    "a table with modules, globals and/or identity_storage keys"
                )
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<LanguageConfig, A::Error> {
                let mut modules = rock_core::source_code::Dialect::Legacy;
                let mut globals = rock_core::source_code::GlobalPolicy::Any;
                let mut identity_storage = rock_core::source_code::IdentityStorage::Enforced;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "modules" => {
                            let value = map.next_value::<String>()?;
                            modules = match value.as_str() {
                                "legacy" => rock_core::source_code::Dialect::Legacy,
                                "v1" => rock_core::source_code::Dialect::V1,
                                other => {
                                    return Err(serde::de::Error::custom(format!(
                                        "unknown `[language] modules` value `{other}`; known values: \"legacy\", \"v1\""
                                    )));
                                }
                            };
                        }
                        "memory" => {
                            return Err(serde::de::Error::custom(
                                "`[language] memory` was removed; delete it: packages have no memory class",
                            ));
                        }
                        "globals" => {
                            let value = map.next_value::<String>()?;
                            globals = rock_core::source_code::GlobalPolicy::parse(&value).ok_or_else(|| {
                                serde::de::Error::custom(format!(
                                    "unknown globals policy `{value}` in `[language] globals`; known values: {}",
                                    rock_core::source_code::GlobalPolicy::NAMES.join(", ")
                                ))
                            })?;
                        }
                        "identity_storage" => {
                            identity_storage = map.next_value::<IdentityStorageDeclaration>()?.0;
                        }
                        other => {
                            return Err(serde::de::Error::custom(format!(
                                "unknown key `{other}` in [language]; known keys: modules, globals, identity_storage"
                            )));
                        }
                    }
                }
                Ok(LanguageConfig {
                    modules,
                    globals,
                    identity_storage,
                })
            }
        }
        d.deserialize_map(Vis)
    }
}

#[derive(Debug, Clone)]
pub struct StickConfig {
    pub package: Package,
    /// Optional runtime requirements, interpreted by the installed host.
    pub engine: Option<EngineConfig>,
    pub native: Option<NativeConfig>,
    /// Declared compile-time inputs (`build_var(..)`) and release channels.
    /// Commands are run by the CLI once per session build, before compilation;
    /// the compiler only ever sees resolved values.
    pub build: Option<BuildConfig>,
    /// Release asset selection consumed by both `rock package` and `rock web`.
    pub release: Option<ReleaseConfig>,
    /// External asset roots.
    pub assets: Option<AssetsConfig>,
    /// Optional compiler-owned sound project declaration.
    pub sound: Option<SoundConfig>,
    /// Source-language selection for this package. `None` = legacy.
    pub language: Option<LanguageConfig>,
    pub dependencies: HashMap<String, Dependency>,
    /// Optional implementation directories that retain this package's owner.
    pub source_groups: HashMap<String, SourceGroupConfig>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceGroupConfig {
    pub directory: String,
    pub channels: Vec<String>,
}

impl StickConfig {
    /// This package's mutable-global policy. Missing `[language]` or missing
    /// `globals` keeps today's permissive behavior.
    pub fn global_policy(&self) -> rock_core::source_code::GlobalPolicy {
        self.language
            .as_ref()
            .map(|l| l.globals)
            .unwrap_or(rock_core::source_code::GlobalPolicy::Any)
    }

    /// This package's identity-storage migration state. Missing `[language]`
    /// or missing `identity_storage` is enforced.
    pub fn identity_storage(&self) -> rock_core::source_code::IdentityStorage {
        self.language
            .as_ref()
            .map(|l| l.identity_storage.clone())
            .unwrap_or_default()
    }

    /// This package's source dialect. Missing `[language]` is legacy.
    pub fn dialect(&self) -> rock_core::source_code::Dialect {
        self.language
            .as_ref()
            .map(|l| l.modules)
            .unwrap_or(rock_core::source_code::Dialect::Legacy)
    }
}

#[derive(Debug, Clone, Default)]
pub struct BuildConfig {
    pub channels: Vec<String>,
    pub default_channel: Option<String>,
    pub vars: HashMap<String, BuildVar>,
}

/// One declared build variable. Exactly one of `cmd`/`builtin`/`value`/
/// `source_fingerprint` is its source (`validate_build` enforces arity; the
/// shorthand `NAME = "literal"` parses as `value`). `channels` restricts where
/// the source runs; outside the list the var resolves to `default`, and a
/// restricted var with no default is a resolution error there.
#[derive(Debug, Clone, Default)]
pub struct BuildVar {
    pub cmd: Option<String>,
    pub builtin: Option<String>,
    pub value: Option<String>,
    /// A compiler-derived fingerprint resolved after semantic checking,
    /// without putting a placeholder into the resolved variable map.
    pub source_fingerprint: Option<SourceFingerprint>,
    pub channels: Option<Vec<String>>,
    /// Channels on which a failing `cmd` degrades to `default` with a warning
    /// instead of failing the build. Per-channel on purpose: a credential a
    /// development build can do without is one a release build cannot, and an
    /// all-channel switch would let a retail artifact ship the empty default.
    /// `validate_build` requires a `default` to fall back to and membership in
    /// `channels`, so the declaration can never be inert.
    pub optional_channels: Option<Vec<String>>,
    pub default: Option<String>,
    /// Hash the value into cache keys but never store it in plaintext in any
    /// on-disk artifact or manifest.
    pub secret: bool,
    /// Explicitly declared metadata that may vary without changing executable
    /// Artifact identity.
    pub volatile: bool,
}

/// Inputs whose contents contribute to a build variable's source fingerprint.
/// Empty lists are accepted by deserialization so validation can report the
/// missing root alongside the owning build variable.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SourceFingerprint {
    pub roots: Vec<String>,
    pub files: Vec<String>,
    pub vars: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ReleaseConfig {
    pub assets: Option<Vec<String>>,
    pub predefined_levels: Option<Vec<String>>,
    pub overlay_assets: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default)]
pub struct AssetsConfig {
    pub overlays: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SoundConfig {
    pub project: String,
}

pub const KNOWN_BUILD_BUILTINS: [&str; 3] = ["channel", "platform", "source_revision"];

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeConfig {
    pub manifest: String,
    pub library: String,
    #[serde(default = "native_shipped_default")]
    pub shipped: bool,
}

fn native_shipped_default() -> bool {
    true
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub needs: Vec<String>,
}

/// The invocation's channel selection, recorded once by the CLI before
/// dispatch. rock-host consumers of the channel (dependency resolution, source
/// loading) cannot read rock-cli's comptime selection, so the CLI mirrors the
/// same values here. Explicit `--channel` and a command-implied channel arrive
/// separately; resolution applies the same precedence as `resolve_build_vars`.
static INVOCATION_CHANNELS: OnceLock<Mutex<(Option<String>, Option<String>)>> = OnceLock::new();

fn invocation_channels_slot() -> &'static Mutex<(Option<String>, Option<String>)> {
    INVOCATION_CHANNELS.get_or_init(|| Mutex::new((None, None)))
}

pub fn set_invocation_channel(explicit: Option<String>, implied: Option<String>) {
    *invocation_channels_slot()
        .lock()
        .expect("invocation channel lock") = (explicit, implied);
}

/// Resolve the channel a manifest's channel-scoped selections evaluate
/// against: explicit `--channel` > command-implied channel (when the project
/// declares it) > the project's `default_channel` > `"dev"`. Mirrors
/// `resolve_build_vars` so a dependency filter and a build var can never
/// disagree about which channel a build is on.
pub fn resolved_channel_for(cfg: &StickConfig) -> String {
    let (explicit, implied) = invocation_channels_slot()
        .lock()
        .expect("invocation channel lock")
        .clone();
    if let Some(channel) = explicit {
        return channel;
    }
    if let Some(channel) = implied {
        if cfg
            .build
            .as_ref()
            .is_some_and(|b| b.channels.iter().any(|c| c == &channel))
        {
            return channel;
        }
    }
    cfg.build
        .as_ref()
        .and_then(|b| b.default_channel.clone())
        .unwrap_or_else(|| "dev".to_string())
}

/// True when `dep` compiles into a build on `channel`. A dependency with no
/// declared channels is unscoped and matches everywhere.
pub fn dependency_matches_channel(dep: &Dependency, channel: &str) -> bool {
    match dep {
        Dependency::Path(_) => true,
        Dependency::Detailed(detailed) => {
            detailed.channels.is_empty() || detailed.channels.iter().any(|c| c == channel)
        }
    }
}

#[derive(Debug, Clone)]
pub struct Package {
    pub name: String,
    // Optional with a sensible default so a minimal `[package]\nname =
    // "x"\n` stick.toml parses cleanly. Required-by-serde used to
    // crash rock-lsp (toml parse error -> `?` in find_from_dir ->
    // load_project_files returns Err -> non-zero exit ~100ms after
    // initialize). Real projects always set version; the default just
    // protects minimal/test/in-progress configs.
    /// Stored as `Option<String>` because serde's default calls `T::default()`;
    /// the read accessor `version()` returns `"0.0.0"` when missing.
    pub version: Option<String>,
    /// `Option` for the same reason as `version`. Read via `package_type()`,
    /// which defaults to `Bin`.
    pub package_type: Option<PackageType>,
}

impl Package {
    pub fn version(&self) -> &str {
        self.version.as_deref().unwrap_or("0.0.0")
    }
    pub fn package_type(&self) -> PackageType {
        self.package_type.unwrap_or(PackageType::Bin)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PackageType {
    Bin,
    Lib,
}

// `[build] target` and `Target` stay removed: Lua was the only codegen
// target Rock ever shipped, so that field was tautological. Game-vs-lib
// detection reads `[package] type` -- never a
// build target. `[build]` now carries channels and declared compile-time
// vars instead (BuildConfig above).

/// `Path(String)` because PathBuf isn't a built-in serde scalar; the
/// consumer sites (`resolver.rs`, `compilation_units.rs`) convert via
/// `PathBuf::from(s)` at use time.
#[derive(Debug, Clone)]
#[repr(u8)]
pub enum Dependency {
    Path(String),
    Detailed(DetailedDependency),
}

#[derive(Debug, Clone)]
pub struct DetailedDependency {
    pub source: DependencySource,
    /// Channels the dependency compiles into. Empty = every channel. This is
    /// the declared gate for development-only packages (such as an agent bridge):
    /// a shipping channel excludes the package from the compile set entirely,
    /// so generated shipping code carries none of its symbols.
    pub channels: Vec<String>,
    /// Authorizes a v1 consumer's dependency edge to a legacy provider
    /// (modules spec 3.2). Rejected when the edge lacks it; a removable
    /// warning when the provider is already v1.
    pub allow_legacy: bool,
}

/// A dependency has exactly one source; package names refer to this toolchain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependencySource {
    Path(String),
    Package(String),
}

// ---------------------------------------------------------------------------
// serde::Deserialize impls — hand-rolled to avoid pulling serde_derive
// (and thus syn) into the front end's compile-time critical path.
// ---------------------------------------------------------------------------

impl<'de> serde::Deserialize<'de> for PackageType {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Vis;
        impl<'de> serde::de::Visitor<'de> for Vis {
            type Value = PackageType;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(f, "\"bin\" or \"lib\"")
            }
            fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<PackageType, E> {
                match s {
                    "bin" => Ok(PackageType::Bin),
                    "lib" => Ok(PackageType::Lib),
                    _ => Err(E::unknown_variant(s, &["bin", "lib"])),
                }
            }
        }
        d.deserialize_str(Vis)
    }
}

impl<'de> serde::Deserialize<'de> for DetailedDependency {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Vis;
        impl<'de> serde::de::Visitor<'de> for Vis {
            type Value = DetailedDependency;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(f, "a table with exactly one path or package key")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<DetailedDependency, A::Error> {
                let mut source = None;
                let mut channels: Vec<String> = Vec::new();
                let mut allow_legacy = false;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "path" | "package" => {
                            if source.is_some() {
                                return Err(serde::de::Error::custom(
                                    "dependency requires exactly one path or package key",
                                ));
                            }
                            let value: String = map.next_value()?;
                            source = Some(if key == "path" {
                                DependencySource::Path(value)
                            } else {
                                DependencySource::Package(value)
                            });
                        }
                        "channels" => {
                            channels = map.next_value()?;
                        }
                        "allow_legacy" => {
                            allow_legacy = map.next_value()?;
                        }
                        other => {
                            return Err(serde::de::Error::custom(format!(
                                "unknown key `{other}` in dependency; known keys: path, package, channels, allow_legacy"
                            )));
                        }
                    }
                }
                let source = source.ok_or_else(|| {
                    serde::de::Error::custom("dependency requires exactly one path or package key")
                })?;
                Ok(DetailedDependency {
                    source,
                    channels,
                    allow_legacy,
                })
            }
        }
        d.deserialize_map(Vis)
    }
}

impl<'de> serde::Deserialize<'de> for Dependency {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Vis;
        impl<'de> serde::de::Visitor<'de> for Vis {
            type Value = Dependency;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(
                    f,
                    "a path string or table with exactly one path or package key"
                )
            }
            fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<Dependency, E> {
                Ok(Dependency::Path(s.to_string()))
            }
            fn visit_string<E: serde::de::Error>(self, s: String) -> Result<Dependency, E> {
                Ok(Dependency::Path(s))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                map: A,
            ) -> Result<Dependency, A::Error> {
                <DetailedDependency as serde::Deserialize>::deserialize(
                    serde::de::value::MapAccessDeserializer::new(map),
                )
                .map(Dependency::Detailed)
            }
        }
        d.deserialize_any(Vis)
    }
}

impl<'de> serde::Deserialize<'de> for EngineConfig {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Vis;
        impl<'de> serde::de::Visitor<'de> for Vis {
            type Value = EngineConfig;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(f, "EngineConfig struct")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<EngineConfig, A::Error> {
                let mut needs: Vec<String> = Vec::new();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "needs" => {
                            needs = map.next_value()?;
                        }
                        other => {
                            return Err(serde::de::Error::custom(format!(
                                "unknown key `{other}` in [engine]; known keys: needs"
                            )));
                        }
                    }
                }
                Ok(EngineConfig { needs })
            }
        }
        d.deserialize_map(Vis)
    }
}

impl<'de> serde::Deserialize<'de> for BuildVar {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Vis;
        impl<'de> serde::de::Visitor<'de> for Vis {
            type Value = BuildVar;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(
                    f,
                    "a literal string or a table with cmd/builtin/value/source_fingerprint/channels/optional_channels/default/secret/volatile"
                )
            }
            fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<BuildVar, E> {
                Ok(BuildVar {
                    value: Some(s.to_string()),
                    ..BuildVar::default()
                })
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<BuildVar, A::Error> {
                let mut var = BuildVar::default();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "cmd" => {
                            var.cmd = Some(map.next_value()?);
                        }
                        "builtin" => {
                            var.builtin = Some(map.next_value()?);
                        }
                        "value" => {
                            var.value = Some(map.next_value()?);
                        }
                        "source_fingerprint" => {
                            var.source_fingerprint = Some(map.next_value()?);
                        }
                        "channels" => {
                            var.channels = Some(map.next_value()?);
                        }
                        "optional_channels" => {
                            var.optional_channels = Some(map.next_value()?);
                        }
                        "default" => {
                            var.default = Some(map.next_value()?);
                        }
                        "secret" => {
                            var.secret = map.next_value()?;
                        }
                        "volatile" => {
                            var.volatile = map.next_value()?;
                        }
                        other => {
                            // Fail closed: a misspelled source key would leave the
                            // var sourceless and every build_var() reading it stale.
                            return Err(serde::de::Error::custom(format!(
                                "unknown key `{other}` in [build.vars] entry; known keys: \
                                 cmd, builtin, value, source_fingerprint, channels, optional_channels, default, secret, volatile"
                            )));
                        }
                    }
                }
                Ok(var)
            }
        }
        d.deserialize_any(Vis)
    }
}

impl<'de> serde::Deserialize<'de> for BuildConfig {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Vis;
        impl<'de> serde::de::Visitor<'de> for Vis {
            type Value = BuildConfig;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(f, "BuildConfig struct")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<BuildConfig, A::Error> {
                let mut build = BuildConfig::default();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "channels" => {
                            build.channels = map.next_value()?;
                        }
                        "default_channel" => {
                            build.default_channel = Some(map.next_value()?);
                        }
                        "vars" => {
                            build.vars = map.next_value()?;
                        }
                        other => {
                            // Fail closed: a misspelled key here would silently drop
                            // every declared var and channel.
                            return Err(serde::de::Error::custom(format!(
                                "unknown key `{other}` in [build]; known keys: \
                                 channels, default_channel, vars"
                            )));
                        }
                    }
                }
                Ok(build)
            }
        }
        d.deserialize_map(Vis)
    }
}

impl<'de> serde::Deserialize<'de> for ReleaseConfig {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Vis;
        impl<'de> serde::de::Visitor<'de> for Vis {
            type Value = ReleaseConfig;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(f, "ReleaseConfig struct")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<ReleaseConfig, A::Error> {
                let mut release = ReleaseConfig::default();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "assets" => {
                            release.assets = Some(map.next_value()?);
                        }
                        "predefined_levels" => {
                            release.predefined_levels = Some(map.next_value()?);
                        }
                        "overlay_assets" => {
                            release.overlay_assets = Some(map.next_value()?);
                        }
                        other => {
                            return Err(serde::de::Error::custom(format!(
                                "unknown key `{other}` in [release]; known keys: assets, predefined_levels, overlay_assets"
                            )));
                        }
                    }
                }
                Ok(release)
            }
        }
        d.deserialize_map(Vis)
    }
}

impl<'de> serde::Deserialize<'de> for AssetsConfig {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Vis;
        impl<'de> serde::de::Visitor<'de> for Vis {
            type Value = AssetsConfig;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(f, "AssetsConfig struct")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<AssetsConfig, A::Error> {
                let mut assets = AssetsConfig::default();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "overlays" => {
                            assets.overlays = map.next_value()?;
                        }
                        other => {
                            return Err(serde::de::Error::custom(format!(
                                "unknown key `{other}` in [assets]; known keys: overlays"
                            )));
                        }
                    }
                }
                Ok(assets)
            }
        }
        d.deserialize_map(Vis)
    }
}

impl<'de> serde::Deserialize<'de> for SoundConfig {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Vis;
        impl<'de> serde::de::Visitor<'de> for Vis {
            type Value = SoundConfig;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(f, "SoundConfig struct")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<SoundConfig, A::Error> {
                let mut project: Option<String> = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "project" => {
                            project = Some(map.next_value()?);
                        }
                        other => {
                            return Err(serde::de::Error::custom(format!(
                                "unknown key `{other}` in [sound]; known keys: project"
                            )));
                        }
                    }
                }
                let project = project.ok_or_else(|| serde::de::Error::missing_field("project"))?;
                Ok(SoundConfig { project })
            }
        }
        d.deserialize_map(Vis)
    }
}

impl<'de> serde::Deserialize<'de> for Package {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Vis;
        impl<'de> serde::de::Visitor<'de> for Vis {
            type Value = Package;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(f, "Package struct")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Package, A::Error> {
                let mut name: Option<String> = None;
                let mut version: Option<String> = None;
                let mut package_type: Option<PackageType> = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "name" => {
                            name = Some(map.next_value()?);
                        }
                        "version" => {
                            version = map.next_value()?;
                        }
                        "type" => {
                            package_type = map.next_value()?;
                        }
                        other => {
                            return Err(serde::de::Error::custom(format!(
                                "unknown key `{other}` in [package]; known keys: name, version, type"
                            )));
                        }
                    }
                }
                let name = name.ok_or_else(|| serde::de::Error::missing_field("name"))?;
                Ok(Package {
                    name,
                    version,
                    package_type,
                })
            }
        }
        d.deserialize_map(Vis)
    }
}

impl<'de> serde::Deserialize<'de> for StickConfig {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Vis;
        impl<'de> serde::de::Visitor<'de> for Vis {
            type Value = StickConfig;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(f, "StickConfig struct")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<StickConfig, A::Error> {
                let mut package: Option<Package> = None;
                let mut engine: Option<EngineConfig> = None;
                let mut native: Option<NativeConfig> = None;
                let mut build: Option<BuildConfig> = None;
                let mut release: Option<ReleaseConfig> = None;
                let mut assets: Option<AssetsConfig> = None;
                let mut sound: Option<SoundConfig> = None;
                let mut language: Option<LanguageConfig> = None;
                let mut dependencies: HashMap<String, Dependency> = HashMap::new();
                let mut source_groups: HashMap<String, SourceGroupConfig> = HashMap::new();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "package" => {
                            package = Some(map.next_value()?);
                        }
                        "build" => {
                            build = Some(map.next_value()?);
                        }
                        "release" => {
                            release = Some(map.next_value()?);
                        }
                        "assets" => {
                            assets = Some(map.next_value()?);
                        }
                        "sound" => {
                            sound = Some(map.next_value()?);
                        }
                        "native" => {
                            native = map.next_value()?;
                        }
                        "engine" => {
                            engine = map.next_value()?;
                        }
                        "language" => {
                            language = map.next_value()?;
                        }
                        "dependencies" => {
                            dependencies = map.next_value()?;
                        }
                        "source_groups" => {
                            source_groups = map.next_value()?;
                        }
                        other => {
                            return Err(serde::de::Error::custom(format!(
                                "unknown key `{other}` at top level; known keys: package, build, release, \
                                 assets, sound, engine, native, language, dependencies, source_groups"
                            )));
                        }
                    }
                }
                let package = package.ok_or_else(|| serde::de::Error::missing_field("package"))?;
                Ok(StickConfig {
                    package,
                    engine,
                    native,
                    build,
                    release,
                    assets,
                    sound,
                    language,
                    dependencies,
                    source_groups,
                })
            }
        }
        d.deserialize_map(Vis)
    }
}

impl<'de> serde::Deserialize<'de> for StickLockDependency {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Vis;
        impl<'de> serde::de::Visitor<'de> for Vis {
            type Value = StickLockDependency;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(f, "StickLockDependency struct")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<StickLockDependency, A::Error> {
                let mut name: Option<String> = None;
                let mut version: Option<String> = None;
                let mut source: Option<String> = None;
                let mut checksum: Option<String> = None;
                let mut dependencies: Vec<String> = Vec::new();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "name" => {
                            name = Some(map.next_value()?);
                        }
                        "version" => {
                            version = Some(map.next_value()?);
                        }
                        "source" => {
                            source = Some(map.next_value()?);
                        }
                        "checksum" => {
                            checksum = map.next_value()?;
                        }
                        "dependencies" => {
                            dependencies = map.next_value()?;
                        }
                        _ => {
                            let _ = map.next_value::<serde::de::IgnoredAny>()?;
                        }
                    }
                }
                let name = name.ok_or_else(|| serde::de::Error::missing_field("name"))?;
                let version = version.ok_or_else(|| serde::de::Error::missing_field("version"))?;
                let source = source.ok_or_else(|| serde::de::Error::missing_field("source"))?;
                Ok(StickLockDependency {
                    name,
                    version,
                    source,
                    checksum,
                    dependencies,
                })
            }
        }
        d.deserialize_map(Vis)
    }
}

impl<'de> serde::Deserialize<'de> for StickLock {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Vis;
        impl<'de> serde::de::Visitor<'de> for Vis {
            type Value = StickLock;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(f, "StickLock struct")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<StickLock, A::Error> {
                let mut version: Option<u32> = None;
                let mut compiler_sha1: Option<String> = None;
                let mut dependencies: Vec<StickLockDependency> = Vec::new();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "version" => {
                            version = Some(map.next_value()?);
                        }
                        "compiler_sha1" => {
                            compiler_sha1 = Some(map.next_value()?);
                        }
                        "dependencies" => {
                            dependencies = map.next_value()?;
                        }
                        _ => {
                            let _ = map.next_value::<serde::de::IgnoredAny>()?;
                        }
                    }
                }
                let version = version.ok_or_else(|| serde::de::Error::missing_field("version"))?;
                let compiler_sha1 = compiler_sha1
                    .ok_or_else(|| serde::de::Error::missing_field("compiler_sha1"))?;
                Ok(StickLock {
                    version,
                    compiler_sha1,
                    dependencies,
                })
            }
        }
        d.deserialize_map(Vis)
    }
}

impl StickConfig {
    pub fn from_path(path: &Path) -> Result<Self> {
        let content = fs_err::read_to_string(path)
            .with_context(|| format!("Failed to read stick.toml at {}", path.display()))?;
        Self::new_from_str(&content)
    }

    /// Load a manifest if it exists. Distinguishes "no manifest" (Ok(None))
    /// from "manifest exists but is malformed" (Err), so callers stop
    /// laundering parse errors as "no game config" — a typo in
    /// `[engine] needs = [...]` would otherwise silently skip
    /// runtime validation and surface as a confusing later failure
    /// (silent-errors audit, 2026-05-02).
    pub fn from_path_optional(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        Self::from_path(path).map(Some)
    }

    pub fn new_from_str(content: &str) -> Result<Self> {
        basic_toml::from_str(content)
            .map_err(|e| anyhow::anyhow!("Failed to parse stick.toml: {}", e))
    }

    /// Reject a malformed `[build]` section at load time: a var with zero or
    /// two sources, an unknown builtin, or a channel name nothing declares
    /// would otherwise surface as a confusing resolution failure (or silently
    /// wrong value) at the first `build_var()` call.
    pub fn validate_build(&self) -> Result<()> {
        let Some(build) = self.build.as_ref() else {
            return Ok(());
        };
        if let Some(default) = build.default_channel.as_deref() {
            if !build.channels.iter().any(|c| c == default) {
                anyhow::bail!(
                    "stick.toml: [build] default_channel {:?} is not in channels {:?}",
                    default,
                    build.channels
                );
            }
        }
        for (name, var) in &build.vars {
            let sources = [
                var.cmd.is_some(),
                var.builtin.is_some(),
                var.value.is_some(),
                var.source_fingerprint.is_some(),
            ];
            match sources.iter().filter(|s| **s).count() {
                1 => {}
                0 => anyhow::bail!(
                    "stick.toml: [build.vars] {name} has no source; set one of cmd, builtin, value, source_fingerprint"
                ),
                _ => anyhow::bail!(
                    "stick.toml: [build.vars] {name} has more than one source; \
                     set exactly one of cmd, builtin, value, source_fingerprint"
                ),
            }
            if let Some(builtin) = var.builtin.as_deref() {
                if !KNOWN_BUILD_BUILTINS.contains(&builtin) {
                    anyhow::bail!(
                        "stick.toml: [build.vars] {name} names unknown builtin {:?}. Known: {:?}",
                        builtin,
                        KNOWN_BUILD_BUILTINS
                    );
                }
            }
            if let Some(var_channels) = var.channels.as_deref() {
                for chan in var_channels {
                    if !build.channels.iter().any(|c| c == chan) {
                        anyhow::bail!(
                            "stick.toml: [build.vars] {name} restricts to channel {:?}, \
                             which is not in [build] channels {:?}",
                            chan,
                            build.channels
                        );
                    }
                }
            }
            if let Some(fingerprint) = &var.source_fingerprint {
                if var.volatile {
                    anyhow::bail!(
                        "[build.vars] {name} uses source_fingerprint and cannot be volatile"
                    );
                }
                if fingerprint.roots.is_empty() {
                    anyhow::bail!(
                        "[build.vars] {name} source_fingerprint requires at least one root"
                    );
                }
                if fingerprint.roots.iter().any(|root| root.trim().is_empty()) {
                    anyhow::bail!("[build.vars] {name} source_fingerprint roots must be nonempty");
                }
                if fingerprint.files.iter().any(|file| file.trim().is_empty()) {
                    anyhow::bail!("[build.vars] {name} source_fingerprint files must be nonempty");
                }
                for dependency in &fingerprint.vars {
                    if dependency.trim().is_empty() {
                        anyhow::bail!(
                            "[build.vars] {name} source_fingerprint var names must be nonempty"
                        );
                    }
                    if dependency == name {
                        anyhow::bail!(
                            "[build.vars] {name} source_fingerprint cannot depend on itself"
                        );
                    }
                    let Some(declared) = build.vars.get(dependency) else {
                        anyhow::bail!(
                            "[build.vars] {name} source_fingerprint references undeclared var {dependency:?}"
                        );
                    };
                    if declared.source_fingerprint.is_some() {
                        anyhow::bail!(
                            "[build.vars] {name} source_fingerprint cannot depend on deferred var {dependency:?}"
                        );
                    }
                }
            }
            if let Some(optional_channels) = var.optional_channels.as_deref() {
                if var.cmd.is_none() {
                    anyhow::bail!(
                        "[build.vars] {name} declares optional_channels but has no cmd source"
                    );
                }
                if var.default.is_none() {
                    anyhow::bail!(
                        "[build.vars] {name} declares optional_channels but has no default"
                    );
                }
                for chan in optional_channels {
                    if !build.channels.iter().any(|c| c == chan) {
                        anyhow::bail!(
                            "[build.vars] {name} declares optional channel {:?}, which is not in [build] channels {:?}",
                            chan,
                            build.channels
                        );
                    }
                }
            }
        }
        Ok(())
    }

    /// True iff the deepest `stick.toml` at or above `dir` is a `bin` package
    /// (the default when `type` is absent). False for a `lib`, and false when
    /// no `stick.toml` exists -- a standalone scratch file is not a closed
    /// world. The gate for closed-world analyses (`unused_fn`), read by both
    /// the LSP and the build lint pass.
    pub fn is_bin_at(dir: &Path) -> bool {
        matches!(
            Self::find_from_dir(dir),
            Ok(Some((_, cfg))) if cfg.package.package_type() == PackageType::Bin
        )
    }

    pub fn find_from_current_dir() -> Result<Option<(PathBuf, Self)>> {
        let current_dir = std::env::current_dir()?;
        Self::find_from_dir(&current_dir)
    }

    pub fn find_from_dir(start_dir: &Path) -> Result<Option<(PathBuf, Self)>> {
        let mut current = start_dir;

        loop {
            let stick_path = current.join("stick.toml");
            if stick_path.exists() {
                let config = Self::from_path(&stick_path)?;
                return Ok(Some((stick_path, config)));
            }

            match current.parent() {
                Some(parent) => current = parent,
                None => return Ok(None),
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct StickLock {
    pub version: u32,
    pub compiler_sha1: String,
    pub dependencies: Vec<StickLockDependency>,
}

#[derive(Debug, Clone)]
pub struct StickLockDependency {
    pub name: String,
    pub version: String,
    pub source: String,
    pub checksum: Option<String>,
    pub dependencies: Vec<String>,
}

impl StickLock {
    pub fn from_path(path: &Path) -> Result<Self> {
        let content = fs_err::read_to_string(path)
            .with_context(|| format!("Failed to read stick.lock at {}", path.display()))?;
        basic_toml::from_str(&content)
            .map_err(|e| anyhow::anyhow!("Failed to parse stick.lock: {}", e))
    }

    pub fn to_path(&self, path: &Path) -> Result<()> {
        let content = self.to_toml_string();
        fs_err::write(path, content)
            .with_context(|| format!("Failed to write stick.lock to {}", path.display()))?;
        Ok(())
    }

    fn to_toml_string(&self) -> String {
        // Hand-rolled writer; the toml crate's `display` feature pulls
        // toml_edit (~6s of compile time). Schema is fixed and tiny.
        let mut out = String::new();
        out.push_str(&format!("version = {}\n", self.version));
        out.push_str(&format!(
            "compiler_sha1 = {}\n",
            toml_str(&self.compiler_sha1)
        ));
        for dep in &self.dependencies {
            out.push_str("\n[[dependencies]]\n");
            out.push_str(&format!("name = {}\n", toml_str(&dep.name)));
            out.push_str(&format!("version = {}\n", toml_str(&dep.version)));
            out.push_str(&format!("source = {}\n", toml_str(&dep.source)));
            if let Some(ref c) = dep.checksum {
                out.push_str(&format!("checksum = {}\n", toml_str(c)));
            }
            if !dep.dependencies.is_empty() {
                let inner: Vec<String> = dep.dependencies.iter().map(|s| toml_str(s)).collect();
                out.push_str(&format!("dependencies = [{}]\n", inner.join(", ")));
            } else {
                out.push_str("dependencies = []\n");
            }
        }
        out
    }
}

/// Quote a string as a TOML basic string with the escapes the spec requires.
fn toml_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod stick_lock_tests {
    use super::*;

    #[test]
    fn stick_lock_roundtrip() {
        let lock = StickLock {
            version: 1,
            compiler_sha1: "abc\"\\def".to_string(),
            dependencies: vec![
                StickLockDependency {
                    name: "foo".to_string(),
                    version: "1.2.3".to_string(),
                    source: "git+https://example.com".to_string(),
                    checksum: Some("deadbeef".to_string()),
                    dependencies: vec!["bar".to_string(), "baz".to_string()],
                },
                StickLockDependency {
                    name: "empty".to_string(),
                    version: "0.0.1".to_string(),
                    source: "registry+https://example.com".to_string(),
                    checksum: None,
                    dependencies: vec![],
                },
            ],
        };
        let s = lock.to_toml_string();
        let parsed: StickLock = basic_toml::from_str(&s).expect("parse");
        assert_eq!(parsed.version, lock.version);
        assert_eq!(parsed.compiler_sha1, lock.compiler_sha1);
        assert_eq!(parsed.dependencies.len(), 2);
        assert_eq!(parsed.dependencies[0].name, "foo");
        assert_eq!(parsed.dependencies[0].dependencies, vec!["bar", "baz"]);
        assert_eq!(parsed.dependencies[1].checksum, None);
        assert!(parsed.dependencies[1].dependencies.is_empty());
    }
}

/// Lowercase 40-char hex SHA1 of `bytes`, computed in-process. Matches the
/// `sha1sum` output format so values stay stable across platforms.
fn sha1_hex(bytes: &[u8]) -> String {
    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(40);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Get the SHA1 hash of the current compiler executable
pub fn get_compiler_sha1() -> Result<String> {
    use fs_err as fs;
    use std::io::Read;

    // Get the path to the current executable
    let exe_path = crate::process_management::current_executable_image()
        .context("Failed to get current executable path")?;

    // Read the executable file
    let mut file = fs::File::open(&exe_path).context("Failed to open compiler executable")?;
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)
        .context("Failed to read compiler executable")?;

    Ok(sha1_hex(&buffer))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_hex_matches_known_vectors() {
        // Standard SHA1 test vectors. get_compiler_sha1 used to shell out
        // to `sha1sum` (absent on Windows); this in-process hash must
        // reproduce the exact 40-hex-char value so existing stick locks
        // stay valid.
        assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
    }

    #[test]
    fn toolchain_package_dependencies_resolve_through_the_graph() {
        let temp = tempfile::tempdir().unwrap();
        let cfg = StickConfig::new_from_str(
            "[package]\nname = \"consumer\"\n[language]\nmodules = \"v1\"\n[dependencies]\nprobe = { package = \"package-probe\" }\n"
        ).unwrap();
        let graph = graph::PackageGraph::build_with_channel(temp.path(), &cfg, "dev").unwrap();
        let provider = graph
            .packages()
            .iter()
            .find(|p| p.manifest_name == "package-probe")
            .unwrap();
        assert_eq!(
            provider.root,
            std::fs::canonicalize(rock_stdlib::package_dir("package-probe").unwrap()).unwrap()
        );
    }

    #[test]
    fn the_language_memory_key_is_a_removal_error() {
        let err =
            StickConfig::new_from_str("[package]\nname = \"x\"\n[language]\nmemory = \"game\"\n")
                .expect_err("`[language] memory` must not load");
        let text = format!("{err:#}");
        assert!(
            text.contains("`[language] memory` was removed; delete it"),
            "{text}"
        );
    }

    #[test]
    fn the_language_identity_storage_key_declares_the_migration_state() {
        use rock_core::source_code::IdentityStorage;
        let state = |language: &str| {
            StickConfig::new_from_str(&format!("[package]\nname = \"x\"\n{language}"))
                .map(|c| c.identity_storage())
        };
        assert_eq!(state("").unwrap(), IdentityStorage::Enforced);
        assert_eq!(
            state("[language]\nidentity_storage = \"enforced\"\n").unwrap(),
            IdentityStorage::Enforced
        );
        assert_eq!(
            state("[language]\nidentity_storage = \"exempt\"\n").unwrap(),
            IdentityStorage::Exempt
        );
        assert_eq!(
            state("[language]\nidentity_storage = { exempt_modules = [\"nav\", \"ui::old\"] }\n")
                .unwrap(),
            IdentityStorage::ExemptModules(vec!["nav".into(), "ui::old".into()])
        );
        for (bad, want) in [
            (
                "\"unchecked\"",
                "unknown `[language] identity_storage` value `unchecked`",
            ),
            (
                "{ modules = [\"nav\"] }",
                "unknown key `modules` in `[language] identity_storage`",
            ),
            ("{}", "table needs `exempt_modules`"),
            (
                "{ exempt_modules = [\"nav::\"] }",
                "exempt module `nav::` is not a `::`-joined module path",
            ),
        ] {
            let err = state(&format!("[language]\nidentity_storage = {bad}\n")).expect_err(bad);
            let text = format!("{err:#}");
            assert!(text.contains(want), "{bad}: {text}");
        }
    }

    #[test]
    fn dependency_tables_require_exactly_one_source() {
        for source in [
            "",
            "path = \"../x\", package = \"x\"",
            "package = \"x\", package = \"y\"",
        ] {
            let text =
                format!("[package]\nname = \"consumer\"\n[dependencies]\nx = {{ {source} }}\n");
            assert!(StickConfig::new_from_str(&text).is_err(), "{text}");
        }
    }

    #[test]
    fn a_channel_scoped_dependency_parses_its_channel_list() {
        let cfg = StickConfig::new_from_str(
            "[package]\nname = \"x\"\n[dependencies]\ndev-only = { path = \"../dep\", channels = [\"dev\"] }\n",
        )
        .expect("channel-scoped dependency parses");
        let Dependency::Detailed(dep) = &cfg.dependencies["dev-only"] else {
            panic!("inline table dependency must parse as detailed");
        };
        assert_eq!(dep.source, DependencySource::Path("../dep".into()));
        assert_eq!(dep.channels, vec!["dev".to_string()]);
    }

    #[test]
    fn a_path_dependency_has_no_channel_scope() {
        let cfg = StickConfig::new_from_str(
            "[package]\nname = \"x\"\n[dependencies]\ncore = \"../core\"\n",
        )
        .expect("path dependency parses");
        assert!(matches!(cfg.dependencies["core"], Dependency::Path(_)));
    }

    #[test]
    fn a_channel_scoped_dependency_resolves_only_on_a_matching_channel() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let consumer = workspace.path().join("consumer");
        let dep = workspace.path().join("dep");
        fs_err::create_dir_all(&consumer).unwrap();
        fs_err::create_dir_all(&dep).unwrap();
        fs_err::write(
            consumer.join("stick.toml"),
            "[package]\nname = \"consumer\"\n[dependencies]\ndev-only = { path = \"../dep\", channels = [\"dev\"] }\n",
        )
        .unwrap();
        fs_err::write(
            dep.join("stick.toml"),
            "[package]\nname = \"dep\"\ntype = \"lib\"\n",
        )
        .unwrap();

        let cfg = StickConfig::from_path(&consumer.join("stick.toml")).unwrap();
        let dev =
            graph::PackageGraph::build_with_channel(&consumer, &cfg, "dev").expect("dev resolve");
        assert_eq!(
            dev.package(dev.root()).dependencies.len(),
            1,
            "matching channel includes the scoped dep"
        );

        let steam = graph::PackageGraph::build_with_channel(&consumer, &cfg, "steam")
            .expect("steam resolve");
        assert!(
            steam.package(steam.root()).dependencies.is_empty(),
            "non-matching channel excludes the scoped dep"
        );
    }

    #[test]
    fn an_unscoped_dependency_resolves_on_every_channel() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let consumer = workspace.path().join("consumer");
        let dep = workspace.path().join("dep");
        fs_err::create_dir_all(&consumer).unwrap();
        fs_err::create_dir_all(&dep).unwrap();
        fs_err::write(
            consumer.join("stick.toml"),
            "[package]\nname = \"consumer\"\n[dependencies]\ncore = \"../dep\"\n",
        )
        .unwrap();
        fs_err::write(
            dep.join("stick.toml"),
            "[package]\nname = \"dep\"\ntype = \"lib\"\n",
        )
        .unwrap();

        let cfg = StickConfig::from_path(&consumer.join("stick.toml")).unwrap();
        for channel in ["dev", "steam", "itch"] {
            let resolved =
                graph::PackageGraph::build_with_channel(&consumer, &cfg, channel).expect("resolve");
            assert_eq!(
                resolved.package(resolved.root()).dependencies.len(),
                1,
                "unscoped dep resolves on {channel}"
            );
        }
    }

    /// A three-package chain, consumer -> mid -> leaf, where the consumer
    /// declares only `mid`. Without transitive resolution `leaf` is silently
    /// dropped from the module set and every symbol it defines is an undefined
    /// name at type-check time, which is what made a shared core package
    /// impossible without editing every consumer's manifest.
    #[test]
    fn a_transitive_dependency_resolves_through_its_parent() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let consumer = workspace.path().join("consumer");
        let mid = workspace.path().join("mid");
        let leaf = workspace.path().join("leaf");
        for dir in [&consumer, &mid, &leaf] {
            fs_err::create_dir_all(dir).unwrap();
        }
        fs_err::write(
            consumer.join("stick.toml"),
            "[package]\nname = \"consumer\"\n[dependencies]\nmid = \"../mid\"\n",
        )
        .unwrap();
        fs_err::write(
            mid.join("stick.toml"),
            "[package]\nname = \"mid\"\ntype = \"lib\"\n[dependencies]\nleaf = \"../leaf\"\n",
        )
        .unwrap();
        fs_err::write(
            leaf.join("stick.toml"),
            "[package]\nname = \"leaf\"\ntype = \"lib\"\n",
        )
        .unwrap();

        let cfg = StickConfig::from_path(&consumer.join("stick.toml")).unwrap();
        let graph =
            graph::PackageGraph::build_with_channel(&consumer, &cfg, "dev").expect("resolve");
        let names: Vec<&str> = graph
            .load_order()
            .iter()
            .filter(|id| **id != graph.root())
            .map(|id| graph.package(*id).manifest_name.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["leaf", "mid"],
            "a transitive dependency is resolved, and before the package that needs it"
        );
    }

    /// The path in a dependency's manifest is relative to THAT manifest, not to
    /// the root project. A chain that only works when every package sits in one
    /// flat directory would pass the test above and still break on the real
    /// tree, where libs/ and games/ are nested at different depths.
    #[test]
    fn a_transitive_path_resolves_against_its_own_manifest() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let consumer = workspace.path().join("games").join("consumer");
        let mid = workspace.path().join("libs").join("mid");
        let leaf = workspace.path().join("leaf");
        for dir in [&consumer, &mid, &leaf] {
            fs_err::create_dir_all(dir).unwrap();
        }
        fs_err::write(
            consumer.join("stick.toml"),
            "[package]\nname = \"consumer\"\n[dependencies]\nmid = \"../../libs/mid\"\n",
        )
        .unwrap();
        // "../../leaf" from libs/mid. Read against the root it would be wrong.
        fs_err::write(
            mid.join("stick.toml"),
            "[package]\nname = \"mid\"\ntype = \"lib\"\n[dependencies]\nleaf = \"../../leaf\"\n",
        )
        .unwrap();
        fs_err::write(
            leaf.join("stick.toml"),
            "[package]\nname = \"leaf\"\ntype = \"lib\"\n",
        )
        .unwrap();

        let cfg = StickConfig::from_path(&consumer.join("stick.toml")).unwrap();
        let graph =
            graph::PackageGraph::build_with_channel(&consumer, &cfg, "dev").expect("resolve");
        let found = graph
            .packages()
            .iter()
            .find(|p| p.manifest_name == "leaf")
            .expect("the transitive leaf resolves");
        assert_eq!(
            found.root.canonicalize().unwrap(),
            leaf.canonicalize().unwrap(),
            "the leaf resolves to the path its own manifest names"
        );
    }

    /// Channel scoping has to survive the extra hop, or a dev-only package
    /// reached through a library would ship in a release build.
    #[test]
    fn a_channel_scoped_transitive_dependency_keeps_its_scope() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let consumer = workspace.path().join("consumer");
        let mid = workspace.path().join("mid");
        let leaf = workspace.path().join("leaf");
        for dir in [&consumer, &mid, &leaf] {
            fs_err::create_dir_all(dir).unwrap();
        }
        fs_err::write(
            consumer.join("stick.toml"),
            "[package]\nname = \"consumer\"\n[dependencies]\nmid = \"../mid\"\n",
        )
        .unwrap();
        fs_err::write(
            mid.join("stick.toml"),
            "[package]\nname = \"mid\"\ntype = \"lib\"\n[dependencies]\nleaf = { path = \"../leaf\", channels = [\"dev\"] }\n",
        )
        .unwrap();
        fs_err::write(
            leaf.join("stick.toml"),
            "[package]\nname = \"leaf\"\ntype = \"lib\"\n",
        )
        .unwrap();

        let cfg = StickConfig::from_path(&consumer.join("stick.toml")).unwrap();
        let names_on = |channel: &str| -> Vec<String> {
            let graph =
                graph::PackageGraph::build_with_channel(&consumer, &cfg, channel).expect("resolve");
            graph
                .packages()
                .iter()
                .map(|p| p.manifest_name.clone())
                .collect()
        };
        let dev_names = names_on("dev");
        assert!(
            dev_names.contains(&"leaf".to_string()),
            "dev keeps the scoped leaf"
        );

        let steam_names = names_on("steam");
        assert!(
            !steam_names.contains(&"leaf".to_string()),
            "a dev-only package reached through a library stays out of a release build"
        );
        assert!(
            steam_names.contains(&"mid".to_string()),
            "the library itself still resolves"
        );
    }

    /// A cycle through a transitive edge must be reported, not looped on.
    #[test]
    fn a_dependency_cycle_is_reported() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let consumer = workspace.path().join("consumer");
        let a = workspace.path().join("a");
        let b = workspace.path().join("b");
        for dir in [&consumer, &a, &b] {
            fs_err::create_dir_all(dir).unwrap();
        }
        fs_err::write(
            consumer.join("stick.toml"),
            "[package]\nname = \"consumer\"\n[dependencies]\na = \"../a\"\n",
        )
        .unwrap();
        fs_err::write(
            a.join("stick.toml"),
            "[package]\nname = \"a\"\ntype = \"lib\"\n[dependencies]\nb = \"../b\"\n",
        )
        .unwrap();
        fs_err::write(
            b.join("stick.toml"),
            "[package]\nname = \"b\"\ntype = \"lib\"\n[dependencies]\na = \"../a\"\n",
        )
        .unwrap();

        let cfg = StickConfig::from_path(&consumer.join("stick.toml")).unwrap();
        let err = graph::PackageGraph::build_with_channel(&consumer, &cfg, "dev")
            .expect_err("a cycle is an error, not a hang");
        assert!(
            err.to_string().contains("circular") && err.to_string().contains("a -> b -> a"),
            "the error names the cycle, got: {err}"
        );
    }

    #[test]
    fn package_version_defaults_when_missing() {
        // Minimal stick.toml without a version field used to crash
        // rock-lsp ~100ms after initialize. Now parses cleanly with
        // a default version. Pinned by editors/nvim/tests/
        // rock_lsp_minimal_project_sentinel_spec.lua — flip when
        // committing this fix.
        let cfg = StickConfig::new_from_str("[package]\nname = \"x\"\n")
            .expect("minimal stick.toml must parse without explicit version");
        assert_eq!(cfg.package.name, "x");
        assert_eq!(cfg.package.version(), "0.0.0");
    }

    #[test]
    fn package_version_preserved_when_set() {
        let cfg =
            StickConfig::new_from_str("[package]\nname = \"x\"\nversion = \"1.2.3\"\n").unwrap();
        assert_eq!(cfg.package.version(), "1.2.3");
    }

    #[test]
    fn native_declarations_are_strict_and_available_to_library_packages() {
        let manifest = "[package]\nname = \"bindings\"\ntype = \"lib\"\n[native]\nmanifest = \"../native/Cargo.toml\"\nlibrary = \"example_native\"\n";
        let config = StickConfig::new_from_str(manifest).unwrap();
        let native = config.native.unwrap();
        assert_eq!(native.manifest, "../native/Cargo.toml");
        assert_eq!(native.library, "example_native");
        assert!(native.shipped);
        assert!(
            !StickConfig::new_from_str(&format!("{manifest}shipped = false\n"))
                .unwrap()
                .native
                .unwrap()
                .shipped
        );
        for invalid in [
            manifest.replace("library =", "libary ="),
            manifest.replace("manifest = \"../native/Cargo.toml\"\n", ""),
            manifest.replace("library = \"example_native\"\n", ""),
        ] {
            assert!(StickConfig::new_from_str(&invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn top_package_and_engine_reject_unknown_keys() {
        for (toml, key, table) in [
            (
                "[package]\nname = \"x\"\n[dependecies]\n",
                "dependecies",
                "top level",
            ),
            (
                "[package]\nname = \"x\"\nverison = \"1\"\n",
                "verison",
                "[package]",
            ),
            (
                "[package]\nname = \"x\"\n[engine]\nneed = []\n",
                "need",
                "[engine]",
            ),
        ] {
            let msg = format!("{}", StickConfig::new_from_str(toml).expect_err(toml));
            assert!(
                msg.contains(&format!("`{key}`")) && msg.contains(table),
                "{msg}"
            );
        }
    }

    #[test]
    fn retired_ios_section_rejects() {
        let err = StickConfig::new_from_str(
            "[package]\nname = \"x\"\n[ios]\norientation = \"portrait\"\n",
        )
        .expect_err("retired [ios] section must be rejected");
        let msg = format!("{}", err);
        assert!(
            msg.contains("unknown key `ios`"),
            "error names the bad key: {msg}"
        );
    }

    // RED-then-GREEN: from_path_optional must distinguish "no manifest"
    // (Ok(None)) from "manifest exists but is malformed" (Err). The old
    // Optional loading must distinguish absence from malformed manifests.
    // (silent-errors audit, 2026-05-02).

    #[test]
    fn from_path_optional_returns_none_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist.toml");
        let result =
            StickConfig::from_path_optional(&missing).expect("missing file must not be an error");
        assert!(result.is_none());
    }

    #[test]
    fn from_path_optional_returns_some_when_valid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stick.toml");
        std::fs::write(&path, "[package]\nname = \"x\"\n").unwrap();
        let result = StickConfig::from_path_optional(&path)
            .expect("valid manifest must parse")
            .expect("must be Some when file exists and parses");
        assert_eq!(result.package.name, "x");
    }

    #[test]
    fn build_section_parses_channels_and_vars() {
        let cfg = StickConfig::new_from_str(concat!(
            "[package]\nname = \"x\"\n",
            "[build]\n",
            "channels = [\"dev\", \"steam\"]\n",
            "default_channel = \"dev\"\n",
            "[build.vars]\n",
            "GIT_SHA = { cmd = \"git rev-parse --short HEAD\" }\n",
            "CHANNEL = { builtin = \"channel\" }\n",
            "ENDPOINT = \"https://example.test/v1\"\n",
            "TOKEN = { cmd = \"./load.sh\", channels = [\"steam\"], default = \"\", secret = true }\n",
        )).expect("build section must parse");
        let build = cfg.build.as_ref().expect("build config present");
        assert_eq!(build.channels, vec!["dev", "steam"]);
        assert_eq!(build.default_channel.as_deref(), Some("dev"));
        let sha = &build.vars["GIT_SHA"];
        assert_eq!(sha.cmd.as_deref(), Some("git rev-parse --short HEAD"));
        assert!(!sha.secret);
        let chan = &build.vars["CHANNEL"];
        assert_eq!(chan.builtin.as_deref(), Some("channel"));
        let endpoint = &build.vars["ENDPOINT"];
        assert_eq!(endpoint.value.as_deref(), Some("https://example.test/v1"));
        let token = &build.vars["TOKEN"];
        assert_eq!(token.channels.as_deref(), Some(&["steam".to_string()][..]));
        assert_eq!(token.default.as_deref(), Some(""));
        assert!(token.secret);
        cfg.validate_build()
            .expect("well-formed build section validates");
    }

    #[test]
    fn build_section_rejects_unknown_key() {
        let err =
            StickConfig::new_from_str("[package]\nname = \"x\"\n[build]\nchanels = [\"dev\"]\n")
                .expect_err("unknown [build] key must be rejected");
        assert!(
            format!("{err}").contains("chanels"),
            "error names the bad key: {err}"
        );
    }

    #[test]
    fn sound_section_declares_a_project_path() {
        let cfg = StickConfig::new_from_str(
            "[package]\nname = \"x\"\n[sound]\nproject = \"sound_project.json\"\n",
        )
        .expect("sound section must parse");

        assert_eq!(
            cfg.sound.as_ref().map(|sound| sound.project.as_str()),
            Some("sound_project.json")
        );
    }

    #[test]
    fn sound_section_rejects_unknown_keys_and_missing_project() {
        let unknown = StickConfig::new_from_str(
            "[package]\nname = \"x\"\n[sound]\npath = \"sound_project.json\"\n",
        )
        .expect_err("unknown sound key must reject");
        assert!(format!("{unknown}").contains("path"));

        let missing = StickConfig::new_from_str("[package]\nname = \"x\"\n[sound]\n")
            .expect_err("sound project is required");
        assert!(format!("{missing}").contains("project"));
    }

    #[test]
    fn assets_section_rejects_unknown_keys() {
        let err = StickConfig::new_from_str(
            "[package]\nname = \"x\"\n[assets]\noverlay = [\"somewhere\"]\n",
        )
        .expect_err("misspelled overlay key must reject");
        assert!(format!("{err}").contains("known keys: overlays"));
    }

    #[test]
    fn build_var_rejects_unknown_key() {
        let err = StickConfig::new_from_str(
            "[package]\nname = \"x\"\n[build.vars]\nX = { comand = \"echo hi\" }\n",
        )
        .expect_err("unknown var key must be rejected");
        assert!(
            format!("{err}").contains("comand"),
            "error names the bad key: {err}"
        );
    }

    #[test]
    fn build_var_parses_source_fingerprint_and_rejects_unknown_nested_keys() {
        let cfg = StickConfig::new_from_str(concat!(
            "[package]\nname = \"x\"\n",
            "[build.vars]\n",
            "REV = \"abc\"\n",
            "TREE = { source_fingerprint = { roots = [\"src\"], files = [\"lock.json\"], vars = [\"REV\"] } }\n",
        )).expect("source fingerprint declaration must parse");
        let fingerprint = cfg.build.as_ref().unwrap().vars["TREE"]
            .source_fingerprint
            .as_ref()
            .expect("source fingerprint is retained");
        assert_eq!(fingerprint.roots, vec!["src"]);
        assert_eq!(fingerprint.files, vec!["lock.json"]);
        assert_eq!(fingerprint.vars, vec!["REV"]);
        cfg.validate_build()
            .expect("source fingerprint declaration validates");

        let defaults = StickConfig::new_from_str(concat!(
            "[package]\nname = \"x\"\n",
            "[build.vars]\n",
            "TREE = { source_fingerprint = {} }\n",
        ))
        .expect("omitted source fingerprint fields default to empty lists");
        let fingerprint = defaults.build.as_ref().unwrap().vars["TREE"]
            .source_fingerprint
            .as_ref()
            .expect("source fingerprint is retained");
        assert!(fingerprint.roots.is_empty());
        assert!(fingerprint.files.is_empty());
        assert!(fingerprint.vars.is_empty());

        let err = StickConfig::new_from_str(concat!(
            "[package]\nname = \"x\"\n",
            "[build.vars]\n",
            "TREE = { source_fingerprint = { roots = [\"src\"], unknown = [\"x\"] } }\n",
        ))
        .expect_err("unknown source fingerprint key must reject");
        assert!(
            format!("{err}").contains("unknown"),
            "error names the nested key: {err}"
        );
    }

    #[test]
    fn validate_build_counts_source_fingerprint_as_a_source() {
        let cfg = StickConfig::new_from_str(concat!(
            "[package]\nname = \"x\"\n",
            "[build.vars]\n",
            "TREE = { source_fingerprint = { roots = [\"src\"] }, value = \"literal\" }\n",
        ))
        .expect("both source declarations parse");
        let err = cfg.validate_build().expect_err("two sources must reject");
        assert!(
            format!("{err}").contains("TREE"),
            "error names the var: {err}"
        );
    }

    #[test]
    fn validate_build_requires_nonempty_source_fingerprint_roots_and_entries() {
        for source in [
            "roots = []",
            "roots = [\"\"]",
            "roots = [\"src\"], files = [\"\"]",
            "roots = [\"src\"], vars = [\"\"]",
        ] {
            let manifest = format!(
                "[package]\nname = \"x\"\n[build.vars]\nTREE = {{ source_fingerprint = {{ {source} }} }}\n"
            );
            let cfg = StickConfig::new_from_str(&manifest).expect("declaration parses");
            let err = cfg
                .validate_build()
                .expect_err("empty source fingerprint entries must reject");
            assert!(
                format!("{err}").contains("TREE"),
                "error names the var: {err}"
            );
        }
    }

    #[test]
    fn validate_build_rejects_invalid_source_fingerprint_dependencies_and_metadata() {
        let missing = StickConfig::new_from_str(concat!(
            "[package]\nname = \"x\"\n",
            "[build.vars]\n",
            "TREE = { source_fingerprint = { roots = [\"src\"], vars = [\"MISSING\"] } }\n",
        ))
        .unwrap();
        let err = missing
            .validate_build()
            .expect_err("undeclared fingerprint var must reject");
        assert!(
            format!("{err}").contains("MISSING"),
            "error names the missing var: {err}"
        );

        let self_ref = StickConfig::new_from_str(concat!(
            "[package]\nname = \"x\"\n",
            "[build.vars]\n",
            "TREE = { source_fingerprint = { roots = [\"src\"], vars = [\"TREE\"] } }\n",
        ))
        .unwrap();
        self_ref
            .validate_build()
            .expect_err("self-reference must reject");

        let deferred_ref = StickConfig::new_from_str(concat!(
            "[package]\nname = \"x\"\n",
            "[build.vars]\n",
            "TREE = { source_fingerprint = { roots = [\"src\"], vars = [\"OTHER\"] } }\n",
            "OTHER = { source_fingerprint = { roots = [\"other\"] } }\n",
        ))
        .unwrap();
        deferred_ref
            .validate_build()
            .expect_err("deferred dependency must reject");

        let volatile = StickConfig::new_from_str(concat!(
            "[package]\nname = \"x\"\n",
            "[build.vars]\n",
            "TREE = { source_fingerprint = { roots = [\"src\"] }, volatile = true }\n",
        ))
        .unwrap();
        volatile
            .validate_build()
            .expect_err("source fingerprint cannot be volatile");
    }

    #[test]
    fn optional_channels_remains_cmd_only_with_source_fingerprint() {
        let cfg = StickConfig::new_from_str(concat!(
            "[package]\nname = \"x\"\n",
            "[build]\nchannels = [\"dev\"]\n",
            "[build.vars]\n",
            "TREE = { source_fingerprint = { roots = [\"src\"] }, optional_channels = [\"dev\"], default = \"\" }\n",
        )).unwrap();
        let err = cfg
            .validate_build()
            .expect_err("optional_channels remains cmd-only");
        assert!(
            format!("{err}").contains("cmd"),
            "error explains the cmd requirement: {err}"
        );
    }

    #[test]
    fn build_var_volatile_metadata_is_explicit_and_typed() {
        let cfg = StickConfig::new_from_str(
            "[package]\nname = \"x\"\n[build.vars]\nCLOCK = { value = \"now\", volatile = true }\n",
        )
        .expect("volatile declaration must parse");
        assert!(cfg.build.as_ref().unwrap().vars["CLOCK"].volatile);
        cfg.validate_build()
            .expect("volatile declaration must validate");

        let err = StickConfig::new_from_str(
            "[package]\nname = \"x\"\n[build.vars]\nCLOCK = { value = \"now\", volatile = \"yes\" }\n",
        ).expect_err("malformed volatile declaration must reject");
        assert!(
            format!("{err}").contains("boolean"),
            "error explains the volatile type: {err}"
        );
    }

    #[test]
    fn build_var_unknown_metadata_cannot_disappear() {
        let err = StickConfig::new_from_str(
            "[package]\nname = \"x\"\n[build.vars]\nCLOCK = { value = \"now\", volatlie = true }\n",
        )
        .expect_err("misspelled volatile declaration must reject");
        assert!(
            format!("{err}").contains("volatlie"),
            "error names the bad key: {err}"
        );
    }

    #[test]
    fn validate_build_rejects_multiple_sources() {
        let cfg = StickConfig::new_from_str(
            "[package]\nname = \"x\"\n[build.vars]\nX = { cmd = \"echo hi\", builtin = \"channel\" }\n",
        ).expect("parses; source arity is a validate error");
        let err = cfg.validate_build().expect_err("two sources must reject");
        assert!(format!("{err}").contains("X"), "error names the var: {err}");
    }

    #[test]
    fn validate_build_rejects_sourceless_var() {
        let cfg = StickConfig::new_from_str(
            "[package]\nname = \"x\"\n[build.vars]\nX = { secret = true }\n",
        )
        .unwrap();
        cfg.validate_build()
            .expect_err("a var with no source must reject");
    }

    #[test]
    fn validate_build_rejects_unknown_builtin() {
        let cfg = StickConfig::new_from_str(
            "[package]\nname = \"x\"\n[build.vars]\nX = { builtin = \"hostname\" }\n",
        )
        .unwrap();
        let err = cfg
            .validate_build()
            .expect_err("unknown builtin must reject");
        assert!(
            format!("{err}").contains("hostname"),
            "error names the builtin: {err}"
        );
    }

    #[test]
    fn validate_build_rejects_undeclared_var_channel() {
        let cfg = StickConfig::new_from_str(concat!(
            "[package]\nname = \"x\"\n",
            "[build]\nchannels = [\"dev\"]\n",
            "[build.vars]\nX = { cmd = \"echo hi\", channels = [\"steam\"] }\n",
        ))
        .unwrap();
        let err = cfg
            .validate_build()
            .expect_err("var channel outside [build] channels must reject");
        assert!(
            format!("{err}").contains("steam"),
            "error names the channel: {err}"
        );
    }

    #[test]
    fn validate_build_rejects_default_channel_outside_channels() {
        let cfg = StickConfig::new_from_str(
            "[package]\nname = \"x\"\n[build]\nchannels = [\"dev\"]\ndefault_channel = \"steam\"\n",
        )
        .unwrap();
        cfg.validate_build()
            .expect_err("default_channel must be a declared channel");
    }

    #[test]
    fn from_path_optional_surfaces_parse_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stick.toml");
        std::fs::write(&path, "this is not valid toml = = =\n").unwrap();
        let err = StickConfig::from_path_optional(&path)
            .expect_err("malformed manifest must surface as Err, not Ok(None)");
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("parse stick.toml") || msg.contains("stick.toml"),
            "error mentions stick.toml: {msg}"
        );
    }
}
