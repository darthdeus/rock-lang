use sound_core::{
    AssetResolver, FsAssetResolver, MeasurementCache, compile_project, parse_project,
};
use std::{collections::BTreeMap, path::PathBuf};

fn main() {
    if let Err(error) = run() {
        eprintln!("loudness-report: {error}");
        std::process::exit(1);
    }
}
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let mode = args.next().ok_or("expected report or measurements")?;
    let path = PathBuf::from(args.next().ok_or("expected sound project path")?);
    let mut roots: Vec<PathBuf> = args.map(PathBuf::from).collect();
    roots.push(
        path.parent()
            .ok_or("project path has no parent")?
            .to_owned(),
    );
    let resolver = FsAssetResolver::new(roots);
    let text = std::fs::read_to_string(path)?;
    let mut raw: serde_json::Value = serde_json::from_str(&text)?;
    for event in raw["events"].as_array_mut().ok_or("missing events")? {
        for step in event["steps"].as_array_mut().ok_or("missing steps")? {
            for layer in step["layers"].as_array_mut().ok_or("missing layers")? {
                let sources: Vec<String> = serde_json::from_value(layer["sources"].clone())?;
                layer["sources"] =
                    serde_json::to_value(sound_core::expand_sources(&sources, &resolver)?)?;
            }
        }
    }
    let cache = MeasurementCache::standard()?;
    if mode == "raw-mixes" {
        let mut results = BTreeMap::new();
        for event in raw["events"].as_array().ok_or("missing events")? {
            for (si, step) in event["steps"]
                .as_array()
                .ok_or("missing steps")?
                .iter()
                .enumerate()
            {
                if step["layers"].as_array().ok_or("missing layers")?.len() < 2 {
                    continue;
                }
                let mut step = step.clone();
                for layer in step["layers"].as_array_mut().unwrap() {
                    if layer.get("volume").is_none() {
                        layer["volume"] = 1.0.into();
                    }
                }
                let step: sound_core::SoundStep = serde_json::from_value(step)?;
                let gains = step
                    .layers
                    .iter()
                    .map(|l| vec![f64::from(l.volume); l.sources.len()])
                    .collect::<Vec<_>>();
                results.insert(
                    format!("{}@{si}", event["id"].as_str().ok_or("missing ID")?),
                    sound_core::measure_step(&step, &gains, &resolver, &cache)?,
                );
            }
        }
        println!("{}", serde_json::to_string_pretty(&results)?);
        return Ok(());
    }
    if mode == "measurements" {
        let mut results = BTreeMap::new();
        for event in raw["events"].as_array().ok_or("missing events")? {
            for step in event["steps"].as_array().ok_or("missing steps")? {
                for layer in step["layers"].as_array().ok_or("missing layers")? {
                    let speed = layer["speed"].as_f64().unwrap_or(1.0);
                    for key in layer["sources"].as_array().ok_or("missing sources")? {
                        let key = key.as_str().ok_or("source is not a string")?;
                        let source = resolver
                            .resolve(key)
                            .ok_or_else(|| format!("missing source: {key}"))?;
                        for speed in [1.0, speed] {
                            let id = format!("{key}@{speed}");
                            if let std::collections::btree_map::Entry::Vacant(entry) =
                                results.entry(id)
                            {
                                entry.insert(cache.measure(&source, speed)?);
                            }
                        }
                    }
                }
            }
        }
        println!("{}", serde_json::to_string_pretty(&results)?);
        return Ok(());
    }
    if mode != "report" && mode != "compile" && mode != "analysis" {
        return Err("expected report, compile, analysis or measurements".into());
    }
    for event in raw["events"].as_array().ok_or("missing events")? {
        if event["group"].as_str().is_none_or(str::is_empty) {
            println!("UNGROUPED {}", event["id"]);
        }
    }
    let project = parse_project(&serde_json::to_string(&raw)?)?;
    let bank = compile_project(&project, &resolver)?;
    for warning in bank.played_peak_warnings() {
        eprintln!("WARNING {warning}");
    }
    if mode == "compile" {
        println!(
            "{}",
            String::from_utf8(sound_core::serialize_runtime_bank(&bank)?)?
        );
        return Ok(());
    }
    if mode == "analysis" {
        println!("{}", serde_json::to_string_pretty(&bank.played)?);
        return Ok(());
    }
    println!(
        "Target {:.1} LUFS; source gains are unrestricted; played peaks above -1.0 dBTP warn without changing gain. Levels include project gains and speed, before call-site dynamics and bus sliders.",
        project.normalization_lufs
    );
    println!(
        "STEP_SUM rows render all variant combinations dry and centered at 48 kHz; layer rows retain individual source diagnostics."
    );
    let mut events: Vec<_> = project.events.iter().collect();
    events.sort_by(|a, b| (&a.group, &a.id).cmp(&(&b.group, &b.id)));
    let mut previous = "";
    for event in events {
        let group = &project.groups[&event.group];
        if previous != event.group {
            println!(
                "\n[{}] gain={:+.2} dB bus={}",
                event.group, group.gain_db, group.bus
            );
            previous = &event.group;
        }
        println!(
            "{} offset={:+.2} dB reason={}",
            event.id,
            event.offset_db,
            event.reason.as_deref().unwrap_or("-")
        );
        for (si, step) in event.steps.iter().enumerate() {
            let played = &bank.played[&event.id][si];
            if let Some(sum) = &played.sum {
                println!(
                    "  STEP_SUM step={si} played={:.2} LUFS range={:.2}..{:.2} peak={:.2} dBTP combinations={}{}",
                    sum.median_lufs,
                    sum.min_lufs,
                    sum.max_lufs,
                    sum.true_peak_dbtp,
                    sum.combinations,
                    over(sum.true_peak_dbtp)
                );
            }
            for (li, layer) in step.layers.iter().enumerate() {
                let played = &played.layers[li];
                for source in &played.sources {
                    if source.true_peak_dbtp > sound_core::PLAYED_PEAK_WARNING_DBTP {
                        let norm = &bank.normalizations[&source.key];
                        println!(
                            "  SOURCE {} ABOVE_CEILING normalized={:.2} LUFS source_peak={:.2} dBTP played_peak={:.2} dBTP",
                            source.key,
                            norm.loudness_lufs + norm.gain_db,
                            norm.true_peak_dbtp + norm.gain_db,
                            source.true_peak_dbtp
                        );
                    }
                }
                if let Some(level) = &played.level {
                    println!(
                        "  step={si} layer={li} played={:.2} LUFS range={:.2}..{:.2} peak={:.2} dBTP balance={:+.2} dB reason={}{}",
                        level.median_lufs,
                        level.min_lufs,
                        level.max_lufs,
                        level.true_peak_dbtp,
                        layer.volume,
                        layer.reason.as_deref().unwrap_or("-"),
                        over(level.true_peak_dbtp)
                    );
                }
            }
        }
    }
    Ok(())
}

fn over(peak: f64) -> &'static str {
    if peak > sound_core::PLAYED_PEAK_WARNING_DBTP {
        " ABOVE_CEILING"
    } else {
        ""
    }
}
