//! Opt-in compile budget records: `ROCK_COMPILE_STATS=<file>` appends one JSON
//! line per compile. The record's `budget` is a tree in which every node's
//! `wall_ms` is the sum of its children, with an explicit `other` child for
//! the remainder, so a serial tail cannot hide between two timers. Phase
//! boundaries are wall time; codegen also carries the summed per-unit time of
//! each emit pass (`unit_cpu`), which overlaps across workers and is
//! comparable between passes and commits, not with a wall window.
//!
//! `perf/run.py` reads these records into the compiler's performance history.

use bone_core::prelude::Instant;
use serde_json::{Value, json};
use std::cell::RefCell;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

thread_local! {
    static PENDING: RefCell<Option<Pending>> = const { RefCell::new(None) };
}

struct Pending {
    path: PathBuf,
    started: Instant,
    boundary: Instant,
    record: Value,
    phases: Vec<Node>,
}

/// One budget node before serialization; children are already exact.
struct Node {
    name: String,
    wall: Duration,
    children: Option<Vec<Node>>,
    extra: serde_json::Map<String, Value>,
}

impl Node {
    fn leaf(name: &str, wall: Duration) -> Self {
        Node {
            name: name.to_string(),
            wall,
            children: None,
            extra: serde_json::Map::new(),
        }
    }

    /// A parent whose children were measured inside its own window: the
    /// remainder becomes `other` so the children sum to the parent.
    fn parent(name: &str, wall: Duration, mut children: Vec<Node>) -> Self {
        let sum: Duration = children.iter().map(|c| c.wall).sum();
        children.push(Node::leaf("other", wall.saturating_sub(sum)));
        Node {
            name: name.to_string(),
            wall: sum.max(wall),
            children: Some(children),
            extra: serde_json::Map::new(),
        }
    }

    /// A parent that is nothing but its children.
    fn sum_of(name: &str, children: Vec<Node>) -> Self {
        let wall = children.iter().map(|c| c.wall).sum();
        Node {
            name: name.to_string(),
            wall,
            children: Some(children),
            extra: serde_json::Map::new(),
        }
    }

