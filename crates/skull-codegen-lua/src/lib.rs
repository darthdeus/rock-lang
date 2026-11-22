//! Emit LuaJIT functions from typed MIR.

mod binding;
mod block;
mod control;
mod effect;
mod for_loops;
mod forward;
mod frame_check;
mod frame_limits;
mod function;
mod hoist;
mod inlinable;
mod inline_lua;
mod inst;
mod late_float;
mod line_directives;
mod names;
mod preludes;
mod regalloc;
mod scc;
mod scope;
mod scope_guard;
mod structured;
mod term;
mod value;

pub use binding::*;
use control::{ControlLines, Label};
use for_loops::*;
use forward::*;
use frame_check::*;
pub use function::*;
pub use hoist::*;
use inlinable::*;
use inline_lua::*;
pub use line_directives::*;
pub use names::*;
pub use preludes::*;
pub use scc::*;
use scope::*;

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::cell::RefCell;
use std::fmt::Write as _;
use std::sync::Mutex;

use bone_core::hir::{FnBackend, Function};
use bone_core::prelude::{CompilerError, Span};
use bone_core::workspace::Workspace;
use comfy_la_arena::Idx;
use skull_mir::cfg::{PredMap, predecessors, reverse_postorder, successors};
use skull_mir::mir::*;
use skull_mir::monoty::MonoInterner;

pub use skull_mir::mangle::mangle_fn;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum EmittedAllocationKind {
    StringConcat,
    StringConversion,
    StringFormat,
    TupleConstruction,
    VecConstruction,
    VecColumnConstruction,
    VecColumnHeader,
    StructConstruction,
    ValueConstruction,
    ExplicitValueMaterialization,
    EnumConstruction,
    DynamicEnumConstruction,
    ClosureEnvironment,
    CoroutineFrame,
    FfiArray,
    FfiReturnValue,
    FfiView,
    ContainerGrowth,
}

impl EmittedAllocationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::StringConcat => "string-concat",
            Self::StringConversion => "string-conversion",
            Self::StringFormat => "string-format",
            Self::TupleConstruction => "tuple-construction",
            Self::VecConstruction => "vec-construction",
            Self::VecColumnConstruction => "vec-column-construction",
            Self::VecColumnHeader => "vec-column-header",
            Self::StructConstruction => "struct-construction",
            Self::ValueConstruction => "value-construction",
            Self::ExplicitValueMaterialization => "explicit-value-materialization",
            Self::EnumConstruction => "enum-construction",
            Self::DynamicEnumConstruction => "dynamic-enum-construction",
            Self::ClosureEnvironment => "closure-environment",
            Self::CoroutineFrame => "coroutine-frame",
            Self::FfiArray => "ffi-array",
            Self::FfiReturnValue => "ffi-return-value",
            Self::FfiView => "ffi-view",
            Self::ContainerGrowth => "container-growth",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmittedAllocationSite {
    pub id: u64,
    pub kind: EmittedAllocationKind,
    pub span: Option<Span>,
    pub lua_body_line: u32,
    pub mir_inst: u32,
    lua_byte_offset: usize,
}

pub type PcSource = (u32, &'static str, Span);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LuaEmission {
    pub lua: String,
    pub allocation_sites: Vec<EmittedAllocationSite>,
    pub call_sites: Vec<Idx<Inst>>,
    pub pc_sources: Vec<PcSource>,
}

fn checked_u32_constant(func: &MirFunc, x: Idx<Inst>) -> Option<f64> {
    let value = match func.insts[x] {
        Inst::Const(Lit::Int(value)) => value as f64,
        Inst::Const(Lit::Uint(value)) => value as f64,
        Inst::Const(Lit::Float(value)) => value,
        _ => return None,
    };
    (0.0..=4294967295.0).contains(&value).then(|| value.floor())
}

fn valid_constant_shift_count(func: &MirFunc, x: Idx<Inst>) -> bool {
    matches!(
        func.insts[x],
        Inst::Const(Lit::Uint(0..=31) | Lit::Int(0..=31))
    )
}

#[cfg(test)]
mod constant_tests {
    use super::*;

