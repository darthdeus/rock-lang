use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::OnceLock;

use bone_core::hir::{
    BinOp, BindingInfo, Body, BodyOwner, BuiltinTy, Callee, Expr, Function, MatchArm, Pattern,
    Statement, TyId, TypeCtor, TypeExpr, VariantArgs, VariantPayload,
};
use bone_core::hir_walk::HirChild;
use bone_core::prelude::{BoundNext, ForIterInfo, ModuleId, Span};
use bone_core::workspace::{BodyInference, Workspace};
use comfy_la_arena::{Idx, IdxRange, RawIdx};
use skull_parser::CompilerError;
use ustr::Ustr;

use crate::code_identity::{CodeIdentity, CodeOwner, CodeOwnerKind};
use crate::concrete::{ConcreteBodyPlan, ConcreteOwner, ConcretePlans};
use crate::intrinsics::ExpandKind;
use crate::mir::*;
use crate::monoty::{MonoCtor, MonoTy};

#[derive(Clone)]
pub(crate) enum CallParamDefault {
    Scalar(Lit),
    Vector {
        shape: ValueShape,
        literals: Vec<Lit>,
    },
}

fn gap(msg: impl std::fmt::Display) -> CompilerError {
    CompilerError::new_no_location(format!("skull-mir: lowering gap: {msg}"))
}

pub(crate) fn required_wire_slot<T>(
    slots: &[T],
    index: usize,
    variadic: bool,
    context: impl std::fmt::Display,
    missing: impl FnOnce(String) -> CompilerError,
) -> Result<&T, CompilerError> {
    slots
        .get(index)
        .or_else(|| variadic.then(|| slots.last()).flatten())
        .ok_or_else(|| missing(format!("{context} has no ABI slot {index}")))
}

/// An `Any` (portable Rock) body serves every target; otherwise the backend must
/// match exactly.
fn serves_target(b: bone_core::hir::FnBackend, target: bone_core::hir::FnBackend) -> bool {
    use bone_core::hir::FnBackend;
    b == FnBackend::Any || b == target
}

mod parameter_demand;

/// What a direct call's parameter transport derives from the callee's body,
/// as a key: a body edit that keeps the key keeps every caller's call text.
pub fn parameter_demand_key(ws: &Workspace, func: FuncRef) -> u64 {
    parameter_demand::key(&parameter_demand::of_function(ws, func))
}

mod shape;
mod sort_key;
pub use shape::*;

/// The scalar-ABI policy is private to this module, so a definition and its
/// calls cannot reach it from two places.
mod abi;
pub use abi::{
    AbiQuestion, BackendFnAbiPlan, BackendFnAbiPlanHandle, BoundaryClass, FunctionRepPlan,
    GlobalPlan, RepArena, RepError, RepGuarantees, RepId, RepLayout, RepPlan, RepUse, RepUseFacts,
    SIGNATURE_SCALAR_LANE_BUDGET, TupleLayout, WIDE_VALUE_PAYLOAD_LEAF_CUT,
    boundary_requires_boxed_representation, declared_boundary_class, owner_abi_plan,
    owner_abi_plan_for_body, requires_concrete_body_plan, selected_sibling_abi_plan,
};

mod struct_plan;
pub use struct_plan::{
    CDATA_LEAF_PREFIX, GcTableField, StructFieldPlan, StructRegistryPlan, cdata_leaf_alias,
    gc_table_fields, nobox_leaf_paths, record_keys, struct_field_plan,
};

mod coroutine;
pub use coroutine::CoroutinePlan;

mod call;
mod container;
pub use container::{ContainerFamily, ContainerPlan, ContainerStorage};
mod control;
mod dispatch;
pub use dispatch::{DispatchCtx, EqRoute, FormatPlan, HolePlan, StringifyRoute, StructuralRender};
mod embed;
mod expr;
mod flat;
mod flat_ref;
use flat_ref::{
    FlatArgCapture, FlatCallOut, FlatRefKey, FlatStorage, FlatVariants, FlatVariantsHandle,
};
mod format;
mod interp;
mod kinds;
mod loops;
mod nobox;
mod pattern;
mod resolve;
pub use resolve::{NoServingBinding, backend_variant, backend_variant_instance, canonical_variant};
mod stmt;
mod storage;
pub use storage::SYNTHESIZED_ENTRIES;
mod structs;
mod types;

fn prefixed(field: Ustr, leaves: Vec<(Ustr, Idx<Inst>)>) -> Vec<(Ustr, Idx<Inst>)> {
    leaves
        .into_iter()
        .map(|(k, v)| (crate::names::ustr(&format!("{field}.{k}")), v))
        .collect()
}

/// Flat enum slots use dotted keys on the parent: `sig.#` holds the variant
/// name, `sig.0` and `shape.r` the payloads.
fn flat_tag_slot(field: Ustr) -> Ustr {
    crate::names::concat(&[field.as_str(), ".#"])
}

fn flat_payload_slot(field: Ustr, i: usize) -> Ustr {
    crate::names::indexed(field.as_str(), ".", i)
}

fn flat_named_slot(field: Ustr, rec_field: Ustr) -> Ustr {
    crate::names::concat(&[field.as_str(), ".", rec_field.as_str()])
}

/// A flat enum's leaves: `#` for the tag, the position for a tuple slot and
/// the name for a record slot.
fn flat_leaf_names(width: usize, fields: &[Ustr]) -> impl Iterator<Item = Ustr> + '_ {
    std::iter::once(crate::names::ustr("#"))
        .chain((0..width).map(|i| crate::names::indexed("", "", i)))
        .chain(fields.iter().copied())
}

fn flat_keys(prefix: Ustr, width: usize, fields: &[Ustr]) -> impl Iterator<Item = Ustr> + '_ {
    flat_leaf_names(width, fields)
        .map(move |leaf| crate::names::concat(&[prefix.as_str(), ".", leaf.as_str()]))
}

fn flat_slot_names(field: Ustr, shape: &EnumFlatShape) -> Vec<Ustr> {
    flat_keys(field, shape.tuple_width(), shape.record_fields()).collect()
}

#[derive(Clone)]
struct FlatCells {
    slots: Vec<Idx<Inst>>,
    fields: std::sync::Arc<[Ustr]>,
}

impl FlatCells {
    fn from_slots(shape: &EnumFlatShape, slots: &[Idx<Inst>]) -> Self {
        Self {
            slots: slots[..shape.leaf_count()].to_vec(),
            fields: shape.record_fields.clone(),
        }
    }

    fn tag(&self) -> Idx<Inst> {
        self.slots[0]
    }

    fn positional(&self) -> &[Idx<Inst>] {
        &self.slots[1..self.slots.len() - self.fields.len()]
    }

    fn named(&self) -> impl Iterator<Item = (Ustr, Idx<Inst>)> + '_ {
        self.fields.iter().copied().zip(
            self.slots[self.slots.len() - self.fields.len()..]
                .iter()
                .copied(),
        )
    }

    fn slots(&self) -> impl Iterator<Item = Idx<Inst>> + '_ {
        self.slots.iter().copied()
    }
}

/// Lower in place to preserve evaluation order.
enum FlatPayload {
    Expr(Idx<Expr>),
    Repr(ValueRepr),
}

enum Scrut {
    Val(Idx<Inst>),
    Tuple(Vec<Scrut>),
    Leaves {
        leaves: Vec<Idx<Inst>>,
        shape: ValueShape,
    },
    Repr {
        repr: ValueRepr,
        boxed: Idx<Inst>,
    },
    Flat {
        parent: Idx<Inst>,
        prefix: Ustr,
        shape: EnumFlatShape,
    },
    Cells {
        cells: FlatCells,
        shape: EnumFlatShape,
    },
}

#[derive(Clone, Default)]
struct ValueReferences {
    taken_by_ref: HashSet<Ustr>,
    per_body: HashMap<(ModuleId, BodyOwner), HashSet<Ustr>>,
    /// Only a 0<->1 flip changes scalar-ABI eligibility, so `update_body` reports
    /// exactly those.
    ref_counts: HashMap<Ustr, u32>,
}

/// Workspace-wide scalar-ABI context. Address-taken functions use the
/// type-derived fn-value ABI at definitions and direct calls, so all three call
/// forms agree.
pub struct ScalarCtx {
    global_read_checks: OnceLock<HashMap<Ustr, crate::global_storage::GlobalReadCheck>>,
    value_references: std::sync::Arc<ValueReferences>,
    values_first_class: bool,
    /// Drives backend-variant selection at every resolved call; `Any` disables it
    /// (GLSL and tests).
    target: bone_core::hir::FnBackend,
    interner: std::sync::Arc<crate::monoty::MonoInterner>,
    rep_arena: std::sync::Arc<abi::RepArena>,
    /// Valid only under this context's target, value mode and taken-by-ref state;
    /// `update_body` clears it.
    abi_plans: abi::AbiPlanCache,
}

impl Clone for ScalarCtx {
    fn clone(&self) -> Self {
        Self {
            global_read_checks: OnceLock::new(),
            value_references: std::sync::Arc::clone(&self.value_references),
            values_first_class: self.values_first_class,
            target: self.target,
            interner: std::sync::Arc::clone(&self.interner),
            rep_arena: std::sync::Arc::clone(&self.rep_arena),
            abi_plans: self.abi_plans.clone(),
        }
    }
}

fn scan_body_value_refs(ws: &Workspace, body: &Body) -> HashSet<Ustr> {
    let mut set = HashSet::default();
    for (_, expr) in body.expr_arena.iter() {
        if let Expr::Global(name) = expr {
            if ws.functions_by_name.contains_key(name) {
                set.insert(*name);
            }
        }
    }
    set
}

impl ScalarCtx {
    pub fn new(ws: &Workspace) -> Self {
        Self::new_with_value_mode(ws, false)
    }

    fn new_with_value_mode(ws: &Workspace, values_first_class: bool) -> Self {
        let value_references = if values_first_class {
            ValueReferences::default()
        } else {
            let mut value_references = ValueReferences::default();
            for module in ws.modules.iter() {
                for (owner, body) in module.bodies.iter() {
                    let set = scan_body_value_refs(ws, body);
                    for name in &set {
                        *value_references.ref_counts.entry(*name).or_insert(0) += 1;
                        value_references.taken_by_ref.insert(*name);
                    }
                    if !set.is_empty() {
                        value_references.per_body.insert((module.id, *owner), set);
                    }
                }
                // `@sim` registration takes each sim as a fn value outside any body scan, so
                // it holds one permanent count no body edit can release.
                for (_, f) in module.functions.iter() {
                    if f.annotations.iter().any(|a| a.is_builtin("sim")) {
                        *value_references.ref_counts.entry(f.name).or_insert(0) += 1;
                        value_references.taken_by_ref.insert(f.name);
                    }
                }
            }
            value_references
        };
        let interner = std::sync::Arc::new(crate::monoty::MonoInterner::new());
        Self {
            global_read_checks: OnceLock::new(),
            value_references: std::sync::Arc::new(value_references),
            values_first_class,
            target: if values_first_class {
                bone_core::hir::FnBackend::Any
            } else {
                bone_core::hir::FnBackend::Lua
            },
            rep_arena: std::sync::Arc::new(abi::RepArena::new(std::sync::Arc::clone(&interner))),
            interner,
            abi_plans: abi::AbiPlanCache::default(),
        }
    }

    pub(crate) fn global_read_checks(
        &self,
        ws: &Workspace,
    ) -> &HashMap<Ustr, crate::global_storage::GlobalReadCheck> {
        self.global_read_checks
            .get_or_init(|| crate::global_storage::global_read_checks(ws, self))
    }

    /// Every `Instance` is minted against this arena.
    pub fn interner(&self) -> &crate::monoty::MonoInterner {
        &self.interner
    }

    pub fn sharing(mut self, interner: std::sync::Arc<crate::monoty::MonoInterner>) -> Self {
        // Representation facts use these arena IDs; nothing may be planned yet.
        assert!(
            self.rep_arena.is_empty(),
            "a context must share its type arena before planning any representation"
        );
        self.rep_arena = std::sync::Arc::new(abi::RepArena::new(std::sync::Arc::clone(&interner)));
        self.interner = interner;
        self
    }

    /// Later consumers must mint against the same arena.
    pub fn interner_handle(&self) -> std::sync::Arc<crate::monoty::MonoInterner> {
        std::sync::Arc::clone(&self.interner)
    }

    pub fn rep_arena(&self) -> &std::sync::Arc<abi::RepArena> {
        &self.rep_arena
    }

    pub(crate) fn abi_plan_cached(
        &self,
        question: AbiQuestion,
        inst: &crate::instance::Instance,
    ) -> Option<BackendFnAbiPlanHandle> {
        self.abi_plans.lookup(question, inst)
    }

    pub(crate) fn abi_plan_store(
        &self,
        question: AbiQuestion,
        inst: &crate::instance::Instance,
        plan: &BackendFnAbiPlanHandle,
    ) {
        self.abi_plans.store(question, inst, plan);
    }

    pub fn target(&self) -> bone_core::hir::FnBackend {
        self.target
    }

    pub fn with_target(mut self, target: bone_core::hir::FnBackend) -> Self {
        self.target = target;
        self
    }

    /// GLSL `vecN` values stay first-class, not scalar leaves.
    pub fn new_values_first_class(ws: &Workspace) -> Self {
        Self::new_with_value_mode(ws, true)
    }

    /// Return names whose function-value uses change ABI eligibility after an edit.
    pub fn update_body(&mut self, ws: &Workspace, mid: ModuleId, owner: BodyOwner) -> Vec<Ustr> {
        // An edit can flip taken-by-ref membership, which the memoized plans read.
        self.abi_plans.clear();
        self.global_read_checks.take();
        if self.values_first_class {
            return Vec::new();
        }

        let refs = std::sync::Arc::make_mut(&mut self.value_references);
        let key = (mid, owner);
        let old = refs.per_body.remove(&key).unwrap_or_default();
        let new = match ws.module(mid).bodies.get(&owner) {
            Some(body) => scan_body_value_refs(ws, body),
            None => HashSet::default(),
        };
        let mut flipped = Vec::new();
        for name in old.difference(&new) {
            if let Some(c) = refs.ref_counts.get_mut(name) {
                *c -= 1;
                if *c == 0 {
                    refs.ref_counts.remove(name);
                    refs.taken_by_ref.remove(name);
                    flipped.push(*name);
                }
            }
        }
        for name in new.difference(&old) {
            let c = refs.ref_counts.entry(*name).or_insert(0);
            *c += 1;
            if *c == 1 {
                refs.taken_by_ref.insert(*name);
                flipped.push(*name);
            }
        }
        if !new.is_empty() {
            refs.per_body.insert(key, new);
        }
        flipped
    }
}

#[derive(Clone)]
struct NestedFn {
    hoisted: Ustr,
    /// Lets a call site place named args into slots as for a top-level callee.
    param_names: Vec<Ustr>,
    representation: FunctionRepPlan,
}

impl NestedFn {
    fn uses_scalar_abi(&self) -> bool {
        self.representation.uses_scalar_abi()
    }

    pub(crate) fn param_slots<'arena>(&self, arena: &'arena RepArena) -> Vec<&'arena RepLayout> {
        self.representation
            .params()
            .iter()
            .map(|&id| arena.layout(id))
            .collect()
    }

    pub(crate) fn result_shape<'a>(&self, arena: &'a RepArena) -> Option<&'a ValueShape> {
        self.representation
            .result()
            .and_then(|id| arena.value_shape(id))
    }
}

pub fn scalar_abi_param_collisions(
    ws: &Workspace,
    scx: &ScalarCtx,
    fref: FuncRef,
) -> Vec<(String, CompilerError)> {
    let f = &ws.module(fref.module).functions[fref.func];
    if f.inline_body.is_none() {
        return Vec::new();
    }
    let plan = BackendFnAbiPlan::for_boundary_declaration(ws, scx, fref);
    let (Some(names), Some(origins)) =
        (plan.lua_param_names(ws), plan.scalar_param_labels(ws, true))
    else {
        return Vec::new();
    };
    debug_assert_eq!(names.len(), origins.len(), "one derivation, two spellings");

    let mut out = Vec::new();
    let mut seen: HashMap<&str, &str> = HashMap::default();
    let mut reported: HashSet<&str> = HashSet::default();
    for (name, origin) in names.iter().zip(origins.iter()) {
        match seen.get(name.as_str()) {
            Some(prev) if !reported.contains(name.as_str()) => {
                reported.insert(name.as_str());
                let fname = f.name;
                let msg = format!(
                    "`{fname}` would emit the Lua parameter `{name}` twice: it is both {prev} and \
                     {origin}. The scalar ABI joins nested value fields with `__`, which is legal \
                     in an identifier too, so the two are indistinguishable and the body would \
                     read whichever Lua bound last -- rename one of them"
                );
                out.push((
                    format!("{fname}.{name}"),
                    CompilerError::new_with_label(f.name_span, msg),
                ));
            }
            Some(_) => {}
            None => {
                seen.insert(name.as_str(), origin.as_str());
            }
        }
    }
    out
}

