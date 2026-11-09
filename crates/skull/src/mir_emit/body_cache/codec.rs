//! The byte encoding of a cached chunk and the facts stored beside it.
//!
//! Every field of [`Chunk`] and of the skull-mir types it carries is written
//! by hand, through exhaustive destructuring, so a new field is a compile
//! error here rather than a chunk silently served without it. Build-local
//! identities are replaced by stable ones: an interned type is written
//! structurally and re-interned on read, while a `FuncRef` is written raw,
//! which the environment hash makes stable (it covers every declaration of
//! every module in module order).

use super::*;
use skull_mir::bound::{ResolvedBoundNext, ResolvedBoundTree, SpecDemand};
use skull_mir::code_identity::{
    CaptureField, CaptureIdentity, CaptureMode, CaptureSchema, CodeArtifact, CodeCompatibility,
    CodeEntry, CodeIdentity, CodeIdentityCensus, CodeOwner, CodeOwnerKind, FrameSchema,
    LambdaSiteId, LambdaTopologyFingerprint,
};
use skull_mir::mir::{ComptimeInputId, ComptimeRead, StaticCell};
use skull_mir::monoty::{MonoCtor, MonoInterner, MonoTy};

pub(super) struct Writer {
    pub(super) out: Vec<u8>,
}

impl Writer {
    pub(super) fn new() -> Self {
        Self { out: Vec::new() }
    }

    pub(super) fn u8(&mut self, v: u8) {
        self.out.push(v);
    }

    pub(super) fn bool(&mut self, v: bool) {
        self.u8(u8::from(v));
    }

    pub(super) fn u32(&mut self, v: u32) {
        self.out.extend_from_slice(&v.to_le_bytes());
    }

    pub(super) fn u64(&mut self, v: u64) {
        self.out.extend_from_slice(&v.to_le_bytes());
    }

    pub(super) fn u128(&mut self, v: u128) {
        self.out.extend_from_slice(&v.to_le_bytes());
    }

    pub(super) fn usize(&mut self, v: usize) {
        self.u64(v as u64);
    }

    pub(super) fn bytes(&mut self, v: &[u8]) {
        self.u32(u32::try_from(v.len()).expect("a cached chunk is under 4 GiB"));
        self.out.extend_from_slice(v);
    }

    pub(super) fn str(&mut self, v: &str) {
        self.bytes(v.as_bytes());
    }

    pub(super) fn ustr(&mut self, v: Ustr) {
        self.str(v.as_str());
    }

    pub(super) fn len(&mut self, n: usize) {
        self.u32(u32::try_from(n).expect("a cached sequence is under 4 G entries"));
    }

    pub(super) fn func_ref(&mut self, f: FuncRef) {
        self.u32(f.module.raw());
        self.u32(f.func.into_raw().into_u32());
    }

    fn span(&mut self, span: &bone_core::prelude::Span) {
        self.ustr(span.file);
        self.usize(span.line_range.0);
        self.usize(span.line_range.1);
        self.usize(span.col_range.0);
        self.usize(span.col_range.1);
        self.usize(span.offset_range.0);
        self.usize(span.offset_range.1);
    }

    fn mono_ty(&mut self, ty: &MonoTy) {
        match ty {
            MonoTy::Int => self.u8(0),
            MonoTy::Uint => self.u8(1),
            MonoTy::Float => self.u8(2),
            MonoTy::Bool => self.u8(3),
            MonoTy::String => self.u8(4),
            MonoTy::Any => self.u8(5),
            MonoTy::Unit => self.u8(6),
            MonoTy::Never => self.u8(7),
            MonoTy::Nullable(inner) => {
                self.u8(8);
                self.mono_ty(inner);
            }
            MonoTy::Tuple(elements) => {
                self.u8(9);
                self.len(elements.len());
                for element in elements {
                    self.mono_ty(element);
                }
            }
            MonoTy::App { ctor, params } => {
                self.u8(10);
                match ctor {
                    MonoCtor::Array => self.u8(0),
                    MonoCtor::Map => self.u8(1),
                    MonoCtor::Ref => self.u8(2),
                    MonoCtor::Nominal(name) => {
                        self.u8(3);
                        self.ustr(*name);
                    }
                }
                self.len(params.len());
                for param in params {
                    self.mono_ty(param);
                }
            }
            MonoTy::Function { params, ret } => {
                self.u8(11);
                self.len(params.len());
                for param in params {
                    self.mono_ty(param);
                }
                self.mono_ty(ret);
            }
        }
    }

