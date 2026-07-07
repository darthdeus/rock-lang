//! `rock completion-replay <corpus.jsonl>...` -- re-rank recorded completion
//! accepts and report MRR (docs/completion-v2-design.md, M6). Each accept row
//! carries the buffer exactly as it was when the user completed; replay
//! applies that buffer to a live workspace, re-runs `bone.completion`, and
//! finds where the accepted item ranks NOW. Tuning loop: replay, change a
//! constant, replay again, diff the per-case output / compare MRR.
//!
//! Two row shapes are accepted (rows lacking `buffer_text` are counted and
//! skipped -- pre-buffer client logs remain measurement-only):
//!   - server corpus rows (rock-lsp `ROCK_LSP_RECORD`): `path`, `pos`,
//!     `buffer_text`, `accepted.label`
//!   - client log accept rows (rock.nvim `completion_log`): `event=="accept"`,
//!     `uri`, `pos`, `buffer_text`, `chosen.label`
//! Pass ONE source per run -- the same accept recorded by both sides would
//! count twice (join key: client `chosen.record_id` == server `record_id`).
//!
//! Per case, the disk text is restored after the probe so a mid-edit
//! (often non-parsing) recorded buffer can't bleed into later cases.

use anyhow::Result;
use bone_query::BoneWorkspace;
use lsp_types::Position;
use std::io::Write;

struct ReplayCase {
    path: String,
    pos: Position,
    prefix: String,
    label: String,
    buffer_text: String,
}

enum ParsedRow {
    Case(ReplayCase),
    /// An accept without `buffer_text` (recorded before buffers landed).
    AcceptNoBuffer,
    /// Dismiss / malformed / anything not an accept.
    Other,
}

#[derive(Default)]
pub struct ReplaySummary {
    pub rows: usize,
    pub replayed: usize,
    pub skipped_no_buffer: usize,
    pub skipped_unreadable: usize,
    pub hits_at: [usize; 3], // @1, @3, @10
    pub misses: usize,
    pub mrr: f64,
}

fn parse_row(row: &serde_json::Value) -> ParsedRow {
    let (path, label) = if row.get("source").and_then(|v| v.as_str()) == Some("server") {
        (
            row.get("path").and_then(|v| v.as_str()),
            row.pointer("/accepted/label").and_then(|v| v.as_str()),
        )
    } else if row.get("event").and_then(|v| v.as_str()) == Some("accept") {
        (
            row.get("uri")
                .and_then(|v| v.as_str())
                .map(|u| u.strip_prefix("file://").unwrap_or(u)),
            row.pointer("/chosen/label").and_then(|v| v.as_str()),
        )
    } else {
        return ParsedRow::Other;
    };
    let (Some(path), Some(label)) = (path, label) else { return ParsedRow::Other };
    let (Some(line), Some(character)) = (
        row.pointer("/pos/line").and_then(|v| v.as_u64()),
        row.pointer("/pos/character").and_then(|v| v.as_u64()),
    ) else {
        return ParsedRow::Other;
    };
    let Some(buffer_text) = row.get("buffer_text").and_then(|v| v.as_str()) else {
        return ParsedRow::AcceptNoBuffer;
    };
    ParsedRow::Case(ReplayCase {
        path: path.to_string(),
        pos: Position::new(line as u32, character as u32),
        prefix: row.get("prefix").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        label: label.to_string(),
        buffer_text: buffer_text.to_string(),
    })
}

fn replay_cases(
    corpus_paths: &[String],
    out: &mut impl Write,
) -> Result<ReplaySummary> {
    let mut sum = ReplaySummary::default();
    let mut bone = BoneWorkspace::new();

    for corpus in corpus_paths {
        let text = fs_err::read_to_string(corpus)?;
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            sum.rows += 1;
            let row: serde_json::Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("completion-replay: skipping malformed row: {e}");
                    continue;
                }
            };
            let case = match parse_row(&row) {
                ParsedRow::Case(c) => c,
                ParsedRow::AcceptNoBuffer => {
                    sum.skipped_no_buffer += 1;
                    continue;
                }
                ParsedRow::Other => continue,
            };
            // The recorded buffer is mid-edit; the disk copy is what the
            // REST of the replay should see afterwards.
            let disk = match fs_err::read_to_string(&case.path) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("completion-replay: {}: {e}; skipping", case.path);
                    sum.skipped_unreadable += 1;
                    continue;
                }
            };
            bone.update(&case.path, case.buffer_text.clone());
            let items = bone.completion(&case.path, case.pos);
            bone.update(&case.path, disk);

            let rank = items.iter().position(|it| it.label == case.label).map(|i| i + 1);
            sum.replayed += 1;
            let rr = match rank {
                Some(r) => {
                    if r <= 1 { sum.hits_at[0] += 1; }
                    if r <= 3 { sum.hits_at[1] += 1; }
                    if r <= 10 { sum.hits_at[2] += 1; }
                    1.0 / r as f64
                }
                None => {
                    sum.misses += 1;
                    0.0
                }
            };
            writeln!(
                out,
                "{rr:.4} rank={} {}:{}:{} prefix=\"{}\" label=\"{}\"",
                rank.map(|r| r.to_string()).unwrap_or_else(|| "miss".to_string()),
                case.path,
                case.pos.line + 1,
                case.pos.character + 1,
                case.prefix,
                case.label,
            )?;
            sum.mrr += rr;
        }
    }
    if sum.replayed > 0 {
        sum.mrr /= sum.replayed as f64;
    }
    Ok(sum)
}

