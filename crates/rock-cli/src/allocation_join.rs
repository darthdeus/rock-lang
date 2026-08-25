use crate::Result;
use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RouteFilter {
    All,
    Update,
    Draw,
    Outside,
}

impl RouteFilter {
    fn parse(value: Option<&str>) -> Result<Self> {
        match value.unwrap_or("all") {
            "all" => Ok(Self::All),
            "update" => Ok(Self::Update),
            "draw" => Ok(Self::Draw),
            "outside" => Ok(Self::Outside),
            other => anyhow::bail!(
                "allocation route must be all, update, draw, or outside; got `{other}`"
            ),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Update => "update",
            Self::Draw => "draw",
            Self::Outside => "outside",
        }
    }
}

#[derive(Debug)]
struct Site<'a> {
    kind: &'a str,
    source: &'a str,
    body: &'a str,
    lua_body_line: Option<u32>,
    update_chain: Option<&'a str>,
    draw_chain: Option<&'a str>,
}

#[derive(Debug, Default)]
struct JitEvidence<'a> {
    traces: HashSet<u32>,
    ir: usize,
    sunk: bool,
    retained: bool,
    retained_object: bool,
    retained_string: bool,
    ops: HashSet<&'a str>,
}

fn emitted_body_name(line: &str) -> Option<&str> {
    line.strip_prefix("function ")?
        .split_once('(')
        .map(|(name, _)| name)
}

fn parse_jit_trace<'a>(
    trace: &'a str,
    lua: &str,
) -> Result<BTreeMap<(String, u32), JitEvidence<'a>>> {
    let mut lines = trace.lines();
    if lines.next() != Some("rock.jit-allocation-trace.v1") {
        anyhow::bail!("JIT allocation trace has the wrong or missing format header");
    }
    let artifact = lines
        .next()
        .and_then(|line| line.strip_prefix("artifact\t"))
        .ok_or_else(|| anyhow::anyhow!("JIT allocation trace has no artifact path"))?;
    if artifact.is_empty() {
        anyhow::bail!("JIT allocation trace has an empty artifact path");
    }
    if lines.next() != Some("trace\tir\tstate\top\tbody_start\tlua_line") {
        anyhow::bail!("JIT allocation trace has the wrong column header");
    }

    let lua_lines = lua.lines().collect::<Vec<_>>();
    let emitted_starts = lua_lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            emitted_body_name(line).map(|name| (index as u32 + 1, name.to_string()))
        })
        .collect::<BTreeMap<_, _>>();
    let mut seen = HashSet::new();
    let mut evidence: BTreeMap<(String, u32), JitEvidence<'a>> = BTreeMap::new();
    for line in lines {
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.len() != 6 {
            anyhow::bail!("malformed JIT allocation trace row `{line}`");
        }
        let trace_id = fields[0].parse::<u32>()?;
        let ir = fields[1].parse::<u32>()?;
        if !seen.insert((trace_id, ir)) {
            anyhow::bail!("JIT allocation trace repeats trace {trace_id} IR {ir}");
        }
        let state = fields[2];
        if state != "sunk" && state != "retained" {
            anyhow::bail!("JIT allocation trace has invalid state `{state}`");
        }
        let op = fields[3];
        if op.is_empty() {
            anyhow::bail!("JIT allocation trace has an empty IR operation");
        }
        let body_start = fields[4].parse::<u32>()?;
        let lua_line = fields[5].parse::<u32>()?;
        if body_start == 0 || lua_line < body_start {
            anyhow::bail!(
                "JIT allocation trace has invalid body/artifact lines {body_start}/{lua_line}"
            );
        }
        if lua_lines.get(lua_line as usize - 1).is_none() {
            anyhow::bail!("JIT allocation trace points past generated Lua at line {lua_line}");
        }
        let Some((&emitted_start, body_name)) = emitted_starts.range(..=lua_line).next_back()
        else {
            continue;
        };
        if body_start < emitted_start {
            continue;
        }
        let local_line = lua_line - emitted_start + 1;
        let entry = evidence.entry((body_name.clone(), local_line)).or_default();
        entry.traces.insert(trace_id);
        entry.ir += 1;
        entry.sunk |= state == "sunk";
        entry.retained |= state == "retained";
        entry.retained_object |=
            state == "retained" && matches!(op, "TNEW" | "TDUP" | "CNEW" | "CNEWI");
        entry.retained_string |=
            state == "retained" && matches!(op, "SNEW" | "XSNEW" | "BUFSTR" | "TOSTR");
        entry.ops.insert(op);
    }
    Ok(evidence)
}

