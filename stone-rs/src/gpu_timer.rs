//! GPU timing from `GL_TIMESTAMP` query pairs. The frame is the root scope,
//! recorded whenever timer queries work (`frame_ms.gpu`, the settings timing
//! row). With `STONE_GPU_SCOPES=1`, named scopes nest under it -- runner stages
//! and every fullscreen pass -- and each also opens a `KHR_debug` group of the
//! same name so RenderDoc and Nsight show the frame as named passes.
//!
//! Timestamps rather than `GL_TIME_ELAPSED`: elapsed queries of one target
//! cannot nest. Results lag a few frames; a ring of frame slots is read back
//! without blocking, because waiting on a query stalls the pipeline and
//! destroys the measurement. A slot still unread when the ring wraps is
//! dropped and counted.
//!
//! Platform support is one question with one answer,
//! `DebugGl::supports_timer_query` (absent on Apple's desktop driver).
//! Unsupported reports `-2.0` -> `n/a`
//! and scope status `unsupported`, never a fake 0.
//!
//! `STONE_GPU_SCOPES=finish` records the same scope tree without queries, on
//! any context: every scope boundary calls `glFinish` and reads the CPU clock,
//! so a scope's time is its CPU submission plus its GPU execution, serialized.
//! It perturbs the frame it measures and reports no `frame_ms.gpu`; it exists
//! for contexts with no timer (macOS) where the alternative is ablation.

use crate::DebugGl;
use std::collections::HashMap;

const RING: usize = 4;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SlotState {
    Empty,
    Recording,
    Pending,
}

struct Entry {
    path: PathId,
    begin: usize,
    end: Option<usize>,
}

struct FrameSlot {
    state: SlotState,
    /// Pooled query objects; `used` of them belong to this frame.
    queries: Vec<glow::Query>,
    used: usize,
    /// Entry 0 is the root `frame` scope.
    entries: Vec<Entry>,
    /// An unbalanced scope stack: the frame total stands, its scopes do not.
    unbalanced: bool,
}

