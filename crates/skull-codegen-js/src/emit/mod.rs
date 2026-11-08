//! The MIR->JS emitter. Output is structured (goto-free), modeled on the GLSL
//! backend's `emit_seq`/`emit_branch` rather than the goto-linear Lua backend:
//! JS, like GLSL, has no goto, so a `Branch` recovers into `if/else` and its
//! arms rejoin at the block's immediate post-dominator. NativeBuffers are
//! TypedArray-backed `{ptr, len}` structs with checked `buf[i]` through the
//! `__nb_checked_*` shims (`crate::JS_NB_CHECK_SHIMS` supplies the bodies). A
//! MIR op the emitter does not support is a spanned `CompilerError`, never
//! emit-and-hope.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;

use bone_core::workspace::Workspace;
use comfy_la_arena::{Idx, RawIdx};
use skull_mir::cfg::relooper::{LoopCtx, LoopFrame, LoopTest, StructuredEmit};
use skull_mir::cfg::structure::{
    self, CountedBound, CountedLoop, LoopRegion, LoopShapes, Structure,
};
use skull_mir::code_identity::{CodeArtifact, CodeIdentity};
use skull_mir::decorator::{DecoratorTable, SimValues, decorator_symbol};
use skull_mir::global_storage::StaticBaseline;
use skull_mir::instance::Instance;
use skull_mir::intrinsics::{Emission, IntrinsicTable};
use skull_mir::mangle::{ident_of_key, symbol_of_key};
use skull_mir::mir::{Block, Cell, ContainerKind, FuncRef, Inst, Lit, MirFunc, Term};

/// The `Lit::Omitted` marker, defined in [`crate::JS_TABLE_PRIM`].
const OMITTED_ARG: &str = "__rock_omitted";
use skull_graph::artifact::{self, ArtifactView};
use skull_graph::instance_graph::{InstanceGraph, Site, SiteOutcome};
use skull_graph::target_projection;
use skull_parser::{CompilerError, Span};
use ustr::Ustr;

use crate::{JsIntrinsics, fill_template};

mod analysis;
mod assemble;
mod coroutine;
mod delivery;
mod effects;
mod function;
mod globals;
mod inst;
mod lanes;
mod overlay;
mod plan_stages;
mod session;
mod structured;
mod tables;
#[cfg(test)]
mod tests;
mod worklist;

use analysis::*;
pub use assemble::*;
use coroutine::*;
use delivery::*;
pub use function::*;
pub use globals::*;
pub use inst::*;
use lanes::*;
pub use overlay::*;
use plan_stages::*;
pub use session::*;
use tables::*;
use worklist::*;

fn raw<T>(idx: Idx<T>) -> u32 {
    idx.into_raw().into()
}

/// JS runs counted-loop recovery over the bodies conversion hoisted here; it
/// has no render-time nested pass to do it later, and never inlines into them.
const JS_CHAIN: skull_transform::body_chain::ChainOptions<'static> =
    skull_transform::body_chain::ChainOptions {
        reserve: &|_| {},
        counted_loops: skull_transform::body_chain::CountedLoops::RootAndNested,
        inline_hoisted: false,
    };

/// The shared pass chain every inlined body takes before discovery and
/// emission.
fn prepare_body(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    self_ref: FuncRef,
    mf: &mut MirFunc,
    name: &str,
    cache: &mut skull_transform::InlineCache,
    plans: &skull_mir::concrete::ConcretePlans,
) -> Result<(), CompilerError> {
    let inliner = skull_transform::body_chain::Inliner {
        scx,
        self_ref,
        cache,
        plans,
    };
    skull_transform::body_chain::prepare_body(ws, inliner, mf, name, &JS_CHAIN, &mut ())
        .map(|_| ())
        .map_err(|e| e.error)
}

/// The shared chain for a body that is not inlined.
fn convert_body(ws: &Workspace, mf: &mut MirFunc, name: &str) -> Result<(), CompilerError> {
    skull_transform::body_chain::convert_body(ws, mf, name, &JS_CHAIN, &mut ()).map_err(|e| e.error)
}