fn site_id(value: &str) -> bool {
    value.len() == 16 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn parse_manifest(manifest: &str) -> Result<BTreeMap<&str, Site<'_>>> {
    let mut sites = BTreeMap::new();
    let mut route = None;
    for line in manifest.lines() {
        if line.starts_with("route frame.update:") {
            route = Some(RouteFilter::Update);
            continue;
        }
        if line.starts_with("route frame.draw:") {
            route = Some(RouteFilter::Draw);
            continue;
        }
        if line.starts_with("route ") {
            route = None;
            continue;
        }

        let trimmed = line.trim_start();
        if route.is_none() {
            let fields = trimmed.split('\t').collect::<Vec<_>>();
            if (fields.len() == 4 || fields.len() == 5) && site_id(fields[0]) {
                let lua_body_line = fields
                    .get(4)
                    .map(|line| line.parse::<u32>())
                    .transpose()
                    .map_err(|error| anyhow::anyhow!(
                        "allocation manifest site {} has invalid Lua body line: {error}",
                        fields[0]
                    ))?;
                if lua_body_line == Some(0) {
                    anyhow::bail!(
                        "allocation manifest site {} has zero Lua body line",
                        fields[0]
                    );
                }
                if sites
                    .insert(
                        fields[0],
                        Site {
                            kind: fields[1],
                            source: fields[2],
                            body: fields[3],
                            lua_body_line,
                            update_chain: None,
                            draw_chain: None,
                        },
                    )
                    .is_some()
                {
                    anyhow::bail!("allocation manifest repeats site {}", fields[0]);
                }
            }
            continue;
        }

        let Some((id, rest)) = trimmed.split_once(' ') else {
            continue;
        };
        if !site_id(id) {
            continue;
        }
        let (_, chain) = rest
            .split_once(" via ")
            .ok_or_else(|| anyhow::anyhow!("allocation route site {id} has no call chain"))?;
        let site = sites
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("allocation route site {id} is absent from manifest"))?;
        match route.expect("route checked above") {
            RouteFilter::Update => site.update_chain = Some(chain),
            RouteFilter::Draw => site.draw_chain = Some(chain),
            _ => unreachable!(),
        }
    }
    if sites.is_empty() {
        anyhow::bail!("allocation manifest contains no emitted sites");
    }
    Ok(sites)
}

fn parse_activation(activation: &str) -> Result<(u64, Vec<(&str, u64)>)> {
    let mut lines = activation.lines();
    if lines.next() != Some("rock.allocation-activation.v1") {
        anyhow::bail!("allocation activation has the wrong or missing format header");
    }
    let frames = lines
        .next()
        .and_then(|line| line.strip_prefix("frames\t"))
        .ok_or_else(|| anyhow::anyhow!("allocation activation has no frame denominator"))?
        .parse::<u64>()?;
    if frames == 0 {
        anyhow::bail!("allocation activation frame denominator is zero");
    }
    let declared = lines
        .next()
        .and_then(|line| line.strip_prefix("sites\t"))
        .ok_or_else(|| anyhow::anyhow!("allocation activation has no site population"))?
        .parse::<usize>()?;
    let mut seen = HashSet::new();
    let mut hits = Vec::with_capacity(declared);
    for line in lines {
        let (id, count) = line
            .split_once('\t')
            .ok_or_else(|| anyhow::anyhow!("malformed allocation activation row `{line}`"))?;
        if !site_id(id) {
            anyhow::bail!("malformed allocation site ID `{id}`");
        }
        if !seen.insert(id) {
            anyhow::bail!("allocation activation repeats site {id}");
        }
        hits.push((id, count.parse::<u64>()?));
    }
    if hits.len() != declared {
        anyhow::bail!(
            "allocation activation declares {declared} sites but contains {} rows",
            hits.len()
        );
    }
    Ok((frames, hits))
}

