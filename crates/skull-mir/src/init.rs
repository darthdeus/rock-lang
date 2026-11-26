//! May-execute dependencies of global initializers, over the emitted MIR.
//! Keeping a body reachable for emission does not execute its instructions.

use crate::cfg::{reachable_blocks_from, successors};
use crate::mir::{
    Block, Cell, ContainerKind, CoroHookKind, FlatVecContents, FuncRef, Inst, Lit, MirFunc, Term,
};
use crate::reach::{PendingInit, order_global_inits};
use bone_core::hir::{BuiltinTy, TyId, TypeExpr};
use bone_core::modules::is_stdlib_module_file;
use bone_core::prelude::CompilerError;
use bone_core::workspace::Workspace;
use comfy_la_arena::Idx;
use rustc_hash::{FxHashMap, FxHashSet};
use ustr::Ustr;

type Local = u32;
const SCALAR: Local = Local::MAX;
type NodeId = usize;
type ObjectId = usize;
type BodyId = usize;
type CallId = usize;

#[derive(Clone, Copy)]
struct Operands {
    start: u32,
    end: u32,
}

impl Operands {
    fn push<T>(pool: &mut Vec<T>, values: impl IntoIterator<Item = T>) -> Self {
        let start = pool.len() as u32;
        pool.extend(values);
        Self {
            start,
            end: pool.len() as u32,
        }
    }

