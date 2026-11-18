use super::*;

#[test]
fn streamed_game_main_writes_program_and_optional_entry() {
    for emits_main in [false, true] {
        let program = GameProgram {
            lua: "program\n".into(),
            comptime_inputs: Default::default(),
        };
        let entry = if emits_main {
            "\n__rock_main_wrapper(main)\n"
        } else {
            ""
        };
        let expected = format!("preamble\nprogram\n{entry}");
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main.lua");
        let overlay = dir.path().join("incremental.lua");
        std::fs::write(&main, "old").unwrap();
        std::fs::write(&overlay, "overlay").unwrap();
        let bytes = write_full_main("preamble\n", &program, emits_main, &main, &overlay).unwrap();
        assert_eq!(bytes, expected.len());
        assert_eq!(std::fs::read(&main).unwrap(), expected.as_bytes());
        assert!(!overlay.exists());
    }
}

#[test]
fn streamed_atomic_write_failure_preserves_destination_and_cleans_temp() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.lua");
    for old in [None, Some("old complete program")] {
        if let Some(old) = old {
            std::fs::write(&path, old).unwrap();
        }
        let result = rock_host::project::write_atomic_streamed(&path, |file| {
            file.write_all(b"partial new program")?;
            Err(std::io::Error::other("injected write failure"))
        });
        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(&path).ok().as_deref(), old);
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            usize::from(old.is_some())
        );
    }
}

#[cfg(unix)]
#[test]
fn streamed_atomic_replace_failure_preserves_old_file() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.lua");
    std::fs::write(&path, "old complete program").unwrap();
    let result = rock_host::project::write_atomic_streamed(&path, |file| {
        file.write_all(b"new complete program")?;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500))?;
        Ok(())
    });
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(result.is_err());
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "old complete program"
    );
}