#[cfg(test)]
fn render_joined_report(
    manifest: &str,
    activation: &str,
    route: Option<&str>,
    lua: Option<&str>,
) -> Result<String> {
    render_joined_report_with_jit(manifest, activation, route, lua, None)
}

pub fn render_joined_report_with_jit(
    manifest: &str,
    activation: &str,
    route: Option<&str>,
    lua: Option<&str>,
    jit_trace: Option<&str>,
) -> Result<String> {
    let filter = RouteFilter::parse(route)?;
    let sites = parse_manifest(manifest)?;
    let (frames, mut hits) = parse_activation(activation)?;
    for (id, _) in &hits {
        if !sites.contains_key(id) {
            anyhow::bail!("active site {id} is absent from the allocation manifest");
        }
    }
    hits.retain(|(id, _)| {
        let site = &sites[id];
        match filter {
            RouteFilter::All => true,
            RouteFilter::Update => site.update_chain.is_some(),
            RouteFilter::Draw => site.draw_chain.is_some(),
            RouteFilter::Outside => site.update_chain.is_none() && site.draw_chain.is_none(),
        }
    });
    hits.sort_by(|(a_id, a_hits), (b_id, b_hits)| b_hits.cmp(a_hits).then_with(|| a_id.cmp(b_id)));

    let mut body_starts = BTreeMap::new();
    let lua_lines = lua.map(|text| text.lines().collect::<Vec<_>>());
    if let Some(lines) = &lua_lines {
        for (index, line) in lines.iter().enumerate() {
            if let Some(name) = emitted_body_name(line) {
                body_starts.insert(name, index as u32 + 1);
            }
        }
    }
    let jit = match (jit_trace, lua) {
        (Some(trace), Some(lua)) => Some(parse_jit_trace(trace, lua)?),
        (Some(_), None) => anyhow::bail!("JIT allocation trace requires generated Lua"),
        (None, _) => None,
    };
    if jit.is_some() && sites.values().any(|site| site.lua_body_line.is_none()) {
        anyhow::bail!("JIT allocation trace requires a five-column allocation manifest");
    }

    let mut out = String::new();
    writeln!(
        out,
        "{}",
        if jit.is_some() {
            "rock.allocation-join.v3"
        } else {
            "rock.allocation-join.v2"
        }
    )?;
    writeln!(out, "frames\t{frames}")?;
    writeln!(out, "filter\t{}", filter.as_str())?;
    writeln!(out, "active_sites\t{}", hits.len())?;
    if jit.is_some() {
        writeln!(
            out,
            "hits/frame\thits\troute\tkind\tsource\tbody\tlua_body_line\tlua_artifact_line\tcall_chain\tid\tjit_state\tjit_class\tjit_traces\tjit_ir\tjit_ops"
        )?;
    } else {
        writeln!(
            out,
            "hits/frame\thits\troute\tkind\tsource\tbody\tlua_body_line\tlua_artifact_line\tcall_chain\tid"
        )?;
    }
    for (id, count) in hits {
        let site = &sites[id];
        let (route, chain) = match (site.update_chain, site.draw_chain) {
            (Some(update), Some(draw)) => ("update+draw", format!("update:{update} | draw:{draw}")),
            (Some(update), None) => ("update", update.to_string()),
            (None, Some(draw)) => ("draw", draw.to_string()),
            (None, None) => ("outside", String::new()),
        };
        let artifact_line = match (site.lua_body_line, lua_lines.as_ref()) {
            (Some(body_line), Some(lines)) => {
                let body_start = body_starts.get(site.body).ok_or_else(|| {
                    anyhow::anyhow!("generated Lua has no emitted body `{}`", site.body)
                })?;
                let artifact_line = body_start + body_line - 1;
                let emitted = lines.get(artifact_line as usize - 1).ok_or_else(|| {
                    anyhow::anyhow!(
                        "allocation site {id} points past generated Lua at line {artifact_line}"
                    )
                })?;
                if jit_trace.is_none() && !emitted.contains(id) {
                    anyhow::bail!(
                        "allocation site {id} does not occur at generated Lua line {artifact_line}"
                    );
                }
                artifact_line.to_string()
            }
            _ => "?".to_string(),
        };
        write!(
            out,
            "{:.4}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            count as f64 / frames as f64,
            count,
            route,
            site.kind,
            site.source,
            site.body,
            site.lua_body_line.map_or_else(|| "?".to_string(), |line| line.to_string()),
            artifact_line,
            chain,
            id,
        )?;
        if let Some(jit) = &jit {
            let key = (
                site.body.to_string(),
                site.lua_body_line.expect("v3 manifest checked above"),
            );
            if let Some(evidence) = jit.get(&key) {
                let state = match (evidence.sunk, evidence.retained) {
                    (true, true) => "mixed",
                    (true, false) => "sunk",
                    (false, true) => "retained",
                    (false, false) => unreachable!(),
                };
                let class = match (evidence.retained_object, evidence.retained_string) {
                    (true, true) => "object+string",
                    (true, false) => "object",
                    (false, true) => "string",
                    (false, false) => "none",
                };
                let mut ops = evidence.ops.iter().copied().collect::<Vec<_>>();
                ops.sort_unstable();
                write!(
                    out,
                    "\t{}\t{}\t{}\t{}\t{}",
                    state,
                    class,
                    evidence.traces.len(),
                    evidence.ir,
                    ops.join(","),
                )?;
            } else {
                write!(out, "\tunseen\tunknown\t0\t0\t")?;
            }
        }
        writeln!(out)?;
    }
    Ok(out)
}