/// The module-global surface an emitted body may reference through `Inst::Global`
/// / `Inst::StoreGlobal`. Module-level `let`/`global` STATE renders into a scoped
/// `__G` object (`__G.<name>`) so no user global can collide with or be shadowed
/// by an emitted body local (`r<N>`/`v<N>`/`c<N>`/`__rock_env`); a top-level fn
/// referenced as a VALUE renders as a logical CodeRef slot (foreign functions
/// remain bare JS functions). Built once per program in
/// [`emit_with_closure`]; empty for the standalone [`emit_fn`] entry.
#[derive(Clone, Default)]
pub struct GlobalCtx {
    /// Compiler-assigned storage keys, keyed by physical symbols rather than DefKeys.
    state: HashMap<ustr::Ustr, String>,
    /// Set when an emitted body reads or writes a storage key: only then does
    /// a closure need the globals runtime.
    uses_storage: std::cell::Cell<bool>,
    /// Compile-time globals remain artifact constants in the backend runtime.
    const_globals: HashSet<ustr::Ustr>,
    /// A top-level fn used as a value -> its co-emitted JS name for direct calls.
    fns: HashMap<ustr::Ustr, String>,
    /// `__rock_enum_<E>_<V>` singletons / `__rock_enum_<E>_names` tables the body
    /// reads (rendered as a bare top-level name; the def is emitted per program).
    enum_globals: HashSet<ustr::Ustr>,
    /// Logical Rock callable identities to their runtime slot keys. Foreign
    /// function values are intentionally absent from this map.
    code_slots: HashMap<skull_mir::code_identity::CodeIdentity, String>,
}

fn err(msg: impl Into<String>) -> CompilerError {
    CompilerError::new_with_label(Span::default(), msg.into())
}

/// A `(message, site)` rejection from a MIR-level check, anchored at the
/// offending instruction when the check could name one.
fn sited_err((msg, site): (String, Option<Span>)) -> CompilerError {
    CompilerError::new_with_label(site.unwrap_or_default(), msg)
}

/// Every `nobox` materialization in one body, as one error carrying the rest as
/// siblings. Mirrors the luajit path: a body with several sinks reports several
/// findings, so neither backend's count of what is left is a floor.
fn nobox_errs(
    ws: &bone_core::workspace::Workspace,
    mf: &skull_mir::mir::MirFunc,
) -> Result<(), CompilerError> {
    // Name the body: the span often points into an INLINED stdlib body, and a
    // report now carries every body's sinks at once, so without the name two
    // sites read as duplicates of each other (same as the luajit path).
    let named = |(msg, site): (String, Option<Span>)| {
        sited_err((format!("{msg}, while emitting `{}`", mf.name), site))
    };
    let mut vs = skull_transform::nobox_materialization_violations(ws, mf).into_iter();
    let Some(first) = vs.next() else {
        return Ok(());
    };
    let mut e = named(first);
    e.siblings = vs.map(named).collect();
    Err(e)
}

/// Every finding in ONE definition as a single error carrying the rest as
/// siblings. `None` when there was nothing to report.
fn merge_findings(findings: Vec<CompilerError>) -> Option<CompilerError> {
    let mut it = findings.into_iter();
    let primary = it.next()?;
    Some(primary.with_siblings(it))
}

/// Collect a fallible check's error instead of returning it, so the checks after
/// it still run on the same body.
fn collect<T>(findings: &mut Vec<CompilerError>, r: Result<T, CompilerError>) {
    if let Err(e) = r {
        findings.push(e);
    }
}

/// The accumulated per-definition rejections as one failure: sorted by emitted
/// name (definition names are unique, and the walk's post-order must not reach
/// the report), first as primary and the rest as siblings.
///
/// Emission accumulates for the same reason the luajit path does: every
/// rejection here is fully computed before any is reported, so bailing at the
/// first definition hid the others rather than preventing them -- and the
/// `--backend js` skip histogram IS the emitter worklist, so a first-only report
/// makes that worklist a floor. Nothing is downgraded: a rejected definition
fn combine_rejections(mut rejections: Vec<(String, CompilerError)>) -> Result<(), CompilerError> {
    rejections.sort_by(|(a, _), (b, _)| a.cmp(b));
    match merge_findings(rejections.into_iter().map(|(_, e)| e).collect()) {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// A cell's JS identifier (`c<raw>`). Params and locals share the scheme; params
/// materialize in the fn arg list, other stored cells as hoisted `let`s.
fn cell_name(mf: &MirFunc, cell: Idx<Cell>) -> String {
    format!("c{}", mf.emit_cell_id(cell))
}

fn cell_is_uint(ws: &Workspace, ty: Option<bone_core::prelude::TyId>) -> bool {
    let Some(bone_core::prelude::TyId::Static(module, index)) = ty else {
        return false;
    };
    matches!(
        ws.module(module).ty_arena[index],
        bone_core::hir::TypeExpr::Builtin(bone_core::hir::BuiltinTy::Uint)
    )
}

/// A finite-float JS numeric literal, or a reject for inf/NaN (outside the
/// subset). `f64::to_string` already renders `3.0` as `3` and `0.5` as `0.5`,
/// both valid JS.
fn js_number(f: f64) -> Result<String, CompilerError> {
    if f.is_finite() {
        Ok(f.to_string())
    } else {
        Err(err("JS subset: non-finite float literal"))
    }
}

/// A JS double-quoted string literal escaping the control chars a JS lexer
/// needs. Non-ASCII chars encode as `\u` escapes (UTF-16 code units, the same
/// scheme the driver's `js_str` uses) -- JS string LENGTH then counts UTF-16
/// units where Lua counts bytes, a known cross-backend divergence for
/// non-ASCII content; rejecting outright bricked whole game builds over an
/// em-dash in a log message.
fn js_string_lit(s: &str) -> Result<String, CompilerError> {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if c.is_ascii() && !c.is_ascii_control() => o.push(c),
            c => {
                for u in c.encode_utf16(&mut [0u16; 2]) {
                    o.push_str(&format!("\\u{:04x}", u));
                }
            }
        }
    }
    o.push('"');
    Ok(o)
}