    fn into_json(self) -> Value {
        let mut map = serde_json::Map::new();
        map.insert("name".into(), json!(self.name));
        map.insert("wall_ms".into(), json!(ms(self.wall)));
        for (key, value) in self.extra {
            map.insert(key, value);
        }
        if let Some(children) = self.children {
            map.insert(
                "children".into(),
                Value::Array(children.into_iter().map(Node::into_json).collect()),
            );
        }
        Value::Object(map)
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn threads() -> usize {
    #[cfg(not(feature = "sequential"))]
    {
        rayon::current_num_threads()
    }
    #[cfg(feature = "sequential")]
    {
        1
    }
}

/// The outer compile owns the record; a nested `begin` is a no-op so helpers
/// cannot duplicate it. A compile that unwinds still publishes its phases.
pub(crate) struct Scope(bool);

impl Scope {
    pub(crate) fn begin(entry: &str) -> Self {
        if PENDING.with(|pending| pending.borrow().is_some()) {
            return Self(false);
        }
        let Some(path) = std::env::var_os("ROCK_COMPILE_STATS").filter(|path| !path.is_empty())
        else {
            return Self(false);
        };
        let now = Instant::now();
        let pending = Pending {
            path: PathBuf::from(path),
            started: now,
            boundary: now,
            record: json!({
                "schema": 2,
                "timestamp_unix_ms": SystemTime::now().duration_since(UNIX_EPOCH)
                    .unwrap_or_default().as_millis() as u64,
                "pid": std::process::id(),
                "command": crate::invocation_log::command(),
                "entry": entry,
                "threads": threads(),
                "totals": {},
            }),
            phases: Vec::new(),
        };
        PENDING.with(|state| *state.borrow_mut() = Some(pending));
        Self(true)
    }

    pub(crate) fn success(self) {
        if self.0 {
            finish("ok");
        }
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        if self.0 {
            finish("error");
        }
    }
}

fn with_pending(f: impl FnOnce(&mut Pending)) {
    PENDING.with(|state| {
        if let Some(pending) = state.borrow_mut().as_mut() {
            f(pending);
        }
    });
}

/// Close the phase that ran since the previous boundary.
pub(crate) fn phase(name: &str) {
    with_pending(|pending| {
        let now = Instant::now();
        pending
            .phases
            .push(Node::leaf(name, now - pending.boundary));
        pending.boundary = now;
    });
}

/// The front end's own pass timers, as children of the phase just closed.
pub(crate) fn detail(timings: &[(&'static str, Duration)]) {
    with_pending(|pending| {
        let Some(last) = pending.phases.pop() else {
            return;
        };
        let children = timings
            .iter()
            .map(|(name, d)| Node::leaf(name, *d))
            .collect();
        pending
            .phases
            .push(Node::parent(&last.name, last.wall, children));
    });
}

/// The discovered program: file count and source bytes.
pub(crate) fn sources(sources: &crate::SourceFiles) {
    with_pending(|pending| {
        let bytes: usize = sources
            .files
            .iter()
            .map(|file| file.borrow().contents.len())
            .sum();
        pending.record["totals"]["source_files"] = json!(sources.files.len());
        pending.record["totals"]["source_bytes"] = json!(bytes);
    });
}

/// Close the codegen phase and break it into setup, fan-out and assembly
/// from the emitter's own timers.
pub(crate) fn emission(emit: &skull::mir_emit::MirEmit) {
    phase("codegen");
    with_pending(|pending| {
        let Some(codegen) = pending.phases.pop() else {
            return;
        };
        let mut setup = Vec::new();
        let mut branches = Vec::new();
        let mut cpu = Vec::new();
        let mut assembly = Vec::new();
        let mut fan_out_window = Duration::ZERO;
        let mut assembly_window = Duration::ZERO;
        let mut cache_lookup_window = None;
        let mut cache_write = None;
        let mut scaffold = None;
        for (name, d) in &emit.phases {
            if let Some(rest) = name.strip_prefix("setup: ") {
                setup.push(Node::leaf(rest, *d));
            } else if let Some(rest) = name.strip_prefix("join: ") {
                branches.push(json!({"name": rest, "wall_ms": ms(*d)}));
            } else if let Some(rest) = name.strip_prefix("assembly: ") {
                assembly.push(Node::leaf(rest, *d));
            } else if *name == "window: fan-out" {
                fan_out_window = *d;
            } else if *name == "window: assembly" {
                assembly_window = *d;
            } else if *name == "window: cache lookup" {
                cache_lookup_window = Some(*d);
            } else if *name == "cache: write" {
                cache_write = Some(*d);
            } else if *name == "scaffold+callgraph+order" {
                scaffold = Some(*d);
            } else if !name.starts_with("incr: ") {
                cpu.push((*name, *d));
            }
        }
        // The lanes run several branches at once; their windows overlap, so
        // they sit beside the lane rather than summing into it.
        if let Some(lanes) = setup.iter_mut().find(|n| n.name == "concrete plans+lanes") {
            lanes
                .extra
                .insert("branches".into(), Value::Array(branches));
        }
        let setup = Node::sum_of("setup", setup);

        let mut fan_out = Node::leaf("fan-out", fan_out_window);
        fan_out.extra.insert("threads".into(), json!(threads()));

        let mut assembly = Node::parent("assembly", assembly_window, assembly);
        if let Some(scaffold) = scaffold {
            // Accumulated across the definition pass; a detail, not a step.
            assembly
                .extra
                .insert("scaffold_ms".into(), json!(ms(scaffold)));
        }
        let mut children = vec![setup];
        // The body cache's lookup pass precedes the fan-out, which then
        // lowers only the misses; its write follows assembly.
        if let Some(lookup) = cache_lookup_window {
            children.push(Node::leaf("body cache lookup", lookup));
        }
        children.push(fan_out);
        children.push(assembly);
        if let Some(write) = cache_write {
            children.push(Node::leaf("body cache write", write));
        }
        let mut codegen = Node::parent(&codegen.name, codegen.wall, children);
        if cache_lookup_window.is_some() {
            let stats = emit.body_cache;
            codegen.extra.insert(
                "body_cache".into(),
                json!({
                    "enabled": stats.enabled,
                    "hits": stats.hits,
                    "misses": stats.misses,
                    "uncacheable": stats.uncacheable,
                    "stored": stats.stored,
                    "shard_records": stats.shard_records,
                    "shard_bytes": stats.shard_bytes,
                    "written_bytes": stats.written_bytes,
                }),
            );
        }
        // Per-pass unit time summed over every lowered body: the fan-out, but
        // also the decorators, globals and specializations assembly lowers
        // serially. It overlaps across workers, so it is comparable between
        // passes and across commits, not with any wall window.
        let cpu_total: Duration = cpu.iter().map(|(_, d)| *d).sum();
        codegen
            .extra
            .insert("unit_cpu_ms".into(), json!(ms(cpu_total)));
        codegen.extra.insert(
            "unit_cpu".into(),
            Value::Array(
                cpu.iter()
                    .map(|(name, d)| json!({"name": name, "cpu_ms": ms(*d)}))
                    .collect(),
            ),
        );
        pending.phases.push(codegen);
        pending.record["totals"]["concrete_plans"] = json!(emit.compile_counts.concrete_plans);
        pending.record["totals"]["chunks"] = json!(emit.compile_counts.chunks);
        pending.record["totals"]["dependency_edges"] = json!(emit.compile_counts.dependency_edges);
        pending.record["totals"]["emitted_lua_bytes"] = json!(emit.lua.len());
        pending.record["totals"]["body_cache_hits"] = json!(emit.body_cache.hits);
        pending.record["totals"]["body_cache_misses"] = json!(emit.body_cache.misses);
    });
}

fn finish(status: &str) {
    let pending = PENDING
        .try_with(|state| state.borrow_mut().take())
        .ok()
        .flatten();
    let Some(mut pending) = pending else { return };
    let wall = pending.started.elapsed();
    let budget = Node::parent("compile", wall, std::mem::take(&mut pending.phases));
    pending.record["status"] = json!(status);
    pending.record["wall_ms"] = json!(ms(budget.wall));
    pending.record["peak_rss_bytes"] = crate::allocator::process_memory()["peak_rss_bytes"].clone();
    pending.record["budget"] = budget.into_json();
    if let Err(error) = append(&pending.path, &pending.record) {
        log::warn!(target: "COMPILE_STATS", "cannot append {}: {error}", pending.path.display());
    }
}

fn append(path: &Path, record: &Value) -> std::io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let mut line = serde_json::to_vec(record)?;
    line.push(b'\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    // O_APPEND assigns each write an end offset; the advisory lock also keeps
    // a short write/retry from splitting a line around another process's line.
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
    file.write_all(&line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_parent_sums_exactly_and_keeps_its_remainder() {
        let node = Node::parent(
            "codegen",
            Duration::from_millis(10),
            vec![
                Node::leaf("setup", Duration::from_millis(3)),
                Node::leaf("fan-out", Duration::from_millis(5)),
            ],
        );
        let json = node.into_json();
        let children = json["children"].as_array().unwrap();
        assert_eq!(children[2]["name"], "other");
        let sum: f64 = children
            .iter()
            .map(|c| c["wall_ms"].as_f64().unwrap())
            .sum();
        assert!((sum - json["wall_ms"].as_f64().unwrap()).abs() < 1e-9);
    }

    #[test]
    fn children_that_overrun_their_parent_widen_it() {
        let node = Node::parent(
            "p",
            Duration::from_millis(1),
            vec![Node::leaf("c", Duration::from_millis(2))],
        );
        assert_eq!(node.wall, Duration::from_millis(2));
    }
}
