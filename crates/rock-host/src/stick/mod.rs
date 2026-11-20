use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub mod build_vars;
pub mod graph;

/// The `[language]` manifest table. Packages use modules v1; the `modules`
/// key accepts only the explicit value `"v1"`. Legacy manifests must migrate.
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
    pub globals: rock_core::source_code::GlobalPolicy,
    pub identity_storage: rock_core::source_code::IdentityStorage,
}

impl Default for LanguageConfig {
    fn default() -> Self {
        Self {
            globals: rock_core::source_code::GlobalPolicy::Any,
            identity_storage: rock_core::source_code::IdentityStorage::Enforced,
        }
    }
}

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
                let mut globals = rock_core::source_code::GlobalPolicy::Any;
                let mut identity_storage = rock_core::source_code::IdentityStorage::Enforced;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "modules" => {
                            let value = map.next_value::<String>()?;
                            match value.as_str() {
                                "v1" => {}
                                other => {
                                    return Err(serde::de::Error::custom(format!(
                                        "unknown `[language] modules` value `{other}`; known values: \"v1\""
                                    )));
                                }
                            }
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
                    globals,
                    identity_storage,
                })
            }
        }
        d.deserialize_map(Vis)
    }
}

fn unknown_key<E: serde::de::Error>(key: &str, scope: &str, keys: &[&str]) -> E {
    E::custom(format!(
        "unknown key `{key}` {scope}; known keys: {}",
        keys.join(", ")
    ))
}

struct ManifestKey {
    scope: &'static str,
    keys: &'static [&'static str],
}

impl<'de> serde::de::DeserializeSeed<'de> for ManifestKey {
    type Value = String;

    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<String, D::Error> {
        let key = <String as serde::Deserialize>::deserialize(d)?;
        if !self.keys.contains(&key.as_str()) {
            return Err(unknown_key(&key, self.scope, self.keys));
        }
        Ok(key)
    }
}