    #[test]
    fn nonfinite_float_literals_preserve_their_values() {
        for (value, check) in [
            (f64::INFINITY, "value == 1 / 0"),
            (f64::NEG_INFINITY, "value == -1 / 0"),
            (f64::NAN, "value ~= value"),
        ] {
            let mut literal = String::new();
            write_lit(&mut literal, &Lit::Float(value));
            let output = std::process::Command::new("luajit")
                .args(["-e", &format!("local value = {literal}; assert({check})")])
                .output()
                .expect("spawn luajit");
            assert!(
                output.status.success(),
                "{literal}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[test]
    fn uint_constant_folding_keeps_invalid_values_checked() {
        let mut func = MirFunc::new(ustr::Ustr::from("checked"));
        for value in [
            Lit::Int(-1),
            Lit::Int(4294967296),
            Lit::Float(-0.1),
            Lit::Float(4294967295.5),
            Lit::Float(f64::NAN),
            Lit::Float(f64::INFINITY),
            Lit::Float(f64::NEG_INFINITY),
        ] {
            let x = func.alloc_inst(Inst::Const(value));
            assert_eq!(checked_u32_constant(&func, x), None);
        }
        let x = func.alloc_inst(Inst::Const(Lit::Float(-0.0)));
        assert_eq!(
            checked_u32_constant(&func, x).unwrap().to_bits(),
            (-0.0f64).to_bits()
        );
    }
}

const ENV_PARAM: &str = "__rock_env";

thread_local! {
    static REGALLOC_TIME: std::cell::Cell<std::time::Duration> =
        const { std::cell::Cell::new(std::time::Duration::ZERO) };
}

/// Zero the per-thread regalloc accumulator (call before an emit run).
pub fn regalloc_time_reset() {
    REGALLOC_TIME.with(|c| c.set(std::time::Duration::ZERO));
}

/// Read the per-thread regalloc accumulator (call after an emit run).
pub fn regalloc_time_elapsed() -> std::time::Duration {
    REGALLOC_TIME.with(|c| c.get())
}

#[inline]
fn raw<T>(idx: Idx<T>) -> u32 {
    idx.into_raw().into()
}

fn emitted_value_allocation_kind(inst: &Inst) -> Option<EmittedAllocationKind> {
    Some(match inst {
        Inst::StringConcat(..) => EmittedAllocationKind::StringConcat,
        Inst::ToString(..) => EmittedAllocationKind::StringConversion,
        Inst::Format { .. } | Inst::FormatDyn { .. } => EmittedAllocationKind::StringFormat,
        Inst::MakeTuple(..) => EmittedAllocationKind::TupleConstruction,
        Inst::MakeVec(..) => EmittedAllocationKind::VecConstruction,
        Inst::MakeFlatVec(vec) => match vec.contents {
            FlatVecContents::Columns { .. } => EmittedAllocationKind::VecColumnConstruction,
            FlatVecContents::Header(None) => EmittedAllocationKind::VecColumnHeader,
            FlatVecContents::Rows { .. } => EmittedAllocationKind::VecConstruction,
            FlatVecContents::Header(Some(_)) => EmittedAllocationKind::StructConstruction,
        },
        Inst::MakeStruct { .. } => EmittedAllocationKind::StructConstruction,
        Inst::MakeValueStruct { .. } => EmittedAllocationKind::ValueConstruction,
        Inst::MaterializeValueStruct { .. } => EmittedAllocationKind::ExplicitValueMaterialization,
        Inst::MakeEnum { .. } => EmittedAllocationKind::EnumConstruction,
        Inst::MakeEnumDyn { .. } => EmittedAllocationKind::DynamicEnumConstruction,
        Inst::MakeEnv { .. } => EmittedAllocationKind::ClosureEnvironment,
        Inst::FfiNewArray { .. } => EmittedAllocationKind::FfiArray,
        Inst::FfiViewCast { .. }
        | Inst::Index {
            kind: ContainerKind::NativeBufferValue(_),
            ..
        } => EmittedAllocationKind::FfiView,
        _ => return None,
    })
}

fn allocation_site_id(body: &str, id: Idx<Inst>, kind: EmittedAllocationKind) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    let mut write = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    };
    write(body.as_bytes());
    write(kind.as_str().as_bytes());
    write(&raw(id).to_le_bytes());
    hash
}

fn push_uint(out: &mut String, mut n: u32) {
    if n == 0 {
        out.push('0');
        return;
    }
    let mut buf = [0u8; 10];
    let mut i = buf.len();
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    out.push_str(std::str::from_utf8(&buf[i..]).unwrap());
}

/// Workspace-wide names, storage and diagnostics shared by function emitters.
pub struct CodegenCtx {
    reserved: HashMap<String, Span>,
    /// Emit boundaries drain these in key order, keeping diagnostics deterministic.
    rejections: Mutex<Vec<(String, CompilerError)>>,
    mangled: std::sync::RwLock<HashMap<FuncRef, ustr::Ustr>>,
    /// Separates this context's per-thread specialized-symbol cache from older builds.
    symbols_epoch: u64,
    state_slots: HashMap<ustr::Ustr, u32>,
    scalar_constants: HashMap<ustr::Ustr, Lit>,
    unread_constants: HashSet<ustr::Ustr>,
    /// An empty graph conservatively disables callee binding.
    call_sccs: HashMap<FuncRef, u32>,
    /// Disabled only by boundary tests that compare our rejection with LuaJIT loadfile.
    frame_limits: bool,
    line_directives: bool,
    allocation_sources: bool,
}

impl Clone for CodegenCtx {
    fn clone(&self) -> Self {
        Self {
            reserved: self.reserved.clone(),
            rejections: Mutex::new(self.rejections.lock().expect("rejection sink").clone()),
            mangled: std::sync::RwLock::new(
                self.mangled.read().expect("mangled name cache").clone(),
            ),
            symbols_epoch: next_symbols_epoch(),
            state_slots: self.state_slots.clone(),
            scalar_constants: self.scalar_constants.clone(),
            unread_constants: self.unread_constants.clone(),
            call_sccs: self.call_sccs.clone(),
            frame_limits: self.frame_limits,
            line_directives: self.line_directives,
            allocation_sources: self.allocation_sources,
        }
    }
}

fn is_reserved_lexical(name: &str) -> bool {
    if name == ENV_PARAM {
        return true;
    }
    let digits = match name.as_bytes().first() {
        Some(b'r' | b'c' | b'v') => &name[1..],
        _ => return false,
    };
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

fn reserved_namespace_collision(name: &str, span: Span, sink: &mut Vec<(String, CompilerError)>) {
    if !is_reserved_lexical(name) {
        return;
    }
    let msg = format!(
        "`{name}` collides with a reserved codegen identifier: skull emits `r<N>`/`v<N>` \
         value temps, `c<N>` cell locals, and the `{ENV_PARAM}` closure-env param as bare \
         locals inside function bodies, which would shadow this top-level definition at \
         runtime -- rename it"
    );
    sink.push((name.to_string(), CompilerError::new_with_label(span, msg)));
}

fn scalar_constants(ws: &Workspace) -> HashMap<ustr::Ustr, Lit> {
    use bone_core::hir::{Body, BodyOwner, Callee, Expr};
    use bone_core::workspace::BodyInference;
    use skull_mir::monoty::{MonoSubst, MonoTy, convert};

    struct Folder<'a> {
        ws: &'a Workspace,
        body: &'a Body,
        inf: Option<&'a BodyInference>,
        known: &'a HashMap<ustr::Ustr, Lit>,
        subst: &'a MonoSubst,
    }
    impl Folder<'_> {
        fn numeric_type(&self, id: Idx<Expr>) -> Option<MonoTy> {
            let inf = self.inf?;
            let ty = convert(self.ws, inf, *inf.expr_types.get(id)?, self.subst).ok()?;
            matches!(ty, MonoTy::Int | MonoTy::Float).then_some(ty)
        }

        fn fold(&self, id: Idx<Expr>, depth: usize) -> Option<Lit> {
            // Initializers nest without bound; a deep one stays a runtime
            // expression rather than growing the compiler stack.
            if depth >= 64 {
                return None;
            }
            match self.body.expr_arena[id] {
                Expr::Int(v) => Some(Lit::Int(v)),
                Expr::Float(v) => Some(Lit::Float(v.into_inner())),
                Expr::Bool(v) => Some(Lit::Bool(v)),
                Expr::String(v) => Some(Lit::Str(v)),
                Expr::Global(name) => self.known.get(&name).cloned(),
                Expr::UnaryMinus(inner, _) => match self.fold(inner, depth + 1)? {
                    Lit::Int(v) => v.checked_neg().map(Lit::Int),
                    Lit::Float(v) => Some(Lit::Float(-v)),
                    _ => None,
                },
                Expr::Call {
                    callee: Callee::Unresolved { name, .. },
                    args,
                    ..
                } if matches!(
                    name.as_str(),
                    "__op_add" | "__op_sub" | "__op_mul" | "__op_div"
                ) =>
                {
                    let mut args = args.into_iter();
                    let left = args.next()?;
                    let right = args.next()?;
                    if args.next().is_some() {
                        return None;
                    }
                    // Only primitive numeric operands fold: uint, newtype operators
                    // and string coercions keep their runtime checks and overloads.
                    self.numeric_type(left)?;
                    self.numeric_type(right)?;
                    let result_ty = self.numeric_type(id)?;
                    let number = |lit| match lit {
                        Lit::Int(v) if v.unsigned_abs() <= 9_007_199_254_740_991 => Some(v as f64),
                        Lit::Float(v) if v.is_finite() => Some(v),
                        _ => None,
                    };
                    let left = number(self.fold(left, depth + 1)?)?;
                    let right = number(self.fold(right, depth + 1)?)?;
                    let result = match name.as_str() {
                        "__op_add" => left + right,
                        "__op_sub" => left - right,
                        "__op_mul" => left * right,
                        "__op_div" if right != 0.0 => left / right,
                        _ => return None,
                    };
                    if !result.is_finite() {
                        return None;
                    }
                    match result_ty {
                        MonoTy::Float => Some(Lit::Float(result)),
                        MonoTy::Int
                            if result.abs() <= 9_007_199_254_740_991.0
                                && !(result == 0.0 && result.is_sign_negative()) =>
                        {
                            Some(Lit::Int(result as i64))
                        }
                        _ => None,
                    }
                }
                _ => None,
            }
        }
    }
    let mut pending: Vec<_> = ws
        .modules
        .iter()
        .flat_map(|module| {
            module.globals.iter().filter_map(|(id, global)| {
                if !global.is_const {
                    return None;
                }
                let owner = BodyOwner::Global(id);
                let inf = ws
                    .inference(module.id)
                    .and_then(|inf| inf.per_fn.get(&owner));
                module
                    .bodies
                    .get(&owner)
                    .map(|body| (global.name, body, inf))
            })
        })
        .collect();
    let mut known = HashMap::default();
    let subst = MonoSubst::default();
    loop {
        let before = pending.len();
        pending.retain(|(name, body, inf)| {
            let folder = Folder {
                ws,
                body,
                inf: *inf,
                known: &known,
                subst: &subst,
            };
            if let Some(lit) = body.root.and_then(|root| folder.fold(root, 0)) {
                known.insert(*name, lit);
                false
            } else {
                true
            }
        });
        if pending.len() == before {
            return known;
        }
    }
}

