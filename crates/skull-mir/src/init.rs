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
    All,
    Callbacks,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Callee {
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
fn classify(ws: &Workspace, func: FuncRef, name: Option<Ustr>) -> Callee {
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
        resolve: &impl Fn(&MirFunc, Idx<Inst>, FuncRef) -> Option<Ustr>,
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
            let blocks = reachable_blocks_from(mf, mf.entry);
            let mut first = vec![false; mf.blocks.len()];
            let mut pending = vec![mf.entry];
            while let Some(block) = pending.pop() {
                if !std::mem::replace(&mut first[raw(block) as usize], true)
                    && !matches!(mf.blocks[block].term, Term::Yield { .. })
                {
                    successors(&mf.blocks[block].term, |next| pending.push(next));
                }
            }
            // Separate blocks before the first possible suspension from later ones.
            let (starts, resumes): (Vec<_>, Vec<_>) = blocks
                .into_iter()
                .partition(|&block| first[raw(block) as usize]);
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

impl<F: Fn(&MirFunc, Idx<Inst>, FuncRef) -> Option<Ustr>> Builder<'_, F> {
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
        let index = self.regions.len();
        let offset = self.mf.insts.len() as Local;
        self.regions.push(Region {
            params: Operands::push(&mut self.locals, params.iter().map(|&id| offset + raw(id))),
            ops: Vec::new(),
            may_repeat: false,
        });
        let mut ops = Vec::new();
        let mut may_repeat = false;
        for block in blocks {
            ops.extend(
                self.mf.blocks[block]
                    .insts
                    .iter()
                    .filter_map(|&id| self.operation(id)),
            );
            let term = &self.mf.blocks[block].term;
            if let Term::Return(values) | Term::Yield { values, .. } = term
                && values.iter().any(|&id| value_local(self.mf, id).is_some())
            {
                ops.push(Op::Return(Operands::push(
                    &mut self.locals,
                    values
                        .iter()
                        .map(|&id| value_local(self.mf, id).unwrap_or(SCALAR)),
                )));
            }
            // Every CFG cycle includes an edge to an earlier block.
            successors(term, |target| may_repeat |= raw(target) <= raw(block));
        }
        self.regions[index].ops = ops;
        self.regions[index].may_repeat = may_repeat;
        index
    }

    fn index(&self, id: Idx<Inst>, kind: ContainerKind) -> Field {
        match (&self.mf.insts[id], kind) {
            (Inst::Const(Lit::Str(name)), ContainerKind::Map | ContainerKind::Record(_)) => {
                Field::Name(*name)
            }
            (Inst::Const(Lit::Int(value)), _) if *value >= 0 => Field::Position(*value as usize),
            _ => Field::Element,
        }
    }

    fn resolve(&self, id: Idx<Inst>, func: FuncRef) -> Callee {
        classify(self.ws, func, (self.resolve)(self.mf, id, func))
    }

    fn operation(&mut self, id: Idx<Inst>) -> Option<Op> {
        let mf = self.mf;
        let dest = raw(id);
        let tracked = |id| value_local(mf, id);
        let scalar_global = |name| {
            self.ws
                .globals_by_name
                .get(&name)
                .is_some_and(|&ty| scalar(self.ws, ty))
        };
        let args = |pool, args: &[Idx<Inst>]| {
            Operands::push(pool, args.iter().map(|&id| tracked(id).unwrap_or(SCALAR)))
        };
        let field = |base, field| Op::Field(dest, raw(base), field);
        let store = |base, field, src| Some(Op::StoreField(raw(base), field, tracked(src)?));

        Some(match &mf.insts[id] {
            Inst::Global(name) => match mf.fn_value_symbols.get(&id) {
                Some(&func) => Op::Function(dest, self.resolve(id, func)),
                None => Op::Read(if scalar_global(*name) { SCALAR } else { dest }, *name),
            },
            Inst::StoreGlobal { name, value } if !scalar_global(*name) => {
                Op::Write(*name, tracked(*value)?)
            }
            Inst::Load(cell) => Op::Copy(dest, self.cell(*cell)),
            Inst::Store { cell, value } => Op::Copy(self.cell(*cell), tracked(*value)?),
            Inst::Field {
                base, field: name, ..
            } => field(*base, Field::Name(*name)),
            Inst::TupleIndex { base, index } => field(*base, Field::Position(*index)),
            Inst::Index { base, index, kind } => field(*base, self.index(*index, *kind)),
            Inst::StoreField {
                base, field, value, ..
            } => return store(*base, Field::Name(*field), *value),
            Inst::StoreIndex {
                base,
                index,
                value,
                kind,
            } => return store(*base, self.index(*index, *kind), *value),
            Inst::ArrayPush { arr, value, .. } => return store(*arr, Field::Element, *value),
            Inst::MapNext {
                val_cell, table, ..
            } => Op::Field(self.cell(*val_cell), raw(*table), Field::Element),
            Inst::MakeNullableValue { value, .. }
            | Inst::VecStrideNilToFalse(value)
            | Inst::VecStrideFalseToNil(value) => Op::Copy(dest, tracked(*value)?),
            Inst::CoroSetInner(value) => {
                Op::Copy((mf.insts.len() + mf.cells.len()) as Local, tracked(*value)?)
            }
            Inst::Lambda { entry, params } => Op::Lambda(dest, self.region(*entry, params)),
            Inst::Call {
                func,
                args: operands,
            } => {
                let callee = self.resolve(id, *func);
                match (callee, operands.as_slice()) {
                    (Callee::Allocate, _) => self.object(dest, None, &[], &[], Field::Position),
                    (Callee::Identity, [src]) => Op::Copy(dest, tracked(*src)?),
                    (Callee::Push, [base, src]) => return store(*base, Field::Element, *src),
                    (Callee::GetIndex, [base, index]) => {
                        field(*base, self.index(*index, ContainerKind::Map))
                    }
                    (Callee::SetIndex, [base, index, src]) => {
                        return store(*base, self.index(*index, ContainerKind::Map), *src);
                    }
                    _ => {
                        let f = &self.ws.module(func.module).functions[func.func];
                        let result =
                            (!f.return_ty.is_some_and(|ty| scalar(self.ws, ty))).then_some(dest);
                        Op::Call(
                            Target::Direct(callee),
                            args(&mut self.locals, operands),
                            Operands::push(&mut self.locals, result),
                        )
                    }
                }
            }
            // The lookup has no effects. The boxed branch models the same
            // callback and argument flow as the scalar branch.
            inst @ Inst::CallValue { .. }
                if crate::lower::scalar_entry_lookup_arg(mf, inst).is_some() =>
            {
                return None;
            }
            Inst::CallValue {
                callee,
                args: operands,
            } => Op::Call(
                Target::Value(raw(*callee)),
                args(&mut self.locals, operands),
                Operands::push(&mut self.locals, [dest]),
            ),
            Inst::CallMulti {
                args: operands,
                dests,
                ..
            }
            | Inst::CallValueMulti {
                args: operands,
                dests,
                ..
            } => {
                let target = match mf.insts[id] {
                    Inst::CallMulti { func, .. } => Target::Direct(self.resolve(id, func)),
                    Inst::CallValueMulti { callee, .. } => Target::Value(raw(callee)),
                    _ => unreachable!(),
                };
                let offset = mf.insts.len() as Local;
                Op::Call(
                    target,
                    args(&mut self.locals, operands),
                    Operands::push(
                        &mut self.locals,
                        dests.iter().map(|&cell| offset + raw(cell)),
                    ),
                )
            }
            Inst::MakeStruct { ty: Some(ty), .. }
                if self.ws.nominals.struct_id(*ty).is_some_and(|s| {
                    self.ws.module(s.module).structs[s.def]
                        .fields
                        .iter()
                        .all(|f| scalar(self.ws, f.ty))
                }) =>
            {
                return None;
            }
            Inst::MakeValueStruct { representation, .. }
            | Inst::MaterializeValueStruct { representation, .. }
                if mf.rep_arena.as_ref().is_some_and(|arena| {
                    arena.get(*representation).value_storage()
                        == Some(crate::lower::ValueStorage::CData)
                }) =>
            {
                return None;
            }
            Inst::MakeStruct { fields, .. }
            | Inst::MakeValueStruct { fields, .. }
            | Inst::MaterializeValueStruct { fields, .. } => {
                self.object(dest, None, &[], fields, Field::Position)
            }
            Inst::MakeEnv { fn_ref, fields } => {
                self.object(dest, *fn_ref, &[], fields, Field::Position)
            }
            Inst::MakeEnum {
                positional, named, ..
            }
            | Inst::MakeEnumDyn {
                positional, named, ..
            } => self.object(dest, None, positional, named, Field::Position),
            Inst::MakeTuple(values) | Inst::MakeVec(values) => {
                self.object(dest, None, values, &[], Field::Position)
            }
            Inst::MakeFlatVec(ctor) => match &ctor.contents {
                FlatVecContents::Header(_) => self.object(dest, None, &[], &[], Field::Position),
                FlatVecContents::Rows { values, .. } => {
                    self.object(dest, None, values, &[], |_| Field::Element)
                }
                FlatVecContents::Columns { columns, .. } => {
                    let fields = columns
                        .iter()
                        .flat_map(|(f, values)| values.iter().map(|&v| (Field::Name(*f), raw(v))));
                    Op::Object(dest, None, Operands::push(&mut self.fields, fields))
                }
            },
            _ => return None,
        })
    }

    fn object(
        &mut self,
        dest: Local,
        function: Option<Ustr>,
        positional: &[Idx<Inst>],
        named: &[(Ustr, Idx<Inst>)],
        index: impl Fn(usize) -> Field,
    ) -> Op {
        let mf = self.mf;
        let fields = positional
            .iter()
            .enumerate()
            .map(|(i, &v)| (index(i), v))
            .chain(named.iter().map(|&(f, v)| (Field::Name(f), v)))
            .filter_map(|(f, v)| Some((f, value_local(mf, v)?)));
        Op::Object(dest, function, Operands::push(&mut self.fields, fields))
    }
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
struct Site(Ustr, usize, usize);

type ContextId = usize;
type Root = usize;

struct Context<'a> {
    body: &'a Body,
    locals: Vec<NodeId>,
    coroutine: Option<Box<CoroutineState>>,
}