impl FrameSlot {
    fn new() -> Self {
        Self {
            state: SlotState::Empty,
            queries: Vec::new(),
            used: 0,
            entries: Vec::new(),
            unbalanced: false,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScopeStatus {
    Off,
    On,
    Unsupported,
    Finish,
}

/// What `STONE_GPU_SCOPES` asked for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScopeMode {
    Off,
    /// `1`: timestamp queries, where the context has them.
    Queries,
    /// `finish`: `glFinish`-bracketed CPU wall time, on any context.
    Finish,
}

/// `None` for a value this parser does not know, so the caller can say so
/// instead of silently measuring nothing.
pub(crate) fn scope_mode_from_env(value: Option<&str>) -> Option<ScopeMode> {
    match value {
        None | Some("") | Some("0") => Some(ScopeMode::Off),
        Some("1") => Some(ScopeMode::Queries),
        Some("finish") => Some(ScopeMode::Finish),
        Some(_) => None,
    }
}

/// Open state of the finish-bracketed recorder for the current frame.
struct FinishFrame {
    recording: bool,
    unbalanced: bool,
    /// (path, start, `waited` at start) of each open scope; entry 0 is the root.
    stack: Vec<(PathId, std::time::Instant, u64)>,
    samples: Vec<ScopeSample>,
    waits: Vec<ScopeSample>,
    /// Nanoseconds blocked in glFinish so far this frame.
    waited: u64,
}

impl FinishFrame {
    fn finish(&mut self, gl: &DebugGl) -> std::time::Instant {
        let start = std::time::Instant::now();
        unsafe { gl.finish() };
        let end = std::time::Instant::now();
        self.waited += end.duration_since(start).as_nanos() as u64;
        end
    }

    fn close(&mut self, gl: &DebugGl) {
        let end = self.finish(gl);
        let (path, start, waited) = self.stack.pop().expect("caller checked an open scope");
        self.samples.push(ScopeSample {
            path,
            ns: end.duration_since(start).as_nanos() as u64,
        });
        self.waits.push(ScopeSample {
            path,
            ns: self.waited - waited,
        });
    }
}

pub struct GpuTimer {
    slots: [FrameSlot; RING],
    write: usize,
    read: usize,
    /// Open entries of the recording slot (indices into its `entries`).
    stack: Vec<usize>,
    mode: ScopeMode,
    finish: FinishFrame,
    /// GL timer queries usable on this context.
    supported: bool,
    debug_groups: bool,
    /// Debug groups pushed and not yet popped in the recording frame.
    open_groups: usize,
    initialized: bool,
    root: PathId,
    aggregate: ScopeAggregator,
    unbalanced_logged: bool,
}

fn scope_mode_by_env() -> ScopeMode {
    let value = std::env::var("STONE_GPU_SCOPES").ok();
    scope_mode_from_env(value.as_deref()).unwrap_or_else(|| {
        ::log::error!(
            target: "STONE",
            "STONE_GPU_SCOPES={:?} is not one of 1, finish, 0; GPU scopes stay off",
            value.unwrap_or_default()
        );
        ScopeMode::Off
    })
}

impl GpuTimer {
    pub fn new() -> Self {
        let mut aggregate = ScopeAggregator::default();
        let root = aggregate.path(None, "frame");
        Self {
            slots: std::array::from_fn(|_| FrameSlot::new()),
            write: 0,
            read: 0,
            stack: Vec::new(),
            mode: scope_mode_by_env(),
            finish: FinishFrame {
                recording: false,
                unbalanced: false,
                stack: Vec::new(),
                samples: Vec::new(),
                waits: Vec::new(),
                waited: 0,
            },
            supported: true,
            debug_groups: false,
            open_groups: 0,
            initialized: false,
            root,
            aggregate,
            unbalanced_logged: false,
        }
    }

    fn scopes_requested(&self) -> bool {
        self.mode != ScopeMode::Off
    }

    fn scopes_active(&self) -> bool {
        self.mode == ScopeMode::Queries && self.supported
    }

    pub fn status(&self) -> ScopeStatus {
        match (self.mode, self.initialized && !self.supported) {
            (ScopeMode::Off, _) => ScopeStatus::Off,
            (ScopeMode::Finish, _) => ScopeStatus::Finish,
            (ScopeMode::Queries, true) => ScopeStatus::Unsupported,
            (ScopeMode::Queries, false) => ScopeStatus::On,
        }
    }

    /// Capability checks on first use (the GL context must be current).
    fn ensure_init(&mut self, gl: &DebugGl) {
        if self.initialized {
            return;
        }
        self.initialized = true;
        if self.mode == ScopeMode::Finish {
            self.debug_groups = gl.supports_debug_groups();
            ::log::info!(
                target: "STONE",
                "GPU scopes on: glFinish-bracketed wall time per scope (no timer queries), KHR_debug groups {}",
                if self.debug_groups { "on" } else { "unsupported on this context" }
            );
            return;
        }
        if !gl.supports_timer_query() {
            self.supported = false;
            if self.scopes_requested() {
                ::log::warn!(
                    target: "STONE",
                    "STONE_GPU_SCOPES=1 requested but GL timer queries are unsupported on this context -- GPU scopes report unsupported"
                );
            } else {
                ::log::info!(
                    target: "STONE",
                    "GPU timer unsupported on this context -- gpu_ms reads n/a"
                );
            }
            return;
        }
        if self.scopes_requested() {
            self.debug_groups = gl.supports_debug_groups();
            ::log::info!(
                target: "STONE",
                "GPU scopes on: timestamp queries per scope, KHR_debug groups {}",
                if self.debug_groups { "on" } else { "unsupported on this context" }
            );
        }
    }

    /// Next query of the recording slot, created on first use of that pool index.
    fn take_query(&mut self, gl: &DebugGl) -> Option<usize> {
        let slot = &mut self.slots[self.write];
        if slot.used == slot.queries.len() {
            match unsafe { gl.create_query() } {
                Ok(q) => slot.queries.push(q),
                Err(e) => {
                    ::log::warn!(target: "STONE", "GPU timer disabled: create_query failed: {e}");
                    self.supported = false;
                    return None;
                }
            }
        }
        let index = slot.used;
        slot.used += 1;
        unsafe { gl.query_counter(slot.queries[index], glow::TIMESTAMP) };
        Some(index)
    }

    /// Begin the frame: the root scope.
    pub fn begin(&mut self, gl: &DebugGl) {
        self.ensure_init(gl);
        if self.mode == ScopeMode::Finish {
            self.finish_begin(gl);
            return;
        }
        if !self.supported {
            return;
        }
        let scopes_requested = self.scopes_requested();
        let slot = &mut self.slots[self.write];
        if slot.state == SlotState::Pending {
            // Readback fell a whole ring behind; this sample is lost.
            if scopes_requested {
                self.aggregate.drop_frame();
            }
            if self.read == self.write {
                self.read = (self.read + 1) % RING;
            }
        }
        slot.state = SlotState::Recording;
        slot.used = 0;
        slot.entries.clear();
        slot.unbalanced = false;
        self.stack.clear();
        self.open_groups = 0;
        let Some(begin) = self.take_query(gl) else {
            return;
        };
        let root = self.root;
        let slot = &mut self.slots[self.write];
        slot.entries.push(Entry {
            path: root,
            begin,
            end: None,
        });
        self.stack.push(0);
    }

    fn recording(&self) -> bool {
        self.slots[self.write].state == SlotState::Recording
    }

    pub fn scope_begin(&mut self, gl: &DebugGl, name: &str) {
        if self.mode == ScopeMode::Finish {
            self.finish_scope_begin(gl, name);
            return;
        }
        if !self.scopes_active() || !self.recording() {
            return;
        }
        let parent_entry = *self
            .stack
            .last()
            .expect("recording frame has its root open");
        let parent = self.slots[self.write].entries[parent_entry].path;
        let path = self.aggregate.path(Some(parent), name);
        let Some(begin) = self.take_query(gl) else {
            return;
        };
        let slot = &mut self.slots[self.write];
        slot.entries.push(Entry {
            path,
            begin,
            end: None,
        });
        self.stack.push(slot.entries.len() - 1);
        if self.debug_groups {
            unsafe { gl.push_debug_group(name) };
            self.open_groups += 1;
        }
    }

    pub fn scope_end(&mut self, gl: &DebugGl) {
        if self.mode == ScopeMode::Finish {
            self.finish_scope_end(gl);
            return;
        }
        if !self.scopes_active() || !self.recording() {
            return;
        }
        if self.stack.len() <= 1 {
            self.mark_unbalanced("GPU scope end without a matching begin");
            return;
        }
        let entry = self.stack.pop().expect("checked above");
        let Some(end) = self.take_query(gl) else {
            return;
        };
        self.slots[self.write].entries[entry].end = Some(end);
        // Without a flush the end timestamp can wait in the driver's command
        // buffer until the next flush, so GPU idle time while the CPU builds
        // later work is billed to this scope. Flushed, that idle time falls
        // between scopes: the parent minus its children.
        unsafe { gl.flush() };
        if self.debug_groups && self.open_groups > 0 {
            unsafe { gl.pop_debug_group() };
            self.open_groups -= 1;
        }
    }

    fn mark_unbalanced(&mut self, what: &str) {
        self.slots[self.write].unbalanced = true;
        if !self.unbalanced_logged {
            self.unbalanced_logged = true;
            ::log::error!(target: "STONE", "{what}; this frame's scope samples are discarded");
        }
    }

    /// End the frame's root scope and hand the slot to readback.
    pub fn end(&mut self, gl: &DebugGl) {
        if self.mode == ScopeMode::Finish {
            self.finish_end(gl);
            return;
        }
        if !self.supported || !self.recording() {
            return;
        }
        if self.stack.len() > 1 {
            // A frame error unwound past open scopes (frame_error_recover).
            self.mark_unbalanced("GPU scopes still open at frame end");
            self.stack.truncate(1);
        }
        while self.open_groups > 0 {
            unsafe { gl.pop_debug_group() };
            self.open_groups -= 1;
        }
        let Some(end) = self.take_query(gl) else {
            return;
        };
        let slot = &mut self.slots[self.write];
        slot.entries[0].end = Some(end);
        slot.state = SlotState::Pending;
        self.stack.clear();
        self.write = (self.write + 1) % RING;
    }

    /// Non-blocking read of the oldest pending frame. Returns:
    /// - `>= 0.0` GPU ms of that frame's root scope (from a few frames ago),
    /// - `-1.0` no result ready yet,
    /// - `-2.0` timer queries unsupported on this context.
    ///
    /// Scope samples of the same frame are aggregated as a side effect.
    pub fn poll_ms(&mut self, gl: &DebugGl) -> f32 {
        self.ensure_init(gl);
        // Finish mode issues no queries; its frame total is the `frame` scope.
        if !self.supported || self.mode == ScopeMode::Finish {
            return -2.0;
        }
        let slot = &self.slots[self.read];
        if slot.state != SlotState::Pending {
            return -1.0;
        }
        // Timestamps complete in submission order, so the root end -- the
        // frame's last query -- being available implies the rest are.
        let last = slot.queries[slot.used - 1];
        if unsafe { gl.get_query_parameter_u32(last, glow::QUERY_RESULT_AVAILABLE) } == 0 {
            return -1.0;
        }
        let read =
            |q: usize| unsafe { gl.get_query_parameter_u64(slot.queries[q], glow::QUERY_RESULT) };
        let span = |e: &Entry| e.end.map(|end| read(end).saturating_sub(read(e.begin)));
        let frame_ns = span(&slot.entries[0]).unwrap_or(0);
        if self.scopes_requested() {
            if slot.unbalanced {
                self.aggregate.drop_frame();
            } else {
                let samples: Vec<ScopeSample> = slot
                    .entries
                    .iter()
                    .filter_map(|e| span(e).map(|ns| ScopeSample { path: e.path, ns }))
                    .collect();
                self.aggregate.ingest_frame(&samples);
            }
        }
        self.slots[self.read].state = SlotState::Empty;
        self.read = (self.read + 1) % RING;
        frame_ns as f32 / 1_000_000.0
    }

    /// The wait for GPU work queued before the frame (the previous swap, and
    /// passes submitted from update) is its own top-level row, so it is not
    /// billed to the frame's first scope.
    fn finish_begin(&mut self, gl: &DebugGl) {
        let before = self.aggregate.path(None, "before_frame");
        let f = &mut self.finish;
        f.recording = true;
        f.unbalanced = false;
        f.stack.clear();
        f.samples.clear();
        f.waits.clear();
        f.waited = 0;
        let start = f.finish(gl);
        f.samples.push(ScopeSample {
            path: before,
            ns: f.waited,
        });
        f.waits.push(ScopeSample {
            path: before,
            ns: f.waited,
        });
        f.stack.push((self.root, start, f.waited));
        self.open_groups = 0;
    }

    fn finish_scope_begin(&mut self, gl: &DebugGl, name: &str) {
        if !self.finish.recording {
            return;
        }
        let parent = self
            .finish
            .stack
            .last()
            .expect("recording frame has its root open")
            .0;
        let path = self.aggregate.path(Some(parent), name);
        let start = self.finish.finish(gl);
        let waited = self.finish.waited;
        self.finish.stack.push((path, start, waited));
        if self.debug_groups {
            unsafe { gl.push_debug_group(name) };
            self.open_groups += 1;
        }
    }

    fn finish_scope_end(&mut self, gl: &DebugGl) {
        if !self.finish.recording {
            return;
        }
        if self.finish.stack.len() <= 1 {
            self.finish.unbalanced = true;
            if !self.unbalanced_logged {
                self.unbalanced_logged = true;
                ::log::error!(target: "STONE", "GPU scope end without a matching begin; this frame's scope samples are discarded");
            }
            return;
        }
        self.finish.close(gl);
        if self.debug_groups && self.open_groups > 0 {
            unsafe { gl.pop_debug_group() };
            self.open_groups -= 1;
        }
    }

    fn finish_end(&mut self, gl: &DebugGl) {
        if !self.finish.recording {
            return;
        }
        if self.finish.stack.len() > 1 {
            // A frame error unwound past open scopes (frame_error_recover).
            self.finish.unbalanced = true;
            if !self.unbalanced_logged {
                self.unbalanced_logged = true;
                ::log::error!(target: "STONE", "GPU scopes still open at frame end; this frame's scope samples are discarded");
            }
        }
        while self.open_groups > 0 {
            unsafe { gl.pop_debug_group() };
            self.open_groups -= 1;
        }
        self.finish.stack.truncate(1);
        self.finish.close(gl);
        if self.finish.unbalanced {
            self.aggregate.drop_frame();
        } else {
            self.aggregate.ingest_frame(&self.finish.samples);
            self.aggregate.ingest_frame_waits(&self.finish.waits);
        }
        self.finish.recording = false;
        self.finish.stack.clear();
    }

    pub fn reset_stats(&mut self) {
        self.aggregate.reset_stats();
    }

    /// The scope report embedded in the perf JSON under `gpu_scopes`.
    pub fn scopes_json(&self) -> String {
        let status = match self.status() {
            ScopeStatus::Off => "off",
            ScopeStatus::On => "on",
            ScopeStatus::Unsupported => "unsupported",
            ScopeStatus::Finish => "finish",
        };
        scopes_json(status, &self.aggregate)
    }
}

pub(crate) fn scopes_json(status: &str, aggregate: &ScopeAggregator) -> String {
    let scopes: Vec<serde_json::Value> = if status == "on" || status == "finish" {
        aggregate
            .stats()
            .into_iter()
            .map(|s| {
                let mut row = serde_json::json!({
                    "name": s.name,
                    "path": s.path,
                    "parent": s.parent,
                    "depth": s.depth,
                    "calls_per_frame": s.calls_per_frame,
                    "ms_per_frame": s.ms_per_frame,
                    "ms": {
                        "mean": s.mean, "stddev": s.stddev, "min": s.min, "max": s.max,
                        "p50": s.p50, "p90": s.p90, "p95": s.p95, "p99": s.p99, "n": s.frames,
                    },
                });
                if status == "finish" {
                    row["wait_ms_per_frame"] = serde_json::json!(s.wait_ms_per_frame);
                }
                row
            })
            .collect()
    } else {
        Vec::new()
    };
    serde_json::json!({
        "status": status,
        "frames": aggregate.frames(),
        "dropped_frames": aggregate.dropped_frames(),
        "truncated_samples": aggregate.truncated_samples(),
        "scopes": scopes,
    })
    .to_string()
}

/// Index into [`ScopeAggregator`]'s path table: one entry per distinct
/// (parent path, name), so a pass that runs under two parents is two rows.
pub(crate) type PathId = u32;

/// One closed scope from a completed frame, in begin order.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ScopeSample {
    pub path: PathId,
    pub ns: u64,
}

struct PathInfo {
    name: u32,
    parent: Option<PathId>,
    depth: u32,
}

#[derive(Default)]
struct PathRow {
    /// Per-frame inclusive total (ms), one entry per frame the path occurred in.
    frame_ms: Vec<f64>,
    /// Finish mode: per-frame time spent blocked in glFinish inside the scope.
    frame_wait_ms: Vec<f64>,
    calls: u64,
}

/// Per-path GPU time over the measured frames. Pure (no GL), so the
/// statistics are testable without a context.
#[derive(Default)]
pub(crate) struct ScopeAggregator {
    names: Vec<String>,
    name_ids: HashMap<String, u32>,
    paths: Vec<PathInfo>,
    path_ids: HashMap<(Option<PathId>, u32), PathId>,
    rows: Vec<PathRow>,
    frames: u64,
    dropped_frames: u64,
    truncated_samples: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ScopeStat {
    pub name: String,
    pub path: String,
    pub parent: Option<String>,
    pub depth: u32,
    pub frames: usize,
    pub calls_per_frame: f64,
    /// Total time over every measured frame, including frames the scope did
    /// not run in: what it costs per frame on average, where `mean` is what it
    /// costs when it runs.
    pub ms_per_frame: f64,
    pub mean: f64,
    pub stddev: f64,
    pub min: f64,
    pub max: f64,
    pub p50: f64,
    pub p90: f64,
    pub p95: f64,
    pub p99: f64,
    /// Finish mode: glFinish wait inside the scope, averaged over every
    /// measured frame. The wall time minus it is CPU time in the scope.
    pub wait_ms_per_frame: f64,
}

/// Samples kept per path. A bounded `rock perf` run records a few hundred
/// frames; the cap only stops an open-ended session from growing without limit.
const MAX_FRAMES_PER_PATH: usize = 16_384;

impl ScopeAggregator {
    /// Interned path for `name` under `parent`. Allocates only the first time
    /// a (parent, name) pair is seen.
    pub fn path(&mut self, parent: Option<PathId>, name: &str) -> PathId {
        let name_id = match self.name_ids.get(name) {
            Some(&id) => id,
            None => {
                let id = self.names.len() as u32;
                self.names.push(name.to_string());
                self.name_ids.insert(name.to_string(), id);
                id
            }
        };
        if let Some(&id) = self.path_ids.get(&(parent, name_id)) {
            return id;
        }
        let depth = parent.map_or(0, |p| self.paths[p as usize].depth + 1);
        let id = self.paths.len() as PathId;
        self.paths.push(PathInfo {
            name: name_id,
            parent,
            depth,
        });
        self.rows.push(PathRow::default());
        self.path_ids.insert((parent, name_id), id);
        id
    }