/// Compiled reads of a scalar constant use its literal, so only an
/// `inline(lua)` body spelling the emitted name can still read its global.
fn unread_constants(ws: &Workspace, constants: &HashMap<ustr::Ustr, Lit>) -> HashSet<ustr::Ustr> {
    let mut by_symbol: HashMap<&'static str, ustr::Ustr> = constants
        .keys()
        .map(|name| (skull_mir::mangle::symbol_of_key(*name).as_str(), *name))
        .collect();
    for module in &ws.modules {
        for (_, f) in module.functions.iter() {
            let Some((FnBackend::Lua, body)) = f.inline_body else {
                continue;
            };
            for word in body
                .as_str()
                .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            {
                by_symbol.remove(word);
            }
        }
    }
    by_symbol.into_values().collect()
}

impl CodegenCtx {
    /// Refresh baked scalar values after a constant initializer edit. Readers of
    /// every returned name need new emitted text, including readers of aliases.
    pub fn refresh_scalar_constants(&mut self, ws: &Workspace) -> HashSet<ustr::Ustr> {
        let next = scalar_constants(ws);
        let changed = self
            .scalar_constants
            .keys()
            .chain(next.keys())
            .filter(
                |name| match (self.scalar_constants.get(name), next.get(name)) {
                    (Some(Lit::Float(x)), Some(Lit::Float(y))) => x.to_bits() != y.to_bits(),
                    (a, b) => a != b,
                },
            )
            .copied()
            .collect();
        self.scalar_constants = next;
        changed
    }