    pub(super) fn instance(&mut self, interner: &MonoInterner, instance: &Instance) {
        let Instance { func, args } = instance;
        self.func_ref(*func);
        let args = interner.get_all(args);
        self.len(args.len());
        for arg in &args {
            self.mono_ty(arg);
        }
    }

    fn site(&mut self, site: &LambdaSiteId) {
        let LambdaSiteId { file, offset } = site;
        self.ustr(*file);
        self.u64(*offset);
    }

    fn code_owner(&mut self, interner: &MonoInterner, owner: &CodeOwner) {
        match owner {
            CodeOwner::Instance(instance) => {
                self.u8(0);
                self.instance(interner, instance);
            }
            CodeOwner::Artifact { kind, module, name } => {
                self.u8(1);
                self.u8(match kind {
                    CodeOwnerKind::Global => 0,
                    CodeOwnerKind::Decorator => 1,
                });
                self.ustr(*module);
                self.ustr(*name);
            }
        }
    }

    fn code_identity(&mut self, interner: &MonoInterner, identity: &CodeIdentity) {
        match identity {
            CodeIdentity::Named(instance) => {
                self.u8(0);
                self.instance(interner, instance);
            }
            CodeIdentity::Artifact(owner) => {
                self.u8(1);
                self.code_owner(interner, owner);
            }
            CodeIdentity::Lambda { enclosing, site } => {
                self.u8(2);
                self.code_owner(interner, enclosing);
                self.site(site);
            }
            CodeIdentity::Nested {
                enclosing,
                site,
                name,
            } => {
                self.u8(3);
                self.code_owner(interner, enclosing);
                self.site(site);
                self.ustr(*name);
            }
            CodeIdentity::Generated { enclosing, role } => {
                self.u8(4);
                self.code_owner(interner, enclosing);
                self.ustr(*role);
            }
        }
    }

    fn code_entry(&mut self, interner: &MonoInterner, entry: &CodeEntry) {
        let CodeEntry {
            identity,
            compatibility,
            body_fingerprint,
            body_name,
            escaping,
            capture_free,
        } = entry;
        self.code_identity(interner, identity);
        let CodeCompatibility {
            call_abi,
            capture,
            frame,
            lambda_topology,
        } = compatibility;
        self.str(call_abi);
        let CaptureSchema { fields, complete } = capture;
        self.len(fields.len());
        for CaptureField { identity, mode } in fields {
            match identity {
                CaptureIdentity::Binding { site, projection } => {
                    self.u8(0);
                    self.site(site);
                    self.ustr(*projection);
                }
                CaptureIdentity::Generated { role } => {
                    self.u8(1);
                    self.ustr(*role);
                }
            }
            self.u8(match mode {
                CaptureMode::ByValue => 0,
                CaptureMode::Shared => 1,
                CaptureMode::Frame => 2,
            });
        }
        self.bool(*complete);
        match frame {
            None => self.u8(0),
            Some(FrameSchema { slots }) => {
                self.u8(1);
                self.len(slots.len());
                for slot in slots {
                    self.u32(*slot);
                }
            }
        }
        self.u64(lambda_topology.0);
        self.u64(*body_fingerprint);
        self.option_ustr(*body_name);
        self.bool(*escaping);
        self.bool(*capture_free);
    }

    fn option_ustr(&mut self, v: Option<Ustr>) {
        match v {
            None => self.u8(0),
            Some(v) => {
                self.u8(1);
                self.ustr(v);
            }
        }
    }

    fn code_artifact(&mut self, interner: &MonoInterner, artifact: &CodeArtifact) {
        let CodeArtifact { entries, census } = artifact;
        self.len(entries.len());
        for entry in entries.values() {
            self.code_entry(interner, entry);
        }
        let CodeIdentityCensus {
            examined,
            assigned,
            missing_identity,
            duplicate_identity,
            duplicate_body,
            unresolved_body,
            direct_call_only,
            named_values,
            lambda_values,
            capturing_lambdas,
            capture_free_lambdas,
        } = census;
        for value in [
            examined,
            assigned,
            missing_identity,
            duplicate_identity,
            duplicate_body,
            unresolved_body,
            direct_call_only,
            named_values,
            lambda_values,
            capturing_lambdas,
            capture_free_lambdas,
        ] {
            self.usize(*value);
        }
    }

