use std::collections::{HashMap, HashSet, VecDeque};

use bone_semantic::prelude::{ArenaMap, SmallVec, Span};
use comfy_la_arena::Idx;
use ustr::Ustr;

use crate::mir::{Cell, FuncRef, Inst, Lit, MirFunc, Term};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Place {
    Pending,
    Static,
    Session,
    Frame,
    Environment,
    Destination,
    Param(u16),
    Runtime(Idx<Inst>),
    /// A borrowed runtime value whose concrete owner is one of several
    /// runtime paths. It proves Frame lifetime but cannot select an allocator.
    RuntimeAny,
    Unknown,
}

impl Place {
    pub fn join(self, other: Self) -> Self {
        match (self, other) {
            (left, right) if left == right => left,
            (Self::Pending, other) | (other, Self::Pending) => other,
            (Self::Static, other) | (other, Self::Static) => other,
            (Self::Environment, Self::Frame) | (Self::Frame, Self::Environment) => {
                Self::Environment
            }
            (Self::Destination, other) | (other, Self::Destination) => other,
            (Self::Frame, Self::Session) | (Self::Session, Self::Frame) => Self::Frame,
            (Self::Session, Self::Param(index)) | (Self::Param(index), Self::Session) => {
                Self::Param(index)
            }
            (Self::Runtime(left), Self::Runtime(right)) => {
                if left == right { Self::Runtime(left) } else { Self::RuntimeAny }
            }
            (Self::RuntimeAny, Self::RuntimeAny)
            | (Self::RuntimeAny, Self::Runtime(_))
            | (Self::Runtime(_), Self::RuntimeAny)
            | (Self::RuntimeAny, Self::Frame)
            | (Self::Frame, Self::RuntimeAny) => Self::RuntimeAny,
            (Self::Unknown, _) | (_, Self::Unknown) => Self::Unknown,
            _ => Self::Unknown,
        }
    }

