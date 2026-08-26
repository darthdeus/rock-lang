use anyhow::{bail, Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::Path;

#[derive(Debug, Clone, Default)]
struct ReplayInput {
    keys: Vec<String>,
    buttons: Vec<i64>,
}

#[derive(Debug, Clone)]
struct ReplayTiming {
    ordinal: usize,
    total_us: u64,
    logic_us: u64,
    render_us: u64,
    present_us: u64,
}

#[derive(Debug, Clone)]
pub struct RecordedSection {
    pub name: String,
    pub inclusive_us: u64,
    pub calls: u64,
}

#[derive(Debug, Clone)]
pub struct RecordedProfile {
    pub heap_bytes: u64,
    pub heap_growth_bytes: u64,
    pub gc: bool,
    pub dropped: u64,
    pub sections: Vec<RecordedSection>,
}

#[derive(Debug)]
pub struct ReplayReport {
    version: u32,
    seed: Option<i64>,
    game: String,
    source: String,
    build: String,
    inputs: Vec<ReplayInput>,
    timings: Vec<ReplayTiming>,
    profiles: HashMap<usize, RecordedProfile>,
}

#[derive(Debug)]
pub struct TopFrame {
    pub ordinal: usize,
    pub total_us: u64,
    pub logic_us: u64,
    pub render_us: u64,
    pub present_us: u64,
    pub dominant_phase: &'static str,
    pub range: (usize, usize),
    pub pressed_keys: Vec<String>,
    pub released_keys: Vec<String>,
    pub pressed_buttons: Vec<i64>,
    pub released_buttons: Vec<i64>,
    pub profile: Option<RecordedProfile>,
}

#[derive(Debug)]
pub struct SlowCluster {
    pub start: usize,
    pub end: usize,
    pub frames: usize,
    pub peak_us: u64,
}

#[derive(Debug)]
pub struct ReplayAnalysis {
    pub version: u32,
    pub seed: Option<i64>,
    pub game: String,
    pub source: String,
    pub build: String,
    pub observation_count: usize,
    pub timing_count: usize,
    pub p50_us: u64,
    pub p95_us: u64,
    pub p99_us: u64,
    pub slow_threshold_us: u64,
    pub top: Vec<TopFrame>,
    pub clusters: Vec<SlowCluster>,
}

fn parse_u64(value: &str, line: usize, field: &str) -> Result<u64> {
    value.parse::<u64>().with_context(|| format!("line {line}: invalid {field} `{value}`"))
}

fn decode_hex(value: &str, line: usize, field: &str) -> Result<String> {
    if value.is_empty() || value.len() % 2 != 0 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("line {line}: malformed {field} encoding");
    }
    let mut bytes = Vec::with_capacity(value.len() / 2);
    for offset in (0..value.len()).step_by(2) {
        bytes.push(u8::from_str_radix(&value[offset..offset + 2], 16)?);
    }
    String::from_utf8(bytes).with_context(|| format!("line {line}: {field} is not UTF-8"))
}

fn parse_list<T>(value: &str, parse: impl Fn(&str) -> Result<T>) -> Result<Vec<T>> {
    if value == "-" {
        return Ok(Vec::new());
    }
    value.split(',').map(parse).collect()
}

