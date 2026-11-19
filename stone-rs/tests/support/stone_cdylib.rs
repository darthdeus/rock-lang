use std::path::PathBuf;

pub fn artifact() -> PathBuf {
    let artifact = std::env::var_os("STONE_TEST_LIB")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_exe()
                .expect("locate test executable")
                .parent()
                .expect("test executable directory")
                .join(format!(
                    "{}stone_rs{}",
                    std::env::consts::DLL_PREFIX,
                    std::env::consts::DLL_SUFFIX
                ))
        });
    assert!(
        artifact.is_file(),
        "Stone test cdylib is missing: {}; run python3 scripts/test_native.py or set STONE_TEST_LIB to Cargo's current cdylib artifact",
        artifact.display()
    );
    artifact
}
