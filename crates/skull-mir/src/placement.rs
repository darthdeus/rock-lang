use bone_semantic::prelude::{ArenaMap, Span};
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

#[derive(Default)]
struct PlacementRequirements {
    values: ArenaMap<Idx<Inst>, Place>,
    cells: ArenaMap<Idx<Cell>, Place>,
}

fn infer_requirements(
    mir: &MirFunc,
    context: AnalysisContext,
    oracle: &impl PlacementOracle,
    blocks: &[Idx<crate::mir::Block>],
) -> PlacementRequirements {
    let mut requirements = PlacementRequirements::default();
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
                            if oracle.carries_placement(mir, *value) {
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

fn operand_join(analysis: &PlacementAnalysis, inst: &Inst) -> Place {
    let mut result = Place::Static;
    inst.uses_each(&mut |operand| result = result.join(inst_place(analysis, operand)));
    result
}

fn value_place(
    mir: &MirFunc,
    analysis: &mut PlacementAnalysis,
    oracle: &impl PlacementOracle,
    context: AnalysisContext,
    requirements: &PlacementRequirements,
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
        | Inst::MakeVec(..)
        | Inst::MakeVecColumns { .. }
        | Inst::MakeVecColumnHeader { .. }
        | Inst::Lambda { .. } => allocation,
        Inst::ToString(value) => inst_place(analysis, *value).join(allocation),
        Inst::Field { base, .. }
        | Inst::TupleIndex { base, .. }
        | Inst::Index { base, .. }
        | Inst::Len { base, .. }
        | Inst::VecCheckedIndex { vec: base, .. } => inst_place(analysis, *base),
        Inst::MakeTuple(values) => values.iter().fold(Place::Static, |place, value| {
            place.join(inst_place(analysis, *value))
        }),
        Inst::MakeStruct { fields, .. }
        | Inst::MakeValueStruct { fields, .. }
        | Inst::MaterializeValueStruct { fields, .. } => {
            let initial = if oracle.is_allocation_site(mir, id) {
                allocation
            } else {
                Place::Static
            };
            fields.iter().fold(initial, |place, (_, value)| {
                place.join(inst_place(analysis, *value))
            })
        }
        Inst::MakeEnv { fields, .. } => fields.iter().fold(allocation, |place, (_, value)| {
            place.join(inst_place(analysis, *value))
        }),
        Inst::MakeEnum {
            positional, named, ..
        } => positional
            .iter()
            .chain(named.iter().map(|(_, value)| value))
            .fold(Place::Static, |place, value| {
                place.join(inst_place(analysis, *value))
            }),
        Inst::MakeEnumDyn {
            tag,
            positional,
            named,
            ..
        } => std::iter::once(tag)
            .chain(positional)
            .chain(named.iter().map(|(_, value)| value))
            .fold(Place::Static, |place, value| {
                place.join(inst_place(analysis, *value))
            }),
        Inst::Call { func, args } | Inst::CallMulti { func, args, .. } => {
            let args = args
                .iter()
                .map(|arg| inst_place(analysis, *arg))
                .collect::<Vec<_>>();
            let effect = oracle.call_effect(mir, id, *func, allocation);
            let span = mir.inst_spans.get(id).copied().unwrap_or_default();
            for store in effect.stores {
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
            for constraint in effect.constraints {
                substitute_constraint(analysis, constraint, &args, allocation, span);
            }
            if let Some(owner) = effect.owner {
                analysis
                    .call_owners
                    .insert(id, owner.substitute(&args, allocation));
            }
            for retained in effect.opaque_retains {
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
    let mut requirements = infer_requirements(mir, context, oracle, &blocks);
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
                            let value = inst_place(&analysis, *value);
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

        return analysis;
    }
}
