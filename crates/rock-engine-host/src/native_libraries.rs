use anyhow::{Context, Result};
use rock_host::stick::{StickConfig, graph::PackageGraph};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Clone, Debug)]
pub struct NativeLibrary {
    pub need: String,
    pub manifest: PathBuf,
    pub dst_stem: String,
    pub shipped: bool,
    pub stage_dir: PathBuf,
}

pub fn project_native_dir(project: &Path) -> Result<PathBuf> {
    Ok(fs_err::canonicalize(project)?.join("tmp/native"))
}

pub fn game_engine_libs(project: &str) -> Result<Vec<NativeLibrary>> {
    let Some((manifest, _)) = StickConfig::find_from_dir(Path::new(project))? else {
        return Ok(Vec::new());
    };
    let root = manifest
        .parent()
        .context("project manifest has no parent")?;
    let graph = PackageGraph::build(root)?;
    let stage_dir = project_native_dir(root)?;
    let mut libs = Vec::new();
    let mut names = BTreeSet::new();
    for package in graph.packages() {
        let config = StickConfig::from_path(&package.root.join("stick.toml"))?;
        if config
            .engine
            .as_ref()
            .is_some_and(|engine| !engine.needs.is_empty())
        {
            anyhow::bail!(
                "{}: replace [engine].needs with [native] declarations on the packages that own the libraries",
                package.root.display()
            );
        }
        let Some(native) = config.native else {
            continue;
        };
        anyhow::ensure!(
            !native.library.is_empty()
                && native
                    .library
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
            "{}: [native].library must be a library stem, not a filename or path",
            package.root.display()
        );
        anyhow::ensure!(
            names.insert(native.library.clone()),
            "duplicate native library {:?} in project {}",
            native.library,
            root.display()
        );
        anyhow::ensure!(
            !native.manifest.is_empty(),
            "{}: empty [native].manifest",
            package.root.display()
        );
        let manifest =
            fs_err::canonicalize(package.root.join(&native.manifest)).with_context(|| {
                format!(
                    "{}: native Cargo manifest {:?} is missing",
                    package.root.display(),
                    native.manifest
                )
            })?;
        libs.push(NativeLibrary {
            need: package.manifest_name.clone(),
            manifest,
            dst_stem: native.library,
            shipped: native.shipped,
            stage_dir: stage_dir.clone(),
        });
    }
    Ok(libs)
}

struct CargoLibrary {
    package_id: String,
    package_name: String,
    target_name: String,
    watch_paths: Vec<PathBuf>,
    target_dir: PathBuf,
}

fn cargo_command(manifest: &Path) -> Command {
    let mut command = Command::new("cargo");
    command.current_dir(
        manifest
            .parent()
            .expect("canonical Cargo manifest has a parent"),
    );
    rock_host::cargo::strip_cargo_harness_env(&mut command);
    command
}

fn cargo_library(lib: &NativeLibrary) -> Result<CargoLibrary> {
    let output = cargo_command(&lib.manifest)
        .args(["metadata", "--format-version=1", "--manifest-path"])
        .arg(&lib.manifest)
        .stderr(Stdio::inherit())
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "Cargo metadata failed for {}",
        lib.manifest.display()
    );
    let metadata: Value = serde_json::from_slice(&output.stdout)?;
    let packages = metadata["packages"]
        .as_array()
        .context("Cargo metadata has no packages")?;
    let package = packages
        .iter()
        .find(|package| {
            package["manifest_path"]
                .as_str()
                .is_some_and(|p| Path::new(p) == lib.manifest)
        })
        .with_context(|| {
            format!(
                "{} must identify a Cargo package, not a virtual workspace",
                lib.manifest.display()
            )
        })?;
    let targets: Vec<_> = package["targets"]
        .as_array()
        .context("Cargo package has no targets")?
        .iter()
        .filter(|target| {
            target["crate_types"]
                .as_array()
                .is_some_and(|types| types.iter().any(|ty| ty == "cdylib"))
        })
        .collect();
    anyhow::ensure!(
        targets.len() == 1,
        "{} must declare exactly one cdylib target",
        lib.manifest.display()
    );
    let string = |value: &Value| -> Result<String> {
        Ok(value
            .as_str()
            .context("expected string in Cargo metadata")?
            .to_owned())
    };
    let package_id = string(&package["id"])?;
    let nodes = metadata["resolve"]["nodes"]
        .as_array()
        .context("Cargo metadata has no dependency graph")?;
    let mut reachable = BTreeSet::new();
    let mut pending = vec![package_id.clone()];
    while let Some(id) = pending.pop() {
        if !reachable.insert(id.clone()) {
            continue;
        }
        let node = nodes
            .iter()
            .find(|node| node["id"] == id)
            .context("Cargo dependency node missing")?;
        for dep in node["dependencies"]
            .as_array()
            .context("Cargo node dependencies missing")?
        {
            pending.push(string(dep)?);
        }
    }
    let workspace = PathBuf::from(string(&metadata["workspace_root"])?);
    let mut watch_paths = vec![
        workspace.join("Cargo.toml"),
        workspace.join("Cargo.lock"),
        workspace.join(".cargo"),
    ];
    for package in packages {
        if !package["source"].is_null() || !reachable.contains(&string(&package["id"])?) {
            continue;
        }
        let manifest = PathBuf::from(string(&package["manifest_path"])?);
        watch_paths.push(
            manifest
                .parent()
                .context("Cargo manifest has no parent")?
                .to_path_buf(),
        );
    }
    Ok(CargoLibrary {
        package_id,
        package_name: string(&package["name"])?,
        target_name: string(&targets[0]["name"])?,
        watch_paths,
        target_dir: PathBuf::from(string(&metadata["target_directory"])?),
    })
}