    fn ty_id(&mut self, ty: Option<bone_core::prelude::TyId>) {
        use bone_core::prelude::TyId;
        match ty {
            None => self.u8(0),
            Some(TyId::Static(module, idx)) => {
                self.u8(1);
                self.u32(module.raw());
                self.u32(idx.into_raw().into_u32());
            }
            Some(TyId::Local(idx)) => {
                self.u8(2);
                self.u32(idx.into_raw().into_u32());
            }
        }
    }

    fn static_cell(&mut self, interner: &MonoInterner, cell: &StaticCell) {
        let StaticCell {
            cell,
            done_flag,
            ty,
            logical_name,
            ordinal,
            type_str,
            init_hash,
            owner,
            site,
        } = cell;
        self.ustr(*cell);
        self.ustr(*done_flag);
        self.ty_id(*ty);
        self.ustr(*logical_name);
        self.u32(*ordinal);
        self.str(type_str);
        self.u64(*init_hash);
        match owner {
            None => self.u8(0),
            Some(owner) => {
                self.u8(1);
                self.code_identity(interner, owner);
            }
        }
        match site {
            None => self.u8(0),
            Some(site) => {
                self.u8(1);
                self.site(site);
            }
        }
    }

    fn comptime_read(&mut self, read: &ComptimeRead) {
        let ComptimeRead {
            input,
            value_hash,
            span,
        } = read;
        match input {
            ComptimeInputId::Var(name) => {
                self.u8(0);
                self.ustr(*name);
            }
            ComptimeInputId::File(path) => {
                self.u8(1);
                self.ustr(*path);
            }
        }
        self.u64(*value_hash);
        self.span(span);
    }

    fn bound_tree(&mut self, interner: &MonoInterner, tree: &ResolvedBoundTree) {
        let ResolvedBoundTree { next } = tree;
        self.len(next.len());
        for (index, next) in next {
            self.usize(*index);
            match next {
                ResolvedBoundNext::Concrete { instance, sub } => {
                    self.u8(0);
                    self.instance(interner, instance);
                    self.bound_tree(interner, sub);
                }
                ResolvedBoundNext::CallerGeneric(index) => {
                    self.u8(1);
                    self.usize(*index);
                }
            }
        }
    }

    fn func_refs_sorted(&mut self, refs: &FxHashSet<FuncRef>) {
        let mut sorted: Vec<FuncRef> = refs.iter().copied().collect();
        sorted.sort_by_key(|f| skull_mir::instance::DeclKey(*f).raw());
        self.len(sorted.len());
        for f in sorted {
            self.func_ref(f);
        }
    }

    fn ustrs(&mut self, names: &[Ustr]) {
        self.len(names.len());
        for name in names {
            self.ustr(*name);
        }
    }

    /// Everything assembly reads from a chunk. Allocation bodies are a
    /// diagnostic the cache is disabled for, so a chunk that carries one is
    /// refused rather than stored incomplete.
    pub(super) fn chunk(&mut self, interner: &MonoInterner, chunk: &Chunk) -> Option<()> {
        let Chunk {
            lua,
            definition_count,
            literal_globals,
            allocation_bodies,
            deps,
            embedded_deps,
            abi_deps,
            direct_globals,
            direct_calls,
            times: _,
            attribution: _,
            statics,
            comptime_reads,
            calls,
            specs,
            code_artifact,
            binds,
        } = chunk;
        if !allocation_bodies.is_empty() {
            return None;
        }
        self.str(lua);
        self.usize(*definition_count);
        self.ustrs(literal_globals);
        self.func_refs_sorted(deps);
        self.func_refs_sorted(embedded_deps);
        self.func_refs_sorted(abi_deps);
        self.ustrs(direct_globals);
        self.ustrs(direct_calls);
        self.len(statics.len());
        for cell in statics {
            self.static_cell(interner, cell);
        }
        self.len(comptime_reads.len());
        for read in comptime_reads {
            self.comptime_read(read);
        }
        self.len(calls.len());
        for call in calls {
            self.func_ref(*call);
        }
        self.len(specs.len());
        for SpecDemand { inst, tree } in specs {
            self.instance(interner, inst);
            self.bound_tree(interner, tree);
        }
        self.code_artifact(interner, code_artifact);
        self.len(binds.len());
        for plan in binds {
            let skull_codegen_lua::CalleeBindingPlan {
                name,
                factory,
                captures,
                rank,
                capped,
            } = plan;
            self.str(name);
            self.str(factory);
            self.len(captures.len());
            for capture in captures {
                self.str(capture);
            }
            self.u32(*rank);
            self.bool(*capped);
        }
        Some(())
    }
}

