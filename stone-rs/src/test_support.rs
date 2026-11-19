use std::ffi::OsStr;
use std::ops::Deref;
use std::path::Path;
use std::process::Command;

pub(crate) struct TestRoot(tempfile::TempDir);

impl Deref for TestRoot {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        self.0.path()
    }
}

impl AsRef<Path> for TestRoot {
    fn as_ref(&self) -> &Path {
        self.0.path()
    }
}

impl AsRef<std::ffi::OsStr> for TestRoot {
    fn as_ref(&self) -> &std::ffi::OsStr {
        self.0.path().as_os_str()
    }
}

pub(crate) fn fresh_root(prefix: &str) -> TestRoot {
    let dir = tempfile::Builder::new()
        .prefix(prefix)
        .tempdir()
        .expect("create temporary test root");
    TestRoot(dir)
}

/// A command that reruns only `test` from this test binary, with `child_env`
/// set so the child takes the test's fixture branch.
pub(crate) fn child_test(test: &str, child_env: &str, value: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(std::env::current_exe().expect("locate test executable"));
    command
        .args([test, "--exact", "--nocapture"])
        .env(child_env, value);
    command
}

/// Run a `child_test` command; it must select one test and pass.
pub(crate) fn assert_child_passes(command: &mut Command, context: &str) {
    let output = command.output().expect("run isolated test fixture");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("running 1 test"),
        "{context}: the child selected no test:\n{stdout}{stderr}"
    );
    assert!(output.status.success(), "{context}:\n{stdout}{stderr}");
}

/// Rerun `test` in a fresh process unless this is that process. True when the
/// child ran, so the caller returns.
pub(crate) fn in_isolated_child(test: &str, child_env: &str) -> bool {
    if std::env::var_os(child_env).is_some() {
        return false;
    }
    assert_child_passes(&mut child_test(test, child_env, "1"), test);
    true
}

#[cfg(test)]
mod tests {
    use super::fresh_root;

    #[test]
    fn fresh_root_is_removed_after_scope_ends() {
        let path;
        {
            let root = fresh_root("stone_test_support_drop_");
            path = root.to_path_buf();
            assert!(path.is_dir());
        }
        assert!(!path.exists());
    }

    #[test]
    fn fresh_root_is_removed_during_unwind() {
        let path = {
            let root = fresh_root("stone_test_support_unwind_");
            let path = root.to_path_buf();
            let result = std::panic::catch_unwind(move || {
                let _root = root;
                panic!("exercise temporary root cleanup during unwind");
            });
            assert!(result.is_err());
            path
        };
        assert!(!path.exists());
    }
}
