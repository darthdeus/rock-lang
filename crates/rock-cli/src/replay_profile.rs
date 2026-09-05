use crate::common::{compile_game_for_run, ProcessManager};
use crate::replay_report::{RecordedProfile, ReplayReport};
use crate::{Backend, Result};
use anyhow::{bail, Context};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

#[derive(Debug)]
struct PlaybackSection {
    name: String,
    inclusive_us: u64,
    calls: u64,
}

#[derive(Debug)]
struct PlaybackRow {
    ordinal: usize,
    revision: String,
    total_us: u64,
    logic_us: u64,
    render_us: u64,
    present_us: u64,
    heap_bytes: u64,
    heap_growth_bytes: u64,
    gc: bool,
    dropped: u64,
    draw_calls: u64,
    text_draw_calls: u64,
    stream_upload_calls: u64,
    stream_upload_bytes: u64,
    streamed_vertices: u64,
    sections: Vec<PlaybackSection>,
}

fn parse_usize(value: &str, field: &str) -> Result<usize> {
    value
        .parse()
        .with_context(|| format!("invalid {field} `{value}`"))
}

fn parse_u64(value: &str, field: &str) -> Result<u64> {
    value
        .parse()
        .with_context(|| format!("invalid {field} `{value}`"))
}

fn decode_hex(value: &str, field: &str) -> Result<String> {
    if value.is_empty() || value.len() % 2 != 0 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("invalid {field} encoding `{value}`");
    }
    let bytes = (0..value.len())
        .step_by(2)
        .map(|offset| u8::from_str_radix(&value[offset..offset + 2], 16))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    String::from_utf8(bytes).with_context(|| format!("{field} is not UTF-8"))
}

fn parse_range(raw: &str, observations: usize) -> Result<(usize, usize)> {
    let (from, to) = raw
        .split_once('-')
        .with_context(|| format!("replay profile range must be A-B, got `{raw}`"))?;
    let from = parse_usize(from, "range start")?;
    let to = parse_usize(to, "range end")?;
    if from > to {
        bail!("replay profile range descends: {from}-{to}");
    }
    if observations == 0 || to >= observations {
        bail!(
            "replay profile range {from}-{to} exceeds replay observations 0-{}",
            observations.saturating_sub(1)
        );
    }
    Ok((from, to))
}

fn validate_runs(runs: usize) -> Result<()> {
    if runs == 0 {
        bail!("--runs must be greater than zero");
    }
    Ok(())
}

fn parse_wire_row(line: &str) -> Result<PlaybackRow> {
    let fields = line.split_whitespace().collect::<Vec<_>>();
    if fields.len() < 17 || fields[0] != "ROCK_REPLAY_PROFILE_ROW" {
        bail!("malformed replay profile row");
    }
    let section_count = parse_usize(fields[16], "section count")?;
    if section_count > 12 || fields.len() != 17 + section_count * 3 {
        bail!("malformed replay profile section population");
    }
    let gc = match fields[9] {
        "0" => false,
        "1" => true,
        other => bail!("invalid replay profile GC flag `{other}`"),
    };
    let mut names = HashSet::new();
    let mut sections = Vec::with_capacity(section_count);
    for section in fields[17..].chunks_exact(3) {
        let name = decode_hex(section[0], "section name")?;
        let calls = parse_u64(section[2], "section calls")?;
        if name.is_empty() || calls == 0 || !names.insert(name.clone()) {
            bail!("invalid or duplicate playback section `{name}`");
        }
        sections.push(PlaybackSection {
            name,
            inclusive_us: parse_u64(section[1], "section duration")?,
            calls,
        });
    }
    Ok(PlaybackRow {
        ordinal: parse_usize(fields[1], "ordinal")?,
        revision: decode_hex(fields[2], "source revision")?,
        total_us: parse_u64(fields[3], "total time")?,
        logic_us: parse_u64(fields[4], "logic time")?,
        render_us: parse_u64(fields[5], "render time")?,
        present_us: parse_u64(fields[6], "present time")?,
        heap_bytes: parse_u64(fields[7], "heap bytes")?,
        heap_growth_bytes: parse_u64(fields[8], "heap growth")?,
        gc,
        dropped: parse_u64(fields[10], "dropped events")?,
        draw_calls: parse_u64(fields[11], "draw calls")?,
        text_draw_calls: parse_u64(fields[12], "text draw calls")?,
        stream_upload_calls: parse_u64(fields[13], "stream upload calls")?,
        stream_upload_bytes: parse_u64(fields[14], "stream upload bytes")?,
        streamed_vertices: parse_u64(fields[15], "streamed vertices")?,
        sections,
    })
}

fn set_difference<T: Clone + Eq + std::hash::Hash>(left: &[T], right: &[T]) -> Vec<T> {
    let right = right.iter().collect::<HashSet<_>>();
    left.iter()
        .filter(|value| !right.contains(value))
        .cloned()
        .collect()
}