enum FmtPiece {
    Lit(String),
    DynInt(Idx<Expr>),
}

fn fmt_pieces(
    spec: &bone_core::hir::InterpSpec,
    inferred_ty: Option<char>,
    skip_width: bool,
    skip_pad_attrs: bool,
) -> Option<Vec<FmtPiece>> {
    if spec.fill.is_some() && !skip_pad_attrs {
        return None;
    }
    let ty = spec.format_type.or(inferred_ty)?;

    let mut pieces = Vec::new();
    let mut buf = String::from("%");
    if !skip_pad_attrs {
        match spec.alignment {
            None | Some('>') => {}      // right-align is printf's default
            Some('<') => buf.push('-'), // left-align
            _ => return None,           // center etc. — no printf equivalent
        }
    }
    match spec.sign {
        Some('+') => buf.push('+'),
        Some(' ') => buf.push(' '),
        _ => {}
    }
    if spec.alternate {
        buf.push('#');
    }
    // A dynamic width cannot bake `0` into the literal, so it space-pads.
    if spec.zero_pad && spec.width_expr.is_none() {
        buf.push('0');
    }
    if skip_width {
    } else if let Some(w) = spec.width {
        buf.push_str(&w.to_string());
    } else if let Some(we) = spec.width_expr {
        pieces.push(FmtPiece::Lit(std::mem::take(&mut buf)));
        pieces.push(FmtPiece::DynInt(we));
    }
    if spec.precision.is_some() || spec.precision_expr.is_some() {
        buf.push('.');
        if let Some(p) = spec.precision {
            buf.push_str(&p.to_string());
        } else if let Some(pe) = spec.precision_expr {
            pieces.push(FmtPiece::Lit(std::mem::take(&mut buf)));
            pieces.push(FmtPiece::DynInt(pe));
        }
    }
    buf.push(ty);
    pieces.push(FmtPiece::Lit(buf));
    Some(pieces)
}

fn expr_tag(e: &Expr) -> &'static str {
    match e {
        Expr::As { .. } => "As",
        Expr::IncDec { .. } => "IncDec",
        Expr::InterpolatedString(_) => "InterpolatedString",
        Expr::Lambda { .. } => "Lambda",
        Expr::Range { .. } => "Range",
        Expr::Return(_) => "Return",
        Expr::SafeNav { .. } => "SafeNav",
        Expr::SafeNavCall { .. } => "SafeNavCall",
        Expr::SafeNavLet { .. } => "SafeNavLet",
        Expr::SiblingRef { .. } => "SiblingRef",
        Expr::Spread(_) => "Spread",
        Expr::Yield(_) => "Yield",
        Expr::YieldDelegate(_) => "YieldDelegate",
        _ => "other",
    }
}

fn raw(b: Idx<BindingInfo>) -> u32 {
    b.into_raw().into()
}