    pub fn ingest_frame(&mut self, samples: &[ScopeSample]) {
        self.frames += 1;
        let mut totals: Vec<(PathId, u64)> = Vec::with_capacity(samples.len());
        for sample in samples {
            match totals.iter_mut().find(|(path, _)| *path == sample.path) {
                Some((_, ns)) => *ns += sample.ns,
                None => totals.push((sample.path, sample.ns)),
            }
            self.rows[sample.path as usize].calls += 1;
        }
        for (path, ns) in totals {
            let row = &mut self.rows[path as usize];
            if row.frame_ms.len() >= MAX_FRAMES_PER_PATH {
                self.truncated_samples += 1;
                continue;
            }
            row.frame_ms.push(ns as f64 / 1_000_000.0);
        }
    }

    /// The glFinish wait inside each scope of the frame just ingested.
    pub fn ingest_frame_waits(&mut self, waits: &[ScopeSample]) {
        let mut totals: Vec<(PathId, u64)> = Vec::with_capacity(waits.len());
        for wait in waits {
            match totals.iter_mut().find(|(path, _)| *path == wait.path) {
                Some((_, ns)) => *ns += wait.ns,
                None => totals.push((wait.path, wait.ns)),
            }
        }
        for (path, ns) in totals {
            let row = &mut self.rows[path as usize];
            if row.frame_wait_ms.len() >= MAX_FRAMES_PER_PATH {
                continue;
            }
            row.frame_wait_ms.push(ns as f64 / 1_000_000.0);
        }
    }

