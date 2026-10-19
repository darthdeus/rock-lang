//! `rock completion-episodes <log.jsonl>...` -- stitch completion-log rows
//! into EPISODES of user struggle and print the tuning metrics
//! (the compiler contract). Pure offline analysis of the client log
//! (`rock.nvim` completion_log rows); no workspace, no replay.
//!
//! An episode groups time- and anchor-proximate popups at one site and
//! resolves accept / typed_out / abandoned; the final word at the anchor is
//! its ground truth. Stitching heuristics live HERE (not in the logger) so
//! they can be iterated against old data. Pre-enrichment rows degrade
//! gracefully: the anchor falls back to `pos - len(prefix)`, the timeline
//! to the row's own `lsp.top`.

use anyhow::Result;
use std::collections::HashMap;
use std::io::Write;

/// Popups further apart than this are different struggles.
const EPISODE_GAP_S: i64 = 15;
/// Edits shift columns a little within a struggle; same word site anyway.
const ANCHOR_COL_TOLERANCE: i64 = 2;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Event {
    Accept,
    Dismiss,
}

struct TlEntry {
    prefix: String,
    top: Vec<String>,
}

struct Row {
    ts: i64,
    uri: String,
    anchor_line: i64,
    anchor_col: i64,
    event: Event,
    post_word: Option<String>,
    chosen_label: Option<String>,
    rank_in_cmp: Option<u64>,
    timeline: Vec<TlEntry>,
}

pub enum Resolution {
    Accept { label: String, rank_in_cmp: Option<u64> },
    TypedOut { word: String },
    Abandoned,
}

pub struct Episode {
    pub uri: String,
    pub line: i64,
    pub popups: usize,
    pub resolution: Resolution,
    /// Typed chars until the ground truth entered the displayed top 3;
    /// None = it never did.
    pub chars_to_top3: Option<usize>,
}

// -- parsing ---------------------------------------------------------------

/// "YYYY-MM-DDTHH:MM:SS" -> unix seconds (no chrono; gaps only need seconds).
fn parse_ts(s: &str) -> Option<i64> {
    let num = |r: std::ops::Range<usize>| s.get(r).and_then(|x| x.parse::<i64>().ok());
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, se) = (num(11..13)?, num(14..16)?, num(17..19)?);
    // Howard Hinnant's days_from_civil.
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + h * 3600 + mi * 60 + se)
}

