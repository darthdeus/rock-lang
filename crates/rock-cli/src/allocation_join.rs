use anyhow::Result;
use skull_codegen_lua::emitted_body_name;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt::Write as _;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RouteFilter {
    All,
    Update,
    Draw,
    Coroutine,
    Outside,
}

impl RouteFilter {
    fn parse(value: Option<&str>) -> Result<Self> {
        match value.unwrap_or("all") {
            "all" => Ok(Self::All),
            "update" => Ok(Self::Update),
            "draw" => Ok(Self::Draw),
            "coroutine" => Ok(Self::Coroutine),
            "outside" => Ok(Self::Outside),
            other => anyhow::bail!(
                "allocation route must be all, update, draw, coroutine, or outside; got `{other}`"
            ),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Update => "update",
            Self::Draw => "draw",
            Self::Coroutine => "coroutine",
            Self::Outside => "outside",
        }
    }
}

#[derive(Debug)]
struct Site<'a> {
    kind: &'a str,
    source: &'a str,
    body: &'a str,
    result_use: &'a str,
    result_uses: u32,
    aggregate_type: &'a str,
    aggregate_fields: u32,
    lua_body_line: Option<u32>,
    update_chain: Option<&'a str>,
    draw_chain: Option<&'a str>,
    coroutine_chain: Option<&'a str>,
    update_callers: Option<&'a str>,
    draw_callers: Option<&'a str>,
    coroutine_callers: Option<&'a str>,
    local_loop_depth: u32,
    update_loop_depth: Option<u32>,
    draw_loop_depth: Option<u32>,
    coroutine_loop_depth: Option<u32>,
}

#[derive(Clone, Copy, Debug)]
struct ActivationHit<'a> {
    id: &'a str,
    total_hits: u64,
    frame_hits: u64,
    active_frames: Option<u64>,
    max_frame_hits: Option<u64>,
    outside_hits: Option<u64>,
}

#[derive(Debug)]
struct Activation<'a> {
    frames: u64,
    frame_aware: bool,
    hits: Vec<ActivationHit<'a>>,
}

#[derive(Debug, Default)]
struct JitEvidence<'a> {
    traces: HashSet<(u32, u32)>,
    ir: usize,
    sunk: bool,
    retained: bool,
    retained_object: bool,
    retained_string: bool,
    ops: HashSet<&'a str>,
}

