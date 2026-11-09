use std::path::PathBuf;

#[test]
fn external_consumers_can_expand_vault_tokens() {
    let expanded = rock_engine_host::art_vault::expand_token("$GAME_ART_VAULT/source", || {
        Ok(PathBuf::from("/vault"))
    })
    .expect("token expansion succeeds");
    assert_eq!(expanded, Some(PathBuf::from("/vault/source")));
}

#[cfg(target_os = "linux")]
#[test]
fn external_consumers_can_read_linux_vulkan_paths() {
    let _: Vec<PathBuf> = rock_engine_host::runtime::linux_vulkan_library_paths();
}