#[derive(Default)]
struct CoroutineState {
    handle: Option<ObjectId>,
    actions: FxHashSet<(CallId, Callee)>,
    resumes: Vec<CallId>,
    kills: Vec<Root>,
}

#[derive(Clone, Copy)]
enum Function {
    Named(Callee, bool),
    Closure(ContextId, usize),
    Coroutine(ContextId),
}

#[derive(Default)]
struct Object {
    function: Option<Function>,
    fields: FxHashMap<Field, NodeId>,
}

#[derive(Clone, Copy)]
enum Use {
    Copy(NodeId),
    Read(Field, NodeId),
    Write(Field, NodeId, Root, Site),
    Call(CallId),
    Collect(NodeId),
    Escape(CallId),
    Coroutine(CallId, Callee),
}

#[derive(Default)]
struct RegionState {
    entered: bool,
    results: Vec<NodeId>,
}

#[derive(Default)]
struct Node {
    values: Vec<ObjectId>,
    membership: Option<FxHashSet<ObjectId>>,
    uses: Vec<Use>,
}

struct Call {
    root: Root,
    site: Site,
    args: Vec<NodeId>,
    dests: Vec<NodeId>,
    escaped: Option<NodeId>,
    unknown_args: bool,
    once: bool,
}

// Activate a body per initializer and static call site. Each activation adds
// value-flow edges once. New values propagate only along the registered edges.
#[derive(Default)]
struct Analysis<'a> {
    bodies: FxHashMap<Ustr, &'a Body>,
    unexamined: FxHashSet<Ustr>,
    owners: FxHashMap<Ustr, Ustr>,
    deps: Vec<FxHashSet<Ustr>>,
    globals: FxHashMap<Ustr, NodeId>,
    functions: FxHashMap<Callee, ObjectId>,
    nodes: Vec<Node>,
    objects: Vec<Object>,
    contexts: Vec<Context<'a>>,
    calls: Vec<Call>,
    work: Vec<(Use, ObjectId)>,
    pending: Vec<(Root, ContextId, usize)>,
    activations: FxHashMap<(Root, Site, Ustr), ContextId>,
    regions: FxHashMap<(Root, ContextId, usize), RegionState>,
}

