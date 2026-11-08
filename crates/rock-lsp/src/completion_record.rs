//! Completion corpus recorder -- the replay corpus's server half.
//!
//! Gated on `ROCK_LSP_RECORD`: `1` -> `rock-completion-corpus.jsonl` in `/tmp`
//! (Windows: the system temp directory),
//! any other non-empty value -> that path (the `ROCK_LSP_DEBUG` convention).
//! Off = zero cost: no snapshots taken, no item commands attached.
//!
//! Flow: `stash_and_tag` snapshots each completion response (the buffer
//! exactly as the ranker saw it, the full ranked item list, phase timings)
//! into a small ring and rides a `rock.completionAccepted` command
//! `[record_id, index, label]` on every item. Per LSP, the client executes
//! `CompletionItem.command` after inserting the item; that arrives as
//! `workspace/executeCommand`, `handle_execute_command` joins the id back
//! to the stashed snapshot, and one self-contained JSONL row is appended.
//! Dismissed popups never write (their snapshots age out of the ring).

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use lsp_types::{Command, CompletionItem, Position};
use serde_json::json;

pub const ACCEPT_COMMAND: &str = "rock.completionAccepted";
/// An accept refers to one of the last few requests (per-keystroke
/// re-query while the popup is open); older snapshots are dead weight.
const RING_CAP: usize = 8;

struct Stashed {
    record_id: u64,
    ts_unix: u64,
    path: String,
    position: Position,
    prefix: String,
    buffer_text: String,
    items: Vec<serde_json::Value>,
    timings: serde_json::Value,
}

pub struct Recorder {
    out_path: PathBuf,
    ring: VecDeque<Stashed>,
    next_id: u64,
    /// git HEAD per directory (what the rest of the workspace looked like);
    /// `None` caches "not a repo" so git isn't respawned per accept.
    git_heads: HashMap<String, Option<String>>,
}

impl Recorder {
    pub fn new(out_path: PathBuf) -> Self {
        Self {
            out_path,
            ring: VecDeque::new(),
            next_id: 1,
            git_heads: HashMap::new(),
        }
    }

    pub fn stash(
        &mut self,
        path: String,
        position: Position,
        prefix: String,
        buffer_text: String,
        items: Vec<serde_json::Value>,
        timings: serde_json::Value,
    ) -> u64 {
        let record_id = self.next_id;
        self.next_id += 1;
        if self.ring.len() == RING_CAP {
            self.ring.pop_front();
        }
        let ts_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.ring.push_back(Stashed {
            record_id,
            ts_unix,
            path,
            position,
            prefix,
            buffer_text,
            items,
            timings,
        });
        record_id
    }

    /// Join an accepted item back to its stashed request and append the
    /// corpus row. Unknown ids (popup outlived the ring, or a stale client)
    /// are warned, never silently dropped.
    pub fn accept(&mut self, record_id: u64, index: Option<u64>, label: Option<&str>) {
        let Some(at) = self.ring.iter().position(|s| s.record_id == record_id) else {
            log::warn!(
                "completion-record: accept for unknown record_id {record_id} \
                 (ring holds {:?})",
                self.ring.iter().map(|s| s.record_id).collect::<Vec<_>>()
            );
            return;
        };
        let s = self.ring.remove(at).expect("position came from iter");

        let stashed_label = index
            .and_then(|i| s.items.get(i as usize))
            .and_then(|v| v.get("label"))
            .and_then(|v| v.as_str());
        if let (Some(sl), Some(cl)) = (stashed_label, label) {
            if sl != cl {
                log::warn!(
                    "completion-record: accept label {cl:?} != stashed items[{}] {sl:?} \
                     (record_id {record_id}); recording the client's label",
                    index.unwrap_or_default()
                );
            }
        }

        let dir = Path::new(&s.path)
            .parent()
            .map(|p| p.to_string_lossy().to_string());
        let git_head = dir.and_then(|d| self.git_head_for(&d));

        let row = json!({
            "v": 1,
            "source": "server",
            "ts_unix": s.ts_unix,
            "record_id": s.record_id,
            "path": s.path,
            "pos": { "line": s.position.line, "character": s.position.character },
            "prefix": s.prefix,
            "git_head": git_head,
            "buffer_text": s.buffer_text,
            "items": s.items,
            "accepted": { "index": index, "label": label },
            "timings": s.timings,
        });
        if let Err(e) = append_line(&self.out_path, &row) {
            log::warn!(
                "completion-record: failed to append to {}: {e}",
                self.out_path.display()
            );
        }
    }

    fn git_head_for(&mut self, dir: &str) -> Option<String> {
        if let Some(cached) = self.git_heads.get(dir) {
            return cached.clone();
        }
        let head = std::process::Command::new("git")
            .args(["-C", dir, "rev-parse", "HEAD"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string());
        self.git_heads.insert(dir.to_string(), head.clone());
        head
    }
}

fn append_line(path: &Path, row: &serde_json::Value) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(f, "{row}")
}

/// `Mutex<Option<..>>` (not `OnceLock`) so tests / a CLI can flip recording
/// on after the first `recording is off` read. One uncontended lock per
/// completion request when off -- noise next to the request itself.
static RECORDER: LazyLock<Mutex<Option<Recorder>>> = LazyLock::new(|| {
    Mutex::new(match std::env::var("ROCK_LSP_RECORD") {
        Ok(v) if v == "1" => Some(Recorder::new(crate::default_debug_file(
            "rock-completion-corpus.jsonl",
        ))),
        Ok(v) if !v.is_empty() && v != "0" => Some(Recorder::new(v.into())),
        _ => None,
    })
});