    fn get<T>(self, pool: &[T]) -> &[T] {
        &pool[self.start as usize..self.end as usize]
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Field {
    Name(Ustr),
    Position(usize),
    Element,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Callee {
    Named(Ustr),
    Opaque,
    Allocate,
    Identity,
    Push,
    GetIndex,
    SetIndex,
    Resume,
    Kill,
    HotReload,
    CoroutineResult,
    InspectCoroutine,
}

/// These intrinsics have typed access to a coroutine's runtime frame.
pub fn callee(ws: &Workspace, func: FuncRef, name: Option<Ustr>) -> Callee {
    let module = ws.module(func.module);
    if is_stdlib_module_file(module.file.as_str()) {
        let f = &module.functions[func.func];
        if f.annotations.iter().any(|ann| ann.is_builtin("intrinsic")) {
            match bone_core::modules::display_name(f.name.as_str()) {
                "table_new" | "map_new" | "vec_new" | "__vec_data_alloc" => {
                    return Callee::Allocate;
                }
                "transmute" => return Callee::Identity,
                "vec_push" | "push" => return Callee::Push,
                "table_get" | "__vec_data_get" => return Callee::GetIndex,
                "table_set" | "__vec_data_set" => return Callee::SetIndex,
                "resume" => return Callee::Resume,
                "kill" => return Callee::Kill,
                "fire_hot_reload" => return Callee::HotReload,
                "result" => return Callee::CoroutineResult,
                "status" | "completed" => return Callee::InspectCoroutine,
                _ => {}
            }
        }
    }
    name.map_or(Callee::Opaque, Callee::Named)
}

#[derive(Clone, Copy)]
enum Target {
    Direct(Callee),
    Value(Local),
}

enum Op {
    Read(Local, Ustr),
    Write(Ustr, Local),
    Function(Local, Callee),
    Copy(Local, Local),
    Object(Local, Option<Ustr>, Operands),
    Lambda(Local, usize),
    Field(Local, Local, Field),
    StoreField(Local, Field, Local),
    Call(Target, Operands, Operands),
    Return(Operands),
    SetInner(Local),
}

struct Region {
    params: Operands,
    ops: Vec<Op>,
    may_repeat: bool,
}

struct Body {
    name: Ustr,
    slots: usize,
    locals: Vec<Local>,
    fields: Vec<(Field, Local)>,
    regions: Vec<Region>,
    coroutine: Option<Coroutine>,
}

struct Coroutine {
    resume_arg: Option<Local>,
    defers: Vec<(usize, bool)>,
    hooks: Vec<(CoroHookKind, usize)>,
}

/// Cached with generated Lua chunks so assembling an overlay or full program
/// need not lower the bodies again. JS can build the same facts from its MIR.
#[derive(Default)]
pub struct InitBodies(Vec<Body>);

impl InitBodies {
    pub fn record(
        &mut self,
        ws: &Workspace,
        mf: &MirFunc,
        name: Ustr,
        resolve: &impl Fn(&MirFunc, Idx<Inst>, FuncRef) -> Callee,
    ) {
        let mut builder = Builder {
            ws,
            mf,
            resolve,
            locals: Vec::new(),
            fields: Vec::new(),
            regions: Vec::new(),
        };
        let coroutine = if mf.is_coroutine {
            let mut first = FxHashSet::default();
            let mut pending = vec![mf.entry];
            while let Some(block) = pending.pop() {
                if first.insert(block) && !matches!(mf.blocks[block].term, Term::Yield { .. }) {
                    successors(&mf.blocks[block].term, |next| pending.push(next));
                }
            }
            let (starts, resumes): (Vec<_>, Vec<_>) = reachable_blocks_from(mf, mf.entry)
                .into_iter()
                .partition(|block| first.contains(block));
            builder.blocks(starts.iter().copied(), &mf.params);
            builder.blocks(resumes, &[]);
            let armed: FxHashSet<_> = starts
                .iter()
                .flat_map(|&block| &mf.blocks[block].insts)
                .filter_map(|&id| match mf.insts[id] {
                    Inst::Store { cell, value }
                        if matches!(mf.insts[value], Inst::Const(Lit::Bool(true))) =>
                    {
                        Some(cell)
                    }
                    _ => None,
                })
                .collect();
            Some(Coroutine {
                resume_arg: mf.resume_arg.map(|cell| builder.cell(cell)),
                defers: mf
                    .coro_defers
                    .iter()
                    .map(|&(flag, entry)| (builder.region(entry, &[]), armed.contains(&flag)))
                    .collect(),
                hooks: mf
                    .blocks
                    .iter()
                    .flat_map(|(_, block)| &block.insts)
                    .filter_map(|&iid| match mf.insts[iid] {
                        Inst::CoroHook { kind, lambda } => match mf.insts[lambda] {
                            Inst::Lambda { entry, ref params } => {
                                Some((kind, builder.region(entry, params)))
                            }
                            _ => None,
                        },
                        _ => None,
                    })
                    .collect(),
            })
        } else {
            builder.region(mf.entry, &mf.params);
            None
        };
        self.0.push(Body {
            name,
            slots: mf.cells.len() + mf.insts.len(),
            locals: builder.locals,
            fields: builder.fields,
            regions: builder.regions,
            coroutine,
        });
        for nested in &mf.nested_fns {
            self.record(ws, nested, nested.name, resolve);
        }
    }
}

struct Builder<'a, F> {
    ws: &'a Workspace,
    mf: &'a MirFunc,
    resolve: &'a F,
    locals: Vec<Local>,
    fields: Vec<(Field, Local)>,
    regions: Vec<Region>,
}

fn raw<T>(id: Idx<T>) -> Local {
    id.into_raw().into()
}

fn scalar(ws: &Workspace, ty: TyId) -> bool {
    let TyId::Static(module, index) = ty else {
        return false;
    };
    match ws.module(module).ty_arena[index] {
        TypeExpr::Builtin(ty) => ty != BuiltinTy::Any,
        TypeExpr::Nullable(inner) => scalar(ws, ty.with_idx(inner)),
        _ => false,
    }
}

fn value_local(mf: &MirFunc, id: Idx<Inst>) -> Option<Local> {
    matches!(
        mf.insts[id],
        Inst::Global(_)
            | Inst::Load(_)
            | Inst::Call { .. }
            | Inst::CallValue { .. }
            | Inst::Lambda { .. }
            | Inst::MakeTuple(_)
            | Inst::MakeVec(_)
            | Inst::MakeFlatVec(_)
            | Inst::MakeStruct { .. }
            | Inst::MakeValueStruct { .. }
            | Inst::MakeNullableValue { .. }
            | Inst::MaterializeValueStruct { .. }
            | Inst::MakeEnum { .. }
            | Inst::MakeEnumDyn { .. }
            | Inst::MakeEnv { .. }
            | Inst::Field { .. }
            | Inst::TupleIndex { .. }
            | Inst::Index { .. }
            | Inst::VecStrideNilToFalse(_)
            | Inst::VecStrideFalseToNil(_)
    )
    .then(|| raw(id))
}

impl<F: Fn(&MirFunc, Idx<Inst>, FuncRef) -> Callee> Builder<'_, F> {
    fn cell(&self, cell: Idx<Cell>) -> Local {
        self.mf.insts.len() as Local + raw(cell)
    }

    fn region(&mut self, entry: Idx<Block>, params: &[Idx<Cell>]) -> usize {
        self.blocks(reachable_blocks_from(self.mf, entry), params)
    }

    fn blocks(
        &mut self,
        blocks: impl IntoIterator<Item = Idx<Block>>,
        params: &[Idx<Cell>],
    ) -> usize {
        let mf = self.mf;
        let index = self.regions.len();
        let offset = mf.insts.len() as Local;
        self.regions.push(Region {
            params: Operands::push(
                &mut self.locals,
                params.iter().map(|&cell| offset + raw(cell)),
            ),
            ops: Vec::new(),
            may_repeat: false,
        });
        let mut ops = Vec::new();
        let mut may_repeat = false;
        for block in blocks {
            ops.extend(
                mf.blocks[block]
                    .insts
                    .iter()
                    .filter_map(|&iid| self.operation(iid)),
            );
            let term = &mf.blocks[block].term;
            if let Term::Return(values) | Term::Yield { values, .. } = term
                && values.iter().any(|&id| value_local(mf, id).is_some())
            {
                ops.push(Op::Return(Operands::push(
                    &mut self.locals,
                    values
                        .iter()
                        .map(|&id| value_local(mf, id).unwrap_or(SCALAR)),
                )));
            }
            // Every CFG cycle has at least one edge to an earlier block.
            successors(term, |target| may_repeat |= raw(target) <= raw(block));
        }
        self.regions[index].ops = ops;
        self.regions[index].may_repeat = may_repeat;
        index
    }

    fn index_field(&self, index: Idx<Inst>, kind: ContainerKind) -> Field {
        match (&self.mf.insts[index], kind) {
            (Inst::Const(Lit::Str(name)), ContainerKind::Map | ContainerKind::Record(_)) => {
                Field::Name(*name)
            }
            (Inst::Const(Lit::Int(value)), _) if *value >= 0 => Field::Position(*value as usize),
            _ => Field::Element,
        }
    }

    fn scalar_global(&self, name: Ustr) -> bool {
        self.ws
            .globals_by_name
            .get(&name)
            .is_some_and(|&ty| scalar(self.ws, ty))
    }

    fn scalar_record(&self, name: Ustr) -> bool {
        self.ws.nominals.struct_id(name).is_some_and(|id| {
            self.ws.module(id.module).structs[id.def]
                .fields
                .iter()
                .all(|field| scalar(self.ws, field.ty))
        })
    }

    fn operation(&mut self, iid: Idx<Inst>) -> Option<Op> {
        let mf = self.mf;
        let dest = raw(iid);
        let tracked = |id| value_local(mf, id);
        let value = |id| tracked(id).unwrap_or(SCALAR);
        fn fields<'a>(
            mf: &'a MirFunc,
            fields: &'a [(Ustr, Idx<Inst>)],
        ) -> impl Iterator<Item = (Field, Local)> + 'a {
            fields
                .iter()
                .filter_map(|&(f, v)| value_local(mf, v).map(|local| (Field::Name(f), local)))
        }
        fn elements<'a>(
            mf: &'a MirFunc,
            values: &'a [Idx<Inst>],
            field: impl Fn(usize) -> Field + 'a,
        ) -> impl Iterator<Item = (Field, Local)> + 'a {
            values.iter().enumerate().filter_map(move |(index, &v)| {
                value_local(mf, v).map(|local| (field(index), local))
            })
        }
        let offset = mf.insts.len() as Local;
        let destinations = |dests: &[Idx<Cell>], pool: &mut Vec<Local>| {
            Operands::push(pool, dests.iter().map(|&v| offset + raw(v)))
        };
        let args = |args: &[Idx<Inst>], pool: &mut Vec<Local>| {
            Operands::push(pool, args.iter().copied().map(value))
        };
        let direct = |f| Target::Direct((self.resolve)(mf, iid, f));
        let result = |func: FuncRef| {
            let f = &self.ws.module(func.module).functions[func.func];
            if f.return_ty.is_some_and(|ty| scalar(self.ws, ty)) {
                SCALAR
            } else {
                dest
            }
        };
        let indirect = |v| Target::Value(raw(v));
        Some(match &mf.insts[iid] {
            Inst::Global(name) => match mf.fn_value_symbols.get(&iid) {
                Some(&func) => Op::Function(dest, (self.resolve)(mf, iid, func)),
                None => Op::Read(
                    if self.scalar_global(*name) {
                        SCALAR
                    } else {
                        dest
                    },
                    *name,
                ),
            },
            Inst::StoreGlobal { name, value } if !self.scalar_global(*name) => {
                Op::Write(*name, tracked(*value)?)
            }
            Inst::Load(cell) => Op::Copy(dest, self.cell(*cell)),
            Inst::Store { cell, value } => Op::Copy(self.cell(*cell), tracked(*value)?),
            Inst::MakeEnv { fn_ref, fields: fs } => Op::Object(
                dest,
                *fn_ref,
                Operands::push(&mut self.fields, fields(mf, fs)),
            ),
            Inst::MakeStruct { ty: Some(ty), .. } if self.scalar_record(*ty) => return None,
            Inst::MakeValueStruct { representation, .. }
            | Inst::MaterializeValueStruct { representation, .. }
                if mf.rep_arena.as_ref().is_some_and(|arena| {
                    arena.get(*representation).value_storage()
                        == Some(crate::lower::ValueStorage::CData)
                }) =>
            {
                return None;
            }
            Inst::MakeStruct { fields: fs, .. }
            | Inst::MakeValueStruct { fields: fs, .. }
            | Inst::MaterializeValueStruct { fields: fs, .. } => {
                Op::Object(dest, None, Operands::push(&mut self.fields, fields(mf, fs)))
            }
            Inst::MakeEnum {
                positional, named, ..
            }
            | Inst::MakeEnumDyn {
                positional, named, ..
            } => Op::Object(
                dest,
                None,
                Operands::push(
                    &mut self.fields,
                    elements(mf, positional, Field::Position).chain(fields(mf, named)),
                ),
            ),
            Inst::MakeTuple(values) => Op::Object(
                dest,
                None,
                Operands::push(&mut self.fields, elements(mf, values, Field::Position)),
            ),
            Inst::MakeVec(values) => Op::Object(
                dest,
                None,
                Operands::push(&mut self.fields, elements(mf, values, Field::Position)),
            ),
            Inst::MakeFlatVec(ctor) => {
                let fields = match &ctor.contents {
                    FlatVecContents::Header(_) => Operands::push(&mut self.fields, []),
                    FlatVecContents::Rows { values, .. } => {
                        Operands::push(&mut self.fields, elements(mf, values, |_| Field::Element))
                    }
                    FlatVecContents::Columns { columns, .. } => Operands::push(
                        &mut self.fields,
                        columns.iter().flat_map(|(f, values)| {
                            values.iter().map(|&v| (Field::Name(*f), raw(v)))
                        }),
                    ),
                };
                Op::Object(dest, None, fields)
            }
            Inst::Field { base, field, .. } => Op::Field(dest, raw(*base), Field::Name(*field)),
            Inst::TupleIndex { base, index } => {
                Op::Field(dest, raw(*base), Field::Position(*index))
            }
            Inst::Index { base, index, kind } => {
                Op::Field(dest, raw(*base), self.index_field(*index, *kind))
            }
            Inst::StoreField {
                base, field, value, ..
            } => Op::StoreField(raw(*base), Field::Name(*field), tracked(*value)?),
            Inst::StoreIndex {
                base,
                index,
                value,
                kind,
            } => Op::StoreField(
                raw(*base),
                self.index_field(*index, *kind),
                tracked(*value)?,
            ),
            Inst::ArrayPush { arr, value, .. } => {
                Op::StoreField(raw(*arr), Field::Element, tracked(*value)?)
            }
            Inst::MapNext {
                val_cell, table, ..
            } => Op::Field(self.cell(*val_cell), raw(*table), Field::Element),
            Inst::MakeNullableValue { value, .. }
            | Inst::VecStrideNilToFalse(value)
            | Inst::VecStrideFalseToNil(value) => Op::Copy(dest, tracked(*value)?),
            Inst::Call { func, args: a } => {
                let callee = (self.resolve)(mf, iid, *func);
                match (callee, a.as_slice()) {
                    (Callee::Allocate, _) => {
                        Op::Object(dest, None, Operands::push(&mut self.fields, []))
                    }
                    (Callee::Identity, [src]) => Op::Copy(dest, tracked(*src)?),
                    (Callee::Push, [base, src]) => {
                        Op::StoreField(raw(*base), Field::Element, tracked(*src)?)
                    }
                    (Callee::GetIndex, [base, index]) => Op::Field(
                        dest,
                        raw(*base),
                        self.index_field(*index, ContainerKind::Map),
                    ),
                    (Callee::SetIndex, [base, index, src]) => Op::StoreField(
                        raw(*base),
                        self.index_field(*index, ContainerKind::Map),
                        tracked(*src)?,
                    ),
                    _ => {
                        let result = result(*func);
                        Op::Call(
                            Target::Direct(callee),
                            args(a, &mut self.locals),
                            Operands::push(&mut self.locals, (result != SCALAR).then_some(result)),
                        )
                    }
                }
            }
            Inst::CallMulti {
                func,
                args: a,
                dests,
            } => Op::Call(
                direct(*func),
                args(a, &mut self.locals),
                destinations(dests, &mut self.locals),
            ),
            Inst::CallValue { callee, args: a } => Op::Call(
                indirect(*callee),
                args(a, &mut self.locals),
                Operands::push(&mut self.locals, [dest]),
            ),
            Inst::CallValueMulti {
                callee,
                args: a,
                dests,
            } => Op::Call(
                indirect(*callee),
                args(a, &mut self.locals),
                destinations(dests, &mut self.locals),
            ),
            Inst::Lambda { entry, params } => Op::Lambda(dest, self.region(*entry, params)),
            Inst::CoroSetInner(value) => Op::SetInner(tracked(*value)?),
            _ => return None,
        })
    }
}