pub fn completion_replay(corpus_paths: &[String]) -> Result<()> {
    if corpus_paths.is_empty() {
        anyhow::bail!("completion-replay: pass at least one corpus .jsonl");
    }
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    let sum = replay_cases(corpus_paths, &mut out)?;
    out.flush()?;
    eprintln!(
        "completion-replay: {} rows, {} replayed ({} no-buffer, {} unreadable skipped)",
        sum.rows, sum.replayed, sum.skipped_no_buffer, sum.skipped_unreadable
    );
    eprintln!(
        "hit@1 {}/{r}  hit@3 {}/{r}  hit@10 {}/{r}  misses {}  MRR {:.4}",
        sum.hits_at[0], sum.hits_at[1], sum.hits_at[2], sum.misses, sum.mrr,
        r = sum.replayed
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "\
struct IVec2 { x: int }
struct Movable { grid: IVec2 }
struct Player { movable: Movable }
fn sfx(g: IVec2) {}
fn r(player: Player) {
  sfx(pg)
}
";

    /// Three replayable accepts (server row picking the live #1, client row
    /// picking the live #1, server row picking a label that can't exist) +
    /// one dismiss + one pre-buffer accept. Pins the harness plumbing --
    /// row parsing for both shapes, rank lookup, skip counting, MRR math --
    /// with the expected top label derived from the live ranker so ranking
    /// changes don't break it.
    #[test]
    fn replays_both_row_shapes_and_aggregates_mrr() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.rock");
        fs_err::write(&path, SRC).unwrap();
        let path_str = fs_err::canonicalize(&path).unwrap().to_string_lossy().to_string();

        let mut probe = BoneWorkspace::new();
        probe.update(&path_str, SRC.to_string());
        let top = probe
            .completion(&path_str, Position::new(5, 8))
            .first()
            .map(|it| it.label.clone())
            .expect("fixture completes for `pg`");

        let corpus = dir.path().join("corpus.jsonl");
        let rows = [
            serde_json::json!({
                "source": "server", "path": path_str,
                "pos": { "line": 5, "character": 8 }, "prefix": "pg",
                "buffer_text": SRC,
                "accepted": { "index": 0, "label": top },
            }),
            serde_json::json!({
                "event": "accept", "uri": format!("file://{path_str}"),
                "pos": { "line": 5, "character": 8 }, "prefix": "pg",
                "buffer_text": SRC,
                "chosen": { "label": top },
            }),
            serde_json::json!({
                "source": "server", "path": path_str,
                "pos": { "line": 5, "character": 8 }, "prefix": "pg",
                "buffer_text": SRC,
                "accepted": { "index": 7, "label": "no_such_label_ever" },
            }),
            serde_json::json!({ "event": "dismiss", "uri": format!("file://{path_str}") }),
            serde_json::json!({
                "event": "accept", "uri": format!("file://{path_str}"),
                "pos": { "line": 5, "character": 8 },
                "chosen": { "label": top },
            }),
        ];
        let jsonl: String = rows.iter().map(|r| format!("{r}\n")).collect();
        fs_err::write(&corpus, jsonl).unwrap();

        let mut out = Vec::new();
        let sum = replay_cases(&[corpus.to_string_lossy().to_string()], &mut out).unwrap();

        assert_eq!(sum.rows, 5);
        assert_eq!(sum.replayed, 3);
        assert_eq!(sum.skipped_no_buffer, 1, "pre-buffer accept counted, not replayed");
        assert_eq!(sum.hits_at[0], 2, "both live-#1 picks replay at rank 1");
        assert_eq!(sum.misses, 1);
        assert!((sum.mrr - 2.0 / 3.0).abs() < 1e-9, "MRR (1 + 1 + 0)/3, got {}", sum.mrr);

        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.lines().count(), 3, "one output line per replayed case");
        assert!(text.contains("rank=miss"), "miss rendered:\n{text}");
        assert!(text.contains("rank=1"), "hit rendered:\n{text}");
    }
}