fn parse_row(v: &serde_json::Value) -> Option<Row> {
    let event = match v.get("event").and_then(|e| e.as_str()) {
        Some("accept") => Event::Accept,
        Some("dismiss") => Event::Dismiss,
        _ => return None,
    };
    let prefix = v.get("prefix").and_then(|p| p.as_str()).unwrap_or("").to_string();
    let pos_line = v.pointer("/pos/line").and_then(|x| x.as_i64());
    let pos_col = v.pointer("/pos/character").and_then(|x| x.as_i64());
    // Pre-enrichment rows have no anchor; the identifier start is derivable.
    let anchor_line = v.pointer("/anchor/line").and_then(|x| x.as_i64()).or(pos_line)?;
    let anchor_col = v
        .pointer("/anchor/col")
        .and_then(|x| x.as_i64())
        .or_else(|| pos_col.map(|c| c - prefix.chars().count() as i64))?;
    let mut timeline: Vec<TlEntry> = v
        .get("timeline")
        .and_then(|t| t.as_array())
        .map(|entries| {
            entries
                .iter()
                .map(|e| TlEntry {
                    prefix: e.get("prefix").and_then(|p| p.as_str()).unwrap_or("").to_string(),
                    top: e
                        .get("top")
                        .and_then(|t| t.as_array())
                        .map(|ls| {
                            ls.iter().filter_map(|l| l.as_str().map(str::to_string)).collect()
                        })
                        .unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default();
    if timeline.is_empty() {
        // Old rows: the row's own lsp.top is a single pseudo-refresh.
        let top: Vec<String> = v
            .pointer("/lsp/top")
            .and_then(|t| t.as_array())
            .map(|ls| {
                ls.iter()
                    .filter_map(|l| l.get("label").and_then(|s| s.as_str()).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if !top.is_empty() {
            timeline.push(TlEntry { prefix: prefix.clone(), top });
        }
    }
    Some(Row {
        ts: v.get("ts").and_then(|t| t.as_str()).and_then(parse_ts).unwrap_or(0),
        uri: v.get("uri").and_then(|u| u.as_str()).unwrap_or("").to_string(),
        anchor_line,
        anchor_col,
        event,
        post_word: v.get("post_word").and_then(|w| w.as_str()).map(str::to_string),
        chosen_label: v.pointer("/chosen/label").and_then(|l| l.as_str()).map(str::to_string),
        rank_in_cmp: v.pointer("/chosen/rank_in_cmp").and_then(|r| r.as_u64()),
        timeline,
    })
}

// -- stitching + classification ---------------------------------------------

fn same_episode(prev: &Row, next: &Row) -> bool {
    prev.uri == next.uri
        && prev.anchor_line == next.anchor_line
        && (prev.anchor_col - next.anchor_col).abs() <= ANCHOR_COL_TOLERANCE
        && (next.ts - prev.ts) <= EPISODE_GAP_S
}

/// Everything up to the first non-ident char: `spawn_mob(x)` -> `spawn_mob`,
/// `MobKind::Imp` -> `MobKind`. How a typed word is matched against labels.
fn leading_ident(label: &str) -> &str {
    let end = label
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(label.len());
    &label[..end]
}

fn resolve(rows: &[Row]) -> Resolution {
    if let Some(acc) = rows.iter().find(|r| r.event == Event::Accept) {
        return Resolution::Accept {
            label: acc.chosen_label.clone().unwrap_or_default(),
            rank_in_cmp: acc.rank_in_cmp,
        };
    }
    let word = rows.iter().rev().find_map(|r| r.post_word.clone()).unwrap_or_default();
    if word.is_empty() {
        Resolution::Abandoned
    } else {
        Resolution::TypedOut { word }
    }
}

fn chars_to_top3(rows: &[Row], resolution: &Resolution) -> Option<usize> {
    // Ground truth to look for in the displayed lists. Accepted labels are
    // matched verbatim; a typed word matches a label's leading identifier.
    let matches: Box<dyn Fn(&str) -> bool> = match resolution {
        Resolution::Accept { label, .. } => {
            let label = label.clone();
            Box::new(move |l: &str| l == label)
        }
        Resolution::TypedOut { word } => {
            let word = word.clone();
            Box::new(move |l: &str| leading_ident(l) == word)
        }
        Resolution::Abandoned => return None,
    };
    for row in rows {
        for entry in &row.timeline {
            if entry.top.iter().take(3).any(|l| matches(l)) {
                return Some(entry.prefix.chars().count());
            }
        }
    }
    None
}

fn stitch(rows: Vec<Row>) -> Vec<Episode> {
    let mut episodes: Vec<Vec<Row>> = Vec::new();
    for row in rows {
        match episodes.last_mut() {
            Some(ep) if same_episode(ep.last().unwrap(), &row) => ep.push(row),
            _ => episodes.push(vec![row]),
        }
    }
    episodes
        .into_iter()
        .map(|rows| {
            let resolution = resolve(&rows);
            Episode {
                uri: rows[0].uri.clone(),
                line: rows[0].anchor_line,
                popups: rows.len(),
                chars_to_top3: chars_to_top3(&rows, &resolution),
                resolution,
            }
        })
        .collect()
}

// -- report ------------------------------------------------------------------

pub struct Report {
    pub episodes: Vec<Episode>,
    pub accepts: usize,
    pub typed_out: usize,
    pub abandoned: usize,
    /// Accepted at cmp index > 1: (label, rank, uri, line).
    pub hunted: Vec<(String, u64, String, i64)>,
    /// typed_out ground truths that NEVER hit the displayed top 3, by count.
    pub never_surfaced: Vec<(String, usize)>,
    /// Ground-truth words with >= 2 multi-popup episodes, by episode count.
    pub repeat_struggle: Vec<(String, usize)>,
}

fn analyze(rows: Vec<Row>) -> Report {
    let episodes = stitch(rows);
    let (mut accepts, mut typed_out, mut abandoned) = (0, 0, 0);
    let mut hunted = Vec::new();
    let mut never: HashMap<String, usize> = HashMap::new();
    let mut struggle: HashMap<String, usize> = HashMap::new();
    for ep in &episodes {
        match &ep.resolution {
            Resolution::Accept { label, rank_in_cmp } => {
                accepts += 1;
                if let Some(r) = rank_in_cmp {
                    if *r > 1 {
                        hunted.push((label.clone(), *r, ep.uri.clone(), ep.line));
                    }
                }
                if ep.popups >= 2 {
                    *struggle.entry(leading_ident(label).to_string()).or_default() += 1;
                }
            }
            Resolution::TypedOut { word } => {
                typed_out += 1;
                if ep.chars_to_top3.is_none() {
                    *never.entry(word.clone()).or_default() += 1;
                }
                if ep.popups >= 2 {
                    *struggle.entry(word.clone()).or_default() += 1;
                }
            }
            Resolution::Abandoned => abandoned += 1,
        }
    }
    hunted.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut never_surfaced: Vec<_> = never.into_iter().collect();
    never_surfaced.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut repeat_struggle: Vec<_> =
        struggle.into_iter().filter(|(_, n)| *n >= 2).collect();
    repeat_struggle.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    Report { episodes, accepts, typed_out, abandoned, hunted, never_surfaced, repeat_struggle }
}

fn parse_files(paths: &[String]) -> Result<Vec<Row>> {
    let mut rows = Vec::new();
    for p in paths {
        let text = fs_err::read_to_string(p)?;
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<serde_json::Value>(line) {
                Ok(v) => rows.extend(parse_row(&v)),
                Err(e) => eprintln!("completion-episodes: skipping malformed row: {e}"),
            }
        }
    }
    Ok(rows)
}

pub fn completion_episodes(paths: &[String]) -> Result<()> {
    if paths.is_empty() {
        anyhow::bail!("completion-episodes: pass at least one completion-log .jsonl");
    }
    let rows = parse_files(paths)?;
    let n_rows = rows.len();
    let rep = analyze(rows);
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());

    writeln!(
        out,
        "episodes: {} from {} rows (accept {}, typed_out {}, abandoned {})",
        rep.episodes.len(), n_rows, rep.accepts, rep.typed_out, rep.abandoned
    )?;

    // chars-to-top3 distribution over resolved episodes.
    let mut hist: HashMap<usize, usize> = HashMap::new();
    let mut resolved = 0usize;
    let mut never_n = 0usize;
    for ep in &rep.episodes {
        if matches!(ep.resolution, Resolution::Abandoned) {
            continue;
        }
        resolved += 1;
        match ep.chars_to_top3 {
            Some(c) => *hist.entry(c).or_default() += 1,
            None => never_n += 1,
        }
    }
    let mut buckets: Vec<_> = hist.into_iter().collect();
    buckets.sort();
    writeln!(out, "\nchars-to-top3 ({resolved} resolved episodes, {never_n} never surfaced):")?;
    for (chars, n) in buckets {
        writeln!(out, "  {chars:>3} chars  {n}")?;
    }

    writeln!(out, "\nnever surfaced (typed out, ground truth never in displayed top 3):")?;
    for (word, n) in rep.never_surfaced.iter().take(25) {
        writeln!(out, "  {n:>4}x  {word}")?;
    }

    writeln!(out, "\nhunted accepts (picked at cmp index > 1):")?;
    for (label, rank, uri, line) in rep.hunted.iter().take(25) {
        writeln!(out, "  #{rank}  {label}  {uri}:{}", line + 1)?;
    }

    writeln!(out, "\nrepeat-struggle words (>= 2 multi-popup episodes):")?;
    for (word, n) in rep.repeat_struggle.iter().take(25) {
        writeln!(out, "  {n:>4}x  {word}")?;
    }
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dismiss(
        ts: &str, uri: &str, line: i64, col: i64, prefix: &str,
        post_word: Option<&str>, timeline: serde_json::Value,
    ) -> serde_json::Value {
        serde_json::json!({
            "event": "dismiss", "ts": ts, "uri": uri, "prefix": prefix,
            "anchor": { "line": line, "col": col },
            "pos": { "line": line, "character": col + prefix.len() as i64 },
            "post_word": post_word,
            "timeline": timeline,
        })
    }

    fn rows_from(vals: &[serde_json::Value]) -> Vec<Row> {
        vals.iter().filter_map(parse_row).collect()
    }

    #[test]
    fn stitches_by_anchor_and_gap_and_classifies_resolutions() {
        let tl = |prefix: &str, top: &[&str]| serde_json::json!([{ "t": 0, "prefix": prefix, "top": top }]);
        let vals = [
            // Episode 1: two popups at the same anchor 2s apart; the user
            // types the word out; the eventual word enters displayed top-3
            // on the second popup at prefix len 3.
            dismiss("2026-07-07T10:00:00", "file:///a.rock", 4, 10, "sp",
                Some("spawn_mob"), tl("sp", &["speed", "space", "sprite"])),
            dismiss("2026-07-07T10:00:02", "file:///a.rock", 4, 10, "spa",
                Some("spawn_mob"), tl("spa", &["space", "spawn_mob(x)", "sparse"])),
            // Episode 2: same anchor but past the gap -> NEW episode; the
            // accept was hunted to cmp index 4. Multi-popup (dismiss first).
            dismiss("2026-07-07T10:01:00", "file:///a.rock", 4, 10, "mo",
                None, tl("mo", &["mob", "move", "mode"])),
            serde_json::json!({
                "event": "accept", "ts": "2026-07-07T10:01:03",
                "uri": "file:///a.rock", "prefix": "mob_k",
                "anchor": { "line": 4, "col": 10 },
                "pos": { "line": 4, "character": 15 },
                "chosen": { "label": "mob_kind_data", "rank_in_cmp": 4 },
                "timeline": [{ "t": 0, "prefix": "mob_k",
                               "top": ["mob_key", "mob_kind_data", "mob_count"] }],
            }),
            // Episode 3: different anchor, no post_word -> abandoned.
            dismiss("2026-07-07T10:01:05", "file:///b.rock", 9, 2, "x",
                None, serde_json::json!([])),
            // Episode 4: typed out, never in displayed top 3.
            dismiss("2026-07-07T10:02:00", "file:///b.rock", 12, 0, "ta",
                Some("target_cycle"), tl("ta", &["table", "tags", "take"])),
        ];
        let rep = analyze(rows_from(&vals));

        assert_eq!(rep.episodes.len(), 4, "gap + anchor stitching");
        assert_eq!(rep.episodes[0].popups, 2);
        assert_eq!((rep.accepts, rep.typed_out, rep.abandoned), (1, 2, 1));

        assert!(matches!(&rep.episodes[0].resolution,
            Resolution::TypedOut { word } if word == "spawn_mob"));
        assert_eq!(rep.episodes[0].chars_to_top3, Some(3),
            "spawn_mob(x) hits displayed top 3 at prefix `spa`");

        assert!(matches!(&rep.episodes[1].resolution,
            Resolution::Accept { label, rank_in_cmp: Some(4) } if label == "mob_kind_data"));
        assert_eq!(rep.hunted.len(), 1, "cmp index 4 accept is a hunted accept");
        assert_eq!(rep.hunted[0].1, 4);

        assert_eq!(rep.episodes[3].chars_to_top3, None);
        assert_eq!(rep.never_surfaced, vec![("target_cycle".to_string(), 1)]);
    }

    #[test]
    fn pre_enrichment_rows_fall_back_to_pos_and_lsp_top() {
        // Old rows: no anchor/timeline/post_word. Anchor derives from
        // pos - len(prefix); the row's lsp.top is the pseudo-timeline.
        let old_accept = serde_json::json!({
            "event": "accept", "ts": "2026-06-23T02:23:22",
            "uri": "file:///old.rock", "prefix": "mob",
            "pos": { "line": 7, "character": 13 },
            "chosen": { "label": "mob_count", "rank_in_cmp": 1 },
            "lsp": { "top": [
                { "label": "mob_count", "sort_text": "0" },
                { "label": "mob_kind", "sort_text": "1" },
            ]},
        });
        let rep = analyze(rows_from(&[old_accept]));
        assert_eq!(rep.episodes.len(), 1);
        assert_eq!(rep.episodes[0].chars_to_top3, Some(3), "lsp.top pseudo-timeline");
        assert!(rep.hunted.is_empty(), "rank 1 is not hunted");
    }

    #[test]
    fn repeat_struggle_counts_multi_popup_episodes_per_word() {
        let tl = serde_json::json!([]);
        let mk = |t: &str, t2: &str| {
            [
                dismiss(t, "file:///a.rock", 1, 0, "gr", Some("grid_pos"), tl.clone()),
                dismiss(t2, "file:///a.rock", 1, 0, "gri", Some("grid_pos"), tl.clone()),
            ]
        };
        let mut vals: Vec<serde_json::Value> = mk("2026-07-07T10:00:00", "2026-07-07T10:00:01").into();
        vals.extend(mk("2026-07-07T11:00:00", "2026-07-07T11:00:01"));
        let rep = analyze(rows_from(&vals));
        assert_eq!(rep.episodes.len(), 2, "an hour apart = two episodes");
        assert_eq!(rep.repeat_struggle, vec![("grid_pos".to_string(), 2)]);
    }
}