pub fn run(files: &[String]) -> Result<()> {
    if !(files.len() >= 2 && files.len() <= 5) {
        anyhow::bail!(
            "usage: rock allocation-join <manifest> <activation> [all|update|draw|outside] [generated-lua] [jit-allocation-trace]"
        );
    }
    let manifest = std::fs::read_to_string(&files[0]).map_err(|error| {
        anyhow::anyhow!("cannot read allocation manifest {}: {error}", files[0])
    })?;
    let activation = std::fs::read_to_string(&files[1]).map_err(|error| {
        anyhow::anyhow!("cannot read allocation activation {}: {error}", files[1])
    })?;
    let lua = files
        .get(3)
        .map(std::fs::read_to_string)
        .transpose()
        .map_err(|error| anyhow::anyhow!(
            "cannot read generated Lua {}: {error}",
            files.get(3).map_or("", String::as_str)
        ))?;
    let jit_trace = files
        .get(4)
        .map(std::fs::read_to_string)
        .transpose()
        .map_err(|error| {
            anyhow::anyhow!(
                "cannot read JIT allocation trace {}: {error}",
                files.get(4).map_or("", String::as_str)
            )
        })?;
    let report = render_joined_report_with_jit(
        &manifest,
        &activation,
        files.get(2).map(String::as_str),
        lua.as_deref(),
        jit_trace.as_deref(),
    )?;
    let mut stdout = std::io::stdout().lock();
    match std::io::Write::write_all(&mut stdout, report.as_bytes()) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::{render_joined_report, render_joined_report_with_jit};

    const MANIFEST: &str = r#"emitted allocation-capable Lua report: 3 bodies, 3 direct sites

emitted site manifest: 3 sites
  aaaaaaaaaaaaaaaa	string-concat	game.rock:10	update_leaf	7
  bbbbbbbbbbbbbbbb	vec-construction	game.rock:20	draw_leaf	2
  cccccccccccccccc	struct-construction	game.rock:30	outside_leaf	3

route frame.update: 1 roots, 1 reachable bodies, 1 allocation-capable sites, 0 opaque boundaries
  aaaaaaaaaaaaaaaa string-concat at game.rock:10 via update_root -> update_leaf

route frame.draw: 1 roots, 1 reachable bodies, 1 allocation-capable sites, 0 opaque boundaries
  bbbbbbbbbbbbbbbb vec-construction at game.rock:20 via draw_root -> draw_leaf
"#;

    const ACTIVATION: &str = r#"rock.allocation-activation.v1
frames	4
sites	3
aaaaaaaaaaaaaaaa	8
bbbbbbbbbbbbbbbb	20
cccccccccccccccc	2
"#;

    #[test]
    fn join_filters_by_route_and_ranks_executions_per_frame() {
        let report = render_joined_report(MANIFEST, ACTIVATION, Some("draw"), None)
            .expect("joined allocation report");

        assert!(report.contains("filter\tdraw"));
        assert!(report.contains("5.0000\t20\tdraw\tvec-construction"));
        assert!(report.contains("draw_leaf\t2\t?\tdraw_root -> draw_leaf"));
        assert!(report.contains("draw_root -> draw_leaf"));
        assert!(!report.contains("update_leaf"));
        assert!(!report.contains("outside_leaf"));
    }

    #[test]
    fn join_resolves_and_checks_the_exact_generated_lua_line() {
        let lua = r#"-- runtime preamble
function draw_leaf()
  local v = __rock_alloc_hit("bbbbbbbbbbbbbbbb", __rock_vec_new())
end
"#;
        let report = render_joined_report(MANIFEST, ACTIVATION, Some("draw"), Some(lua))
            .expect("joined allocation report with generated Lua");

        assert!(report.contains("draw_leaf\t2\t3\tdraw_root -> draw_leaf"));
    }

    #[test]
    fn join_classifies_observed_jit_allocations_as_sunk_or_retained() {
        let lua = r#"-- runtime preamble
function draw_leaf()
  local v = __rock_alloc_hit("bbbbbbbbbbbbbbbb", __rock_vec_new())
end
"#;
        let trace = r#"rock.jit-allocation-trace.v1
artifact	/generated/main.lua
trace	ir	state	op	body_start	lua_line
1	4	sunk	TNEW	2	3
2	8	retained	TNEW	2	3
"#;
        let report = render_joined_report_with_jit(
            MANIFEST,
            ACTIVATION,
            Some("draw"),
            Some(lua),
            Some(trace),
        )
        .expect("joined JIT allocation evidence");

        assert!(report.starts_with("rock.allocation-join.v3\n"));
        assert!(report.contains("jit_state\tjit_class\tjit_traces\tjit_ir\tjit_ops"));
        assert!(report.contains("mixed\tobject\t2\t2\tTNEW"));
    }

    #[test]
    fn jit_join_rejects_a_manifest_without_body_lines() {
        let old_manifest = MANIFEST
            .lines()
            .map(|line| {
                if line.trim_start().starts_with("bbbbbbbbbbbbbbbb\t") {
                    line.rsplit_once('\t').expect("manifest row").0
                } else {
                    line
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let trace = "rock.jit-allocation-trace.v1\nartifact\tmain.lua\ntrace\tir\tstate\top\tbody_start\tlua_line\n";
        let error = render_joined_report_with_jit(
            &old_manifest,
            ACTIVATION,
            Some("draw"),
            Some("function draw_leaf()\nend\n"),
            Some(trace),
        )
        .expect_err("JIT join needs exact body-local lines");

        assert!(error.to_string().contains("five-column allocation manifest"));
    }

    #[test]
    fn join_rejects_an_activation_from_a_different_manifest() {
        let activation = ACTIVATION.replace("aaaaaaaaaaaaaaaa", "dddddddddddddddd");
        let error = render_joined_report(MANIFEST, &activation, None, None)
            .expect_err("cross-artifact site IDs must reject");

        assert!(
            error
                .to_string()
                .contains("absent from the allocation manifest")
        );
    }

    #[test]
    fn outside_filter_has_a_nonzero_population_and_the_recorded_denominator() {
        let report = render_joined_report(MANIFEST, ACTIVATION, Some("outside"), None)
            .expect("outside allocation report");

        assert!(report.contains("frames\t4"));
        assert!(report.contains("active_sites\t1"));
        assert!(report.contains("0.5000\t2\toutside\tstruct-construction"));
    }
}