#[derive(Clone, Copy)]
struct Site(BodyId, usize, usize);

type ContextId = usize;

struct Context {
    body: BodyId,
    initializer: bool,
    locals: Vec<Option<NodeId>>,
    inner: Option<NodeId>,
    handle: Option<ObjectId>,
}

#[derive(Clone, Copy)]
enum Function {
    Named(Callee, bool),
    Lambda(ContextId, usize),
    Coroutine(ContextId),
}

#[derive(Default)]
struct Object {
    function: Option<Function>,
    fields: FxHashMap<Field, NodeId>,
    all_fields: Option<NodeId>,
    callbacks: Option<NodeId>,
}

#[derive(Clone, Copy)]
enum Use {
    Copy(NodeId),
    Read(Field, NodeId),
    Write(Field, NodeId, usize, Site),
    Call(CallId),
    ContainCallbacks(NodeId),
    Escape(CallId),
    Coroutine(CallId, Callee),
}

#[derive(Clone, Copy)]
enum Source {
    Copy(NodeId),
    Field(NodeId, Field),
    Object(ContextId, Site),
}

#[derive(Default)]
struct Node {
    values: Vec<ObjectId>,
    membership: Option<FxHashSet<ObjectId>>,
    uses: Vec<Use>,
    sources: Vec<Source>,
    demanded: bool,
}

