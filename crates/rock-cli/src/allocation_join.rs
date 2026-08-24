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
    update_chain: Option<&'a str>,
    draw_chain: Option<&'a str>,
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
            if fields.len() == 4 && site_id(fields[0]) {
                if sites
                    .insert(
                        fields[0],
                        Site {
                            kind: fields[1],
                            source: fields[2],
                            body: fields[3],
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

pub fn render_joined_report(
    manifest: &str,
    activation: &str,
    route: Option<&str>,
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

    let mut out = String::new();
    writeln!(out, "rock.allocation-join.v1")?;
    writeln!(out, "frames\t{frames}")?;
    writeln!(out, "filter\t{}", filter.as_str())?;
    writeln!(out, "active_sites\t{}", hits.len())?;
    writeln!(
        out,
        "hits/frame\thits\troute\tkind\tsource\tbody\tcall_chain\tid"
    )?;
    for (id, count) in hits {
        let site = &sites[id];
        let (route, chain) = match (site.update_chain, site.draw_chain) {
            (Some(update), Some(draw)) => ("update+draw", format!("update:{update} | draw:{draw}")),
            (Some(update), None) => ("update", update.to_string()),
            (None, Some(draw)) => ("draw", draw.to_string()),
            (None, None) => ("outside", String::new()),
        };
        writeln!(
            out,
            "{:.4}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            count as f64 / frames as f64,
            count,
            route,
            site.kind,
            site.source,
            site.body,
            chain,
            id,
        )?;
    }
    Ok(out)
}

pub fn run(files: &[String]) -> Result<()> {
    if !(files.len() == 2 || files.len() == 3) {
        anyhow::bail!(
            "usage: rock allocation-join <manifest> <activation> [all|update|draw|outside]"
        );
    }
    let manifest = std::fs::read_to_string(&files[0]).map_err(|error| {
        anyhow::anyhow!("cannot read allocation manifest {}: {error}", files[0])
    })?;
    let activation = std::fs::read_to_string(&files[1]).map_err(|error| {
        anyhow::anyhow!("cannot read allocation activation {}: {error}", files[1])
    })?;
    let report = render_joined_report(&manifest, &activation, files.get(2).map(String::as_str))?;
    let mut stdout = std::io::stdout().lock();
    match std::io::Write::write_all(&mut stdout, report.as_bytes()) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::render_joined_report;

    const MANIFEST: &str = r#"emitted allocation-capable Lua report: 3 bodies, 3 direct sites

emitted site manifest: 3 sites
  aaaaaaaaaaaaaaaa	string-concat	game.rock:10	update_leaf
  bbbbbbbbbbbbbbbb	vec-construction	game.rock:20	draw_leaf
  cccccccccccccccc	struct-construction	game.rock:30	outside_leaf

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
        let report = render_joined_report(MANIFEST, ACTIVATION, Some("draw"))
            .expect("joined allocation report");

        assert!(report.contains("filter\tdraw"));
        assert!(report.contains("5.0000\t20\tdraw\tvec-construction"));
        assert!(report.contains("draw_root -> draw_leaf"));
        assert!(!report.contains("update_leaf"));
        assert!(!report.contains("outside_leaf"));
    }

    #[test]
    fn join_rejects_an_activation_from_a_different_manifest() {
        let activation = ACTIVATION.replace("aaaaaaaaaaaaaaaa", "dddddddddddddddd");
        let error = render_joined_report(MANIFEST, &activation, None)
            .expect_err("cross-artifact site IDs must reject");

        assert!(
            error
                .to_string()
                .contains("absent from the allocation manifest")
        );
    }

    #[test]
    fn outside_filter_has_a_nonzero_population_and_the_recorded_denominator() {
        let report = render_joined_report(MANIFEST, ACTIVATION, Some("outside"))
            .expect("outside allocation report");

        assert!(report.contains("frames\t4"));
        assert!(report.contains("active_sites\t1"));
        assert!(report.contains("0.5000\t2\toutside\tstruct-construction"));
    }
}