impl ReplayReport {
    pub fn parse(text: &str) -> Result<Self> {
        let mut lines = text.lines();
        let first = lines.next().context("empty replay")?;
        let version = first.strip_prefix("STONE_REPLAY ")
            .context("not a replay log")?
            .parse::<u32>()
            .context("malformed replay version")?;
        let mut report = Self {
            version,
            seed: None,
            game: String::new(),
            source: String::new(),
            build: String::new(),
            inputs: Vec::new(),
            timings: Vec::new(),
            profiles: HashMap::new(),
        };
        let mut timing_ordinals = HashSet::new();

        for (offset, line) in lines.enumerate() {
            let line_number = offset + 2;
            let fields = line.split_whitespace().collect::<Vec<_>>();
            let Some(tag) = fields.first().copied() else { continue };
            match tag {
                "seed" if fields.len() == 2 => {
                    report.seed = Some(fields[1].parse().with_context(|| {
                        format!("line {line_number}: malformed seed")
                    })?);
                }
                "game" => report.game = line.strip_prefix("game ").unwrap_or("").to_string(),
                "source" if fields.len() == 2 => {
                    report.source = decode_hex(fields[1], line_number, "replay source")?;
                }
                "build" if fields.len() == 2 => report.build = fields[1].to_string(),
                "t" => {
                    if fields.len() < 8 {
                        bail!("line {line_number}: malformed replay observation");
                    }
                    report.inputs.push(ReplayInput {
                        keys: parse_list(fields[6], |value| Ok(value.to_string()))?,
                        buttons: parse_list(fields[7], |value| {
                            value.parse::<i64>().with_context(|| {
                                format!("line {line_number}: invalid mouse button `{value}`")
                            })
                        })?,
                    });
                }
                "p" => {
                    if fields.len() != 7 {
                        bail!("line {line_number}: malformed phase timing row");
                    }
                    let ordinal = parse_u64(fields[1], line_number, "ordinal")? as usize;
                    let tick = parse_u64(fields[2], line_number, "tick")? as usize;
                    if ordinal != tick {
                        bail!("line {line_number}: timing ordinal {ordinal} does not match tick {tick}");
                    }
                    if ordinal >= report.inputs.len() {
                        bail!("line {line_number}: timing row {ordinal} has no observation");
                    }
                    if !timing_ordinals.insert(ordinal) {
                        bail!("line {line_number}: duplicate timing row for observation {ordinal}");
                    }
                    report.timings.push(ReplayTiming {
                        ordinal,
                        total_us: parse_u64(fields[3], line_number, "total time")?,
                        logic_us: parse_u64(fields[4], line_number, "logic time")?,
                        render_us: parse_u64(fields[5], line_number, "render time")?,
                        present_us: parse_u64(fields[6], line_number, "present time")?,
                    });
                }
                "s" => {
                    if fields.len() < 11 {
                        bail!("line {line_number}: malformed recorded slow profile");
                    }
                    let ordinal = parse_u64(fields[1], line_number, "profile ordinal")? as usize;
                    let tick = parse_u64(fields[2], line_number, "profile tick")? as usize;
                    let section_count = parse_u64(fields[7], line_number, "profile section count")? as usize;
                    if ordinal != tick || !timing_ordinals.contains(&ordinal) {
                        bail!("line {line_number}: slow profile does not follow timing observation {ordinal}");
                    }
                    if section_count == 0 || section_count > 12
                            || fields.len() != 8 + section_count * 3 {
                        bail!("line {line_number}: malformed recorded section population");
                    }
                    let mut sections = Vec::with_capacity(section_count);
                    let mut names = HashSet::new();
                    for section in fields[8..].chunks_exact(3) {
                        let name = decode_hex(section[0], line_number, "section name")?;
                        let inclusive_us = parse_u64(section[1], line_number, "section time")?;
                        let calls = parse_u64(section[2], line_number, "section calls")?;
                        if name.is_empty() || calls == 0 || !names.insert(name.clone()) {
                            bail!("line {line_number}: invalid or duplicate recorded section `{name}`");
                        }
                        sections.push(RecordedSection { name, inclusive_us, calls });
                    }
                    let gc = match fields[5] {
                        "0" => false,
                        "1" => true,
                        value => bail!("line {line_number}: invalid GC flag `{value}`"),
                    };
                    let profile = RecordedProfile {
                        heap_bytes: parse_u64(fields[3], line_number, "heap bytes")?,
                        heap_growth_bytes: parse_u64(fields[4], line_number, "heap growth bytes")?,
                        gc,
                        dropped: parse_u64(fields[6], line_number, "dropped events")?,
                        sections,
                    };
                    if report.profiles.insert(ordinal, profile).is_some() {
                        bail!("line {line_number}: duplicate slow profile for observation {ordinal}");
                    }
                }
                _ => {}
            }
        }

        if report.version >= 12 && report.source.is_empty() {
            bail!("v12 replay is missing its project source");
        }
        Ok(report)
    }

