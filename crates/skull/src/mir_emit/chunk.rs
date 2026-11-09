//! Emitted body chunks and the scoped Lua definition buffer.

use super::*;
use skull_codegen_lua::CALLEE_FACTORY_PREFIX;

/// One emitted ordinary/coroutine function definition plus the facts incremental
/// emit needs. The cheap definitions (prelude, decorators, externs, inline(lua),
/// global inits, init ordering, deco applications) are NOT cached — they are
/// regenerated on every [`assemble`].
#[derive(Clone)]
pub(super) struct Chunk {
    pub(super) lua: String,
    /// Top-level definitions in `lua`, counted where the chunk is built so
    /// serial assembly does not rescan every emitted line.
    pub(super) definition_count: usize,
    /// Includes hoisted closures and spliced bodies, unlike init-ordering refs.
    pub(super) literal_globals: Vec<Ustr>,
    pub(super) allocation_bodies: Vec<crate::allocation_report::AllocationBody>,
    /// Source and eligibility dependencies, including residual callees.
    pub(super) deps: FxHashSet<FuncRef>,
    /// Bodies embedded in this chunk, including transitive and hoisted splices.
    /// These always invalidate on a body edit; residual edges can instead use
    /// the old/new inline candidate's eligibility facts.
    pub(super) embedded_deps: FxHashSet<FuncRef>,
    /// Body-derived transport is independent of inline candidate eligibility.
    pub(super) abi_deps: FxHashSet<FuncRef>,
    /// Global-init ordering facts (globals read + functions called).
    pub(super) direct_globals: Vec<Ustr>,
    pub(super) direct_calls: Vec<Ustr>,
    /// Per-stage emit time captured when this chunk was built, summed back into
    /// the `backend-stats` breakdown by [`assemble`] when the chunk is pulled.
    pub(super) times: PhaseTimes,
    pub(super) attribution: UnitAttribution,
    /// `static` cells this body declares -- aggregated at chunk-pull for the
    /// hot-reload preservation preamble + `__rock_known_globals` registration.
    pub(super) statics: Vec<skull_mir::mir::StaticCell>,
    /// Comptime inputs baked into this chunk's Lua (build_var/embed_string).
    /// `update_chunk_cache` compares them against the current inputs and treats
    /// a stale read as a body edit -- bone's reinferred set cannot see these.
    pub(super) comptime_reads: Vec<skull_mir::mir::ComptimeRead>,
    /// The callees this body still CALLS after inlining -- the reachability edge
    /// set. Distinct from `deps`, which is an invalidation superset (it also holds
    /// everything inlined AWAY, and would keep spliced-out callees alive).
    pub(super) calls: Vec<FuncRef>,
    /// Callees this body invokes through a SPECIALIZED instantiation that no
    /// splice served, so each needs a standalone specialized copy emitted.
    /// Deduplicated program-wide by [`assemble`] -- many bodies ask for the same
    /// one, and it must be defined exactly once.
    pub(super) specs: Vec<skull_mir::bound::SpecDemand>,
    /// Compiler-owned logical callable identities in this emitted body.
    pub(super) code_artifact: skull_mir::code_identity::CodeArtifact,
    /// The definitions in this chunk that were emitted as factories, each with
    /// the second-pass line that binds it. [`assemble`] emits these in
    /// ascending `rank` after EVERY definition, so each captured callee is
    /// already its final closure.
    pub(super) binds: Vec<skull_codegen_lua::CalleeBindingPlan>,
}

// Each global definition costs the enclosing LuaJIT proto a child proto and name.
const LUA_DEFINITIONS_PER_SCOPE: usize = 4_096;

// Every global a statement names is a constant of the enclosing LuaJIT proto,
// whose constant index is 16 bits. The binding pass is nothing BUT global
// names, so it is split by name count rather than by statement count.
const LUA_NAMES_PER_BIND_SCOPE: usize = 16_384;