const UNKNOWN: ObjectId = 0;
const UNKNOWN_ARG: NodeId = 1;

impl<'a> Analysis<'a> {
    fn node(&mut self) -> NodeId {
        self.nodes.push(Node::default());
        self.nodes.len() - 1
    }

    fn add(&mut self, dest: NodeId, value: ObjectId) {
        let node = &mut self.nodes[dest];
        if node
            .membership
            .as_mut()
            .map_or_else(|| !node.values.contains(&value), |set| set.insert(value))
        {
            node.values.push(value);
            if node.values.len() == 8 {
                node.membership = Some(node.values.iter().copied().collect());
            }
            self.work
                .extend(node.uses.iter().map(|&use_| (use_, value)));
        }
    }

    fn watch(&mut self, src: NodeId, use_: Use) {
        if src == 0 {
            return;
        }
        let node = &mut self.nodes[src];
        self.work
            .extend(node.values.iter().map(|&value| (use_, value)));
        node.uses.push(use_);
    }

    fn copy(&mut self, src: NodeId, dest: NodeId) {
        if dest != 0 && src != dest {
            self.watch(src, Use::Copy(dest));
        }
    }

    fn named(&mut self, callee: Callee) -> ObjectId {
        *self.functions.entry(callee).or_insert_with(|| {
            self.objects.push(Object {
                function: Some(Function::Named(callee, false)),
                ..Object::default()
            });
            self.objects.len() - 1
        })
    }