fn is_native_buffer(name: Ustr) -> bool {
    bone_core::modules::is_intrinsic_key_in(
        name,
        &[
            "NativeBuffer",
            "NativeBufferU8",
            "NativeBufferI8",
            "NativeBufferU16",
            "NativeBufferI16",
            "NativeBufferU32",
            "NativeBufferI32",
        ],
    )
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ArithTy {
    Int,
    Uint,
    Float,
    Str,
}

fn arith_inst(
    op: &str,
    lt: ArithTy,
    rt: ArithTy,
    lv: Idx<Inst>,
    rv: Idx<Inst>,
) -> Result<Inst, CompilerError> {
    let float = lt == ArithTy::Float || rt == ArithTy::Float;
    Ok(match op {
        "__op_add" if lt == ArithTy::Str || rt == ArithTy::Str => Inst::StringConcat(lv, rv),
        "__op_add" if float => Inst::FloatAdd(lv, rv),
        "__op_add" if lt == ArithTy::Uint && rt == ArithTy::Uint => Inst::UintAdd(lv, rv),
        "__op_add" => Inst::IntAdd(lv, rv),
        "__op_sub" if float => Inst::FloatSub(lv, rv),
        "__op_sub" if lt == ArithTy::Uint && rt == ArithTy::Uint => Inst::UintSub(lv, rv),
        "__op_sub" => Inst::IntSub(lv, rv),
        "__op_mul" if float => Inst::FloatMul(lv, rv),
        "__op_mul" if lt == ArithTy::Uint && rt == ArithTy::Uint => Inst::UintMul(lv, rv),
        "__op_mul" => Inst::IntMul(lv, rv),
        // Lua `/` is always float division; integer floor division is `__op_idiv`.
        "__op_div" => Inst::FloatDiv(lv, rv),
        "__op_idiv" if lt == ArithTy::Uint && rt == ArithTy::Uint => Inst::UintDiv(lv, rv),
        "__op_idiv" => Inst::IntDiv(lv, rv),
        "__op_mod" if float => Inst::FloatMod(lv, rv),
        "__op_mod" if lt == ArithTy::Uint && rt == ArithTy::Uint => Inst::UintMod(lv, rv),
        "__op_mod" => Inst::IntMod(lv, rv),
        _ => return Err(gap(format!("native arith op {op}"))),
    })
}

/// Concrete symbols keep generic instances' cells distinct.
fn static_owner_tag(
    ws: &Workspace,
    scx: &ScalarCtx,
    inf: Option<&BodyInference>,
    inst: &crate::instance::Instance,
) -> Ustr {
    use std::hash::{Hash, Hasher};
    let func = &ws.module(inst.func.module).functions[inst.func.func];
    let name = if inst.args.is_empty() {
        Ustr::from(bone_core::modules::artifact_name(func.name.as_str()))
    } else {
        crate::names::ustr(&inst.symbol(ws, scx.interner()))
    };
    let mut h = std::collections::hash_map::DefaultHasher::new();
    name.as_str().hash(&mut h);
    for pd in &func.param_decls {
        ws.render(inf, pd.ty).hash(&mut h);
    }
    crate::names::ustr(&format!("{}_{:06x}", name, h.finish() & 0xff_ffff))
}

/// Visits child contents to keep their arena indices out of the reload key.
fn hash_init_expr(body: &Body, e: Idx<Expr>, h: &mut std::collections::hash_map::DefaultHasher) {
    hash_init_node(body, HirChild::Expr(e), h);
}

fn hash_init_node(body: &Body, node: HirChild, h: &mut std::collections::hash_map::DefaultHasher) {
    use bone_core::hir::InterpPart;
    use std::hash::Hash;
    macro_rules! tag {
        ($n:expr) => {
            ($n as u16).hash(h)
        };
    }

    match node {
        HirChild::Expr(e) => match body.expr_arena[e] {
            Expr::Unit => tag!(0),
            Expr::Null => tag!(1),
            Expr::Error => tag!(12),
            Expr::Hole(n) => {
                tag!(2);
                n.hash(h);
            }
            Expr::Int(v) => {
                tag!(3);
                v.hash(h);
            }
            Expr::Float(v) => {
                tag!(4);
                v.hash(h);
            }
            Expr::Bool(v) => {
                tag!(5);
                v.hash(h);
            }
            Expr::String(s) => {
                tag!(6);
                s.hash(h);
            }
            Expr::Var(b) => {
                tag!(7);
                body.binding_arena[b].name.hash(h);
            }
            Expr::Global(n) => {
                tag!(8);
                Ustr::from(bone_core::modules::artifact_name(n.as_str())).hash(h);
            }
            Expr::SiblingRef(n) => {
                tag!(9);
                n.hash(h);
            }
            Expr::Tuple(_) | Expr::Array(_) => tag!(10),
            Expr::InterpolatedString(parts) => {
                tag!(13);
                for p in parts {
                    match body.interp_part_arena[p] {
                        InterpPart::Literal(s) => {
                            tag!(130);
                            s.hash(h);
                        }
                        InterpPart::Value { expr, spec } => {
                            tag!(131);
                            hash_init_expr(body, expr, h);
                            if let Some(spec) = spec {
                                tag!(132);
                                hash_interp_spec(body, &spec, h);
                            }
                        }
                    }
                }
                return;
            }
            Expr::Not(value, _) | Expr::UnaryMinus(value, _) => {
                let numeric_literal =
                    matches!(body.expr_arena[value], Expr::Int(_) | Expr::Float(_));
                let negation = matches!(body.expr_arena[e], Expr::UnaryMinus(..));
                // The operand tag distinguishes the two legacy encodings.
                tag!(match (negation, numeric_literal) {
                    (true, true) | (false, false) => 14,
                    (true, false) => 15,
                    (false, true) => 16,
                });
            }
            Expr::BitNot(_) => tag!(17),
            Expr::Spread(_) => tag!(18),
            Expr::YieldDelegate(_) => tag!(26),
            Expr::Return(_) => tag!(19),
            Expr::Yield(_) => tag!(20),
            Expr::Break => tag!(21),
            Expr::Continue => tag!(22),
            Expr::As { target, kind, .. } => {
                tag!(23);
                target.hash(h);
                kind.hash(h);
            }
            Expr::TupleIndex { index, .. } => {
                tag!(24);
                index.hash(h);
            }
            Expr::FieldAccess { field, .. } | Expr::SafeNav { field, .. } => {
                tag!(25);
                field.hash(h);
            }
            Expr::IncDec {
                decrement, postfix, ..
            } => {
                tag!(27);
                decrement.hash(h);
                postfix.hash(h);
            }
            Expr::Index { .. } => tag!(28),
            Expr::Range { .. } => tag!(29),
            Expr::BinOp { op, .. } => {
                tag!(30);
                std::mem::discriminant(&op).hash(h);
            }
            Expr::Assign { .. } => tag!(31),
            Expr::CompoundAssign { op_fn, .. } => {
                tag!(32);
                op_fn.hash(h);
            }
            Expr::If { .. } => tag!(33),
            Expr::IfLet { .. } => tag!(34),
            Expr::Lambda { .. } => tag!(35),
            Expr::Block { .. } => tag!(36),
            Expr::Match { .. } => tag!(37),
            Expr::Call { callee, .. } => {
                tag!(38);
                match callee {
                    Callee::Unresolved { name, .. } => {
                        tag!(380);
                        Ustr::from(bone_core::modules::artifact_name(name.as_str())).hash(h);
                    }
                    Callee::Local(b) => {
                        tag!(381);
                        body.binding_arena[b].name.hash(h);
                    }
                    Callee::Value(_) => tag!(382),
                }
            }
            Expr::SafeNavCall { method, .. } => {
                tag!(39);
                method.hash(h);
            }
            Expr::SafeNavLet { .. } => tag!(40),
            Expr::StructLiteral {
                name,
                fields,
                bases,
            } => {
                tag!(41);
                Ustr::from(bone_core::modules::artifact_name(name.as_str())).hash(h);
                for base in bases {
                    hash_init_expr(body, base, h);
                }
                hash_init_fields(body, fields, h);
                return;
            }
            Expr::EnumVariant {
                enum_name,
                variant,
                args,
                ..
            } => {
                tag!(42);
                Ustr::from(bone_core::modules::artifact_name(enum_name.as_str())).hash(h);
                variant.hash(h);
                match args {
                    VariantArgs::Unit => tag!(420),
                    VariantArgs::Tuple(_) => tag!(421),
                    VariantArgs::Record(fields) => {
                        tag!(422);
                        hash_init_fields(body, fields, h);
                        return;
                    }
                }
            }
        },
        HirChild::Statement(s) => match body.statement_arena[s] {
            Statement::Let {
                pattern,
                value,
                else_block,
            } => {
                hash_init_expr(body, value, h);
                hash_init_node(body, HirChild::Pattern(pattern), h);
                else_block.is_some().hash(h);
                if let Some(e) = else_block {
                    hash_init_expr(body, e, h);
                }
                return;
            }
            Statement::Return(None)
            | Statement::Break
            | Statement::Continue
            | Statement::Comment(_) => b'x'.hash(h),
            Statement::Expr(_)
            | Statement::Defer(_)
            | Statement::Return(Some(_))
            | Statement::Static { .. }
            | Statement::Fn { .. }
            | Statement::For { .. }
            | Statement::While { .. } => {}
        },
        HirChild::Pattern(p) => match body.pattern_arena[p] {
            Pattern::Wildcard => tag!(50),
            Pattern::Bind(b) => {
                tag!(51);
                body.binding_arena[b].name.hash(h);
            }
            Pattern::Literal(_) => tag!(52),
            Pattern::Range {
                start,
                end,
                inclusive,
            } => {
                tag!(53);
                inclusive.hash(h);
                for e in [start, end] {
                    e.is_some().hash(h);
                    if let Some(e) = e {
                        hash_init_expr(body, e, h);
                    }
                }
                return;
            }
            Pattern::EnumVariant {
                enum_name, variant, ..
            } => {
                tag!(54);
                (
                    enum_name
                        .map(|name| Ustr::from(bone_core::modules::artifact_name(name.as_str()))),
                    variant,
                )
                    .hash(h);
            }
            Pattern::Record {
                enum_name,
                name,
                fields,
                rest,
                ..
            } => {
                tag!(55);
                (
                    enum_name
                        .map(|name| Ustr::from(bone_core::modules::artifact_name(name.as_str()))),
                    name.map(|name| Ustr::from(bone_core::modules::artifact_name(name.as_str()))),
                    rest,
                )
                    .hash(h);
                for f in fields {
                    let f = body.record_pat_field_arena[f];
                    f.name.hash(h);
                    hash_init_node(body, HirChild::Pattern(f.pat), h);
                }
                return;
            }
            Pattern::Tuple { .. } => tag!(56),
            Pattern::Or { .. } => tag!(57),
            Pattern::Newtype { name, .. } => {
                tag!(58);
                name.hash(h);
            }
        },
    }
    body.for_each_child(node, |child| hash_init_node(body, child, h));
}

fn hash_init_fields(
    body: &Body,
    fields: IdxRange<bone_core::hir::StructLitField>,
    h: &mut std::collections::hash_map::DefaultHasher,
) {
    use std::hash::Hash;
    for field in fields {
        let field = body.struct_lit_field_arena[field];
        field.name.hash(h);
        hash_init_expr(body, field.value, h);
    }
}

fn hash_interp_spec(
    body: &Body,
    spec: &bone_core::hir::InterpSpec,
    h: &mut std::collections::hash_map::DefaultHasher,
) {
    use std::hash::Hash;
    let bone_core::hir::InterpSpec {
        precision,
        precision_expr,
        width,
        width_expr,
        alignment,
        fill,
        sign,
        alternate,
        zero_pad,
        grouping,
        format_type,
    } = *spec;
    (precision, width, alignment, fill, sign).hash(h);
    (alternate, zero_pad, grouping, format_type).hash(h);
    for e in [width_expr, precision_expr] {
        e.is_some().hash(h);
        if let Some(e) = e {
            hash_init_expr(body, e, h);
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum BindingPattern {
    Leaf(Idx<BindingInfo>),
    Tuple(Vec<BindingPattern>),
}

impl BindingPattern {
    pub(crate) fn leaves(&self) -> Vec<Idx<BindingInfo>> {
        let mut out = Vec::new();

        fn go(p: &BindingPattern, out: &mut Vec<Idx<BindingInfo>>) {
            match p {
                BindingPattern::Leaf(b) => out.push(*b),
                BindingPattern::Tuple(subs) => subs.iter().for_each(|s| go(s, out)),
            }
        }

        go(self, &mut out);
        out
    }
}

pub(crate) fn binding_view(body: &Body, p: Idx<Pattern>) -> Option<BindingPattern> {
    match body.pattern_arena[p] {
        Pattern::Bind(b) => Some(BindingPattern::Leaf(b)),
        // A tuple inside the newtype needs the wrapped type's layout: a splatting
        // tuple's box is flat.
        Pattern::Newtype { sub, .. } => match body.pattern_arena[sub] {
            Pattern::Bind(b) => Some(BindingPattern::Leaf(b)),
            _ => None,
        },
        Pattern::Tuple { elements } => elements
            .into_iter()
            .map(|e| binding_view(body, e))
            .collect::<Option<Vec<_>>>()
            .map(BindingPattern::Tuple),
        _ => None,
    }
}

/// Lower one concrete body plan, resolving every callee ABI from the same
/// graph-owned plan set.
pub fn lower_instance_with_plans(
    ws: &Workspace,
    scx: &ScalarCtx,
    plan: &ConcreteBodyPlan,
    concrete_plans: &ConcretePlans,
) -> Result<MirFunc, CompilerError> {
    let inst = plan
        .function_instance()
        .ok_or_else(|| gap("concrete plan does not own a function instance"))?;
    let (mid, fid) = (inst.func.module, inst.func.func);
    let module = ws.module(mid);
    let func = &module.functions[fid];
    let at_decl = |error: CompilerError| error.or_at(func.name_span);
    let owner = BodyOwner::Function(fid);
    validate_concrete_bound_plan(func, plan)?;
    let body = module
        .bodies
        .get(&owner)
        .ok_or_else(|| gap("concrete plan function has no body"))?;
    let inf = ws.inference(mid).and_then(|ir| ir.per_fn.get(&owner));
    // The plan audits only fail on a compiler bug, so they run with the debug checks.
    if crate::verify::checks_enabled() {
        if inf.is_some() {
            plan.types
                .check_source(scx.interner(), (mid, owner), body, "concrete plan")
                .map_err(gap)?;
        }
        validate_plan_domains(plan, "concrete plan")?;
        validate_concrete_callee_plans(ws, scx, plan, concrete_plans).map_err(at_decl)?;
    }
    if func.inline_body.is_some() {
        return Err(at_decl(gap("inline(lua) body")));
    }
    let _shape_memo = shape::ShapeMemoScope::enter(scx.rep_arena.generation());

    let mut f = MirFunc::new(func.name);
    f.rep_arena = Some(scx.rep_arena().clone());
    let resume_arg = func.is_coroutine.then(|| {
        let cell = f.new_cell(Some(crate::names::ustr("__resume_arg")));
        f.cells[cell].capture_identity = Some(crate::code_identity::CaptureIdentity::Generated {
            role: crate::names::ustr("coroutine-resume-arg"),
        });
        cell
    });
    f.is_coroutine = func.is_coroutine;
    f.resume_arg = resume_arg;
    let abi = owner_abi_plan_for_body(ws, scx, concrete_plans, inst, plan).map_err(at_decl)?;
    f.abi_plan = Some(abi.clone());
    f.code_identity = Some(crate::code_identity::CodeIdentity::Named(inst.clone()));
    f.code_abi_fingerprint = Some(abi.logical_fingerprint());
    let mut b = Builder::new(
        ws,
        scx,
        plan,
        concrete_plans,
        body,
        inf,
        f,
        FlatVariants::for_root(inst.clone()),
    );
    b.generic_bounds = &func.generic_bounds;
    b.defer_mode = stmt::DeferMode::for_body(func.is_coroutine, scx.target);
    b.ret_rep = abi.representation().result();
    b.param_plan = Some(abi);
    b.fn_name_span = Some(func.name_span);
    b.site_owner = Some(inst.clone());
    // Only statics and hoisted functions need the overload's owner name.
    if body
        .statement_arena
        .iter()
        .any(|(_, s)| matches!(s, Statement::Static { .. } | Statement::Fn { .. }))
    {
        b.static_owner = static_owner_tag(ws, scx, inf, inst);
    }
    b.flat_ret = b.own_flat_return(inst).map_err(at_decl)?;
    b.lower_params().map_err(at_decl)?;
    b.lower_root().map_err(|e| at_decl(b.located(e)))?;
    let variants = b.flat_variants.borrow_mut().take_funcs();
    b.f.nested_fns.extend(variants);
    Ok(b.f)
}

/// Reject a specialized bounded body whose witness tree was lost before any
/// lowering consumer can interpret its placeholder calls.
pub fn validate_concrete_bound_plan(
    func: &bone_core::hir::Function,
    plan: &ConcreteBodyPlan,
) -> Result<(), CompilerError> {
    if func.has_iterator_bounds() && plan.bound_tree.is_empty() {
        return Err(CompilerError::new_with_label(
            func.name_span,
            "concrete callee bound plan is empty for its site".to_string(),
        ));
    }
    Ok(())
}

pub fn lower_definition_factory(
    ws: &Workspace,
    scx: &ScalarCtx,
    concrete_plans: &ConcretePlans,
    factory: &crate::instance::Instance,
) -> Result<MirFunc, CompilerError> {
    let concrete_plan = concrete_plans
        .get(factory)
        .ok_or_else(|| gap("definition factory has no concrete body plan"))?;
    let module = ws.module(factory.func.module);
    let body = module
        .bodies
        .get(&BodyOwner::Function(factory.func.func))
        .ok_or_else(|| gap("definition factory has no body"))?;
    let abi = owner_abi_plan(ws, scx, concrete_plans, factory)?;
    let mut f = MirFunc::new(crate::names::ustr("__rock_definition_materialize"));
    f.rep_arena = Some(scx.rep_arena().clone());
    let parameter = f.new_cell(Some(crate::names::ustr("factory")));
    f.params.push(parameter);
    let mut b = Builder::new(
        ws,
        scx,
        concrete_plan,
        concrete_plans,
        body,
        None,
        f,
        FlatVariantsHandle::default(),
    );
    let callee = b.emit_cur(Inst::Load(parameter));
    let result = b.indirect_call_by_plan(abi.representation().clone(), callee, &[])?;
    let result = b.pack(result.into_value())?;
    b.f.set_term(b.cur, Term::Return(vec![result]));
    Ok(b.f)
}

pub fn lower_global_with_plans(
    ws: &Workspace,
    scx: &ScalarCtx,
    plan: &ConcreteBodyPlan,
    concrete_plans: &ConcretePlans,
    name: Ustr,
) -> Result<Option<MirFunc>, CompilerError> {
    let ConcreteOwner::Global { module, index } = &plan.owner else {
        return Err(gap("concrete plan does not own a global"));
    };
    let mid = *module;
    let module = ws.module(mid);
    if *index as usize >= module.globals.len() {
        return Err(gap("concrete global plan has an invalid global index"));
    }
    let gid = Idx::from_raw(RawIdx::from_u32(*index));
    let def = &module.globals[gid];
    let (body, inf) =
        validate_always_plan(ws, scx, mid, BodyOwner::Global(gid), plan, concrete_plans)?;
    if body.root.is_none() {
        return Ok(None);
    }
    let f = artifact_mir(scx, name, CodeOwnerKind::Global, module.file, def.name);
    let mut b = Builder::new(
        ws,
        scx,
        plan,
        concrete_plans,
        body,
        Some(inf),
        f,
        FlatVariantsHandle::default(),
    );
    let global_plan = GlobalPlan::for_concrete_result(
        ws,
        scx,
        plan.signature.result.map(|id| b.concrete_mono(id).clone()),
    )
    .map_err(|error| {
        CompilerError::new_with_label(error.span.unwrap_or(def.name_span), error.message)
    })?;
    // A `nobox` global's initializer returns leaves; `mir_emit` binds one Lua
    // global per leaf.
    b.ret_rep = (!scx.values_first_class).then(|| global_plan.representation());
    b.lower_root()
        .map_err(|e| b.located(e).or_at(def.name_span))?;
    Ok(Some(b.f))
}

pub fn lower_decorator_with_plans(
    ws: &Workspace,
    scx: &ScalarCtx,
    plan: &ConcreteBodyPlan,
    concrete_plans: &ConcretePlans,
    name: Ustr,
) -> Result<MirFunc, CompilerError> {
    let ConcreteOwner::Decorator { module, index } = &plan.owner else {
        return Err(gap("concrete plan does not own a decorator"));
    };
    let mid = *module;
    let module = ws.module(mid);
    if *index as usize >= module.decorators.len() {
        return Err(gap(
            "concrete decorator plan has an invalid decorator index",
        ));
    }
    let did = Idx::from_raw(RawIdx::from_u32(*index));
    let def = &module.decorators[did];
    let (body, inf) = validate_always_plan(
        ws,
        scx,
        mid,
        BodyOwner::Decorator(did),
        plan,
        concrete_plans,
    )?;
    let target = body
        .deco_target
        .ok_or_else(|| gap("decorator body has no target binding").or_at(def.span))?;
    let mut f = artifact_mir(
        scx,
        name,
        CodeOwnerKind::Decorator,
        module.file,
        def.def_key,
    );
    let representation = scx
        .rep_arena()
        .boxed_function(scx.target(), plan.signature.params.len());
    f.abi_representation = Some(representation.clone());
    let mut b = Builder::new(
        ws,
        scx,
        plan,
        concrete_plans,
        body,
        Some(inf),
        f,
        FlatVariantsHandle::default(),
    );
    b.param_reps = representation.params().to_vec();
    b.ret_rep = plan.signature.result.map(|_| {
        representation
            .result()
            .expect("boxed decorator result plan")
    });
    // `target` is param 0, then the decorator's declared params.
    let at_decl = |error: CompilerError| error.or_at(def.span);
    b.bind_param_slot(target, &RepLayout::Opaque)
        .map_err(at_decl)?;
    b.lower_params().map_err(at_decl)?;
    b.lower_root().map_err(|e| at_decl(b.located(e)))?;
    Ok(b.f)
}

fn artifact_mir(
    scx: &ScalarCtx,
    symbol: Ustr,
    kind: CodeOwnerKind,
    module: Ustr,
    name: Ustr,
) -> MirFunc {
    let mut f = MirFunc::new(symbol);
    f.code_identity = Some(CodeIdentity::Artifact(CodeOwner::Artifact {
        kind,
        module,
        name,
    }));
    f.rep_arena = Some(scx.rep_arena().clone());
    f
}

fn validate_always_plan<'a>(
    ws: &'a Workspace,
    scx: &ScalarCtx,
    mid: ModuleId,
    owner: BodyOwner,
    plan: &ConcreteBodyPlan,
    concrete_plans: &ConcretePlans,
) -> Result<(&'a Body, &'a BodyInference), CompilerError> {
    let inf = ws
        .inference(mid)
        .and_then(|ir| ir.per_fn.get(&owner))
        .ok_or_else(|| gap("concrete always-emitted plan has no inference"))?;
    let body = ws
        .module(mid)
        .bodies
        .get(&owner)
        .ok_or_else(|| gap("concrete always-emitted plan has no body"))?;
    if crate::verify::checks_enabled() {
        let label = "concrete always-emitted plan";
        plan.types
            .check_source(scx.interner(), (mid, owner), body, label)
            .map_err(gap)?;
        validate_plan_domains(plan, label)?;
        validate_concrete_callee_plans(ws, scx, plan, concrete_plans)?;
    }
    Ok((body, inf))
}

fn validate_plan_domains(plan: &ConcreteBodyPlan, label: &str) -> Result<(), CompilerError> {
    for (site, _) in plan.callees.rows() {
        if !matches!(site.domain, crate::site::SiteDomain::Own)
            && plan.domain_types(&site.domain).is_none()
        {
            return Err(gap(format!(
                "{label} is missing type tables for site domain {:?}",
                site.domain
            )));
        }
    }
    Ok(())
}

fn callee_has_body_plan(
    ws: &Workspace,
    scx: &ScalarCtx,
    owner: &crate::instance::Instance,
) -> bool {
    requires_concrete_body_plan(ws, owner)
        && crate::monoty::too_deep_to_instantiate(owner.arg_types(scx.interner()).iter()).is_none()
}

fn validate_concrete_callee_plans(
    ws: &Workspace,
    scx: &ScalarCtx,
    plan: &ConcreteBodyPlan,
    concrete_plans: &ConcretePlans,
) -> Result<(), CompilerError> {
    for (site, callee) in plan.callees.rows() {
        let owner = &callee.instance;
        if !callee_has_body_plan(ws, scx, owner) {
            continue;
        }
        let callee_plan = concrete_plans.get(owner).ok_or_else(|| {
            gap(format!(
                "concrete callee body plan is missing for site {:?}",
                site.op
            ))
        })?;
        let recorded = &callee.signature;
        if recorded != &callee_plan.signature {
            return Err(gap(format!(
                "concrete callee signature disagrees with its body plan at site {:?}",
                site.op
            )));
        }
    }
    Ok(())
}

struct Builder<'a> {
    ws: &'a Workspace,
    scx: &'a ScalarCtx,
    concrete_plan: &'a ConcreteBodyPlan,
    concrete_plans: &'a ConcretePlans,
    /// `Some(elem)` where a generic is declared `I: Iterator<elem>`. A call's
    /// inferred type does not carry the instantiation, so a placeholder `next`'s
    /// element shape must come from here.
    generic_bounds: &'a [Option<TyId>],
    body: &'a Body,
    inf: Option<&'a BodyInference>,
    f: MirFunc,
    cur: Idx<Block>,
    binds: HashMap<u32, Idx<Cell>>,
    /// A break also leaves the scope owning a temporary loop generator.
    loop_targets: Vec<(Idx<Block>, Idx<Block>, usize, usize)>,
    /// A `while true` whose exit is never targeted terminates the statement list.
    used_break_targets: HashSet<Idx<Block>>,
    /// Spliced LIFO at block exit, before every `return` and at `break`/`continue`.
    defers: Vec<Vec<(Idx<Expr>, stmt::PendingDefer)>>,
    /// Global initializers and decorators are `Inline`: they are not emitted under
    /// the unwind trampoline.
    defer_mode: stmt::DeferMode,
    /// `yield`/`await` inside a defer body is a compile error.
    in_defer_body: bool,
    owned_reload_tail: Option<Idx<Block>>,
    resume_arg: Option<Idx<Cell>>,
    param_reps: Vec<RepId>,
    param_plan: Option<BackendFnAbiPlanHandle>,
    ret_rep: Option<RepId>,
    ret_ty: Option<&'a MonoTy>,
    fn_name_span: Option<Span>,
    cur_span: Option<Span>,
    cur_expr: Option<Idx<Expr>>,
    sibling_values: HashMap<Ustr, (ValueRepr, Vec<Idx<Cell>>)>,
    /// Struct defaults use their own expression arena, not `site_owner`'s.
    cur_domain: crate::site::SiteDomain,
    site_owner: Option<crate::instance::Instance>,
    cur_op: crate::site::SiteOp,
    group_binds: HashMap<u32, (Vec<GroupLeaf>, ValueShape)>,
    /// Copy reassigned boxes so SROA sees a constructor at every store.
    owned_value_binds: HashSet<u32>,
    ref_borrowed_binds: std::cell::OnceCell<(HashSet<u32>, HashSet<u32>)>,
    body_facts: std::cell::OnceCell<BodyFacts>,
    value_binds: HashMap<u32, Idx<Inst>>,
    preflight_reads: Vec<HashMap<u32, Idx<Inst>>>,
    /// A recursive `V?` child cannot be unrolled into its own copy.
    value_copy_stack: Vec<Ustr>,
    borrowed_value_children: HashMap<Idx<Inst>, std::borrow::Cow<'a, MonoTy>>,
    /// Boxes held only by immutable values; owned storage still copies them.
    shareable_value_boxes: HashSet<Idx<Inst>>,
    /// A host may supply heap records with property callbacks, so even generated
    /// projections must keep that effect.
    callback_records: HashMap<Idx<Inst>, Option<Ustr>>,
    /// Shallow local snapshots cannot be lent across a call or return.
    unshareable_value_cells: HashSet<Idx<Cell>>,
    boundary_materializes: HashSet<Idx<Expr>>,
    /// Every other box of a splatting tuple is the flat wire.
    element_tuples: HashSet<Idx<Inst>>,
    tuple_bind_boxes: HashMap<u32, Idx<Inst>>,
    scalar_tuple_reentry: HashSet<Idx<Expr>>,
    shallow_pack: bool,
    lending_pack: bool,
    /// Every assignment lowers its value through an expression entry that rounds
    /// these, so no store path rounds on its own.
    f32_round_exprs: HashSet<Idx<Expr>>,
    integer_field_checks: HashMap<Idx<Expr>, bool>,
    /// The shape is non-nullable: the branch that bound it consumed the presence.
    tuple_binds: HashMap<u32, (Vec<Idx<Cell>>, TupleLayout)>,
    static_cells: HashMap<u32, Ustr>,
    /// Stable across body edits and distinct per overload; emitted verbatim.
    static_owner: Ustr,
    /// Shadowing duplicates need distinct cells without renaming siblings.
    static_counters: HashMap<Ustr, u32>,
    nested_fn_globals: HashMap<u32, NestedFn>,
    nested_fn_counters: HashMap<Ustr, u32>,
    /// The exact expression, never a subtree, lowered only for its presence; a
    /// `V?` place read there skips its copy-out.
    presence_only: Option<Idx<Expr>>,
    flat_ref_binds: HashMap<u32, FlatRefKey>,
    flat_ref_cells: HashMap<u32, (Idx<Cell>, Idx<Cell>)>,
    flat_scan_lets: HashMap<u32, Idx<Expr>>,
    flat_ret: Option<FlatRefKey>,
    flat_variants: FlatVariantsHandle,
    flat_arg_captures: Vec<FlatArgCapture>,
}

impl<'a> Builder<'a> {
    fn new(
        ws: &'a Workspace,
        scx: &'a ScalarCtx,
        concrete_plan: &'a ConcreteBodyPlan,
        concrete_plans: &'a ConcretePlans,
        body: &'a Body,
        inf: Option<&'a BodyInference>,
        f: MirFunc,
        flat_variants: FlatVariantsHandle,
    ) -> Self {
        let resume_arg = f.resume_arg;
        let cur = f.entry();
        let static_owner = f.name;
        Self {
            ws,
            scx,
            concrete_plan,
            concrete_plans,
            generic_bounds: &[],
            body,
            inf,
            f,
            cur,
            binds: HashMap::default(),
            loop_targets: Vec::new(),
            used_break_targets: HashSet::default(),
            defers: Vec::new(),
            defer_mode: stmt::DeferMode::Inline,
            in_defer_body: false,
            owned_reload_tail: None,
            resume_arg,
            param_reps: Vec::new(),
            param_plan: None,
            ret_rep: None,
            ret_ty: concrete_plan
                .signature
                .result
                .map(|result| scx.interner().get_ref(result)),
            fn_name_span: None,
            cur_span: None,
            cur_expr: None,
            sibling_values: HashMap::default(),
            cur_domain: crate::site::SiteDomain::Own,
            cur_op: crate::site::SiteOp::DirectCall,
            site_owner: None,
            group_binds: HashMap::default(),
            owned_value_binds: HashSet::default(),
            ref_borrowed_binds: std::cell::OnceCell::new(),
            body_facts: std::cell::OnceCell::new(),
            value_binds: HashMap::default(),
            preflight_reads: Vec::new(),
            value_copy_stack: Vec::new(),
            borrowed_value_children: HashMap::default(),
            shareable_value_boxes: HashSet::default(),
            callback_records: HashMap::default(),
            unshareable_value_cells: HashSet::default(),
            boundary_materializes: HashSet::default(),
            element_tuples: HashSet::default(),
            tuple_bind_boxes: HashMap::default(),
            scalar_tuple_reentry: HashSet::default(),
            shallow_pack: false,
            lending_pack: false,
            f32_round_exprs: HashSet::default(),
            integer_field_checks: HashMap::default(),
            tuple_binds: HashMap::default(),
            static_cells: HashMap::default(),
            static_owner,
            static_counters: HashMap::default(),
            nested_fn_globals: HashMap::default(),
            nested_fn_counters: HashMap::default(),
            presence_only: None,
            flat_ref_binds: HashMap::default(),
            flat_ref_cells: HashMap::default(),
            flat_scan_lets: HashMap::default(),
            flat_ret: None,
            flat_variants,
            flat_arg_captures: Vec::new(),
        }
    }
}

impl<'a> Builder<'a> {
    fn new_binding_cell(
        &mut self,
        binding: Idx<BindingInfo>,
        name: Ustr,
        ty: Option<TyId>,
        projection: Ustr,
    ) -> Idx<Cell> {
        let cell = self.f.new_cell_typed(Some(name), ty);
        if let Some(concrete) = ty.and_then(|ty| self.concrete_ty_id(ty)) {
            self.f.cells[cell].never_nil = crate::nilness::monoty_never_nil(self.ws, concrete);
        }
        let binding_info = &self.body.binding_arena[binding];
        self.f.cells[cell].capture_identity =
            Some(crate::code_identity::CaptureIdentity::Binding {
                file: binding_info.span.file,
                index: raw(binding),
                name: binding_info.name,
                projection,
            });
        cell
    }

    fn new_binding_slots(&mut self, binding: Idx<BindingInfo>, width: usize) -> Vec<Idx<Cell>> {
        let name = self.body.binding_arena[binding].name;
        (0..width)
            .map(|i| {
                self.new_binding_cell(
                    binding,
                    crate::names::indexed(name.as_str(), "__", i),
                    None,
                    crate::names::indexed("", "", i),
                )
            })
            .collect()
    }

    fn store_binding_slots(
        &mut self,
        binding: Idx<BindingInfo>,
        slots: &[Idx<Inst>],
    ) -> Vec<Idx<Cell>> {
        let cells = self.new_binding_slots(binding, slots.len());
        for (&cell, &value) in cells.iter().zip(slots) {
            self.store_cell(cell, value);
        }
        cells
    }

    fn body_facts(&self) -> &BodyFacts {
        self.body_facts.get_or_init(|| BodyFacts::of(self.body))
    }

    /// Nothing writes, captures or borrows it after its binding, so it can be held
    /// as values with no cell.
    pub(crate) fn binding_stays_unwritten(&self, name: Idx<BindingInfo>) -> bool {
        // GLSL names locals only for cells; otherwise each use rewrites the value.
        !(self.scx.values_first_class
            || self.f.is_coroutine
            || self.body.binding_arena[name].kind != bone_core::hir::BindingKind::Let
            || self.binding_is_ref(name)
            || self.binding_ref_borrowed(name)
            || self.body_facts().may_capture
            || self.body_facts().written.contains(&raw(name)))
    }

    pub(crate) fn read_leaf(&mut self, leaf: GroupLeaf) -> Idx<Inst> {
        match leaf {
            GroupLeaf::Cell(cell) => self.emit_cur(Inst::Load(cell)),
            GroupLeaf::Value(value) => value,
        }
    }

    /// Never for an aggregate: a value struct's whole read copies from its cell.
    pub(crate) fn bind_let_value(&mut self, name: Idx<BindingInfo>, value: Idx<Inst>) -> bool {
        if !self.binding_stays_unwritten(name) {
            return false;
        }
        // Inlining can leave a constructor behind any aggregate; scalar
        // replacement splits only through cells.
        let holds_aggregate = self
            .binding_ty(name)
            .and_then(|ty| self.concrete_ty_id(ty))
            .is_none_or(|ty| match ty.peel_nullable().0 {
                crate::monoty::MonoTy::Tuple(_) => true,
                other => other
                    .nominal()
                    .is_some_and(|n| self.ws.nominals.struct_id(n).is_some()),
            });
        // An aggregate read records its own logical type, which the binding's type
        // need not match.
        if holds_aggregate
            || matches!(
                self.f.insts[value],
                Inst::MakeValueStruct { .. }
                    | Inst::MaterializeValueStruct { .. }
                    | Inst::MakeTuple(..)
                    | Inst::MakeStruct { .. }
                    | Inst::MakeEnum { .. }
                    | Inst::MakeEnumDyn { .. }
            )
        {
            return false;
        }
        self.value_binds.insert(raw(name), value);
        true
    }

    fn rep_slot(&self, id: RepId) -> &'a RepLayout {
        self.scx.rep_arena().layout(id)
    }

    fn param_slot(&self, index: usize) -> Result<&'a RepLayout, CompilerError> {
        if let Some(plan) = self.param_plan.as_ref() {
            if !plan.representation().uses_scalar_abi() {
                return Ok(&RepLayout::Opaque);
            }
            return plan
                .representation()
                .params()
                .get(index)
                .copied()
                .map(|id| self.rep_slot(id))
                .ok_or_else(|| {
                    self.gap_here(format!("parameter {index} has no representation plan"))
                });
        }
        self.param_reps
            .get(index)
            .copied()
            .map(|id| self.rep_slot(id))
            .ok_or_else(|| self.gap_here(format!("parameter {index} has no representation plan")))
    }

    fn result_shape(&self) -> Option<&'a ValueShape> {
        self.ret_rep
            .and_then(|id| self.scx.rep_arena().value_shape(id))
    }

    fn result_tuple(&self) -> Option<&'a TupleLayout> {
        self.ret_rep
            .and_then(|id| self.scx.rep_arena().tuple_shape(id))
    }

    fn gap_here(&self, msg: impl std::fmt::Display) -> CompilerError {
        match self.cur_span {
            Some(span) => {
                CompilerError::new_with_label(span, format!("skull-mir: lowering gap: {msg}"))
            }
            None => gap(msg),
        }
    }

    /// Call before the scope restores the cursor. Innermost wins.
    fn located(&self, error: CompilerError) -> CompilerError {
        match self.cur_span {
            Some(span) => error.or_at(span),
            None => error,
        }
    }

    pub(crate) fn branch_on_nil(
        &mut self,
        v: Idx<Inst>,
        nil_bb: Idx<Block>,
        value_bb: Idx<Block>,
    ) -> Idx<Inst> {
        let cond = self.is_nil(v);
        self.branch_to(cond, nil_bb, value_bb);
        cond
    }

    fn is_nil(&mut self, v: Idx<Inst>) -> Idx<Inst> {
        let nil = self.emit_cur(Inst::Const(Lit::Nil));
        self.emit_cur(Inst::Eq(v, nil))
    }

    fn read_field(&mut self, base: Idx<Inst>, field: Ustr) -> Idx<Inst> {
        self.emit_cur(Inst::Field {
            base,
            field,
            nonthrowing: false,
        })
    }

    /// A projection of a non-nil Rock value; still a mutable load at this point.
    fn read_value_field(&mut self, base: Idx<Inst>, field: Ustr) -> Idx<Inst> {
        self.emit_cur(Inst::Field {
            base,
            field,
            nonthrowing: true,
        })
    }

    fn write_field(&mut self, base: Idx<Inst>, field: Ustr, value: Idx<Inst>) -> Idx<Inst> {
        self.emit_cur(Inst::StoreField {
            base,
            field,
            value,
            native_leaf: None,
        })
    }

    fn read_index(&mut self, base: Idx<Inst>, index: Idx<Inst>, kind: ContainerKind) -> Idx<Inst> {
        self.emit_cur(Inst::Index { base, index, kind })
    }

    fn write_index(
        &mut self,
        base: Idx<Inst>,
        index: Idx<Inst>,
        value: Idx<Inst>,
        kind: ContainerKind,
    ) -> Idx<Inst> {
        self.emit_cur(Inst::StoreIndex {
            base,
            index,
            value,
            kind,
        })
    }

    fn store_cell(&mut self, cell: Idx<Cell>, value: Idx<Inst>) -> Idx<Inst> {
        self.emit_cur(Inst::Store { cell, value })
    }

    fn goto_if_open(&mut self, target: Idx<Block>) {
        if self.is_open() {
            self.f.set_term(self.cur, Term::Goto(target));
        }
    }

    fn branch_to(&mut self, cond: Idx<Inst>, then_bb: Idx<Block>, else_bb: Idx<Block>) {
        self.f.set_term(
            self.cur,
            Term::Branch {
                cond,
                then_bb,
                else_bb,
            },
        );
    }

    fn rep_error(&self, error: RepError) -> CompilerError {
        match error.span.or(self.cur_span) {
            Some(span) => CompilerError::new_with_label(span, error.message),
            None => CompilerError::new_no_location(error.message),
        }
    }

    fn fresh_cells(&mut self, n: usize) -> Vec<Idx<Cell>> {
        (0..n).map(|_| self.f.new_cell(None)).collect()
    }

    fn load_cells(&mut self, cells: &[Idx<Cell>]) -> Vec<Idx<Inst>> {
        cells
            .iter()
            .map(|&c| self.emit_cur(Inst::Load(c)))
            .collect()
    }

    fn int_plus_one(&mut self, value: Idx<Inst>) -> Idx<Inst> {
        let one = self.emit_cur(Inst::Const(Lit::Int(1)));
        self.emit_cur(Inst::IntAdd(value, one))
    }

    fn increment_cell(&mut self, cell: Idx<Cell>) {
        let value = self.emit_cur(Inst::Load(cell));
        let value = self.int_plus_one(value);
        self.store_cell(cell, value);
    }

    fn call_multi(
        &mut self,
        dests: Vec<Idx<Cell>>,
        func: FuncRef,
        args: Vec<Idx<Inst>>,
    ) -> (Idx<Inst>, Vec<Idx<Inst>>) {
        let loads_from = dests.clone();
        let call = self.emit_cur(Inst::CallMulti { dests, func, args });
        (call, self.load_cells(&loads_from))
    }

    fn call_value_multi(
        &mut self,
        dests: Vec<Idx<Cell>>,
        callee: Idx<Inst>,
        args: Vec<Idx<Inst>>,
    ) -> (Idx<Inst>, Vec<Idx<Inst>>) {
        let loads_from = dests.clone();
        let call = self.emit_cur(Inst::CallValueMulti {
            dests,
            callee,
            args,
        });
        (call, self.load_cells(&loads_from))
    }

    fn emit_cur(&mut self, mut inst: Inst) -> Idx<Inst> {
        match inst {
            Inst::Field { base, field, .. } => {
                if let Some(&owner) = self.callback_records.get(&base) {
                    let index = self.emit_cur(Inst::Const(Lit::Str(field)));
                    inst = Inst::Index {
                        base,
                        index,
                        kind: ContainerKind::Record(owner),
                    };
                }
            }
            Inst::StoreField {
                base,
                field,
                value,
                native_leaf: None,
            } => {
                if let Some(&owner) = self.callback_records.get(&base) {
                    let index = self.emit_cur(Inst::Const(Lit::Str(field)));
                    inst = Inst::StoreIndex {
                        base,
                        index,
                        value,
                        kind: ContainerKind::Record(owner),
                    };
                }
            }
            _ => {}
        }
        let is_call = matches!(
            inst,
            Inst::Call { .. }
                | Inst::CallMulti { .. }
                | Inst::CallValue { .. }
                | Inst::CallValueMulti { .. }
                | Inst::CallBoundMulti { .. }
        );
        let id = self.f.emit(self.cur, inst);
        if let Some(span) = self.cur_span {
            self.f.inst_spans.insert(id, span);
        }
        // Record the source site before instruction indices enter later passes.
        let is_mint = self.cur_op == crate::site::SiteOp::FnValue;
        if is_call || is_mint {
            if let (Some(owner), Some(expr)) = (self.site_owner.clone(), self.cur_expr) {
                let owner = crate::site::intern_owner(&mut self.f.site_owners, owner);
                self.f.call_sites.insert(
                    id,
                    crate::site::CallSite {
                        owner,
                        site: crate::site::Site {
                            domain: self.cur_domain.clone(),
                            expr,
                            op: self.cur_op.clone(),
                        },
                    },
                );
            }
        }
        // Cleanup drops the unreachable continuation after a raising call.
        if let Inst::Call { func, .. } = self.f.insts[id]
            && crate::intrinsics::never_returns(&self.ws.module(func.module).functions[func.func])
        {
            self.cur = self.f.new_block();
        }
        if let Inst::Global(name) = self.f.insts[id]
            && crate::verify::checks_enabled()
            && matches!(
                self.scx.target,
                bone_core::hir::FnBackend::Lua | bone_core::hir::FnBackend::Js
            )
            && let Some(check) = self
                .scx
                .global_read_checks(self.ws)
                .get(&crate::mangle::symbol_of_key(name))
                .copied()
        {
            self.check_global_read(id, &check);
        }
        id
    }

    fn check_global_read(
        &mut self,
        value: Idx<Inst>,
        check: &crate::global_storage::GlobalReadCheck,
    ) {
        if self.f.blocks[self.cur].insts.iter().any(|i| {
            matches!(self.f.insts[*i], Inst::StoreGlobal { name, .. } if crate::mangle::symbol_of_key(name) == check.probe)
        }) {
            return;
        }
        let error = crate::names::ustr("error");
        let error_function = self
            .ws
            .functions_by_name
            .get(&error)
            .and_then(|defs| defs.first())
            .copied();
        let probe = if matches!(self.f.insts[value], Inst::Global(name) if crate::mangle::symbol_of_key(name) == check.probe)
        {
            value
        } else {
            self.f.emit(self.cur, Inst::Global(check.probe))
        };
        let nil = self.f.emit(self.cur, Inst::Const(Lit::Nil));
        let missing = self.f.emit(self.cur, Inst::Eq(probe, nil));
        let fail = self.f.new_block();
        let next = self.f.new_block();
        self.branch_to(missing, fail, next);
        let span = self.cur_span.unwrap_or_default();
        let message = format!(
            "uninitialized global `{}` at {}:{}:{}",
            check.name,
            span.file,
            span.line_range().0 + 1,
            span.col_range().0 + 1
        );
        let message = self
            .f
            .emit(fail, Inst::Const(Lit::Str(crate::names::ustr(&message))));
        let call = match error_function {
            Some((module, func)) => Inst::Call {
                func: FuncRef { module, func },
                args: vec![message],
            },
            None => {
                let callee = self.f.emit(fail, Inst::Global(error));
                Inst::CallValue {
                    callee,
                    args: vec![message],
                }
            }
        };
        let call = self.f.emit(fail, call);
        self.f.inst_spans.insert(call, span);
        self.cur = next;
    }

    pub(crate) fn dispatch(&self) -> dispatch::DispatchCtx<'_> {
        dispatch::DispatchCtx {
            ws: self.ws,
            inf: self.inf,
            body: self.body,
            concrete_plan: Some(self.concrete_plan),
            concrete_types: self.concrete_plan.domain_types(&self.cur_domain),
            interner: Some(self.scx.interner()),
            rep_arena: self.scx.rep_arena.as_ref(),
            target: self.scx.target,
            logical_subst: None,
            values_first_class: self.scx.values_first_class,
        }
    }

    fn with_site_expr<T>(&mut self, expr: Idx<Expr>, f: impl FnOnce(&mut Self) -> T) -> T {
        let outer = self.cur_expr.replace(expr);
        let out = f(self);
        self.cur_expr = outer;
        out
    }

    fn in_domain<T>(
        &mut self,
        domain: crate::site::SiteDomain,
        f: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let outer = std::mem::replace(&mut self.cur_domain, domain);
        let out = f(self);
        self.cur_domain = outer;
        out
    }

    /// A `for` head carries several operations at one expression.
    fn with_site_op<T>(&mut self, op: crate::site::SiteOp, f: impl FnOnce(&mut Self) -> T) -> T {
        let outer = std::mem::replace(&mut self.cur_op, op);
        let out = f(self);
        self.cur_op = outer;
        out
    }

    /// False after a diverging sub-expression; the caller must not append a
    /// fall-through `Store` or `Goto` then.
    fn is_open(&self) -> bool {
        matches!(self.f.blocks[self.cur].term, Term::Unreachable)
    }

    fn lower_params(&mut self) -> Result<(), CompilerError> {
        for (i, pb) in self.body.param_bindings.into_iter().enumerate() {
            let slot = self.param_slot(i)?;
            self.bind_param_slot(pb, slot)?;
        }
        self.finish_params()
    }

    fn finish_params(&mut self) -> Result<(), CompilerError> {
        self.fill_param_defaults()?;
        self.copy_ref_borrowed_params(self.body.param_bindings)
    }

    /// A caller may lend its own box to a boxed value parameter, so a `ref`-borrowed
    /// one is copied on entry.
    pub(crate) fn copy_ref_borrowed_params(
        &mut self,
        params: IdxRange<BindingInfo>,
    ) -> Result<(), CompilerError> {
        for pb in params {
            if self.scx.values_first_class
                || self.binding_is_ref(pb)
                || !self.binding_ref_borrowed(pb)
            {
                continue;
            }
            let Some(ty) = self.binding_ty(pb).and_then(|ty| self.concrete_ty_id(ty)) else {
                continue;
            };
            if !ty
                .peel_nullable()
                .0
                .nominal()
                .is_some_and(|name| self.is_value_struct(name) && !self.is_nobox_struct(name))
            {
                continue;
            }
            if let Some((cells, shape)) = self.group_binds.remove(&raw(pb)) {
                let leaves = cells
                    .iter()
                    .map(|&leaf| self.read_leaf(leaf))
                    .collect::<Vec<_>>();
                let repr = Self::rebuild_from_leaves(&shape, &leaves);
                self.mark_borrowed_value_children(&shape, &repr, Some(ty))?;
                let value = self.with_value_lending(false, |s| s.pack(repr))?;
                let cell = self.new_binding_cell(
                    pb,
                    self.body.binding_arena[pb].name,
                    self.binding_ty(pb),
                    crate::names::ustr(""),
                );
                self.store_cell(cell, value);
                self.binds.insert(raw(pb), cell);
                self.owned_value_binds.insert(raw(pb));
                continue;
            }
            let Some(&cell) = self.binds.get(&raw(pb)) else {
                continue;
            };
            let v = self.emit_cur(Inst::Load(cell));
            let copy = self.copied_value_box(ty, v)?;
            self.store_cell(cell, copy);
        }
        Ok(())
    }

    pub(crate) fn bind_param_slot(
        &mut self,
        pb: Idx<BindingInfo>,
        slot: &RepLayout,
    ) -> Result<(), CompilerError> {
        let name = self.body.binding_arena[pb].name;
        if !slot.splats() {
            let cty = self.binding_ty(pb);
            let cell = self.new_binding_cell(pb, name, cty, crate::names::ustr(""));
            self.f.params.push(cell);
            self.binds.insert(raw(pb), cell);
            return Ok(());
        }
        let width = slot.slot_count(self.scx.rep_arena());
        let leaf_tys = match slot {
            RepLayout::Leaves(shape) if self.flat_binding_width(shape)?.is_none() => {
                self.value_shape_leaf_monos(shape)
            }
            _ => None,
        };
        let cells = self.new_binding_slots(pb, width);
        for (&cell, ty) in cells.iter().zip(leaf_tys.iter().flatten()) {
            self.f.cells[cell].never_nil = crate::nilness::monoty_never_nil(self.ws, ty);
        }
        self.f.params.extend(cells.iter().copied());
        match slot {
            RepLayout::Leaves(shape) => {
                self.group_binds.insert(
                    raw(pb),
                    (
                        cells.iter().map(|&c| GroupLeaf::Cell(c)).collect(),
                        shape.clone(),
                    ),
                );
            }
            RepLayout::Tuple(inner) => {
                self.tuple_binds.insert(raw(pb), (cells, inner.clone()));
            }
            RepLayout::Opaque => unreachable!("guarded by slot.splats"),
        }
        Ok(())
    }

    /// `null` stays omitted for call-site constants and callee default guards.
    fn constant_param_default(
        &self,
        body: &Body,
        inf: Option<&BodyInference>,
        expr: Idx<Expr>,
        ty: Option<TyId>,
    ) -> Option<CallParamDefault> {
        let literal = match body.expr_arena[expr] {
            Expr::Int(v) if ty == Some(self.ws.builtins.uint()) => Lit::Uint(v as u32),
            Expr::Int(v) => Lit::Int(v),
            Expr::Float(v) => Lit::Float(v.0),
            Expr::Bool(v) => Lit::Bool(v),
            Expr::String(v) => Lit::Str(v),
            _ => {
                let TypeExpr::App {
                    ctor: TypeCtor::Nominal(name),
                    ..
                } = static_ty(self.ws, ty?)?
                else {
                    return None;
                };
                if name.as_str() != "Vec3" {
                    return None;
                }
                return self.constant_vector_default(body, inf, expr);
            }
        };
        Some(CallParamDefault::Scalar(literal))
    }

    /// Reads both arenas in their lexical owners and never lowers a foreign
    /// expression in the caller.
    fn constant_vector_default(
        &self,
        body: &Body,
        inf: Option<&BodyInference>,
        expr: Idx<Expr>,
    ) -> Option<CallParamDefault> {
        let number = |body: &Body, e: Idx<Expr>| match body.expr_arena[e] {
            Expr::Float(v) => Some(Lit::Float(v.0)),
            Expr::Int(v) => Some(Lit::Float(v as f64)),
            _ => None,
        };
        let mut bindings = HashMap::default();
        let (body, expr) = match body.expr_arena[expr] {
            Expr::Call {
                callee: Callee::Unresolved { name, .. },
                args,
                ..
            } => {
                let (module, func) = resolve_call_fn(self.ws, inf?, expr, name)?;
                let target =
                    backend_variant(self.ws, name, FuncRef { module, func }, self.scx.target)
                        .ok()?;
                let module = self.ws.module(target.module);
                let decl = &module.functions[target.func];
                let ctor = module.bodies.get(&BodyOwner::Function(target.func))?;
                if decl.is_variadic
                    || decl.is_coroutine
                    || decl.is_extern
                    || decl.inline_body.is_some()
                    || !decl.generics.is_empty()
                    || args.len() != decl.param_decls.len()
                    || ctor.param_bindings.len() != decl.param_decls.len()
                {
                    return None;
                }
                let params: Vec<_> = ctor.param_bindings.into_iter().collect();
                let mut next_pos = 0;
                for arg in args {
                    if body.spread_args.contains(&arg) {
                        return None;
                    }
                    let index = match body.arg_names.get(&arg) {
                        Some(name) => decl.param_decls.iter().position(|pd| pd.name == *name)?,
                        None => {
                            let i = next_pos;
                            next_pos += 1;
                            i
                        }
                    };
                    let binding = *params.get(index)?;
                    if bindings.insert(binding, number(body, arg)?).is_some() {
                        return None;
                    }
                }
                if bindings.len() != params.len() {
                    return None;
                }
                let root = ctor.root?;
                let returned = match ctor.expr_arena[root] {
                    Expr::Block { statements, tail } => {
                        let statements: Vec<_> = statements
                            .into_iter()
                            .filter(|&s| !matches!(ctor.statement_arena[s], Statement::Comment(_)))
                            .take(2)
                            .collect();
                        match (statements.as_slice(), tail) {
                            ([], Some(tail)) => tail,
                            ([statement], None) => match ctor.statement_arena[*statement] {
                                Statement::Return(Some(value)) => value,
                                _ => return None,
                            },
                            _ => return None,
                        }
                    }
                    Expr::Return(Some(value)) => value,
                    _ => root,
                };
                (ctor, returned)
            }
            _ => (body, expr),
        };
        let Expr::StructLiteral {
            name,
            fields,
            bases,
        } = body.expr_arena[expr]
        else {
            return None;
        };
        if name.as_str() != "Vec3" || !bases.is_empty() {
            return None;
        }
        let shape = value_shape(self.ws, name)?;
        if shape.nullable
            || shape.fields.len() != 3
            || fields.len() != shape.fields.len()
            || shape
                .fields
                .iter()
                .any(|(_, field)| !matches!(field, FieldShape::Prim))
        {
            return None;
        }
        let mut values = HashMap::default();
        for field in fields {
            let field = &body.struct_lit_field_arena[field];
            let value = match body.expr_arena[field.value] {
                Expr::Var(binding) => bindings.get(&binding)?.clone(),
                _ => number(body, field.value)?,
            };
            if values.insert(field.name, value).is_some() {
                return None;
            }
        }
        let literals = shape
            .fields
            .iter()
            .map(|(name, _)| values.remove(name))
            .collect::<Option<_>>()?;
        Some(CallParamDefault::Vector { shape, literals })
    }

    pub(crate) fn call_param_defaults(&self, func: FuncRef) -> Vec<Option<CallParamDefault>> {
        let module = self.ws.module(func.module);
        let decl = &module.functions[func.func];
        let Some(body) = module.bodies.get(&BodyOwner::Function(func.func)) else {
            return Vec::new();
        };
        if decl.is_variadic {
            return Vec::new();
        }
        let inf = self
            .ws
            .inference(func.module)
            .and_then(|ir| ir.per_fn.get(&BodyOwner::Function(func.func)));
        // A foreign wrapper has no Rock prologue to test the omitted marker.
        let rock_body = decl.inline_body.is_none() && !decl.is_extern;
        body.param_defaults
            .iter()
            .enumerate()
            .map(|(i, expr)| {
                let ty = decl.param_decls.get(i)?.ty;
                self.constant_param_default(body, inf, (*expr)?, Some(ty))
                    .or_else(|| {
                        (rock_body && param_null_overrides_default(self.ws, body, i, ty))
                            .then_some(CallParamDefault::Scalar(Lit::Omitted))
                    })
            })
            .collect()
    }

    fn append_omitted_argument(
        &mut self,
        out: &mut Vec<Idx<Inst>>,
        default: Option<&CallParamDefault>,
        slot: &RepLayout,
    ) -> Result<(), CompilerError> {
        match default {
            Some(CallParamDefault::Scalar(lit)) if slot.slot_count(self.scx.rep_arena()) == 1 => {
                out.push(self.emit_cur(Inst::Const(lit.clone())));
            }
            // An absent scalarized argument needs nil in every lane of its slot.
            None | Some(CallParamDefault::Scalar(Lit::Omitted)) => {
                for _ in 0..slot.slot_count(self.scx.rep_arena()) {
                    out.push(self.emit_cur(Inst::Const(Lit::Nil)));
                }
            }
            Some(CallParamDefault::Vector { shape, literals }) => {
                let leaves: Vec<_> = literals
                    .iter()
                    .map(|lit| self.emit_cur(Inst::Const(lit.clone())))
                    .collect();
                match slot {
                    RepLayout::Leaves(expected) if expected == shape => out.extend(leaves),
                    RepLayout::Opaque => {
                        let parts = leaves.into_iter().map(ValueRepr::Scalar).collect();
                        out.push(self.pack(ValueRepr::Group {
                            name: shape.name,
                            parts,
                            present: None,
                        })?);
                    }
                    _ => {
                        return Err(self
                            .gap_here("constant vector default does not match its parameter ABI"));
                    }
                }
            }
            _ => {
                return Err(
                    self.gap_here("constant scalar default does not match its parameter ABI")
                );
            }
        }
        Ok(())
    }

    /// Opaque callers may omit arguments, so the callee fills defaults too.
    fn fill_param_defaults(&mut self) -> Result<(), CompilerError> {
        let pbs: Vec<Idx<BindingInfo>> = self.body.param_bindings.into_iter().collect();
        let defaults = self.body.param_defaults.clone();
        for (i, def) in defaults.iter().enumerate() {
            let Some(def) = def else { continue };
            let Some(&pb) = pbs.get(i) else { continue };
            let ty = self.body.binding_arena[pb]
                .declared_ty
                .or_else(|| self.binding_ty(pb));
            let materialized = self
                .constant_param_default(self.body, self.inf, *def, ty)
                .is_some();
            let (slot, cells, marker) = if let Some((leaves, _)) = self.group_binds.get(&raw(pb)) {
                (
                    self.param_slot(i)?,
                    leaves.iter().map(|leaf| leaf.cell()).collect(),
                    Lit::Nil,
                )
            } else if let Some((cells, _)) = self.tuple_binds.get(&raw(pb)) {
                (self.param_slot(i)?, cells.clone(), Lit::Nil)
            } else if let Some(&cell) = self.binds.get(&raw(pb))
                && !matches!(self.body.expr_arena[*def], Expr::Null)
            {
                let marker = match self.body.binding_arena[pb].declared_ty {
                    Some(ty) if param_null_overrides_default(self.ws, self.body, i, ty) => {
                        Lit::Omitted
                    }
                    _ => Lit::Nil,
                };
                (&RepLayout::Opaque, vec![cell], marker)
            } else {
                continue;
            };
            for &cell in &cells {
                self.f.mark_default_param(cell);
            }
            let (default, join) =
                self.branch_for_parameter_default(materialized, slot, &cells, marker);
            self.cur = default;
            let values = match slot {
                RepLayout::Opaque => {
                    let expected = self.binding_ty(pb).and_then(|ty| self.concrete_ty_id(ty));
                    vec![self.lower_expr_with_expected(*def, expected)?]
                }
                RepLayout::Leaves(shape) => {
                    let value = self.lower_expr_repr(*def)?;
                    self.repr_to_storage_leaves(&value, shape, "struct default")?
                }
                RepLayout::Tuple(shape) => {
                    let value = self.lower_expr_repr(*def)?.into_value();
                    self.tuple_repr_to_slots(value, shape)?
                }
            };
            for (&cell, value) in cells.iter().zip(values) {
                self.store_cell(cell, value);
            }
            if matches!(slot, RepLayout::Opaque) {
                self.f.set_term(self.cur, Term::Goto(join));
            } else {
                self.goto_if_open(join);
            }
            self.cur = join;
        }
        Ok(())
    }

    fn branch_for_parameter_default(
        &mut self,
        materialized: bool,
        slot: &RepLayout,
        cells: &[Idx<Cell>],
        marker: Lit,
    ) -> (Idx<Block>, Idx<Block>) {
        let test = |s: &mut Self, cell, marker| {
            let cur = s.emit_cur(Inst::Load(cell));
            let marker = s.emit_cur(Inst::Const(marker));
            let missing = s.emit_cur(Inst::Eq(cur, marker));
            if materialized {
                s.f.materialized_default_guards.insert(missing);
            }
            missing
        };
        if matches!(slot, RepLayout::Tuple(_)) {
            // Every tuple lane must be nil. Individual supplied elements may be nil.
            let default = self.f.new_block();
            let join = self.f.new_block();
            for &cell in cells {
                let missing = test(self, cell, Lit::Nil);
                let next = self.f.new_block();
                self.branch_to(missing, next, join);
                self.cur = next;
            }
            self.f.set_term(self.cur, Term::Goto(default));
            return (default, join);
        }
        let missing = test(self, cells[0], marker);
        let default = self.f.new_block();
        let join = self.f.new_block();
        self.branch_to(missing, default, join);
        (default, join)
    }

    fn lower_root(&mut self) -> Result<(), CompilerError> {
        let Some(root) = self.body.root else {
            self.f.set_term(self.cur, Term::Return(vec![]));
            return Ok(());
        };
        if let Expr::Block { statements, tail } = self.body.expr_arena[root] {
            self.defers.push(Vec::new());
            let terminated = self.lower_stmts(statements)?;
            if terminated {
                self.defers.pop();
                return Ok(());
            }
            // Coroutine tails are discarded, so a reachable end is fall-off.
            if self.f.is_coroutine {
                self.reject_nonunit_coroutine_falloff()?;
            }
            if !self.f.is_coroutine {
                self.lower_return(tail)?;
                self.defers.pop();
                return Ok(());
            }
            if let Some(tail) = tail {
                self.lower_stmt_expr(tail)?;
            }
            if !self.is_open() {
                self.defers.pop();
                return Ok(());
            }
            self.run_top_frame()?; // splice this scope's defers before the return
            self.defers.pop();
            self.f.set_term(self.cur, Term::Return(Vec::new()));
            Ok(())
        } else {
            self.lower_return(Some(root))
        }
    }

    /// Reachable fallthrough in a non-null, non-unit coroutine would publish a
    /// missing result.
    fn reject_nonunit_coroutine_falloff(&self) -> Result<(), CompilerError> {
        let plan = self.concrete_plan;
        let Some(result_id) = plan.signature.result else {
            return Ok(());
        };
        let result = self.concrete_mono(result_id);
        let crate::monoty::MonoTy::App {
            ctor: crate::monoty::MonoCtor::Nominal(name),
            params,
        } = result
        else {
            return Ok(());
        };
        if !bone_core::modules::is_intrinsic_key(*name, "Coroutine") {
            return Ok(());
        }
        let Some(result) = params.get(1) else {
            return Ok(());
        };
        let nonunit_concrete = match result {
            crate::monoty::MonoTy::Unit | crate::monoty::MonoTy::Nullable(_) => false,
            crate::monoty::MonoTy::Tuple(parts) => !parts.is_empty(),
            _ => true,
        };
        if nonunit_concrete {
            let msg = format!(
                "co fn `{}` can fall off the end of its body, but its coroutine result type is not `()` -- \
                 a completed coroutine would hand `result()` a nil typed as the declared result; \
                 add an explicit `return <value>` on every path",
                self.f.name
            );
            return Err(match self.fn_name_span {
                Some(span) => CompilerError::new_with_label(span, msg),
                None => CompilerError::new_no_location(msg),
            });
        }
        Ok(())
    }

    fn lower_return(&mut self, opt: Option<Idx<Expr>>) -> Result<(), CompilerError> {
        let direct = self.scx.target == bone_core::hir::FnBackend::Lua
            && !self.f.is_coroutine
            && self.ret_rep.is_some()
            && self.defers.iter().all(Vec::is_empty);
        if direct && let Some(e) = opt {
            let outer = self.cur_span.replace(self.body.expr_spans[e]);
            let outer_expr = self.cur_expr.replace(e);
            let result = self.lower_expr_to_return(e);
            self.cur_span = outer;
            self.cur_expr = outer_expr;
            if let Some(result) = result {
                return result;
            }
        }
        let leaves = self.return_leaves(opt)?;
        if self.is_open() {
            self.run_all_defers()?;
            self.f.set_term(self.cur, Term::Return(leaves));
        }
        Ok(())
    }

    pub(crate) fn return_leaves(
        &mut self,
        opt: Option<Idx<Expr>>,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        self.with_value_lending(true, |s| s.return_leaves_lent(opt))
    }

    fn return_leaves_lent(
        &mut self,
        opt: Option<Idx<Expr>>,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        let Some(e) = opt else { return Ok(vec![]) };
        let json_default_return = self.site_owner.as_ref().is_some_and(|owner| {
            matches!(
                self.ws.module(owner.func.module).functions[owner.func.func].json_field_for,
                Some((_, _, skull_parser::JsonFieldOperation::Default))
            )
        }) && self.body.root.is_some_and(|root| {
            let Expr::Block { statements, .. } = self.body.expr_arena[root] else {
                return false;
            };
            statements.into_iter().next_back().is_some_and(|statement| {
                matches!(self.body.statement_arena[statement], Statement::Return(Some(value)) if value == e)
            })
        });
        if json_default_return {
            let repr = self.lower_expr_repr(e)?.into_value();
            return Ok(vec![self.materialize_repr(repr)?]);
        }
        if self.ret_rep.is_none() {
            let gpu_context = self.site_owner.as_ref().is_some_and(|owner| {
                crate::intrinsics::is_gpu_context(
                    &self.ws.module(owner.func.module).functions[owner.func.func],
                )
            });
            // Shader planning needs value-form control flow to keep GPU-only effects
            // in a discarded tail.
            if gpu_context {
                self.lower_expr(e)?;
            } else {
                self.lower_stmt_expr(e)?;
            }
            return Ok(vec![]);
        }
        if self.ret_ty.is_some_and(|result| {
            matches!(
                result.peel_nullable().0,
                MonoTy::App {
                    ctor: MonoCtor::Ref,
                    ..
                }
            )
        }) {
            if self.flat_ret.is_some() {
                return self.flat_return_leaves(e);
            }
            return Ok(vec![self.lower_ref_borrow(e)?]);
        }
        if let Some(shape) = self.result_shape() {
            let is_phantom = match &self.body.expr_arena[e] {
                Expr::Call {
                    callee: Callee::Unresolved { name, .. },
                    args,
                    ..
                } => name.as_str() == "phantom" && (*args).into_iter().next().is_none(),
                _ => false,
            };
            if is_phantom {
                self.lower_expr(e)?;
                return Ok((0..shape.leaf_count())
                    .map(|_| self.emit_cur(Inst::Const(Lit::Nil)))
                    .collect());
            }
            let r = self.lower_expr_repr(e)?;
            // A terminating block returns a placeholder, not an aggregate to project.
            if !self.is_open() {
                return Ok(Vec::new());
            }
            let scx = self.scx;
            let representation = r.representation();
            let plan = scx.rep_arena().get(representation);
            return match plan.layout() {
                RepLayout::Opaque => self.repr_to_leaves_shape(r.value(), shape),
                RepLayout::Leaves(actual) => {
                    let r = ValueBundle::new(
                        representation,
                        self.owned_value_children(r.into_value())?,
                    );
                    self.repr_to_leaves_for_shape(&r, actual, shape)
                }
                RepLayout::Tuple(_) => Err(self.gap_here(format!(
                    "representation {representation:?} is not an opaque boundary at return boundary"
                ))),
            };
        }
        if let Some(shape) = self.result_tuple() {
            if shape.splats(self.scx.rep_arena()) {
                let r = self.lower_expr_repr(e)?.into_value();
                if !self.is_open() {
                    return Ok(Vec::new());
                }
                let r = self.owned_value_children(r)?;
                return self.tuple_repr_to_slots(r, shape);
            }
            if let Expr::Tuple(elems) = self.body.expr_arena[e] {
                return self.lower_tuple_operands(e, elems);
            }
            let v = self.lower_expr(e)?;
            if !self.is_open() {
                return Ok(Vec::new());
            }
            return Ok(self.tuple_return_operands(v, shape.slots.len()));
        }
        let v = self.lower_expr_with_expected(e, self.ret_ty)?;
        if !self.is_open() {
            return Ok(Vec::new());
        }
        let v = match self.ret_ty {
            Some(result) => self.coerced_mono(v, e, result),
            None => v,
        };
        Ok(vec![v])
    }

    fn absent_tuple_slots(&mut self, shape: &TupleLayout) -> Vec<Idx<Inst>> {
        let count = shape.slot_count(self.scx.rep_arena());
        let mut out = Vec::with_capacity(count);
        if shape.nullable {
            out.push(self.emit_cur(Inst::Const(Lit::Bool(false))));
        }
        while out.len() < count {
            out.push(self.emit_cur(Inst::Const(Lit::Nil)));
        }
        out
    }

    /// Every other value of a splatting tuple type is the flat box; this is the one
    /// place layout is decided.
    pub(crate) fn is_tuple_aggregate(&self, v: Idx<Inst>) -> bool {
        self.element_tuples.contains(&v)
    }

    pub(crate) fn flat_box_slots(&mut self, base: Idx<Inst>, width: usize) -> Vec<Idx<Inst>> {
        self.f.flat_box_widths.insert(base, width);
        (0..width)
            .map(|index| {
                let slot = self.emit_cur(Inst::TupleIndex { base, index });
                self.f.synthetic_projections.insert(slot);
                slot
            })
            .collect()
    }

    pub(crate) fn flat_box_width(&self, base: Idx<Inst>) -> Option<usize> {
        self.f.flat_box_widths.get(&base).copied()
    }

    pub(crate) fn element_tuple(&mut self, values: Vec<Idx<Inst>>) -> Idx<Inst> {
        let tuple = self.emit_cur(Inst::MakeTuple(values));
        self.element_tuples.insert(tuple);
        tuple
    }

    fn tuple_return_operands(&mut self, v: Idx<Inst>, n: usize) -> Vec<Idx<Inst>> {
        if let Inst::MakeTuple(vs) = &self.f.insts[v] {
            if vs.len() == n {
                return vs.clone();
            }
        }
        self.flat_box_slots(v, n)
    }

    /// The `present, <slots...>` wire of a flat box that may be nil; absent is
    /// `false` plus nils.
    fn nullable_box_slots(&mut self, boxed: Idx<Inst>, shape: &TupleLayout) -> Vec<Idx<Inst>> {
        let absent = self.absent_tuple_slots(shape);
        let cells = self.fresh_cells(absent.len());
        let present_bb = self.f.new_block();
        let absent_bb = self.f.new_block();
        let join = self.f.new_block();
        self.branch_on_nil(boxed, absent_bb, present_bb);

        self.cur = present_bb;
        let mut present = vec![self.emit_cur(Inst::Const(Lit::Bool(true)))];
        present.extend(self.flat_box_slots(boxed, absent.len() - 1));
        for (&cell, value) in cells.iter().zip(present) {
            self.store_cell(cell, value);
        }
        self.f.set_term(self.cur, Term::Goto(join));

        self.cur = absent_bb;
        for (&cell, value) in cells.iter().zip(absent) {
            self.store_cell(cell, value);
        }
        self.f.set_term(self.cur, Term::Goto(join));

        self.cur = join;
        self.load_cells(&cells)
    }

    fn tuple_repr_to_slots(
        &mut self,
        r: ValueRepr,
        shape: &TupleLayout,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        // A phantom tuple has no aggregate to project; every wire slot is absent.
        if let ValueRepr::Scalar(v) = &r {
            if matches!(self.f.insts[*v], Inst::Const(Lit::Nil)) {
                return Ok(self.absent_tuple_slots(shape));
            }
            if shape.nullable && !self.is_tuple_aggregate(*v) {
                return Ok(self.nullable_box_slots(*v, shape));
            }
        }
        let present = match (shape.nullable, &r) {
            (false, _) => None,
            (
                true,
                ValueRepr::Tuple {
                    present: Some(p), ..
                },
            ) => Some(*p),
            (true, _) => Some(self.emit_cur(Inst::Const(Lit::Bool(true)))),
        };
        let count = shape.slot_count(self.scx.rep_arena());
        let parts: Vec<ValueRepr> = match r {
            ValueRepr::Tuple { parts, .. } if parts.len() == shape.slots.len() => parts,
            other => {
                let base = self.pack(other)?;
                if shape.splats(self.scx.rep_arena()) && !self.is_tuple_aggregate(base) {
                    let mut out = Vec::with_capacity(count);
                    out.extend(present);
                    let n = count - shape.nullable as usize;
                    out.extend(self.flat_box_slots(base, n));
                    return Ok(out);
                }
                (0..shape.slots.len())
                    .map(|index| ValueRepr::Scalar(self.emit_cur(Inst::TupleIndex { base, index })))
                    .collect()
            }
        };
        let mut out = Vec::with_capacity(count);
        out.extend(present);
        for (&slot, part) in shape.slots.iter().zip(parts) {
            match self.scx.rep_arena().layout(slot) {
                RepLayout::Opaque => out.push(self.pack(part)?),
                // The iterator's missing element can leave a non-null slot empty.
                RepLayout::Leaves(vs)
                    if !vs.nullable && !matches!(part, ValueRepr::Group { .. }) =>
                {
                    let mut nullable = vs.clone();
                    nullable.nullable = true;
                    let leaves = self.repr_to_leaves_shape(&part, &nullable)?;
                    out.extend(leaves.into_iter().skip(1));
                }
                RepLayout::Leaves(vs) => out.extend(self.repr_to_leaves_shape(&part, vs)?),
                RepLayout::Tuple(inner) => out.extend(self.tuple_repr_to_slots(part, inner)?),
            }
        }
        Ok(out)
    }

    /// Never boxes a `nobox` element.
    pub(crate) fn tuple_from_slots(
        &mut self,
        shape: &TupleLayout,
        slots: &[Idx<Inst>],
        idx: &mut usize,
    ) -> ValueRepr {
        // Presence leads the wire, exactly as it does one level down in
        // `rebuild_from_leaves`, so it is consumed before the slots.
        let present = shape.nullable.then(|| {
            let v = slots[*idx];
            *idx += 1;
            v
        });
        let parts = shape
            .slots
            .iter()
            .map(|&slot| self.slot_repr(self.scx.rep_arena().layout(slot), slots, idx))
            .collect();
        ValueRepr::Tuple { parts, present }
    }

    fn slot_repr(&mut self, slot: &RepLayout, slots: &[Idx<Inst>], idx: &mut usize) -> ValueRepr {
        match slot {
            RepLayout::Opaque => {
                let v = slots[*idx];
                *idx += 1;
                ValueRepr::Scalar(v)
            }
            RepLayout::Leaves(vs) => Self::rebuild_leaves_at(vs, slots, idx),
            RepLayout::Tuple(inner) => self.tuple_from_slots(inner, slots, idx),
        }
    }

    /// Short argument lists are nil-filled; caller and callee must agree on arity.
    pub(crate) fn arg_values_for_slot(
        &mut self,
        slot: &RepLayout,
        a: Idx<Expr>,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        self.arg_values_for_slot_lending(slot, a, true)
    }

    pub(crate) fn arg_values_for_slot_lending(
        &mut self,
        slot: &RepLayout,
        a: Idx<Expr>,
        lend: bool,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        if lend
            && !slot.splats()
            && let Expr::Var(b) = self.body.expr_arena[a]
            && self.boxed_value_local(b, a).is_some()
            && self.binding_value_is_shareable(b)
        {
            // The callee may retain an immutable box: a later mutable store copies it,
            // and a `ref` borrow copies its parameter on entry.
            return Ok(vec![self.emit_cur(Inst::Load(self.binds[&raw(b)]))]);
        }
        self.with_value_lending(lend, |s| {
            let r = match s.json_vec_argument(a, slot)? {
                Some(value) => value,
                None => s.lower_expr_repr(a)?,
            };
            s.snapshot_argument_slot(r, slot)
        })
    }

    pub(crate) fn snapshot_argument_slot(
        &mut self,
        r: ValueBundle,
        slot: &RepLayout,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        let representation = r.representation();
        let value = r.into_value();
        let owned = match slot {
            RepLayout::Leaves(shape) => self.own_opaque_parameter_children(value, shape)?,
            _ => self.owned_value_children(value)?,
        };
        let r = ValueBundle::new(representation, owned);
        if !slot.splats() {
            return Ok(vec![self.pack_bundle(&r)?]);
        }
        self.repr_to_slot_values_bundle(&r, slot)
    }

    fn is_ref_call_arg(&self, call: Idx<Expr>, arg: Idx<Expr>) -> bool {
        self.inf
            .and_then(|inf| inf.ref_call_args.get(&call))
            .is_some_and(|args| args.contains(&arg))
            || self.is_flat_ref_call_arg(arg)
    }

    pub(crate) fn arg_values_for_call_slot(
        &mut self,
        call: Idx<Expr>,
        slot: &RepLayout,
        arg: Idx<Expr>,
        lend: bool,
    ) -> Result<Vec<Idx<Inst>>, CompilerError> {
        if self.is_ref_call_arg(call, arg) {
            return Ok(vec![self.lower_ref_borrow(arg)?]);
        }
        self.arg_values_for_slot_lending(slot, arg, lend)
    }
}