fn parse_jit_trace<'a>(
    trace: &'a str,
    lua: &str,
) -> Result<BTreeMap<(String, u32), JitEvidence<'a>>> {
    let mut lines = trace.lines();
    let format = lines.next();
    let legacy = format == Some("rock.jit-allocation-trace.v1");
    let current = format == Some("rock.jit-allocation-trace.v2");
    if !legacy && !current {
        anyhow::bail!("JIT allocation trace has the wrong or missing format header");
    }
    let artifact = lines
        .next()
        .and_then(|line| line.strip_prefix("artifact\t"))
        .ok_or_else(|| anyhow::anyhow!("JIT allocation trace has no artifact path"))?;
    if artifact.is_empty() {
        anyhow::bail!("JIT allocation trace has an empty artifact path");
    }
    let expected_header = if legacy {
        "trace\tir\tstate\top\tbody_start\tlua_line"
    } else {
        "trace_generation\ttrace\tir\tstate\top\tbody_start\tlua_line"
    };
    if lines.next() != Some(expected_header) {
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
        if fields.len() != if legacy { 6 } else { 7 } {
            anyhow::bail!("malformed JIT allocation trace row `{line}`");
        }
        let (trace_generation, trace_index, ir_index) = if legacy {
            (0, 0, 1)
        } else {
            (fields[0].parse::<u32>()?, 1, 2)
        };
        let trace_id = fields[trace_index].parse::<u32>()?;
        let ir = fields[ir_index].parse::<u32>()?;
        if !seen.insert((trace_generation, trace_id, ir)) {
            anyhow::bail!(
                "JIT allocation trace repeats generation {trace_generation} trace {trace_id} IR {ir}"
            );
        }
        let state = fields[ir_index + 1];
        if state != "sunk" && state != "retained" {
            anyhow::bail!("JIT allocation trace has invalid state `{state}`");
        }
        let op = fields[ir_index + 2];
        if op.is_empty() {
            anyhow::bail!("JIT allocation trace has an empty IR operation");
        }
        let body_start = fields[ir_index + 3].parse::<u32>()?;
        let lua_line = fields[ir_index + 4].parse::<u32>()?;
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
        entry.traces.insert((trace_generation, trace_id));
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
        if line.starts_with("route coroutine.resume:") {
            route = Some(RouteFilter::Coroutine);
            continue;
        }
        if line.starts_with("route ") {
            route = None;
            continue;
        }

        let trimmed = line.trim_start();
        if route.is_none() {
            let fields = trimmed.split('\t').collect::<Vec<_>>();
            if matches!(fields.len(), 4 | 5 | 6 | 10) && site_id(fields[0]) {
                let lua_body_line = fields
                    .get(4)
                    .map(|line| line.parse::<u32>())
                    .transpose()
                    .map_err(|error| {
                        anyhow::anyhow!(
                            "allocation manifest site {} has invalid Lua body line: {error}",
                            fields[0]
                        )
                    })?;
                if lua_body_line == Some(0) {
                    anyhow::bail!(
                        "allocation manifest site {} has zero Lua body line",
                        fields[0]
                    );
                }
                let local_loop_depth = fields
                    .get(5)
                    .map(|depth| depth.parse::<u32>())
                    .transpose()
                    .map_err(|error| {
                        anyhow::anyhow!(
                            "allocation manifest site {} has invalid local loop depth: {error}",
                            fields[0]
                        )
                    })?
                    .unwrap_or(0);
                let result_use = fields.get(6).copied().unwrap_or("unknown");
                let result_uses = fields
                    .get(7)
                    .map(|uses| uses.parse::<u32>())
                    .transpose()
                    .map_err(|error| {
                        anyhow::anyhow!(
                            "allocation manifest site {} has invalid result use count: {error}",
                            fields[0]
                        )
                    })?
                    .unwrap_or(0);
                let aggregate_type = fields.get(8).copied().unwrap_or("-");
                let aggregate_fields = fields
                    .get(9)
                    .map(|count| count.parse::<u32>())
                    .transpose()
                    .map_err(|error| anyhow::anyhow!(
                        "allocation manifest site {} has invalid aggregate field count: {error}", fields[0]
                    ))?
                    .unwrap_or(0);
                if sites
                    .insert(
                        fields[0],
                        Site {
                            kind: fields[1],
                            source: fields[2],
                            body: fields[3],
                            result_use,
                            result_uses,
                            aggregate_type,
                            aggregate_fields,
                            lua_body_line,
                            update_chain: None,
                            draw_chain: None,
                            coroutine_chain: None,
                            update_callers: None,
                            draw_callers: None,
                            coroutine_callers: None,
                            local_loop_depth,
                            update_loop_depth: None,
                            draw_loop_depth: None,
                            coroutine_loop_depth: None,
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
        let (_, route_detail) = rest
            .split_once(" via ")
            .ok_or_else(|| anyhow::anyhow!("allocation route site {id} has no call chain"))?;
        let (route_and_depth, callers) = route_detail
            .split_once("\tcallers\t")
            .map_or((route_detail, ""), |(route, callers)| (route, callers));
        let (chain, loop_depth) = match route_and_depth.split_once("\tloop_depth\t") {
            Some((chain, depth)) => (
                chain,
                depth.parse::<u32>().map_err(|error| {
                    anyhow::anyhow!("allocation route site {id} has invalid loop depth: {error}")
                })?,
            ),
            None => (route_and_depth, 0),
        };
        let site = sites
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("allocation route site {id} is absent from manifest"))?;
        match route.expect("route checked above") {
            RouteFilter::Update => {
                site.update_chain = Some(chain);
                site.update_callers = Some(callers);
                site.update_loop_depth = Some(loop_depth);
            }
            RouteFilter::Draw => {
                site.draw_chain = Some(chain);
                site.draw_callers = Some(callers);
                site.draw_loop_depth = Some(loop_depth);
            }
            RouteFilter::Coroutine => {
                site.coroutine_chain = Some(chain);
                site.coroutine_callers = Some(callers);
                site.coroutine_loop_depth = Some(loop_depth);
            }
            _ => unreachable!(),
        }
    }
    if sites.is_empty() {
        anyhow::bail!("allocation manifest contains no emitted sites");
    }
    Ok(sites)
}

fn parse_activation(activation: &str) -> Result<Activation<'_>> {
    let mut lines = activation.lines();
    let frame_aware = match lines.next() {
        Some("rock.allocation-activation.v1") => false,
        Some("rock.allocation-activation.v2") => true,
        _ => anyhow::bail!("allocation activation has the wrong or missing format header"),
    };
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
        let fields = line.split('\t').collect::<Vec<_>>();
        let expected = if frame_aware { 6 } else { 2 };
        if fields.len() != expected {
            anyhow::bail!("malformed allocation activation row `{line}`");
        }
        let id = fields[0];
        if !site_id(id) {
            anyhow::bail!("malformed allocation site ID `{id}`");
        }
        if !seen.insert(id) {
            anyhow::bail!("allocation activation repeats site {id}");
        }
        let total_hits = fields[1].parse::<u64>()?;
        let (frame_hits, active_frames, max_frame_hits, outside_hits) = if frame_aware {
            let frame_hits = fields[2].parse::<u64>()?;
            let active_frames = fields[3].parse::<u64>()?;
            let max_frame_hits = fields[4].parse::<u64>()?;
            let outside_hits = fields[5].parse::<u64>()?;
            if frame_hits + outside_hits != total_hits {
                anyhow::bail!(
                    "allocation activation site {id} has {total_hits} total hits but {} classified hits",
                    frame_hits + outside_hits
                );
            }
            if active_frames > frames {
                anyhow::bail!(
                    "allocation activation site {id} is active on {active_frames} of {frames} frames"
                );
            }
            if (active_frames == 0) != (frame_hits == 0) {
                anyhow::bail!(
                    "allocation activation site {id} has inconsistent frame hits and active frames"
                );
            }
            if max_frame_hits > frame_hits || (frame_hits > 0 && max_frame_hits == 0) {
                anyhow::bail!(
                    "allocation activation site {id} has invalid maximum {max_frame_hits} for {frame_hits} frame hits"
                );
            }
            (
                frame_hits,
                Some(active_frames),
                Some(max_frame_hits),
                Some(outside_hits),
            )
        } else {
            (total_hits, None, None, None)
        };
        hits.push(ActivationHit {
            id,
            total_hits,
            frame_hits,
            active_frames,
            max_frame_hits,
            outside_hits,
        });
    }
    if hits.len() != declared {
        anyhow::bail!(
            "allocation activation declares {declared} sites but contains {} rows",
            hits.len()
        );
    }
    Ok(Activation {
        frames,
        frame_aware,
        hits,
    })
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
    let activation = parse_activation(activation)?;
    let frames = activation.frames;
    let frame_aware = activation.frame_aware;
    let mut hits = activation.hits;
    for hit in &hits {
        if !sites.contains_key(hit.id) {
            anyhow::bail!(
                "active site {} is absent from the allocation manifest",
                hit.id
            );
        }
    }
    hits.retain(|hit| {
        let site = &sites[hit.id];
        match filter {
            RouteFilter::All => true,
            RouteFilter::Update => site.update_chain.is_some(),
            RouteFilter::Draw => site.draw_chain.is_some(),
            RouteFilter::Coroutine => site.coroutine_chain.is_some(),
            RouteFilter::Outside => {
                site.update_chain.is_none()
                    && site.draw_chain.is_none()
                    && site.coroutine_chain.is_none()
            }
        }
    });
    hits.sort_by(|a, b| b.frame_hits.cmp(&a.frame_hits).then_with(|| a.id.cmp(b.id)));

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
        if jit.is_some() && frame_aware {
            "rock.allocation-join.v13"
        } else if jit.is_some() {
            "rock.allocation-join.v11"
        } else if frame_aware {
            "rock.allocation-join.v12"
        } else {
            "rock.allocation-join.v10"
        }
    )?;
    writeln!(out, "frames\t{frames}")?;

    writeln!(out, "filter\t{}", filter.as_str())?;
    writeln!(out, "active_sites\t{}", hits.len())?;
    let base_header = if frame_aware {
        "hits/frame\thits\tframe_hits\tactive_frames\tframe_coverage\thits/active_frame\tmax_hits/frame\toutside_hits\troute\tstructural_loop_depth\tkind\tresult_use\tresult_uses\taggregate_type\taggregate_fields\tsource\tbody\tlua_body_line\tlua_artifact_line\tcall_chain\tdirect_caller_count\tdirect_callers\tid"
    } else {
        "hits/frame\thits\troute\tstructural_loop_depth\tkind\tresult_use\tresult_uses\taggregate_type\taggregate_fields\tsource\tbody\tlua_body_line\tlua_artifact_line\tcall_chain\tdirect_caller_count\tdirect_callers\tid"
    };
    if jit.is_some() {
        writeln!(
            out,
            "{base_header}\tjit_state\tjit_class\tjit_traces\tjit_ir\tjit_ops"
        )?;
    } else {
        writeln!(out, "{base_header}")?;
    }
    for hit in hits {
        let id = hit.id;
        let site = &sites[id];
        let routes = [
            ("update", site.update_chain),
            ("draw", site.draw_chain),
            ("coroutine", site.coroutine_chain),
        ]
        .into_iter()
        .filter_map(|(name, chain)| chain.map(|chain| (name, chain)))
        .collect::<Vec<_>>();
        let route = if routes.is_empty() {
            "outside".to_string()
        } else {
            routes
                .iter()
                .map(|(name, _)| *name)
                .collect::<Vec<_>>()
                .join("+")
        };
        let chain = if routes.len() == 1 {
            routes[0].1.to_string()
        } else {
            routes
                .iter()
                .map(|(name, chain)| format!("{name}:{chain}"))
                .collect::<Vec<_>>()
                .join(" | ")
        };
        let callers = [
            site.update_callers,
            site.draw_callers,
            site.coroutine_callers,
        ]
        .into_iter()
        .flatten()
        .flat_map(|callers| callers.split(" | "))
        .filter(|caller| !caller.is_empty())
        .collect::<BTreeSet<_>>();
        let caller_count = callers.len();
        let callers = callers.into_iter().collect::<Vec<_>>().join(" | ");
        let structural_loop_depth = [site.local_loop_depth]
            .into_iter()
            .chain(site.update_loop_depth)
            .chain(site.draw_loop_depth)
            .chain(site.coroutine_loop_depth)
            .max()
            .unwrap_or(0);
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
        if frame_aware {
            let active_frames = hit.active_frames.unwrap_or(0);
            let coverage = active_frames as f64 / frames as f64;
            let per_active = if active_frames == 0 {
                0.0
            } else {
                hit.frame_hits as f64 / active_frames as f64
            };
            write!(
                out,
                "{:.4}\t{}\t{}\t{}\t{:.4}\t{:.4}\t{}\t{}\t",
                hit.frame_hits as f64 / frames as f64,
                hit.total_hits,
                hit.frame_hits,
                active_frames,
                coverage,
                per_active,
                hit.max_frame_hits.unwrap_or(0),
                hit.outside_hits.unwrap_or(0),
            )?;
        } else {
            write!(
                out,
                "{:.4}\t{}\t",
                hit.frame_hits as f64 / frames as f64,
                hit.total_hits
            )?;
        }
        write!(
            out,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            route,
            structural_loop_depth,
            site.kind,
            site.result_use,
            site.result_uses,
            site.aggregate_type,
            site.aggregate_fields,
            site.source,
            site.body,
            site.lua_body_line
                .map_or_else(|| "?".to_string(), |line| line.to_string()),
            artifact_line,
            chain,
            caller_count,
            callers,
            id,
        )?;
        if let Some(jit) = &jit {
            let key = (
                site.body.to_string(),
                site.lua_body_line.expect("JIT manifest checked above"),
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
  bbbbbbbbbbbbbbbb vec-construction at game.rock:20 via draw_root -> draw_leaf	callers	draw_left | draw_right
"#;

    const ACTIVATION: &str = r#"rock.allocation-activation.v1
frames	4
sites	3
aaaaaaaaaaaaaaaa	8
bbbbbbbbbbbbbbbb	20
cccccccccccccccc	2
"#;

    const FRAME_ACTIVATION: &str = r#"rock.allocation-activation.v2
frames	4
sites	3
aaaaaaaaaaaaaaaa	8	8	4	2	0
bbbbbbbbbbbbbbbb	20	18	3	9	2
cccccccccccccccc	2	1	1	1	1
"#;

    const CLASSIFIED_MANIFEST: &str = r#"emitted allocation-capable Lua report: 1 bodies, 1 direct sites

emitted site manifest: 1 sites
  bbbbbbbbbbbbbbbb	struct-construction	game.rock:20	draw_leaf	2	0	direct-call	1	Carrier	3

route frame.draw: 1 roots, 1 reachable bodies, 1 allocation-capable sites, 0 opaque boundaries
  bbbbbbbbbbbbbbbb struct-construction at game.rock:20 via draw_root -> draw_leaf	callers	draw_root
"#;

    const CLASSIFIED_ACTIVATION: &str = r#"rock.allocation-activation.v2
frames	4
sites	1
bbbbbbbbbbbbbbbb	20	18	3	9	2
"#;

    #[test]
    fn join_preserves_result_use_and_aggregate_shape_classification() {
        let report = render_joined_report(
            CLASSIFIED_MANIFEST,
            CLASSIFIED_ACTIVATION,
            Some("draw"),
            None,
        )
        .expect("joined classified allocation report");
        let mut lines = report.lines().skip(4);
        let header = lines
            .next()
            .expect("joined header")
            .split('\t')
            .collect::<Vec<_>>();
        let row = lines
            .next()
            .expect("joined row")
            .split('\t')
            .collect::<Vec<_>>();
        let value = |name: &str| {
            let index = header
                .iter()
                .position(|column| *column == name)
                .unwrap_or_else(|| panic!("missing {name} column: {report}"));
            row[index]
        };

        assert_eq!(value("result_use"), "direct-call");
        assert_eq!(value("result_uses"), "1");
        assert_eq!(value("aggregate_type"), "Carrier");
        assert_eq!(value("aggregate_fields"), "3");
    }

    #[test]
    fn join_can_isolate_coroutine_resume_routes() {
        let manifest = format!(
            "{MANIFEST}\nroute coroutine.resume: 1 roots, 1 reachable bodies, 1 allocation-capable sites, 0 opaque boundaries\n  cccccccccccccccc struct-construction at game.rock:30 via worker__step\tloop_depth\t1\tcallers\t\n"
        );
        let report = render_joined_report(&manifest, FRAME_ACTIVATION, Some("coroutine"), None)
            .expect("joined coroutine route");
        assert!(report.contains("filter\tcoroutine"));
        assert!(report.contains("0.2500\t2\t1\t1\t0.2500\t1.0000\t1\t1\tcoroutine\t1"));
        assert!(report.contains("worker__step"));
        assert!(!report.contains("update_leaf"));
        assert!(!report.contains("draw_leaf"));
    }

    #[test]
    fn join_filters_by_route_and_ranks_executions_per_frame() {
        let report = render_joined_report(MANIFEST, ACTIVATION, Some("draw"), None)
            .expect("joined allocation report");

        assert!(report.contains("filter\tdraw"));
        assert!(report.contains("5.0000\t20\tdraw\t0\tvec-construction"));
        assert!(report.contains("draw_leaf\t2\t?\tdraw_root -> draw_leaf"));
        assert!(report.contains("draw_root -> draw_leaf"));
        assert!(!report.contains("update_leaf"));
        assert!(!report.contains("outside_leaf"));
    }

    #[test]
    fn join_reports_frame_coverage_multiplicity_and_outside_hits() {
        let report = render_joined_report(MANIFEST, FRAME_ACTIVATION, Some("draw"), None)
            .expect("joined frame-aware activation report");
        let mut lines = report.lines().skip(4);
        let header = lines
            .next()
            .expect("joined header")
            .split('\t')
            .collect::<Vec<_>>();
        let row = lines
            .next()
            .expect("joined row")
            .split('\t')
            .collect::<Vec<_>>();
        let value = |name: &str| {
            let index = header
                .iter()
                .position(|column| *column == name)
                .unwrap_or_else(|| panic!("missing {name} column: {report}"));
            row[index]
        };

        assert_eq!(value("hits/frame"), "4.5000");
        assert_eq!(value("frame_hits"), "18");
        assert_eq!(value("active_frames"), "3");
        assert_eq!(value("frame_coverage"), "0.7500");
        assert_eq!(value("hits/active_frame"), "6.0000");
        assert_eq!(value("max_hits/frame"), "9");
        assert_eq!(value("outside_hits"), "2");
    }

    #[test]
    fn join_preserves_structural_loop_amplification() {
        let manifest = MANIFEST.replace(
            "via draw_root -> draw_leaf\tcallers",
            "via draw_root -> draw_leaf\tloop_depth\t2\tcallers",
        );
        let report = render_joined_report(&manifest, FRAME_ACTIVATION, Some("draw"), None)
            .expect("joined loop-aware allocation report");
        let mut lines = report.lines().skip(4);
        let header = lines
            .next()
            .expect("joined header")
            .split('\t')
            .collect::<Vec<_>>();
        let row = lines
            .next()
            .expect("joined row")
            .split('\t')
            .collect::<Vec<_>>();
        let depth = header
            .iter()
            .position(|column| *column == "structural_loop_depth")
            .expect("structural loop depth column");

        assert_eq!(row[depth], "2", "{report}");
    }

    #[test]
    fn join_exposes_every_direct_caller_beside_the_shortest_chain() {
        let report = render_joined_report(MANIFEST, ACTIVATION, Some("draw"), None)
            .expect("joined caller-aware allocation report");
        let mut lines = report.lines().skip(4);
        let header = lines
            .next()
            .expect("joined header")
            .split('\t')
            .collect::<Vec<_>>();
        let caller_count = header
            .iter()
            .position(|column| *column == "direct_caller_count")
            .expect("direct caller count column");
        let callers = header
            .iter()
            .position(|column| *column == "direct_callers")
            .expect("direct callers column");
        let row = lines
            .next()
            .expect("joined row")
            .split('\t')
            .collect::<Vec<_>>();

        assert_eq!(row.len(), header.len(), "joined row must match its schema");
        assert_eq!(row[caller_count], "2", "{report}");
        assert_eq!(row[callers], "draw_left | draw_right");
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
    fn join_resolves_callee_bound_body_and_jit_lines() {
        let lua = "-- runtime preamble\n__rock_mk_draw_leaf = function(callee) return function()\n  local v = __rock_alloc_hit(\"bbbbbbbbbbbbbbbb\", callee())\nend end\ndraw_leaf = __rock_mk_draw_leaf(callee)\n";
        let trace = "rock.jit-allocation-trace.v1\nartifact\t/generated/main.lua\ntrace\tir\tstate\top\tbody_start\tlua_line\n1\t4\tsunk\tTNEW\t2\t3\n";
        let report = render_joined_report_with_jit(
            MANIFEST,
            ACTIVATION,
            Some("draw"),
            Some(lua),
            Some(trace),
        )
        .expect("callee-bound allocation join");
        assert!(
            report.contains("draw_leaf\t2\t3\tdraw_root -> draw_leaf"),
            "{report}"
        );
        assert!(report.contains("sunk\tnone\t1\t1\tTNEW"), "{report}");
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

        assert!(report.starts_with("rock.allocation-join.v11\n"));
        assert!(report.contains("jit_state\tjit_class\tjit_traces\tjit_ir\tjit_ops"));
        assert!(report.contains("mixed\tobject\t2\t2\tTNEW"));
    }

    #[test]
    fn jit_join_accepts_reused_trace_ids_when_generations_differ() {
        let lua = r#"function draw_leaf()
  local v = __rock_alloc_hit("bbbbbbbbbbbbbbbb", __rock_vec_new())
end
"#;
        let trace = r#"rock.jit-allocation-trace.v2
artifact	/generated/main.lua
trace_generation	trace	ir	state	op	body_start	lua_line
0	1	4	sunk	TNEW	1	2
1	1	4	retained	TNEW	1	2
"#;
        let report = render_joined_report_with_jit(
            MANIFEST,
            ACTIVATION,
            Some("draw"),
            Some(lua),
            Some(trace),
        )
        .expect("joined generation-aware JIT allocation evidence");

        assert!(report.contains("mixed\tobject\t2\t2\tTNEW"), "{report}");
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

        assert!(
            error
                .to_string()
                .contains("five-column allocation manifest")
        );
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
        assert!(report.contains("0.5000\t2\toutside\t0\tstruct-construction"));
    }
}