    fn object(&mut self, function: Option<Function>) -> ObjectId {
        self.objects.push(Object {
            function,
            ..Object::default()
        });
        self.objects.len() - 1
    }

    fn field(&mut self, object: ObjectId, field: Field) -> NodeId {
        if let Some(&node) = self.objects[object].fields.get(&field) {
            return node;
        }
        let node = self.node();
        let fields = &self.objects[object].fields;
        match field {
            Field::All => {
                let members: Vec<_> = fields.values().copied().collect();
                for member in members {
                    self.copy(member, node);
                }
            }
            Field::Callbacks => {
                if self.objects[object].function.is_some() {
                    self.add(node, object);
                }
                let all = self.field(object, Field::All);
                self.watch(all, Use::Collect(node));
            }
            Field::Element => {
                let members: Vec<_> = fields
                    .iter()
                    .filter_map(|(key, &node)| {
                        (!matches!(key, Field::All | Field::Callbacks)).then_some(node)
                    })
                    .collect();
                for member in members {
                    self.copy(node, member);
                }
            }
            _ => {
                if let Some(&wildcard) = fields.get(&Field::Element) {
                    self.copy(wildcard, node);
                }
            }
        }
        if !matches!(field, Field::All | Field::Callbacks)
            && let Some(&all) = self.objects[object].fields.get(&Field::All)
        {
            self.copy(node, all);
        }
        self.objects[object].fields.insert(field, node);
        node
    }

