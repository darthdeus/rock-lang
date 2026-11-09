#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    rock_cli::main_with(&rock_cli::plain::REGISTRATIONS);
}

#[cfg(test)]
mod allocator_tests {
    unsafe extern "C" {
        fn mi_option_get(option: std::ffi::c_int) -> std::ffi::c_long;
    }

    #[test]
    fn compiler_thp_default_does_not_reach_runtime_child() {
        let exe = std::env::current_exe().expect("test executable");
        for (key, value, compiler, runtime) in [
            ("MIMALLOC_ALLOW_THP", None, "0", "1"),
            ("MIMALLOC_ALLOW_THP", Some("1"), "1", "1"),
            ("MIMALLOC_ALLOW_THP", Some("0"), "0", "0"),
            ("MIMALLOC_ALLOW_THP", Some("ON"), "1", "1"),
            ("mimalloc_allow_thp", Some("1"), "1", "1"),
            ("mimalloc_allow_thp", Some("0"), "0", "0"),
        ] {
            let mut child = std::process::Command::new(&exe);
            child
                .args([
                    "--exact",
                    "allocator_tests::thp_process_child",
                    "--nocapture",
                ])
                .env(
                    "ROCK_THP_TEST_DEFAULT",
                    if value.is_none() { "1" } else { "0" },
                )
                .env("ROCK_THP_TEST_ROLE", "compiler")
                .env("ROCK_THP_TEST_EXPECT", compiler)
                .env("ROCK_THP_TEST_RUNTIME_EXPECT", runtime)
                .env_remove("MIMALLOC_ROCK_COMPILER_THP_DEFAULT")
                .env_remove("MIMALLOC_ALLOW_THP")
                .env_remove("mimalloc_allow_thp");
            if let Some(value) = value {
                child.env(key, value);
            }
            let out = child.output().expect("spawn compiler witness");
            assert!(
                out.status.success(),
                "{key}={value:?}: {}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            print!("{key}={value:?}: {}", String::from_utf8_lossy(&out.stdout));
        }
    }

    #[test]
    fn compiler_process_kinds() {
        let named = |name| { rock_cli::CliCommand::named(name) }.expect("registered command");
        for command in ["check", "compile", "compile-single", "test", "test-compile"] {
            assert!(rock_cli::compiler_process(named(command), false));
        }
        assert!(rock_cli::compiler_process(named("sim"), true));
        for command in ["sim", "run", "stone", "frame", "watch", "perf"] {
            assert!(!rock_cli::compiler_process(named(command), false));
        }
    }

    #[test]
    fn thp_process_child() {
        let Ok(role) = std::env::var("ROCK_THP_TEST_ROLE") else {
            return;
        };
        if role == "compiler" {
            rock_host::compiler_thp::enter_compiler_process().expect("compiler restart");
        }
        let expected: i64 = std::env::var("ROCK_THP_TEST_EXPECT")
            .unwrap()
            .parse()
            .unwrap();
        let option = unsafe { mi_option_get(43) };
        assert_eq!(option as i64, expected, "{role} allocator option");
        if role == "runtime" && std::env::var("ROCK_THP_TEST_DEFAULT").as_deref() == Ok("1") {
            assert!(std::env::var_os("MIMALLOC_ALLOW_THP").is_none());
            assert!(std::env::var_os("MIMALLOC_ROCK_COMPILER_THP_DEFAULT").is_none());
        }
        println!("{role} THP option={option}");
        #[cfg(target_os = "linux")]
        {
            let disabled =
                unsafe { libc::prctl(libc::PR_GET_THP_DISABLE, 0usize, 0usize, 0usize, 0usize) };
            assert_eq!(
                disabled,
                i32::from(expected == 0),
                "{role} kernel THP state"
            );
            println!("{role} THP disabled={disabled}");
        }
        if role == "compiler" {
            // Exercise the shared production spawn recipe used by tests, sims and games.
            unsafe { std::env::set_var("ROCK_LUAJIT", std::env::current_exe().unwrap()) };
            let mut runtime = rock_engine_host::runtime::build_luajit_command(
                std::path::Path::new("--exact"),
                &[
                    "allocator_tests::thp_process_child".into(),
                    "--nocapture".into(),
                ],
            );
            runtime.env("ROCK_THP_TEST_ROLE", "runtime").env(
                "ROCK_THP_TEST_EXPECT",
                std::env::var("ROCK_THP_TEST_RUNTIME_EXPECT").unwrap(),
            );
            let out = runtime.output().expect("spawn runtime witness");
            assert!(
                out.status.success(),
                "runtime child: {}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            print!("{}", String::from_utf8_lossy(&out.stdout));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let root = tempfile::tempdir().expect("Lua runner proxy");
                let proxy = root.path().join("luajit");
                let exe = std::env::current_exe()
                    .unwrap()
                    .to_string_lossy()
                    .replace('\'', "'\"'\"'");
                std::fs::write(&proxy, format!(
                    "#!/usr/bin/env bash\nexec '{exe}' --exact allocator_tests::thp_process_child --nocapture\n"
                )).unwrap();
                std::fs::set_permissions(&proxy, std::fs::Permissions::from_mode(0o755)).unwrap();
                unsafe { std::env::set_var("ROCK_LUAJIT", &proxy) };
                let expected = std::env::var("ROCK_THP_TEST_RUNTIME_EXPECT").unwrap();
                let out = rock_host::lua_runner::run_lua_code_with_timeout_and_env(
                    "",
                    Some(test_utils::HANG_BACKSTOP),
                    &[
                        ("ROCK_THP_TEST_ROLE", "runtime"),
                        ("ROCK_THP_TEST_EXPECT", &expected),
                    ],
                )
                .expect("stdin Lua runtime witness");
                assert!(out.success, "stdin Lua runtime: {}", out.combined());
                print!("stdin Lua runtime: {}", out.stdout);
            }
        }
    }
}
