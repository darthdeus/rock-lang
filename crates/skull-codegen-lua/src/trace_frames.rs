use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

pub(crate) type Contributors = BTreeMap<String, Vec<String>>;

pub(crate) fn enabled() -> bool {
    std::env::var_os("ROCK_LUA_TRACE_FRAMES").is_some_and(|v| v == "1")
}

#[derive(Debug)]
struct Call {
    pc: usize,
    opcode: String,
    prefix: usize,
    target: String,
}

#[derive(Debug)]
struct Frame {
    name: String,
    line: usize,
    slots: usize,
    params: usize,
    calls: Vec<Call>,
}

#[derive(Clone, Default)]
struct Chain {
    peak: usize,
    path: Vec<usize>,
    unknown: u8,
}

const INDIRECT: u8 = 1;
const RECURSIVE: u8 = 2;
const TAIL: u8 = 4;

fn probe(lua: &str, executable: &Path) -> Result<String, String> {
    let mut child = Command::new(executable)
        .args(["-e", include_str!("trace_frames.lua")])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot start LuaJIT bytecode probe: {error}"))?;
    // The probe consumes all input before producing output, so pipe capacity
    // cannot deadlock a large generated module against its report.
    let sent = child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(lua.as_bytes());
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

fn parse(raw: &str) -> Result<(usize, Vec<Frame>), String> {
    let invalid = || "invalid LuaJIT frame probe output".to_string();
    let number = |s: &str| s.parse::<usize>().map_err(|_| invalid());
    let mut links = None;
    let mut frames = Vec::<Frame>::new();
    for line in raw.lines() {
        let fields: Vec<_> = line.split('\t').collect();
        match fields.as_slice() {
            ["VM", n] => links = Some(number(n)?),
            ["F", id, name, slots, params, line] => {
                if number(id)? != frames.len() {
                    return Err(invalid());
                }
                frames.push(Frame {
                    name: name.to_string(),
                    line: number(line)?,
                    slots: number(slots)?,
                    params: number(params)?,
                    calls: Vec::new(),
                });
            }
            ["C", id, pc, opcode, prefix, target] => {
                let frame = frames.get_mut(number(id)?).ok_or_else(invalid)?;
                frame.calls.push(Call {
                    pc: number(pc)?,
                    opcode: opcode.to_string(),
                    prefix: number(prefix)?,
                    target: target.to_string(),
                });
            }
            _ => return Err(invalid()),
        }
    }
    Ok((
        links.filter(|&n| n == 1 || n == 2).ok_or_else(invalid)?,
        frames,
    ))
}

fn chain(
    id: usize,
    frames: &[Frame],
    names: &BTreeMap<&str, Option<usize>>,
    links: usize,
    visiting: &mut [bool],
    memo: &mut [Option<Chain>],
) -> Chain {
    if let Some(cached) = &memo[id] {
        return cached.clone();
    }
    let frame = &frames[id];
    let mut result = Chain {
        peak: frame.slots,
        path: vec![id],
        unknown: 0,
    };
    visiting[id] = true;
    for call in &frame.calls {
        if call.opcode == "CALLT" || call.opcode == "CALLMT" {
            result.unknown |= TAIL;
            continue;
        }
        let Some(Some(target)) = names.get(call.target.as_str()) else {
            result.unknown |= INDIRECT;
            continue;
        };
        if visiting[*target] {
            result.unknown |= RECURSIVE;
            continue;
        }
        let child = chain(*target, frames, names, links, visiting, memo);
        result.unknown |= child.unknown;
        let peak = call.prefix + links + child.peak;
        if peak > result.peak {
            result.peak = peak;
            result.path = std::iter::once(id).chain(child.path).collect();
        }
    }
    visiting[id] = false;
    // A cycle cut leaves a partial path, explicitly marked unknown. Retaining
    // partial results keeps highly connected recursive graphs bounded.
    memo[id] = Some(result.clone());
    result
}

pub(crate) fn report_with_vm(
    lua: &str,
    contributors: &Contributors,
    executable: &Path,
) -> Result<String, String> {
    let (links, frames) = parse(&probe(lua, executable)?)?;
    let mut names = BTreeMap::new();
    for (id, frame) in frames.iter().enumerate() {
        names
            .entry(frame.name.as_str())
            .and_modify(|value| *value = None)
            .or_insert(Some(id));
    }
    let mut out = format!(
        "LuaJIT emitted-call frame report: frame-links={links}; LJ_MAX_JSLOTS=250\n\
         Parsed final emitted Lua without execution. Known non-tail paths estimate selected inlining path pressure; \
         branches, loop trace entry, recursion, indirect/external targets and tail calls can make the full chain unknown. \
         A warning predicts a slot cliff on that path, not a guaranteed abort: an independently compiled callee can link. \
         Register coalescing is a separate constraint.\n"
    );
    let mut memo = vec![None; frames.len()];
    let mut visiting = vec![false; frames.len()];
    for (id, frame) in frames.iter().enumerate() {
        let estimate = chain(id, &frames, &names, links, &mut visiting, &mut memo);
        let peak = links + estimate.peak;
        let path = estimate
            .path
            .iter()
            .map(|&i| frames[i].name.as_str())
            .collect::<Vec<_>>()
            .join(" -> ");
        let _ = writeln!(
            out,
            "function {}: frame={} params={} lua-line={} known-chain-peak={peak} path={path}",
            frame.name, frame.slots, frame.params, frame.line
        );
        for (flag, label) in [
            (INDIRECT, "indirect/external"),
            (RECURSIVE, "recursion"),
            (TAIL, "tail call"),
        ] {
            if estimate.unknown & flag != 0 {
                let _ = writeln!(out, "  {label}: unknown");
            }
        }
        for call in &frame.calls {
            let target = names.get(call.target.as_str()).and_then(|id| *id);
            let slots = target
                .map(|id| frames[id].slots.to_string())
                .unwrap_or_else(|| "unknown".into());
            let _ = writeln!(
                out,
                "  pc={} {} A={} + frame-links={} -> {} frame={slots}",
                call.pc, call.opcode, call.prefix, links, call.target
            );
        }
        if let Some(lines) = contributors.get(&frame.name) {
            for line in lines {
                let _ = writeln!(out, "  wide value: {line}");
            }
        }
        if peak >= 250 {
            let _ = writeln!(
                out,
                "  warning: {path} reaches {peak} slots on this non-tail path; LuaJIT requires fewer than LJ_MAX_JSLOTS=250"
            );
            for &member in estimate.path.iter().skip(1) {
                if let Some(lines) = contributors.get(&frames[member].name) {
                    for line in lines {
                        let _ = writeln!(out, "    {}: {line}", frames[member].name);
                    }
                }
            }
        }
    }
    Ok(out)
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
        let contributors = BTreeMap::from([(
            "run".into(),
            vec!["parameter `input` of `run` is 130 scalar lanes (nobox: cannot be boxed)".into()],
        )]);
        let report = report(&lua, &contributors).expect("both functions load");
        assert!(report.contains("warning:"), "{report}");
        assert!(report.contains("LJ_MAX_JSLOTS=250"), "{report}");
        assert!(report.contains("nobox"), "{report}");
        assert!(report.contains("run -> leaf"), "{report}");
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
    fn includes_nested_prefixes_and_root_frame_links() {
        let lua = "function leaf(x) return x + 1 end\nfunction middle(x) local y = leaf(x); return y + x end\nfunction root(x) local y = middle(x); return y + x end\n";
        let report = analyze(lua);
        assert!(report.contains("root -> middle -> leaf"), "{report}");
        assert!(report.contains("selected inlining path"), "{report}");
        let executable = std::env::var_os("ROCK_LUAJIT").unwrap_or_else(|| "luajit".into());
        let (links, _) = parse(&probe(lua, Path::new(&executable)).unwrap()).unwrap();
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