    fn substitute(self, args: &[Self], destination: Self) -> Self {
        match self {
            Self::Param(index) => args.get(index as usize).copied().unwrap_or(Self::Unknown),
            Self::Destination => destination,
            other => other,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoreEffect {
    pub destination: u16,
    pub value: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutlivesConstraint {
    pub value: Place,
    pub destination: Place,
    pub span: Span,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CallEffect {
    pub result: Option<Place>,
    pub owner: Option<Place>,
    pub stores: Vec<StoreEffect>,
    pub constraints: Vec<OutlivesConstraint>,
    pub opaque_retains: Vec<u16>,
    pub omitted_args: Vec<(u16, Place)>,
}

pub trait PlacementOracle {
    fn global_place(&self, name: Ustr) -> Place;

    fn global_store_requires_clone(&self, _name: Ustr) -> bool {
        false
    }

    fn parameter_derived_place(&self, _mir: &MirFunc, _inst: Idx<Inst>) -> Option<Place> {
        None
    }

    fn entity_place(&self, _mir: &MirFunc, _inst: Idx<Inst>) -> Option<Place> {
        None
    }

    fn entity_content_place(&self, _mir: &MirFunc, _inst: Idx<Inst>) -> Option<Place> {
        None
    }

    fn carries_placement(&self, mir: &MirFunc, inst: Idx<Inst>) -> bool;

    fn is_allocation_site(&self, mir: &MirFunc, inst: Idx<Inst>) -> bool;

    fn call_effect(
        &self,
        mir: &MirFunc,
        inst: Idx<Inst>,
        func: FuncRef,
        default_allocation: Place,
    ) -> CallEffect;

    fn call_value_effect(
        &self,
        _mir: &MirFunc,
        _inst: Idx<Inst>,
        _callee: Idx<Inst>,
        _default_allocation: Place,
    ) -> CallEffect {
        CallEffect::default()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnalysisContext {
    pub default_allocation: Place,
    pub return_destination: Option<Place>,
}

impl AnalysisContext {
    pub fn frame() -> Self {
        Self {
            default_allocation: Place::Frame,
            return_destination: Some(Place::Destination),
        }
    }

    pub fn session_initializer() -> Self {
        Self {
            default_allocation: Place::Session,
            return_destination: Some(Place::Session),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FunctionPlacement {
    pub result: Option<Place>,
    pub entity_result: Option<Place>,
    pub entity_content: Option<Place>,
    pub owner: Option<Place>,
    pub constraints: Vec<OutlivesConstraint>,
    pub default_params: Vec<Option<Place>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlacementViolationKind {
    DoesNotOutlive,
    UnknownProvenance,
    OpaqueRetention,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlacementViolation {
    pub value: Place,
    pub destination: Place,
    pub span: Span,
    pub kind: PlacementViolationKind,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlacementAnalysis {
    pub values: ArenaMap<Idx<Inst>, Place>,
    pub cells: ArenaMap<Idx<Cell>, Place>,
    cell_origins: ArenaMap<Idx<Cell>, Vec<Place>>,
    pub call_owners: ArenaMap<Idx<Inst>, Place>,
    pub summary: FunctionPlacement,
    pub violations: Vec<PlacementViolation>,
}

fn known_outlives(value: Place, destination: Place) -> Option<bool> {
    match (value, destination) {
        (Place::Pending, _) | (_, Place::Pending) => None,
        (Place::Static, _) => Some(true),
        (left, right) if left == right => Some(true),
        (Place::Unknown, _) | (_, Place::Unknown) => None,
        (Place::Param(_) | Place::Destination, _) | (_, Place::Param(_) | Place::Destination) => {
            None
        }
        (Place::Environment, Place::Frame) => Some(true),
        (Place::Environment, _) => None,
        (Place::Session, Place::Session | Place::Frame | Place::RuntimeAny) => Some(true),
        (Place::Session, Place::Runtime(_)) => Some(true),
        (Place::Runtime(_), Place::Frame) => Some(true),
        (Place::RuntimeAny, Place::Frame) => Some(true),
        (Place::Runtime(left), Place::Runtime(right)) => Some(left == right),
        (Place::Runtime(_), Place::RuntimeAny) | (Place::RuntimeAny, Place::Runtime(_)) => None,
        (Place::RuntimeAny, Place::RuntimeAny) => None,
        (Place::Frame, Place::Frame) => Some(true),
        _ => Some(false),
    }
}

fn merge_inst(map: &mut ArenaMap<Idx<Inst>, Place>, key: Idx<Inst>, value: Place) -> bool {
    let joined = map.get(key).copied().map_or(value, |old| old.join(value));
    if map.get(key).copied() == Some(joined) {
        return false;
    }
    map.insert(key, joined);
    true
}

fn merge_cell(map: &mut ArenaMap<Idx<Cell>, Place>, key: Idx<Cell>, value: Place) -> bool {
    let joined = map.get(key).copied().map_or(value, |old| old.join(value));
    if map.get(key).copied() == Some(joined) {
        return false;
    }
    map.insert(key, joined);
    true
}

fn merge_cell_origins(
    map: &mut ArenaMap<Idx<Cell>, Vec<Place>>,
    key: Idx<Cell>,
    origins: impl IntoIterator<Item = Place>,
) -> bool {
    let mut merged = map.get(key).cloned().unwrap_or_default();
    let mut changed = false;
    for origin in origins {
        if matches!(origin, Place::Pending) || merged.contains(&origin) {
            continue;
        }
        merged.push(origin);
        changed = true;
    }
    if changed {
        map.insert(key, merged);
    }
    changed
}

fn merge_requirement<T>(map: &mut ArenaMap<Idx<T>, Place>, key: Idx<T>, value: Place) -> bool {
    let merged = merge_requirement_place(map.get(key).copied(), value);
    if map.get(key).copied() == Some(merged) {
        return false;
    }
    map.insert(key, merged);
    true
}

fn merge_requirement_place(old: Option<Place>, value: Place) -> Place {
    match (old, value) {
        (None, value) => value,
        (Some(old), value) if old == value => old,
        (Some(Place::Pending), value) | (Some(value), Place::Pending) => value,
        (Some(Place::Static), value) | (Some(value), Place::Static) => value,
        (Some(Place::Frame), Place::Session) | (Some(Place::Session), Place::Frame) => {
            Place::Session
        }
        (Some(Place::Frame), Place::Destination) | (Some(Place::Destination), Place::Frame) => {
            Place::Destination
        }
        (Some(Place::Frame), param @ Place::Param(_))
        | (Some(param @ Place::Param(_)), Place::Frame) => param,
        (Some(Place::Session), Place::Param(_)) | (Some(Place::Param(_)), Place::Session) => {
            Place::Session
        }
        (Some(Place::Destination), Place::Session) | (Some(Place::Session), Place::Destination) => {
            Place::Session
        }
        (Some(Place::Destination), param @ Place::Param(_))
        | (Some(param @ Place::Param(_)), Place::Destination) => param,
        (Some(Place::Session), Place::Runtime(_))
        | (Some(Place::Runtime(_)), Place::Session) => Place::Session,
        (Some(Place::RuntimeAny), Place::Session) | (Some(Place::Session), Place::RuntimeAny) => {
            Place::Unknown
        }
        (Some(Place::Frame), runtime @ Place::Runtime(_))
        | (Some(runtime @ Place::Runtime(_)), Place::Frame) => runtime,
        (Some(Place::Frame), Place::RuntimeAny)
        | (Some(Place::RuntimeAny), Place::Frame) => Place::RuntimeAny,
        (Some(Place::Destination), Place::Runtime(_))
        | (Some(Place::Runtime(_)), Place::Destination) => Place::Unknown,
        (Some(Place::Destination), Place::RuntimeAny)
        | (Some(Place::RuntimeAny), Place::Destination) => Place::Unknown,
        (Some(Place::RuntimeAny), Place::Param(_))
        | (Some(Place::Param(_)), Place::RuntimeAny) => Place::Unknown,
        (Some(Place::RuntimeAny), Place::Environment)
        | (Some(Place::Environment), Place::RuntimeAny) => Place::Unknown,
        (Some(Place::Runtime(left)), Place::Runtime(right)) => {
            if left == right { Place::Runtime(left) } else { Place::Unknown }
        }
        (Some(Place::RuntimeAny), Place::Runtime(_))
        | (Some(Place::Runtime(_)), Place::RuntimeAny)
        | (Some(Place::RuntimeAny), Place::RuntimeAny) => Place::RuntimeAny,
        (Some(Place::Param(left)), Place::Param(right)) => {
            if left == right { Place::Param(left) } else { Place::Unknown }
        }
        (Some(Place::Runtime(_) | Place::Param(_)), Place::Runtime(_) | Place::Param(_)) => {
            Place::Unknown
        }
        (Some(Place::Unknown), _) | (Some(_), Place::Unknown) => Place::Unknown,
        (Some(_), value) => value,
    }
}

#[derive(Clone, Default, PartialEq, Eq)]
struct PlacementRequirements {
    values: ArenaMap<Idx<Inst>, Place>,
    cells: ArenaMap<Idx<Cell>, Place>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RequirementKey {
    Value(Idx<Inst>),
    Cell(Idx<Cell>),
}

#[derive(Default)]
struct RequirementWorklist {
    queue: VecDeque<RequirementKey>,
    queued_values: ArenaMap<Idx<Inst>, bool>,
    queued_cells: ArenaMap<Idx<Cell>, bool>,
    cell_loads: ArenaMap<Idx<Cell>, Vec<Idx<Inst>>>,
    cell_multi_calls: ArenaMap<Idx<Cell>, Vec<Idx<Inst>>>,
    cell_stores: ArenaMap<Idx<Cell>, Vec<Idx<Inst>>>,
    call_users: ArenaMap<Idx<Inst>, Vec<Idx<Inst>>>,
}

impl RequirementWorklist {
    fn enqueue_value(&mut self, value: Idx<Inst>) {
        if !self.queued_values.get(value).copied().unwrap_or(false) {
            self.queued_values.insert(value, true);
            self.queue.push_back(RequirementKey::Value(value));
        }
        if let Some(users) = self.call_users.get(value).cloned() {
            for user in users {
                self.enqueue_value_only(user);
            }
        }
    }

    fn enqueue_value_only(&mut self, value: Idx<Inst>) {
        if !self.queued_values.get(value).copied().unwrap_or(false) {
            self.queued_values.insert(value, true);
            self.queue.push_back(RequirementKey::Value(value));
        }
    }

    fn enqueue_cell(&mut self, cell: Idx<Cell>) {
        if !self.queued_cells.get(cell).copied().unwrap_or(false) {
            self.queued_cells.insert(cell, true);
            self.queue.push_back(RequirementKey::Cell(cell));
        }
    }

    fn merge_value(
        &mut self,
        requirements: &mut PlacementRequirements,
        value: Idx<Inst>,
        place: Place,
    ) -> bool {
        let changed = merge_requirement(&mut requirements.values, value, place);
        if changed {
            self.enqueue_value(value);
        }
        changed
    }

    fn merge_cell(
        &mut self,
        requirements: &mut PlacementRequirements,
        cell: Idx<Cell>,
        place: Place,
    ) -> bool {
        let changed = merge_requirement(&mut requirements.cells, cell, place);
        if changed {
            self.enqueue_cell(cell);
        }
        changed
    }

    fn initialize(&mut self, mir: &MirFunc, blocks: &[Idx<crate::mir::Block>]) {
        for &block in blocks {
            for &id in &mir.blocks[block].insts {
                match &mir.insts[id] {
                    Inst::Load(cell) => {
                        self.cell_loads.entry(*cell).or_default().push(id);
                    }
                    Inst::Store { cell, .. } => {
                        self.cell_stores.entry(*cell).or_default().push(id);
                    }
                    Inst::Call { args, .. } | Inst::CallValue { args, .. } => {
                        for &arg in args {
                            self.call_users.entry(arg).or_default().push(id);
                        }
                    }
                    Inst::CallMulti { dests, args, .. }
                    | Inst::CallValueMulti { dests, args, .. } => {
                        for &dest in dests {
                            self.cell_multi_calls.entry(dest).or_default().push(id);
                        }
                        for &arg in args {
                            self.call_users.entry(arg).or_default().push(id);
                        }
                    }
                    _ => {}
                }
            }
        }
        for &block in blocks {
            for &id in &mir.blocks[block].insts {
                self.enqueue_value_only(id);
            }
        }
    }

    fn process_cell(&mut self, cell: Idx<Cell>) {
        if let Some(loads) = self.cell_loads.get(cell).cloned() {
            for load in loads {
                self.enqueue_value(load);
            }
        }
        if let Some(calls) = self.cell_multi_calls.get(cell).cloned() {
            for call in calls {
                self.enqueue_value(call);
            }
        }
        if let Some(stores) = self.cell_stores.get(cell).cloned() {
            for store in stores {
                self.enqueue_value(store);
            }
        }
    }
}

fn inferred_argument_place(
    mir: &MirFunc,
    requirements: &PlacementRequirements,
    oracle: &impl PlacementOracle,
    context: AnalysisContext,
    analysis: Option<&PlacementAnalysis>,
    arg: Idx<Inst>,
) -> Place {
    if let Some(place) = oracle.parameter_derived_place(mir, arg) {
        return place;
    }
    if let Some(analysis) = analysis {
        let place = match mir.insts[arg] {
            Inst::Load(cell) => analysis.cells.get(cell).copied(),
            _ => analysis.values.get(arg).copied(),
        };
        if let Some(place) = place.filter(|place| !matches!(place, Place::Pending)) {
            return place;
        }
    }
    match &mir.insts[arg] {
        Inst::Const(_) => Place::Static,
        Inst::Global(name) => oracle.global_place(*name),
        Inst::Load(cell) => mir
            .params
            .iter()
            .position(|param| *param == *cell)
            .and_then(|index| u16::try_from(index).ok())
            .map(Place::Param)
            .or_else(|| requirements.cells.get(*cell).copied())
            .unwrap_or(context.default_allocation),
        Inst::Field { base, .. }
        | Inst::TupleIndex { base, .. }
        | Inst::Index { base, .. }
        | Inst::Len { base, .. }
        | Inst::VecCheckedIndex { vec: base, .. } => {
            inferred_argument_place(mir, requirements, oracle, context, analysis, *base)
        }
        _ => requirements.values.get(arg).copied().unwrap_or_else(|| {
            if oracle.carries_placement(mir, arg) {
                context.default_allocation
            } else {
                Place::Static
            }
        }),
    }
}

fn contextual_argument_place(
    mir: &MirFunc,
    requirements: &PlacementRequirements,
    oracle: &impl PlacementOracle,
    analysis: Option<&PlacementAnalysis>,
    arg: Idx<Inst>,
) -> Option<Place> {
    let contextual = oracle.parameter_derived_place(mir, arg).or_else(|| match mir.insts[arg] {
        Inst::Global(name) => Some(oracle.global_place(name)),
        Inst::Load(cell) => mir
            .params
            .iter()
            .position(|param| *param == cell)
            .and_then(|index| u16::try_from(index).ok())
            .map(Place::Param)
            .or_else(|| requirements.cells.get(cell).copied()),
        Inst::Field { base, .. }
        | Inst::TupleIndex { base, .. }
        | Inst::Index { base, .. }
        | Inst::Len { base, .. }
        | Inst::VecCheckedIndex { vec: base, .. } => {
            contextual_argument_place(mir, requirements, oracle, analysis, base)
        }
        _ => requirements.values.get(arg).copied(),
    });
    let analyzed = analysis.and_then(|analysis| match mir.insts[arg] {
        Inst::Load(cell) => analysis.cells.get(cell).copied(),
        _ => analysis.values.get(arg).copied(),
    });
    match (contextual, analyzed) {
        (Some(contextual), Some(Place::Static)) if contextual != Place::Static => Some(contextual),
        (Some(contextual), Some(analyzed)) => {
            Some(merge_requirement_place(Some(contextual), analyzed))
        }
        (Some(place), None) | (None, Some(place)) => Some(place),
        (None, None) => None,
    }
    .filter(|place| !matches!(place, Place::Pending))
}

fn explicit_call_argument(
    mir: &MirFunc,
    args: &[Idx<Inst>],
    index: u16,
    omitted_args: &[(u16, Place)],
) -> Option<Idx<Inst>> {
    let arg = *args.get(index as usize)?;
    if omitted_args.iter().any(|(omitted, _)| *omitted == index)
        || matches!(mir.insts[arg], Inst::Const(Lit::Nil))
    {
        return None;
    }
    Some(arg)
}

fn is_call_result(mir: &MirFunc, inst: Idx<Inst>) -> bool {
    matches!(
        mir.insts[inst],
        Inst::Call { .. }
            | Inst::CallMulti { .. }
            | Inst::CallValue { .. }
            | Inst::CallValueMulti { .. }
    )
}

fn is_composite_constructor(inst: &Inst) -> bool {
    matches!(
        inst,
        Inst::MakeTuple(_)
            | Inst::MakeVec(_)
            | Inst::MakeVecColumns { .. }
            | Inst::MakeStruct { .. }
            | Inst::MakeValueStruct { .. }
            | Inst::MaterializeValueStruct { .. }
            | Inst::MakeEnv { .. }
            | Inst::MakeEnum { .. }
            | Inst::MakeEnumDyn { .. }
    )
}

fn requirement_flow_source(
    mir: &MirFunc,
    oracle: &impl PlacementOracle,
    parameter_cells: &HashSet<Idx<Cell>>,
    inst: Idx<Inst>,
) -> bool {
    if !oracle.carries_placement(mir, inst) {
        return false;
    }
    match mir.insts[inst] {
        Inst::Load(cell) => parameter_cells.contains(&cell),
        _ => {
            is_call_result(mir, inst)
                || (is_composite_constructor(&mir.insts[inst])
                    && !oracle.is_allocation_site(mir, inst))
        }
    }
}

fn carries_requirement(
    mir: &MirFunc,
    oracle: &impl PlacementOracle,
    flow_cells: &HashSet<Idx<Cell>>,
    inst: Idx<Inst>,
) -> bool {
    oracle.carries_placement(mir, inst)
        && (is_call_result(mir, inst)
            || is_composite_constructor(&mir.insts[inst])
            || matches!(mir.insts[inst], Inst::StringConcat(..))
            || matches!(mir.insts[inst], Inst::Load(cell) if flow_cells.contains(&cell)))
}

fn call_effect_consumes_argument(effect: &CallEffect, index: u16) -> bool {
    effect.stores.iter().any(|store| store.value == index)
        || effect.constraints.iter().any(|constraint| {
            matches!(constraint.value, Place::Param(value) if value == index)
        })
        || effect.result == Some(Place::Param(index))
        || effect.opaque_retains.contains(&index)
}

fn infer_call_requirements(
    mir: &MirFunc,
    context: AnalysisContext,
    oracle: &impl PlacementOracle,
    flow_cells: &HashSet<Idx<Cell>>,
    flow_calls: &HashSet<Idx<Inst>>,
    analysis: Option<&PlacementAnalysis>,
    requirements: &mut PlacementRequirements,
    call_effects: &mut HashMap<(Idx<Inst>, Place), CallEffect>,
    id: Idx<Inst>,
) -> bool {
    let raw_args = match &mir.insts[id] {
        Inst::Call { args, .. }
        | Inst::CallMulti { args, .. }
        | Inst::CallValue { args, .. }
        | Inst::CallValueMulti { args, .. } => args,
        _ => return false,
    };
    let mut call_requirement = requirements
        .values
        .get(id)
        .copied()
        .filter(|place| !matches!(place, Place::Pending));
    if let Inst::CallMulti { dests, .. } | Inst::CallValueMulti { dests, .. } = &mir.insts[id] {
        for &dest in dests {
            if let Some(place) = requirements
                .cells
                .get(dest)
                .copied()
                .filter(|place| !matches!(place, Place::Pending))
            {
                call_requirement = Some(merge_requirement_place(call_requirement, place));
            }
        }
    }
    let direct_requirement = raw_args
        .iter()
        .any(|arg| carries_requirement(mir, oracle, flow_cells, *arg));
    let flow_argument = flow_calls.contains(&id);
    let allocation = call_requirement.unwrap_or(context.default_allocation);
    let effect = call_effects.entry((id, allocation)).or_insert_with(|| {
        match mir.insts[id] {
            Inst::Call { func, .. } | Inst::CallMulti { func, .. } => {
                oracle.call_effect(mir, id, func, allocation)
            }
            Inst::CallValue { callee, .. } | Inst::CallValueMulti { callee, .. } => {
                oracle.call_value_effect(mir, id, callee, allocation)
            }
            _ => unreachable!("call requirements requested for a non-call"),
        }
    });
    let unconditional_retention = !effect.opaque_retains.is_empty()
        || effect.constraints.iter().any(|constraint| {
            matches!(constraint.value, Place::Param(_) | Place::Destination)
                && !matches!(
                    constraint.destination,
                    Place::Param(_) | Place::Destination | Place::Pending
                )
        });
    let contextual_store = effect.stores.iter().any(|store| {
        let Some(destination) = raw_args.get(usize::from(store.destination)) else {
            return false;
        };
        let Some(value) = raw_args.get(usize::from(store.value)) else {
            return false;
        };
        contextual_argument_place(mir, requirements, oracle, analysis, *destination).is_some()
            && (is_call_result(mir, *value) || is_composite_constructor(&mir.insts[*value]))
            && oracle.carries_placement(mir, *value)
    });
    let contextual_constraint = effect.constraints.iter().any(|constraint| {
        let (Place::Param(value_index), Place::Param(destination_index)) =
            (constraint.value, constraint.destination)
        else {
            return false;
        };
        let Some(destination) = explicit_call_argument(
            mir,
            raw_args,
            destination_index,
            &effect.omitted_args,
        )
        .and_then(|destination| {
            contextual_argument_place(mir, requirements, oracle, analysis, destination)
        })
        else {
            return false;
        };
        let Some(value) = explicit_call_argument(mir, raw_args, value_index, &effect.omitted_args)
        else {
            return false;
        };
        !matches!(destination, Place::Pending) && oracle.carries_placement(mir, value)
    });
    let contextual_retention = contextual_store || contextual_constraint;
    if !direct_requirement
        && !flow_argument
        && call_requirement.is_none()
        && !unconditional_retention
        && !contextual_retention
    {
        return false;
    }
    if call_requirement.is_none()
        && !direct_requirement
        && !raw_args.iter().enumerate().any(|(index, arg)| {
            u16::try_from(index).ok().is_some_and(|index| {
                is_derived_load(mir, flow_cells, *arg)
                    && call_effect_consumes_argument(effect, index)
            })
        })
        && !unconditional_retention
        && !contextual_retention
    {
        return false;
    }
    let mut args: SmallVec<[Place; 8]> = raw_args
        .iter()
        .map(|arg| inferred_argument_place(mir, requirements, oracle, context, analysis, *arg))
        .collect();
    apply_omitted_args(&mut args, &effect.omitted_args);
    let mut changed = false;

    if let Some(call_requirement) = call_requirement {
        changed |= merge_requirement(&mut requirements.values, id, call_requirement);
    }

    for store in &effect.stores {
        let Some(destination) = explicit_call_argument(
            mir,
            raw_args,
            store.destination,
            &effect.omitted_args,
        )
        .and_then(|destination| {
            contextual_argument_place(mir, requirements, oracle, analysis, destination)
        })
        else {
            continue;
        };
        let Some(value) = explicit_call_argument(mir, raw_args, store.value, &effect.omitted_args)
        else {
            continue;
        };
        if (is_call_result(mir, value) || is_composite_constructor(&mir.insts[value]))
            && oracle.carries_placement(mir, value)
        {
            changed |= merge_requirement(&mut requirements.values, value, destination);
        }
    }

    if let Some(Place::Param(index)) = effect.result
        && let Some(arg) = explicit_call_argument(mir, raw_args, index, &effect.omitted_args)
        && oracle.carries_placement(mir, arg)
        && let Some(call_requirement) = call_requirement
    {
        changed |= merge_requirement(&mut requirements.values, arg, call_requirement);
    }

    for constraint in &effect.constraints {
        let destination = match constraint.destination {
            Place::Destination => call_requirement,
            Place::Param(index) => explicit_call_argument(
                mir,
                raw_args,
                index,
                &effect.omitted_args,
            )
            .and_then(|arg| {
                contextual_argument_place(mir, requirements, oracle, analysis, arg)
            })
            .or_else(|| args.get(index as usize).copied())
            .or(Some(Place::Unknown)),
            place => Some(place),
        };
        match constraint.value {
            Place::Param(index) => {
                let Some(arg) =
                    explicit_call_argument(mir, raw_args, index, &effect.omitted_args)
                else {
                    continue;
                };
                if oracle.carries_placement(mir, arg) && let Some(destination) = destination {
                    changed |= merge_requirement(&mut requirements.values, arg, destination);
                }
            }
            Place::Destination if oracle.carries_placement(mir, id) => {
                if let Some(destination) = destination {
                    changed |= merge_requirement(&mut requirements.values, id, destination);
                }
            }
            _ => {}
        }
    }

    changed
}

fn infer_requirements(
    mir: &MirFunc,
    context: AnalysisContext,
    oracle: &impl PlacementOracle,
    blocks: &[Idx<crate::mir::Block>],
    flow_cells: &HashSet<Idx<Cell>>,
    flow_calls: &HashSet<Idx<Inst>>,
    analysis: Option<&PlacementAnalysis>,
    mut requirements: PlacementRequirements,
    call_effects: &mut HashMap<(Idx<Inst>, Place), CallEffect>,
) -> PlacementRequirements {
    let mut worklist = RequirementWorklist::default();
    worklist.initialize(mir, blocks);
    if let Some(destination) = context.return_destination {
        for &block in blocks {
            if let Term::Return(values) = &mir.blocks[block].term {
                for &value in values {
                    if oracle.carries_placement(mir, value) {
                        worklist.merge_value(&mut requirements, value, destination);
                    }
                }
            }
        }
    }
    while let Some(key) = worklist.queue.pop_front() {
        let RequirementKey::Value(id) = key else {
            let RequirementKey::Cell(cell) = key else {
                unreachable!()
            };
            worklist.queued_cells.remove(cell);
            worklist.process_cell(cell);
            continue;
        };
        worklist.queued_values.remove(id);
        match &mir.insts[id] {
            Inst::StoreGlobal { name, value }
                if oracle.carries_placement(mir, *value)
                    && !oracle.global_store_requires_clone(*name) =>
            {
                worklist.merge_value(&mut requirements, *value, Place::Session);
            }
            Inst::StoreField { base, value, .. } | Inst::StoreIndex { base, value, .. }
                if oracle.carries_placement(mir, *value) =>
            {
                let can_contextualize = match mir.insts[*value] {
                    Inst::Load(cell) => {
                        cell_can_contextualize(mir, oracle, blocks, cell)
                    }
                    _ => true,
                };
                let destination =
                    inferred_argument_place(mir, &requirements, oracle, context, analysis, *base);
                if can_contextualize && !matches!(destination, Place::Pending) {
                    worklist.merge_value(&mut requirements, *value, destination);
                }
            }
            Inst::Store { cell, value } => {
                if let Some(required) = requirements.cells.get(*cell).copied()
                    && oracle.carries_placement(mir, *value)
                    && !is_default_param_store(mir, *cell, *value)
                    && !oracle.is_allocation_site(mir, *value)
                {
                    worklist.merge_value(&mut requirements, *value, required);
                }
            }
            Inst::Load(cell) => {
                if let Some(required) = requirements.values.get(id).copied() {
                    worklist.merge_cell(&mut requirements, *cell, required);
                }
            }
            Inst::Call { .. }
            | Inst::CallMulti { .. }
            | Inst::CallValue { .. }
            | Inst::CallValueMulti { .. } => {
                if infer_call_requirements(
                    mir,
                    context,
                    oracle,
                    flow_cells,
                    flow_calls,
                    analysis,
                    &mut requirements,
                    call_effects,
                    id,
                ) {
                    worklist.enqueue_value(id);
                    if let Inst::Call { args, .. }
                    | Inst::CallMulti { args, .. }
                    | Inst::CallValue { args, .. }
                    | Inst::CallValueMulti { args, .. } = &mir.insts[id]
                    {
                        for &arg in args {
                            worklist.enqueue_value(arg);
                        }
                    }
                }
            }
            Inst::Field { base, .. }
            | Inst::TupleIndex { base, .. }
            | Inst::Index { base, .. } => {
                if let Some(required) = requirements.values.get(id).copied()
                    && oracle.carries_placement(mir, *base)
                {
                    worklist.merge_value(&mut requirements, *base, required);
                }
            }
            Inst::MakeTuple(values) | Inst::MakeVec(values) => {
                if let Some(required) = requirements.values.get(id).copied() {
                    for &value in values {
                        if composite_operand_needs_context(mir, oracle, blocks, context, value) {
                            worklist.merge_value(&mut requirements, value, required);
                        }
                    }
                }
            }
            Inst::MakeVecColumns { columns, .. } => {
                if let Some(required) = requirements.values.get(id).copied() {
                    for &value in columns.iter().flat_map(|(_, values)| values) {
                        if composite_operand_needs_context(mir, oracle, blocks, context, value) {
                            worklist.merge_value(&mut requirements, value, required);
                        }
                    }
                }
            }
            Inst::MakeStruct { fields, .. }
            | Inst::MakeValueStruct { fields, .. }
            | Inst::MaterializeValueStruct { fields, .. }
            | Inst::MakeEnv { fields, .. } => {
                if let Some(required) = requirements.values.get(id).copied() {
                    for &(_, value) in fields {
                        if composite_operand_needs_context(mir, oracle, blocks, context, value) {
                            worklist.merge_value(&mut requirements, value, required);
                        }
                    }
                }
            }
            Inst::MakeEnum {
                positional, named, ..
            }
            | Inst::MakeEnumDyn {
                positional, named, ..
            } => {
                if let Some(required) = requirements.values.get(id).copied() {
                    for &value in positional
                        .iter()
                        .chain(named.iter().map(|(_, value)| value))
                    {
                        if composite_operand_needs_context(mir, oracle, blocks, context, value) {
                            worklist.merge_value(&mut requirements, value, required);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    requirements
}

fn inst_place(analysis: &PlacementAnalysis, inst: Idx<Inst>) -> Place {
    analysis.values.get(inst).copied().unwrap_or(Place::Pending)
}

fn cell_place(analysis: &PlacementAnalysis, cell: Idx<Cell>) -> Place {
    analysis.cells.get(cell).copied().unwrap_or(Place::Pending)
}

fn is_derived_load(mir: &MirFunc, flow_cells: &HashSet<Idx<Cell>>, value: Idx<Inst>) -> bool {
    match mir.insts[value] {
        Inst::Load(cell) => flow_cells.contains(&cell),
        _ => false,
    }
}

fn is_default_param_store(mir: &MirFunc, cell: Idx<Cell>, value: Idx<Inst>) -> bool {
    let Some(index) = mir.params.iter().position(|param| *param == cell) else {
        return false;
    };
    mir.param_default_values
        .get(&index)
        .is_some_and(|roots| roots.contains(&value))
}

fn all_contextual_source<I>(
    mir: &MirFunc,
    oracle: &impl PlacementOracle,
    blocks: &[Idx<crate::mir::Block>],
    context: AnalysisContext,
    operands: I,
    visiting_values: &mut HashSet<Idx<Inst>>,
    visiting_cells: &mut HashSet<Idx<Cell>>,
) -> bool
where
    I: IntoIterator<Item = Idx<Inst>>,
{
    let mut found = false;
    for operand in operands {
        if !oracle.carries_placement(mir, operand) {
            continue;
        }
        found = true;
        if !value_has_contextual_source(
            mir,
            oracle,
            blocks,
            context,
            operand,
            visiting_values,
            visiting_cells,
        ) {
            return false;
        }
    }
    found
}

fn visit_contextual_source_cell(
    mir: &MirFunc,
    oracle: &impl PlacementOracle,
    blocks: &[Idx<crate::mir::Block>],
    context: AnalysisContext,
    cell: Idx<Cell>,
    visiting_values: &mut HashSet<Idx<Inst>>,
    visiting_cells: &mut HashSet<Idx<Cell>>,
) -> bool {
    if mir.params.contains(&cell) {
        return true;
    }
    if !visiting_cells.insert(cell) {
        return false;
    }
    let mut found = false;
    let mut contextual = true;
    for &block in blocks {
        for &id in &mir.blocks[block].insts {
            let Inst::Store {
                cell: stored,
                value,
            } = mir.insts[id]
            else {
                continue;
            };
            if stored != cell || is_default_param_store(mir, stored, value) {
                continue;
            }
            if !oracle.carries_placement(mir, value) {
                continue;
            }
            found = true;
            contextual &= value_has_contextual_source(
                mir,
                oracle,
                blocks,
                context,
                value,
                visiting_values,
                visiting_cells,
            );
        }
    }
    visiting_cells.remove(&cell);
    found && contextual
}

fn value_has_contextual_source(
    mir: &MirFunc,
    oracle: &impl PlacementOracle,
    blocks: &[Idx<crate::mir::Block>],
    context: AnalysisContext,
    value: Idx<Inst>,
    visiting_values: &mut HashSet<Idx<Inst>>,
    visiting_cells: &mut HashSet<Idx<Cell>>,
) -> bool {
    if !oracle.carries_placement(mir, value) || !visiting_values.insert(value) {
        return false;
    }
    let contextual = match &mir.insts[value] {
        Inst::Const(_) => true,
        Inst::Load(cell) => visit_contextual_source_cell(
            mir,
            oracle,
            blocks,
            context,
            *cell,
            visiting_values,
            visiting_cells,
        ),
        Inst::Global(_) => true,
        Inst::Field { base, .. }
        | Inst::TupleIndex { base, .. }
        | Inst::Index { base, .. }
        | Inst::Len { base, .. }
        | Inst::VecCheckedIndex { vec: base, .. } => value_has_contextual_source(
            mir,
            oracle,
            blocks,
            context,
            *base,
            visiting_values,
            visiting_cells,
        ),
        Inst::MakeTuple(values) => all_contextual_source(
            mir,
            oracle,
            blocks,
            context,
            values.iter().copied(),
            visiting_values,
            visiting_cells,
        ),
        Inst::MakeStruct { fields, .. }
        | Inst::MaterializeValueStruct { fields, .. }
        | Inst::MakeValueStruct { fields, .. } => {
            if oracle.is_allocation_site(mir, value) {
                false
            } else {
                all_contextual_source(
                    mir,
                    oracle,
                    blocks,
                    context,
                    fields.iter().map(|(_, value)| *value),
                    visiting_values,
                    visiting_cells,
                )
            }
        }
        Inst::MakeEnum {
            positional, named, ..
        }
        | Inst::MakeEnumDyn {
            positional, named, ..
        } => all_contextual_source(
            mir,
            oracle,
            blocks,
            context,
            positional
                .iter()
                .copied()
                .chain(named.iter().map(|(_, value)| *value)),
            visiting_values,
            visiting_cells,
        ),
        Inst::Call { func, .. } | Inst::CallMulti { func, .. } => matches!(
            oracle
                .call_effect(mir, value, *func, context.default_allocation)
                .result,
            Some(
                Place::Static
                    | Place::Session
                    | Place::Param(_)
                    | Place::Runtime(_)
                    | Place::RuntimeAny
            )
        ),
        Inst::CallValue { callee, .. } | Inst::CallValueMulti { callee, .. } => matches!(
            oracle
                .call_value_effect(mir, value, *callee, context.default_allocation)
                .result,
            Some(
                Place::Static
                    | Place::Session
                    | Place::Param(_)
                    | Place::Runtime(_)
                    | Place::RuntimeAny
            )
        ),
        _ => false,
    };
    visiting_values.remove(&value);
    contextual
}

fn cell_can_contextualize(
    mir: &MirFunc,
    oracle: &impl PlacementOracle,
    blocks: &[Idx<crate::mir::Block>],
    cell: Idx<Cell>,
) -> bool {
    fn visit(
        mir: &MirFunc,
        oracle: &impl PlacementOracle,
        blocks: &[Idx<crate::mir::Block>],
        cell: Idx<Cell>,
        visiting: &mut HashSet<Idx<Cell>>,
    ) -> bool {
        if mir.params.contains(&cell) {
            return true;
        }
        if !visiting.insert(cell) {
            return false;
        }
        let mut found = false;
        let mut can_contextualize = true;
        for &block in blocks {
            for &id in &mir.blocks[block].insts {
                let Inst::Store {
                    cell: stored,
                    value,
                } = mir.insts[id]
                else {
                    continue;
                };
                if stored != cell || is_default_param_store(mir, stored, value) {
                    continue;
                }
                found = true;
                can_contextualize &= match mir.insts[value] {
                    Inst::Load(source) => visit(mir, oracle, blocks, source, visiting),
                    _ => {
                        is_call_result(mir, value)
                            || (is_composite_constructor(&mir.insts[value])
                                && !oracle.is_allocation_site(mir, value))
                    }
                };
            }
        }
        visiting.remove(&cell);
        found && can_contextualize
    }

    visit(mir, oracle, blocks, cell, &mut HashSet::new())
}

fn composite_operand_needs_context(
    mir: &MirFunc,
    oracle: &impl PlacementOracle,
    blocks: &[Idx<crate::mir::Block>],
    context: AnalysisContext,
    value: Idx<Inst>,
) -> bool {
    let carries = oracle.carries_placement(mir, value);
    carries
        && !value_has_contextual_source(
            mir,
            oracle,
            blocks,
            context,
            value,
            &mut HashSet::new(),
            &mut HashSet::new(),
        )
}

fn add_constraint(analysis: &mut PlacementAnalysis, value: Place, destination: Place, span: Span) {
    match known_outlives(value, destination) {
        Some(true) => {}
        None if matches!(value, Place::Pending) || matches!(destination, Place::Pending) => {}
        Some(false) => analysis.violations.push(PlacementViolation {
            value,
            destination,
            span,
            kind: PlacementViolationKind::DoesNotOutlive,
        }),
        None if matches!(value, Place::Param(_) | Place::Destination)
            || matches!(destination, Place::Param(_) | Place::Destination) =>
        {
            let constraint = OutlivesConstraint {
                value,
                destination,
                span,
            };
            if !analysis.summary.constraints.contains(&constraint) {
                analysis.summary.constraints.push(constraint);
            }
        }
        None => analysis.violations.push(PlacementViolation {
            value,
            destination,
            span,
            kind: PlacementViolationKind::UnknownProvenance,
        }),
    }
}

fn export_place(place: Place) -> Place {
    match place {
        Place::Runtime(_) | Place::RuntimeAny => Place::Frame,
        place => place,
    }
}

fn substitute_constraint(
    analysis: &mut PlacementAnalysis,
    constraint: OutlivesConstraint,
    argument_origins: &[Vec<Place>],
    call_destination: Place,
    call_span: Span,
) {
    let substitute = |place| match place {
        Place::Param(index) => argument_origins
            .get(index as usize)
            .cloned()
            .filter(|places| !places.is_empty())
            .unwrap_or_else(|| vec![Place::Unknown]),
        Place::Destination => vec![call_destination],
        place => vec![place],
    };
    let values = substitute(constraint.value);
    let destinations = substitute(constraint.destination);
    for value in values {
        for destination in &destinations {
            let span = if matches!(value, Place::Param(_) | Place::Destination)
                || matches!(destination, Place::Param(_) | Place::Destination)
            {
                constraint.span
            } else {
                call_span
            };
            add_constraint(analysis, value, *destination, span);
        }
    }
}

fn apply_omitted_args(args: &mut SmallVec<[Place; 8]>, omitted_args: &[(u16, Place)]) {
    for &(index, place) in omitted_args {
        let index = usize::from(index);
        if args.len() <= index {
            args.resize(index + 1, Place::Unknown);
        }
        args[index] = place;
    }
}

fn operand_join(analysis: &PlacementAnalysis, inst: &Inst) -> Place {
    let mut result = Place::Static;
    inst.uses_each(&mut |operand| result = result.join(inst_place(analysis, operand)));
    result
}

fn composite_place(
    mir: &MirFunc,
    analysis: &mut PlacementAnalysis,
    requirements: &PlacementRequirements,
    id: Idx<Inst>,
    initial: Place,
    operands: impl IntoIterator<Item = Idx<Inst>>,
) -> Place {
    let operands = operands
        .into_iter()
        .flat_map(|operand| provenance_places(mir, analysis, operand))
        .collect::<Vec<_>>();
    let Some(destination) = requirements.values.get(id).copied() else {
        return operands
            .into_iter()
            .fold(initial, |place, operand| place.join(operand));
    };
    if matches!(destination, Place::Pending | Place::Unknown) {
        return Place::Unknown;
    }
    if initial == Place::Static && operands.iter().all(|operand| *operand == Place::Static) {
        return Place::Static;
    }
    let span = mir.inst_spans.get(id).copied().unwrap_or_default();
    for operand in operands {
        add_constraint(analysis, operand, destination, span);
    }
    destination
}

fn provenance_places(
    mir: &MirFunc,
    analysis: &PlacementAnalysis,
    inst: Idx<Inst>,
) -> Vec<Place> {
    fn visit(
        mir: &MirFunc,
        analysis: &PlacementAnalysis,
        inst: Idx<Inst>,
        visiting: &mut HashSet<Idx<Inst>>,
    ) -> Vec<Place> {
        let place = inst_place(analysis, inst);
        if !matches!(place, Place::Pending | Place::Unknown) {
            return vec![place];
        }
        if !visiting.insert(inst) {
            return vec![place];
        }
        let operands = match &mir.insts[inst] {
            Inst::Load(cell) => return {
                visiting.remove(&inst);
                let mut places = analysis
                    .cell_origins
                    .get(*cell)
                    .cloned()
                    .unwrap_or_default();
                if places.is_empty() {
                    places.push(place);
                }
                places
            },
            Inst::Field { base, .. }
            | Inst::TupleIndex { base, .. }
            | Inst::Index { base, .. }
            | Inst::Len { base, .. }
            | Inst::VecCheckedIndex { vec: base, .. } => vec![*base],
            Inst::MakeTuple(values) | Inst::MakeVec(values) => values.clone(),
            Inst::MakeVecColumns { columns, .. } => {
                columns
                    .iter()
                    .flat_map(|(_, values)| values.iter().copied())
                    .collect()
            }
            Inst::MakeStruct { fields, .. }
            | Inst::MaterializeValueStruct { fields, .. }
            | Inst::MakeValueStruct { fields, .. }
            | Inst::MakeEnv { fields, .. } => fields.iter().map(|(_, value)| *value).collect(),
            Inst::MakeEnum {
                positional, named, ..
            }
            | Inst::MakeEnumDyn {
                positional, named, ..
            } => positional
                .iter()
                .copied()
                .chain(named.iter().map(|(_, value)| *value))
                .collect(),
            _ => Vec::new(),
        };
        let mut places = operands
            .into_iter()
            .flat_map(|operand| visit(mir, analysis, operand, visiting))
            .collect::<Vec<_>>();
        visiting.remove(&inst);
        if places.is_empty() {
            places.push(place);
        }
        places
    }

    let mut places = visit(mir, analysis, inst, &mut HashSet::new());
    places.dedup();
    places
}

fn value_place(
    mir: &MirFunc,
    analysis: &mut PlacementAnalysis,
    oracle: &impl PlacementOracle,
    context: AnalysisContext,
    requirements: &PlacementRequirements,
    call_effects: &mut HashMap<(Idx<Inst>, Place), CallEffect>,
    flow_cells: &HashSet<Idx<Cell>>,
    id: Idx<Inst>,
) -> Place {
    let inst = &mir.insts[id];
    let is_call = matches!(
        inst,
        Inst::Call { .. }
            | Inst::CallMulti { .. }
            | Inst::CallValue { .. }
            | Inst::CallValueMulti { .. }
    );
    if !is_call && !oracle.carries_placement(mir, id) {
        return Place::Static;
    }
    let allocation = requirements
        .values
        .get(id)
        .copied()
        .unwrap_or(context.default_allocation);
    match inst {
        Inst::Const(Lit::Str(_)) => Place::Static,
        Inst::Const(_) => Place::Static,
        Inst::Global(name) => oracle.global_place(*name),
        Inst::Load(cell) => cell_place(analysis, *cell),
        Inst::StringConcat(..)
        | Inst::Format { .. }
        | Inst::FormatDyn { .. }
        | Inst::MakeVecColumnHeader { .. }
        | Inst::Lambda { .. } => allocation,
        Inst::MakeVec(values) => composite_place(
            mir,
            analysis,
            requirements,
            id,
            allocation,
            values.iter().copied(),
        ),
        Inst::MakeVecColumns { columns, .. } => composite_place(
            mir,
            analysis,
            requirements,
            id,
            allocation,
            columns.iter().flat_map(|(_, values)| values.iter().copied()),
        ),
        Inst::ToString(value) => inst_place(analysis, *value).join(allocation),
        Inst::Field { .. }
        | Inst::TupleIndex { .. }
        | Inst::Index { .. }
        | Inst::Len { .. }
        | Inst::VecCheckedIndex { .. } => provenance_places(mir, analysis, id)
            .into_iter()
            .fold(Place::Static, Place::join),
        Inst::MakeTuple(values) => composite_place(
            mir,
            analysis,
            requirements,
            id,
            Place::Static,
            values.iter().copied(),
        ),
        Inst::MakeStruct { fields, .. }
        | Inst::MakeValueStruct { fields, .. }
        | Inst::MaterializeValueStruct { fields, .. } => {
            let initial = if oracle.is_allocation_site(mir, id) {
                allocation
            } else {
                Place::Static
            };
            composite_place(
                mir,
                analysis,
                requirements,
                id,
                initial,
                fields.iter().map(|(_, value)| *value),
            )
        }
        Inst::MakeEnv { fields, .. } => composite_place(
            mir,
            analysis,
            requirements,
            id,
            allocation,
            fields.iter().map(|(_, value)| *value),
        ),
        Inst::MakeEnum {
            positional, named, ..
        } => composite_place(
            mir,
            analysis,
            requirements,
            id,
            Place::Static,
            positional
                .iter()
                .copied()
                .chain(named.iter().map(|(_, value)| *value)),
        ),
        Inst::MakeEnumDyn {
            tag,
            positional,
            named,
            ..
        } => composite_place(
            mir,
            analysis,
            requirements,
            id,
            Place::Static,
            std::iter::once(*tag)
                .chain(positional.iter().copied())
                .chain(named.iter().map(|(_, value)| *value)),
        ),
        Inst::Call { .. }
        | Inst::CallMulti { .. }
        | Inst::CallValue { .. }
        | Inst::CallValueMulti { .. } => {
            let raw_args = match inst {
                Inst::Call { args, .. }
                | Inst::CallMulti { args, .. }
                | Inst::CallValue { args, .. }
                | Inst::CallValueMulti { args, .. } => args,
                _ => unreachable!("call placement requested for a non-call"),
            };
            let effect = call_effects.entry((id, allocation)).or_insert_with(|| {
                match inst {
                    Inst::Call { func, .. } | Inst::CallMulti { func, .. } => {
                        oracle.call_effect(mir, id, *func, allocation)
                    }
                    Inst::CallValue { callee, .. } | Inst::CallValueMulti { callee, .. } => {
                        oracle.call_value_effect(mir, id, *callee, allocation)
                    }
                    _ => unreachable!("call placement requested for a non-call"),
                }
            });
            let mut args: SmallVec<[Place; 8]> = raw_args
                .iter()
                .map(|arg| {
                    inferred_argument_place(
                        mir,
                        requirements,
                        oracle,
                        context,
                        Some(analysis),
                        *arg,
                    )
                })
                .collect();
            apply_omitted_args(&mut args, &effect.omitted_args);
            let span = mir.inst_spans.get(id).copied().unwrap_or_default();
            let mut argument_origins = raw_args
                .iter()
                .map(|arg| provenance_places(mir, analysis, *arg))
                .collect::<Vec<_>>();
            for &(index, place) in &effect.omitted_args {
                let index = usize::from(index);
                if argument_origins.len() <= index {
                    argument_origins.resize(index + 1, vec![Place::Unknown]);
                }
                argument_origins[index] = vec![place];
            }
            for store in &effect.stores {
                if raw_args
                    .get(store.value as usize)
                    .copied()
                    .is_some_and(|value| is_derived_load(mir, flow_cells, value))
                {
                    continue;
                }
                let value = args
                    .get(store.value as usize)
                    .copied()
                    .unwrap_or(Place::Unknown);
                let destination = args
                    .get(store.destination as usize)
                    .copied()
                    .unwrap_or(Place::Unknown);
                add_constraint(analysis, value, destination, span);
            }
            for &constraint in &effect.constraints {
                if let Place::Param(index) = constraint.value
                    && raw_args
                        .get(index as usize)
                        .copied()
                        .is_some_and(|value| is_derived_load(mir, flow_cells, value))
                {
                    continue;
                }
                substitute_constraint(
                    analysis,
                    constraint,
                    &argument_origins,
                    allocation,
                    span,
                );
            }
            if let Some(owner) = effect.owner {
                analysis
                    .call_owners
                    .insert(id, owner.substitute(&args, allocation));
            }
            for &retained in &effect.opaque_retains {
                let value = args
                    .get(retained as usize)
                    .copied()
                    .unwrap_or(Place::Unknown);
                if !matches!(value, Place::Static | Place::Pending) {
                    analysis.violations.push(PlacementViolation {
                        value,
                        destination: allocation,
                        span,
                        kind: PlacementViolationKind::OpaqueRetention,
                    });
                }
            }
            let is_multi = matches!(inst, Inst::CallMulti { .. } | Inst::CallValueMulti { .. });
            if !oracle.carries_placement(mir, id) && !is_multi {
                return Place::Static;
            }
            match effect
                .result
                .map(|place| place.substitute(&args, allocation))
                .unwrap_or(Place::Unknown)
            {
                Place::Unknown if effect.owner == Some(Place::Destination) => allocation,
                result => result,
            }
        }
        _ => operand_join(analysis, inst),
    }
}

pub fn analyze(
    mir: &MirFunc,
    context: AnalysisContext,
    oracle: &impl PlacementOracle,
) -> PlacementAnalysis {
    let blocks = crate::reach::emitted_blocks(mir);
    let parameter_cells = mir.params.iter().copied().collect::<HashSet<_>>();
    let multi_result_cells = blocks
        .iter()
        .flat_map(|block| mir.blocks[*block].insts.iter().copied())
        .filter_map(|id| match mir.insts[id] {
            Inst::Load(cell) if oracle.carries_placement(mir, id) => Some(cell),
            _ => None,
        })
        .collect::<HashSet<_>>();
    let mut call_effects = HashMap::new();
    let mut cell_blocks = HashMap::<Idx<Cell>, HashSet<Idx<crate::mir::Block>>>::new();
    let mut alias_predecessors = HashMap::<Idx<Cell>, HashSet<Idx<Cell>>>::new();
    let mut alias_successors = HashMap::<Idx<Cell>, HashSet<Idx<Cell>>>::new();
    let mut source_cells = HashSet::new();
    let mut single_call_sources = HashSet::new();
    for &block in &blocks {
        for &id in &mir.blocks[block].insts {
            let Inst::Store { cell, value } = mir.insts[id] else {
                continue;
            };
            cell_blocks.entry(cell).or_default().insert(block);
            if let Inst::Load(source) = mir.insts[value] {
                alias_predecessors.entry(cell).or_default().insert(source);
                alias_successors.entry(source).or_default().insert(cell);
            }
            if requirement_flow_source(mir, oracle, &parameter_cells, value) {
                source_cells.insert(cell);
                if is_call_result(mir, value) {
                    single_call_sources.insert(cell);
                }
            }
        }
    }
    loop {
        let mut changed = false;
        for &block in &blocks {
            for &id in &mir.blocks[block].insts {
                let Inst::Store { cell, value } = mir.insts[id] else {
                    continue;
                };
                if matches!(mir.insts[value], Inst::Load(source) if source_cells.contains(&source)) {
                    changed |= source_cells.insert(cell);
                }
            }
        }
        if !changed {
            break;
        }
    }
    let joined_sources = cell_blocks
        .iter()
        .filter_map(|(&cell, blocks)| {
            (blocks.len() > 1 && source_cells.contains(&cell)).then_some(cell)
        })
        .collect::<HashSet<_>>();
    let flow_sources = joined_sources
        .into_iter()
        .chain(single_call_sources.iter().copied())
        .collect::<HashSet<_>>();
    let mut candidate_cells = flow_sources
        .iter()
        .filter(|cell| !single_call_sources.contains(cell))
        .copied()
        .collect::<HashSet<_>>();
    let mut pending = candidate_cells.iter().copied().collect::<Vec<_>>();
    while let Some(cell) = pending.pop() {
        if let Some(successors) = alias_successors.get(&cell) {
            for &successor in successors {
                if candidate_cells.insert(successor) {
                    pending.push(successor);
                }
            }
        }
    }
    let mut sink_cells = HashSet::new();
    let mut sink_calls = Vec::new();
    for &block in &blocks {
        for &id in &mir.blocks[block].insts {
            let raw_args = match &mir.insts[id] {
                Inst::Call { args, .. }
                | Inst::CallMulti { args, .. }
                | Inst::CallValue { args, .. }
                | Inst::CallValueMulti { args, .. } => args,
                _ => continue,
            };
            let allocation = context.default_allocation;
            let effect = call_effects.entry((id, allocation)).or_insert_with(|| {
                match mir.insts[id] {
                    Inst::Call { func, .. } | Inst::CallMulti { func, .. } => {
                        oracle.call_effect(mir, id, func, allocation)
                    }
                    Inst::CallValue { callee, .. } | Inst::CallValueMulti { callee, .. } => {
                        oracle.call_value_effect(mir, id, callee, allocation)
                    }
                    _ => unreachable!("call sink requested for a non-call"),
                }
            });
            for (index, arg) in raw_args.iter().enumerate() {
                let Ok(index) = u16::try_from(index) else {
                    continue;
                };
                if let Inst::Load(cell) = mir.insts[*arg]
                    && (candidate_cells.contains(&cell) || single_call_sources.contains(&cell))
                    && oracle.carries_placement(mir, *arg)
                    && call_effect_consumes_argument(effect, index)
                {
                    sink_cells.insert(cell);
                    sink_calls.push((id, cell));
                }
            }
        }
    }
    let mut flow_cells = HashSet::new();
    for sink in sink_cells {
        let mut path = HashSet::new();
        let mut pending = vec![sink];
        while let Some(cell) = pending.pop() {
            if !path.insert(cell) {
                continue;
            }
            if flow_sources.contains(&cell) {
                continue;
            }
            if let Some(predecessors) = alias_predecessors.get(&cell) {
                pending.extend(predecessors.iter().copied());
            }
        }
        if path.iter().any(|cell| flow_sources.contains(cell)) {
            flow_cells.extend(path);
        }
    }
    let flow_calls = sink_calls
        .into_iter()
        .filter_map(|(id, cell)| flow_cells.contains(&cell).then_some(id))
        .collect::<HashSet<_>>();
    let mut requirements = infer_requirements(
        mir,
        context,
        oracle,
        &blocks,
        &flow_cells,
        &flow_calls,
        None,
        PlacementRequirements::default(),
        &mut call_effects,
    );
    loop {
        let mut analysis = PlacementAnalysis::default();
        for (index, cell) in mir.params.iter().copied().enumerate() {
            let place = u16::try_from(index).map_or(Place::Unknown, Place::Param);
            analysis.cells.insert(cell, place);
            analysis.cell_origins.insert(cell, vec![place]);
        }

        loop {
            let mut changed = false;
            for &block in &blocks {
                for &id in &mir.blocks[block].insts {
                    let span = mir.inst_spans.get(id).copied().unwrap_or_default();
                    match &mir.insts[id] {
                        Inst::Store { cell, value } => {
                            if is_default_param_store(mir, *cell, *value) {
                                continue;
                            }
                            let value_inst = *value;
                            let value = inst_place(&analysis, value_inst);
                            let origins = provenance_places(mir, &analysis, value_inst);
                            if let Some(destination) = requirements.cells.get(*cell).copied()
                                && !is_derived_load(mir, &flow_cells, value_inst)
                            {
                                for value in &origins {
                                    add_constraint(&mut analysis, *value, destination, span);
                                }
                            }
                            changed |= merge_cell(&mut analysis.cells, *cell, value);
                            changed |= merge_cell_origins(&mut analysis.cell_origins, *cell, origins);
                        }
                        Inst::StoreGlobal { name, value } => {
                            if !oracle.global_store_requires_clone(*name) {
                                for value in provenance_places(mir, &analysis, *value) {
                                    add_constraint(&mut analysis, value, Place::Session, span);
                                }
                            }
                        }
                        Inst::StoreField { base, value, .. }
                        | Inst::StoreIndex { base, value, .. } => {
                            let destination = inst_place(&analysis, *base);
                            for value in provenance_places(mir, &analysis, *value) {
                                add_constraint(&mut analysis, value, destination, span);
                            }
                        }
                        Inst::ArrayPush { arr, value, .. } => {
                            let destination = inst_place(&analysis, *arr);
                            for value in provenance_places(mir, &analysis, *value) {
                                add_constraint(&mut analysis, value, destination, span);
                            }
                        }
                        Inst::CallMulti { dests, .. } | Inst::CallValueMulti { dests, .. } => {
                            let place = value_place(
                                mir,
                                &mut analysis,
                                oracle,
                                context,
                                &requirements,
                                &mut call_effects,
                                &flow_cells,
                                id,
                            );
                            changed |= merge_inst(&mut analysis.values, id, place);
                            for &dest in dests {
                                if multi_result_cells.contains(&dest) {
                                    changed |= merge_cell(&mut analysis.cells, dest, place);
                                    changed |= merge_cell_origins(
                                        &mut analysis.cell_origins,
                                        dest,
                                        [place],
                                    );
                                }
                            }
                        }
                        _ => {
                            let place = value_place(
                                mir,
                                &mut analysis,
                                oracle,
                                context,
                                &requirements,
                                &mut call_effects,
                                &flow_cells,
                                id,
                            );
                            changed |= merge_inst(&mut analysis.values, id, place);
                        }
                    }
                }
            }
            if !changed {
                break;
            }
            analysis.violations.clear();
            analysis.summary.constraints.clear();
        }

        let mut contextualized = false;
        for &block in &blocks {
            for &id in &mir.blocks[block].insts {
                let Inst::StoreField { base, value, .. } = &mir.insts[id] else {
                    continue;
                };
                if matches!(mir.insts[*value], Inst::Load(_)) || !oracle.carries_placement(mir, *value)
                {
                    continue;
                }
                let destination = inst_place(&analysis, *base);
                if matches!(destination, Place::Param(_) | Place::Runtime(_) | Place::Session) {
                    contextualized |=
                        merge_requirement(&mut requirements.values, *value, destination);
                }
            }
        }
        if contextualized {
            requirements = infer_requirements(
                mir,
                context,
                oracle,
                &blocks,
                &flow_cells,
                &flow_calls,
                Some(&analysis),
                requirements,
                &mut call_effects,
            );
            continue;
        }

        let refined = infer_requirements(
            mir,
            context,
            oracle,
            &blocks,
            &flow_cells,
            &flow_calls,
            Some(&analysis),
            requirements.clone(),
            &mut call_effects,
        );
        if refined != requirements {
            requirements = refined;
            continue;
        }

        for &block in &blocks {
            if let Term::Return(values) = &mir.blocks[block].term {
                let result = values.iter().fold(Place::Static, |place, value| {
                    place.join(inst_place(&analysis, *value))
                });
                let entity_result = values
                    .iter()
                    .filter_map(|value| oracle.entity_place(mir, *value))
                    .fold(None::<Place>, |place, value| {
                        Some(place.map_or(value, |place| place.join(value)))
                    });
                let entity_content = values
                    .iter()
                    .filter_map(|value| oracle.entity_content_place(mir, *value))
                    .fold(None::<Place>, |place, value| {
                        Some(place.map_or(value, |place| place.join(value)))
                    });
                if !values.is_empty() {
                    analysis.summary.result = Some(
                        analysis
                            .summary
                            .result
                            .map_or(result, |previous| previous.join(result)),
                    );
                }
                if let Some(entity_result) = entity_result {
                    analysis.summary.entity_result = Some(
                        analysis
                            .summary
                            .entity_result
                            .map_or(entity_result, |previous| previous.join(entity_result)),
                    );
                }
                if let Some(entity_content) = entity_content {
                    analysis.summary.entity_content = Some(
                        analysis
                            .summary
                            .entity_content
                            .map_or(entity_content, |previous| previous.join(entity_content)),
                    );
                }
                if let Some(destination) = context.return_destination {
                    for &value in values {
                        let span = mir.inst_spans.get(value).copied().unwrap_or_default();
                        for value in provenance_places(mir, &analysis, value) {
                            add_constraint(&mut analysis, value, destination, span);
                        }
                    }
                }
            }
        }

        for (value, required) in requirements.values.iter() {
            let requires_owner = oracle.carries_placement(mir, value)
                && !value_has_contextual_source(
                    mir,
                    oracle,
                    &blocks,
                    context,
                    value,
                    &mut HashSet::new(),
                    &mut HashSet::new(),
                );
            if requires_owner && matches!(required, Place::Param(_) | Place::Destination) {
                analysis.summary.owner = Some(
                    analysis
                        .summary
                        .owner
                        .map_or(*required, |previous| previous.join(*required)),
                );
            }
        }

        for (&index, roots) in &mir.param_default_values {
            let place = roots.iter().fold(Place::Static, |place, root| {
                let value = match analysis.values.get(*root).copied() {
                    Some(Place::Pending) | None => Place::Unknown,
                    Some(value) => value,
                };
                place.join(value)
            });
            if analysis.summary.default_params.len() <= index {
                analysis.summary.default_params.resize(index + 1, None);
            }
            analysis.summary.default_params[index] = Some(place);
        }

        analysis.summary.result = analysis.summary.result.map(export_place);
        analysis.summary.entity_result = analysis.summary.entity_result.map(export_place);
        analysis.summary.entity_content = analysis.summary.entity_content.map(export_place);
        analysis.summary.owner = analysis.summary.owner.map(export_place);
        for constraint in &mut analysis.summary.constraints {
            constraint.value = export_place(constraint.value);
            constraint.destination = export_place(constraint.destination);
        }
        for place in &mut analysis.summary.default_params {
            *place = place.map(export_place);
        }

        return analysis;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RuntimeResultOracle;

    impl PlacementOracle for RuntimeResultOracle {
        fn global_place(&self, _name: Ustr) -> Place {
            Place::Static
        }

        fn carries_placement(&self, _mir: &MirFunc, _inst: Idx<Inst>) -> bool {
            true
        }

        fn is_allocation_site(&self, _mir: &MirFunc, _inst: Idx<Inst>) -> bool {
            false
        }

        fn call_effect(
            &self,
            _mir: &MirFunc,
            _inst: Idx<Inst>,
            _func: FuncRef,
            _default_allocation: Place,
        ) -> CallEffect {
            CallEffect {
                result: Some(Place::Runtime(_inst)),
                ..CallEffect::default()
            }
        }
    }

    #[test]
    fn function_summaries_do_not_export_callee_local_runtime_places() {
        let mut mir = MirFunc::new(Ustr::from("runtime_result"));
        let block = mir.entry();
        let call = mir.emit(
            block,
            Inst::Call {
                func: FuncRef {
                    module: bone_semantic::prelude::ModuleId::builtin(),
                    func: Idx::from_raw(comfy_la_arena::RawIdx::from_u32(0)),
                },
                args: Vec::new(),
            },
        );
        mir.set_term(block, Term::Return(vec![call]));

        let analysis = analyze(&mir, AnalysisContext::frame(), &RuntimeResultOracle);

        assert_eq!(analysis.summary.result, Some(Place::Frame));
        assert!(analysis.summary.constraints.iter().all(|constraint| {
            !matches!(constraint.value, Place::Runtime(_))
                && !matches!(constraint.destination, Place::Runtime(_))
        }));
    }

    #[test]
    fn requirement_merge_is_order_independent_for_param_and_frame() {
        let mut mir = MirFunc::new(Ustr::from("requirement_merge"));
        let block = mir.entry();
        let key = mir.emit(block, Inst::Const(Lit::Int(0)));

        let mut param_then_frame = ArenaMap::default();
        merge_requirement(&mut param_then_frame, key, Place::Param(3));
        merge_requirement(&mut param_then_frame, key, Place::Frame);

        let mut frame_then_param = ArenaMap::default();
        merge_requirement(&mut frame_then_param, key, Place::Frame);
        merge_requirement(&mut frame_then_param, key, Place::Param(3));

        assert_eq!(
            param_then_frame.get(key).copied(),
            Some(Place::Param(3))
        );
        assert_eq!(
            frame_then_param.get(key).copied(),
            Some(Place::Param(3))
        );
    }

    #[test]
    fn requirement_merge_is_order_independent_for_session_and_param() {
        let mut mir = MirFunc::new(Ustr::from("requirement_merge"));
        let block = mir.entry();
        let key = mir.emit(block, Inst::Const(Lit::Int(0)));

        let mut param_then_session = ArenaMap::default();
        merge_requirement(&mut param_then_session, key, Place::Param(3));
        merge_requirement(&mut param_then_session, key, Place::Session);

        let mut session_then_param = ArenaMap::default();
        merge_requirement(&mut session_then_param, key, Place::Session);
        merge_requirement(&mut session_then_param, key, Place::Param(3));

        assert_eq!(
            param_then_session.get(key).copied(),
            Some(Place::Session)
        );
        assert_eq!(
            session_then_param.get(key).copied(),
            Some(Place::Session)
        );
    }

    #[test]
    fn requirement_merge_is_order_independent_for_destination_and_runtime() {
        let mut mir = MirFunc::new(Ustr::from("requirement_merge"));
        let block = mir.entry();
        let key = mir.emit(block, Inst::Const(Lit::Int(0)));
        let runtime = mir.emit(block, Inst::Const(Lit::Int(1)));

        let mut destination_then_runtime = ArenaMap::default();
        merge_requirement(&mut destination_then_runtime, key, Place::Destination);
        merge_requirement(
            &mut destination_then_runtime,
            key,
            Place::Runtime(runtime),
        );

        let mut runtime_then_destination = ArenaMap::default();
        merge_requirement(&mut runtime_then_destination, key, Place::Runtime(runtime));
        merge_requirement(
            &mut runtime_then_destination,
            key,
            Place::Destination,
        );

        assert_eq!(
            destination_then_runtime.get(key).copied(),
            Some(Place::Unknown)
        );
        assert_eq!(
            runtime_then_destination.get(key).copied(),
            Some(Place::Unknown)
        );
    }

    #[test]
    fn requirement_merge_is_order_independent_for_destination_and_param() {
        let mut mir = MirFunc::new(Ustr::from("requirement_merge"));
        let block = mir.entry();
        let key = mir.emit(block, Inst::Const(Lit::Int(0)));

        let mut destination_then_param = ArenaMap::default();
        merge_requirement(&mut destination_then_param, key, Place::Destination);
        merge_requirement(&mut destination_then_param, key, Place::Param(3));

        let mut param_then_destination = ArenaMap::default();
        merge_requirement(&mut param_then_destination, key, Place::Param(3));
        merge_requirement(&mut param_then_destination, key, Place::Destination);

        assert_eq!(
            destination_then_param.get(key).copied(),
            Some(Place::Param(3))
        );
        assert_eq!(
            param_then_destination.get(key).copied(),
            Some(Place::Param(3))
        );
    }

    #[test]
    fn requirement_merge_is_order_independent_for_static_and_runtime() {
        let mut mir = MirFunc::new(Ustr::from("requirement_merge"));
        let block = mir.entry();
        let key = mir.emit(block, Inst::Const(Lit::Int(0)));
        let runtime = mir.emit(block, Inst::Const(Lit::Int(1)));

        let mut static_then_runtime = ArenaMap::default();
        merge_requirement(&mut static_then_runtime, key, Place::Static);
        merge_requirement(
            &mut static_then_runtime,
            key,
            Place::Runtime(runtime),
        );

        let mut runtime_then_static = ArenaMap::default();
        merge_requirement(&mut runtime_then_static, key, Place::Runtime(runtime));
        merge_requirement(&mut runtime_then_static, key, Place::Static);

        assert_eq!(
            static_then_runtime.get(key).copied(),
            Some(Place::Runtime(runtime))
        );
        assert_eq!(
            runtime_then_static.get(key).copied(),
            Some(Place::Runtime(runtime))
        );
    }

    #[test]
    fn distinct_runtime_places_keep_frame_lifetime_without_owner_identity() {
        let mut mir = MirFunc::new(Ustr::from("runtime_join"));
        let block = mir.entry();
        let left = mir.emit(block, Inst::Const(Lit::Int(0)));
        let right = mir.emit(block, Inst::Const(Lit::Int(1)));

        assert_eq!(
            Place::Runtime(left).join(Place::Runtime(right)),
            Place::RuntimeAny
        );
        assert_eq!(
            known_outlives(Place::RuntimeAny, Place::Frame),
            Some(true)
        );
        assert_eq!(
            known_outlives(Place::RuntimeAny, Place::Session),
            Some(false)
        );
        assert_eq!(
            known_outlives(Place::RuntimeAny, Place::Runtime(left)),
            None
        );
        assert_eq!(export_place(Place::RuntimeAny), Place::Frame);
    }

    #[test]
    fn static_composite_does_not_acquire_a_destination_owner() {
        let mut mir = MirFunc::new(Ustr::from("static_composite"));
        let block = mir.entry();
        let operand = mir.emit(block, Inst::Const(Lit::Int(1)));
        let aggregate = mir.emit(block, Inst::Const(Lit::Int(0)));
        let mut analysis = PlacementAnalysis::default();
        analysis.values.insert(operand, Place::Static);
        let mut requirements = PlacementRequirements::default();
        requirements
            .values
            .insert(aggregate, Place::Destination);

        assert_eq!(
            composite_place(
                &mir,
                &mut analysis,
                &requirements,
                aggregate,
                Place::Static,
                [operand],
            ),
            Place::Static
        );
        assert!(analysis.violations.is_empty());
    }
}