#[derive(Default)]
pub struct NativeInputs {
    pub paths: Vec<PathBuf>,
    excluded: Vec<PathBuf>,
}

impl NativeInputs {
    pub fn excludes(&self, path: &Path) -> bool {
        self.excluded.iter().any(|root| path.starts_with(root))
    }

    pub fn contains(&self, path: &Path) -> bool {
        !self.excludes(path) && self.paths.iter().any(|root| path.starts_with(root))
    }
}

pub fn native_watch_inputs(libs: &[NativeLibrary]) -> Result<NativeInputs> {
    let mut paths = BTreeSet::new();
    let mut excluded = BTreeSet::new();
    for lib in libs {
        let cargo = cargo_library(lib)?;
        for path in &cargo.watch_paths {
            if path.is_dir() {
                excluded.insert(path.join(".git"));
            }
        }
        paths.extend(cargo.watch_paths);
        excluded.insert(cargo.target_dir);
        excluded.insert(
            lib.stage_dir
                .parent()
                .expect("native directory is under tmp")
                .to_path_buf(),
        );
    }
    Ok(NativeInputs {
        paths: paths.into_iter().collect(),
        excluded: excluded.into_iter().collect(),
    })
}

pub fn native_preamble(project: &str) -> Result<String> {
    let libs = game_engine_libs(project)?;
    let Some(lib) = libs.first() else {
        return Ok(String::new());
    };
    let path = lib
        .stage_dir
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    // Installed game bootstraps supply their own directory before loading bytecode.
    Ok(format!(
        "__native_library_dir = __native_library_dir or \"{path}\"\n"
    ))
}

pub fn build_optional_engine_lib(lib: &NativeLibrary) -> Result<()> {
    let cargo = cargo_library(lib)?;
    log::info!("Building project native library {}", lib.dst_stem);
    let output = cargo_command(&lib.manifest)
        .args(["build", "--lib", "--manifest-path"])
        .arg(&lib.manifest)
        .args([
            "-p",
            &cargo.package_name,
            "--message-format=json-render-diagnostics",
        ])
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("cannot build {}", lib.manifest.display()))?;
    anyhow::ensure!(
        output.status.success(),
        "native build failed for {}",
        lib.manifest.display()
    );
    let mut artifact = None;
    for line in output
        .stdout
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
    {
        let message: Value = serde_json::from_slice(line).context("invalid Cargo build message")?;
        if message["reason"] == "compiler-artifact"
            && message["package_id"] == cargo.package_id
            && message["target"]["name"] == cargo.target_name
        {
            for filename in message["filenames"]
                .as_array()
                .context("Cargo artifact has no filenames")?
            {
                let path = PathBuf::from(
                    filename
                        .as_str()
                        .context("invalid Cargo artifact filename")?,
                );
                if path
                    .extension()
                    .is_some_and(|ext| ext == std::env::consts::DLL_EXTENSION)
                {
                    artifact = Some(path);
                }
            }
        }
    }
    let artifact = artifact.context("Cargo succeeded without producing the requested cdylib")?;
    fs_err::create_dir_all(&lib.stage_dir)?;
    let destination = lib
        .stage_dir
        .join(crate::engine_build::cdylib_name(&lib.dst_stem));
    crate::engine_build::stage_cdylib_from(&artifact, &destination, |path| {
        crate::engine_build::prepare_staged_cdylib(path, &lib.dst_stem)
    })
}