    pub fn from_path(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read replay {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("parse replay {}", path.display()))
    }

    pub fn analyze(&self, top_count: usize, padding: usize) -> Result<ReplayAnalysis> {
        if self.timings.is_empty() {
            bail!("replay contains no recorded timing rows");
        }
        let mut totals = self.timings.iter().map(|row| row.total_us).collect::<Vec<_>>();
        totals.sort_unstable();
        let percentile = |numerator: usize| {
            let rank = (totals.len() * numerator).div_ceil(100).max(1);
            totals[rank - 1]
        };
        let p50_us = percentile(50);
        let p95_us = percentile(95);
        let p99_us = percentile(99);
        let slow_threshold_us = (p50_us.saturating_mul(2)).max(16_667);

        let mut ranked = self.timings.clone();
        ranked.sort_by_key(|row| (std::cmp::Reverse(row.total_us), row.ordinal));
        let top = ranked.into_iter().take(top_count).map(|row| {
            let current = &self.inputs[row.ordinal];
            let previous = row.ordinal.checked_sub(1).and_then(|i| self.inputs.get(i));
            let previous_keys = previous.map(|input| input.keys.as_slice()).unwrap_or(&[]);
            let previous_buttons = previous.map(|input| input.buttons.as_slice()).unwrap_or(&[]);
            let pressed_keys = difference(&current.keys, previous_keys);
            let released_keys = difference(previous_keys, &current.keys);
            let pressed_buttons = difference(&current.buttons, previous_buttons);
            let released_buttons = difference(previous_buttons, &current.buttons);
            let phases = [
                ("logic", row.logic_us),
                ("render", row.render_us),
                ("present", row.present_us),
                ("runner", row.total_us.saturating_sub(
                    row.logic_us.saturating_add(row.render_us).saturating_add(row.present_us))),
            ];
            let dominant_phase = phases.into_iter().max_by_key(|(_, us)| *us).unwrap().0;
            TopFrame {
                ordinal: row.ordinal,
                total_us: row.total_us,
                logic_us: row.logic_us,
                render_us: row.render_us,
                present_us: row.present_us,
                dominant_phase,
                range: (
                    row.ordinal.saturating_sub(padding),
                    row.ordinal.saturating_add(padding).min(self.inputs.len().saturating_sub(1)),
                ),
                pressed_keys,
                released_keys,
                pressed_buttons,
                released_buttons,
                profile: self.profiles.get(&row.ordinal).cloned(),
            }
        }).collect();

        let mut slow = self.timings.iter()
            .filter(|row| row.total_us >= slow_threshold_us)
            .cloned()
            .collect::<Vec<_>>();
        slow.sort_by_key(|row| row.ordinal);
        let mut clusters: Vec<SlowCluster> = Vec::new();
        for row in slow {
            if let Some(cluster) = clusters.last_mut().filter(|cluster| cluster.end + 1 == row.ordinal) {
                cluster.end = row.ordinal;
                cluster.frames += 1;
                cluster.peak_us = cluster.peak_us.max(row.total_us);
            } else {
                clusters.push(SlowCluster {
                    start: row.ordinal,
                    end: row.ordinal,
                    frames: 1,
                    peak_us: row.total_us,
                });
            }
        }

        Ok(ReplayAnalysis {
            version: self.version,
            seed: self.seed,
            game: self.game.clone(),
            source: self.source.clone(),
            build: self.build.clone(),
            observation_count: self.inputs.len(),
            timing_count: self.timings.len(),
            p50_us,
            p95_us,
            p99_us,
            slow_threshold_us,
            top,
            clusters,
        })
    }
}

fn difference<T: Clone + Eq + std::hash::Hash>(left: &[T], right: &[T]) -> Vec<T> {
    let excluded = right.iter().collect::<HashSet<_>>();
    left.iter().filter(|value| !excluded.contains(value)).cloned().collect()
}