/// The callee binding's SECOND PASS: one `<name> = __rock_mk_<name>(<callees>)`
/// line per factory, emitted after every definition so a captured callee's
/// global already holds its final closure.
///
/// Ordered by call-graph component rank, which [`skull_codegen_lua::call_sccs`]
/// numbers in Tarjan completion order: a captured callee's component is
/// strictly lower than its caller's (they are never equal -- a same-component
/// callee stays a global lookup), so ascending rank binds callees first. Ties
/// break on name so the emitted text is deterministic.
pub(super) fn callee_binding_pass_lua(
    plans: &mut [skull_codegen_lua::CalleeBindingPlan],
) -> String {
    if plans.is_empty() {
        return String::new();
    }
    plans.sort_by(|a, b| a.rank.cmp(&b.rank).then_with(|| a.name.cmp(&b.name)));
    let mut out = String::new();
    let mut names_in_scope = 0usize;
    for plan in plans.iter() {
        if names_in_scope == 0 {
            out.push_str("do\n(function()\n");
        }
        out.push_str(&plan.bind_line());
        names_in_scope += plan.captures.len() + 2;
        if names_in_scope >= LUA_NAMES_PER_BIND_SCOPE {
            out.push_str("end)()\nend\n");
            names_in_scope = 0;
        }
    }
    if names_in_scope > 0 {
        out.push_str("end)()\nend\n");
    }
    out
}

pub(super) struct LuaDefinitionBuffer {
    lua: String,
    units: Vec<(usize, usize, usize)>,
    definition_count: usize,
}

/// The global a top-level definition assigns, in either emitted shape.
/// Definition splitting consumes this inventory.
fn definition_global(line: &str) -> Option<&str> {
    let body = skull_codegen_lua::emitted_body_name(line)?;
    if line.starts_with("function ") {
        return Some(body);
    }
    Some(&line[..CALLEE_FACTORY_PREFIX.len() + body.len()])
}

/// How many top-level definitions `lua` holds. A definition starts its line
/// with one of two fixed prefixes, so only line starts are examined: the
/// text is 46 MB of mostly short lines on a large project, where splitting
/// into `str` lines cost more than the whole emit of some bodies.
pub(super) fn lua_definition_count(lua: &str) -> usize {
    fn is_definition(lua: &str, start: usize) -> bool {
        let rest = &lua[start..];
        if !(rest.starts_with("function ") || rest.starts_with(CALLEE_FACTORY_PREFIX)) {
            return false;
        }
        let line = rest.split_once('\n').map_or(rest, |(line, _)| line);
        definition_global(line).is_some()
    }
    let mut count = usize::from(is_definition(lua, 0));
    count += memchr::memchr_iter(b'\n', lua.as_bytes())
        .filter(|&at| is_definition(lua, at + 1))
        .count();
    count
}

impl LuaDefinitionBuffer {
    pub(super) fn new() -> Self {
        Self {
            lua: String::new(),
            units: Vec::new(),
            definition_count: 0,
        }
    }

    pub(super) fn push_str(&mut self, lua: &str) {
        self.push_counted(lua, lua_definition_count(lua));
    }

    pub(super) fn push_chunk(&mut self, chunk: &Chunk) {
        self.push_counted(&chunk.lua, chunk.definition_count);
    }

    fn push_counted(&mut self, lua: &str, count: usize) {
        let start = self.lua.len();
        self.lua.push_str(lua);
        self.units.push((start, self.lua.len(), count));
        self.definition_count += count;
    }

    pub(super) fn append_to(self, out: &mut String) {
        if self.definition_count <= LUA_DEFINITIONS_PER_SCOPE {
            out.push_str(&self.lua);
            return;
        }

        out.push_str("do\n(function()\n");
        let mut definitions_in_scope = 0;
        for (start, end, count) in self.units {
            if definitions_in_scope > 0 && definitions_in_scope + count > LUA_DEFINITIONS_PER_SCOPE
            {
                out.push_str("end)()\nend\ndo\n(function()\n");
                definitions_in_scope = 0;
            }
            out.push_str(&self.lua[start..end]);
            definitions_in_scope += count;
        }
        out.push_str("end)()\nend\n");
    }
}