    /// Whether the global of constant `name` is never read, so it needs neither
    /// an initializer nor storage.
    pub fn is_unread_constant(&self, name: ustr::Ustr) -> bool {
        self.unread_constants.contains(&name)
    }

    /// Recompute [`is_unread_constant`] after an edit, returning the constants
    /// that something reads now but did not before: their global must be
    /// initialized again.
    pub fn refresh_unread_constants(&mut self, ws: &Workspace) -> Vec<ustr::Ustr> {
        let next = unread_constants(ws, &self.scalar_constants);
        let now_read = self
            .unread_constants
            .iter()
            .filter(|name| !next.contains(*name))
            .copied()
            .collect();
        self.unread_constants = next;
        now_read
    }

    pub fn new(ws: &Workspace) -> Self {
        Self::new_with_state(ws, HashMap::default())
    }

    pub fn new_with_state(ws: &Workspace, state_slots: HashMap<ustr::Ustr, u32>) -> Self {
        let mut reserved: HashMap<String, Span> = HashMap::default();
        let mut collisions: Vec<(String, CompilerError)> = Vec::new();
        let names: Vec<(&Function, String)> = {
            use bone_core::prelude::*;
            let functions: Vec<&Function> = ws
                .modules
                .iter()
                .flat_map(|m| m.functions.iter().map(|(_, f)| f))
                .collect();
            functions
                .into_par_iter()
                .map(|f| (f, mangle_fn(ws, f)))
                .collect()
        };
        for (f, name) in &names {
            reserved_namespace_collision(name, f.name_span, &mut collisions);
            reserved.insert(name.clone(), f.name_span);
        }
        for m in ws.modules.iter() {
            for (_, g) in m.globals.iter() {
                let symbol = skull_mir::mangle::symbol_of_key(g.name);
                reserved_namespace_collision(symbol.as_str(), g.name_span, &mut collisions);
                reserved.insert(symbol.as_str().to_string(), g.name_span);
            }
        }
        collisions.extend(skull_mir::mangle::collision_errors_of(
            ws,
            names.iter().map(|(f, name)| (*f, name.as_str())),
        ));
        let scalar_constants = scalar_constants(ws);
        let unread_constants = unread_constants(ws, &scalar_constants);
        let ctx = CodegenCtx {
            reserved,
            rejections: Mutex::new(collisions),
            mangled: std::sync::RwLock::default(),
            symbols_epoch: next_symbols_epoch(),
            state_slots,
            scalar_constants,
            unread_constants,
            call_sccs: HashMap::default(),
            frame_limits: true,
            line_directives: line_directives_mode(),
            allocation_sources: false,
        };
        if skull_mir::verify::checks_enabled() {
            ctx.mint_synthetic(skull_mir::mir::VALUE_INTEGER_CHECK.to_string());
        }
        ctx
    }