impl ReplayAnalysis {
    pub fn render_json(&self) -> Result<String> {
        let top = self.top.iter().map(|row| serde_json::json!({
            "ordinal": row.ordinal,
            "total_us": row.total_us,
            "logic_us": row.logic_us,
            "render_us": row.render_us,
            "present_us": row.present_us,
            "dominant_phase": row.dominant_phase,
            "suggested_range": format!("{}-{}", row.range.0, row.range.1),
            "input": {
                "pressed_keys": row.pressed_keys,
                "released_keys": row.released_keys,
                "pressed_buttons": row.pressed_buttons,
                "released_buttons": row.released_buttons,
            },
            "recorded_profile": row.profile.as_ref().map(|profile| serde_json::json!({
                "heap_bytes": profile.heap_bytes,
                "heap_growth_bytes": profile.heap_growth_bytes,
                "gc": profile.gc,
                "dropped": profile.dropped,
                "sections": profile.sections.iter().map(|section| serde_json::json!({
                    "name": section.name,
                    "inclusive_us": section.inclusive_us,
                    "calls": section.calls,
                })).collect::<Vec<_>>(),
            })),
        })).collect::<Vec<_>>();
        let clusters = self.clusters.iter().map(|cluster| serde_json::json!({
            "start": cluster.start,
            "end": cluster.end,
            "frames": cluster.frames,
            "peak_us": cluster.peak_us,
        })).collect::<Vec<_>>();
        Ok(serde_json::to_string_pretty(&serde_json::json!({
            "format": "rock.replay-report.v1",
            "replay": {
                "version": self.version,
                "seed": self.seed,
                "game": self.game,
                "source": self.source,
                "build": self.build,
                "observations": self.observation_count,
                "timing_rows": self.timing_count,
            },
            "percentiles_us": { "p50": self.p50_us, "p95": self.p95_us, "p99": self.p99_us },
            "slow_threshold_us": self.slow_threshold_us,
            "top_frames": top,
            "slow_clusters": clusters,
        }))?)
    }

    fn render_text(&self) -> String {
        let mut out = format!(
            "Replay: {} [{}] build {} (v{}, seed {})\nObservations: {} | timings: {} | p50 {:.2} ms | p95 {:.2} ms | p99 {:.2} ms\nSlow burst threshold: {:.2} ms (max(2 x p50, 16.667 ms))\n\nTop frames:\n",
            self.source,
            self.game,
            self.build,
            self.version,
            self.seed.map(|seed| seed.to_string()).unwrap_or_else(|| "?".to_string()),
            self.observation_count,
            self.timing_count,
            self.p50_us as f64 / 1000.0,
            self.p95_us as f64 / 1000.0,
            self.p99_us as f64 / 1000.0,
            self.slow_threshold_us as f64 / 1000.0,
        );
        for row in &self.top {
            let mut changes = Vec::new();
            if !row.pressed_keys.is_empty() { changes.push(format!("+key {}", row.pressed_keys.join(","))); }
            if !row.released_keys.is_empty() { changes.push(format!("-key {}", row.released_keys.join(","))); }
            if !row.pressed_buttons.is_empty() { changes.push(format!("+button {:?}", row.pressed_buttons)); }
            if !row.released_buttons.is_empty() { changes.push(format!("-button {:?}", row.released_buttons)); }
            out.push_str(&format!(
                "  #{:<6} {:>8.2} ms  {:<7} logic {:>7.2} render {:>7.2} present {:>7.2}  range {}-{}{}\n",
                row.ordinal,
                row.total_us as f64 / 1000.0,
                row.dominant_phase,
                row.logic_us as f64 / 1000.0,
                row.render_us as f64 / 1000.0,
                row.present_us as f64 / 1000.0,
                row.range.0,
                row.range.1,
                if changes.is_empty() { String::new() } else { format!("  {}", changes.join(" ")) },
            ));
            if let Some(profile) = &row.profile {
                out.push_str(&format!(
                    "           recorded heap {} B ({:+} B), GC {}, dropped {}\n",
                    profile.heap_bytes,
                    profile.heap_growth_bytes as i128,
                    if profile.gc { "yes" } else { "no" },
                    profile.dropped,
                ));
                for section in &profile.sections {
                    out.push_str(&format!(
                        "             {:<36} {:>7.2} ms x{}\n",
                        section.name,
                        section.inclusive_us as f64 / 1000.0,
                        section.calls,
                    ));
                }
            }
        }
        out.push_str("\nSlow clusters:\n");
        if self.clusters.is_empty() {
            out.push_str("  none\n");
        } else {
            for cluster in &self.clusters {
                out.push_str(&format!(
                    "  {}-{}  {} frame(s), peak {:.2} ms\n",
                    cluster.start, cluster.end, cluster.frames, cluster.peak_us as f64 / 1000.0,
                ));
            }
        }
        out
    }
}