pub fn registry_json(libs: &[NativeLibrary]) -> Result<Value> {
    libs.iter()
        .map(|lib| {
            let cargo = cargo_library(lib)?;
            Ok(
                serde_json::json!({ "need": lib.need, "crate": cargo.package_name,
            "manifest": lib.manifest, "stage_dir": lib.stage_dir, "src_stem": cargo.target_name,
            "dst_stem": lib.dst_stem, "shipped": lib.shipped }),
            )
        })
        .collect::<Result<Vec<_>>>()
        .map(Value::Array)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        for dir in ["game", "bindings", "native", "support/src"] {
            fs_err::create_dir_all(root.path().join(dir)).unwrap();
        }
        fs_err::write(
            root.path().join("game/stick.toml"),
            "[package]\nname = \"game\"\n[dependencies]\nbindings = { path = \"../bindings\" }\n",
        )
        .unwrap();
        fs_err::write(root.path().join("bindings/stick.toml"),
            "[package]\nname = \"bindings\"\ntype = \"lib\"\n[native]\nmanifest = \"../native/Cargo.toml\"\nlibrary = \"probe\"\n").unwrap();
        fs_err::write(root.path().join("native/Cargo.toml"),
            "[package]\nname = \"native-source\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[lib]\nname = \"different_cargo_name\"\npath = \"lib.rs\"\ncrate-type = [\"cdylib\"]\n[dependencies]\nsupport = { path = \"../support\" }\n[workspace]\n").unwrap();
        fs_err::write(
            root.path().join("native/lib.rs"),
            "#[unsafe(no_mangle)] pub extern \"C\" fn native_probe() -> i32 { support::value() }\n",
        )
        .unwrap();
        fs_err::write(
            root.path().join("support/Cargo.toml"),
            "[package]\nname = \"support\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        fs_err::write(
            root.path().join("support/src/lib.rs"),
            "pub fn value() -> i32 { 17 }\n",
        )
        .unwrap();
        root
    }

    #[test]
    fn discovery_follows_package_dependencies_and_rejects_collisions() {
        let root = fixture();
        let game = root.path().join("game");
        let libs = game_engine_libs(game.to_str().unwrap()).unwrap();
        assert_eq!(libs.len(), 1);
        assert_eq!(libs[0].need, "bindings");
        assert_eq!(
            libs[0].manifest,
            root.path()
                .join("native/Cargo.toml")
                .canonicalize()
                .unwrap()
        );
        assert_eq!(
            libs[0].stage_dir,
            game.canonicalize().unwrap().join("tmp/native")
        );
        let path = game.join("stick.toml");
        let original = fs_err::read_to_string(&path).unwrap();
        fs_err::write(
            &path,
            format!(
                "{original}\n[native]\nmanifest = \"../native/Cargo.toml\"\nlibrary = \"probe\"\n"
            ),
        )
        .unwrap();
        assert!(
            game_engine_libs(game.to_str().unwrap())
                .unwrap_err()
                .to_string()
                .contains("duplicate")
        );
        fs_err::write(&path, "[package]\nname = \"other\"\n").unwrap();
        assert!(game_engine_libs(game.to_str().unwrap()).unwrap().is_empty());
    }

    #[test]
    fn prebuilt_toolchain_rebuilds_only_project_libraries_and_preserves_last_good_artifact() {
        const CHILD: &str = "ROCK_NATIVE_LIBRARY_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let result = Command::new(std::env::current_exe().unwrap())
                .args(["native_libraries::tests::prebuilt_toolchain_rebuilds_only_project_libraries_and_preserves_last_good_artifact", "--exact", "--nocapture"])
                .env(CHILD, "1").env("ROCK_PREBUILT_GAME", "1")
                .env("ROCK_ENGINE_BUILD_RECEIPT", "/deliberately-unavailable-toolchain-receipt")
                .output().unwrap();
            assert!(
                result.status.success(),
                "{}{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }
        let root = fixture();
        let target = root.path().join("custom-cargo-output");
        unsafe {
            std::env::set_var("CARGO_TARGET_DIR", &target);
        }
        let game = root.path().join("game");
        let libs = game_engine_libs(game.to_str().unwrap()).unwrap();
        let lib = &libs[0];
        build_optional_engine_lib(lib).unwrap();
        let inputs = native_watch_inputs(&libs).unwrap();
        let canonical_root = root.path().canonicalize().unwrap();
        assert!(inputs.contains(&canonical_root.join("support/src/lib.rs")));
        assert!(inputs.contains(&canonical_root.join("native/Cargo.toml")));
        assert!(!inputs.contains(&target.join("debug/example.rs")));
        assert!(!inputs.contains(&lib.stage_dir.join("example.rs")));
        let staged = lib
            .stage_dir
            .join(crate::engine_build::cdylib_name("probe"));
        let read = || {
            let program = format!(
                "local ffi = require('ffi'); ffi.cdef('int native_probe(void);'); print(ffi.load({:?}).native_probe())",
                staged.to_str().unwrap()
            );
            let output = Command::new(crate::runtime::luajit_path())
                .args(["-e", &program])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        assert_eq!(read(), "17");
        fs_err::write(
            root.path().join("support/src/lib.rs"),
            "pub fn value() -> i32 { 29 }\n",
        )
        .unwrap();
        build_optional_engine_lib(lib).unwrap();
        assert_eq!(read(), "29");
        let good = fs_err::read(&staged).unwrap();
        fs_err::write(root.path().join("native/lib.rs"), "this does not compile\n").unwrap();
        assert!(build_optional_engine_lib(lib).is_err());
        assert_eq!(fs_err::read(&staged).unwrap(), good);
        assert_eq!(read(), "29");
        assert!(
            !target
                .join("debug")
                .join(crate::engine_build::cdylib_name("stone_rs"))
                .exists()
        );
    }
}