    pub fn state_table(&self) -> &'static str {
        STATE_CELL
    }

    /// Boundary tests need the Lua of rejected bodies to check LuaJIT loadfile.
    pub fn set_frame_limits(&mut self, on: bool) {
        self.frame_limits = on;
    }

    pub fn frame_limits(&self) -> bool {
        self.frame_limits
    }

    /// Override `ROCK_SKULL_LINE_DIRECTIVES` for this context.
    pub fn set_line_directives(&mut self, on: bool) {
        self.line_directives = on;
    }

    pub fn set_allocation_sources(&mut self, on: bool) {
        self.allocation_sources = on;
    }

    pub fn line_directives(&self) -> bool {
        self.line_directives
    }

    /// The driver projects instance-graph SCCs onto declarations. Merging components
    /// only disables bindings, so codegen may conservatively use that projection.
    pub fn set_call_sccs(&mut self, sccs: HashMap<FuncRef, u32>) {
        self.call_sccs = sccs;
    }

    pub fn scc_of(&self, func: FuncRef) -> Option<u32> {
        self.call_sccs.get(&func).copied()
    }

    /// [`mangle_fn`] of the declaration `fref`, cached for this build.
    pub fn mangled_name(&self, ws: &Workspace, fref: FuncRef) -> ustr::Ustr {
        if let Some(name) = self.mangled.read().expect("mangled name cache").get(&fref) {
            return *name;
        }
        let name = ustr::Ustr::from(&mangle_fn(ws, func_of(ws, fref)));
        self.mangled
            .write()
            .expect("mangled name cache")
            .insert(fref, name);
        name
    }

    /// [`skull_mir::instance::Instance::symbol`], cached per worker thread.
    pub fn instance_symbol(
        &self,
        ws: &Workspace,
        interner: &MonoInterner,
        inst: &skull_mir::instance::Instance,
    ) -> ustr::Ustr {
        if inst.args.is_empty() {
            return self.mangled_name(ws, inst.func);
        }
        INSTANCE_SYMBOLS.with(|symbols| {
            let mut symbols = symbols.borrow_mut();
            if symbols.0 != self.symbols_epoch {
                symbols.0 = self.symbols_epoch;
                symbols.1.clear();
            }
            if let Some(name) = symbols.1.get(inst) {
                return *name;
            }
            let name = ustr::Ustr::from(&inst.symbol(ws, interner));
            symbols.1.insert(inst.clone(), name);
            name
        })
    }

    /// Record a spanned rejection if this synthetic name collides with a user definition.
    pub fn mint_synthetic(&self, name: String) -> String {
        if let Some(&span) = self.reserved.get(&name) {
            let msg = format!(
                "`{name}` collides with a compiler-reserved synthetic name emitted by \
                 codegen; rename this definition (the `__lambda<k>`, `__step`, `__resume`, \
                 `__advance`, `__defer<k>`, and `__on_killed`/`__on_resume`/`__on_hot_reload` \
                 forms are reserved)"
            );
            self.reject(name.clone(), CompilerError::new_with_label(span, msg));
        }
        name
    }

    /// Keyed errors keep parallel emission diagnostics deterministic.
    pub fn reject(&self, key: String, err: CompilerError) {
        self.rejections
            .lock()
            .expect("rejection sink")
            .push((key, err));
    }

    /// Drain all errors and return the first by key.
    pub fn take_rejection(&self) -> Option<CompilerError> {
        let mut sink = self.rejections.lock().expect("rejection sink");
        sink.sort_by(|a, b| a.0.cmp(&b.0));
        sink.drain(..).next().map(|(_, e)| e)
    }

    /// Accept either the global's DefKey or its emitted symbol.
    pub fn state_slot(&self, name: ustr::Ustr) -> Option<u32> {
        self.state_slots
            .get(&skull_mir::mangle::symbol_of_key(name))
            .copied()
    }

    /// The highest compiler-assigned storage slot, 0 when there is none. The
    /// emitter sizes the storage table's array part to this slot's rehash fixpoint.
    pub fn state_slot_max(&self) -> u32 {
        self.state_slots.values().copied().max().unwrap_or(0)
    }
}