    pub fn drop_frame(&mut self) {
        self.dropped_frames += 1;
    }

    pub fn reset_stats(&mut self) {
        for row in &mut self.rows {
            row.frame_ms.clear();
            row.frame_wait_ms.clear();
            row.calls = 0;
        }
        self.frames = 0;
        self.dropped_frames = 0;
        self.truncated_samples = 0;
    }

    fn path_string(&self, id: PathId) -> String {
        let info = &self.paths[id as usize];
        match info.parent {
            Some(parent) => format!(
                "{}/{}",
                self.path_string(parent),
                self.names[info.name as usize]
            ),
            None => self.names[info.name as usize].clone(),
        }
    }

    /// Rows in depth-first order (children after their parent, siblings in
    /// first-seen order), skipping paths with no sample in the window.
    pub fn stats(&self) -> Vec<ScopeStat> {
        let mut out = Vec::new();
        for (id, info) in self.paths.iter().enumerate() {
            if info.parent.is_none() {
                self.collect(id as PathId, &mut out);
            }
        }
        out
    }

    fn collect(&self, id: PathId, out: &mut Vec<ScopeStat>) {
        let info = &self.paths[id as usize];
        let row = &self.rows[id as usize];
        if !row.frame_ms.is_empty() {
            let mut sorted = row.frame_ms.clone();
            sorted.sort_by(f64::total_cmp);
            let n = sorted.len();
            let mean = sorted.iter().sum::<f64>() / n as f64;
            let variance = sorted.iter().map(|v| (v - mean) * (v - mean)).sum::<f64>() / n as f64;
            let pick = |p: usize| sorted[metric_percentile_index(p, n)];
            out.push(ScopeStat {
                name: self.names[info.name as usize].clone(),
                path: self.path_string(id),
                parent: info.parent.map(|p| self.path_string(p)),
                depth: info.depth,
                frames: n,
                calls_per_frame: if self.frames > 0 {
                    row.calls as f64 / self.frames as f64
                } else {
                    0.0
                },
                ms_per_frame: if self.frames > 0 {
                    sorted.iter().sum::<f64>() / self.frames as f64
                } else {
                    0.0
                },
                mean,
                stddev: variance.sqrt(),
                min: sorted[0],
                max: sorted[n - 1],
                p50: pick(50),
                p90: pick(90),
                p95: pick(95),
                p99: pick(99),
                wait_ms_per_frame: if self.frames > 0 {
                    row.frame_wait_ms.iter().sum::<f64>() / self.frames as f64
                } else {
                    0.0
                },
            });
        }
        for (child, child_info) in self.paths.iter().enumerate() {
            if child_info.parent == Some(id) {
                self.collect(child as PathId, out);
            }
        }
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    pub fn dropped_frames(&self) -> u64 {
        self.dropped_frames
    }

    pub fn truncated_samples(&self) -> u64 {
        self.truncated_samples
    }
}

/// Same index rule as the Rock profiler's `metric_percentile_index`, so a GPU
/// scope's p99 means what a CPU section's p99 means.
fn metric_percentile_index(p: usize, n: usize) -> usize {
    if n <= 1 {
        return 0;
    }
    ((p * (n - 1) + 50) / 100).min(n - 1)
}

#[cfg(test)]
mod tests {
    use super::{scope_mode_from_env, ScopeAggregator, ScopeMode, ScopeSample};