pub fn run(path: &Path, top: usize, padding: usize, format: &str) -> Result<()> {
    if top == 0 { bail!("--top must be greater than zero") }
    let analysis = ReplayReport::from_path(path)?.analyze(top, padding)?;
    match format {
        "pretty" => print!("{}", analysis.render_text()),
        "json" => println!("{}", analysis.render_json()?),
        other => bail!("replay-report format must be `pretty` or `json`, got `{other}`"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = "STONE_REPLAY 12\n\
seed 42\n\
window 0x1p+10 0x1p+9\n\
game MAG\n\
source 6d6167\n\
build 0123456789abcdef\n\
t 0x1p-6 0x0p+0 0x0p+0 0 0 - - - -\n\
p 0 0 10000 3000 6000 1000\n\
t 0x1p-6 0x0p+0 0x0p+0 0 0 W - - -\n\
p 1 1 40000 28000 10000 2000\n\
t 0x1p-6 0x0p+0 0x0p+0 0 0 W 1 - -\n\
p 2 2 50000 12000 35000 3000\n\
s 2 2 1000 200 0 0 1 64726177 30000 4\n\
t 0x1p-6 0x0p+0 0x0p+0 0 0 - - - -\n\
p 3 3 11000 4000 6000 1000";

    #[test]
    fn report_ranks_frames_clusters_suggested_ranges_and_input_changes() {
        let replay = ReplayReport::parse(FIXTURE).expect("parse fixture");
        let report = replay.analyze(2, 1).expect("analyze fixture");

        assert_eq!(report.source, "mag");
        assert_eq!(report.timing_count, 4);
        assert_eq!(report.top.len(), 2);
        assert_eq!(report.top[0].ordinal, 2);
        assert_eq!(report.top[0].dominant_phase, "render");
        assert_eq!(report.top[0].range, (1, 3));
        assert_eq!(report.top[0].pressed_buttons, vec![1]);
        assert_eq!(report.top[0].profile.as_ref().unwrap().sections[0].name, "draw");
        assert_eq!(report.top[1].ordinal, 1);
        assert_eq!(report.top[1].dominant_phase, "logic");
        assert_eq!(report.top[1].pressed_keys, vec!["W"]);
        assert_eq!(report.clusters.len(), 1);
        assert_eq!((report.clusters[0].start, report.clusters[0].end), (1, 2));

        let json = report.render_json().expect("render json");
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["percentiles_us"]["p95"], 50_000);
        assert_eq!(value["top_frames"][0]["suggested_range"], "1-3");
        assert_eq!(value["top_frames"][0]["recorded_profile"]["sections"][0]["name"], "draw");
    }

    #[test]
    fn report_refuses_a_zero_timing_population() {
        let replay = ReplayReport::parse(
            "STONE_REPLAY 12\nseed 1\ngame MAG\nsource 6d6167\nbuild 0123456789abcdef\nt 0 0 0 0 0 - - - -",
        )
        .expect("the replay itself parses");
        let error = replay.analyze(10, 2).unwrap_err().to_string();
        assert!(error.contains("no recorded timing"), "{error}");
    }
}