/// `ROCK_SKULL_LINE_DIRECTIVES=1` writes LuaJIT line directives into every
/// emitted function (see [`write_line_directives`]). Off otherwise: a LuaJIT
/// without directive support rejects them as a syntax error.
pub fn line_directives_mode() -> bool {
    static MODE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| {
        matches!(
            std::env::var("ROCK_SKULL_LINE_DIRECTIVES").as_deref(),
            Ok("1")
        )
    })
}

/// The global holding the `Lit::Omitted` marker: a private table, so no Rock
/// value compares equal to it. The runtime prelude creates it once per Lua
/// state, so reloaded code compares against the same table.
pub const OMITTED_ARG: &str = "__rock_omitted";

/// Chunk-level local that every emitted function reads a mutable global
/// through: the process's one global storage table, `_G.__rock_globals`. Each
/// chunk (a full program or an overlay) declares its own and assigns the same
/// table, so no upvalue ever needs rejoining.
pub const STATE_CELL: &str = "__rock_state_cell";

/// The return statements of a one-expression wrapper -- an `extern(ffi)`
/// binding, a value-struct `_vt`/`_lct` ctor -- whose whole result is `expr`.
/// One-space indented, no trailing newline.
///
/// `return lib.f(..)` would be a `CALLT` into C: a root trace started at the
/// wrapper's entry cannot leave through it (`rec_ret` needs a real `RET`) and
/// aborts `leaving loop in root trace` every time it is tried. Returning
/// through a local costs one bytecode and gives the trace a real `RET`. The
/// local is a reserved `__rock` name so it can never shadow a wrapper
/// parameter that `expr` reads.
pub fn wrapper_return(expr: &str) -> String {
    format!(" local __rock_ret = {expr}\n return __rock_ret")
}