    #[test]
    fn scope_mode_names_queries_finish_and_rejects_the_rest() {
        assert_eq!(scope_mode_from_env(None), Some(ScopeMode::Off));
        assert_eq!(scope_mode_from_env(Some("0")), Some(ScopeMode::Off));
        assert_eq!(scope_mode_from_env(Some("1")), Some(ScopeMode::Queries));
        assert_eq!(scope_mode_from_env(Some("finish")), Some(ScopeMode::Finish));
        assert_eq!(scope_mode_from_env(Some("yes")), None);
    }

    const MS: u64 = 1_000_000;

    fn stat<'a>(stats: &'a [super::ScopeStat], path: &str) -> &'a super::ScopeStat {
        stats
            .iter()
            .find(|s| s.path == path)
            .unwrap_or_else(|| panic!("no scope {path} in {stats:?}"))
    }

    #[test]
    fn nested_and_repeated_scopes_sum_per_frame() {
        let mut agg = ScopeAggregator::default();
        let frame = agg.path(None, "frame");
        let a = agg.path(Some(frame), "a");
        let b = agg.path(Some(a), "b");
        for (f, a_ms, b1, b2) in [(10, 4, 1, 2), (12, 6, 2, 3)] {
            agg.ingest_frame(&[
                ScopeSample {
                    path: frame,
                    ns: f * MS,
                },
                ScopeSample {
                    path: a,
                    ns: a_ms * MS,
                },
                ScopeSample {
                    path: b,
                    ns: b1 * MS,
                },
                ScopeSample {
                    path: b,
                    ns: b2 * MS,
                },
            ]);
        }
        let stats = agg.stats();
        let b = stat(&stats, "frame/a/b");
        assert_eq!(b.frames, 2);
        assert_eq!(b.calls_per_frame, 2.0);
        assert_eq!(b.mean, 4.0);
        assert_eq!((b.min, b.max), (3.0, 5.0));
        assert_eq!(b.depth, 2);
        assert_eq!(b.parent.as_deref(), Some("frame/a"));
        assert_eq!(stat(&stats, "frame").mean, 11.0);
        let order: Vec<&str> = stats.iter().map(|s| s.path.as_str()).collect();
        assert_eq!(order, ["frame", "frame/a", "frame/a/b"]);
    }