fn recorded_profile_json(profile: Option<&RecordedProfile>) -> Value {
    profile.map_or(Value::Null, |profile| {
        json!({
            "heap_bytes": profile.heap_bytes,
            "heap_growth_bytes": profile.heap_growth_bytes,
            "gc": profile.gc,
            "dropped": profile.dropped,
            "sections": profile.sections.iter().map(|section| json!({
                "name": section.name,
                "inclusive_us": section.inclusive_us,
                "calls": section.calls,
            })).collect::<Vec<_>>(),
        })
    })
}

fn stderr_tail(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .rev()
        .take(20)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn run(
    backend: Backend,
    game: &str,
    output: String,
    replay_path: &Path,
    raw_range: &str,
    runs: usize,
    app_args: &[String],
) -> Result<()> {
    validate_runs(runs)?;
    let replay = ReplayReport::from_path(replay_path)?;
    let (from, to) = parse_range(raw_range, replay.inputs.len())?;
    let requested_frames = to - from + 1;
    let compiled = compile_game_for_run(backend, game, &output)?;
    let playback_source = crate::stone::replay_source_for(game)?;
    if replay.build != compiled.artifact_fingerprint {
        bail!(
            "replay Artifact mismatch: replay={}, current={}; exact playback is required",
            replay.build,
            compiled.artifact_fingerprint
        );
    }
    if replay.source != playback_source {
        bail!(
            "replay source mismatch: replay={}, current={}",
            replay.source,
            playback_source
        );
    }

    let timings = replay
        .timings
        .iter()
        .map(|timing| (timing.ordinal, timing))
        .collect::<HashMap<_, _>>();
    let timeout_secs = std::env::var("ROCK_FRAME_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(60);
    let mut any_failure = false;

    for run_index in 1..=runs {
        let mut cmd = rock_host::project::build_luajit_command(&compiled.lua_file, app_args);
        cmd.env("FRAME_COUNT", (to + 1).to_string())
            .env("STONE_HEADLESS", "1")
            .env("STONE_REPLAY", replay_path)
            .env("STONE_BUILD_ID", &compiled.artifact_fingerprint)
            .env("STONE_REPLAY_SOURCE", &playback_source)
            .env("STONE_REPLAY_PROFILE_FROM", from.to_string())
            .env("STONE_REPLAY_PROFILE_TO", to.to_string())
            .env("STONE_REPLAY_PROFILE_WIRE", "1")
            .env("STONE_REPLAY_SLOW_PROFILE_MS", "0")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        crate::stone::apply_headless_audio_env(&mut cmd);

        let mut child = cmd.spawn().context("spawn replay profile")?;
        let result =
            ProcessManager::wait_with_timeout(&mut child, Duration::from_secs(timeout_secs))?;
        let mut parsed = Vec::new();
        let mut parse_error = None;
        let mut seen = HashSet::new();
        for line in String::from_utf8_lossy(&result.output.stdout).lines() {
            if !line.starts_with("ROCK_REPLAY_PROFILE_ROW ") {
                continue;
            }
            match parse_wire_row(line) {
                Ok(row) if row.ordinal >= from && row.ordinal <= to && seen.insert(row.ordinal) => {
                    parsed.push(row);
                }
                Ok(row) if !seen.insert(row.ordinal) => {
                    parse_error = Some(format!("duplicate observation {}", row.ordinal));
                }
                Ok(row) => {
                    parse_error = Some(format!(
                        "observation {} outside requested range",
                        row.ordinal
                    ));
                }
                Err(error) => parse_error = Some(error.to_string()),
            }
        }
        parsed.sort_by_key(|row| row.ordinal);
        let playback_revisions = parsed
            .iter()
            .map(|row| row.revision.as_str())
            .collect::<HashSet<_>>();
        if playback_revisions.len() > 1 {
            parse_error = Some("playback source revision changed within one run".to_string());
        }
        let playback_revision = parsed
            .first()
            .map(|row| row.revision.as_str())
            .unwrap_or("");

        for row in &parsed {
            let current = &replay.inputs[row.ordinal];
            let previous = row
                .ordinal
                .checked_sub(1)
                .and_then(|ordinal| replay.inputs.get(ordinal));
            let previous_keys = previous.map(|input| input.keys.as_slice()).unwrap_or(&[]);
            let previous_buttons = previous
                .map(|input| input.buttons.as_slice())
                .unwrap_or(&[]);
            let previous_pad_buttons = previous
                .map(|input| input.pad_buttons.as_slice())
                .unwrap_or(&[]);
            let recorded = timings.get(&row.ordinal).map_or(Value::Null, |timing| {
                json!({
                    "total_us": timing.total_us,
                    "logic_us": timing.logic_us,
                    "render_us": timing.render_us,
                    "present_us": timing.present_us,
                })
            });
            println!(
                "{}",
                serde_json::to_string(&json!({
                    "format": "rock.replay-profile.v1",
                    "kind": "frame",
                    "run": run_index,
                    "status": "complete",
                    "identity": {
                        "replay_version": replay.version,
                        "game": replay.game,
                        "source": replay.source,
                        "replay_revision": replay.revision,
                        "playback_revision": row.revision,
                        "replay_build": replay.build,
                        "playback_build": compiled.artifact_fingerprint,
                        "exact_artifact": true,
                        "same_revision": replay.revision.is_empty() || replay.revision == row.revision,
                    },
                    "ordinal": row.ordinal,
                    "tick": row.ordinal,
                    "recorded": recorded,
                    "playback": {
                        "total_us": row.total_us,
                        "logic_us": row.logic_us,
                        "render_us": row.render_us,
                        "present_us": row.present_us,
                    },
                    "input": {
                        "pressed_keys": set_difference(&current.keys, previous_keys),
                        "released_keys": set_difference(previous_keys, &current.keys),
                        "pressed_buttons": set_difference(&current.buttons, previous_buttons),
                        "released_buttons": set_difference(previous_buttons, &current.buttons),
                        "pressed_pad_buttons": set_difference(&current.pad_buttons, previous_pad_buttons),
                        "released_pad_buttons": set_difference(previous_pad_buttons, &current.pad_buttons),
                        "mouse": {
                            "x_hex": current.mouse_x,
                            "y_hex": current.mouse_y,
                            "wheel": current.wheel,
                            "imgui_captured": current.imgui,
                        },
                        "pad_axes": current.pad_axes.iter().map(|(axis, magnitude)| json!({
                            "axis": axis,
                            "magnitude_hex": magnitude,
                        })).collect::<Vec<_>>(),
                    },
                    "recorded_profile": recorded_profile_json(replay.profiles.get(&row.ordinal)),
                    "playback_profile": {
                        "heap_bytes": row.heap_bytes,
                        "heap_growth_bytes": row.heap_growth_bytes,
                        "gc": row.gc,
                        "dropped": row.dropped,
                        "allocation_traffic": {
                            "available": false,
                            "bytes": Value::Null,
                        },
                        "sections": row.sections.iter().map(|section| json!({
                            "name": section.name,
                            "inclusive_us": section.inclusive_us,
                            "calls": section.calls,
                        })).collect::<Vec<_>>(),
                    },
                    "graphics": {
                        "draw_calls": row.draw_calls,
                        "text_draw_calls": row.text_draw_calls,
                        "stream_upload_calls": row.stream_upload_calls,
                        "stream_upload_bytes": row.stream_upload_bytes,
                        "streamed_vertices": row.streamed_vertices,
                    },
                }))?
            );
        }

        let expected = (from..=to).collect::<Vec<_>>();
        let observed = parsed.iter().map(|row| row.ordinal).collect::<Vec<_>>();
        let status = if result.timed_out {
            "timeout"
        } else if !result.output.status.success() {
            "process_failure"
        } else if parse_error.is_some() {
            "invalid_output"
        } else if observed != expected {
            "incomplete"
        } else {
            "complete"
        };
        any_failure |= status != "complete";
        println!(
            "{}",
            serde_json::to_string(&json!({
                "format": "rock.replay-profile.v1",
                "kind": "run",
                "run": run_index,
                "status": status,
                "identity": {
                    "replay_version": replay.version,
                    "game": replay.game,
                    "source": replay.source,
                    "replay_revision": replay.revision,
                    "playback_revision": playback_revision,
                    "replay_build": replay.build,
                    "playback_build": compiled.artifact_fingerprint,
                    "exact_artifact": true,
                    "same_revision": replay.revision.is_empty()
                        || (!playback_revision.is_empty() && replay.revision == playback_revision),
                },
                "range": { "from": from, "to": to },
                "warmup_frames": from,
                "requested_frames": requested_frames,
                "observed_frames": parsed.len(),
                "observed_ordinals": observed,
                "exit_code": result.output.status.code(),
                "timed_out": result.timed_out,
                "error": parse_error,
                "stderr_tail": if status == "complete" { String::new() } else { stderr_tail(&result.output.stderr) },
            }))?
        );
    }

    if any_failure {
        bail!("one or more replay-profile runs were incomplete; inspect terminal JSONL rows");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_is_inclusive_and_fail_closed() {
        assert_eq!(parse_range("2-4", 6).unwrap(), (2, 4));
        assert!(parse_range("4-2", 6).is_err());
        assert!(parse_range("2", 6).is_err());
        assert!(parse_range("2-6", 6).is_err());
    }

    #[test]
    fn wire_parser_rejects_duplicate_sections() {
        let row = "ROCK_REPLAY_PROFILE_ROW 2 616263 10 2 7 1 100 20 0 0 3 1 2 48 12 2 64726177 7 1 64726177 3 1";
        assert!(parse_wire_row(row).is_err());
    }

    #[test]
    fn zero_runs_and_future_replay_versions_fail_closed() {
        assert!(validate_runs(0).is_err());
        assert!(validate_runs(1).is_ok());
        assert!(ReplayReport::parse("STONE_REPLAY 15").is_err());
        assert!(parse_range("0-0", 0).is_err());
    }
}