/// A field name that is not a bare JS identifier -- flattened enum/embed slots
/// are dotted physical keys (`active.#`, `active.0`) -- must be quoted in an
/// object literal and bracketed in a member access.
fn is_js_ident(s: &str) -> bool {
    let mut cs = s.chars();
    match cs.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$' => {}
        _ => return false,
    }
    cs.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

/// `f` as an object-literal key: bare when it is an identifier, quoted otherwise.
fn js_key(f: &str) -> Result<String, CompilerError> {
    if is_js_ident(f) {
        Ok(f.to_string())
    } else {
        js_string_lit(f)
    }
}

/// `base.f` / `base["f"]` -- property access that survives a non-identifier key.
fn js_member(base: &str, f: &str) -> Result<String, CompilerError> {
    if is_js_ident(f) {
        Ok(format!("{base}.{f}"))
    } else {
        Ok(format!("{base}[{}]", js_string_lit(f)?))
    }
}

/// The JS name every co-emitted callee is called by. An ordinary call targets a
/// base instance, while a call carrying an [`Instance`](skull_mir::instance::Instance)
/// targets that instantiation's standalone copy (`spec_fn_name`) -- for a
/// column-stored element the base body reads a `_data` the layout does not have,
/// so it cannot serve the call at all. One map over `Instance` keeps that choice
/// at the shared identity seam.
#[derive(Default)]
pub struct CalleeNames {
    names: HashMap<Instance, String>,
    graph_instances: Vec<Instance>,
    owner_instances: Vec<Instance>,
    site_targets: HashMap<(Instance, Site), Instance>,
}

impl CalleeNames {
    fn from_graph(graph: &InstanceGraph) -> Self {
        let owner_instances = graph
            .substitutions()
            .map(|(instance, _)| instance.clone())
            .collect();
        let site_targets = graph
            .call_sites()
            .flat_map(|(caller, sites)| {
                sites.iter().filter_map(|(site, record)| {
                    let SiteOutcome::Resolved(callee) = &record.outcome else {
                        return None;
                    };
                    Some(((caller.clone(), site.clone()), callee.clone()))
                })
            })
            .collect();
        Self {
            names: HashMap::new(),
            graph_instances: graph.instances.iter().cloned().collect(),
            owner_instances,
            site_targets,
        }
    }

    fn get(&self, body: &Instance) -> Option<&String> {
        self.names.get(body)
    }

    fn insert(&mut self, body: Instance, name: String) {
        self.names.insert(body, name);
    }

    fn base_instance(&self, ws: &Workspace, fr: FuncRef) -> Option<Instance> {
        let canonical = skull_mir::lower::canonical_variant(ws, fr);
        self.graph_instances
            .iter()
            .find(|instance| instance.func == canonical && instance.args.is_empty())
            .cloned()
    }

    fn name_for_decl(&self, ws: &Workspace, fr: FuncRef) -> Option<&String> {
        let instance = self.base_instance(ws, fr)?;
        self.get(&instance)
    }

    fn instance_for_owner(&self, ws: &Workspace, owner: &Instance) -> Option<Instance> {
        let func = skull_mir::lower::canonical_variant(ws, owner.func);
        self.owner_instances
            .iter()
            .find(|candidate| candidate.func == func && candidate.args == owner.args)
            .cloned()
    }

    fn instance_for_call(
        &self,
        ws: &Workspace,
        mf: &MirFunc,
        iid: Idx<Inst>,
        requested: FuncRef,
    ) -> Option<Instance> {
        if let Some(owner) = mf.callee_instances.get(&iid) {
            return self.instance_for_owner(ws, owner);
        }
        if let Some(instance) = self.instance_for_site(ws, mf, iid) {
            return Some(instance);
        }
        ws.module(requested.module).functions[requested.func]
            .generics
            .is_empty()
            .then(|| self.base_instance(ws, requested))
            .flatten()
    }