/// The shared per-type metatable global of a GC-tier `value` struct, so
/// LuaJIT's `__eq` fires between any two instances.
pub fn value_struct_metatable_name(name: &str) -> String {
    format!("__rock_vt_{name}")
}

/// LuaJIT's hard limit on upvalues per function prototype (`LJ_MAX_UPVAL`).
/// A body with more bindable callees than this binds the first
/// `MAX_BOUND_CALLEES` in name order and leaves the rest global lookups.
pub const MAX_BOUND_CALLEES: usize = 60;

/// The factory holding the only copy of a bound function's body.
pub const CALLEE_FACTORY_PREFIX: &str = "__rock_mk_";

/// Recover the body symbol from a top-level emitted definition header.
/// Factories return the body on the same line, preserving body-local offsets.
pub fn emitted_body_name(line: &str) -> Option<&str> {
    let name = if let Some(rest) = line.strip_prefix("function ") {
        rest.split_once('(')?.0
    } else {
        let rest = line.strip_prefix(CALLEE_FACTORY_PREFIX)?;
        let (name, signature) = rest.split_once(" = function(")?;
        let (_, body) = signature.split_once(')')?;
        body.strip_prefix(" return function(")?;
        name
    };
    is_lua_ident(name).then_some(name)
}

type EnumBoxGroups = Vec<(Vec<ustr::Ustr>, (usize, Vec<ustr::Ustr>))>;

enum SelectionValue {
    MinMax(Idx<Inst>, Idx<Inst>, &'static str),
    Clamp(Idx<Inst>, f64, f64),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SelectionTarget {
    Cell { cell: Idx<Cell>, join: Idx<Block> },
    Return,
}

#[derive(Clone, Copy)]
enum SelectionArm {
    Join {
        store: Option<(Idx<Cell>, Idx<Inst>)>,
        join: Idx<Block>,
    },
    Return(Idx<Inst>),
}

impl SelectionArm {
    fn target_value(self) -> Option<(SelectionTarget, Idx<Inst>)> {
        match self {
            Self::Join {
                store: Some((cell, value)),
                join,
            } => Some((SelectionTarget::Cell { cell, join }, value)),
            Self::Return(value) => Some((SelectionTarget::Return, value)),
            _ => None,
        }
    }
}

thread_local! {
    static INSTANCE_SYMBOLS: RefCell<(u64, HashMap<skull_mir::instance::Instance, ustr::Ustr>)> =
        RefCell::default();
}

fn next_symbols_epoch() -> u64 {
    // From 1: a thread's empty table starts at epoch 0.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

struct Emitter<'a> {
    bound: Option<&'a CalleeBindingPlan>,
    enum_box_groups: RefCell<HashMap<ustr::Ustr, std::rc::Rc<EnumBoxGroups>>>,
    selection_preds: std::cell::OnceCell<PredMap>,
    ws: &'a Workspace,
    ctx: &'a CodegenCtx,
    interner: &'a MonoInterner,
    func: &'a MirFunc,
    /// `u32::MAX` = uncolored.
    colors: Vec<u32>,
    /// `None` = function top.
    color_scope: Vec<Option<Idx<Block>>>,
    cell_colors: &'a [u32],
    frame: Option<&'a FrameCtx>,
    loops: &'a HashMap<Idx<Block>, ForLoop>,
    rpo: &'a [Idx<Block>],
    inlinable: &'a [bool],
    alias: &'a [Option<Idx<Cell>>],
    emitted: Vec<bool>,
    label_refs: Vec<u32>,
    control: ControlLines,
    hoist: &'a HashMap<Idx<Inst>, u32>,
    hoist_names: &'a [String],
    state_slots: &'a HashMap<ustr::Ustr, u32>,
    static_slots: &'a HashMap<ustr::Ustr, String>,
    allocation: Option<AllocationEmission<'a>>,
    tail_forwards: &'a [Option<&'a [Idx<Inst>]>],
    call_arg_forwards: &'a CallArgForwards,
    suppressed: &'a [bool],
    kills: regalloc::NilKills<'a>,
    line_marks: Option<&'a RefCell<Vec<LineMark>>>,
}

fn write_lit(out: &mut String, l: &Lit) {
    match l {
        Lit::Int(i) => {
            let _ = write!(out, "{i}");
        }
        Lit::Uint(i) => {
            let _ = write!(out, "{i}");
        }
        Lit::Float(x) if x.is_nan() => out.push_str("(0 / 0)"),
        Lit::Float(x) if x.is_infinite() => {
            out.push_str(if x.is_sign_negative() {
                "(-1 / 0)"
            } else {
                "(1 / 0)"
            });
        }
        Lit::Float(x) => {
            let _ = write!(out, "{x:?}");
        }
        Lit::Bool(b) => {
            let _ = write!(out, "{b}");
        }
        Lit::Str(s) => write_lua_string(out, s.as_str()),
        Lit::Nil => out.push_str("nil"),
        Lit::Omitted => out.push_str(OMITTED_ARG),
    }
}

/// Escape `s` as a double-quoted Lua string literal.
pub fn lua_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    write_lua_string(&mut out, s);
    out
}

