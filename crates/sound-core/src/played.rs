use std::{collections::BTreeMap, fmt};

use serde::Serialize;

use crate::{
    AssetResolver, MeasurementCache, PLAYED_PEAK_WARNING_DBTP, RenderLayer, SoundProject,
    SoundStep, SourceMeasurement, SourceNormalization, db_gain, layer_gain_db,
};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlayedLevel {
    pub median_lufs: f64,
    pub min_lufs: f64,
    pub max_lufs: f64,
    pub true_peak_dbtp: f64,
    pub combinations: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlayedSource {
    pub key: String,
    pub loudness_lufs: f64,
    pub true_peak_dbtp: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlayedLayer {
    pub sources: Vec<PlayedSource>,
    pub level: Option<PlayedLevel>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlayedStep {
    pub layers: Vec<PlayedLayer>,
    pub sum: Option<PlayedLevel>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlayedPeakWarning {
    pub event: String,
    pub step: usize,
    pub layer: Option<usize>,
    pub true_peak_dbtp: f64,
}

impl fmt::Display for PlayedPeakWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} step={}", self.event, self.step)?;
        if let Some(layer) = self.layer {
            write!(f, " layer={layer}")?;
        } else {
            write!(f, " layered sum")?;
        }
        write!(
            f,
            " played peak {:+.6} dBTP exceeds {:.1} dBTP; author review (gain unchanged)",
            self.true_peak_dbtp, PLAYED_PEAK_WARNING_DBTP
        )
    }
}

fn summarize(values: &[(f64, f64)]) -> Option<PlayedLevel> {
    if values.is_empty() {
        return None;
    }
    let mut loudness: Vec<_> = values.iter().map(|v| v.0).collect();
    loudness.sort_by(f64::total_cmp);
    Some(PlayedLevel {
        median_lufs: (loudness[(loudness.len() - 1) / 2] + loudness[loudness.len() / 2]) / 2.0,
        min_lufs: loudness[0],
        max_lufs: loudness[loudness.len() - 1],
        true_peak_dbtp: values.iter().map(|v| v.1).fold(f64::NEG_INFINITY, f64::max),
        combinations: values.len(),
    })
}

pub fn measure_step<R: AssetResolver + ?Sized>(
    step: &SoundStep,
    gains: &[Vec<f64>],
    resolver: &R,
    cache: &MeasurementCache,
) -> Result<Vec<SourceMeasurement>, String> {
    if step.layers.len() != gains.len()
        || step
            .layers
            .iter()
            .zip(gains)
            .any(|(layer, gains)| layer.sources.len() != gains.len())
    {
        return Err("played step gains do not match its sources".into());
    }
    let mut combinations = vec![Vec::<RenderLayer>::new()];
    for (li, layer) in step.layers.iter().enumerate() {
        let mut next = Vec::new();
        for prior in &combinations {
            for (si, source) in layer.sources.iter().enumerate() {
                let mut combination = prior.clone();
                combination.push(RenderLayer {
                    path: resolver
                        .resolve(source)
                        .ok_or_else(|| format!("missing source: {source}"))?,
                    gain: gains[li][si],
                    speed: f64::from(layer.speed),
                    offset: f64::from(layer.offset),
                });
                next.push(combination);
                if next.len() > 4096 {
                    return Err("played step exceeds 4096 variant combinations".into());
                }
            }
        }
        combinations = next;
    }
    combinations
        .iter()
        .map(|layers| cache.measure_mix(layers))
        .collect()
}

pub(crate) fn analyze_project<R: AssetResolver + ?Sized>(
    project: &SoundProject,
    normalizations: &BTreeMap<String, SourceNormalization>,
    resolver: &R,
    cache: &MeasurementCache,
) -> Result<BTreeMap<String, Vec<PlayedStep>>, String> {
    let mut result = BTreeMap::new();
    for event in &project.events {
        let mut steps = Vec::new();
        for step in &event.steps {
            let mut layers = Vec::new();
            let mut available = step.clone();
            let mut gains = Vec::new();
            for (li, layer) in step.layers.iter().enumerate() {
                let mut sources = Vec::new();
                let mut layer_gains = Vec::new();
                available.layers[li]
                    .sources
                    .retain(|key| normalizations.contains_key(key));
                let role_db = layer_gain_db(project, event, layer);
                for key in &available.layers[li].sources {
                    let normalization = &normalizations[key];
                    let (loudness, peak) = if layer.speed == 1.0 {
                        (normalization.loudness_lufs, normalization.true_peak_dbtp)
                    } else {
                        let path = resolver
                            .resolve(key)
                            .ok_or_else(|| format!("source disappeared: {key}"))?;
                        let measurement = cache.measure(&path, f64::from(layer.speed))?;
                        (
                            measurement.loudness(normalization.looped)?,
                            measurement.true_peak_dbtp,
                        )
                    };
                    let gain_db = normalization.gain_db + role_db;
                    sources.push(PlayedSource {
                        key: key.clone(),
                        loudness_lufs: loudness + gain_db,
                        true_peak_dbtp: peak + gain_db,
                    });
                    layer_gains.push(normalization.gain * db_gain(role_db));
                }
                let level = summarize(
                    &sources
                        .iter()
                        .map(|s| (s.loudness_lufs, s.true_peak_dbtp))
                        .collect::<Vec<_>>(),
                );
                layers.push(PlayedLayer { sources, level });
                gains.push(layer_gains);
            }
            // Partial development banks still sum every layer that can play.
            let (render_layers, render_gains): (Vec<_>, Vec<_>) = available
                .layers
                .into_iter()
                .zip(gains)
                .filter(|(layer, _)| !layer.sources.is_empty())
                .unzip();
            let sum = if render_layers.len() > 1 {
                let rendered = measure_step(
                    &SoundStep {
                        layers: render_layers,
                    },
                    &render_gains,
                    resolver,
                    cache,
                )?;
                summarize(
                    &rendered
                        .iter()
                        .map(|m| (m.momentary_lufs, m.true_peak_dbtp))
                        .collect::<Vec<_>>(),
                )
            } else {
                None
            };
            steps.push(PlayedStep { layers, sum });
        }
        result.insert(event.id.clone(), steps);
    }
    Ok(result)
}

pub(crate) fn warnings(played: &BTreeMap<String, Vec<PlayedStep>>) -> Vec<PlayedPeakWarning> {
    let mut result = Vec::new();
    for (event, steps) in played {
        for (si, step) in steps.iter().enumerate() {
            let levels = if let Some(sum) = &step.sum {
                vec![(None, sum)]
            } else {
                step.layers
                    .iter()
                    .enumerate()
                    .filter_map(|(li, layer)| layer.level.as_ref().map(|level| (Some(li), level)))
                    .collect()
            };
            for (layer, level) in levels {
                if level.true_peak_dbtp > PLAYED_PEAK_WARNING_DBTP {
                    result.push(PlayedPeakWarning {
                        event: event.clone(),
                        step: si,
                        layer,
                        true_peak_dbtp: level.true_peak_dbtp,
                    });
                }
            }
        }
    }
    result
}