/// A nullable parameter with a non-null default treats explicit null as a supplied value.
pub fn param_null_overrides_default(ws: &Workspace, body: &Body, index: usize, ty: TyId) -> bool {
    let Some(Some(default)) = body.param_defaults.get(index) else {
        return false;
    };
    if matches!(body.expr_arena[*default], Expr::Null) {
        return false;
    }
    match ty {
        TyId::Static(mid, idx) => matches!(ws.module(mid).ty_arena[idx], TypeExpr::Nullable(_)),
        // Declared parameter types are lowered into the module's static arena.
        TyId::Local(_) => false,
    }
}

pub(crate) fn ty_node(ws: &Workspace, inf: &BodyInference, ty: TyId) -> Option<TypeExpr> {
    match ty {
        TyId::Static(mid, idx) => Some(ws.module(mid).ty_arena[idx]),
        TyId::Local(idx) => Some(inf.ty_arena[idx]),
    }
}

fn resolve_tyid(ws: &Workspace, inf: &BodyInference, ty: TyId) -> Option<TyId> {
    match ty_node(ws, inf, ty)? {
        TypeExpr::Infer(var, _) => resolve_tyid(ws, inf, inf.substitutions.get(var)?),
        _ => Some(ty),
    }
}

fn map_key_rejection_reason(
    ws: &Workspace,
    inf: &BodyInference,
    k: TyId,
    depth: u32,
) -> Option<String> {
    // A malformed self-wrapping newtype chain must not spin.
    if depth > 8 {
        return None;
    }
    match ty_node(ws, inf, k)? {
        TypeExpr::Builtin(
            BuiltinTy::Int
            | BuiltinTy::Uint
            | BuiltinTy::Float
            | BuiltinTy::F32
            | BuiltinTy::Bool
            | BuiltinTy::String
            | BuiltinTy::Any
            | BuiltinTy::Unit
            | BuiltinTy::Never
            | BuiltinTy::Error,
        ) => None,
        // A type parameter or an uninstantiated var is undecidable here.
        TypeExpr::Generic(..) | TypeExpr::Infer(..) => None,
        TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            ..
        } => {
            if let Some(nid) = ws.nominals.newtype_id(name) {
                let wrapped = ws.modules[nid.module.index()].newtypes[nid.def].wrapped;
                let wrapped = resolve_tyid(ws, inf, wrapped)?;
                return map_key_rejection_reason(ws, inf, wrapped, depth + 1).map(|why| {
                    format!("`{name}` wraps {why}, and a newtype is keyed as what it wraps")
                });
            }
            // A payload-free enum lowers to an int ordinal; one payload variant makes the
            // type cdata- or table-carrying.
            if ws.nominals.enum_id(name).is_some() {
                return (!is_payload_free_enum(ws, name)).then(|| {
                    format!(
                        "`{name}` has payload-carrying variants, keyed by cdata or object \
                             identity (a payload-free enum is an int ordinal and stays allowed)"
                    )
                });
            }
            if is_value_struct(ws, name) {
                Some(format!(
                    "`{name}` is a `value` struct, keyed by cdata identity"
                ))
            } else {
                Some(format!("`{name}` is a struct, keyed by object identity"))
            }
        }
        TypeExpr::Tuple { .. } => {
            Some("a tuple is an aggregate, keyed by object identity".to_string())
        }
        TypeExpr::App {
            ctor: TypeCtor::Array,
            ..
        } => Some("an array is an aggregate, keyed by object identity".to_string()),
        TypeExpr::App {
            ctor: TypeCtor::Map,
            ..
        } => Some("a `Map` is an aggregate, keyed by object identity".to_string()),
        TypeExpr::App {
            ctor: TypeCtor::Ref,
            ..
        } => Some("a frame reference cannot be stored as a map key".to_string()),
        TypeExpr::Nullable(..) => {
            Some("a nullable key cannot be distinguished from a missing entry".to_string())
        }
        TypeExpr::Function { .. } => Some("a function is keyed by identity".to_string()),
    }
}