/// A cursor over one record. Every read is checked; a short or malformed
/// record reads as `None` and the lookup is a miss.
pub(super) struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub(super) fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub(super) fn at_end(&self) -> bool {
        self.pos == self.buf.len()
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let slice = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(slice)
    }

    pub(super) fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    pub(super) fn bool(&mut self) -> Option<bool> {
        match self.u8()? {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }

    pub(super) fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    pub(super) fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    pub(super) fn u128(&mut self) -> Option<u128> {
        Some(u128::from_le_bytes(self.take(16)?.try_into().ok()?))
    }

    pub(super) fn usize(&mut self) -> Option<usize> {
        usize::try_from(self.u64()?).ok()
    }

    pub(super) fn bytes(&mut self) -> Option<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }

    pub(super) fn str(&mut self) -> Option<&'a str> {
        std::str::from_utf8(self.bytes()?).ok()
    }

    pub(super) fn ustr(&mut self) -> Option<Ustr> {
        Some(Ustr::from(self.str()?))
    }

    /// A sequence length, bounded by the bytes left so a corrupt count cannot
    /// reserve gigabytes.
    pub(super) fn len(&mut self) -> Option<usize> {
        let n = self.u32()? as usize;
        (n <= self.buf.len() - self.pos).then_some(n)
    }

    pub(super) fn func_ref(&mut self) -> Option<FuncRef> {
        let module = ModuleId::new(self.u32()?);
        let func = Idx::from_raw(comfy_la_arena::RawIdx::from_u32(self.u32()?));
        Some(FuncRef { module, func })
    }

    fn span(&mut self) -> Option<bone_core::prelude::Span> {
        Some(bone_core::prelude::Span {
            file: self.ustr()?,
            line_range: (self.usize()?, self.usize()?),
            col_range: (self.usize()?, self.usize()?),
            offset_range: (self.usize()?, self.usize()?),
        })
    }

    fn mono_ty(&mut self) -> Option<MonoTy> {
        Some(match self.u8()? {
            0 => MonoTy::Int,
            1 => MonoTy::Uint,
            2 => MonoTy::Float,
            3 => MonoTy::Bool,
            4 => MonoTy::String,
            5 => MonoTy::Any,
            6 => MonoTy::Unit,
            7 => MonoTy::Never,
            8 => MonoTy::Nullable(Box::new(self.mono_ty()?)),
            9 => {
                let n = self.len()?;
                let mut elements = Vec::with_capacity(n);
                for _ in 0..n {
                    elements.push(self.mono_ty()?);
                }
                MonoTy::Tuple(elements)
            }
            10 => {
                let ctor = match self.u8()? {
                    0 => MonoCtor::Array,
                    1 => MonoCtor::Map,
                    2 => MonoCtor::Ref,
                    3 => MonoCtor::Nominal(self.ustr()?),
                    _ => return None,
                };
                let n = self.len()?;
                let mut params = Vec::with_capacity(n);
                for _ in 0..n {
                    params.push(self.mono_ty()?);
                }
                MonoTy::App { ctor, params }
            }
            11 => {
                let n = self.len()?;
                let mut params = Vec::with_capacity(n);
                for _ in 0..n {
                    params.push(self.mono_ty()?);
                }
                let ret = Box::new(self.mono_ty()?);
                MonoTy::Function { params, ret }
            }
            _ => return None,
        })
    }

    pub(super) fn instance(&mut self, interner: &MonoInterner) -> Option<Instance> {
        let func = self.func_ref()?;
        let n = self.len()?;
        let mut args = Vec::with_capacity(n);
        for _ in 0..n {
            args.push(interner.intern(self.mono_ty()?));
        }
        Some(Instance { func, args })
    }

    fn site(&mut self) -> Option<LambdaSiteId> {
        Some(LambdaSiteId {
            file: self.ustr()?,
            offset: self.u64()?,
        })
    }

    fn code_owner(&mut self, interner: &MonoInterner) -> Option<CodeOwner> {
        Some(match self.u8()? {
            0 => CodeOwner::Instance(self.instance(interner)?),
            1 => {
                let kind = match self.u8()? {
                    0 => CodeOwnerKind::Global,
                    1 => CodeOwnerKind::Decorator,
                    _ => return None,
                };
                CodeOwner::Artifact {
                    kind,
                    module: self.ustr()?,
                    name: self.ustr()?,
                }
            }
            _ => return None,
        })
    }

    fn code_identity(&mut self, interner: &MonoInterner) -> Option<CodeIdentity> {
        Some(match self.u8()? {
            0 => CodeIdentity::Named(self.instance(interner)?),
            1 => CodeIdentity::Artifact(self.code_owner(interner)?),
            2 => CodeIdentity::Lambda {
                enclosing: self.code_owner(interner)?,
                site: self.site()?,
            },
            3 => CodeIdentity::Nested {
                enclosing: self.code_owner(interner)?,
                site: self.site()?,
                name: self.ustr()?,
            },
            4 => CodeIdentity::Generated {
                enclosing: self.code_owner(interner)?,
                role: self.ustr()?,
            },
            _ => return None,
        })
    }

    fn option_ustr(&mut self) -> Option<Option<Ustr>> {
        Some(match self.u8()? {
            0 => None,
            1 => Some(self.ustr()?),
            _ => return None,
        })
    }

    fn code_entry(&mut self, interner: &MonoInterner) -> Option<CodeEntry> {
        let identity = self.code_identity(interner)?;
        let call_abi = self.str()?.to_string();
        let n = self.len()?;
        let mut fields = Vec::with_capacity(n);
        for _ in 0..n {
            let identity = match self.u8()? {
                0 => CaptureIdentity::Binding {
                    site: self.site()?,
                    projection: self.ustr()?,
                },
                1 => CaptureIdentity::Generated { role: self.ustr()? },
                _ => return None,
            };
            let mode = match self.u8()? {
                0 => CaptureMode::ByValue,
                1 => CaptureMode::Shared,
                2 => CaptureMode::Frame,
                _ => return None,
            };
            fields.push(CaptureField { identity, mode });
        }
        let complete = self.bool()?;
        let frame = match self.u8()? {
            0 => None,
            1 => {
                let n = self.len()?;
                let mut slots = Vec::with_capacity(n);
                for _ in 0..n {
                    slots.push(self.u32()?);
                }
                Some(FrameSchema { slots })
            }
            _ => return None,
        };
        let lambda_topology = LambdaTopologyFingerprint(self.u64()?);
        Some(CodeEntry {
            identity,
            compatibility: CodeCompatibility {
                call_abi,
                capture: CaptureSchema { fields, complete },
                frame,
                lambda_topology,
            },
            body_fingerprint: self.u64()?,
            body_name: self.option_ustr()?,
            escaping: self.bool()?,
            capture_free: self.bool()?,
        })
    }

    fn code_artifact(&mut self, interner: &MonoInterner) -> Option<CodeArtifact> {
        let n = self.len()?;
        let mut entries = BTreeMap::new();
        for _ in 0..n {
            let entry = self.code_entry(interner)?;
            entries.insert(entry.identity.clone(), entry);
        }
        let census = CodeIdentityCensus {
            examined: self.usize()?,
            assigned: self.usize()?,
            missing_identity: self.usize()?,
            duplicate_identity: self.usize()?,
            duplicate_body: self.usize()?,
            unresolved_body: self.usize()?,
            direct_call_only: self.usize()?,
            named_values: self.usize()?,
            lambda_values: self.usize()?,
            capturing_lambdas: self.usize()?,
            capture_free_lambdas: self.usize()?,
        };
        Some(CodeArtifact { entries, census })
    }

    fn ty_id(&mut self) -> Option<Option<bone_core::prelude::TyId>> {
        use bone_core::prelude::TyId;
        Some(match self.u8()? {
            0 => None,
            1 => {
                let module = ModuleId::new(self.u32()?);
                let idx = Idx::from_raw(comfy_la_arena::RawIdx::from_u32(self.u32()?));
                Some(TyId::Static(module, idx))
            }
            2 => Some(TyId::Local(Idx::from_raw(
                comfy_la_arena::RawIdx::from_u32(self.u32()?),
            ))),
            _ => return None,
        })
    }

    fn static_cell(&mut self, interner: &MonoInterner) -> Option<StaticCell> {
        Some(StaticCell {
            cell: self.ustr()?,
            done_flag: self.ustr()?,
            ty: self.ty_id()?,
            logical_name: self.ustr()?,
            ordinal: self.u32()?,
            type_str: self.str()?.to_string(),
            init_hash: self.u64()?,
            owner: match self.u8()? {
                0 => None,
                1 => Some(self.code_identity(interner)?),
                _ => return None,
            },
            site: match self.u8()? {
                0 => None,
                1 => Some(self.site()?),
                _ => return None,
            },
        })
    }

    fn comptime_read(&mut self) -> Option<ComptimeRead> {
        let input = match self.u8()? {
            0 => ComptimeInputId::Var(self.ustr()?),
            1 => ComptimeInputId::File(self.ustr()?),
            _ => return None,
        };
        Some(ComptimeRead {
            input,
            value_hash: self.u64()?,
            span: self.span()?,
        })
    }

    fn bound_tree(&mut self, interner: &MonoInterner) -> Option<ResolvedBoundTree> {
        let n = self.len()?;
        let mut next = BTreeMap::new();
        for _ in 0..n {
            let index = self.usize()?;
            let value = match self.u8()? {
                0 => ResolvedBoundNext::Concrete {
                    instance: self.instance(interner)?,
                    sub: self.bound_tree(interner)?,
                },
                1 => ResolvedBoundNext::CallerGeneric(self.usize()?),
                _ => return None,
            };
            next.insert(index, value);
        }
        Some(ResolvedBoundTree { next })
    }

    fn func_ref_set(&mut self) -> Option<FxHashSet<FuncRef>> {
        let n = self.len()?;
        let mut set = FxHashSet::default();
        set.reserve(n);
        for _ in 0..n {
            set.insert(self.func_ref()?);
        }
        Some(set)
    }

    fn ustrs(&mut self) -> Option<Vec<Ustr>> {
        let n = self.len()?;
        let mut names = Vec::with_capacity(n);
        for _ in 0..n {
            names.push(self.ustr()?);
        }
        Some(names)
    }

    pub(super) fn chunk(&mut self, interner: &MonoInterner) -> Option<Chunk> {
        let lua = self.str()?.to_string();
        let definition_count = self.usize()?;
        let literal_globals = self.ustrs()?;
        let deps = self.func_ref_set()?;
        let embedded_deps = self.func_ref_set()?;
        let abi_deps = self.func_ref_set()?;
        let direct_globals = self.ustrs()?;
        let direct_calls = self.ustrs()?;
        let n = self.len()?;
        let mut statics = Vec::with_capacity(n);
        for _ in 0..n {
            statics.push(self.static_cell(interner)?);
        }
        let n = self.len()?;
        let mut comptime_reads = Vec::with_capacity(n);
        for _ in 0..n {
            comptime_reads.push(self.comptime_read()?);
        }
        let n = self.len()?;
        let mut calls = Vec::with_capacity(n);
        for _ in 0..n {
            calls.push(self.func_ref()?);
        }
        let n = self.len()?;
        let mut specs = Vec::with_capacity(n);
        for _ in 0..n {
            specs.push(SpecDemand {
                inst: self.instance(interner)?,
                tree: self.bound_tree(interner)?,
            });
        }
        let code_artifact = self.code_artifact(interner)?;
        let n = self.len()?;
        let mut binds = Vec::with_capacity(n);
        for _ in 0..n {
            let name = self.str()?.to_string();
            let factory = self.str()?.to_string();
            let count = self.len()?;
            let mut captures = Vec::with_capacity(count);
            for _ in 0..count {
                captures.push(self.str()?.to_string());
            }
            binds.push(skull_codegen_lua::CalleeBindingPlan {
                name,
                factory,
                captures,
                rank: self.u32()?,
                capped: self.bool()?,
            });
        }
        Some(Chunk {
            lua,
            definition_count,
            literal_globals,
            allocation_bodies: Vec::new(),
            deps,
            embedded_deps,
            abi_deps,
            direct_globals,
            direct_calls,
            times: PhaseTimes::default(),
            attribution: UnitAttribution::default(),
            statics,
            comptime_reads,
            calls,
            specs,
            code_artifact,
            binds,
        })
    }
}