    #[test]
    fn a_scope_absent_from_a_frame_counts_only_frames_it_ran_in() {
        let mut agg = ScopeAggregator::default();
        let frame = agg.path(None, "frame");
        let bloom = agg.path(Some(frame), "bloom");
        agg.ingest_frame(&[
            ScopeSample {
                path: frame,
                ns: 5 * MS,
            },
            ScopeSample {
                path: bloom,
                ns: MS,
            },
        ]);
        agg.ingest_frame(&[ScopeSample {
            path: frame,
            ns: 5 * MS,
        }]);
        let stats = agg.stats();
        let bloom = stat(&stats, "frame/bloom");
        assert_eq!(bloom.frames, 1);
        assert_eq!(bloom.calls_per_frame, 0.5);
        assert_eq!(bloom.mean, 1.0);
        assert_eq!(bloom.ms_per_frame, 0.5);
        assert_eq!(stat(&stats, "frame").ms_per_frame, 5.0);
        assert_eq!(stat(&stats, "frame").frames, 2);
    }

    #[test]
    fn one_name_under_two_parents_is_two_rows() {
        let mut agg = ScopeAggregator::default();
        let frame = agg.path(None, "frame");
        let scene = agg.path(Some(frame), "scene_passes");
        let post = agg.path(Some(frame), "post_processing");
        let blit_scene = agg.path(Some(scene), "pass:blit");
        let blit_post = agg.path(Some(post), "pass:blit");
        assert_ne!(blit_scene, blit_post);
        assert_eq!(agg.path(Some(scene), "pass:blit"), blit_scene);
        agg.ingest_frame(&[
            ScopeSample {
                path: frame,
                ns: 9 * MS,
            },
            ScopeSample {
                path: scene,
                ns: 3 * MS,
            },
            ScopeSample {
                path: blit_scene,
                ns: MS,
            },
            ScopeSample {
                path: post,
                ns: 4 * MS,
            },
            ScopeSample {
                path: blit_post,
                ns: 2 * MS,
            },
        ]);
        let stats = agg.stats();
        assert_eq!(stat(&stats, "frame/scene_passes/pass:blit").mean, 1.0);
        assert_eq!(stat(&stats, "frame/post_processing/pass:blit").mean, 2.0);
    }