struct Call {
    root: usize,
    site: Site,
    args: Vec<NodeId>,
    dests: Vec<NodeId>,
    opaque: Option<NodeId>,
    unknown_args: Option<NodeId>,
    once: bool,
}

// Follow callable and aggregate origins only where invocation, mutation, or
// an opaque escape demands them. A callee's facts are instantiated per
// initializer and static call site: unrelated callback arguments stay separate
// while recursive calls converge on the same activation.
struct Analysis<'a> {
    bodies: Vec<&'a Body>,
    definitions: FxHashMap<Ustr, BodyId>,
    unexamined: FxHashSet<Ustr>,
    owners: &'a FxHashMap<Ustr, Ustr>,
    initialized: FxHashSet<Ustr>,
    deps: Vec<FxHashSet<Ustr>>,
    globals: FxHashMap<Ustr, NodeId>,
    functions: FxHashMap<Callee, ObjectId>,
    nodes: Vec<Node>,
    objects: Vec<Object>,
    contexts: Vec<Context>,
    calls: Vec<Call>,
    work: Vec<(Use, ObjectId)>,
    demands: Vec<NodeId>,
    pending: Vec<(usize, ContextId, usize)>,
    coroutine_calls: FxHashSet<(ContextId, CallId, Callee)>,
    resumes: FxHashMap<ContextId, Vec<CallId>>,
    kills: FxHashMap<ContextId, Vec<usize>>,
    unknown: Option<(NodeId, ObjectId)>,
    activations: FxHashMap<(usize, BodyId, usize, usize, BodyId), ContextId>,
    regions: FxHashSet<(usize, ContextId, usize)>,
    returns: FxHashMap<(usize, ContextId, usize), Vec<NodeId>>,
}