/// Point recording at `path`, replacing whatever the env configured.
#[doc(hidden)]
pub fn init_recorder_at(path: PathBuf) {
    match RECORDER.lock() {
        Ok(mut g) => *g = Some(Recorder::new(path)),
        Err(e) => log::warn!("completion-record: recorder lock poisoned: {e}"),
    }
}

/// Snapshot the completion request into the ring and tag every item with
/// the accept command. No-op (and no buffer copy) when recording is off.
pub fn stash_and_tag(
    bone: &bone_query::BoneWorkspace,
    file_path: &str,
    pos: Position,
    items: &mut [CompletionItem],
) {
    let mut guard = match RECORDER.lock() {
        Ok(g) => g,
        Err(e) => {
            log::warn!("completion-record: recorder lock poisoned: {e}");
            return;
        }
    };
    let Some(rec) = guard.as_mut() else { return };
    let Some(src) = bone.file_text(file_path) else {
        log::warn!("completion-record: no tracked text for {file_path}; skipping snapshot");
        return;
    };
    let prefix = bone_query::ident_prefix_at(src, pos).to_string();
    let t = bone_query::last_completion_phase_timings();
    let timings = json!({
        "candidates_ms": t.candidates_ms,
        "rank_ms": t.rank_ms,
        "reach_ms": t.reach_ms,
        "call_through_ms": t.call_through_ms,
        "n_general": t.n_general,
        "n_out": t.n_out,
    });
    let item_rows = items
        .iter()
        .map(|it| json!({ "label": it.label, "sortText": it.sort_text, "kind": it.kind }))
        .collect();
    let record_id = rec.stash(
        file_path.to_string(),
        pos,
        prefix,
        src.to_string(),
        item_rows,
        timings,
    );
    for (i, it) in items.iter_mut().enumerate() {
        it.command = Some(Command::new(
            String::new(),
            ACCEPT_COMMAND.to_string(),
            Some(vec![json!(record_id), json!(i), json!(it.label)]),
        ));
    }
}

/// The `workspace/executeCommand` endpoint; `rock.completionAccepted` is the
/// only command the server advertises.
pub fn handle_execute_command(
    params: &lsp_types::ExecuteCommandParams,
) -> Result<serde_json::Value, String> {
    if params.command != ACCEPT_COMMAND {
        return Err(format!("unknown command {:?}", params.command));
    }
    let Some(record_id) = params.arguments.first().and_then(|v| v.as_u64()) else {
        return Err("rock.completionAccepted: missing/invalid record_id argument".to_string());
    };
    let index = params.arguments.get(1).and_then(|v| v.as_u64());
    let label = params.arguments.get(2).and_then(|v| v.as_str());
    match RECORDER.lock() {
        Ok(mut g) => match g.as_mut() {
            Some(rec) => rec.accept(record_id, index, label),
            // A client can only hold tagged items if recording was on when
            // they were served; reaching here means it was turned off since.
            None => log::warn!("completion-record: accept received while recording is off"),
        },
        Err(e) => log::warn!("completion-record: recorder lock poisoned: {e}"),
    }
    Ok(serde_json::Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(label: &str) -> serde_json::Value {
        json!({ "label": label, "sortText": "0", "kind": 6 })
    }

    fn read_rows(path: &Path) -> Vec<serde_json::Value> {
        match std::fs::read_to_string(path) {
            Ok(s) => s
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    #[test]
    fn accept_joins_stash_and_appends_row() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("corpus.jsonl");
        let mut rec = Recorder::new(out.clone());
        let id = rec.stash(
            "/nowhere/main.rock".into(),
            Position::new(3, 7),
            "pg".into(),
            "fn r() { sfx(pg) }".into(),
            vec![item("player.movable.grid"), item("pg_other")],
            json!({ "rank_ms": 0.5 }),
        );
        rec.accept(id, Some(1), Some("pg_other"));
        let rows = read_rows(&out);
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r["record_id"], json!(id));
        assert_eq!(r["prefix"], json!("pg"));
        assert_eq!(r["buffer_text"], json!("fn r() { sfx(pg) }"));
        assert_eq!(r["pos"], json!({ "line": 3, "character": 7 }));
        assert_eq!(r["accepted"], json!({ "index": 1, "label": "pg_other" }));
        assert_eq!(r["items"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn unknown_id_and_evicted_stash_write_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("corpus.jsonl");
        let mut rec = Recorder::new(out.clone());
        rec.accept(999, Some(0), Some("x"));
        assert!(read_rows(&out).is_empty(), "unknown id must not write");

        let first = rec.stash(
            "/nowhere/a.rock".into(),
            Position::new(0, 0),
            String::new(),
            String::new(),
            vec![item("a")],
            json!({}),
        );
        for _ in 0..RING_CAP {
            rec.stash(
                "/nowhere/a.rock".into(),
                Position::new(0, 0),
                String::new(),
                String::new(),
                vec![item("b")],
                json!({}),
            );
        }
        rec.accept(first, Some(0), Some("a"));
        assert!(read_rows(&out).is_empty(), "evicted id must not write");
    }
}