fn map_key_rejected(ws: &Workspace, inf: &BodyInference, ty: TyId) -> Option<String> {
    let ty = resolve_tyid(ws, inf, ty)?;
    let TypeExpr::App {
        ctor: TypeCtor::Map,
        params,
    } = ty_node(ws, inf, ty)?
    else {
        return None;
    };
    let k = resolve_tyid(ws, inf, ty.with_idx(params.into_iter().next()?))?;
    map_key_rejection_reason(ws, inf, k, 0)
}

pub fn map_key_rejection(ws: &Workspace, mid: ModuleId) -> Option<(String, Span)> {
    let module = ws.module(mid);
    let ir = ws.inference(mid)?;
    for (fid, _f) in module.functions.iter() {
        let owner = BodyOwner::Function(fid);
        let (Some(body), Some(inf)) = (module.bodies.get(&owner), ir.per_fn.get(&owner)) else {
            continue;
        };
        for (e, tyid) in inf.expr_types.iter() {
            if let Some(why) = map_key_rejected(ws, inf, *tyid) {
                return Some((why, body.expr_spans[e]));
            }
        }
    }
    None
}

/// A GC-carrying `value` struct has no `ffi.cdef` typedef, so it cannot cross
/// a C boundary.
fn nominal_is_gc_value(ws: &Workspace, name: Ustr) -> bool {
    is_value_struct(ws, name)
        && !matches!(
            crate::lower::declared_value_storage(ws, bone_core::hir::FnBackend::Lua, name),
            Some(ValueStorage::CData)
        )
}

