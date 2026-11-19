//! The MIR->JS emitter. Output is structured (goto-free): a `Branch` recovers into `if/else`
//! rejoining at the immediate post-dominator. An unsupported MIR op is a spanned `CompilerError`,
//! never emit-and-hope.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;

use bone_core::workspace::Workspace;
use comfy_la_arena::{Idx, RawIdx};
use skull_mir::code_identity::{CodeArtifact, CodeIdentity};
use skull_mir::decorator::{DecoratorTable, SimValues, decorator_symbol};
use skull_mir::global_storage::StaticBaseline;
use skull_mir::instance::Instance;
use skull_mir::intrinsics::{Emission, IntrinsicTable};
use skull_mir::mangle::{ident_of_key, symbol_of_key};
use skull_mir::mir::{Block, Cell, ContainerKind, FuncRef, Inst, Lit, MirFunc, Term};

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
mod json_types;
mod lanes;
mod overlay;
mod placement;
mod plan_stages;
mod session;
mod structured;
mod tables;
mod worklist;

use analysis::*;
pub use assemble::*;
use coroutine::*;
use delivery::*;
pub use function::*;
pub(crate) use globals::*;
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

const JS_CHAIN: skull_transform::body_chain::ChainOptions<'static> =
    skull_transform::body_chain::ChainOptions {
        target: bone_core::hir::FnBackend::Js,
        reserve: &|_| {},
        counted_loops: skull_transform::body_chain::CountedLoops::RootAndNested,
        inline_hoisted: false,
    };

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

fn convert_body(ws: &Workspace, mf: &mut MirFunc, name: &str) -> Result<(), CompilerError> {
    skull_transform::body_chain::convert_body(ws, mf, name, &JS_CHAIN, &mut ()).map_err(|e| e.error)
}

/// The module-global surface an emitted body may reference: state renders into the scoped `__G`
/// object so no body local can shadow it; a top-level fn used as a value renders as a logical
/// CodeRef slot.
#[derive(Clone, Default)]
pub struct GlobalCtx {
    state: HashMap<ustr::Ustr, String>,
    uses_storage: std::cell::Cell<bool>,
    const_globals: HashSet<ustr::Ustr>,
    fns: HashMap<ustr::Ustr, String>,
    enum_globals: HashSet<ustr::Ustr>,
    code_slots: HashMap<skull_mir::code_identity::CodeIdentity, String>,
}

fn err(msg: impl Into<String>) -> CompilerError {
    CompilerError::new_with_label(Span::default(), msg.into())
}

fn sited_err((msg, site): (String, Option<Span>)) -> CompilerError {
    CompilerError::new_with_label(site.unwrap_or_default(), msg)
}

fn nobox_errs(
    ws: &bone_core::workspace::Workspace,
    mf: &skull_mir::mir::MirFunc,
) -> Result<(), CompilerError> {
    // Name the body: an inlined stdlib span would otherwise make distinct sites read as duplicates.
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

fn nobox_tree_errs(ws: &Workspace, mf: &MirFunc, findings: &mut Vec<CompilerError>) {
    collect(findings, nobox_errs(ws, mf));
    for nf in &mf.nested_fns {
        collect(findings, nobox_errs(ws, nf));
    }
}

fn merge_findings(findings: Vec<CompilerError>) -> Option<CompilerError> {
    let mut it = findings.into_iter();
    let primary = it.next()?;
    Some(primary.with_siblings(it))
}

fn collect<T>(findings: &mut Vec<CompilerError>, r: Result<T, CompilerError>) {
    if let Err(e) = r {
        findings.push(e);
    }
}

/// The accumulated per-definition rejections as one failure, sorted by emitted name; emission
/// accumulates so the `--backend js` skip histogram lists every rejected definition.
fn combine_rejections(mut rejections: Vec<(String, CompilerError)>) -> Result<(), CompilerError> {
    rejections.sort_by(|(a, _), (b, _)| a.cmp(b));
    match merge_findings(rejections.into_iter().map(|(_, e)| e).collect()) {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

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

fn js_number(f: f64) -> Result<String, CompilerError> {
    if f.is_finite() {
        Ok(f.to_string())
    } else {
        Err(err("JS subset: non-finite float literal"))
    }
}

pub fn js_string_lit(s: &str) -> String {
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
    o
}

fn is_js_ident(s: &str) -> bool {
    let mut cs = s.chars();
    match cs.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$' => {}
        _ => return false,
    }
    cs.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

fn js_key(f: &str) -> String {
    if is_js_ident(f) {
        f.to_string()
    } else {
        js_string_lit(f)
    }
}

fn js_member(base: &str, f: &str) -> String {
    if is_js_ident(f) {
        format!("{base}.{f}")
    } else {
        format!("{base}[{}]", js_string_lit(f))
    }
}

/// The JS name every co-emitted callee is called by; a call carrying an `Instance` targets that
/// instantiation's standalone copy.
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
    callees: &'a CalleeNames,
    /// Hoisted closure body names: the only `Inst::Global` a capture-free lambda creation site may
    /// reference.
    hoisted: &'a HashSet<ustr::Ustr>,
    globals: &'a GlobalCtx,
    use_count: Vec<u32>,
    /// Whether each inst folds into its single use site; a non-inlinable used value binds to a
    /// `v<raw>` temp so cell reads and effectful calls keep their order.
    inlinable: Vec<bool>,
    /// Whether a multi-value `Term::Return` in the trampoline printer packs an array (ordinary-body
    /// fallback) or rejects (coroutine step, which returns through the step protocol).
    pack_multi_return: bool,
    /// Value temps (raw inst ids) whose use crosses their defining block: hoist-declared at the fn
    /// top and assigned at their MIR position, so follower scopes and trampoline cases see one value.
    hoisted_temps: HashSet<u32>,
    /// NativeBuffer slot refs used only as `Field`/`StoreField` bases render flat-slot access from
    /// their operands and emit no statement; any other use materializes a whole-element copy.
    out: String,
    indent: usize,
}