    #[test]
    fn finish_waits_sum_per_frame_beside_the_wall_time() {
        let mut agg = ScopeAggregator::default();
        let frame = agg.path(None, "frame");
        let pass = agg.path(Some(frame), "pass:x");
        agg.ingest_frame(&[
            ScopeSample {
                path: frame,
                ns: 10 * MS,
            },
            ScopeSample {
                path: pass,
                ns: 4 * MS,
            },
        ]);
        agg.ingest_frame_waits(&[
            ScopeSample {
                path: frame,
                ns: 6 * MS,
            },
            ScopeSample { path: pass, ns: MS },
            ScopeSample {
                path: pass,
                ns: 2 * MS,
            },
        ]);
        agg.ingest_frame(&[ScopeSample {
            path: frame,
            ns: 10 * MS,
        }]);
        agg.ingest_frame_waits(&[ScopeSample {
            path: frame,
            ns: 4 * MS,
        }]);
        let stats = agg.stats();
        assert_eq!(stat(&stats, "frame").wait_ms_per_frame, 5.0);
        assert_eq!(stat(&stats, "frame/pass:x").wait_ms_per_frame, 1.5);
    }

    #[test]
    fn percentiles_use_the_profiler_metric_index() {
        let mut agg = ScopeAggregator::default();
        let frame = agg.path(None, "frame");
        for v in 1..=10u64 {
            agg.ingest_frame(&[ScopeSample {
                path: frame,
                ns: v * MS,
            }]);
        }
        let f = agg.stats().remove(0);
        // metric_percentile_index: (p * (n - 1) + 50) / 100 over the sorted window.
        assert_eq!((f.p50, f.p90, f.p95, f.p99), (6.0, 9.0, 10.0, 10.0));
        assert_eq!(f.mean, 5.5);
    }

    #[test]
    fn reset_keeps_paths_and_clears_samples() {
        let mut agg = ScopeAggregator::default();
        let frame = agg.path(None, "frame");
        agg.ingest_frame(&[ScopeSample {
            path: frame,
            ns: MS,
        }]);
        agg.drop_frame();
        agg.reset_stats();
        assert_eq!((agg.frames(), agg.dropped_frames()), (0, 0));
        assert!(agg.stats().is_empty());
        assert_eq!(agg.path(None, "frame"), frame);
        agg.ingest_frame(&[ScopeSample {
            path: frame,
            ns: 2 * MS,
        }]);
        agg.drop_frame();
        assert_eq!(agg.stats()[0].mean, 2.0);
        assert_eq!((agg.frames(), agg.dropped_frames()), (1, 1));
    }
}
