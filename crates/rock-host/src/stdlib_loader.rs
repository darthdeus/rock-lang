use crate::preprocessor::{preprocess, PreprocessorContext};
use std::path::PathBuf;
use std::sync::OnceLock;

/// A stdlib source file: its short name and full contents (read from disk).
pub struct StdlibFile {
    pub name: &'static str,
    pub contents: &'static str,
}

/// The stdlib files, in load order. `unified.rock` is the core stdlib;
/// `strings.rock` (portable string algorithms) and `json.rock` build on it.
/// `js.rock` holds JS-backend bodies for stdlib fns defined in the others, and
/// is LAST so adding one shifts no `__SITE__` line in a lua-emitting file.
const STDLIB_NAMES: &[&str] = &["unified.rock", "logging.rock", "vec.rock", "mat.rock", "arena.rock", "scene.rock", "strings.rock", "json.rock", "shader.rock", "js.rock"];

/// Directory the stdlib `.rock` files are read from at runtime (not embedded).
/// `ROCK_STDLIB_DIR` overrides; otherwise the shared Rock-root resolver selects
/// the active checkout. A fresh process (or LSP restart) picks up stdlib edits.
pub fn stdlib_dir() -> PathBuf {
    resolve_stdlib_dir(
        std::env::var("ROCK_STDLIB_DIR").ok().map(PathBuf::from),
        PathBuf::from(crate::rock_dir())
    )
}

fn resolve_stdlib_dir(override_dir: Option<PathBuf>, rock_root: PathBuf) -> PathBuf {
    override_dir.unwrap_or_else(|| rock_root.join("stdlib"))
}

/// All stdlib files, read from disk on first call and cached for the process
/// lifetime. Contents are leaked to `&'static str`: the stdlib is resident for
/// the whole process exactly as the old `include_str!` form was, so a one-time
/// leak of a bounded, always-live buffer is equivalent -- not an unbounded leak.
/// Panics with the resolved path if a file can't be read (the stdlib is not
/// optional; a clear failure beats a thousand downstream "callee not found"s).
pub fn stdlib_files() -> &'static [StdlibFile] {
    static FILES: OnceLock<Vec<StdlibFile>> = OnceLock::new();
    FILES.get_or_init(|| {
        let dir = stdlib_dir();
        STDLIB_NAMES
            .iter()
            .map(|&name| {
                let path = dir.join(name);
                let contents = std::fs::read_to_string(&path).unwrap_or_else(|e| {
                    panic!(
                        "rock: cannot read stdlib `{}`: {e}\n  (resolved stdlib dir: {}; \
                         override with ROCK_STDLIB_DIR)",
                        path.display(),
                        dir.display()
                    )
                });
                StdlibFile {
                    name,
                    contents: Box::leak(contents.into_boxed_str()),
                }
            })
            .collect()
    })
}

/// The source-file id the loaders give stdlib file `name`. Bone reads the
/// compiler-owned `std` package and the std-only primitives from this
/// spelling (`bone_core::modules::is_stdlib_module_file`), so every host that
/// feeds bone the stdlib -- the CLI and the editor alike -- keys it this way.
pub fn stdlib_file_id(name: &str) -> String {
    format!("<stdlib>/{name}")
}

/// Load all stdlib files and apply preprocessing for the given backend name.
pub fn load_stdlib_for_backend(backend: &str) -> Vec<(String, String, String)> {
    let mut result = Vec::new();

    let context = PreprocessorContext::new(backend);

    for stdlib_file in stdlib_files() {
        let file_path = stdlib_file_id(stdlib_file.name);

        // Apply preprocessing to the file contents
        let processed_contents = match preprocess(stdlib_file.contents, &context) {
            Ok(preprocessed) => {
                // Debug: write preprocessed output
                if std::env::var("ROCK_DEBUG_PREPROCESS").is_ok() {
                    if let Ok(mut f) = fs_err::File::create(format!(
                        "/tmp/{}_preprocessed.rock",
                        stdlib_file.name
                    )) {
                        use std::io::Write;
                        writeln!(
                            f,
                            "// Preprocessed from {} for backend {}",
                            stdlib_file.name, backend
                        )
                        .ok();
                        write!(f, "{}", preprocessed.content).ok();
                    }
                }
                preprocessed.content
            }
            Err(e) => {
                eprintln!(
                    "Warning: Failed to preprocess stdlib file {}: {}",
                    stdlib_file.name, e
                );
                stdlib_file.contents.to_string()
            }
        };

        // Only include the file if it has content after preprocessing
        if !processed_contents.trim().is_empty() {
            result.push((file_path, stdlib_file.name.to_string(), processed_contents));
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_stdlib_follows_runtime_rock_root() {
        let rock_root = PathBuf::from("/runtime/rock-worktree");
        assert_eq!(
            resolve_stdlib_dir(None, rock_root.clone()),
            rock_root.join("stdlib")
        );
    }

    #[test]
    fn explicit_stdlib_override_wins() {
        let override_dir = PathBuf::from("/controlled/stdlib");
        assert_eq!(
            resolve_stdlib_dir(
                Some(override_dir.clone()),
                PathBuf::from("/runtime/rock-worktree")
            ),
            override_dir
        );
    }
}