fn gc_value_under_newtypes(
    ws: &Workspace,
    ty: MonoTy,
) -> Result<Option<Ustr>, crate::monoty::NewtypeResolutionError> {
    let ty = crate::monoty::resolve_newtype_chain(ws, ty)?;
    let MonoTy::App {
        ctor: MonoCtor::Nominal(name),
        ..
    } = ty
    else {
        return Ok(None);
    };
    Ok(nominal_is_gc_value(ws, name).then_some(name))
}

/// Extern C signatures require value structs that can cross a genuine C boundary.
pub fn gc_value_c_boundary(ws: &Workspace, mid: ModuleId) -> Option<(String, Span)> {
    let module = ws.module(mid);
    for (_, f) in module.functions.iter() {
        if !f.is_extern {
            continue;
        }
        let tys = f
            .param_decls
            .iter()
            .map(|p| (p.ty, false))
            .chain(f.return_ty.map(|ty| (ty, true)));
        for (ty, is_return) in tys {
            if matches!(
                static_ty(ws, ty),
                Some(TypeExpr::App {
                    ctor: TypeCtor::Nominal(_),
                    ..
                })
            ) {
                let normalized =
                    crate::monoty::convert_decl(ws, ty, &rustc_hash::FxHashMap::default())
                        .map_err(crate::monoty::NewtypeResolutionError::Conversion)
                        .and_then(|ty| gc_value_under_newtypes(ws, ty));
                match normalized {
                    Ok(Some(n)) => {
                        return Some((
                            format!(
                                "`{n}` is a GC-carrying `value` struct (immutable Lua table) and cannot \
                             cross an `extern` C boundary -- it has no C layout. Use an FFI-clean \
                             value struct (fields int/float/bool or nested FFI-clean value structs), \
                             or pass the fields individually."
                            ),
                            f.name_span,
                        ));
                    }
                    Ok(None) => {}
                    Err(reason) => {
                        let context = if is_return {
                            format!("extern `{}` returns {reason}", f.name)
                        } else {
                            format!("extern `{}` has a parameter type {reason}", f.name)
                        };
                        return Some((
                            format!(
                                "{context}, which has no C representation. Declare a concrete type."
                            ),
                            f.name_span,
                        ));
                    }
                }
            }
        }
    }
    None
}