impl Analysis<'_> {
    fn node(&mut self) -> NodeId {
        let id = self.nodes.len();
        self.nodes.push(Node::default());
        id
    }

    fn add(&mut self, node: NodeId, value: ObjectId) {
        let node = &mut self.nodes[node];
        let inserted = if let Some(membership) = &mut node.membership {
            membership.insert(value)
        } else {
            !node.values.contains(&value)
        };
        if inserted {
            node.values.push(value);
            if node.values.len() == 8 {
                node.membership = Some(node.values.iter().copied().collect());
            }
            self.work
                .extend(node.uses.iter().map(|&use_| (use_, value)));
        }
    }

    fn watch(&mut self, id: NodeId, use_: Use) {
        if id == 0 {
            return;
        }
        let node = &mut self.nodes[id];
        if !std::mem::replace(&mut node.demanded, true) {
            self.demands.push(id);
        }
        self.work
            .extend(node.values.iter().map(|&value| (use_, value)));
        node.uses.push(use_);
    }

    fn source(&mut self, dest: NodeId, source: Source) {
        if self.nodes[dest].demanded {
            self.follow(dest, source);
        } else {
            self.nodes[dest].sources.push(source);
        }
    }

    fn follow(&mut self, dest: NodeId, source: Source) {
        match source {
            Source::Copy(src) => self.watch(src, Use::Copy(dest)),
            Source::Field(base, field) => self.watch(base, Use::Read(field, dest)),
            Source::Object(ctx, Site(body_id, region, op)) => {
                let body = self.bodies[body_id];
                let Op::Object(_, function, fields) = body.regions[region].ops[op] else {
                    unreachable!("a recorded object source points to its constructor");
                };
                let function = function.map(|name| Function::Named(Callee::Named(name), true));
                let object = self.object(function);
                for &(field, src) in fields.get(&body.fields) {
                    let dest = self.field(object, field);
                    let src = self.local(ctx, src);
                    self.copy(src, dest);
                }
                self.add(dest, object);
            }
        }
    }

    fn copy(&mut self, from: NodeId, to: NodeId) {
        if from != 0 && to != 0 && from != to {
            self.source(to, Source::Copy(from));
        }
    }

    fn object(&mut self, function: Option<Function>) -> ObjectId {
        let id = self.objects.len();
        self.objects.push(Object {
            function,
            ..Object::default()
        });
        id
    }

    fn function(&mut self, callee: Callee) -> ObjectId {
        if let Some(&object) = self.functions.get(&callee) {
            return object;
        }
        let object = self.object(Some(Function::Named(callee, false)));
        self.functions.insert(callee, object);
        object
    }

    fn local(&mut self, ctx: ContextId, local: Local) -> NodeId {
        if local == SCALAR {
            return 0;
        }
        if let Some(node) = self.contexts[ctx].locals[local as usize] {
            return node;
        }
        let node = self.node();
        self.contexts[ctx].locals[local as usize] = Some(node);
        node
    }

    fn field(&mut self, object: ObjectId, field: Field) -> NodeId {
        if let Some(&node) = self.objects[object].fields.get(&field) {
            return node;
        }
        let node = self.node();
        if field == Field::Element {
            let fields: Vec<_> = self.objects[object].fields.values().copied().collect();
            for field in fields {
                self.copy(node, field);
            }
        } else if let Some(&wildcard) = self.objects[object].fields.get(&Field::Element) {
            self.copy(wildcard, node);
        }
        self.objects[object].fields.insert(field, node);
        if let Some(all) = self.objects[object].all_fields {
            self.copy(node, all);
        }
        node
    }

    fn all_fields(&mut self, object: ObjectId) -> NodeId {
        if let Some(node) = self.objects[object].all_fields {
            return node;
        }
        let node = self.node();
        self.objects[object].all_fields = Some(node);
        let fields: Vec<_> = self.objects[object].fields.values().copied().collect();
        for field in fields {
            self.copy(field, node);
        }
        node
    }

    // Many opaque calls see the same aggregate, or overlapping subtrees.
    // Share each object's set of contained callables across those calls.
    fn callbacks(&mut self, object: ObjectId) -> NodeId {
        if let Some(node) = self.objects[object].callbacks {
            return node;
        }
        let node = self.node();
        self.objects[object].callbacks = Some(node);
        if self.objects[object].function.is_some() {
            self.add(node, object);
        }
        let fields = self.all_fields(object);
        self.watch(fields, Use::ContainCallbacks(node));
        node
    }

    fn global(&mut self, name: Ustr) -> NodeId {
        if let Some(&node) = self.globals.get(&name) {
            return node;
        }
        let node = self.node();
        self.globals.insert(name, node);
        node
    }

    fn dependency(&mut self, root: usize, name: Ustr) {
        let owner = self.owners.get(&name).copied().unwrap_or(name);
        if self.initialized.contains(&owner) {
            self.deps[root].insert(owner);
        }
    }

    fn context(&mut self, body: BodyId, initializer: bool) -> ContextId {
        let ctx = self.contexts.len();
        self.contexts.push(Context {
            body,
            initializer,
            locals: vec![None; self.bodies[body].slots],
            inner: None,
            handle: None,
        });
        ctx
    }

    fn activation(&mut self, body: BodyId, call: CallId) -> ContextId {
        let root = self.calls[call].root;
        let Site(parent, region, op) = self.calls[call].site;
        let key = (root, parent, region, op, body);
        if let Some(&ctx) = self.activations.get(&key) {
            return ctx;
        }
        let ctx = self.context(body, false);
        self.activations.insert(key, ctx);
        ctx
    }

    fn bind(&mut self, ctx: ContextId, region: usize, call: CallId, prefix: Option<ObjectId>) {
        let body = self.bodies[self.contexts[ctx].body];
        let params = body.regions[region].params.get(&body.locals);
        let unknown_args = self.calls[call].unknown_args;
        let mut params = params.iter().copied();
        if let Some(object) = prefix
            && let Some(param) = params.next()
        {
            let dest = self.local(ctx, param);
            self.add(dest, object);
        }
        for (index, param) in params.enumerate() {
            if let Some(src) = self.calls[call].args.get(index).copied().or(unknown_args) {
                let dest = self.local(ctx, param);
                self.copy(src, dest);
            }
        }
    }

    fn result(&mut self, root: usize, ctx: ContextId, region: usize, lane: usize) -> NodeId {
        let key = (root, ctx, region);
        while self.returns.get(&key).map_or(0, Vec::len) <= lane {
            let node = self.node();
            self.returns.entry(key).or_default().push(node);
        }
        self.returns[&key][lane]
    }

    fn results(&mut self, ctx: ContextId, region: usize, call: CallId) {
        let root = self.calls[call].root;
        for lane in 0..self.calls[call].dests.len() {
            let dest = self.calls[call].dests[lane];
            let from = self.result(root, ctx, region, lane);
            self.copy(from, dest);
        }
    }

    fn execute(&mut self, ctx: ContextId, region: usize, root: usize) {
        if self.regions.insert((root, ctx, region)) {
            self.pending.push((root, ctx, region));
        }
    }

    fn apply(&mut self, ctx: ContextId, root: usize, site: Site, op: &Op) {
        let body = self.bodies[self.contexts[ctx].body];
        match *op {
            Op::Read(SCALAR, name) => self.dependency(root, name),
            Op::Read(dest, name) => {
                let dest = self.local(ctx, dest);
                if self.definitions.contains_key(&name) {
                    let value = self.function(Callee::Named(name));
                    self.add(dest, value);
                } else {
                    assert!(
                        !self.unexamined.contains(&name),
                        "initializer value {name} is absent from the concrete dependency closure"
                    );
                    self.dependency(root, name);
                    let src = self.global(name);
                    self.copy(src, dest);
                    // Compiler runtime functions (for example the defer stack)
                    // have no Rock body. Their arguments can escape.
                    if !self
                        .initialized
                        .contains(&self.owners.get(&name).copied().unwrap_or(name))
                    {
                        let (_, unknown) = self.unknown();
                        self.add(dest, unknown);
                    }
                }
            }
            Op::Write(name, src) => {
                let src = self.local(ctx, src);
                let dest = self.global(name);
                self.copy(src, dest);
            }
            Op::Function(dest, callee) => {
                let dest = self.local(ctx, dest);
                let object = self.function(callee);
                self.add(dest, object);
            }
            Op::Copy(dest, src) => {
                let dest = self.local(ctx, dest);
                let src = self.local(ctx, src);
                self.copy(src, dest);
            }
            Op::Object(dest, _, _) => {
                let dest = self.local(ctx, dest);
                self.source(dest, Source::Object(ctx, site));
            }
            Op::Lambda(dest, region) => {
                let object = self.object(Some(Function::Lambda(ctx, region)));
                let dest = self.local(ctx, dest);
                self.add(dest, object);
            }
            Op::Field(dest, base, field) => {
                let dest = self.local(ctx, dest);
                let base = self.local(ctx, base);
                self.source(dest, Source::Field(base, field));
            }
            Op::StoreField(base, field, src) => {
                let src = self.local(ctx, src);
                let base = self.local(ctx, base);
                self.watch(base, Use::Write(field, src, root, site));
            }
            Op::Call(target, args, dests) => {
                let call = self.calls.len();
                let args = args
                    .get(&body.locals)
                    .iter()
                    .map(|&arg| self.local(ctx, arg))
                    .collect();
                let dests = dests
                    .get(&body.locals)
                    .iter()
                    .map(|&dest| self.local(ctx, dest))
                    .collect();
                self.calls.push(Call {
                    root,
                    site,
                    args,
                    dests,
                    opaque: None,
                    unknown_args: None,
                    once: self.contexts[ctx].initializer
                        && site.1 == 0
                        && !body.regions[0].may_repeat,
                });
                match target {
                    Target::Direct(callee) => self.call(callee, call, None),
                    Target::Value(value) => {
                        let value = self.local(ctx, value);
                        self.watch(value, Use::Call(call));
                    }
                }
            }
            Op::Return(values) => {
                for (lane, &src) in values.get(&body.locals).iter().enumerate() {
                    let src = self.local(ctx, src);
                    let region = if site.1 == 1 && body.coroutine.is_some() {
                        0
                    } else {
                        site.1
                    };
                    let dest = self.result(root, ctx, region, lane);
                    self.copy(src, dest);
                }
            }
            Op::SetInner(src) => {
                let inner = self.inner(ctx);
                let src = self.local(ctx, src);
                self.copy(src, inner);
            }
        }
    }

    fn call(&mut self, callee: Callee, call: CallId, env: Option<ObjectId>) {
        match callee {
            Callee::Named(name) => {
                let Some(&body) = self.definitions.get(&name) else {
                    assert!(
                        !self.unexamined.contains(&name),
                        "initializer callee {name} is absent from the concrete dependency closure"
                    );
                    self.opaque(call);
                    return;
                };
                let ctx = self.activation(body, call);
                self.bind(ctx, 0, call, env);
                if self.bodies[body].coroutine.is_some() {
                    if let Some(&dest) = self.calls[call].dests.first() {
                        let object = match self.contexts[ctx].handle {
                            Some(object) => object,
                            None => {
                                let object = self.object(Some(Function::Coroutine(ctx)));
                                self.contexts[ctx].handle = Some(object);
                                object
                            }
                        };
                        self.add(dest, object);
                    }
                } else {
                    let root = self.calls[call].root;
                    self.execute(ctx, 0, root);
                    self.results(ctx, 0, call);
                }
            }
            Callee::Opaque
            | Callee::Allocate
            | Callee::Identity
            | Callee::Push
            | Callee::GetIndex
            | Callee::SetIndex => self.opaque(call),
            Callee::InspectCoroutine => {}
            action => {
                if let Some(&receiver) = self.calls[call].args.first() {
                    self.watch(receiver, Use::Coroutine(call, action));
                }
            }
        }
    }

    fn unknown(&mut self) -> (NodeId, ObjectId) {
        if let Some(unknown) = self.unknown {
            return unknown;
        }
        let node = self.node();
        let object = self.object(Some(Function::Named(Callee::Opaque, false)));
        self.add(node, object);
        self.unknown = Some((node, object));
        (node, object)
    }

    fn opaque(&mut self, call: CallId) {
        if self.calls[call].opaque.is_some() {
            return;
        }
        self.calls[call].opaque = Some(0);
        if self.calls[call].args.iter().any(|&arg| arg != 0) {
            let escaped = self.node();
            self.calls[call].opaque = Some(escaped);
            self.watch(escaped, Use::Escape(call));
            for index in 0..self.calls[call].args.len() {
                let arg = self.calls[call].args[index];
                self.watch(arg, Use::ContainCallbacks(escaped));
            }
        }
        // A callable crossing an opaque boundary is charged to the escape.
        // Reusing its enclosing object's fields as the opaque result would
        // conflate unrelated objects (even scalar-returning foreign calls).
        let (_, unknown) = self.unknown();
        for index in 0..self.calls[call].dests.len() {
            let dest = self.calls[call].dests[index];
            self.add(dest, unknown);
        }
    }

    fn escape_store(&mut self, src: NodeId, root: usize, site: Site) {
        let call = self.calls.len();
        self.calls.push(Call {
            root,
            site,
            args: vec![src],
            dests: Vec::new(),
            opaque: None,
            unknown_args: None,
            once: false,
        });
        self.opaque(call);
    }

    fn invoke(&mut self, object: ObjectId, call: CallId) {
        match self.objects[object].function {
            Some(Function::Named(callee, capture)) => {
                self.call(callee, call, capture.then_some(object));
            }
            Some(Function::Lambda(ctx, region)) => {
                let root = self.calls[call].root;
                self.bind(ctx, region, call, None);
                self.execute(ctx, region, root);
                self.results(ctx, region, call);
            }
            Some(Function::Coroutine(ctx)) => self.coroutine(ctx, call, Callee::Resume),
            None => {}
        }
    }

    fn coroutine(&mut self, ctx: ContextId, call: CallId, action: Callee) {
        if !self.coroutine_calls.insert((ctx, call, action)) {
            return;
        }
        let root = self.calls[call].root;
        let body = self.contexts[ctx].body;
        let coroutine = self.bodies[body].coroutine.as_ref().unwrap();
        if matches!(action, Callee::Resume | Callee::Opaque) {
            if let Some(arg) = coroutine.resume_arg
                && let Some(&src) = self.calls[call].args.get(1)
            {
                let dest = self.local(ctx, arg);
                self.copy(src, dest);
            }
            self.execute(ctx, 0, root);
            self.results(ctx, 0, call);
            let calls = self.resumes.entry(ctx).or_default();
            calls.push(call);
            let kills = self.kills.get(&ctx).cloned().unwrap_or_default();
            let may_resume_again =
                calls.len() > 1 || !self.calls[call].once || matches!(action, Callee::Opaque);
            let resume_roots: Vec<_> = if may_resume_again {
                calls.iter().map(|&id| self.calls[id].root).collect()
            } else {
                vec![root]
            };
            for root in resume_roots {
                if may_resume_again {
                    self.execute(ctx, 1, root);
                }
                for &(region, armed_on_start) in &coroutine.defers {
                    if may_resume_again || armed_on_start {
                        self.execute(ctx, region, root);
                        for &kill in &kills {
                            self.execute(ctx, region, kill);
                        }
                    }
                }
            }
        }
        if matches!(action, Callee::CoroutineResult) {
            // Results exist only after some initializer resumed this handle.
            // The caller's argument already depends on whichever global
            // publishes it.
            let roots = self.deps.len();
            for prior in 0..roots {
                if let Some(&dest) = self.calls[call].dests.first() {
                    let src = self.result(prior, ctx, 0, 0);
                    self.copy(src, dest);
                }
            }
        }
        if matches!(action, Callee::Kill) {
            self.kills.entry(ctx).or_default().push(root);
            if let Some(calls) = self.resumes.get(&ctx) {
                let may_resume_again = calls.len() > 1 || !self.calls[calls[0]].once;
                for &(region, armed_on_start) in &coroutine.defers {
                    if may_resume_again || armed_on_start {
                        self.execute(ctx, region, root);
                    }
                }
            }
        }
        for &(kind, region) in &coroutine.hooks {
            if matches!(
                (kind, action),
                (_, Callee::Opaque)
                    | (CoroHookKind::OnResume, Callee::Resume)
                    | (CoroHookKind::OnKilled, Callee::Kill | Callee::Resume)
                    | (
                        CoroHookKind::OnHotReload | CoroHookKind::OnOwnedHotReload,
                        Callee::HotReload
                    )
            ) {
                self.execute(ctx, region, root);
            }
        }
        if matches!(action, Callee::Kill | Callee::Opaque) {
            let inner = self.inner(ctx);
            self.watch(inner, Use::Coroutine(call, action));
        }
    }

    fn inner(&mut self, ctx: ContextId) -> NodeId {
        if let Some(node) = self.contexts[ctx].inner {
            return node;
        }
        let node = self.node();
        self.contexts[ctx].inner = Some(node);
        node
    }

    fn propagate(&mut self) {
        loop {
            while let Some((root, ctx, region)) = self.pending.pop() {
                let body = self.contexts[ctx].body;
                for (i, op) in self.bodies[body].regions[region].ops.iter().enumerate() {
                    self.apply(ctx, root, Site(body, region, i), op);
                }
            }
            while let Some(dest) = self.demands.pop() {
                for source in std::mem::take(&mut self.nodes[dest].sources) {
                    self.follow(dest, source);
                }
            }
            let Some((use_, object)) = self.work.pop() else {
                break;
            };
            match use_ {
                Use::Copy(dest) => self.add(dest, object),
                Use::Read(field, dest) => {
                    if let Some((_, unknown)) = self.unknown
                        && object == unknown
                    {
                        self.add(dest, unknown);
                        continue;
                    }
                    let src = if field == Field::Element {
                        self.all_fields(object)
                    } else {
                        self.field(object, field)
                    };
                    self.copy(src, dest);
                    if let (Some(Function::Coroutine(_)), Field::Name(name)) =
                        (self.objects[object].function, field)
                        && matches!(name.as_str(), "__resume" | "__advance")
                    {
                        self.add(dest, object);
                    }
                }
                Use::Write(field, src, root, site) => {
                    if self.unknown.is_some_and(|(_, id)| id == object) {
                        self.escape_store(src, root, site);
                    } else {
                        let dest = self.field(object, field);
                        self.copy(src, dest);
                    }
                }
                Use::Call(call) => self.invoke(object, call),
                Use::Coroutine(call, action) => {
                    if let Some(Function::Coroutine(ctx)) = self.objects[object].function {
                        self.coroutine(ctx, call, action);
                    }
                }
                Use::ContainCallbacks(dest) => {
                    if self.unknown.is_none_or(|(_, id)| id != object) {
                        let src = self.callbacks(object);
                        self.copy(src, dest);
                    }
                }
                Use::Escape(call) => {
                    let dest = self.calls[call].opaque.unwrap();
                    if let Some(Function::Coroutine(ctx)) = self.objects[object].function {
                        self.coroutine(ctx, call, Callee::Opaque);
                    } else if match self.objects[object].function {
                        Some(Function::Lambda(..)) => true,
                        Some(Function::Named(Callee::Named(name), _)) => {
                            self.definitions.contains_key(&name)
                        }
                        _ => false,
                    } {
                        let (unknown, _) = self.unknown();
                        let returned = self.node();
                        self.watch(returned, Use::ContainCallbacks(dest));
                        let outer = &self.calls[call];
                        let inner = self.calls.len();
                        self.calls.push(Call {
                            root: outer.root,
                            site: outer.site,
                            args: Vec::new(),
                            dests: vec![returned],
                            opaque: None,
                            unknown_args: Some(unknown),
                            once: false,
                        });
                        self.invoke(object, inner);
                    }
                }
            }
        }
    }
}

