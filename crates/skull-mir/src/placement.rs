use std::collections::{HashMap, HashSet};

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
    Destination,
    Param(u16),
    Runtime(Idx<Inst>),
    Unknown,
}

impl Place {
    fn join(self, other: Self) -> Self {
        match (self, other) {
            (left, right) if left == right => left,
            (Self::Pending, other) | (other, Self::Pending) => other,
            (Self::Static, other) | (other, Self::Static) => other,
            (Self::Destination, other) | (other, Self::Destination) => other,
            (Self::Frame, Self::Session) | (Self::Session, Self::Frame) => Self::Frame,
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

    fn carries_placement(&self, mir: &MirFunc, inst: Idx<Inst>) -> bool;

    fn is_allocation_site(&self, mir: &MirFunc, inst: Idx<Inst>) -> bool;

    fn call_effect(
        &self,
        mir: &MirFunc,
        inst: Idx<Inst>,
        func: FuncRef,
        default_allocation: Place,
    ) -> CallEffect;
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
        (Place::Session, Place::Session | Place::Frame) => Some(true),
        (Place::Session, Place::Runtime(_)) => Some(true),
        (Place::Runtime(_), Place::Frame) => Some(true),
        (Place::Runtime(left), Place::Runtime(right)) => Some(left == right),
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

fn merge_requirement<T>(map: &mut ArenaMap<Idx<T>, Place>, key: Idx<T>, value: Place) -> bool {
    let merged = match (map.get(key).copied(), value) {
        (None, value) => value,
        (Some(old), value) if old == value => old,
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
        (Some(Place::Session), Place::Runtime(_))
        | (Some(Place::Runtime(_)), Place::Session) => Place::Session,
        (Some(Place::Frame), runtime @ Place::Runtime(_))
        | (Some(runtime @ Place::Runtime(_)), Place::Frame) => runtime,
        (Some(Place::Runtime(left)), Place::Runtime(right)) => {
            if left == right { Place::Runtime(left) } else { Place::Unknown }
        }
        (Some(Place::Param(left)), Place::Param(right)) => {
            if left == right { Place::Param(left) } else { Place::Unknown }
        }
        (Some(Place::Runtime(_) | Place::Param(_)), Place::Runtime(_) | Place::Param(_)) => {
            Place::Unknown
        }
        (Some(Place::Unknown), _) | (Some(_), Place::Unknown) => Place::Unknown,
        (Some(_), value) => value,
    };
    if map.get(key).copied() == Some(merged) {
        return false;
    }
    map.insert(key, merged);
    true
}

#[derive(Clone, Default, PartialEq, Eq)]
struct PlacementRequirements {
    values: ArenaMap<Idx<Inst>, Place>,
    cells: ArenaMap<Idx<Cell>, Place>,
}

fn inferred_argument_place(
    mir: &MirFunc,
    requirements: &PlacementRequirements,
    oracle: &impl PlacementOracle,
    context: AnalysisContext,
    analysis: Option<&PlacementAnalysis>,
    arg: Idx<Inst>,
) -> Place {
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
    matches!(mir.insts[inst], Inst::Call { .. } | Inst::CallMulti { .. })
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

fn carries_requirement(
    mir: &MirFunc,
    oracle: &impl PlacementOracle,
    flow_cells: &HashSet<Idx<Cell>>,
    inst: Idx<Inst>,
) -> bool {
    oracle.carries_placement(mir, inst)
        && (is_call_result(mir, inst)
            || is_composite_constructor(&mir.insts[inst])
            || matches!(mir.insts[inst], Inst::Load(cell) if flow_cells.contains(&cell)))
}

fn infer_call_requirements(
    mir: &MirFunc,
    context: AnalysisContext,
    oracle: &impl PlacementOracle,
    flow_cells: &HashSet<Idx<Cell>>,
    analysis: Option<&PlacementAnalysis>,
    requirements: &mut PlacementRequirements,
    call_effects: &mut HashMap<(Idx<Inst>, Place), CallEffect>,
    id: Idx<Inst>,
) -> bool {
    let (func, raw_args) = match &mir.insts[id] {
        Inst::Call { func, args } | Inst::CallMulti { func, args, .. } => (*func, args),
        _ => return false,
    };
    if !raw_args
        .iter()
        .any(|arg| carries_requirement(mir, oracle, flow_cells, *arg))
    {
        return false;
    }
    let allocation = requirements
        .values
        .get(id)
        .copied()
        .filter(|place| !matches!(place, Place::Pending))
        .unwrap_or(context.default_allocation);
    let effect = call_effects
        .entry((id, allocation))
        .or_insert_with(|| oracle.call_effect(mir, id, func, allocation));
    let mut args: SmallVec<[Place; 8]> = raw_args
        .iter()
        .map(|arg| inferred_argument_place(mir, requirements, oracle, context, analysis, *arg))
        .collect();
    apply_omitted_args(&mut args, &effect.omitted_args);
    let call_requirement = requirements
        .values
        .get(id)
        .copied()
        .filter(|place| !matches!(place, Place::Pending));
    let mut changed = false;

    if let Some(Place::Param(index)) = effect.result
        && let Some(arg) = explicit_call_argument(mir, raw_args, index, &effect.omitted_args)
        && is_call_result(mir, arg)
        && oracle.carries_placement(mir, arg)
        && let Some(call_requirement) = call_requirement
    {
        changed |= merge_requirement(&mut requirements.values, arg, call_requirement);
    }

    for constraint in &effect.constraints {
        let destination = match constraint.destination {
            Place::Destination => call_requirement,
            Place::Param(index) => Some(
                args.get(index as usize)
                    .copied()
                    .unwrap_or(Place::Unknown),
            ),
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
    analysis: Option<&PlacementAnalysis>,
    mut requirements: PlacementRequirements,
    call_effects: &mut HashMap<(Idx<Inst>, Place), CallEffect>,
) -> PlacementRequirements {
    loop {
        let mut changed = false;
        for &block in blocks {
            if let Term::Return(values) = &mir.blocks[block].term {
                if let Some(destination) = context.return_destination {
                    for &value in values {
                        if oracle.carries_placement(mir, value) {
                            changed |=
                                merge_requirement(&mut requirements.values, value, destination);
                        }
                    }
                }
            }
            for &id in &mir.blocks[block].insts {
                match &mir.insts[id] {
                    Inst::StoreGlobal { value, .. } if oracle.carries_placement(mir, *value) => {
                        changed |=
                            merge_requirement(&mut requirements.values, *value, Place::Session);
                    }
                    Inst::Store { cell, value } => {
                        if let Some(required) = requirements.cells.get(*cell).copied() {
                            if oracle.carries_placement(mir, *value)
                                && !oracle.is_allocation_site(mir, *value)
                            {
                                changed |=
                                    merge_requirement(&mut requirements.values, *value, required);
                            }
                        }
                    }
                    Inst::Load(cell) => {
                        if let Some(required) = requirements.values.get(id).copied() {
                            changed |= merge_requirement(&mut requirements.cells, *cell, required);
                        }
                    }
                    Inst::Call { .. } | Inst::CallMulti { .. } => {
                        changed |= infer_call_requirements(
                            mir,
                            context,
                            oracle,
                            flow_cells,
                            analysis,
                            &mut requirements,
                            call_effects,
                            id,
                        );
                    }
                    Inst::TupleIndex { base, .. } => {
                        if let Some(required) = requirements.values.get(id).copied()
                            && oracle.carries_placement(mir, *base)
                        {
                            changed |= merge_requirement(
                                &mut requirements.values,
                                *base,
                                required,
                            );
                        }
                    }
                    Inst::MakeTuple(values) | Inst::MakeVec(values) => {
                        if let Some(required) = requirements.values.get(id).copied() {
                            for &value in values {
                                if oracle.carries_placement(mir, value) {
                                    changed |= merge_requirement(
                                        &mut requirements.values,
                                        value,
                                        required,
                                    );
                                }
                            }
                        }
                    }
                    Inst::MakeVecColumns { columns, .. } => {
                        if let Some(required) = requirements.values.get(id).copied() {
                            for &value in columns.iter().flat_map(|(_, values)| values) {
                                if oracle.carries_placement(mir, value) {
                                    changed |= merge_requirement(
                                        &mut requirements.values,
                                        value,
                                        required,
                                    );
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
                                if oracle.carries_placement(mir, value) {
                                    changed |= merge_requirement(
                                        &mut requirements.values,
                                        value,
                                        required,
                                    );
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
                                if oracle.carries_placement(mir, value) {
                                    changed |= merge_requirement(
                                        &mut requirements.values,
                                        value,
                                        required,
                                    );
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        if !changed {
            return requirements;
        }
    }
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

fn substitute_constraint(
    analysis: &mut PlacementAnalysis,
    constraint: OutlivesConstraint,
    args: &[Place],
    call_destination: Place,
    call_span: Span,
) {
    let value = constraint.value.substitute(args, call_destination);
    let destination = constraint.destination.substitute(args, call_destination);
    let span = if matches!(value, Place::Param(_) | Place::Destination)
        || matches!(destination, Place::Param(_) | Place::Destination)
    {
        constraint.span
    } else {
        call_span
    };
    add_constraint(analysis, value, destination, span);
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
        .map(|operand| inst_place(analysis, operand))
        .collect::<Vec<_>>();
    let Some(destination) = requirements.values.get(id).copied() else {
        return operands
            .into_iter()
            .fold(initial, |place, operand| place.join(operand));
    };
    if matches!(destination, Place::Pending | Place::Unknown) {
        return Place::Unknown;
    }
    let span = mir.inst_spans.get(id).copied().unwrap_or_default();
    for operand in operands {
        add_constraint(analysis, operand, destination, span);
    }
    destination
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
    let is_call = matches!(inst, Inst::Call { .. } | Inst::CallMulti { .. });
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
        Inst::Field { base, .. }
        | Inst::TupleIndex { base, .. }
        | Inst::Index { base, .. }
        | Inst::Len { base, .. }
        | Inst::VecCheckedIndex { vec: base, .. } => inst_place(analysis, *base),
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
        Inst::Call {
            func,
            args: raw_args,
        }
        | Inst::CallMulti {
            func,
            args: raw_args,
            ..
        } => {
            let mut args: SmallVec<[Place; 8]> = raw_args
                .iter()
                .map(|arg| inst_place(analysis, *arg))
                .collect();
            let effect = call_effects
                .entry((id, allocation))
                .or_insert_with(|| oracle.call_effect(mir, id, *func, allocation));
            apply_omitted_args(&mut args, &effect.omitted_args);
            let span = mir.inst_spans.get(id).copied().unwrap_or_default();
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
                substitute_constraint(analysis, constraint, &args, allocation, span);
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
                        destination: Place::Unknown,
                        span,
                        kind: PlacementViolationKind::OpaqueRetention,
                    });
                }
            }
            if !oracle.carries_placement(mir, id) {
                return Place::Static;
            }
            effect
                .result
                .map(|place| place.substitute(&args, allocation))
                .unwrap_or(Place::Unknown)
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
    let mut cell_blocks = HashMap::<Idx<Cell>, HashSet<Idx<crate::mir::Block>>>::new();
    for &block in &blocks {
        for &id in &mir.blocks[block].insts {
            let Inst::Store { cell, .. } = mir.insts[id] else {
                continue;
            };
            cell_blocks.entry(cell).or_default().insert(block);
        }
    }
    let mut flow_cells = cell_blocks
        .into_iter()
        .filter_map(|(cell, blocks)| (blocks.len() > 1).then_some(cell))
        .collect::<HashSet<_>>();
    loop {
        let mut changed = false;
        for &block in &blocks {
            for &id in &mir.blocks[block].insts {
                let Inst::Store { cell, value } = mir.insts[id] else {
                    continue;
                };
                if matches!(mir.insts[value], Inst::Load(source) if flow_cells.contains(&source)) {
                    changed |= flow_cells.insert(cell);
                }
            }
        }
        if !changed {
            break;
        }
    }
    let mut call_effects = HashMap::new();
    let mut requirements = infer_requirements(
        mir,
        context,
        oracle,
        &blocks,
        &flow_cells,
        None,
        PlacementRequirements::default(),
        &mut call_effects,
    );
    loop {
        let mut analysis = PlacementAnalysis::default();
        for (index, cell) in mir.params.iter().copied().enumerate() {
            let place = u16::try_from(index).map_or(Place::Unknown, Place::Param);
            analysis.cells.insert(cell, place);
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
                            if let Some(destination) = requirements.cells.get(*cell).copied()
                                && !is_derived_load(mir, &flow_cells, value_inst)
                            {
                                add_constraint(&mut analysis, value, destination, span);
                            }
                            changed |= merge_cell(&mut analysis.cells, *cell, value);
                        }
                        Inst::StoreGlobal { value, .. } => {
                            let value = inst_place(&analysis, *value);
                            add_constraint(&mut analysis, value, Place::Session, span);
                        }
                        Inst::StoreField { base, value, .. }
                        | Inst::StoreIndex { base, value, .. } => {
                            let value = inst_place(&analysis, *value);
                            let destination = inst_place(&analysis, *base);
                            add_constraint(&mut analysis, value, destination, span);
                        }
                        Inst::ArrayPush { arr, value, .. } => {
                            let value = inst_place(&analysis, *value);
                            let destination = inst_place(&analysis, *arr);
                            add_constraint(&mut analysis, value, destination, span);
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
                if matches!(destination, Place::Param(_) | Place::Runtime(_)) {
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
                if !values.is_empty() {
                    analysis.summary.result = Some(
                        analysis
                            .summary
                            .result
                            .map_or(result, |previous| previous.join(result)),
                    );
                }
                if let Some(destination) = context.return_destination {
                    let span = values
                        .first()
                        .and_then(|value| mir.inst_spans.get(*value).copied())
                        .unwrap_or_default();
                    add_constraint(&mut analysis, result, destination, span);
                }
            }
        }

        for (_, required) in requirements.values.iter() {
            if matches!(required, Place::Param(_) | Place::Destination) {
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

        return analysis;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
