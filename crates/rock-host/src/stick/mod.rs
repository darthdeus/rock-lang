use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

pub mod build_vars;
pub mod resolver;

#[derive(Debug, Clone)]
pub struct StickConfig {
    pub package: Package,
    /// Optional engine-side requirements for this game (cdylibs that the
    /// runner must stage into `tmp/` before luajit boots). Recognised
    /// entries: `"video"` gates the stone-video crate (ffmpeg-the-third +
    /// bindgen — slow, only anvil needs it). Default: empty
    /// list = stone-rs only.
    pub engine: Option<EngineConfig>,
    /// The renderer is a build-time choice, independent of optional engine
    /// capabilities such as wgpu compute.
    pub renderer: Option<RendererConfig>,
    /// Optional iOS bundle settings. Read by the deploy scripts
    /// (`tools/ios/*.sh`) when they write Info.plist -- orientation is a
    /// build-time property of the bundle, not something the running game can
    /// choose, so it cannot live in `GameConfig`. Default: landscape.
    pub ios: Option<IosConfig>,
    /// Declared compile-time inputs (`build_var(..)`) and release channels.
    /// Commands are run by the CLI once per session build, before compilation;
    /// the compiler only ever sees resolved values (docs/comptime-plan.md).
    pub build: Option<BuildConfig>,
    /// Release asset selection consumed by both `rock package` and `rock web`.
    pub release: Option<ReleaseConfig>,
    /// External asset roots used when ROCK_ASSET_OVERLAYS is unset.
    pub assets: Option<AssetsConfig>,
    /// Optional compiler-owned sound project declaration.
    pub sound: Option<SoundConfig>,
    pub dependencies: HashMap<String, Dependency>,
}

#[derive(Debug, Clone, Default)]
pub struct BuildConfig {
    pub channels: Vec<String>,
    pub default_channel: Option<String>,
    pub vars: HashMap<String, BuildVar>,
}

/// One declared build variable. Exactly one of `cmd`/`builtin`/`value` is its
/// source (`validate_build` enforces arity; the shorthand `NAME = "literal"`
/// parses as `value`). `channels` restricts where the source runs; outside the
/// list the var resolves to `default`, and a restricted var with no default is
/// a resolution error there.
#[derive(Debug, Clone, Default)]
pub struct BuildVar {
    pub cmd: Option<String>,
    pub builtin: Option<String>,
    pub value: Option<String>,
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

#[derive(Debug, Clone, Default)]
pub struct ReleaseConfig {
    pub assets: Option<Vec<String>>,
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

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub needs: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RendererConfig {
    pub backend: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RendererBackend {
    Gl,
    Wgpu,
}

static RENDERER_OVERRIDE: OnceLock<Mutex<Option<RendererBackend>>> = OnceLock::new();

fn renderer_override_slot() -> &'static Mutex<Option<RendererBackend>> {
    RENDERER_OVERRIDE.get_or_init(|| Mutex::new(None))
}

/// Process-local selection installed by a CLI invocation. It is read by the
/// same manifest resolver used by compiler, engine build, and generated game
/// preamble, so an override cannot affect only the runtime half.
pub struct RendererOverrideGuard {
    previous: Option<RendererBackend>,
}

impl Drop for RendererOverrideGuard {
    fn drop(&mut self) {
        *renderer_override_slot().lock().expect("renderer override lock") = self.previous;
    }
}

pub fn install_renderer_override(backend: Option<RendererBackend>) -> RendererOverrideGuard {
    let mut slot = renderer_override_slot().lock().expect("renderer override lock");
    let previous = *slot;
    *slot = backend;
    RendererOverrideGuard { previous }
}

#[derive(Debug, Clone)]
pub struct IosConfig {
    /// `landscape` | `portrait` | `all`. None = unset, deploy uses landscape.
    pub orientation: Option<String>,
}

impl<'de> serde::Deserialize<'de> for IosConfig {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Vis;
        impl<'de> serde::de::Visitor<'de> for Vis {
            type Value = IosConfig;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(f, "IosConfig struct")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<IosConfig, A::Error> {
                let mut orientation: Option<String> = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "orientation" => { orientation = Some(map.next_value()?); }
                        other => {
                            // Fail closed: a misspelled key here would silently
                            // ship the default orientation, and the only symptom
                            // is a phone that rotates when it shouldn't.
                            return Err(serde::de::Error::custom(format!(
                                "unknown key `{other}` in [ios]; known keys: orientation"
                            )));
                        }
                    }
                }
                Ok(IosConfig { orientation })
            }
        }
        d.deserialize_map(Vis)
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
// detection reads `[package] type` (or the `stone` dependency) -- never a
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
    pub path: String,
}