    fn local(&mut self, ctx: ContextId, local: Local) -> NodeId {
        if local == SCALAR {
            return 0;
        }
        let locals = &mut self.contexts[ctx].locals;
        let node = &mut locals[local as usize];
        if *node == 0 {
            *node = self.nodes.len();
            self.nodes.push(Node::default());
        }
        *node
    }

    fn global(&mut self, name: Ustr) -> NodeId {
        if let Some(&node) = self.globals.get(&name) {
            return node;
        }
        let node = self.node();
        self.globals.insert(name, node);
        node
    }

    fn result(&mut self, key: (Root, ContextId, usize), lane: usize) -> NodeId {
        let nodes = &mut self.regions.entry(key).or_default().results;
        while nodes.len() <= lane {
            nodes.push(self.nodes.len());
            self.nodes.push(Node::default());
        }
        nodes[lane]
    }

    fn context(&mut self, body: &'a Body) -> ContextId {
        self.contexts.push(Context {
            body,
            locals: vec![0; body.slots + 1],
            coroutine: body.coroutine.as_ref().map(|_| Box::default()),
        });
        self.contexts.len() - 1
    }

    fn run(&mut self, root: Root, ctx: ContextId, region: usize) {
        let key = (root, ctx, region);
        if !std::mem::replace(&mut self.regions.entry(key).or_default().entered, true) {
            self.pending.push(key);
        }
    }

    fn apply(&mut self, (root, ctx, region): (Root, ContextId, usize), once: bool) {
        let body = self.contexts[ctx].body;
        for (i, op) in body.regions[region].ops.iter().enumerate() {
            let site = Site(body.name, region, i);
            match *op {
                Op::Read(SCALAR, name) => self.deps[root].extend(self.owners.get(&name).copied()),
                Op::Read(dest, name) => {
                    let dest = self.local(ctx, dest);
                    if self.bodies.contains_key(&name) {
                        let object = self.named(Callee::Named(name));
                        self.add(dest, object);
                    } else {
                        assert!(
                            !self.unexamined.contains(&name),
                            "initializer value {name} is absent from the concrete dependency closure"
                        );
                        if let Some(&owner) = self.owners.get(&name) {
                            self.deps[root].insert(owner);
                        } else {
                            self.add(dest, UNKNOWN);
                        }
                        let src = self.global(name);
                        self.copy(src, dest);
                    }
                }
                Op::Write(name, src) => {
                    let src = self.local(ctx, src);
                    let dest = self.global(name);
                    self.copy(src, dest);
                }
                Op::Copy(dest, src) => {
                    let src = self.local(ctx, src);
                    let dest = self.local(ctx, dest);
                    self.copy(src, dest);
                }
                Op::Function(dest, callee) => {
                    let dest = self.local(ctx, dest);
                    let object = self.named(callee);
                    self.add(dest, object);
                }
                Op::Lambda(dest, region) => {
                    let dest = self.local(ctx, dest);
                    let object = self.object(Some(Function::Closure(ctx, region)));
                    self.add(dest, object);
                }
                Op::Object(dest, function, fields) => {
                    let dest = self.local(ctx, dest);
                    let object = self
                        .object(function.map(|name| Function::Named(Callee::Named(name), true)));
                    for &(field, src) in fields.get(&body.fields) {
                        let src = self.local(ctx, src);
                        let dest = self.field(object, field);
                        self.copy(src, dest);
                    }
                    self.add(dest, object);
                }
                Op::Field(dest, base, field) => {
                    let dest = self.local(ctx, dest);
                    let base = self.local(ctx, base);
                    self.watch(base, Use::Read(field, dest));
                }
                Op::StoreField(base, field, src) => {
                    let src = self.local(ctx, src);
                    let base = self.local(ctx, base);
                    self.watch(base, Use::Write(field, src, root, site));
                }
                Op::Return(values) => {
                    let region = if body.coroutine.is_some() && region == 1 {
                        0
                    } else {
                        region
                    };
                    for (lane, &src) in values.get(&body.locals).iter().enumerate() {
                        let src = self.local(ctx, src);
                        let dest = self.result((root, ctx, region), lane);
                        self.copy(src, dest);
                    }
                }
                Op::Call(target, args, dests) => {
                    let args = args
                        .get(&body.locals)
                        .iter()
                        .map(|&id| self.local(ctx, id))
                        .collect();
                    let dests = dests
                        .get(&body.locals)
                        .iter()
                        .map(|&id| self.local(ctx, id))
                        .collect();
                    let call = self.push_call(
                        root,
                        site,
                        args,
                        dests,
                        once && !body.regions[0].may_repeat,
                    );
                    match target {
                        Target::Direct(callee) => self.call(call, callee, None),
                        Target::Value(value) => {
                            let src = self.local(ctx, value);
                            self.watch(src, Use::Call(call));
                        }
                    }
                }
            }
        }
    }

