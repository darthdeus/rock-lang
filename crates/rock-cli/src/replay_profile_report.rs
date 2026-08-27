use crate::Result;
use anyhow::{bail, Context};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Phases {
    total_us: u64,
    logic_us: u64,
    render_us: u64,
    present_us: u64,
}

#[derive(Clone, Debug)]
struct Section {
    name: String,
    inclusive_us: u64,
    calls: u64,
}

#[derive(Clone, Debug)]
struct Frame {
    run: usize,
    ordinal: usize,
    identity: Value,
    recorded: Option<Phases>,
    recorded_profile: Value,
    playback: Phases,
    input: Value,
    heap_bytes: u64,
    heap_growth_bytes: u64,
    gc: bool,
    dropped: u64,
    sections: Vec<Section>,
    graphics: Value,
}

#[derive(Clone, Debug)]
struct Terminal {
    run: usize,
    status: String,
    identity: Value,
    from: usize,
    to: usize,
    requested_frames: usize,
    observed_frames: usize,
    observed_ordinals: Vec<usize>,
    exit_code: Value,
    timed_out: bool,
    error: Value,
    stderr_tail: String,
}

#[derive(Debug)]
struct ParsedProfile {
    identity: Value,
    from: usize,
    to: usize,
    frames: HashMap<(usize, usize), Frame>,
    terminals: BTreeMap<usize, Terminal>,
}

fn required<'a>(value: &'a Value, field: &str) -> Result<&'a Value> {
    value
        .get(field)
        .with_context(|| format!("missing replay-profile field `{field}`"))
}

fn usize_field(value: &Value, field: &str) -> Result<usize> {
    usize::try_from(
        required(value, field)?.as_u64().with_context(|| {
            format!("replay-profile field `{field}` is not an unsigned integer")
        })?,
    )
    .with_context(|| format!("replay-profile field `{field}` exceeds usize"))
}

fn u64_field(value: &Value, field: &str) -> Result<u64> {
    required(value, field)?
        .as_u64()
        .with_context(|| format!("replay-profile field `{field}` is not an unsigned integer"))
}

fn bool_field(value: &Value, field: &str) -> Result<bool> {
    required(value, field)?
        .as_bool()
        .with_context(|| format!("replay-profile field `{field}` is not a boolean"))
}

fn string_field(value: &Value, field: &str) -> Result<String> {
    required(value, field)?
        .as_str()
        .map(ToOwned::to_owned)
        .with_context(|| format!("replay-profile field `{field}` is not a string"))
}

fn parse_phases(value: &Value, field: &str) -> Result<Phases> {
    let value = required(value, field)?;
    let phases = Phases {
        total_us: u64_field(value, "total_us")?,
        logic_us: u64_field(value, "logic_us")?,
        render_us: u64_field(value, "render_us")?,
        present_us: u64_field(value, "present_us")?,
    };
    let accounted = phases
        .logic_us
        .checked_add(phases.render_us)
        .and_then(|value| value.checked_add(phases.present_us))
        .context("replay-profile phase durations overflow")?;
    if accounted > phases.total_us {
        bail!("replay-profile phase durations exceed total duration");
    }
    Ok(phases)
}

fn parse_identity(value: &Value) -> Result<Value> {
    let identity = required(value, "identity")?;
    if !identity.is_object() {
        bail!("replay-profile identity is not an object");
    }
    u64_field(identity, "replay_version")?;
    for field in ["game", "source", "replay_revision", "playback_revision"] {
        string_field(identity, field)?;
    }
    let replay_build = string_field(identity, "replay_build")?;
    let playback_build = string_field(identity, "playback_build")?;
    if !bool_field(identity, "exact_artifact")? {
        bail!("replay-profile report requires exact-Artifact rows");
    }
    bool_field(identity, "same_revision")?;
    if replay_build.is_empty() || replay_build != playback_build {
        bail!("replay-profile identity does not contain one exact Artifact");
    }
    Ok(identity.clone())
}