// ---------------------------------------------------------------------------
// serde::Deserialize impls — hand-rolled to avoid pulling serde_derive
// (and thus syn) into rock-semantic's compile-time critical path.
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
                write!(f, "a table with a path key")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<DetailedDependency, A::Error> {
                let mut path: Option<String> = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "path" => { path = Some(map.next_value()?); }
                        _ => { let _ = map.next_value::<serde::de::IgnoredAny>()?; }
                    }
                }
                let path = path.ok_or_else(|| serde::de::Error::missing_field("path"))?;
                Ok(DetailedDependency { path })
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
                write!(f, "a path string or table with a path key")
            }
            fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<Dependency, E> {
                Ok(Dependency::Path(s.to_string()))
            }
            fn visit_string<E: serde::de::Error>(self, s: String) -> Result<Dependency, E> {
                Ok(Dependency::Path(s))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Dependency, A::Error> {
                let mut path: Option<String> = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "path" => { path = Some(map.next_value()?); }
                        _ => { let _ = map.next_value::<serde::de::IgnoredAny>()?; }
                    }
                }
                let path = path.ok_or_else(|| serde::de::Error::missing_field("path"))?;
                Ok(Dependency::Detailed(DetailedDependency { path }))
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
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<EngineConfig, A::Error> {
                let mut needs: Vec<String> = Vec::new();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "needs" => { needs = map.next_value()?; }
                        _ => { let _ = map.next_value::<serde::de::IgnoredAny>()?; }
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
                write!(f, "a literal string or a table with cmd/builtin/value/channels/optional_channels/default/secret/volatile")
            }
            fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<BuildVar, E> {
                Ok(BuildVar { value: Some(s.to_string()), ..BuildVar::default() })
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<BuildVar, A::Error> {
                let mut var = BuildVar::default();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "cmd" => { var.cmd = Some(map.next_value()?); }
                        "builtin" => { var.builtin = Some(map.next_value()?); }
                        "value" => { var.value = Some(map.next_value()?); }
                        "channels" => { var.channels = Some(map.next_value()?); }
                        "optional_channels" => { var.optional_channels = Some(map.next_value()?); }
                        "default" => { var.default = Some(map.next_value()?); }
                        "secret" => { var.secret = map.next_value()?; }
                        "volatile" => { var.volatile = map.next_value()?; }
                        other => {
                            // Fail closed: a misspelled source key would leave the
                            // var sourceless and every build_var() reading it stale.
                            return Err(serde::de::Error::custom(format!(
                                "unknown key `{other}` in [build.vars] entry; known keys: \
                                 cmd, builtin, value, channels, optional_channels, default, secret, volatile"
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
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<BuildConfig, A::Error> {
                let mut build = BuildConfig::default();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "channels" => { build.channels = map.next_value()?; }
                        "default_channel" => { build.default_channel = Some(map.next_value()?); }
                        "vars" => { build.vars = map.next_value()?; }
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
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<ReleaseConfig, A::Error> {
                let mut release = ReleaseConfig::default();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "assets" => { release.assets = Some(map.next_value()?); }
                        "overlay_assets" => { release.overlay_assets = Some(map.next_value()?); }
                        other => {
                            return Err(serde::de::Error::custom(format!(
                                "unknown key `{other}` in [release]; known keys: assets, overlay_assets"
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
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<AssetsConfig, A::Error> {
                let mut assets = AssetsConfig::default();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "overlays" => { assets.overlays = map.next_value()?; }
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
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<SoundConfig, A::Error> {
                let mut project: Option<String> = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "project" => { project = Some(map.next_value()?); }
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

impl<'de> serde::Deserialize<'de> for RendererConfig {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Vis;
        impl<'de> serde::de::Visitor<'de> for Vis {
            type Value = RendererConfig;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(f, "RendererConfig struct")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<RendererConfig, A::Error> {
                let mut backend: Option<String> = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "backend" => { backend = Some(map.next_value()?); }
                        other => {
                            return Err(serde::de::Error::custom(format!(
                                "unknown key `{other}` in [renderer]; known keys: backend"
                            )));
                        }
                    }
                }
                Ok(RendererConfig { backend })
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
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Package, A::Error> {
                let mut name: Option<String> = None;
                let mut version: Option<String> = None;
                let mut package_type: Option<PackageType> = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "name" => { name = Some(map.next_value()?); }
                        "version" => { version = map.next_value()?; }
                        "type" => { package_type = map.next_value()?; }
                        _ => { let _ = map.next_value::<serde::de::IgnoredAny>()?; }
                    }
                }
                let name = name.ok_or_else(|| serde::de::Error::missing_field("name"))?;
                Ok(Package { name, version, package_type })
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
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<StickConfig, A::Error> {
                let mut package: Option<Package> = None;
                let mut engine: Option<EngineConfig> = None;
                let mut renderer: Option<RendererConfig> = None;
                let mut ios: Option<IosConfig> = None;
                let mut build: Option<BuildConfig> = None;
                let mut release: Option<ReleaseConfig> = None;
                let mut assets: Option<AssetsConfig> = None;
                let mut sound: Option<SoundConfig> = None;
                let mut dependencies: HashMap<String, Dependency> = HashMap::new();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "package" => { package = Some(map.next_value()?); }
                        "build" => { build = Some(map.next_value()?); }
                        "release" => { release = Some(map.next_value()?); }
                        "assets" => { assets = Some(map.next_value()?); }
                        "sound" => { sound = Some(map.next_value()?); }
                        "engine" => { engine = map.next_value()?; }
                        "renderer" => { renderer = map.next_value()?; }
                        "ios" => { ios = map.next_value()?; }
                        "dependencies" => { dependencies = map.next_value()?; }
                        _ => { let _ = map.next_value::<serde::de::IgnoredAny>()?; }
                    }
                }
                let package = package.ok_or_else(|| serde::de::Error::missing_field("package"))?;
                Ok(StickConfig { package, engine, renderer, ios, build, release, assets, sound, dependencies })
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
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<StickLockDependency, A::Error> {
                let mut name: Option<String> = None;
                let mut version: Option<String> = None;
                let mut source: Option<String> = None;
                let mut checksum: Option<String> = None;
                let mut dependencies: Vec<String> = Vec::new();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "name" => { name = Some(map.next_value()?); }
                        "version" => { version = Some(map.next_value()?); }
                        "source" => { source = Some(map.next_value()?); }
                        "checksum" => { checksum = map.next_value()?; }
                        "dependencies" => { dependencies = map.next_value()?; }
                        _ => { let _ = map.next_value::<serde::de::IgnoredAny>()?; }
                    }
                }
                let name = name.ok_or_else(|| serde::de::Error::missing_field("name"))?;
                let version = version.ok_or_else(|| serde::de::Error::missing_field("version"))?;
                let source = source.ok_or_else(|| serde::de::Error::missing_field("source"))?;
                Ok(StickLockDependency { name, version, source, checksum, dependencies })
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
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<StickLock, A::Error> {
                let mut version: Option<u32> = None;
                let mut compiler_sha1: Option<String> = None;
                let mut dependencies: Vec<StickLockDependency> = Vec::new();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "version" => { version = Some(map.next_value()?); }
                        "compiler_sha1" => { compiler_sha1 = Some(map.next_value()?); }
                        "dependencies" => { dependencies = map.next_value()?; }
                        _ => { let _ = map.next_value::<serde::de::IgnoredAny>()?; }
                    }
                }
                let version = version.ok_or_else(|| serde::de::Error::missing_field("version"))?;
                let compiler_sha1 = compiler_sha1.ok_or_else(|| serde::de::Error::missing_field("compiler_sha1"))?;
                Ok(StickLock { version, compiler_sha1, dependencies })
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
    /// `validate_engine_needs` and surface as a confusing later failure
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

    /// True if the game's `[engine] needs = [...]` list contains `lib`.
    /// Used by the stone runner to decide whether to build optional
    /// engine-side cdylibs (e.g. stone-video) before launching luajit.
    pub fn needs_engine_lib(&self, lib: &str) -> bool {
        self.engine.as_ref()
            .map(|e| e.needs.iter().any(|n| n == lib))
            .unwrap_or(false)
    }

    pub fn renderer_backend(&self) -> Result<RendererBackend> {
        if let Some(backend) = *renderer_override_slot().lock().expect("renderer override lock") {
            return Ok(backend);
        }
        match self.renderer.as_ref().and_then(|r| r.backend.as_deref()).unwrap_or("gl") {
            "gl" => Ok(RendererBackend::Gl),
            "wgpu" => Ok(RendererBackend::Wgpu),
            backend => anyhow::bail!(
                "stick.toml: unknown [renderer] backend {:?}. Known: \"gl\", \"wgpu\"",
                backend
            ),
        }
    }

    pub fn validate_renderer(&self) -> Result<()> {
        self.renderer_backend().map(|_| ())
    }

    /// Recognised values for `[engine] needs = [...]`, read off the build table
    /// rather than restated. A hand-maintained copy drifts silently, and in the
    /// direction that looks like the user's fault: a need the table can build
    /// but the validator rejects reads as a typo in their manifest.
    pub fn known_engine_needs() -> Vec<&'static str> {
        crate::engine_build::OPTIONAL_ENGINE_LIBS
            .iter()
            .map(|l| l.need)
            .collect()
    }

    /// Reject typos in `[engine] needs = [...]` (e.g. "vidoe") so the
    /// build doesn't silently skip an optional cdylib the user asked for.
    /// Recognised values for `[ios] orientation`. Keep in sync with the case
    /// arms in `tools/ios/run-game-device.sh`.
    pub const KNOWN_IOS_ORIENTATIONS: [&'static str; 3] = ["landscape", "portrait", "all"];

    /// The orientation the iOS bundle should pin, if the manifest sets one.
    /// The deploy scripts default to landscape when this is None.
    pub fn ios_orientation(&self) -> Option<&str> {
        self.ios.as_ref()?.orientation.as_deref()
    }

    /// Reject a bad `[ios] orientation` at build time. Without this the deploy
    /// script sees an unrecognised string; the failure mode is a phone that
    /// rotates freely, which thrashes canvas re-init hard enough to look like
    /// a hang (docs/ios-bringup.md).
    pub fn validate_ios(&self) -> Result<()> {
        let Some(orientation) = self.ios_orientation() else {
            return Ok(());
        };
        if !Self::KNOWN_IOS_ORIENTATIONS.contains(&orientation) {
            anyhow::bail!(
                "stick.toml: unknown [ios] orientation {:?}. Known: {:?}",
                orientation,
                Self::KNOWN_IOS_ORIENTATIONS
            );
        }
        Ok(())
    }

    pub fn validate_engine_needs(&self) -> Result<()> {
        let Some(engine) = self.engine.as_ref() else {
            return Ok(());
        };
        let known = Self::known_engine_needs();
        for entry in &engine.needs {
            if !known.contains(&entry.as_str()) {
                anyhow::bail!(
                    "stick.toml: unknown [engine] needs entry {:?}. Known: {:?}",
                    entry,
                    known
                );
            }
        }
        Ok(())
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
                    default, build.channels
                );
            }
        }
        for (name, var) in &build.vars {
            let sources = [var.cmd.is_some(), var.builtin.is_some(), var.value.is_some()];
            match sources.iter().filter(|s| **s).count() {
                1 => {}
                0 => anyhow::bail!(
                    "stick.toml: [build.vars] {name} has no source; set one of cmd, builtin, value"
                ),
                _ => anyhow::bail!(
                    "stick.toml: [build.vars] {name} has more than one source; \
                     set exactly one of cmd, builtin, value"
                ),
            }
            if let Some(builtin) = var.builtin.as_deref() {
                if !KNOWN_BUILD_BUILTINS.contains(&builtin) {
                    anyhow::bail!(
                        "stick.toml: [build.vars] {name} names unknown builtin {:?}. Known: {:?}",
                        builtin, KNOWN_BUILD_BUILTINS
                    );
                }
            }
            if let Some(var_channels) = var.channels.as_deref() {
                for chan in var_channels {
                    if !build.channels.iter().any(|c| c == chan) {
                        anyhow::bail!(
                            "stick.toml: [build.vars] {name} restricts to channel {:?}, \
                             which is not in [build] channels {:?}",
                            chan, build.channels
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

    /// Resolve the release asset list once from the already-parsed StickConfig.
    /// The default mirrors the historical release collector: game `assets/`,
    /// plus a root-level `level.txt` when present.
    pub fn release_assets(&self, project_dir: &Path) -> Result<Vec<String>> {
        let assets = self
            .release
            .as_ref()
            .and_then(|release| release.assets.clone())
            .unwrap_or_else(|| {
                let mut defaults = vec!["assets".to_string()];
                if project_dir.join("level.txt").exists() {
                    defaults.push("level.txt".to_string());
                }
                defaults
            });
        for rel in &assets {
            let path = Path::new(rel);
            if rel.is_empty()
                || path.is_absolute()
                || path
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                anyhow::bail!(
                    "stick.toml: [release].assets entry {:?} must be a non-empty relative path inside the project",
                    rel
                );
            }
        }
        Ok(assets)
    }

    pub fn release_overlay_assets(&self) -> Result<Vec<String>> {
        let assets = self
            .release
            .as_ref()
            .and_then(|release| release.overlay_assets.clone())
            .unwrap_or_default();
        for rel in &assets {
            let path = Path::new(rel);
            if !rel.starts_with("assets/")
                || rel.contains('\\')
                || path.is_absolute()
                || path.components().any(|c| matches!(c, std::path::Component::ParentDir))
            {
                anyhow::bail!(
                    "stick.toml: [release].overlay_assets entry {:?} must be a canonical assets-relative file",
                    rel
                );
            }
        }
        Ok(assets)
    }

    pub fn asset_overlay_defaults(&self, project_dir: &Path) -> Result<Vec<PathBuf>> {
        let Some(assets) = &self.assets else {
            return Ok(Vec::new());
        };
        let base = project_dir.canonicalize().unwrap_or_else(|_| {
            if project_dir.is_absolute() {
                project_dir.to_path_buf()
            } else {
                std::env::current_dir().unwrap_or_default().join(project_dir)
            }
        });
        assets
            .overlays
            .iter()
            .map(|raw| {
                if raw.is_empty() {
                    anyhow::bail!("stick.toml: [assets].overlays entries must not be empty");
                }
                let expanded = if raw == "~" || raw.starts_with("~/") || raw.starts_with("~\\") {
                    let home = std::env::var_os("HOME")
                        .or_else(|| std::env::var_os("USERPROFILE"))
                        .ok_or_else(|| anyhow::anyhow!(
                            "stick.toml: cannot expand [assets].overlays entry {raw:?}: no home directory environment"
                        ))?;
                    let tail = raw.trim_start_matches('~').trim_start_matches(['/', '\\']);
                    PathBuf::from(home).join(tail)
                } else if raw.starts_with('~') {
                    anyhow::bail!(
                        "stick.toml: [assets].overlays entry {raw:?} uses unsupported user-home syntax; use ~/path"
                    );
                } else {
                    PathBuf::from(raw)
                };
                Ok(if expanded.is_absolute() { expanded } else { base.join(expanded) })
            })
            .collect()
    }

    pub fn validate(&self) -> Result<()> {
        // Libs shouldn't ship an `[engine] needs` -- that drives the
        // game runner's cdylib build, which a library would never
        // legitimately request. Catches the wrong type= annotation
        // before it confuses the runner.
        if self.package.package_type() == PackageType::Lib && self.engine.is_some() {
            anyhow::bail!(
                "library packages cannot have an [engine] section -- that's a game-runner directive"
            );
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
        out.push_str(&format!("compiler_sha1 = {}\n", toml_str(&self.compiler_sha1)));
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
    let exe_path = std::env::current_exe()
        .context("Failed to get current executable path")?;

    // Read the executable file
    let mut file = fs::File::open(&exe_path)
        .context("Failed to open compiler executable")?;
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
        let cfg = StickConfig::new_from_str(
            "[package]\nname = \"x\"\nversion = \"1.2.3\"\n",
        ).unwrap();
        assert_eq!(cfg.package.version(), "1.2.3");
    }

    /// Every lib the build table can produce must be ACCEPTED by the validator,
    /// and a non-entry must still be rejected. Asserting the accept side alone
    /// passes on a validator that accepts everything.
    #[test]
    fn the_validator_admits_exactly_the_build_table() {
        let table = crate::engine_build::OPTIONAL_ENGINE_LIBS;
        assert!(!table.is_empty(), "no optional libs to check");
        for lib in table {
            let cfg = StickConfig::new_from_str(&format!(
                "[package]\nname = \"x\"\n[engine]\nneeds = [\"{}\"]\n",
                lib.need
            ))
            .unwrap();
            cfg.validate_engine_needs()
                .unwrap_or_else(|e| panic!("table lib {:?} rejected: {e}", lib.need));
            assert!(cfg.needs_engine_lib(lib.need));
        }
        let cfg = StickConfig::new_from_str(
            "[package]\nname = \"x\"\n[engine]\nneeds = [\"not-a-lib\"]\n",
        )
        .unwrap();
        cfg.validate_engine_needs()
            .expect_err("a need outside the table must be rejected");
    }

    #[test]
    fn validate_engine_needs_accepts_wgpu() {
        let cfg = StickConfig::new_from_str(
            "[package]\nname = \"x\"\n[engine]\nneeds = [\"wgpu\"]\n",
        ).unwrap();
        cfg.validate_engine_needs().expect("wgpu is a known need");
        assert!(cfg.needs_engine_lib("wgpu"));
        assert!(!cfg.needs_engine_lib("video"));
    }

    #[test]
    fn validate_engine_needs_accepts_known_entry() {
        let cfg = StickConfig::new_from_str(
            "[package]\nname = \"x\"\n[engine]\nneeds = [\"video\"]\n",
        ).unwrap();
        cfg.validate_engine_needs().expect("video is a known need");
    }

    #[test]
    fn validate_engine_needs_rejects_typo() {
        // Without this check a misspelled `[engine] needs` entry would
        // silently skip the optional cdylib build, and the game would
        // hit a less-obvious load failure at runtime.
        let cfg = StickConfig::new_from_str(
            "[package]\nname = \"x\"\n[engine]\nneeds = [\"vidoe\"]\n",
        ).unwrap();
        let err = cfg.validate_engine_needs().expect_err("typo must be rejected");
        let msg = format!("{}", err);
        assert!(msg.contains("vidoe"), "error mentions the bad entry: {msg}");
        assert!(msg.contains("video"), "error suggests the known list: {msg}");
    }

    #[test]
    fn ios_orientation_round_trips() {
        let cfg = StickConfig::new_from_str(
            "[package]\nname = \"x\"\n[ios]\norientation = \"portrait\"\n",
        ).unwrap();
        assert_eq!(cfg.ios_orientation(), Some("portrait"));
        cfg.validate_ios().expect("portrait is a known orientation");
    }

    #[test]
    fn ios_orientation_defaults_to_unset() {
        let cfg = StickConfig::new_from_str("[package]\nname = \"x\"\n").unwrap();
        assert_eq!(cfg.ios_orientation(), None);
        cfg.validate_ios().expect("no ios section is fine");
    }

    #[test]
    fn validate_ios_rejects_bad_orientation() {
        // The deploy script would otherwise write an Info.plist with no
        // recognised orientation and the phone rotates freely -- which thrashes
        // canvas re-init hard enough to look like a hang.
        let cfg = StickConfig::new_from_str(
            "[package]\nname = \"x\"\n[ios]\norientation = \"sideways\"\n",
        ).unwrap();
        let err = cfg.validate_ios().expect_err("bad orientation must be rejected");
        let msg = format!("{}", err);
        assert!(msg.contains("sideways"), "error mentions the bad value: {msg}");
        assert!(msg.contains("portrait"), "error suggests the known list: {msg}");
    }

    #[test]
    fn ios_section_rejects_unknown_key() {
        // A misspelled key would ship the default orientation silently.
        let err = StickConfig::new_from_str(
            "[package]\nname = \"x\"\n[ios]\norientaton = \"portrait\"\n",
        ).expect_err("unknown [ios] key must be rejected");
        let msg = format!("{}", err);
        assert!(msg.contains("orientaton"), "error names the bad key: {msg}");
    }

    #[test]
    fn validate_engine_needs_empty_is_ok() {
        let cfg = StickConfig::new_from_str("[package]\nname = \"x\"\n").unwrap();
        cfg.validate_engine_needs().expect("no engine section is fine");
    }

    #[test]
    fn renderer_defaults_to_gl_and_accepts_wgpu() {
        let gl = StickConfig::new_from_str("[package]\nname = \"x\"\n").unwrap();
        assert_eq!(gl.renderer_backend().unwrap(), RendererBackend::Gl);

        let wgpu = StickConfig::new_from_str(
            "[package]\nname = \"x\"\n[renderer]\nbackend = \"wgpu\"\n",
        ).unwrap();
        assert_eq!(wgpu.renderer_backend().unwrap(), RendererBackend::Wgpu);
        wgpu.validate_renderer().expect("wgpu is a known renderer");
    }

    #[test]
    fn renderer_rejects_unknown_backend() {
        let cfg = StickConfig::new_from_str(
            "[package]\nname = \"x\"\n[renderer]\nbackend = \"vulkan\"\n",
        ).unwrap();
        let err = cfg.validate_renderer().expect_err("unknown renderer must reject");
        assert!(format!("{err}").contains("vulkan"));
    }

    // RED-then-GREEN: from_path_optional must distinguish "no manifest"
    // (Ok(None)) from "manifest exists but is malformed" (Err). The old
    // call sites in rock-cli/stone.rs used .ok() which conflated the two,
    // silently skipping validate_engine_needs on malformed manifests
    // (silent-errors audit, 2026-05-02).

    #[test]
    fn from_path_optional_returns_none_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist.toml");
        let result = StickConfig::from_path_optional(&missing)
            .expect("missing file must not be an error");
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
        cfg.validate_build().expect("well-formed build section validates");
    }

    #[test]
    fn build_section_rejects_unknown_key() {
        let err = StickConfig::new_from_str(
            "[package]\nname = \"x\"\n[build]\nchanels = [\"dev\"]\n",
        ).expect_err("unknown [build] key must be rejected");
        assert!(format!("{err}").contains("chanels"), "error names the bad key: {err}");
    }

    #[test]
    fn release_section_parses_assets_through_stick_config() {
        let cfg = StickConfig::new_from_str(concat!(
            "[package]\nname = \"x\"\n",
            "[release]\nassets = [\"assets\", \"saves/level.json\"]\n",
            "overlay_assets = [\"assets/particles/blood.png\"]\n",
        ))
        .expect("release section must parse");
        assert_eq!(
            cfg.release.as_ref().unwrap().assets.as_deref(),
            Some(&["assets".to_string(), "saves/level.json".to_string()][..])
        );
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(cfg.release_assets(dir.path()).unwrap(), vec!["assets", "saves/level.json"]);
        assert_eq!(
            cfg.release_overlay_assets().unwrap(),
            vec!["assets/particles/blood.png"]
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

        let missing = StickConfig::new_from_str(
            "[package]\nname = \"x\"\n[sound]\n",
        )
        .expect_err("sound project is required");
        assert!(format!("{missing}").contains("project"));
    }

    #[test]
    fn release_section_rejects_unknown_keys_and_escape_paths() {
        let err = StickConfig::new_from_str(
            "[package]\nname = \"x\"\n[release]\nasset = [\"assets\"]\n",
        )
        .expect_err("unknown release key must reject");
        assert!(format!("{err}").contains("asset"), "error names the bad release key: {err}");

        let cfg = StickConfig::new_from_str(
            "[package]\nname = \"x\"\n[release]\nassets = [\"../outside\"]\n",
        )
        .expect("path validation happens when selecting assets");
        let dir = tempfile::tempdir().unwrap();
        let err = cfg.release_assets(dir.path()).expect_err("asset escape must reject");
        assert!(format!("{err}").contains("outside"), "error names the escape: {err}");

        let cfg = StickConfig::new_from_str(
            "[package]\nname = \"x\"\n[release]\noverlay_assets = [\"../outside.png\"]\n",
        )
        .expect("overlay path validation happens when selecting assets");
        let err = cfg.release_overlay_assets().expect_err("overlay asset escape must reject");
        assert!(format!("{err}").contains("outside.png"), "error names the escape: {err}");
    }

    #[test]
    fn asset_overlay_defaults_resolve_relative_and_home_paths() {
        let project = tempfile::tempdir().unwrap();
        let cfg = StickConfig::new_from_str(
            "[package]\nname = \"x\"\n[assets]\noverlays = [\"../shared\", \"~/rock-assets\"]\n",
        )
        .unwrap();
        let resolved = cfg.asset_overlay_defaults(project.path()).unwrap();
        assert_eq!(resolved[0], project.path().join("../shared"));
        let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).unwrap();
        assert_eq!(resolved[1], PathBuf::from(home).join("rock-assets"));
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
        ).expect_err("unknown var key must be rejected");
        assert!(format!("{err}").contains("comand"), "error names the bad key: {err}");
    }

    #[test]
    fn build_var_volatile_metadata_is_explicit_and_typed() {
        let cfg = StickConfig::new_from_str(
            "[package]\nname = \"x\"\n[build.vars]\nCLOCK = { value = \"now\", volatile = true }\n",
        ).expect("volatile declaration must parse");
        assert!(cfg.build.as_ref().unwrap().vars["CLOCK"].volatile);
        cfg.validate_build().expect("volatile declaration must validate");

        let err = StickConfig::new_from_str(
            "[package]\nname = \"x\"\n[build.vars]\nCLOCK = { value = \"now\", volatile = \"yes\" }\n",
        ).expect_err("malformed volatile declaration must reject");
        assert!(format!("{err}").contains("boolean"), "error explains the volatile type: {err}");
    }

    #[test]
    fn build_var_unknown_metadata_cannot_disappear() {
        let err = StickConfig::new_from_str(
            "[package]\nname = \"x\"\n[build.vars]\nCLOCK = { value = \"now\", volatlie = true }\n",
        ).expect_err("misspelled volatile declaration must reject");
        assert!(format!("{err}").contains("volatlie"), "error names the bad key: {err}");
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
        ).unwrap();
        cfg.validate_build().expect_err("a var with no source must reject");
    }

    #[test]
    fn validate_build_rejects_unknown_builtin() {
        let cfg = StickConfig::new_from_str(
            "[package]\nname = \"x\"\n[build.vars]\nX = { builtin = \"hostname\" }\n",
        ).unwrap();
        let err = cfg.validate_build().expect_err("unknown builtin must reject");
        assert!(format!("{err}").contains("hostname"), "error names the builtin: {err}");
    }

    #[test]
    fn validate_build_rejects_undeclared_var_channel() {
        let cfg = StickConfig::new_from_str(concat!(
            "[package]\nname = \"x\"\n",
            "[build]\nchannels = [\"dev\"]\n",
            "[build.vars]\nX = { cmd = \"echo hi\", channels = [\"steam\"] }\n",
        )).unwrap();
        let err = cfg.validate_build().expect_err("var channel outside [build] channels must reject");
        assert!(format!("{err}").contains("steam"), "error names the channel: {err}");
    }

    #[test]
    fn validate_build_rejects_default_channel_outside_channels() {
        let cfg = StickConfig::new_from_str(
            "[package]\nname = \"x\"\n[build]\nchannels = [\"dev\"]\ndefault_channel = \"steam\"\n",
        ).unwrap();
        cfg.validate_build().expect_err("default_channel must be a declared channel");
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
