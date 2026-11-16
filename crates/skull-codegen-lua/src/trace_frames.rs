use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

pub(crate) type Contributors = BTreeMap<String, Vec<String>>;

pub(crate) fn enabled() -> bool {
    std::env::var_os("ROCK_LUA_TRACE_FRAMES").is_some_and(|v| v == "1")
}

pub(crate) fn report_with_vm(
    lua: &str,
    contributors: &Contributors,
    executable: &Path,
) -> Result<String, String> {
    let mut input = String::from("return {");
    for (name, lines) in contributors {
        input.push('[');
        crate::write_lua_string(&mut input, name);
        input.push_str("] = {");
        for line in lines {
            crate::write_lua_string(&mut input, line);
            input.push(',');
        }
        input.push_str("},");
    }
    input.push_str("}\n");

    let mut child = Command::new(executable)
        .args(["-e", include_str!("trace_frames.lua")])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot start LuaJIT bytecode probe: {error}"))?;
    // The probe consumes all input before producing output, so pipe capacity
    // cannot deadlock a large generated module against its report.
    let sent = {
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin
            .write_all(input.as_bytes())
            .and_then(|_| stdin.write_all(lua.as_bytes()))
    };
    let output = child
        .wait_with_output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "LuaJIT bytecode probe failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    sent.map_err(|error| error.to_string())?;
    String::from_utf8(output.stdout).map_err(|error| error.to_string())
}