fn validate_recorded_profile(value: &Value) -> Result<()> {
    if value.is_null() {
        return Ok(());
    }
    u64_field(value, "heap_bytes")?;
    u64_field(value, "heap_growth_bytes")?;
    bool_field(value, "gc")?;
    u64_field(value, "dropped")?;
    let sections = required(value, "sections")?
        .as_array()
        .context("recorded replay-profile sections are not an array")?;
    if sections.len() > 12 {
        bail!("recorded replay-profile exceeds the 12-section bound");
    }
    let mut names = HashSet::new();
    for section in sections {
        let name = string_field(section, "name")?;
        let calls = u64_field(section, "calls")?;
        u64_field(section, "inclusive_us")?;
        if name.is_empty() || calls == 0 || !names.insert(name.clone()) {
            bail!("invalid or duplicate recorded replay-profile section `{name}`");
        }
    }
    Ok(())
}

fn parse_frame(value: &Value) -> Result<Frame> {
    if string_field(value, "status")? != "complete" {
        bail!("replay-profile frame row is not complete");
    }
    let profile = required(value, "playback_profile")?;
    let mut section_names = HashSet::new();
    let sections = required(profile, "sections")?
        .as_array()
        .context("replay-profile sections are not an array")?
        .iter()
        .map(|section| {
            let name = string_field(section, "name")?;
            let calls = u64_field(section, "calls")?;
            if name.is_empty() || calls == 0 || !section_names.insert(name.clone()) {
                bail!("invalid or duplicate replay-profile section `{name}`");
            }
            Ok(Section {
                name,
                inclusive_us: u64_field(section, "inclusive_us")?,
                calls,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if sections.len() > 12 {
        bail!("replay-profile frame exceeds the 12-section bound");
    }
    let ordinal = usize_field(value, "ordinal")?;
    if usize_field(value, "tick")? != ordinal {
        bail!("replay-profile tick does not match observation ordinal");
    }
    let recorded_value = required(value, "recorded")?;
    let recorded = if recorded_value.is_null() {
        None
    } else {
        Some(parse_phases(value, "recorded")?)
    };
    let recorded_profile = required(value, "recorded_profile")?;
    validate_recorded_profile(recorded_profile)?;
    Ok(Frame {
        run: usize_field(value, "run")?,
        ordinal,
        identity: parse_identity(value)?,
        recorded,
        recorded_profile: recorded_profile.clone(),
        playback: parse_phases(value, "playback")?,
        input: required(value, "input")?.clone(),
        heap_bytes: u64_field(profile, "heap_bytes")?,
        heap_growth_bytes: u64_field(profile, "heap_growth_bytes")?,
        gc: bool_field(profile, "gc")?,
        dropped: u64_field(profile, "dropped")?,
        sections,
        graphics: required(value, "graphics")?.clone(),
    })
}

fn parse_terminal(value: &Value) -> Result<Terminal> {
    let status = string_field(value, "status")?;
    if ![
        "complete",
        "timeout",
        "process_failure",
        "invalid_output",
        "incomplete",
    ]
    .contains(&status.as_str())
    {
        bail!("unknown replay-profile terminal status `{status}`");
    }
    let range = required(value, "range")?;
    let ordinals = required(value, "observed_ordinals")?
        .as_array()
        .context("replay-profile observed ordinals are not an array")?
        .iter()
        .map(|ordinal| {
            usize::try_from(
                ordinal
                    .as_u64()
                    .context("observed ordinal is not unsigned")?,
            )
            .context("observed ordinal exceeds usize")
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Terminal {
        run: usize_field(value, "run")?,
        status,
        identity: parse_identity(value)?,
        from: usize_field(range, "from")?,
        to: usize_field(range, "to")?,
        requested_frames: usize_field(value, "requested_frames")?,
        observed_frames: usize_field(value, "observed_frames")?,
        observed_ordinals: ordinals,
        exit_code: required(value, "exit_code")?.clone(),
        timed_out: bool_field(value, "timed_out")?,
        error: required(value, "error")?.clone(),
        stderr_tail: string_field(value, "stderr_tail")?,
    })
}

impl ParsedProfile {
    fn parse(text: &str) -> Result<Self> {
        let mut frames = HashMap::new();
        let mut terminals = BTreeMap::new();
        let mut identity = None;
        for (line_index, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(line).with_context(|| {
                format!("invalid replay-profile JSON on line {}", line_index + 1)
            })?;
            if required(&value, "format")?.as_str() != Some("rock.replay-profile.v1") {
                bail!(
                    "unsupported replay-profile format on line {}",
                    line_index + 1
                );
            }
            match required(&value, "kind")?.as_str() {
                Some("frame") => {
                    let frame = parse_frame(&value)?;
                    if frame.run == 0 {
                        bail!("replay-profile run indices start at one");
                    }
                    Self::accept_identity(&mut identity, &frame.identity)?;
                    let key = (frame.run, frame.ordinal);
                    if frames.insert(key, frame).is_some() {
                        bail!(
                            "duplicate replay-profile frame for run {} observation {}",
                            key.0,
                            key.1
                        );
                    }
                }
                Some("run") => {
                    let terminal = parse_terminal(&value)?;
                    if terminal.run == 0 {
                        bail!("replay-profile run indices start at one");
                    }
                    Self::accept_identity(&mut identity, &terminal.identity)?;
                    if terminals.insert(terminal.run, terminal).is_some() {
                        bail!("duplicate replay-profile terminal row");
                    }
                }
                other => bail!("unknown replay-profile row kind {other:?}"),
            }
        }
        let identity = identity.context("replay-profile input has no rows")?;
        let first = terminals
            .values()
            .next()
            .context("replay-profile input has no terminal rows")?;
        let (from, to) = (first.from, first.to);
        if from > to {
            bail!("replay-profile terminal range descends: {from}-{to}");
        }
        let expected_runs = (1..=terminals.len()).collect::<BTreeSet<_>>();
        let actual_runs = terminals.keys().copied().collect::<BTreeSet<_>>();
        if actual_runs != expected_runs {
            bail!("replay-profile terminal run indices are not contiguous from one");
        }
        for terminal in terminals.values() {
            if (terminal.from, terminal.to) != (from, to) {
                bail!("replay-profile terminal ranges disagree");
            }
            let requested = to - from + 1;
            if terminal.requested_frames != requested {
                bail!(
                    "run {} requested denominator disagrees with its range",
                    terminal.run
                );
            }
            let mut actual = frames
                .keys()
                .filter_map(|(run, ordinal)| (*run == terminal.run).then_some(*ordinal))
                .collect::<Vec<_>>();
            actual.sort_unstable();
            if terminal.observed_frames != actual.len() || terminal.observed_ordinals != actual {
                bail!(
                    "run {} terminal denominator disagrees with its frame rows",
                    terminal.run
                );
            }
            if actual
                .iter()
                .any(|ordinal| *ordinal < from || *ordinal > to)
            {
                bail!(
                    "run {} contains an observation outside its requested range",
                    terminal.run
                );
            }
            if terminal.status == "complete" {
                let expected = (from..=to).collect::<Vec<_>>();
                if actual != expected {
                    bail!(
                        "complete run {} does not contain its exact requested range",
                        terminal.run
                    );
                }
                if terminal.timed_out
                    || !terminal.error.is_null()
                    || terminal.exit_code.as_i64() != Some(0)
                {
                    bail!("complete run {} carries a failure terminal", terminal.run);
                }
            } else if terminal.status == "timeout" && !terminal.timed_out {
                bail!("timeout run {} is not marked timed out", terminal.run);
            }
        }
        for run in frames.keys().map(|(run, _)| *run) {
            if !terminals.contains_key(&run) {
                bail!("replay-profile frame run {run} has no terminal row");
            }
        }
        Ok(Self {
            identity,
            from,
            to,
            frames,
            terminals,
        })
    }

    fn accept_identity(identity: &mut Option<Value>, candidate: &Value) -> Result<()> {
        match identity {
            Some(existing) => {
                for field in [
                    "replay_version",
                    "game",
                    "source",
                    "replay_revision",
                    "replay_build",
                    "playback_build",
                    "exact_artifact",
                ] {
                    if existing[field] != candidate[field] {
                        bail!("replay-profile identities disagree at `{field}`");
                    }
                }
                let existing_revision = existing["playback_revision"]
                    .as_str()
                    .context("stored playback revision is not a string")?
                    .to_string();
                let candidate_revision = candidate["playback_revision"]
                    .as_str()
                    .context("candidate playback revision is not a string")?;
                if !existing_revision.is_empty()
                    && !candidate_revision.is_empty()
                    && existing_revision != candidate_revision
                {
                    bail!("replay-profile playback revisions disagree");
                }
                if existing_revision.is_empty() && !candidate_revision.is_empty() {
                    existing["playback_revision"] = json!(candidate_revision);
                    existing["same_revision"] = candidate["same_revision"].clone();
                } else if !existing_revision.is_empty()
                    && !candidate_revision.is_empty()
                    && existing["same_revision"] != candidate["same_revision"]
                {
                    bail!("replay-profile revision comparisons disagree");
                }
            }
            None => *identity = Some(candidate.clone()),
        }
        Ok(())
    }
}

fn median(sorted: &[u64]) -> u64 {
    let middle = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        sorted[middle]
    } else {
        sorted[middle - 1] + (sorted[middle] - sorted[middle - 1]) / 2
    }
}

fn stats_us(values: impl IntoIterator<Item = u64>) -> Value {
    let mut values = values.into_iter().collect::<Vec<_>>();
    values.sort_unstable();
    json!({
        "min_us": values.first().copied().unwrap_or(0),
        "median_us": if values.is_empty() { 0 } else { median(&values) },
        "max_us": values.last().copied().unwrap_or(0),
    })
}

fn stats_bytes(values: impl IntoIterator<Item = u64>) -> Value {
    let mut values = values.into_iter().collect::<Vec<_>>();
    values.sort_unstable();
    json!({
        "min_bytes": values.first().copied().unwrap_or(0),
        "median_bytes": if values.is_empty() { 0 } else { median(&values) },
        "max_bytes": values.last().copied().unwrap_or(0),
    })
}

fn repeatability(values: &[u64]) -> (&'static str, u64, Option<usize>) {
    if values.len() < 2 {
        return ("single_run", 0, None);
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let span = sorted[sorted.len() - 1] - sorted[0];
    let limit = 1_000_u64.max(median(&sorted) / 5);
    if span <= limit {
        return ("consistent", 0, None);
    }
    let mut best = None;
    if sorted.len() >= 4 {
        for split in 2..=sorted.len() - 2 {
            let gap = sorted[split] - sorted[split - 1];
            if best.map_or(true, |(_, best_gap)| gap > best_gap) {
                best = Some((split, gap));
            }
        }
    }
    if let Some((split, gap)) = best {
        let left_span = sorted[split - 1] - sorted[0];
        let right_span = sorted[sorted.len() - 1] - sorted[split];
        if gap >= 1_000 && gap > left_span.max(right_span).saturating_mul(2) {
            return ("bimodal_candidate", gap, Some(split));
        }
    }
    ("variable", best.map_or(0, |(_, gap)| gap), None)
}

fn recording_relation(recorded: Option<u64>, playback: &[u64]) -> &'static str {
    let Some(recorded) = recorded else {
        return "unavailable";
    };
    let min = playback.iter().copied().min().unwrap_or(0);
    let max = playback.iter().copied().max().unwrap_or(0);
    if recorded < min {
        "recorded_faster"
    } else if recorded > max {
        "recorded_slower"
    } else {
        "within_playback_range"
    }
}

fn dominant_phase(logic: u64, render: u64, present: u64, unattributed: u64) -> &'static str {
    [
        ("logic", logic),
        ("render", render),
        ("present", present),
        ("unattributed", unattributed),
    ]
    .into_iter()
    .max_by_key(|(_, duration)| *duration)
    .map(|(name, _)| name)
    .unwrap()
}

fn aggregate_frame(ordinal: usize, frames: &[&Frame]) -> Result<Value> {
    let first = frames
        .first()
        .context("cannot aggregate an empty frame population")?;
    if frames.iter().any(|frame| frame.input != first.input) {
        bail!("input transitions disagree at observation {ordinal}");
    }
    if frames.iter().any(|frame| frame.recorded != first.recorded) {
        bail!("recorded timing disagrees at observation {ordinal}");
    }
    if frames
        .iter()
        .any(|frame| frame.recorded_profile != first.recorded_profile)
    {
        bail!("recorded profile disagrees at observation {ordinal}");
    }
    let playback_total = frames
        .iter()
        .map(|frame| frame.playback.total_us)
        .collect::<Vec<_>>();
    let playback_logic = frames
        .iter()
        .map(|frame| frame.playback.logic_us)
        .collect::<Vec<_>>();
    let playback_render = frames
        .iter()
        .map(|frame| frame.playback.render_us)
        .collect::<Vec<_>>();
    let playback_present = frames
        .iter()
        .map(|frame| frame.playback.present_us)
        .collect::<Vec<_>>();
    let playback_unattributed = frames
        .iter()
        .map(|frame| {
            let accounted = frame
                .playback
                .logic_us
                .saturating_add(frame.playback.render_us)
                .saturating_add(frame.playback.present_us);
            frame.playback.total_us.saturating_sub(accounted)
        })
        .collect::<Vec<_>>();
    let mut sorted_total = playback_total.clone();
    sorted_total.sort_unstable();
    let logic_median = {
        let mut v = playback_logic.clone();
        v.sort_unstable();
        median(&v)
    };
    let render_median = {
        let mut v = playback_render.clone();
        v.sort_unstable();
        median(&v)
    };
    let present_median = {
        let mut v = playback_present.clone();
        v.sort_unstable();
        median(&v)
    };
    let unattributed_median = {
        let mut v = playback_unattributed.clone();
        v.sort_unstable();
        median(&v)
    };
    let (repeatability, largest_gap_us, split_after_samples) = repeatability(&playback_total);

    let mut section_values = BTreeMap::<String, (Vec<u64>, Vec<u64>, usize)>::new();
    for (run_index, frame) in frames.iter().enumerate() {
        for section in &frame.sections {
            let entry = section_values
                .entry(section.name.clone())
                .or_insert_with(|| (vec![0; frames.len()], vec![0; frames.len()], 0));
            entry.0[run_index] = section.inclusive_us;
            entry.1[run_index] = section.calls;
            entry.2 += 1;
        }
    }
    let mut sections = section_values
        .into_iter()
        .map(|(name, (mut durations, mut calls, runs_seen))| {
            durations.sort_unstable();
            calls.sort_unstable();
            json!({
                "name": name,
                "runs_seen": runs_seen,
                "median_us": median(&durations),
                "max_us": durations.last().copied().unwrap_or(0),
                "median_calls": median(&calls),
            })
        })
        .collect::<Vec<_>>();
    sections.sort_by_key(|section| std::cmp::Reverse(section["median_us"].as_u64().unwrap_or(0)));

    let recorded = first.recorded.as_ref().map_or(Value::Null, |phase| {
        json!({
            "total_us": phase.total_us,
            "logic_us": phase.logic_us,
            "render_us": phase.render_us,
            "present_us": phase.present_us,
        })
    });
    let recorded_total = first.recorded.as_ref().map(|phase| phase.total_us);
    let graphics = [
        "draw_calls",
        "text_draw_calls",
        "stream_upload_calls",
        "stream_upload_bytes",
        "streamed_vertices",
    ]
    .into_iter()
    .map(|field| {
        let values = frames
            .iter()
            .map(|frame| u64_field(&frame.graphics, field))
            .collect::<Result<Vec<_>>>()?;
        let mut sorted = values;
        sorted.sort_unstable();
        Ok((field.to_string(), json!(median(&sorted))))
    })
    .collect::<Result<serde_json::Map<String, Value>>>()?;

    Ok(json!({
        "ordinal": ordinal,
        "samples": frames.len(),
        "recorded": recorded,
        "recording_relation": recording_relation(recorded_total, &playback_total),
        "repeatability": repeatability,
        "repeatability_evidence": {
            "span_us": sorted_total.last().unwrap() - sorted_total.first().unwrap(),
            "largest_interior_gap_us": largest_gap_us,
            "split_after_samples": split_after_samples,
        },
        "playback": {
            "total": stats_us(playback_total),
            "logic": stats_us(playback_logic),
            "render": stats_us(playback_render),
            "present": stats_us(playback_present),
            "unattributed": stats_us(playback_unattributed),
            "dominant_phase": dominant_phase(logic_median, render_median, present_median, unattributed_median),
        },
        "input": first.input,
        "recorded_profile": first.recorded_profile,
        "playback_profile": {
            "heap_bytes": stats_bytes(frames.iter().map(|frame| frame.heap_bytes)),
            "heap_growth_bytes": stats_bytes(frames.iter().map(|frame| frame.heap_growth_bytes)),
            "gc_runs": frames.iter().filter(|frame| frame.gc).count(),
            "dropped": frames.iter().map(|frame| frame.dropped).sum::<u64>(),
        },
        "sections": sections,
        "graphics_median": graphics,
    }))
}

fn analyze(parsed: ParsedProfile, top: usize, source_file: &Path) -> Result<Value> {
    if top == 0 {
        bail!("--top must be greater than zero");
    }
    let complete_runs = parsed
        .terminals
        .values()
        .filter(|terminal| terminal.status == "complete")
        .map(|terminal| terminal.run)
        .collect::<Vec<_>>();
    let mut frames = Vec::new();
    for ordinal in parsed.from..=parsed.to {
        let population = complete_runs
            .iter()
            .map(|run| {
                parsed
                    .frames
                    .get(&(*run, ordinal))
                    .with_context(|| format!("complete run {run} is missing observation {ordinal}"))
            })
            .collect::<Result<Vec<_>>>()?;
        if !population.is_empty() {
            frames.push(aggregate_frame(ordinal, &population)?);
        }
    }
    frames.sort_by_key(|frame| {
        std::cmp::Reverse(
            frame["playback"]["total"]["median_us"]
                .as_u64()
                .unwrap_or(0),
        )
    });
    frames.truncate(top.min(frames.len()));
    let incomplete = parsed
        .terminals
        .values()
        .filter(|terminal| terminal.status != "complete")
        .map(|terminal| {
            json!({
                "run": terminal.run,
                "status": terminal.status,
                "requested_frames": terminal.requested_frames,
                "observed_frames": terminal.observed_frames,
                "observed_ordinals": terminal.observed_ordinals,
                "exit_code": terminal.exit_code,
                "timed_out": terminal.timed_out,
                "error": terminal.error,
                "stderr_tail": terminal.stderr_tail,
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "format": "rock.replay-profile-report.v1",
        "source_file": source_file.display().to_string(),
        "identity": parsed.identity,
        "range": { "from": parsed.from, "to": parsed.to },
        "runs": {
            "total": parsed.terminals.len(),
            "complete": complete_runs.len(),
            "incomplete": incomplete.len(),
        },
        "observations": parsed.to - parsed.from + 1,
        "repeatability_heuristic": {
            "consistent": "max-min <= max(1000us, median/5)",
            "bimodal_candidate": "at least two samples per side and the largest interior gap is >=1000us and >2x either cluster span",
        },
        "incomplete_runs": incomplete,
        "top_frames": frames,
    }))
}

fn input_summary(input: &Value) -> String {
    let mut changes = Vec::new();
    for (field, prefix) in [
        ("pressed_keys", "+key"),
        ("released_keys", "-key"),
        ("pressed_buttons", "+mouse"),
        ("released_buttons", "-mouse"),
        ("pressed_pad_buttons", "+pad"),
        ("released_pad_buttons", "-pad"),
    ] {
        let values = input
            .get(field)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if !values.is_empty() {
            let values = values
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .map_or_else(|| value.to_string(), ToOwned::to_owned)
                })
                .collect::<Vec<_>>();
            changes.push(format!("{prefix} {}", values.join(",")));
        }
    }
    changes.join(" ")
}

fn render_text(report: &Value) -> String {
    let runs = &report["runs"];
    let range = &report["range"];
    let identity = &report["identity"];
    let mut out = format!(
        "Replay profile: {} / {}\nArtifact: {}\nRange: {}-{} ({} observations)\nRuns: {} complete, {} incomplete\n",
        identity["game"].as_str().unwrap_or("?"),
        identity["source"].as_str().unwrap_or("?"),
        identity["replay_build"].as_str().unwrap_or("?"),
        range["from"],
        range["to"],
        report["observations"],
        runs["complete"],
        runs["incomplete"],
    );
    for terminal in report["incomplete_runs"].as_array().unwrap() {
        out.push_str(&format!(
            "  run {}: {} ({}/{} frames) {}\n",
            terminal["run"],
            terminal["status"].as_str().unwrap_or("?"),
            terminal["observed_frames"],
            terminal["requested_frames"],
            terminal["error"].as_str().unwrap_or(""),
        ));
    }
    out.push_str("\nSlowest playback observations:\n");
    if report["top_frames"].as_array().unwrap().is_empty() {
        out.push_str("  none (no complete runs)\n");
        return out;
    }
    for frame in report["top_frames"].as_array().unwrap() {
        let total = &frame["playback"]["total"];
        let recorded = frame["recorded"]["total_us"].as_u64();
        let input = input_summary(&frame["input"]);
        out.push_str(&format!(
            "  #{:<6} playback {:>7.2} ms [{:>7.2}..{:>7.2}]  {:<19} {:<21} recorded {}{}\n",
            frame["ordinal"],
            total["median_us"].as_u64().unwrap_or(0) as f64 / 1000.0,
            total["min_us"].as_u64().unwrap_or(0) as f64 / 1000.0,
            total["max_us"].as_u64().unwrap_or(0) as f64 / 1000.0,
            frame["repeatability"].as_str().unwrap_or("?"),
            frame["recording_relation"].as_str().unwrap_or("?"),
            recorded.map_or_else(
                || "n/a".to_string(),
                |value| format!("{:.2} ms", value as f64 / 1000.0)
            ),
            if input.is_empty() {
                String::new()
            } else {
                format!("  {input}")
            },
        ));
        out.push_str(&format!(
            "           dominant {}, logic {:.2} ms, render {:.2} ms, present {:.2} ms\n",
            frame["playback"]["dominant_phase"].as_str().unwrap_or("?"),
            frame["playback"]["logic"]["median_us"]
                .as_u64()
                .unwrap_or(0) as f64
                / 1000.0,
            frame["playback"]["render"]["median_us"]
                .as_u64()
                .unwrap_or(0) as f64
                / 1000.0,
            frame["playback"]["present"]["median_us"]
                .as_u64()
                .unwrap_or(0) as f64
                / 1000.0,
        ));
        for section in frame["sections"].as_array().unwrap().iter().take(5) {
            out.push_str(&format!(
                "             {:<36} {:>7.2} ms  seen {}/{}\n",
                section["name"].as_str().unwrap_or("?"),
                section["median_us"].as_u64().unwrap_or(0) as f64 / 1000.0,
                section["runs_seen"],
                frame["samples"],
            ));
        }
        if let Some(sections) = frame["recorded_profile"]["sections"].as_array() {
            for section in sections.iter().take(3) {
                out.push_str(&format!(
                    "             recorded {:<27} {:>7.2} ms x{}\n",
                    section["name"].as_str().unwrap_or("?"),
                    section["inclusive_us"].as_u64().unwrap_or(0) as f64 / 1000.0,
                    section["calls"],
                ));
            }
        }
    }
    out
}

pub fn run(path: &Path, top: usize, format: &str) -> Result<()> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read replay profile {}", path.display()))?;
    let report = analyze(ParsedProfile::parse(&text)?, top, path)?;
    match format {
        "pretty" => print!("{}", render_text(&report)),
        "json" => println!("{}", serde_json::to_string_pretty(&report)?),
        other => bail!("replay-profile-report format must be `pretty` or `json`, got `{other}`"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeatability_is_descriptive_and_non_degenerate() {
        assert_eq!(repeatability(&[10_000]), ("single_run", 0, None));
        assert_eq!(repeatability(&[20_000, 21_000]), ("consistent", 0, None));
        assert_eq!(
            repeatability(&[10_000, 11_000, 40_000, 42_000]),
            ("bimodal_candidate", 29_000, Some(2))
        );
        assert_eq!(
            repeatability(&[10_000, 18_000, 20_000]),
            ("variable", 0, None)
        );
    }
}