    fn push_call(
        &mut self,
        root: Root,
        site: Site,
        args: Vec<NodeId>,
        dests: Vec<NodeId>,
        once: bool,
    ) -> CallId {
        self.calls.push(Call {
            root,
            site,
            args,
            dests,
            once,
            escaped: None,
            unknown_args: false,
        });
        self.calls.len() - 1
    }

    fn bind(&mut self, call: CallId, ctx: ContextId, region: usize, capture: Option<ObjectId>) {
        let body = self.contexts[ctx].body;
        let mut params = body.regions[region]
            .params
            .get(&body.locals)
            .iter()
            .copied();
        if let Some(object) = capture
            && let Some(param) = params.next()
        {
            let dest = self.local(ctx, param);
            self.add(dest, object);
        }
        for (index, param) in params.enumerate() {
            let call = &self.calls[call];
            if let Some(src) = call
                .args
                .get(index)
                .copied()
                .or(call.unknown_args.then_some(UNKNOWN_ARG))
            {
                let dest = self.local(ctx, param);
                self.copy(src, dest);
            }
        }
    }

    fn results(&mut self, call: CallId, ctx: ContextId, region: usize) {
        let root = self.calls[call].root;
        for lane in 0..self.calls[call].dests.len() {
            let dest = self.calls[call].dests[lane];
            let from = self.result((root, ctx, region), lane);
            self.copy(from, dest);
        }
    }

    fn call(&mut self, call: CallId, callee: Callee, capture: Option<ObjectId>) {
        match callee {
            Callee::Named(name) => {
                let Some(&body) = self.bodies.get(&name) else {
                    assert!(
                        !self.unexamined.contains(&name),
                        "initializer callee {name} is absent from the concrete dependency closure"
                    );
                    self.opaque(call);
                    return;
                };
                let Call { root, site, .. } = self.calls[call];
                let key = (root, site, name);
                let ctx = if let Some(&ctx) = self.activations.get(&key) {
                    ctx
                } else {
                    let ctx = self.context(body);
                    self.activations.insert(key, ctx);
                    ctx
                };
                self.bind(call, ctx, 0, capture);
                if body.coroutine.is_none() {
                    self.run(root, ctx, 0);
                    self.results(call, ctx, 0);
                } else if let Some(&dest) = self.calls[call].dests.first() {
                    let object = if let Some(object) =
                        self.contexts[ctx].coroutine.as_mut().unwrap().handle
                    {
                        object
                    } else {
                        let object = self.object(Some(Function::Coroutine(ctx)));
                        self.contexts[ctx].coroutine.as_mut().unwrap().handle = Some(object);
                        object
                    };
                    self.add(dest, object);
                }
            }
            Callee::InspectCoroutine => {}
            Callee::Resume | Callee::Kill | Callee::HotReload | Callee::CoroutineResult => {
                if let Some(&receiver) = self.calls[call].args.first() {
                    self.watch(receiver, Use::Coroutine(call, callee));
                }
            }
            _ => self.opaque(call),
        }
    }

