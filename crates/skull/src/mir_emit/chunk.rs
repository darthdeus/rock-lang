use super::*;
use skull_codegen_lua::CALLEE_FACTORY_PREFIX;

#[derive(Clone)]
pub(super) struct Chunk {
    pub(super) name: Ustr,
    pub(super) lua: String,
    pub(super) definition_count: usize,
    pub(super) literal_globals: Vec<Ustr>,
    pub(super) allocation_bodies: Vec<crate::allocation_report::AllocationBody>,
    // Invalidation superset: also holds everything inlined away.
    pub(super) deps: FxHashSet<FuncRef>,
    // Always invalidate on a body edit; a residual edge can instead compare the
    // old and new candidate's eligibility.
    pub(super) embedded_deps: FxHashSet<FuncRef>,
    pub(super) abi_deps: FxHashMap<FuncRef, u64>,
    pub(super) direct_globals: Vec<Ustr>,
    pub(super) direct_calls: Vec<Ustr>,
    pub(super) times: PhaseTimes,
    pub(super) attribution: Option<Box<UnitAttribution>>,
    pub(super) statics: Vec<skull_mir::mir::StaticCell>,
    // A stale read is a body edit bone's reinferred set cannot see.
    pub(super) comptime_reads: Vec<skull_mir::mir::ComptimeRead>,
    // The callees still called after inlining: the reachability edges.
    pub(super) calls: Vec<FuncRef>,
    // Deduplicated program-wide: each specialization is defined exactly once.
    pub(super) specs: Vec<skull_mir::bound::SpecDemand>,
    pub(super) code_artifact: skull_mir::code_identity::BodyCodeArtifact,
    // Bound after every definition, so each captured callee is already final.
    pub(super) binds: Vec<skull_codegen_lua::CalleeBindingPlan>,
}

impl Chunk {
    pub(super) fn index_dependents(
        &self,
        body: &Instance,
        dependents: &mut FxHashMap<FuncRef, FxHashSet<Instance>>,
    ) -> usize {
        for callee in &self.deps {
            dependents.entry(*callee).or_default().insert(body.clone());
        }
        self.deps.len()
    }

    pub(super) fn code_identities(
        &self,
    ) -> impl Iterator<Item = skull_mir::code_identity::CodeIdentity> + '_ {
        self.code_artifact
            .entries()
            .iter()
            .map(|entry| entry.identity.clone())
    }

    // `shrink_to_fit` would not do: mimalloc keeps a block in place when it
    // shrinks by less than half.
    pub(super) fn finish(&mut self, attribution: bool) {
        if !attribution {
            self.attribution = None;
        }
        if self.lua.capacity() > self.lua.len() {
            self.lua = self.lua.as_str().to_owned();
        }
    }
}

// Each global definition costs the enclosing LuaJIT proto a child proto and name.
const LUA_DEFINITIONS_PER_SCOPE: usize = 4_096;

// Every global a statement names is a constant of the enclosing LuaJIT proto,
// whose constant index is 16 bits. The binding pass is nothing BUT global
// names, so it is split by name count rather than by statement count.
const LUA_NAMES_PER_BIND_SCOPE: usize = 16_384;

/// Ordered by call-graph component rank: a captured callee's component is
/// A same-component callee stays a global lookup. Ties break on name so the
/// text is deterministic.
/// strictly lower than its caller's, so ascending rank binds callees first.
pub(super) fn callee_binding_pass_lua<
    P: std::borrow::Borrow<skull_codegen_lua::CalleeBindingPlan> + Sync,
>(
    plans: &mut [P],
) -> String {
    const PLANS_PER_JOB: usize = 256;
    if plans.is_empty() {
        return String::new();
    }
    plans.sort_by(|a, b| {
        let (a, b) = (a.borrow(), b.borrow());
        a.rank.cmp(&b.rank).then_with(|| a.name.cmp(&b.name))
    });
    let mut opens = Vec::with_capacity(plans.len());
    let mut closes = Vec::with_capacity(plans.len());
    let mut names_in_scope = 0usize;
    for plan in plans.iter().map(|plan| plan.borrow()) {
        opens.push(names_in_scope == 0);
        names_in_scope += plan.captures.len() + 2;
        let close = names_in_scope >= LUA_NAMES_PER_BIND_SCOPE;
        if close {
            names_in_scope = 0;
        }
        closes.push(close);
    }
    if names_in_scope > 0 {
        *closes.last_mut().expect("plans is not empty") = true;
    }
    let plans = &*plans;
    let starts: Vec<usize> = (0..plans.len()).step_by(PLANS_PER_JOB).collect();
    let parts = par_map(starts, |start| {
        let mut out = String::new();
        for at in start..(start + PLANS_PER_JOB).min(plans.len()) {
            if opens[at] {
                out.push_str("do\n(function()\n");
            }
            plans[at].borrow().write_bind_line(&mut out);
            if closes[at] {
                out.push_str("end)()\nend\n");
            }
        }
        out
    });
    par_concat(&parts.iter().map(String::as_str).collect::<Vec<_>>())
}

pub(super) enum DefinitionPiece<'c> {
    Text(String),
    Chunk(&'c Chunk),
}

#[derive(Default)]
pub(super) struct DefinitionOrder<'c> {
    pub(super) pieces: Vec<DefinitionPiece<'c>>,
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

#[derive(Default)]
struct DefinitionScopes {
    definitions_in_scope: usize,
    scoped: bool,
}

impl DefinitionScopes {
    fn before(&mut self, count: usize) -> (bool, Option<&'static str>) {
        let mut opens_section = false;
        let mut separator = None;
        if self.definitions_in_scope + count > LUA_DEFINITIONS_PER_SCOPE {
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

pub(super) fn definition_global(line: &str) -> Option<&str> {
    let body = skull_codegen_lua::emitted_body_name(line)?;
    if line.starts_with("function ") {
        return Some(body);
    }
    Some(&line[..CALLEE_FACTORY_PREFIX.len() + body.len()])
}

pub(super) fn lua_definition_count(lua: &str) -> usize {
    fn is_definition(lua: &str, start: usize) -> bool {
        if !matches!(lua.as_bytes().get(start), Some(b'f' | b'_')) {
            return false;
        }
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
