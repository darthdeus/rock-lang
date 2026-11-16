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

pub(crate) fn in_isolated_child(test: &str, child_env: &str) -> bool {
    if std::env::var_os(child_env).is_some() {
        return false;
    }
    let output = Command::new(std::env::current_exe().expect("locate test executable"))
        .args([test, "--exact", "--nocapture"])
        .env(child_env, "1")
        .output()
        .expect("run isolated test fixture");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("running 1 test"),
        "the child must select {test}"
    );
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
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