fn resolve_call_fn(
    ws: &Workspace,
    inf: &BodyInference,
    e: Idx<Expr>,
    name: Ustr,
) -> Option<(ModuleId, Idx<Function>)> {
    if let Some(&r) = inf.resolved_overload_calls.get(&e) {
        return Some(r);
    }
    unique_fn(ws, name)
}

fn unique_fn(ws: &Workspace, name: Ustr) -> Option<(ModuleId, Idx<Function>)> {
    match ws.functions_by_name.get(&name).map(|l| l.as_slice()) {
        Some([single]) => Some(*single),
        _ => None,
    }
}

fn expansion_calls<'a>(
    ws: &'a Workspace,
    body: &'a Body,
    inf: Option<&'a BodyInference>,
) -> impl Iterator<Item = (Idx<Expr>, &'a Function, IdxRange<Expr>, ExpandKind)> {
    body.expr_arena.iter().filter_map(move |(e, expr)| {
        let Expr::Call {
            callee: Callee::Unresolved { name, .. },
            args,
            ..
        } = expr
        else {
            return None;
        };
        let (fmod, fid) = match inf {
            Some(inf) => resolve_call_fn(ws, inf, e, *name),
            None => unique_fn(ws, *name),
        }?;
        let f = &ws.module(fmod).functions[fid];
        Some((e, f, *args, crate::intrinsics::expand_kind(f)?))
    })
}