pub struct Initializer<T> {
    pub name: Ustr,
    pub body: Ustr,
    /// Storage lanes in the initializer's return order. Empty for boxed globals.
    pub leaves: Vec<Ustr>,
    pub item: T,
}

pub fn order<'a, T>(
    inits: Vec<Initializer<T>>,
    summaries: impl IntoIterator<Item = &'a InitBodies>,
    unexamined: impl IntoIterator<Item = Ustr>,
) -> Result<Vec<T>, CompilerError> {
    let owners = inits
        .iter()
        .flat_map(|init| init.leaves.iter().map(move |&leaf| (leaf, init.name)))
        .collect();
    let bodies: Vec<_> = summaries.into_iter().flat_map(|facts| &facts.0).collect();
    let mut analysis = Analysis {
        definitions: bodies
            .iter()
            .enumerate()
            .map(|(id, body)| (body.name, id))
            .collect(),
        bodies,
        unexamined: unexamined.into_iter().collect(),
        owners: &owners,
        initialized: inits.iter().map(|init| init.name).collect(),
        deps: vec![FxHashSet::default(); inits.len()],
        globals: FxHashMap::default(),
        functions: FxHashMap::default(),
        nodes: vec![Node::default()],
        objects: Vec::new(),
        contexts: Vec::new(),
        calls: Vec::new(),
        work: Vec::new(),
        demands: Vec::new(),
        pending: Vec::new(),
        coroutine_calls: FxHashSet::default(),
        resumes: FxHashMap::default(),
        kills: FxHashMap::default(),
        unknown: None,
        activations: FxHashMap::default(),
        regions: FxHashSet::default(),
        returns: FxHashMap::default(),
    };
    for (root, init) in inits.iter().enumerate() {
        let Some(&body) = analysis.definitions.get(&init.body) else {
            continue;
        };
        if analysis.bodies[body].regions[0].ops.is_empty() {
            continue;
        }
        let ctx = analysis.context(body, true);
        analysis.execute(ctx, 0, root);
        let leaves = if init.leaves.is_empty() {
            std::slice::from_ref(&init.name)
        } else {
            init.leaves.as_slice()
        };
        for (lane, &name) in leaves.iter().enumerate() {
            let src = analysis.result(root, ctx, 0, lane);
            let dest = analysis.global(name);
            analysis.copy(src, dest);
        }
    }
    analysis.propagate();
    order_global_inits(
        inits
            .into_iter()
            .zip(analysis.deps)
            .map(|(init, deps)| PendingInit {
                name: init.name,
                deps: deps.into_iter().collect(),
                item: init.item,
            })
            .collect(),
        &owners,
    )
    .map_err(cycle_error)
}

pub fn cycle_error(stuck: Vec<Ustr>) -> CompilerError {
    let names: Vec<_> = stuck.iter().map(|name| name.as_str()).collect();
    CompilerError::new_no_location(format!(
        "global initializers are mutually recursive: {}",
        names.join(", ")
    ))
}