fn write_vec(out: &mut String, len: usize, items: impl FnOnce(&mut String)) {
    if len == 0 {
        // Reserve four array slots without a constructor call frame.
        out.push_str("setmetatable({[0]=0,nil,nil,nil,nil}, __rock_box_mts.Vec)");
        return;
    }
    let _ = write!(out, "setmetatable({{[0]={len},");
    items(out);
    out.push_str("}, __rock_box_mts.Vec)");
}

/// A Rock `[string]` value as Lua.
pub fn lua_string_vec(items: &[&str]) -> String {
    let mut out = String::new();
    write_vec(&mut out, items.len(), |out| {
        for (i, s) in items.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            write_lua_string(out, s);
        }
    });
    out
}

/// The `[int]` sibling of [`lua_string_vec`].
pub fn lua_int_vec(items: &[i64]) -> String {
    let mut out = String::new();
    write_vec(&mut out, items.len(), |out| {
        for (i, n) in items.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            let _ = write!(out, "{n}");
        }
    });
    out
}

/// Lua keywords never reach here: the tokenizer escapes them to `__rock_<kw>`.
/// A bracketed constant key compiles to the same `TGETS` as dot syntax.
fn is_lua_ident(s: &str) -> bool {
    let mut cs = s.chars();
    cs.next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && cs.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn write_field_key(out: &mut String, field: &str) {
    if field == skull_mir::mir::VEC_LENGTH_SLOT {
        out.push_str("[0]");
    } else if is_lua_ident(field) {
        let _ = write!(out, ".{field}");
    } else {
        out.push('[');
        write_lua_string(out, field);
        out.push(']');
    }
}

fn write_table_key(out: &mut String, field: &str) {
    if is_lua_ident(field) {
        let _ = write!(out, "{field} = ");
    } else {
        out.push('[');
        write_lua_string(out, field);
        out.push_str("] = ");
    }
}

fn write_lua_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_ascii_control() => {
                let _ = write!(out, "\\{:03}", c as u32);
            }
            _ => out.push(c),
        }
    }
    out.push('"');
}

pub(crate) fn raises(ws: &Workspace, func: &MirFunc, bb: Idx<Block>) -> bool {
    matches!(func.blocks[bb].term, Term::Unreachable)
        && func.blocks[bb].insts.last().is_some_and(|last| {
            matches!(func.insts[*last], Inst::Call { func: callee, .. }
                if skull_mir::intrinsics::never_returns(func_of(ws, callee)))
        })
}