/// Whether call `e` to `name` is a `rock_shader` expansion. Its arguments name
/// stage entries for GLSL; the call collapses to a key, so they are no values.
pub fn is_shader_pipeline_call(
    ws: &Workspace,
    inf: &BodyInference,
    e: Idx<Expr>,
    name: Ustr,
) -> bool {
    resolve_call_fn(ws, inf, e, name).is_some_and(|(m, f)| {
        crate::intrinsics::expand_kind(&ws.module(m).functions[f])
            == Some(crate::intrinsics::ExpandKind::ShaderPipeline)
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShaderPair {
    pub key: String,
    pub vert: Option<FuncRef>,
    pub frag: FuncRef,
}

pub struct ShaderCollection {
    pub pairs: Vec<ShaderPair>,
    pub callsites: Vec<(ModuleId, BodyOwner)>,
    pub contributions: std::collections::HashMap<(ModuleId, BodyOwner), Vec<ShaderPair>>,
}

/// Whether `f` is a `@shader` stage entry: GLSL text, not a CPU root.
pub fn is_shader_entry(f: &Function) -> bool {
    shader_stage_of(f).is_some()
}

/// skull-mir must not depend on skull-codegen-glsl, which has the same helper.
fn shader_stage_of(f: &Function) -> Option<&'static str> {
    let ann = f.annotations.iter().find(|a| a.is_builtin("shader"))?;
    match ann.args.first().map(|a| &a.value) {
        Some(bone_core::hir::AnnoValue::Ident(id)) if id.name.as_str() == "vertex" => {
            Some("vertex")
        }
        Some(bone_core::hir::AnnoValue::Ident(id)) if id.name.as_str() == "fragment" => {
            Some("fragment")
        }
        _ => None,
    }
}

fn resolve_shader_arg(
    ws: &Workspace,
    body: &Body,
    arg: Idx<Expr>,
    want_stage: &str,
) -> Result<FuncRef, CompilerError> {
    let span = body.expr_spans[arg];
    let reject = |msg: String| CompilerError::new_with_label(span, msg);
    let Expr::Global(name) = body.expr_arena[arg] else {
        return Err(reject(format!(
            "`rock_shader` argument must be a direct reference to a top-level `@shader({want_stage})` fn"
        )));
    };
    let Some((module, func)) = unique_fn(ws, name) else {
        return Err(reject(format!(
            "`rock_shader` argument `{name}` is not a unique top-level fn"
        )));
    };
    let fref = FuncRef { module, func };
    let f = &ws.module(fref.module).functions[fref.func];
    match shader_stage_of(f) {
        Some(stage) if stage == want_stage => Ok(fref),
        Some(other) => Err(reject(format!(
            "`rock_shader` expects a `@shader({want_stage})` fn here, but `{name}` is `@shader({other})`"
        ))),
        None => Err(reject(format!(
            "`rock_shader` argument `{name}` is not a `@shader` fn"
        ))),
    }
}

pub(crate) fn resolve_shader_pair(
    ws: &Workspace,
    body: &Body,
    call_expr: Idx<Expr>,
    args: &[Idx<Expr>],
) -> Result<ShaderPair, CompilerError> {
    match args {
        [frag_arg] => {
            let frag = resolve_shader_arg(ws, body, *frag_arg, "fragment")?;
            let fname = ws.module(frag.module).functions[frag.func].name;
            // Bare frag name -- no '+', so it can never collide with a pair key.
            Ok(ShaderPair {
                key: bone_core::modules::artifact_name(fname.as_str()).to_string(),
                vert: None,
                frag,
            })
        }
        [vert_arg, frag_arg] => {
            let vert = resolve_shader_arg(ws, body, *vert_arg, "vertex")?;
            let frag = resolve_shader_arg(ws, body, *frag_arg, "fragment")?;
            let vname = ws.module(vert.module).functions[vert.func].name;
            let fname = ws.module(frag.module).functions[frag.func].name;
            Ok(ShaderPair {
                key: format!(
                    "{}+{}",
                    bone_core::modules::artifact_name(vname.as_str()),
                    bone_core::modules::artifact_name(fname.as_str())
                ),
                vert: Some(vert),
                frag,
            })
        }
        _ => Err(CompilerError::new_with_label(
            body.expr_spans[call_expr],
            "`rock_shader` takes either one argument -- rock_shader(fragment_fn) \
             (fullscreen) -- or two -- rock_shader(vertex_fn, fragment_fn)"
                .to_string(),
        )),
    }
}

enum UniformExpansion {
    Fingerprint(Idx<Expr>),
    Write(Idx<Expr>, Idx<Expr>, Idx<Expr>),
}

fn uniform_expand_signature(
    ws: &Workspace,
    span: Span,
    f: &Function,
    args: &[Idx<Expr>],
) -> Result<UniformExpansion, CompilerError> {
    Ok(match args {
        [u] if f.return_ty.and_then(|ret| static_ty(ws, ret))
            == Some(TypeExpr::Builtin(BuiltinTy::Int)) =>
        {
            UniformExpansion::Fingerprint(*u)
        }
        [fv, iv, u] => UniformExpansion::Write(*fv, *iv, *u),
        _ => {
            return Err(CompilerError::new_with_label(
                span,
                uniform_expand_arity_msg(f.name),
            ));
        }
    })
}

fn uniform_values_rejection_for_call(
    ws: &Workspace,
    inf: &BodyInference,
    body: &Body,
    call: Idx<Expr>,
    f: &Function,
    args: &[Idx<Expr>],
) -> Option<CompilerError> {
    let fname = f.name;
    let [_u] = args else {
        return Some(CompilerError::new_with_label(
            body.expr_spans[call],
            format!("`@uniform_values` fn `{fname}` takes exactly one uniform group"),
        ));
    };
    let Some(&ret_ty) = inf.expr_types.get(call) else {
        return Some(CompilerError::new_with_label(
            body.expr_spans[call],
            format!("`@uniform_values` fn `{fname}` has no inferred packet result"),
        ));
    };
    let Some(ret_ty) = resolve_tyid(ws, inf, ret_ty) else {
        return Some(CompilerError::new_with_label(
            body.expr_spans[call],
            format!("`@uniform_values` fn `{fname}` has no concrete packet result"),
        ));
    };
    let Some(TypeExpr::App {
        ctor: TypeCtor::Nominal(block_name),
        ..
    }) = static_ty(ws, ret_ty)
    else {
        return Some(CompilerError::new_with_label(
            body.expr_spans[call],
            format!("`@uniform_values` fn `{fname}` must return the canonical packet struct"),
        ));
    };
    if let Err(msg) = uniform_values_return_shape(ws, block_name) {
        return Some(CompilerError::new_with_label(
            body.expr_spans[call],
            format!("`@uniform_values`: {msg}"),
        ));
    }
    None
}

pub(crate) fn resolve_comptime_value(
    ws: &Workspace,
    body: &Body,
    call_expr: Idx<Expr>,
    args: &[Idx<Expr>],
    kind: ExpandKind,
) -> Result<ResolvedComptime, CompilerError> {
    let fname = match kind {
        ExpandKind::BuildVar => "build_var",
        ExpandKind::EmbedString => "embed_string",
        _ => unreachable!("resolve_comptime_value only serves the comptime-input kinds"),
    };
    let reject =
        |span: Idx<Expr>, msg: String| CompilerError::new_with_label(body.expr_spans[span], msg);
    let [arg] = args else {
        return Err(reject(
            call_expr,
            format!("`{fname}` takes exactly one argument"),
        ));
    };
    let Expr::String(key) = body.expr_arena[*arg] else {
        return Err(reject(
            *arg,
            format!("`{fname}` argument must be a plain string literal (no interpolation)"),
        ));
    };
    match kind {
        ExpandKind::BuildVar => match ws.comptime_inputs.vars.get(&key) {
            Some(var) => Ok(ResolvedComptime {
                value: var.value.clone(),
                file_abs: None,
            }),
            // Fully-default inputs mean resolution never ran, so do not blame
            // the project's [build.vars].
            None if ws.comptime_inputs.embed_root.is_none()
                && ws.comptime_inputs.vars.is_empty()
                && ws.comptime_inputs.channel.is_empty() =>
            {
                Err(reject(
                    call_expr,
                    format!(
                        "`build_var(\"{key}\")`: no [build.vars] were resolved for this \
                         compile (no stick.toml found at or above the entry point, or \
                         this rock entry point did not resolve declared inputs)"
                    ),
                ))
            }
            None => Err(reject(
                call_expr,
                format!(
                    "`build_var(\"{key}\")`: {key} is not declared in this project's [build.vars]"
                ),
            )),
        },
        ExpandKind::EmbedString => {
            // Resolve against the nearest stick.toml at or above the calling file, so a
            // library embeds its own files; synthetic files fall back to embed_root.
            let call_file = body.expr_spans[call_expr].file;
            let files = &ws.comptime_inputs.embed_files;
            if bone_core::modules::is_stdlib_module_file(call_file.as_str()) {
                let (file, value) = files.read_stdlib(key.as_str()).map_err(|e| {
                    reject(
                        *arg,
                        format!("`embed_string` cannot read stdlib asset {key:?}: {e:?}"),
                    )
                })?;
                return Ok(ResolvedComptime {
                    value,
                    file_abs: file.as_deref().map(crate::names::ustr),
                });
            }
            let package_root = files.package_root(call_file.as_str());
            let Some(root) = package_root.or_else(|| ws.comptime_inputs.embed_root.clone()) else {
                return Err(reject(
                    call_expr,
                    "`embed_string` needs a package root (a stick.toml at or above the \
                     calling file, or a project root); none is available in this compilation"
                        .to_string(),
                ));
            };
            let rel = std::path::Path::new(key.as_str());
            if rel.is_absolute() {
                return Err(reject(
                    *arg,
                    format!(
                        "`embed_string` path must be relative to the package root, got {key:?}"
                    ),
                ));
            }
            use bone_core::comptime_files::EmbedReadError;
            let (file, value) = files.read(&root, rel).map_err(|e| match e {
                EmbedReadError::Unreadable(e) => reject(
                    *arg,
                    format!("`embed_string(\"{key}\")`: cannot read {key:?}: {e}"),
                ),
                EmbedReadError::RootUnreadable(e) => reject(
                    call_expr,
                    format!("`embed_string`: package root is unreadable: {e}"),
                ),
                EmbedReadError::EscapesRoot => reject(
                    *arg,
                    format!("`embed_string(\"{key}\")`: path escapes the package root"),
                ),
            })?;
            Ok(ResolvedComptime {
                value,
                file_abs: Some(crate::names::ustr(&file)),
            })
        }
        _ => unreachable!(),
    }
}

pub(crate) struct ResolvedComptime {
    pub value: String,
    /// The path invalidation re-reads and the watcher watches.
    pub file_abs: Option<Ustr>,
}

/// Fatal gate for `build_var` / `embed_string` misuse, run even for bodies that
/// never lower.
pub fn comptime_input_rejection(ws: &Workspace, mid: ModuleId) -> Option<CompilerError> {
    let module = ws.module(mid);
    let ir = ws.inference(mid)?;
    for (owner, body) in module.bodies.iter() {
        let Some(inf) = ir.per_fn.get(owner) else {
            continue;
        };
        for (e, _, args, kind) in expansion_calls(ws, body, Some(inf)) {
            if !matches!(kind, ExpandKind::BuildVar | ExpandKind::EmbedString) {
                continue;
            }
            let arg_idxs: Vec<Idx<Expr>> = args.into_iter().collect();
            if let Err(err) = resolve_comptime_value(ws, body, e, &arg_idxs, kind) {
                return Some(err);
            }
        }
    }
    None
}

fn vertex_expand_signature(
    ws: &Workspace,
    span: Span,
    f: &Function,
    args: &[Idx<Expr>],
) -> Result<(Idx<Expr>, VertexExpandRet), CompilerError> {
    let fname = f.name;
    let [arg] = args else {
        return Err(CompilerError::new_with_label(
            span,
            format!("`@vertex_expand` fn `{fname}` takes exactly one argument"),
        ));
    };
    let kind = f
        .return_ty
        .and_then(|ret| vertex_expand_return(ws, ret))
        .ok_or_else(|| {
            CompilerError::new_with_label(span, vertex_expand_ret_msg(ws, fname, f.return_ty))
        })?;
    Ok((*arg, kind))
}

/// Scan module `mid` once for every direct expansion call. Within a category
/// the first rejection wins, and categories keep their fixed priority.
pub fn expansion_rejection(ws: &Workspace, mid: ModuleId) -> Option<CompilerError> {
    let ir = ws.inference(mid)?;
    let mut first = None::<(usize, CompilerError)>;
    for (owner, body) in ws.module(mid).bodies.iter() {
        let Some(inf) = ir.per_fn.get(owner) else {
            continue;
        };
        for (e, f, args, kind) in expansion_calls(ws, body, Some(inf)) {
            let priority = match kind {
                ExpandKind::ShaderPipeline => 1,
                ExpandKind::VertexLayout => 2,
                ExpandKind::UniformBlock => 3,
                ExpandKind::UniformValues => 4,
                _ => continue,
            };
            if first.as_ref().is_some_and(|(rank, _)| *rank <= priority) {
                continue;
            }
            let args: Vec<_> = args.into_iter().collect();
            let span = body.expr_spans[e];
            let rejection = match kind {
                ExpandKind::ShaderPipeline => resolve_shader_pair(ws, body, e, &args).err(),
                ExpandKind::VertexLayout => vertex_expand_signature(ws, span, f, &args).err(),
                ExpandKind::UniformBlock => uniform_expand_signature(ws, span, f, &args).err(),
                ExpandKind::UniformValues => {
                    uniform_values_rejection_for_call(ws, inf, body, e, f, &args)
                }
                _ => unreachable!("only ranked expansions have a preflight check"),
            };
            if let Some(error) = rejection {
                first = Some((priority, error));
            }
        }
    }
    first.map(|(_, error)| error)
}

/// Every distinct `rock_shader` pair, deduped and sorted by key for
/// byte-stable output.
pub fn collect_shader_pairs(ws: &Workspace) -> Result<Vec<ShaderPair>, CompilerError> {
    Ok(collect_shader_collection(ws)?.pairs)
}

pub fn body_has_shader_pipeline_call(ws: &Workspace, mid: ModuleId, owner: BodyOwner) -> bool {
    let module = ws.module(mid);
    let Some(body) = module.bodies.get(&owner) else {
        return false;
    };
    let inf = ws.inference(mid).and_then(|ir| ir.per_fn.get(&owner));
    expansion_calls(ws, body, inf).any(|(_, _, _, kind)| kind == ExpandKind::ShaderPipeline)
}

pub fn collect_shader_body(
    ws: &Workspace,
    mid: ModuleId,
    owner: BodyOwner,
) -> Result<(Vec<ShaderPair>, bool), CompilerError> {
    let module = ws.module(mid);
    let Some(body) = module.bodies.get(&owner) else {
        return Ok((Vec::new(), false));
    };
    let inf = ws.inference(mid).and_then(|ir| ir.per_fn.get(&owner));
    let mut by_key = BTreeMap::new();
    let mut callsite = false;
    for (e, _, args, kind) in expansion_calls(ws, body, inf) {
        if kind != ExpandKind::ShaderPipeline {
            continue;
        }
        callsite = true;
        let arg_idxs: Vec<Idx<Expr>> = args.into_iter().collect();
        let pair = resolve_shader_pair(ws, body, e, &arg_idxs)?;
        by_key.entry(pair.key.clone()).or_insert(pair);
    }
    Ok((by_key.into_values().collect(), callsite))
}

pub fn collect_shader_collection(ws: &Workspace) -> Result<ShaderCollection, CompilerError> {
    let mut by_key: BTreeMap<String, ShaderPair> = BTreeMap::new();
    let mut callsites: HashSet<(ModuleId, BodyOwner)> = HashSet::default();
    let mut contributions = std::collections::HashMap::new();
    let keys: Vec<(ModuleId, BodyOwner)> = ws
        .module_ids
        .values()
        .flat_map(|&mid| ws.module(mid).bodies.keys().map(move |owner| (mid, *owner)))
        .collect();
    // The fold keeps serial order, so the first registration of a key and the
    // first error are the same ones.
    let scanned: Vec<_> = {
        use bone_core::prelude::*;
        keys.par_iter()
            .map(|&(mid, owner)| collect_shader_body(ws, mid, owner))
            .collect()
    };
    for (key, scanned) in keys.into_iter().zip(scanned) {
        let (pairs, callsite) = scanned?;
        if callsite {
            callsites.insert(key);
        }
        for pair in &pairs {
            by_key
                .entry(pair.key.clone())
                .or_insert_with(|| pair.clone());
        }
        contributions.insert(key, pairs);
    }
    Ok(ShaderCollection {
        pairs: by_key.into_values().collect(),
        callsites: callsites.into_iter().collect(),
        contributions,
    })
}

/// Reject CPU-side calls to `@gpu_only` intrinsics and GPU functions used as
/// runtime fn values. Shader-pipeline expansion arguments are names, not values.
pub fn gpu_only_call_rejection(ws: &Workspace, mid: ModuleId) -> Option<CompilerError> {
    let module = ws.module(mid);
    let ir = ws.inference(mid)?;
    for (owner, body) in module.bodies.iter() {
        let Some(inf) = ir.per_fn.get(owner) else {
            continue;
        };
        let caller_is_gpu = match owner {
            BodyOwner::Function(fid) => {
                let function = &module.functions[*fid];
                crate::intrinsics::is_gpu_context(function)
            }
            _ => false,
        };
        if caller_is_gpu {
            continue;
        }
        let shader_expansion_args: HashSet<_> = expansion_calls(ws, body, Some(inf))
            .filter(|(_, _, _, kind)| *kind == ExpandKind::ShaderPipeline)
            .flat_map(|(_, _, args, _)| args)
            .collect();
        for (e, expr) in body.expr_arena.iter() {
            match expr {
                Expr::Call {
                    callee: Callee::Unresolved { name, .. },
                    ..
                } => {
                    let Some((fmod, fid)) = resolve_call_fn(ws, inf, e, *name) else {
                        continue;
                    };
                    let f = &ws.module(fmod).functions[fid];
                    if !crate::intrinsics::is_gpu_only(f) {
                        continue;
                    }
                    let fname = f.name;
                    return Some(CompilerError::new_with_label(
                        body.expr_spans[e],
                        format!(
                            "`{fname}` is GPU-only and cannot be called from CPU (Lua) code. \
                             Shader helper code that calls `{fname}` must annotate its fn \
                             `@shader(...)` or `@gpu_only`."
                        ),
                    ));
                }
                Expr::Global(name) if !shader_expansion_args.contains(&e) => {
                    let Some((fmod, fid)) = resolve_call_fn(ws, inf, e, *name) else {
                        continue;
                    };
                    let f = &ws.module(fmod).functions[fid];
                    if !crate::intrinsics::is_gpu_context(f) {
                        continue;
                    }
                    let fname = f.name;
                    let kind = if crate::intrinsics::is_gpu_only(f) {
                        "GPU-only"
                    } else {
                        "a shader entry"
                    };
                    return Some(CompilerError::new_with_label(
                        body.expr_spans[e],
                        format!(
                            "`{fname}` is {kind} and cannot be used as a fn value from CPU \
                             (Lua) code"
                        ),
                    ));
                }
                _ => {}
            }
        }
    }
    None
}

fn const_int(body: &Body, e: Idx<Expr>) -> Option<i64> {
    match body.expr_arena[e] {
        Expr::Int(v) => Some(v),
        Expr::UnaryMinus(inner, _) => match body.expr_arena[inner] {
            Expr::Int(v) => Some(-v),
            _ => None,
        },
        _ => None,
    }
}

pub fn string_char_literal_out_of_range(ws: &Workspace, mid: ModuleId) -> Option<Span> {
    let module = ws.module(mid);
    for body in module.bodies.values() {
        for (_e, expr) in body.expr_arena.iter() {
            let Expr::Call {
                callee: Callee::Unresolved { name, .. },
                args,
                ..
            } = *expr
            else {
                continue;
            };
            if name.as_str() != "string_char" {
                continue;
            }
            let Some(arg) = args.into_iter().next() else {
                continue;
            };
            let Some(code) = const_int(body, arg) else {
                continue;
            };
            if !(0..=127).contains(&code) {
                return Some(body.expr_spans[arg]);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests;

#[derive(Clone, Copy)]
pub(crate) enum GroupLeaf {
    Cell(Idx<Cell>),
    Value(Idx<Inst>),
}

impl GroupLeaf {
    /// Only a never-written binding holds its leaves as values, so a write always
    /// finds cells.
    pub(crate) fn cell(self) -> Idx<Cell> {
        match self {
            Self::Cell(cell) => cell,
            Self::Value(_) => unreachable!("a written binding keeps its leaf cells"),
        }
    }
}

#[derive(Default)]
pub(crate) struct BodyFacts {
    /// A lambda or `defer` may read bindings from another function body.
    may_capture: bool,
    written: HashSet<u32>,
    /// `None` for a whole use, otherwise the directly projected fields.
    projected: HashMap<u32, Option<Vec<Ustr>>>,
}

impl BodyFacts {
    fn of(body: &Body) -> Self {
        let mut facts = BodyFacts {
            may_capture: body
                .statement_arena
                .iter()
                .any(|(_, s)| matches!(s, Statement::Defer(_))),
            written: HashSet::default(),
            projected: HashMap::default(),
        };
        let mut projected_bases = HashSet::default();
        for (_, e) in body.expr_arena.iter() {
            if let Expr::FieldAccess { base, field } | Expr::SafeNav { base, field } = e
                && let Expr::Var(binding) = body.expr_arena[*base]
            {
                projected_bases.insert(*base);
                let fields = facts
                    .projected
                    .entry(raw(binding))
                    .or_insert_with(|| Some(Vec::new()));
                if let Some(fields) = fields
                    && !fields.contains(field)
                {
                    fields.push(*field);
                }
            }
        }
        for (id, e) in body.expr_arena.iter() {
            match e {
                Expr::Var(binding) if !projected_bases.contains(&id) => {
                    facts.projected.insert(raw(*binding), None);
                }
                Expr::Call {
                    callee: Callee::Local(binding),
                    ..
                } => {
                    facts.projected.insert(raw(*binding), None);
                }
                _ => {}
            }
            let target = match e {
                Expr::Lambda { .. } => {
                    facts.may_capture = true;
                    continue;
                }
                Expr::Assign { target, .. }
                | Expr::CompoundAssign { target, .. }
                | Expr::IncDec { target, .. } => *target,
                _ => continue,
            };
            let mut at = target;
            loop {
                match &body.expr_arena[at] {
                    Expr::Var(b) => {
                        facts.written.insert(raw(*b));
                        break;
                    }
                    Expr::FieldAccess { base, .. } | Expr::Index { base, .. } => at = *base,
                    Expr::TupleIndex { tuple, .. } => at = *tuple,
                    _ => break,
                }
            }
        }
        facts
    }
}