    fn invoke(&mut self, call: CallId, object: ObjectId) {
        match self.objects[object].function {
            Some(Function::Named(callee, capture)) => {
                self.call(call, callee, capture.then_some(object))
            }
            Some(Function::Closure(ctx, region)) => {
                self.bind(call, ctx, region, None);
                self.run(self.calls[call].root, ctx, region);
                self.results(call, ctx, region);
            }
            Some(Function::Coroutine(ctx)) => self.coroutine(ctx, call, Callee::Resume),
            None => {}
        }
    }

    fn opaque(&mut self, call: CallId) {
        if self.calls[call].escaped.is_some() {
            return;
        }
        let escaped = if self.calls[call].args.iter().any(|&arg| arg != 0) {
            let escaped = self.node();
            self.watch(escaped, Use::Escape(call));
            for index in 0..self.calls[call].args.len() {
                self.watch(self.calls[call].args[index], Use::Collect(escaped));
            }
            escaped
        } else {
            0
        };
        self.calls[call].escaped = Some(escaped);
        // Contained callables cross the escape, not the result. Propagating
        // their aggregate as an opaque return conflates unrelated objects.
        for index in 0..self.calls[call].dests.len() {
            self.add(self.calls[call].dests[index], UNKNOWN);
        }
    }

    fn escape(&mut self, call: CallId, object: ObjectId) {
        match self.objects[object].function {
            Some(Function::Coroutine(ctx)) => self.coroutine(ctx, call, Callee::Opaque),
            Some(Function::Named(Callee::Named(name), _)) if !self.bodies.contains_key(&name) => {}
            Some(Function::Closure(..) | Function::Named(Callee::Named(_), _)) => {
                let outer = &self.calls[call];
                let (root, site, escaped) = (outer.root, outer.site, outer.escaped.unwrap());
                let returned = self.node();
                self.watch(returned, Use::Collect(escaped));
                let inner = self.push_call(root, site, Vec::new(), vec![returned], false);
                self.calls[inner].unknown_args = true;
                self.invoke(inner, object);
            }
            _ => {}
        }
    }