macro_rules! manifest_table {
    (
        $(#[$meta:meta])*
        pub struct $name:ident {
            $($(#[$attr:meta])* $field:ident: $ty:ty, $mode:ident $(as $key:literal)?;)*
        }
        $scope:literal $(, $spanned:ident)?
        $(, $expected:literal, |$literal:ident| $shorthand:expr)?
    ) => {
        $(#[$meta])*
        pub struct $name {
            $($(#[$attr])* pub $field: $ty,)*
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                struct Visitor;
                impl<'de> serde::de::Visitor<'de> for Visitor {
                    type Value = $name;

                    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                        f.write_str(manifest_table!(@expected $name $($expected)?))
                    }

                    $(fn visit_str<E: serde::de::Error>(self, $literal: &str) -> Result<$name, E> {
                        Ok($shorthand)
                    })?

                    fn visit_map<A: serde::de::MapAccess<'de>>(
                        self,
                        mut map: A,
                    ) -> Result<$name, A::Error> {
                        const KEYS: &[&str] = &[$(manifest_table!(@key $field $($key)?)),*];
                        $(let mut $field = None;)*
                        while let Some(key) = manifest_table!(@next map $scope KEYS $($spanned)?)? {
                            match key.as_str() {
                                $(manifest_table!(@key $field $($key)?) => {
                                    $field = Some(manifest_table!(@read map $mode));
                                })*
                                _ => return Err(unknown_key(&key, $scope, KEYS)),
                            }
                        }
                        Ok($name {$($field: manifest_table!(@finish $field $mode $($key)?),)*})
                    }
                }
                manifest_table!(@deserialize d Visitor $($expected)?)
            }
        }
    };
    (@key $field:ident) => { stringify!($field) };
    (@key $field:ident $key:literal) => { $key };
    (@expected $name:ident) => { concat!(stringify!($name), " struct") };
    (@expected $name:ident $expected:literal) => { $expected };
    (@next $map:ident $scope:literal $keys:ident) => { $map.next_key::<String>() };
    (@next $map:ident $scope:literal $keys:ident spanned) => {
        $map.next_key_seed(ManifestKey { scope: $scope, keys: $keys })
    };
    (@read $map:ident required) => { $map.next_value()? };
    (@read $map:ident default) => { $map.next_value()? };
    (@read $map:ident present) => { Some($map.next_value()?) };
    (@finish $field:ident required $($key:literal)?) => {
        $field.ok_or_else(|| serde::de::Error::missing_field(manifest_table!(@key $field $($key)?)))?
    };
    (@finish $field:ident $mode:ident $($key:literal)?) => { $field.unwrap_or_default() };
    (@deserialize $d:ident $visitor:ident) => { $d.deserialize_map($visitor) };
    (@deserialize $d:ident $visitor:ident $expected:literal) => { $d.deserialize_any($visitor) };
}

manifest_table! {
    #[derive(Debug, Clone)]
    pub struct StickConfig {
        package: Package, required;
        build: Option<BuildConfig>, present;
        release: Option<ReleaseConfig>, present;
        assets: Option<AssetsConfig>, present;
        sound: Option<SoundConfig>, present;
        sim: Option<SimConfig>, default;
        lint: Option<LintConfig>, present;
        native: Option<NativeConfig>, present;
        language: Option<LanguageConfig>, present;
        dependencies: HashMap<String, Dependency>, default;
        source_groups: HashMap<String, SourceGroupConfig>, default;
    }
    "at top level", spanned
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionLayerPolicy {
    pub production_root: String,
    pub production_label: String,
    pub action_prefix: String,
    pub raw_input_names: Vec<String>,
    pub raw_input_classes: Vec<String>,
    pub registration_functions: Vec<String>,
}

impl ActionLayerPolicy {
    pub fn load(path: &Path) -> Result<Option<Self>> {
        // Bare Wasm uses editor buffers and embedded stdlib, with no disk manifests.
        if cfg!(all(target_family = "wasm", target_os = "unknown")) {
            return Ok(None);
        }
        // An unsaved editor file can have an empty parent, meaning the current directory.
        let path = if path.as_os_str().is_empty() {
            Path::new(".")
        } else {
            path
        };
        // A host without a current directory has nothing above the path it was given.
        let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        let dir = if path.is_file() {
            path.parent().unwrap_or(Path::new("."))
        } else {
            &path
        };
        Ok(StickConfig::find_from_dir(dir)?
            .and_then(|(_, config)| config.lint)
            .and_then(|lint| lint.action_layer))
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LintConfig {
    pub action_layer: Option<ActionLayerPolicy>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SimConfig {
    #[serde(default, deserialize_with = "sim_environment_prefixes")]
    pub environment_prefixes: Vec<String>,
}

fn sim_environment_prefixes<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Vec<String>, D::Error> {
    let prefixes = <Vec<String> as serde::Deserialize>::deserialize(d)?;
    if prefixes.iter().any(|p| {
        p.len() < 2
            || !p.ends_with('_')
            || !p
                .bytes()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_')
    }) {
        return Err(serde::de::Error::custom(
            "sim environment prefixes must be uppercase names ending in _",
        ));
    }
    Ok(prefixes)
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceGroupConfig {
    pub directory: String,
    pub channels: Vec<String>,
}

impl StickConfig {
    /// Missing `[language]` or `globals` keeps the permissive default.
    pub fn global_policy(&self) -> rock_core::source_code::GlobalPolicy {
        self.language
            .as_ref()
            .map(|l| l.globals)
            .unwrap_or(rock_core::source_code::GlobalPolicy::Any)
    }

    /// Missing `[language]` or `identity_storage` means enforced.
    pub fn identity_storage(&self) -> rock_core::source_code::IdentityStorage {
        self.language
            .as_ref()
            .map(|l| l.identity_storage.clone())
            .unwrap_or_default()
    }
}

manifest_table! {
    #[derive(Debug, Clone, Default)]
    pub struct BuildConfig {
        channels: Vec<String>, default;
        default_channel: Option<String>, present;
        vars: HashMap<String, BuildVar>, default;
    }
    "in [build]"
}

manifest_table! {
    /// The shorthand `NAME = "literal"` supplies `value`.
    #[derive(Debug, Clone, Default)]
    pub struct BuildVar {
        cmd: Option<String>, present;
        builtin: Option<String>, present;
        value: Option<String>, present;
        source_fingerprint: Option<SourceFingerprint>, present;
        channels: Option<Vec<String>>, present;
        default: Option<String>, present;
        /// Hash the value into cache keys but never store it in plaintext in any
        /// on-disk artifact or manifest.
        secret: bool, default;
        volatile: bool, default;
    }
    "in [build.vars] entry",
    "a literal string or a table with cmd/builtin/value/source_fingerprint/channels/default/secret/volatile",
    |literal| BuildVar { value: Some(literal.to_string()), ..BuildVar::default() }
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SourceFingerprint {
    pub roots: Vec<String>,
    pub files: Vec<String>,
    pub vars: Vec<String>,
}

manifest_table! {
    #[derive(Debug, Clone, Default)]
    pub struct ReleaseConfig {
        assets: Option<Vec<String>>, present;
        overlay_assets: Option<Vec<String>>, present;
        visual_banks: Option<Vec<String>>, present;
    }
    "in [release]"
}

manifest_table! {
    #[derive(Debug, Clone, Default)]
    pub struct AssetsConfig {
        overlays: Vec<String>, default;
    }
    "in [assets]"
}

manifest_table! {
    #[derive(Debug, Clone)]
    pub struct SoundConfig {
        project: String, required;
    }
    "in [sound]"
}

pub(crate) const KNOWN_BUILD_BUILTINS: [&str; 3] = ["channel", "platform", "source_revision"];

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

/// The CLI mirrors its comptime channel selection here; rock-host cannot read it.
static INVOCATION_CHANNEL: Mutex<Option<String>> = Mutex::new(None);

pub fn set_invocation_channel(channel: Option<String>) {
    *INVOCATION_CHANNEL.lock().expect("invocation channel lock") = channel;
}

/// Precedence: explicit `--channel`, then `default_channel`, then "dev". Mirrors
/// `resolve_build_vars` so a dependency filter and a build var agree on the channel.
pub fn resolved_channel_for(cfg: &StickConfig) -> String {
    if let Some(channel) = INVOCATION_CHANNEL
        .lock()
        .expect("invocation channel lock")
        .clone()
    {
        return channel;
    }
    cfg.build
        .as_ref()
        .and_then(|b| b.default_channel.clone())
        .unwrap_or_else(|| "dev".to_string())
}

/// A dependency with no declared channels matches everywhere.
pub(crate) fn dependency_matches_channel(dep: &Dependency, channel: &str) -> bool {
    match dep {
        Dependency::Path(_) => true,
        Dependency::Detailed(detailed) => {
            detailed.channels.is_empty() || detailed.channels.iter().any(|c| c == channel)
        }
    }
}

manifest_table! {
    #[derive(Debug, Clone)]
    pub struct Package {
        name: String, required;
        /// `Option` so a minimal `[package]` manifest parses; `version()` defaults to "0.0.0".
        version: Option<String>, default;
        package_type: Option<PackageType>, default as "type";
    }
    "in [package]"
}

impl Package {
    pub fn version(&self) -> &str {
        self.version.as_deref().unwrap_or("0.0.0")
    }
    pub(crate) fn package_type(&self) -> PackageType {
        self.package_type.unwrap_or(PackageType::Bin)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PackageType {
    Bin,
    Lib,
}

#[derive(Debug, Clone)]
#[repr(u8)]
pub enum Dependency {
    Path(String),
    Detailed(DetailedDependency),
}

#[derive(Debug, Clone)]
pub struct DetailedDependency {
    pub source: DependencySource,
    /// Empty means every channel; a shipping channel excluding a dev-only package keeps
    /// its symbols out of generated code.
    pub channels: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependencySource {
    Path(String),
    Package(String),
}

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
                        other => {
                            return Err(serde::de::Error::custom(format!(
                                "unknown key `{other}` in dependency; known keys: path, package, channels"
                            )));
                        }
                    }
                }
                let source = source.ok_or_else(|| {
                    serde::de::Error::custom("dependency requires exactly one path or package key")
                })?;
                Ok(DetailedDependency { source, channels })
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

impl StickConfig {
    pub fn from_path(path: &Path) -> Result<Self> {
        let content = fs_err::read_to_string(path)
            .with_context(|| format!("Failed to read stick.toml at {}", path.display()))?;
        Self::new_from_str(&content)
    }

    /// `Ok(None)` for no manifest, `Err` for a malformed one, so parse errors are not
    /// taken for "no config".
    pub fn from_path_optional(path: &Path) -> Result<Option<Self>> {
        if !path
            .try_exists()
            .with_context(|| format!("Failed to inspect stick.toml at {}", path.display()))?
        {
            return Ok(None);
        }
        Self::from_path(path).map(Some)
    }

    pub fn new_from_str(content: &str) -> Result<Self> {
        toml::from_str(content).map_err(|e| anyhow::anyhow!("Failed to parse stick.toml: {}", e))
    }

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
        }
        Ok(())
    }

    /// False for a `lib` and when no `stick.toml` exists: a scratch file is not a closed
    /// world for closed-world lints.
    pub fn is_bin_at(dir: &Path) -> bool {
        matches!(
            Self::find_from_dir(dir),
            Ok(Some((_, cfg))) if cfg.package.package_type() == PackageType::Bin
        )
    }

    pub fn find_from_dir(start_dir: &Path) -> Result<Option<(PathBuf, Self)>> {
        let mut current = start_dir;

        loop {
            let stick_path = current.join("stick.toml");
            if let Some(config) = Self::from_path_optional(&stick_path)? {
                return Ok(Some((stick_path, config)));
            }

            match current.parent() {
                Some(parent) => current = parent,
                None => return Ok(None),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(body: &str) -> Result<StickConfig> {
        StickConfig::new_from_str(&format!("[package]\nname = \"x\"\n{body}"))
    }

    #[test]
    fn unknown_top_level_key_reports_its_own_location() {
        for (prefix, line, column) in [
            ("unknown_probe = true\n\n", 1, 1),
            (
                "# comment\r\n\r\n  \"unknown_\\u0070robe\" = true\r\n\r\n",
                3,
                3,
            ),
        ] {
            let content = format!("{prefix}[package]\nname = \"probe\"\n");
            let error = StickConfig::new_from_str(&content).unwrap_err().to_string();
            assert!(
                error.contains("unknown key `unknown_probe` at top level"),
                "{error}"
            );
            assert!(error.contains(&format!("line {line}")), "{error}");
            assert!(error.contains(&format!("column {column}")), "{error}");
        }
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
    fn manifest_rejections_name_the_offending_key() {
        for (body, wants) in [
            ("[dependecies]\n", ["`dependecies`", "top level"]),
            (
                "[ios]\norientation = \"portrait\"\n",
                ["unknown key `ios`", "top level"],
            ),
            ("verison = \"1\"\n", ["`verison`", "[package]"]),
            (
                "[engine]\nneeds = []\n",
                ["unknown key `engine`", "top level"],
            ),
            (
                "[release]\npredefined_levels = []\n",
                ["`predefined_levels`", "[release]"],
            ),
            (
                "[build.vars]\nX = { cmd = \"true\", optional_channels = [\"dev\"] }\n",
                ["`optional_channels`", "[build.vars] entry"],
            ),
            ("[build]\nchanels = [\"dev\"]\n", ["`chanels`", "[build]"]),
            (
                "[build.vars]\nX = { comand = \"echo hi\" }\n",
                ["`comand`", "[build.vars] entry"],
            ),
            (
                "[build.vars]\nCLOCK = { value = \"now\", volatlie = true }\n",
                ["`volatlie`", "[build.vars] entry"],
            ),
            (
                "[build.vars]\nTREE = { source_fingerprint = { roots = [\"src\"], unknown = [\"x\"] } }\n",
                ["`unknown`", "roots"],
            ),
            (
                "[build.vars]\nCLOCK = { value = \"now\", volatile = \"yes\" }\n",
                ["expected a boolean", "CLOCK"],
            ),
            ("[sound]\npath = \"x.json\"\n", ["`path`", "[sound]"]),
            ("[sound]\n", ["missing field", "project"]),
            (
                "[assets]\noverlay = [\"somewhere\"]\n",
                ["`overlay`", "known keys: overlays"],
            ),
            (
                "[release]\nasset = [\"assets\"]\n",
                ["`asset`", "[release]"],
            ),
            (
                "[language]\nmemory = \"game\"\n",
                ["unknown key `memory`", "in [language]"],
            ),
            (
                "[language]\nmodules = \"v2\"\n",
                ["unknown `[language] modules` value", "`v2`"],
            ),
            (
                "[dependencies]\nlib = { path = \"../lib\", allow_legacy = true }\n",
                ["unknown key `allow_legacy`", "in dependency"],
            ),
        ] {
            let text = format!("{:#}", manifest(body).expect_err(body));
            assert!(
                wants.iter().all(|want| text.contains(want)),
                "{body}: {text}"
            );
        }
    }

    #[test]
    fn the_language_identity_storage_key_declares_the_migration_state() {
        use rock_core::source_code::IdentityStorage;
        let state = |language: &str| manifest(language).map(|c| c.identity_storage());
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
    fn language_modules_value_is_validated() {
        for body in ["", "[language]\nmodules = \"v1\"\n"] {
            manifest(body).unwrap();
        }
    }

    #[test]
    fn dependencies_are_a_path_or_a_table_with_exactly_one_source() {
        let cfg = manifest(
            "[dependencies]\ncore = \"../core\"\ndev-only = { path = \"../dep\", channels = [\"dev\"] }\n",
        )
        .unwrap();
        assert!(matches!(cfg.dependencies["core"], Dependency::Path(_)));
        let Dependency::Detailed(dep) = &cfg.dependencies["dev-only"] else {
            panic!("inline table dependency must parse as detailed");
        };
        assert_eq!(dep.source, DependencySource::Path("../dep".into()));
        assert_eq!(dep.channels, vec!["dev".to_string()]);
        for source in [
            "",
            "path = \"../x\", package = \"x\"",
            "package = \"x\", package = \"y\"",
        ] {
            let body = format!("[dependencies]\nx = {{ {source} }}\n");
            assert!(manifest(&body).is_err(), "{body}");
        }
    }

    #[test]
    fn package_version_defaults_when_missing() {
        let cfg = manifest("").expect("minimal stick.toml must parse without explicit version");
        assert_eq!(cfg.package.name, "x");
        assert_eq!(cfg.package.version(), "0.0.0");
        assert_eq!(
            manifest("version = \"1.2.3\"\n").unwrap().package.version(),
            "1.2.3"
        );
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
    fn from_path_optional_distinguishes_absence_from_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stick.toml");
        assert!(StickConfig::from_path_optional(&path).unwrap().is_none());

        std::fs::write(&path, "[package]\nname = \"x\"\n").unwrap();
        let found = StickConfig::from_path_optional(&path).unwrap().unwrap();
        assert_eq!(found.package.name, "x");

        std::fs::write(&path, "this is not valid toml = = =\n").unwrap();
        let err = StickConfig::from_path_optional(&path)
            .expect_err("malformed manifest must surface as Err, not Ok(None)");
        assert!(format!("{err:#}").contains("stick.toml"), "{err:#}");

        #[cfg(unix)]
        {
            std::fs::remove_file(&path).unwrap();
            std::os::unix::fs::symlink("stick.toml", &path).unwrap();
            let error = StickConfig::from_path_optional(&path)
                .expect_err("manifest metadata failure must not mean absence");
            assert!(
                format!("{error:#}").contains(path.to_str().unwrap()),
                "{error:#}"
            );
            let error = StickConfig::find_from_dir(dir.path())
                .expect_err("manifest discovery must surface metadata failure");
            assert!(
                format!("{error:#}").contains(path.to_str().unwrap()),
                "{error:#}"
            );
        }
    }

    #[test]
    fn build_section_parses_channels_and_vars() {
        let cfg = manifest(concat!(
            "[build]\n",
            "channels = [\"dev\", \"steam\"]\n",
            "default_channel = \"dev\"\n",
            "[build.vars]\n",
            "GIT_SHA = { cmd = \"git rev-parse --short HEAD\" }\n",
            "CHANNEL = { builtin = \"channel\" }\n",
            "ENDPOINT = \"https://example.test/v1\"\n",
            "TOKEN = { cmd = \"./load.sh\", channels = [\"steam\"], default = \"\", secret = true }\n",
            "CLOCK = { value = \"now\", volatile = true }\n",
            "REV = \"abc\"\n",
            "TREE = { source_fingerprint = { roots = [\"src\"], files = [\"lock.json\"], vars = [\"REV\"] } }\n",
        ))
        .expect("build section must parse");
        let build = cfg.build.as_ref().expect("build config present");
        assert_eq!(build.channels, vec!["dev", "steam"]);
        assert_eq!(build.default_channel.as_deref(), Some("dev"));
        let sha = &build.vars["GIT_SHA"];
        assert_eq!(sha.cmd.as_deref(), Some("git rev-parse --short HEAD"));
        assert!(!sha.secret && !sha.volatile);
        assert_eq!(build.vars["CHANNEL"].builtin.as_deref(), Some("channel"));
        assert_eq!(
            build.vars["ENDPOINT"].value.as_deref(),
            Some("https://example.test/v1")
        );
        let token = &build.vars["TOKEN"];
        assert_eq!(token.channels.as_deref(), Some(&["steam".to_string()][..]));
        assert_eq!(token.default.as_deref(), Some(""));
        assert!(token.secret);
        assert!(build.vars["CLOCK"].volatile);
        let fingerprint = build.vars["TREE"].source_fingerprint.as_ref().unwrap();
        assert_eq!(fingerprint.roots, vec!["src"]);
        assert_eq!(fingerprint.files, vec!["lock.json"]);
        assert_eq!(fingerprint.vars, vec!["REV"]);
        cfg.validate_build()
            .expect("well-formed build section validates");
    }

    #[test]
    fn source_fingerprint_fields_default_to_empty_lists() {
        let cfg = manifest("[build.vars]\nTREE = { source_fingerprint = {} }\n").unwrap();
        let empty = cfg.build.unwrap().vars["TREE"]
            .source_fingerprint
            .clone()
            .unwrap();
        assert!(empty.roots.is_empty() && empty.files.is_empty() && empty.vars.is_empty());
    }

    #[test]
    fn validate_build_rejects_inconsistent_declarations() {
        let fingerprint = |fields: &str| {
            format!("[build.vars]\nTREE = {{ source_fingerprint = {{ {fields} }} }}\n")
        };
        for (body, want) in [
            (
                "[build]\nchannels = [\"dev\"]\ndefault_channel = \"steam\"\n".to_string(),
                "default_channel \"steam\"",
            ),
            (
                "[build.vars]\nX = { cmd = \"echo hi\", builtin = \"channel\" }\n".into(),
                "X has more than one source",
            ),
            (
                "[build.vars]\nTREE = { source_fingerprint = { roots = [\"src\"] }, value = \"literal\" }\n"
                    .into(),
                "TREE has more than one source",
            ),
            (
                "[build.vars]\nX = { secret = true }\n".into(),
                "X has no source",
            ),
            (
                "[build.vars]\nX = { builtin = \"hostname\" }\n".into(),
                "unknown builtin \"hostname\"",
            ),
            (
                "[build]\nchannels = [\"dev\"]\n[build.vars]\nX = { cmd = \"echo hi\", channels = [\"steam\"] }\n"
                    .into(),
                "restricts to channel \"steam\"",
            ),
            (fingerprint("roots = []"), "TREE source_fingerprint requires at least one root"),
            (fingerprint("roots = [\"\"]"), "roots must be nonempty"),
            (
                fingerprint("roots = [\"src\"], files = [\"\"]"),
                "files must be nonempty",
            ),
            (
                fingerprint("roots = [\"src\"], vars = [\"\"]"),
                "var names must be nonempty",
            ),
            (
                fingerprint("roots = [\"src\"], vars = [\"MISSING\"]"),
                "undeclared var \"MISSING\"",
            ),
            (
                fingerprint("roots = [\"src\"], vars = [\"TREE\"]"),
                "cannot depend on itself",
            ),
            (
                format!(
                    "{}OTHER = {{ source_fingerprint = {{ roots = [\"other\"] }} }}\n",
                    fingerprint("roots = [\"src\"], vars = [\"OTHER\"]")
                ),
                "deferred var \"OTHER\"",
            ),
            (
                "[build.vars]\nTREE = { source_fingerprint = { roots = [\"src\"] }, volatile = true }\n"
                    .into(),
                "cannot be volatile",
            ),
        ] {
            let cfg = manifest(&body).expect(&body);
            let err = cfg.validate_build().expect_err(&body);
            assert!(format!("{err}").contains(want), "{body}: {err}");
        }
    }

    #[test]
    fn sim_timing_environment_requires_explicit_nonempty_namespaces() {
        assert!(manifest("").unwrap().sim.is_none());
        let config = manifest("[sim]\nenvironment_prefixes = [\"APP_\"]\n").unwrap();
        assert_eq!(config.sim.unwrap().environment_prefixes, ["APP_"]);
        for prefix in ["", "_", "APP", "app_", "APP-_"] {
            assert!(
                manifest(&format!("[sim]\nenvironment_prefixes = [\"{prefix}\"]\n")).is_err(),
                "{prefix:?}"
            );
        }
    }
}