pub(crate) fn report(lua: &str, contributors: &Contributors) -> Result<String, String> {
    let executable = std::env::var_os("ROCK_LUAJIT").unwrap_or_else(|| "luajit".into());
    report_with_vm(lua, contributors, Path::new(&executable))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn analyze(lua: &str) -> String {
        report(lua, &BTreeMap::new()).expect("target bytecode report")
    }

    #[test]
    fn reports_target_frames_and_direct_non_tail_chain() {
        let report = analyze(
            "function leaf(x) return x + 1 end\nfunction run(x) local y = leaf(x); return y + x end\nerror('must not execute')\n",
        );
        assert!(report.contains("function leaf: frame="), "{report}");
        assert!(report.contains("function run: frame="), "{report}");
        assert!(report.contains("run -> leaf"), "{report}");
        assert!(report.contains("CALL A="), "{report}");
        assert!(report.contains("frame-links="), "{report}");
    }

    #[test]
    fn warns_for_chain_pressure_without_rejecting_loadable_frames() {
        let params = (0..130)
            .map(|i| format!("p{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let args = (0..130)
            .map(|i| format!("p{i}"))
            .collect::<Vec<_>>()
            .join(" + ");
        let locals = (0..130)
            .map(|i| format!("local p{i} = x + {i}\n"))
            .collect::<String>();
        let lua = format!(
            "function leaf(x)\n{locals}return {args} end\nfunction run({params}) local y = leaf(p0); return y + p0 end\n"
        );
        let delimiter_text = "\"}; error('metadata executed') --\\\r\ttab\nnext\0byte\u{1}\u{7f}9";
        let contributors = BTreeMap::from([
            (
                "leaf".into(),
                vec!["leaf parameter has 130 scalar lanes".into()],
            ),
            (
                "run".into(),
                vec![
                    "parameter `input` of `run` is 130 scalar lanes (nobox: cannot be boxed)"
                        .into(),
                    delimiter_text.into(),
                ],
            ),
        ]);
        let report = report(&lua, &contributors).expect("both functions load");
        assert!(report.contains("warning:"), "{report}");
        assert!(report.contains("LJ_MAX_JSLOTS=250"), "{report}");
        assert!(report.contains("nobox"), "{report}");
        assert!(report.contains(delimiter_text), "{report:?}");
        assert!(report.contains("run -> leaf"), "{report}");
        assert!(
            report.contains("    leaf: leaf parameter has 130 scalar lanes"),
            "{report}"
        );
    }

    #[test]
    fn marks_indirect_recursive_and_tail_edges_unknown() {
        let report = analyze(
            "function recurse(x) local y = recurse(x); return y + 1 end\nfunction indirect(f, x) local y = f(x); return y + 1 end\nfunction tail(x) return recurse(x) end\n",
        );
        assert!(report.contains("recursion: unknown"), "{report}");
        assert!(report.contains("indirect/external: unknown"), "{report}");
        assert!(report.contains("tail call: unknown"), "{report}");
    }

    #[test]
    fn recognizes_bound_callees_and_physical_headers_with_directives() {
        let report = analyze(
            "# 20 \"sample.rock\"\nfunction leaf(x) return x + 1 end\n# 40 \"sample.rock\"\n__rock_mk_run = function(leaf) return function(x) local y = leaf(x); return y + x end end\n",
        );
        assert!(report.contains("function run: frame="), "{report}");
        assert!(report.contains("run -> leaf"), "{report}");
    }

    #[test]
    fn resolves_private_factory_parameters_through_the_binding_line() {
        let report = analyze(
            "function leaf(x) return x + 1 end\n__rock_mk_run = function(b0) return function(x) local y = b0(x); return y + x end end\nrun = __rock_mk_run(leaf)\n",
        );
        assert!(report.contains("run -> leaf"), "{report}");
        assert!(!report.contains("-> b0"), "{report}");
    }

    #[test]
    fn does_not_resolve_global_calls_to_scoped_local_functions() {
        let report = analyze(
            "do\nlocal function leaf(x) return x + 1 end\nend\nfunction run(x) local y = leaf(x); return y + x end\n",
        );
        assert!(
            !report.contains("run -> leaf"),
            "a local declaration was treated as a global target: {report}"
        );
        let run = report.split("function run:").nth(1).expect("run report");
        assert!(run.contains("indirect/external: unknown"), "{run}");
    }

    #[test]
    fn duplicate_names_remain_ambiguous_call_targets() {
        let report = analyze(
            "function duplicate(x) return x + 1 end\nfunction duplicate(x) return x + 2 end\nfunction run(x) local y = duplicate(x); return y + x end\n",
        );
        let run = report.split("function run:").nth(1).expect("run report");
        assert!(!run.contains("run -> duplicate"), "{run}");
        assert!(run.contains("-> duplicate frame=unknown"), "{run}");
        assert!(run.contains("indirect/external: unknown"), "{run}");
    }

    #[test]
    fn includes_nested_prefixes_and_root_frame_links() {
        let lua = "function leaf(x) return x + 1 end\nfunction middle(x) local y = leaf(x); return y + x end\nfunction root(x) local y = middle(x); return y + x end\n";
        let report = analyze(lua);
        assert!(report.contains("root -> middle -> leaf"), "{report}");
        assert!(report.contains("selected inlining path"), "{report}");
        let links = report
            .lines()
            .next()
            .and_then(|line| line.split("frame-links=").nth(1))
            .and_then(|field| field.split(';').next())
            .and_then(|value| value.parse::<usize>().ok())
            .expect("frame-links in report header");
        let root = report
            .lines()
            .find(|line| line.starts_with("function root:"))
            .unwrap();
        // Each call has A=1, and the leaf needs two slots. Include the trace
        // root's frame link as well as both inlined calls' links.
        assert!(
            root.contains(&format!("known-chain-peak={}", 4 + 3 * links)),
            "{root}"
        );
    }

    #[test]
    fn warns_at_the_trace_limit_boundary() {
        let params = (0..120)
            .map(|i| format!("p{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let mut saw_boundary = false;
        for count in 118..128 {
            let locals = (0..count)
                .map(|i| format!("local a{i} = x + {i}\n"))
                .collect::<String>();
            let sum = (0..count)
                .map(|i| format!("a{i}"))
                .collect::<Vec<_>>()
                .join(" + ");
            let lua = format!(
                "function leaf(x)\n{locals}return {sum} end\nfunction run({params}) local y = leaf(p0); return y + p0 end\n"
            );
            let report = analyze(&lua);
            let run = report.split("function run:").nth(1).expect("run report");
            if run
                .lines()
                .next()
                .unwrap()
                .contains("known-chain-peak=250 ")
            {
                saw_boundary = true;
                assert!(run.contains("warning:"), "{run}");
            }
        }
        assert!(saw_boundary, "fixture did not reach LJ_MAX_JSLOTS");
    }
}