    fn instance_for_site(&self, ws: &Workspace, mf: &MirFunc, iid: Idx<Inst>) -> Option<Instance> {
        let call_site = mf.call_sites.get(&iid)?;
        let owner = mf.site_owners.get(call_site.owner.0 as usize)?;
        let owner = self.instance_for_owner(ws, owner)?;
        self.site_targets
            .get(&(owner, call_site.site.clone()))
            .cloned()
    }
}

struct Emitter<'a> {
    ws: &'a Workspace,
    mf: &'a MirFunc,
    /// Known co-emitted plain-Rock callees (`Instance` -> emitted JS name).
    callees: &'a CalleeNames,
    /// Names of hoisted closure bodies (`convert_closures`' `<base>__lambda<k>`)
    /// emitted somewhere in this program: the ONLY `Inst::Global` a capture-free
    /// lambda creation site legitimately references. Any other global read (a
    /// module `let`, an enum name table, a named fn as a value) is out of subset.
    hoisted: &'a HashSet<ustr::Ustr>,
    /// Module-global surface: state globals (`__G.<name>`) and fn-value globals
    /// (co-emitted JS name). See [`GlobalCtx`].
    globals: &'a GlobalCtx,
    /// Times each inst is used as an operand (by an inst or terminator), dense
    /// by raw inst id. 0 == statement-position (an effectful `Call`) or dead.
    use_count: Vec<u32>,
    /// Whether each inst folds into its single use site (dense by raw id):
    /// pure, single-use, same-block. A non-inlinable USED value inst is bound to
    /// a `v<raw>` temp at its program position instead -- so a cell read (`Load`)
    /// captures the cell's value THEN, not at the (possibly-later) use, and an
    /// effectful `Call` keeps its order (the defer/increment miscompiles the
    /// all-inline model produced). Mirrors codegen-lua's `compute_inlinable`.
    inlinable: Vec<bool>,
    /// Dominators / immediate post-dominators / natural loops.
    structure: Structure,
    /// Range-`for` headers -> their counted-loop info (the `for (;;)` fast path).
    counted: rustc_hash::FxHashMap<Idx<Block>, CountedLoop>,
    /// Other natural-loop headers -> their region (general `while (true)` path).
    general: rustc_hash::FxHashMap<Idx<Block>, LoopRegion>,
    /// Loop headers currently being emitted, so the header-region walk (a
    /// compound `while a && b` condition) does not re-wrap the header as a fresh
    /// loop.
    active: HashSet<Idx<Block>>,
    visited: HashSet<Idx<Block>>,
    /// The stack of enclosing loops (innermost last), for multi-level `continue`/
    /// `break` to a non-innermost loop (`labeled_jump`).
    loop_stack: Vec<LoopFrame>,
    /// Every loop that emitted a `loop_marker` prefix, in emission order.
    loop_markers: Vec<Idx<Block>>,
    /// Loop headers a `labeled_jump` actually targeted -- these get a real
    /// `L<header>:` label at finalization; the rest have their marker stripped, so
    /// single-level loops stay byte-identical. Discovered DURING emission (the
    /// runtime `loop_stack` nesting), avoiding a fragile static pre-pass.
    labels_used: HashSet<Idx<Block>>,
    /// Set when [`Self::emit_seq`] raises the shared-branch-tail reject (the ONE
    /// reject the per-body trampoline fallback retries -- every other reject rides
    /// `?` out untouched). A typed signal, not a message-string compare.
    shared_tail_reject: bool,
    /// Whether a multi-value `Term::Return` in the trampoline printer packs an
    /// array (the ordinary-body fallback ABI, mirroring the structured
    /// `emit_return`) or rejects. A coroutine step's multi-value return keeps the
    /// reject -- its return channels through the step protocol, not a raw array.
    pack_multi_return: bool,
    /// Materialized `v<N>` temps (raw inst ids) whose use crosses their defining
    /// block: the flat trampoline switch puts each block in a SIBLING case, so a
    /// per-case `let v<N>` would be out of scope at a cross-case use. These are
    /// hoist-declared at the fn top and ASSIGNED in-case; block-local temps keep
    /// their per-case `let`. Empty for the structured path (block nesting keeps a
    /// temp in scope) and the spilled coroutine step (no cross-block temps).
    hoisted_temps: HashSet<u32>,
    /// Value-element NativeBuffer slot refs (raw inst ids) whose EVERY use is a
    /// `Field`/`StoreField` base: those consumers render flat-slot access from
    /// the ref's own operands, so the ref itself emits no statement (its Lua
    /// analogue is a cdata slot local; on JS there is no per-element view to
    /// bind). A ref with any OTHER use materializes as a whole-element COPY.
    out: String,
    indent: usize,
}
