use anyhow::{bail, Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::Path;

#[derive(Debug, Clone, Default)]
pub(crate) struct ReplayInput {
    pub(crate) mouse_x: String,
    pub(crate) mouse_y: String,
    pub(crate) wheel: i64,
    pub(crate) imgui: bool,
    pub(crate) keys: Vec<String>,
    pub(crate) buttons: Vec<i64>,
    pub(crate) pad_buttons: Vec<String>,
    pub(crate) pad_axes: Vec<(String, String)>,
    pub(crate) input_details: Option<RecordedInputDetails>,
    pub(crate) image_publications: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct RecordedInputDetails {
    pressed_keys: Vec<String>,
    repeated_keys: Vec<String>,
    pressed_buttons: Vec<i64>,
    released_buttons: Vec<i64>,
}

// Mirrors the runtime's v15 input projection schema; reject a corrupt field
// before replay-profile can treat a partially decoded observation as complete.
fn parse_input_details(fields: &[&str], line: usize) -> Result<RecordedInputDetails> {
    if fields.len() != 14 || !matches!(fields[13], "0" | "1") {
        bail!("line {line}: malformed input detail row");
    }
    let mut details = RecordedInputDetails::default();
    for index in 1..=11 {
        let field = fields[index + 1];
        if field == "-" { continue; }
        let mut seen = HashSet::new();
        for entry in field.split(',') {
            let (encoded, value) = entry.split_once('=').with_context(||
                format!("line {line}: malformed input detail entry"))?;
            let key = if index == 3 || index == 4 {
                let key = parse_u64(encoded, line, "input mouse button")?;
                if key == 0 || key > 255 || !encoded.bytes().all(|b| b.is_ascii_digit()) {
                    bail!("line {line}: invalid input mouse button");
                }
                key.to_string()
            } else {
                let key = decode_hex(encoded, line, "input identity")?;
                if key.is_empty() || key.bytes().any(|b| b == 0 || b.is_ascii_whitespace()) {
                    bail!("line {line}: invalid input identity");
                }
                key
            };
            if !seen.insert(key.clone()) { bail!("line {line}: duplicate input identity"); }
            match index {
                8 => {
                    let modifier = parse_u64(value, line, "input modifier")?;
                    if modifier > 65535 || !value.bytes().all(|b| b.is_ascii_digit()) {
                        bail!("line {line}: invalid input modifier");
                    }
                }
                11 => {
                    let motion: f64 = value.parse().with_context(||
                        format!("line {line}: invalid input axis motion"))?;
                    if !motion.is_finite() || !(-1.0..=1.0).contains(&motion) {
                        bail!("line {line}: invalid input axis motion");
                    }
                }
                _ if value != "1" => bail!("line {line}: invalid input edge flag"),
                _ => {}
            }
            match index {
                1 => details.pressed_keys.push(key),
                2 => details.repeated_keys.push(key),
                3 => details.pressed_buttons.push(key.parse().unwrap()),
                4 => details.released_buttons.push(key.parse().unwrap()),
                _ => {}
            }
        }
    }
    Ok(details)
}

#[derive(Debug, Clone)]
pub(crate) struct ReplayTiming {
    pub(crate) ordinal: usize,
    pub(crate) total_us: u64,
    pub(crate) logic_us: u64,
    pub(crate) render_us: u64,
    pub(crate) present_us: u64,
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

#[derive(Debug, Clone)]
pub struct RecordedImagePath {
    pub path: String,
    pub succeeded: bool,
}

#[derive(Debug, Clone)]
pub struct RecordedImageResources {
    pub load_calls: u64,
    pub load_failures: u64,
    pub decoded_bytes: u64,
    pub load_us: u64,
    pub upload_calls: u64,
    pub upload_bytes: u64,
    pub wait_us: u64,
    pub upload_us: u64,
    pub paths: Vec<RecordedImagePath>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecordedJobDecision {
    pub(crate) tick: usize,
    pub(crate) kind: String,
    pub(crate) key: u64,
    pub(crate) ready: bool,
}

#[derive(Debug)]
pub struct ReplayReport {
    pub(crate) version: u32,
    pub(crate) seed: Option<i64>,
    pub(crate) game: String,
    pub(crate) source: String,
    pub(crate) revision: String,
    pub(crate) build: String,
    /// Recorded desync-checkpoint cadence in ticks; 0 means the recording
    /// carries no checkpoints, so a divergent playback cannot be detected.
    pub(crate) checkpoint_interval: usize,
    pub(crate) inputs: Vec<ReplayInput>,
    pub(crate) timings: Vec<ReplayTiming>,
    pub(crate) profiles: HashMap<usize, RecordedProfile>,
    pub(crate) image_resources: HashMap<usize, RecordedImageResources>,
    pub(crate) job_decisions: Vec<RecordedJobDecision>,
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
    pub image_resources: Option<RecordedImageResources>,
}

#[derive(Debug)]
pub struct SlowCluster {
    pub start: usize,
    pub end: usize,
    pub frames: usize,
    pub peak_us: u64,
}

#[derive(Debug)]
pub struct ImageResourceFrame {
    pub ordinal: usize,
    pub total_us: u64,
    pub resources: RecordedImageResources,
}

#[derive(Debug)]
pub struct RepeatedImageLoad {
    pub path: String,
    pub loads: usize,
    pub failures: usize,
    pub ordinals: Vec<usize>,
}

#[derive(Debug)]
pub struct ReplayAnalysis {
    pub version: u32,
    pub seed: Option<i64>,
    pub game: String,
    pub source: String,
    pub revision: String,
    pub build: String,
    pub observation_count: usize,
    pub timing_count: usize,
    pub p50_us: u64,
    pub p95_us: u64,
    pub p99_us: u64,
    pub slow_threshold_us: u64,
    pub top: Vec<TopFrame>,
    pub clusters: Vec<SlowCluster>,
    pub image_resource_frames: Vec<ImageResourceFrame>,
    pub repeated_image_loads: Vec<RepeatedImageLoad>,
}

fn parse_u64(value: &str, line: usize, field: &str) -> Result<u64> {
    value.parse::<u64>().with_context(|| format!("line {line}: invalid {field} `{value}`"))
}

const MAX_REPLAY_INTEGER: u64 = 9_007_199_254_740_991;

fn parse_positive_key(value: &str, line: usize) -> Result<u64> {
    let key = parse_u64(value, line, "job decision key")?;
    if key == 0 || key > MAX_REPLAY_INTEGER {
        bail!("line {line}: job decision key must be positive and at most {MAX_REPLAY_INTEGER}");
    }
    Ok(key)
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
        if !(1..=rock_replay_protocol::REPLAY_VERSION).contains(&version) {
            bail!("unsupported replay version {version}");
        }
        let mut report = Self {
            version,
            seed: None,
            game: String::new(),
            source: String::new(),
            revision: String::new(),
            build: String::new(),
            checkpoint_interval: 0,
            inputs: Vec::new(),
            timings: Vec::new(),
            profiles: HashMap::new(),
            image_resources: HashMap::new(),
            job_decisions: Vec::new(),
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
                "revision" if fields.len() == 2 => {
                    if !report.revision.is_empty() {
                        bail!("line {line_number}: duplicate replay revision");
                    }
                    report.revision = decode_hex(fields[1], line_number, "replay revision")?;
                }
                "build" if fields.len() == 2 => report.build = fields[1].to_string(),
                "checkpoint" if fields.len() == 2 => {
                    report.checkpoint_interval = usize::try_from(
                        parse_u64(fields[1], line_number, "checkpoint interval")?,
                    )
                    .with_context(|| {
                        format!("line {line_number}: checkpoint interval does not fit usize")
                    })?;
                }
                "t" => {
                    if fields.len() < 8 || fields.len() > 10 {
                        bail!("line {line_number}: malformed replay observation");
                    }
                    let imgui = match fields[5] {
                        "0" => false,
                        "1" => true,
                        value => bail!("line {line_number}: invalid ImGui flag `{value}`"),
                    };
                    let pad_buttons = fields.get(8).copied().unwrap_or("-");
                    let pad_axes = fields.get(9).copied().unwrap_or("-");
                    report.inputs.push(ReplayInput {
                        input_details: None,
                        image_publications: None,
                        mouse_x: fields[2].to_string(),
                        mouse_y: fields[3].to_string(),
                        wheel: fields[4].parse::<i64>().with_context(|| {
                            format!("line {line_number}: invalid mouse wheel `{}`", fields[4])
                        })?,
                        imgui,
                        keys: parse_list(fields[6], |value| Ok(value.to_string()))?,
                        buttons: parse_list(fields[7], |value| {
                            value.parse::<i64>().with_context(|| {
                                format!("line {line_number}: invalid mouse button `{value}`")
                            })
                        })?,
                        pad_buttons: parse_list(pad_buttons, |value| Ok(value.to_string()))?,
                        pad_axes: parse_list(pad_axes, |value| {
                            let (axis, magnitude) = value.split_once('=').with_context(|| {
                                format!("line {line_number}: invalid gamepad axis `{value}`")
                            })?;
                            Ok((axis.to_string(), magnitude.to_string()))
                        })?,
                    });
                }
                "a" => {
                    if report.version < 15 { bail!("line {line_number}: input details require v15"); }
                    let details = parse_input_details(&fields, line_number)?;
                    let tick = parse_u64(fields[1], line_number, "input detail tick")?;
                    if !fields[1].bytes().all(|b| b.is_ascii_digit())
                        || report.inputs.is_empty() || tick != (report.inputs.len() - 1) as u64 {
                        bail!("line {line_number}: orphaned input detail row");
                    }
                    let input = report.inputs.last_mut().unwrap();
                    if input.input_details.replace(details).is_some() {
                        bail!("line {line_number}: duplicate input detail row");
                    }
                }
                "u" => {
                    if report.version < 16 || fields.len() != 3 {
                        bail!("line {line_number}: malformed image publication row");
                    }
                    let tick = parse_u64(fields[1], line_number, "image publication tick")?;
                    if !fields[1].bytes().all(|b| b.is_ascii_digit())
                        || report.inputs.is_empty() || tick != (report.inputs.len() - 1) as u64 {
                        bail!("line {line_number}: orphaned image publication row");
                    }
                    let data = decode_hex(fields[2], line_number, "image publications")?;
                    rock_replay_protocol::validate_image_events(&data)
                        .map_err(|e| anyhow::anyhow!("line {line_number}: {e}"))?;
                    if report.inputs.last_mut().unwrap().image_publications.replace(data).is_some() {
                        bail!("line {line_number}: duplicate image publication row");
                    }
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
                "i" => {
                    if fields.len() < 12 {
                        bail!("line {line_number}: malformed image resource row");
                    }
                    let ordinal = parse_u64(fields[1], line_number, "image ordinal")? as usize;
                    let tick = parse_u64(fields[2], line_number, "image tick")? as usize;
                    let path_count = parse_u64(fields[11], line_number, "image path count")? as usize;
                    if ordinal != tick || !timing_ordinals.contains(&ordinal) {
                        bail!("line {line_number}: image resource row does not follow timing observation {ordinal}");
                    }
                    if fields.len() != 12 + path_count * 2 {
                        bail!("line {line_number}: malformed image path population");
                    }
                    let mut paths = Vec::with_capacity(path_count);
                    for path in fields[12..].chunks_exact(2) {
                        let succeeded = match path[0] {
                            "0" => false,
                            "1" => true,
                            value => bail!("line {line_number}: invalid image success flag `{value}`"),
                        };
                        paths.push(RecordedImagePath {
                            succeeded,
                            path: decode_hex(path[1], line_number, "image path")?,
                        });
                    }
                    let resources = RecordedImageResources {
                        load_calls: parse_u64(fields[3], line_number, "image load calls")?,
                        load_failures: parse_u64(fields[4], line_number, "image load failures")?,
                        decoded_bytes: parse_u64(fields[5], line_number, "decoded image bytes")?,
                        load_us: parse_u64(fields[6], line_number, "image load time")?,
                        upload_calls: parse_u64(fields[7], line_number, "image upload calls")?,
                        upload_bytes: parse_u64(fields[8], line_number, "uploaded image bytes")?,
                        wait_us: parse_u64(fields[9], line_number, "image wait time")?,
                        upload_us: parse_u64(fields[10], line_number, "image upload time")?,
                        paths,
                    };
                    if report.image_resources.insert(ordinal, resources).is_some() {
                        bail!("line {line_number}: duplicate image resource row for observation {ordinal}");
                    }
                }
                "j" => {
                    if report.version < 14 {
                        bail!("line {line_number}: job decision rows require replay version 14");
                    }
                    if fields.len() != 5 {
                        bail!("line {line_number}: malformed job decision row");
                    }
                    let tick = usize::try_from(parse_u64(fields[1], line_number, "job decision tick")?)
                        .with_context(|| format!("line {line_number}: job decision tick does not fit usize"))?;
                    if report.inputs.len().checked_sub(1) != Some(tick) {
                        bail!("line {line_number}: job decision tick {tick} is not the current recorded observation");
                    }
                    let kind = fields[2];
                    if kind.is_empty() || kind.chars().any(char::is_whitespace) {
                        bail!("line {line_number}: job decision kind must be one token");
                    }
                    let key = parse_positive_key(fields[3], line_number)?;
                    let ready = match fields[4] {
                        "0" => false,
                        "1" => true,
                        value => bail!("line {line_number}: invalid job decision readiness `{value}`"),
                    };
                    report.job_decisions.push(RecordedJobDecision {
                        tick,
                        kind: kind.to_string(),
                        key,
                        ready,
                    });
                }
                _ => {}
            }
        }

        if report.version >= 16 && report.inputs.iter().any(|input| input.image_publications.is_none()) {
            bail!("v16 replay is missing image publications");
        }
        if report.version >= 15 && report.inputs.iter().any(|input| input.input_details.is_none()) {
            bail!("v15 replay is missing input details");
        }
        if report.version >= 12 && report.source.is_empty() {
            bail!("v12+ replay is missing its project source");
        }
        if report.version >= 13 && report.revision.is_empty() {
            bail!("v13 replay is missing its source revision");
        }
        Ok(report)
    }

    pub(crate) fn from_path(path: &Path) -> Result<Self> {
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
            let mut pressed_keys = difference(&current.keys, previous_keys);
            let mut released_keys = difference(previous_keys, &current.keys);
            let mut pressed_buttons = difference(&current.buttons, previous_buttons);
            let mut released_buttons = difference(previous_buttons, &current.buttons);
            if let Some(details) = &current.input_details {
                pressed_keys.retain(|key| !details.repeated_keys.contains(key));
                pressed_keys.extend(details.pressed_keys.iter().cloned());
                released_keys.extend(details.pressed_keys.iter()
                    .filter(|key| !current.keys.contains(key)).cloned());
                pressed_buttons.extend(&details.pressed_buttons);
                released_buttons.extend(&details.released_buttons);
                pressed_keys.sort(); pressed_keys.dedup();
                released_keys.sort(); released_keys.dedup();
                pressed_buttons.sort(); pressed_buttons.dedup();
                released_buttons.sort(); released_buttons.dedup();
            }
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
                image_resources: self.image_resources.get(&row.ordinal).cloned(),
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

        let timing_by_ordinal = self.timings.iter()
            .map(|row| (row.ordinal, row.total_us))
            .collect::<HashMap<_, _>>();
        let mut image_resource_frames = self.image_resources.iter()
            .map(|(ordinal, resources)| ImageResourceFrame {
                ordinal: *ordinal,
                total_us: timing_by_ordinal[ordinal],
                resources: resources.clone(),
            })
            .collect::<Vec<_>>();
        image_resource_frames.sort_by_key(|row| row.ordinal);
        let mut loads_by_path: HashMap<String, (usize, usize, Vec<usize>)> = HashMap::new();
        for row in &image_resource_frames {
            for path in &row.resources.paths {
                let entry = loads_by_path.entry(path.path.clone()).or_default();
                entry.0 += 1;
                if !path.succeeded { entry.1 += 1; }
                entry.2.push(row.ordinal);
            }
        }
        let mut repeated_image_loads = loads_by_path.into_iter()
            .filter_map(|(path, (loads, failures, ordinals))| {
                (loads > 1).then_some(RepeatedImageLoad { path, loads, failures, ordinals })
            })
            .collect::<Vec<_>>();
        repeated_image_loads.sort_by(|left, right| {
            right.loads.cmp(&left.loads).then_with(|| left.path.cmp(&right.path))
        });

        Ok(ReplayAnalysis {
            version: self.version,
            seed: self.seed,
            game: self.game.clone(),
            source: self.source.clone(),
            revision: self.revision.clone(),
            build: self.build.clone(),
            observation_count: self.inputs.len(),
            timing_count: self.timings.len(),
            p50_us,
            p95_us,
            p99_us,
            slow_threshold_us,
            top,
            clusters,
            image_resource_frames,
            repeated_image_loads,
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
            "image_resources": row.image_resources.as_ref().map(|resources| serde_json::json!({
                "load_calls": resources.load_calls,
                "load_failures": resources.load_failures,
                "decoded_bytes": resources.decoded_bytes,
                "load_us": resources.load_us,
                "upload_calls": resources.upload_calls,
                "upload_bytes": resources.upload_bytes,
                "wait_us": resources.wait_us,
                "upload_us": resources.upload_us,
                "paths": resources.paths.iter().map(|path| serde_json::json!({
                    "path": path.path,
                    "succeeded": path.succeeded,
                })).collect::<Vec<_>>(),
            })),
        })).collect::<Vec<_>>();
        let clusters = self.clusters.iter().map(|cluster| serde_json::json!({
            "start": cluster.start,
            "end": cluster.end,
            "frames": cluster.frames,
            "peak_us": cluster.peak_us,
        })).collect::<Vec<_>>();
        let image_resource_frames = self.image_resource_frames.iter().map(|row| serde_json::json!({
            "ordinal": row.ordinal,
            "total_us": row.total_us,
            "load_calls": row.resources.load_calls,
            "load_failures": row.resources.load_failures,
            "decoded_bytes": row.resources.decoded_bytes,
            "load_us": row.resources.load_us,
            "upload_calls": row.resources.upload_calls,
            "upload_bytes": row.resources.upload_bytes,
            "wait_us": row.resources.wait_us,
            "upload_us": row.resources.upload_us,
            "paths": row.resources.paths.iter().map(|path| serde_json::json!({
                "path": path.path,
                "succeeded": path.succeeded,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>();
        let repeated_image_loads = self.repeated_image_loads.iter().map(|row| serde_json::json!({
            "path": row.path,
            "loads": row.loads,
            "failures": row.failures,
            "first_ordinal": row.ordinals[0],
            "last_ordinal": row.ordinals[row.ordinals.len() - 1],
            "ordinals": row.ordinals,
        })).collect::<Vec<_>>();
        Ok(serde_json::to_string_pretty(&serde_json::json!({
            "format": "rock.replay-report.v1",
            "replay": {
                "version": self.version,
                "seed": self.seed,
                "game": self.game,
                "source": self.source,
                "revision": self.revision,
                "build": self.build,
                "observations": self.observation_count,
                "timing_rows": self.timing_count,
            },
            "percentiles_us": { "p50": self.p50_us, "p95": self.p95_us, "p99": self.p99_us },
            "slow_threshold_us": self.slow_threshold_us,
            "top_frames": top,
            "slow_clusters": clusters,
            "image_resource_frames": image_resource_frames,
            "repeated_image_loads": repeated_image_loads,
        }))?)
    }

    fn render_text(&self) -> String {
        let mut out = format!(
            "Replay: {} [{}] revision {} build {} (v{}, seed {})\nObservations: {} | timings: {} | p50 {:.2} ms | p95 {:.2} ms | p99 {:.2} ms\nSlow burst threshold: {:.2} ms (max(2 x p50, 16.667 ms))\n\nTop frames:\n",
            self.source,
            self.game,
            if self.revision.is_empty() { "?" } else { &self.revision },
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
            if let Some(resources) = &row.image_resources {
                out.push_str(&format!(
                    "           images load {}/{} ({:.2} MiB, {:.2} ms), upload {} ({:.2} MiB, {:.2} ms), wait {:.2} ms\n",
                    resources.load_calls,
                    resources.load_failures,
                    resources.decoded_bytes as f64 / 1_048_576.0,
                    resources.load_us as f64 / 1000.0,
                    resources.upload_calls,
                    resources.upload_bytes as f64 / 1_048_576.0,
                    resources.upload_us as f64 / 1000.0,
                    resources.wait_us as f64 / 1000.0,
                ));
                for path in &resources.paths {
                    out.push_str(&format!(
                        "             {} {}\n",
                        if path.succeeded { "loaded" } else { "failed" },
                        path.path,
                    ));
                }
            }
        }
        out.push_str("\nRepeated image loads:\n");
        if self.repeated_image_loads.is_empty() {
            out.push_str("  none\n");
        } else {
            for row in &self.repeated_image_loads {
                out.push_str(&format!(
                    "  {} load(s), {} failure(s), ordinals {}: {}\n",
                    row.loads,
                    row.failures,
                    row.ordinals.iter().map(|ordinal| ordinal.to_string())
                        .collect::<Vec<_>>().join(","),
                    row.path,
                ));
            }
        }
        out.push_str("\nImage resource frames:\n");
        if self.image_resource_frames.is_empty() {
            out.push_str("  none\n");
        } else {
            for row in &self.image_resource_frames {
                out.push_str(&format!(
                    "  #{} {:.2} ms: {} load(s), {} failure(s), {:.2} MiB decoded, {:.2} ms load, {:.2} ms wait\n",
                    row.ordinal,
                    row.total_us as f64 / 1000.0,
                    row.resources.load_calls,
                    row.resources.load_failures,
                    row.resources.decoded_bytes as f64 / 1_048_576.0,
                    row.resources.load_us as f64 / 1000.0,
                    row.resources.wait_us as f64 / 1000.0,
                ));
                for path in &row.resources.paths {
                    out.push_str(&format!(
                        "    {} {}\n",
                        if path.succeeded { "loaded" } else { "failed" },
                        path.path,
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
i 2 2 1 0 16777216 1500 1 16777216 9000 2000 1 1 6173736574732f7666782f666c616d65737472696b652e706e67\n\
s 2 2 1000 200 0 0 1 64726177 30000 4\n\
t 0x1p-6 0x0p+0 0x0p+0 0 0 - - - -\n\
p 3 3 11000 4000 6000 1000\n\
i 3 3 1 0 16777216 900 0 0 0 0 1 1 6173736574732f7666782f666c616d65737472696b652e706e67";

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
        assert_eq!(value["top_frames"][0]["image_resources"]["load_calls"], 1);
        assert_eq!(value["top_frames"][0]["image_resources"]["decoded_bytes"], 16_777_216);
        assert_eq!(value["top_frames"][0]["image_resources"]["paths"][0]["path"],
            "assets/vfx/flamestrike.png");
        assert_eq!(value["image_resource_frames"][0]["ordinal"], 2);
        assert_eq!(value["repeated_image_loads"][0]["path"],
            "assets/vfx/flamestrike.png");
        assert_eq!(value["repeated_image_loads"][0]["loads"], 2);
        assert_eq!(value["repeated_image_loads"][0]["first_ordinal"], 2);
        assert_eq!(value["repeated_image_loads"][0]["last_ordinal"], 3);
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
    #[test]
    fn v13_report_requires_the_recorded_source_revision() {
        let error = ReplayReport::parse(
            "STONE_REPLAY 13\nseed 1\ngame MAG\nsource 6d6167\nbuild 0123456789abcdef\nt 0 0 0 0 0 - - - -",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("source revision"), "{error}");
    }

    #[test]
    fn report_rejects_a_duplicate_image_resource_row() {
        let duplicated = FIXTURE.replace(
            "s 2 2 1000",
            "i 2 2 0 0 0 0 0 0 0 0 0\ns 2 2 1000",
        );
        let error = ReplayReport::parse(&duplicated).unwrap_err().to_string();
        assert!(error.contains("duplicate image resource row"), "{error}");
    }
}
