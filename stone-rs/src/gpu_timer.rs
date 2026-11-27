//! Timestamp queries can nest; `GL_TIME_ELAPSED` queries cannot. Readback must
//! not block or it stalls the pipeline. Timer queries are unavailable on Apple's
//! desktop driver and report -2.0; `finish` mode serializes CPU and GPU work
//! with `glFinish`.

use crate::DebugGl;
use std::collections::{HashMap, VecDeque};

const RING: usize = 4;

/// Timed passes awaiting readback. A pass timed while this many are pending
/// is not timed; its caller simply gets no sample for it.
const TIMED_PASSES: usize = 16;

struct TimedPass {
    tag: u32,
    begin: glow::Query,
    end: glow::Query,
}

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
    /// Finish mode's timestamps instead: (ns since the frame's epoch, ns
    /// waited in glFinish by then).
    clock: Vec<(u64, u64)>,
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
            clock: Vec::new(),
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

pub struct GpuTimer {
    slots: [FrameSlot; RING],
    write: usize,
    read: usize,
    /// Open entries of the recording slot (indices into its `entries`).
    stack: Vec<usize>,
    mode: ScopeMode,
    /// Finish mode: the frame's clock origin, when its first glFinish began.
    epoch: std::time::Instant,
    /// Finish mode: nanoseconds blocked in glFinish so far this frame.
    waited: u64,
    supported: bool,
    debug_groups: bool,
    /// Debug groups pushed and not yet popped in the recording frame.
    open_groups: usize,
    initialized: bool,
    root: PathId,
    aggregate: ScopeAggregator,
    unbalanced_logged: bool,
    timed: VecDeque<TimedPass>,
    timed_open: Option<glow::Query>,
    timed_free: Vec<glow::Query>,
    timed_tag: u32,
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
            epoch: std::time::Instant::now(),
            waited: 0,
            supported: true,
            debug_groups: false,
            open_groups: 0,
            initialized: false,
            root,
            aggregate,
            unbalanced_logged: false,
            timed: VecDeque::new(),
            timed_open: None,
            timed_free: Vec::new(),
            timed_tag: 0,
        }
    }

    fn scopes_requested(&self) -> bool {
        self.mode != ScopeMode::Off
    }

    fn scopes_active(&self) -> bool {
        self.scopes_requested() && self.supported
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
        if self.mode != ScopeMode::Finish && !gl.supports_timer_query() {
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
                "GPU scopes on: {}, KHR_debug groups {}",
                if self.mode == ScopeMode::Finish {
                    "glFinish-bracketed wall time per scope (no timer queries)"
                } else {
                    "timestamp queries per scope"
                },
                if self.debug_groups { "on" } else { "unsupported on this context" }
            );
        }
    }

    /// Next timestamp of the recording slot: a GL query, or in finish mode
    /// the clock after a glFinish. Query objects are created on first use of
    /// that pool index.
    fn take_stamp(&mut self, gl: &DebugGl) -> Option<usize> {
        let slot = &mut self.slots[self.write];
        if self.mode == ScopeMode::Finish {
            let start = std::time::Instant::now();
            if slot.clock.is_empty() {
                self.epoch = start;
                slot.clock.push((0, 0));
            }
            unsafe { gl.finish() };
            let end = std::time::Instant::now();
            self.waited += end.duration_since(start).as_nanos() as u64;
            let at = end.duration_since(self.epoch).as_nanos() as u64;
            slot.clock.push((at, self.waited));
            return Some(slot.clock.len() - 1);
        }
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

    /// Begin the frame: the root scope. Finish mode also records the wait
    /// for earlier work as `before_frame`.
    pub fn begin(&mut self, gl: &DebugGl) {
        self.ensure_init(gl);
        if !self.supported {
            return;
        }
        let scopes_requested = self.scopes_requested();
        let slot = &mut self.slots[self.write];
        if slot.state == SlotState::Pending {
            if scopes_requested {
                self.aggregate.drop_frame();
            }
            if self.read == self.write {
                self.read = (self.read + 1) % RING;
            }
        }
        slot.state = SlotState::Recording;
        slot.used = 0;
        slot.clock.clear();
        slot.entries.clear();
        slot.unbalanced = false;
        self.stack.clear();
        self.open_groups = 0;
        self.waited = 0;
        let before =
            (self.mode == ScopeMode::Finish).then(|| self.aggregate.path(None, "before_frame"));
        let Some(begin) = self.take_stamp(gl) else {
            return;
        };
        let root = self.root;
        let entries = &mut self.slots[self.write].entries;
        entries.push(Entry {
            path: root,
            begin,
            end: None,
        });
        if let Some(path) = before {
            entries.push(Entry {
                path,
                begin: 0,
                end: Some(begin),
            });
        }
        self.stack.push(0);
    }

    fn recording(&self) -> bool {
        self.slots[self.write].state == SlotState::Recording
    }

    pub fn scope_begin(&mut self, gl: &DebugGl, name: &str) {
        if !self.scopes_active() || !self.recording() {
            return;
        }
        let parent_entry = *self
            .stack
            .last()
            .expect("recording frame has its root open");
        let parent = self.slots[self.write].entries[parent_entry].path;
        let path = self.aggregate.path(Some(parent), name);
        let Some(begin) = self.take_stamp(gl) else {
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
        if !self.scopes_active() || !self.recording() {
            return;
        }
        if self.stack.len() <= 1 {
            self.mark_unbalanced("GPU scope end without a matching begin");
            return;
        }
        let entry = self.stack.pop().expect("checked above");
        let Some(end) = self.take_stamp(gl) else {
            return;
        };
        self.slots[self.write].entries[entry].end = Some(end);
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

    /// End the frame's root scope and hand the slot to readback, which is
    /// immediate in finish mode.
    pub fn end(&mut self, gl: &DebugGl) {
        if !self.supported || !self.recording() {
            return;
        }
        if self.stack.len() > 1 {
            self.mark_unbalanced("GPU scopes still open at frame end");
            self.stack.truncate(1);
        }
        while self.open_groups > 0 {
            unsafe { gl.pop_debug_group() };
            self.open_groups -= 1;
        }
        let Some(end) = self.take_stamp(gl) else {
            return;
        };
        let slot = &mut self.slots[self.write];
        slot.entries[0].end = Some(end);
        slot.state = SlotState::Pending;
        self.stack.clear();
        self.write = (self.write + 1) % RING;
        if self.mode == ScopeMode::Finish {
            self.read_slot(gl);
        }
    }

    pub fn poll_ms(&mut self, gl: &DebugGl) -> f32 {
        self.ensure_init(gl);
        if !self.supported || self.mode == ScopeMode::Finish {
            return -2.0;
        }
        let slot = &self.slots[self.read];
        if slot.state != SlotState::Pending {
            return -1.0;
        }
        // Timestamps complete in submission order, so the last query being available implies the
        // rest are.
        let last = slot.queries[slot.used - 1];
        if unsafe { gl.get_query_parameter_u32(last, glow::QUERY_RESULT_AVAILABLE) } == 0 {
            return -1.0;
        }
        self.read_slot(gl) as f32 / 1_000_000.0
    }

    /// Ingest the oldest pending slot's scopes; returns its frame ns.
    fn read_slot(&mut self, gl: &DebugGl) -> u64 {
        let slot = &self.slots[self.read];
        let finish = self.mode == ScopeMode::Finish;
        let stamp = |i: usize| {
            if finish {
                slot.clock[i]
            } else {
                let query = slot.queries[i];
                (
                    unsafe { gl.get_query_parameter_u64(query, glow::QUERY_RESULT) },
                    0,
                )
            }
        };
        let span = |e: &Entry| {
            let ((begin, begin_wait), (end, end_wait)) = (stamp(e.begin), stamp(e.end?));
            Some((end.saturating_sub(begin), end_wait - begin_wait))
        };
        let frame_ns = span(&slot.entries[0]).map_or(0, |(ns, _)| ns);
        if self.scopes_requested() {
            if slot.unbalanced {
                self.aggregate.drop_frame();
            } else {
                let (samples, waits): (Vec<_>, Vec<_>) = slot
                    .entries
                    .iter()
                    .filter_map(|e| {
                        let (ns, wait) = span(e)?;
                        let path = e.path;
                        Some((ScopeSample { path, ns }, ScopeSample { path, ns: wait }))
                    })
                    .unzip();
                self.aggregate.ingest_frame(&samples);
                if finish {
                    self.aggregate.ingest_frame_waits(&waits);
                }
            }
        }
        self.slots[self.read].state = SlotState::Empty;
        self.read = (self.read + 1) % RING;
        frame_ns
    }

    pub fn reset_stats(&mut self) {
        self.aggregate.reset_stats();
    }

    fn timed_query(&mut self, gl: &DebugGl) -> Option<glow::Query> {
        let query = match self.timed_free.pop() {
            Some(q) => q,
            None => match unsafe { gl.create_query() } {
                Ok(q) => q,
                Err(e) => {
                    ::log::warn!(target: "STONE", "timed pass skipped: create_query failed: {e}");
                    return None;
                }
            },
        };
        unsafe { gl.query_counter(query, glow::TIMESTAMP) };
        Some(query)
    }

    pub fn timed_begin(&mut self, gl: &DebugGl) {
        self.ensure_init(gl);
        if !self.supported
            || self.mode == ScopeMode::Finish
            || self.timed_open.is_some()
            || self.timed.len() >= TIMED_PASSES
        {
            return;
        }
        self.timed_open = self.timed_query(gl);
    }

    /// Close the open timed pass and queue it for readback under `tag`.
    pub fn timed_end(&mut self, gl: &DebugGl, tag: u32) {
        let Some(begin) = self.timed_open.take() else {
            return;
        };
        let Some(end) = self.timed_query(gl) else {
            self.timed_free.push(begin);
            return;
        };
        // Tagged work can be polled before the next frame is submitted.
        unsafe { gl.flush() };
        self.timed.push_back(TimedPass { tag, begin, end });
    }

    pub fn timed_poll_ms(&mut self, gl: &DebugGl) -> f32 {
        self.ensure_init(gl);
        if !self.supported || self.mode == ScopeMode::Finish {
            return -2.0;
        }
        let Some(front) = self.timed.front() else {
            return -1.0;
        };
        if unsafe { gl.get_query_parameter_u32(front.end, glow::QUERY_RESULT_AVAILABLE) } == 0 {
            return -1.0;
        }
        let pass = self.timed.pop_front().expect("front checked above");
        let read = |q: glow::Query| unsafe { gl.get_query_parameter_u64(q, glow::QUERY_RESULT) };
        let timestamps = (read(pass.begin), read(pass.end));
        let ns = timestamps.1.saturating_sub(timestamps.0);
        self.timed_free.push(pass.begin);
        self.timed_free.push(pass.end);
        self.timed_tag = pass.tag;
        ns as f32 / 1_000_000.0
    }

    /// The tag of the pass `timed_poll_ms` last returned.
    pub fn timed_tag(&self) -> u32 {
        self.timed_tag
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
    let scopes = if status == "on" || status == "finish" {
        aggregate.rows(status == "finish")
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

    /// The `gpu_scopes` report rows in depth-first order (children after their
    /// parent, siblings in first-seen order), skipping paths with no sample in the
    /// window. `waits` adds finish mode's glFinish wait, averaged over every
    /// measured frame; the wall time minus it is CPU time in the scope.
    pub fn rows(&self, waits: bool) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        for (id, info) in self.paths.iter().enumerate() {
            if info.parent.is_none() {
                self.collect(id as PathId, waits, &mut out);
            }
        }
        out
    }

    fn collect(&self, id: PathId, waits: bool, out: &mut Vec<serde_json::Value>) {
        let info = &self.paths[id as usize];
        let row = &self.rows[id as usize];
        if !row.frame_ms.is_empty() {
            let mut sorted = row.frame_ms.clone();
            sorted.sort_by(f64::total_cmp);
            let n = sorted.len();
            let total = sorted.iter().sum::<f64>();
            let mean = total / n as f64;
            let variance = sorted.iter().map(|v| (v - mean) * (v - mean)).sum::<f64>() / n as f64;
            let pick = |p: usize| sorted[metric_percentile_index(p, n)];
            let per_frame = |sum: f64| {
                if self.frames > 0 {
                    sum / self.frames as f64
                } else {
                    0.0
                }
            };
            let mut stat = serde_json::json!({
                "name": self.names[info.name as usize],
                "path": self.path_string(id),
                "parent": info.parent.map(|p| self.path_string(p)),
                "depth": info.depth,
                "calls_per_frame": per_frame(row.calls as f64),
                "ms_per_frame": per_frame(total),
                "ms": {
                    "mean": mean, "stddev": variance.sqrt(), "min": sorted[0], "max": sorted[n - 1],
                    "p50": pick(50), "p90": pick(90), "p95": pick(95), "p99": pick(99), "n": n,
                },
            });
            if waits {
                stat["wait_ms_per_frame"] = per_frame(row.frame_wait_ms.iter().sum()).into();
            }
            out.push(stat);
        }
        for (child, child_info) in self.paths.iter().enumerate() {
            if child_info.parent == Some(id) {
                self.collect(child as PathId, waits, out);
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
    fn named_query_scopes_record_timestamps_without_flushing() {
        use std::cell::Cell;
        use std::ffi::c_void;

        thread_local! {
            static STAMPS: Cell<u32> = const { Cell::new(0) };
            static FLUSHES: Cell<u32> = const { Cell::new(0) };
        }
        unsafe extern "system" fn get_string(_: u32) -> *const u8 {
            c"3.3".as_ptr().cast()
        }
        unsafe extern "system" fn get_integer(_: u32, value: *mut i32) {
            *value = 0;
        }
        unsafe extern "system" fn get_error() -> u32 {
            glow::NO_ERROR
        }
        unsafe extern "system" fn gen_queries(_: i32, query: *mut u32) {
            *query = STAMPS.with(Cell::get) + 1;
        }
        unsafe extern "system" fn query_counter(_: u32, _: u32) {
            STAMPS.with(|stamps| stamps.set(stamps.get() + 1));
        }
        unsafe extern "system" fn flush() {
            FLUSHES.with(|flushes| flushes.set(flushes.get() + 1));
        }

        let gl = crate::DebugGl::new(unsafe {
            glow::Context::from_loader_function(|name| match name {
                "glGetString" => get_string as *const c_void,
                "glGetIntegerv" => get_integer as *const c_void,
                "glGetError" => get_error as *const c_void,
                "glGenQueries" => gen_queries as *const c_void,
                "glQueryCounter" => query_counter as *const c_void,
                "glFlush" => flush as *const c_void,
                _ => std::ptr::null(),
            })
        });
        let mut timer = super::GpuTimer::new();
        timer.mode = ScopeMode::Queries;
        timer.initialized = true;
        timer.begin(&gl);
        timer.scope_begin(&gl, "pass:probe");
        timer.scope_end(&gl);
        timer.end(&gl);

        assert_eq!(STAMPS.with(Cell::get), 4);
        assert_eq!(FLUSHES.with(Cell::get), 0);
    }

    fn sample(path: super::PathId, ns: u64) -> ScopeSample {
        ScopeSample { path, ns }
    }

    #[test]
    fn scope_mode_names_queries_finish_and_rejects_the_rest() {
        assert_eq!(scope_mode_from_env(None), Some(ScopeMode::Off));
        assert_eq!(scope_mode_from_env(Some("0")), Some(ScopeMode::Off));
        assert_eq!(scope_mode_from_env(Some("1")), Some(ScopeMode::Queries));
        assert_eq!(scope_mode_from_env(Some("finish")), Some(ScopeMode::Finish));
        assert_eq!(scope_mode_from_env(Some("yes")), None);
    }

    const MS: u64 = 1_000_000;

    fn stat<'a>(stats: &'a [serde_json::Value], path: &str) -> &'a serde_json::Value {
        stats
            .iter()
            .find(|s| s["path"] == path)
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
                sample(frame, f * MS),
                sample(a, a_ms * MS),
                sample(b, b1 * MS),
                sample(b, b2 * MS),
            ]);
        }
        let stats = agg.rows(true);
        let b = stat(&stats, "frame/a/b");
        assert_eq!(b["ms"]["n"], 2);
        assert_eq!(b["calls_per_frame"], 2.0);
        assert_eq!(b["ms"]["mean"], 4.0);
        assert_eq!(b["ms"]["min"], 3.0);
        assert_eq!(b["ms"]["max"], 5.0);
        assert_eq!(b["depth"], 2);
        assert_eq!(b["parent"], "frame/a");
        assert_eq!(stat(&stats, "frame")["ms"]["mean"], 11.0);
        let order: Vec<&str> = stats.iter().map(|s| s["path"].as_str().unwrap()).collect();
        assert_eq!(order, ["frame", "frame/a", "frame/a/b"]);
    }

    #[test]
    fn finish_waits_sum_per_frame_beside_the_wall_time() {
        let mut agg = ScopeAggregator::default();
        let frame = agg.path(None, "frame");
        let pass = agg.path(Some(frame), "pass:x");
        agg.ingest_frame(&[sample(frame, 10 * MS), sample(pass, 4 * MS)]);
        agg.ingest_frame_waits(&[
            sample(frame, 6 * MS),
            sample(pass, MS),
            sample(pass, 2 * MS),
        ]);
        agg.ingest_frame(&[sample(frame, 10 * MS)]);
        agg.ingest_frame_waits(&[sample(frame, 4 * MS)]);
        let stats = agg.rows(true);
        assert_eq!(stat(&stats, "frame")["wait_ms_per_frame"], 5.0);
        assert_eq!(stat(&stats, "frame/pass:x")["wait_ms_per_frame"], 1.5);
    }

    #[test]
    fn percentiles_use_the_profiler_metric_index() {
        let mut agg = ScopeAggregator::default();
        let frame = agg.path(None, "frame");
        for v in 1..=10u64 {
            agg.ingest_frame(&[sample(frame, v * MS)]);
        }
        let f = &agg.rows(false)[0]["ms"];
        let percentiles = ["p50", "p90", "p95", "p99"].map(|p| f[p].as_f64().unwrap());
        assert_eq!(percentiles, [6.0, 9.0, 10.0, 10.0]);
        assert_eq!(f["mean"], 5.5);
    }

    #[test]
    fn reset_keeps_paths_and_clears_samples() {
        let mut agg = ScopeAggregator::default();
        let frame = agg.path(None, "frame");
        agg.ingest_frame(&[sample(frame, MS)]);
        agg.drop_frame();
        agg.reset_stats();
        assert_eq!((agg.frames(), agg.dropped_frames()), (0, 0));
        assert!(agg.rows(false).is_empty());
        assert_eq!(agg.path(None, "frame"), frame);
        agg.ingest_frame(&[sample(frame, 2 * MS)]);
        agg.drop_frame();
        assert_eq!(agg.rows(false)[0]["ms"]["mean"], 2.0);
        assert_eq!((agg.frames(), agg.dropped_frames()), (1, 1));
    }
}
