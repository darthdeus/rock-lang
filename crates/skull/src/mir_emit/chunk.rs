//! Emitted body chunks and the scoped Lua definition writer.

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
    /// Callees whose call transport follows their body, each with the
    /// parameter demand key it was lowered against. This is independent of
    /// inline candidate eligibility.
    pub(super) abi_deps: FxHashMap<FuncRef, u64>,
    /// Global-init ordering facts (globals read + functions called).
    pub(super) direct_globals: Vec<Ustr>,
    pub(super) direct_calls: Vec<Ustr>,
    /// Per-stage emit time captured when this chunk was built, summed back into
    /// the `backend-stats` breakdown by [`assemble`] when the chunk is pulled.
    pub(super) times: PhaseTimes,
    /// Boxed: it is most of a chunk's size, and chunks move between the
    /// fan-out, the chunk map and assembly. A full build that does not fold
    /// attribution drops it as each chunk is built.
    pub(super) attribution: Option<Box<UnitAttribution>>,
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
pub(super) fn callee_binding_pass_lua<
    P: std::borrow::Borrow<skull_codegen_lua::CalleeBindingPlan>,
>(
    plans: &mut [P],
) -> String {
    if plans.is_empty() {
        return String::new();
    }
    plans.sort_by(|a, b| {
        let (a, b) = (a.borrow(), b.borrow());
        a.rank.cmp(&b.rank).then_with(|| a.name.cmp(&b.name))
    });
    let mut out = String::new();
    let mut names_in_scope = 0usize;
    for plan in plans.iter().map(|plan| plan.borrow()) {
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

/// One definition's text, in output order: generated text or a chunk's Lua.
pub(super) enum DefinitionPiece<'c> {
    Text(String),
    Chunk(&'c Chunk),
}

/// The definitions an assembly emits, in output order, as references into the
/// chunk caches. The ordered pass only records these; what reads them (the
/// text, the code artifact, the callee binds, the runtime tables) runs after
/// it, independently.
#[derive(Default)]
pub(super) struct DefinitionOrder<'c> {
    pub(super) pieces: Vec<DefinitionPiece<'c>>,
    /// The chunks among `pieces`, in the same order.
    pub(super) emitted: Vec<&'c Chunk>,
}

impl<'c> DefinitionOrder<'c> {
    pub(super) fn push_text(&mut self, lua: String) {
        self.pieces.push(DefinitionPiece::Text(lua));
    }

    pub(super) fn push_chunk(&mut self, chunk: &'c Chunk) {
        self.pieces.push(DefinitionPiece::Chunk(chunk));
        self.emitted.push(chunk);
    }

    /// The definitions' text, scoped as [`LuaDefinitionWriter`] scopes a
    /// section. The scopes are decided from the counts alone, so the copying
    /// is one parallel concatenation.
    pub(super) fn definitions_lua(&self) -> String {
        let mut scopes = DefinitionScopes::default();
        let mut segments: Vec<&str> = Vec::with_capacity(self.pieces.len() + 2);
        segments.push("");
        for piece in &self.pieces {
            let (lua, count) = match piece {
                DefinitionPiece::Text(lua) => (lua.as_str(), lua_definition_count(lua)),
                DefinitionPiece::Chunk(chunk) => (chunk.lua.as_str(), chunk.definition_count),
            };
            let (opens_section, separator) = scopes.before(count);
            if opens_section {
                segments[0] = SCOPE_OPEN;
            }
            segments.extend(separator);
            segments.push(lua);
        }
        segments.extend(scopes.close());
        par_concat(&segments)
    }

    /// The enum names tables and value ctors the emitted chunks read.
    pub(super) fn runtime_refs(&self) -> FxHashSet<Ustr> {
        self.emitted
            .iter()
            .flat_map(|chunk| chunk.literal_globals.iter().copied())
            .filter(|name| crate::is_enum_names_global(name) || crate::is_value_ctor_global(name))
            .collect()
    }
}

pub(super) struct LuaDefinitionWriter {
    lua: String,
    section_start: usize,
    scopes: DefinitionScopes,
}

const SCOPE_OPEN: &str = "do\n(function()\n";
const SCOPE_CLOSE: &str = "end)()\nend\n";
const SCOPE_NEXT: &str = "end)()\nend\ndo\n(function()\n";

/// Where a definition section breaks into function scopes, so no scope holds
/// more than [`LUA_DEFINITIONS_PER_SCOPE`] definitions.
#[derive(Default)]
struct DefinitionScopes {
    definitions_in_scope: usize,
    scoped: bool,
}

impl DefinitionScopes {
    /// Before a piece of `count` definitions: whether the first scope opens
    /// at the section start, and the text that ends the current scope.
    fn before(&mut self, count: usize) -> (bool, Option<&'static str>) {
        let mut opens_section = false;
        let mut separator = None;
        if self.definitions_in_scope + count > LUA_DEFINITIONS_PER_SCOPE {
            // Small sections stay unwrapped.
            if !self.scoped {
                opens_section = true;
                self.scoped = true;
            }
            if self.definitions_in_scope > 0 {
                separator = Some(SCOPE_NEXT);
                self.definitions_in_scope = 0;
            }
        }
        self.definitions_in_scope += count;
        (opens_section, separator)
    }

    fn close(&self) -> Option<&'static str> {
        self.scoped.then_some(SCOPE_CLOSE)
    }
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

impl LuaDefinitionWriter {
    pub(super) fn new(lua: String) -> Self {
        Self {
            section_start: lua.len(),
            lua,
            scopes: DefinitionScopes::default(),
        }
    }

    pub(super) fn push_str(&mut self, lua: &str) {
        let (opens_section, separator) = self.scopes.before(lua_definition_count(lua));
        if opens_section {
            // Moves only the first scope's text.
            self.lua.insert_str(self.section_start, SCOPE_OPEN);
        }
        self.lua.extend(separator);
        self.lua.push_str(lua);
    }

    pub(super) fn finish(mut self) -> String {
        self.lua.extend(self.scopes.close());
        self.lua
    }
}