    fn coroutine(&mut self, ctx: ContextId, call: CallId, action: Callee) {
        let state = self.contexts[ctx].coroutine.as_mut().unwrap();
        if !state.actions.insert((call, action)) {
            return;
        }
        let root = self.calls[call].root;
        let coroutine = self.contexts[ctx].body.coroutine.as_ref().unwrap();
        if matches!(action, Callee::Resume | Callee::Opaque) {
            if let Some(arg) = coroutine.resume_arg
                && let Some(&src) = self.calls[call].args.get(1)
            {
                let dest = self.local(ctx, arg);
                self.copy(src, dest);
            }
            self.run(root, ctx, 0);
            self.results(call, ctx, 0);
            let state = self.contexts[ctx].coroutine.as_mut().unwrap();
            let calls = &mut state.resumes;
            calls.push(call);
            let kills = state.kills.clone();
            let may_resume_again =
                calls.len() > 1 || !self.calls[call].once || matches!(action, Callee::Opaque);
            let resume_roots: Vec<_> = if may_resume_again {
                calls.iter().map(|&id| self.calls[id].root).collect()
            } else {
                vec![root]
            };
            for root in resume_roots {
                if may_resume_again {
                    self.run(root, ctx, 1);
                }
                for &(region, armed_on_start) in &coroutine.defers {
                    if may_resume_again || armed_on_start {
                        self.run(root, ctx, region);
                        for &kill in &kills {
                            self.run(kill, ctx, region);
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
                    let src = self.result((prior, ctx, 0), 0);
                    self.copy(src, dest);
                }
            }
        }
        if matches!(action, Callee::Kill) {
            let state = self.contexts[ctx].coroutine.as_mut().unwrap();
            state.kills.push(root);
            let calls = &state.resumes;
            if !calls.is_empty() {
                let may_resume_again = calls.len() > 1 || !self.calls[calls[0]].once;
                for &(region, armed_on_start) in &coroutine.defers {
                    if may_resume_again || armed_on_start {
                        self.run(root, ctx, region);
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
                self.run(root, ctx, region);
            }
        }
        if matches!(action, Callee::Kill | Callee::Opaque) {
            let inner = self.local(ctx, self.contexts[ctx].body.slots as Local);
            self.watch(inner, Use::Coroutine(call, action));
        }
    }

    fn propagate(&mut self, initializer_contexts: usize) {
        loop {
            while let Some(key @ (_, ctx, region)) = self.pending.pop() {
                self.apply(key, ctx < initializer_contexts && region == 0);
            }
            let Some((use_, object)) = self.work.pop() else {
                break;
            };
            match use_ {
                Use::Copy(dest) => self.add(dest, object),
                Use::Read(_, dest) if object == UNKNOWN => self.add(dest, UNKNOWN),
                Use::Read(field, dest) => {
                    let src = self.field(
                        object,
                        if field == Field::Element {
                            Field::All
                        } else {
                            field
                        },
                    );
                    self.copy(src, dest);
                    if let (Some(Function::Coroutine(_)), Field::Name(name)) =
                        (self.objects[object].function, field)
                        && matches!(name.as_str(), "__resume" | "__advance")
                    {
                        self.add(dest, object);
                    }
                }
                Use::Write(_, src, root, site) if object == UNKNOWN => {
                    let call = self.push_call(root, site, vec![src], Vec::new(), false);
                    self.opaque(call);
                }
                Use::Write(field, src, ..) => {
                    let dest = self.field(object, field);
                    self.copy(src, dest);
                }
                Use::Call(call) => self.invoke(call, object),
                Use::Coroutine(call, action) => {
                    if let Some(Function::Coroutine(ctx)) = self.objects[object].function {
                        self.coroutine(ctx, call, action);
                    }
                }
                Use::Collect(dest) if object != UNKNOWN => {
                    let src = self.field(object, Field::Callbacks);
                    self.copy(src, dest);
                }
                Use::Collect(_) => {}
                Use::Escape(call) => self.escape(call, object),
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
    if inits.is_empty() {
        return Ok(Vec::new());
    }
    let mut analysis = Analysis {
        bodies: summaries
            .into_iter()
            .flat_map(|facts| &facts.0)
            .map(|body| (body.name, body))
            .collect(),
        unexamined: unexamined.into_iter().collect(),
        owners: inits
            .iter()
            .map(|init| (init.name, init.name))
            .chain(
                inits
                    .iter()
                    .flat_map(|init| init.leaves.iter().map(|&leaf| (leaf, init.name))),
            )
            .collect(),
        deps: vec![FxHashSet::default(); inits.len()],
        ..Analysis::default()
    };
    assert_eq!(
        analysis.object(Some(Function::Named(Callee::Opaque, false))),
        UNKNOWN
    );
    let _ = analysis.node();
    assert_eq!(analysis.node(), UNKNOWN_ARG);
    analysis.add(UNKNOWN_ARG, UNKNOWN);
    for (root, init) in inits.iter().enumerate() {
        let Some(&body) = analysis.bodies.get(&init.body) else {
            continue;
        };
        if body.regions[0].ops.is_empty() {
            continue;
        }
        let ctx = analysis.context(body);
        analysis.run(root, ctx, 0);
        let leaves = if init.leaves.is_empty() {
            std::slice::from_ref(&init.name)
        } else {
            &init.leaves
        };
        for (lane, &name) in leaves.iter().enumerate() {
            let src = analysis.result((root, ctx, 0), lane);
            let dest = analysis.global(name);
            analysis.copy(src, dest);
        }
    }
    analysis.propagate(analysis.contexts.len());
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
        &analysis.owners,
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
