//! Whether a MIR value can be nil at runtime.
//!
//! Two tiers, separated by what a wrong answer costs, not by how much they
//! can prove:
//!
//! - [`constructed_never_nil`] is for consumers that DELETE code on the
//!   answer (cleanup's presence-test folding). A wrong `true` removes a
//!   live branch, so it admits only values this body just constructed.
//! - [`inst_never_nil_with`] only skips the Lua emitter's nil-write guard.
//!   For plain Rock tables, a wrong answer costs a rehash rather than a
//!   wrong value, so this tier may use declared types and emitted shapes.
//!   Heap-record guards preserve host callbacks by testing for a metatable
//!   before deciding that a nil store is unobservable.
//!
//! Neither tier may assume "probably not nil". The second is wider only
//! because its blast radius is bounded; promoting one of its answers to
//! the first tier needs the argument that the value cannot be nil, not
//! that it usually is not.
//!
//! Int-to-float subsumption is a NO-OP on both value backends, so
//! `IntToFloat`/`UintToFloat`/`UintToInt` render as their operand and are
//! exactly as nil-able as it is. They are peeled rather than answered,
//! because the coercion funnel peels nullable when it classifies an
//! operand: `let v: float? = some_int_opt` wraps a value that IS nil when
//! absent.

use crate::mir::{Inst, Lit, MirFunc};
use comfy_la_arena::Idx;

/// The value `id` is an aggregate this body just constructed, so a
/// presence test on it is decided: the constructor cannot have produced
/// nil. This is the only tier a consumer may use to DELETE a test.
///
/// It covers aggregate constructors. SROA forwards a field read to its
/// constructor before this check; a broader classification could delete a live
/// branch, as with presence tests on int-to-float-widened nullable values.
pub fn constructed_never_nil(func: &MirFunc, id: Idx<Inst>) -> bool {
    matches!(
        &func.insts[id],
        Inst::MakeValueStruct { .. }
            | Inst::MakeStruct { .. }
            | Inst::MakeEnum { .. }
            | Inst::MakeTuple(_)
    )
}

/// Whether `id` can be nil where the emitted code reads it, with `extra`
/// answering for the instructions this module cannot decide: loads, calls,
/// field and element reads. `extra` receives the instruction reached after
/// transparent conversions are peeled, so a caller that knows a cell's
/// type sees that cell's load rather than the coercion wrapped around it.
///
/// This tier only informs the Lua emitter's store guard. A wrong answer
/// does not change a plain Rock table's value. It must not decide a
/// branch: see [`constructed_never_nil`].
pub fn inst_never_nil_with(
    func: &MirFunc,
    id: Idx<Inst>,
    extra: &dyn Fn(Idx<Inst>) -> bool,
) -> bool {
    // A chain of conversions is a chain of renamings; walk to the value
    // that actually decides. MIR instructions only reference earlier ones,
    // so this terminates.
    let mut id = id;
    while let Inst::IntToFloat(x)
    | Inst::UintToFloat(x)
    | Inst::UintToInt(x)
    | Inst::NbIndexSlot(x) = &func.insts[id]
    {
        id = *x;
    }
    match &func.insts[id] {
        // `Omitted` is a private marker table, not nil.
        Inst::Const(lit) => !matches!(lit, Lit::Nil),

        // Numbers: arithmetic raises or produces a number, never nil.
        Inst::IntAdd(..)
        | Inst::IntSub(..)
        | Inst::IntMul(..)
        | Inst::IntDiv(..)
        | Inst::IntMod(..)
        | Inst::UintAdd(..)
        | Inst::UintSub(..)
        | Inst::UintMul(..)
        | Inst::UintDiv(..)
        | Inst::UintMod(..)
        | Inst::FloatAdd(..)
        | Inst::FloatSub(..)
        | Inst::FloatMul(..)
        | Inst::FloatDiv(..)
        | Inst::FloatMod(..)
        | Inst::Neg(..)
        | Inst::BitNot(..)
        | Inst::UintBitNot(..)
        | Inst::BitAnd(..)
        | Inst::BitOr(..)
        | Inst::BitXor(..)
        | Inst::Shl(..)
        | Inst::Shr(..)
        | Inst::UintBitAnd(..)
        | Inst::UintBitOr(..)
        | Inst::UintBitXor(..)
        | Inst::UintShl(..)
        | Inst::UintShr(..)
        // These conversions materialize: a nil operand raises (or wraps to
        // a number on JS) rather than passing through.
        | Inst::FloatToInt(..)
        | Inst::FloatToF32(..)
        | Inst::IntToUint(..)
        | Inst::FloatToUint(..)
        | Inst::UintBitcastInt(..)
        | Inst::IntBitcastUint(..)
        | Inst::VecIndexSlot(..)
        | Inst::Len { .. }

        // Strings.
        | Inst::StringConcat(..)
        | Inst::ToString(..)
        | Inst::Format { .. }
        | Inst::FormatDyn { .. }

        // Bools.
        | Inst::IntLt(..)
        | Inst::IntLeq(..)
        | Inst::IntGt(..)
        | Inst::IntGeq(..)
        | Inst::UintLt(..)
        | Inst::UintLeq(..)
        | Inst::UintGt(..)
        | Inst::UintGeq(..)
        | Inst::FloatLt(..)
        | Inst::FloatLeq(..)
        | Inst::FloatGt(..)
        | Inst::FloatGeq(..)
        | Inst::Eq(..)
        | Inst::NotEq(..)
        | Inst::Not(..)

        // Constructed tables, cdata and functions. `MaterializeValueStruct`
        // emits exactly as `MakeValueStruct`; `VecStrideNilToFalse` exists to
        // replace nil with an unambiguous false sentinel.
        | Inst::Lambda { .. }
        | Inst::MakeTuple(..)
        | Inst::MakeVec(..)
        | Inst::MakeVecColumns { .. }
        | Inst::MakeVecColumnHeader { .. }
        | Inst::MakeVecStrided { .. }
        | Inst::MakeVecStridedHeader { .. }
        | Inst::VecStrideNilToFalse(..)
        | Inst::MakeStruct { .. }
        | Inst::MakeValueStruct { .. }
        | Inst::MaterializeValueStruct { .. }
        | Inst::MakeEnum { .. }
        | Inst::MakeEnumDyn { .. }
        | Inst::MakeEnv { .. }
        | Inst::FfiNewArray { .. }
        | Inst::FfiViewCast { .. } => true,

        // Reads, calls and loads carry no nilness of their own, so they ask
        // `extra`. An absent nullable value is nil by construction, and
        // effects are not values: those stay `false` whatever `extra` says.
        Inst::Global(..)
        | Inst::Load(..)
        | Inst::Call { .. }
        | Inst::CallMulti { .. }
        | Inst::CallValue { .. }
        | Inst::CallValueMulti { .. }
        | Inst::CallBoundMulti { .. }
        | Inst::Field { .. }
        | Inst::TupleIndex { .. }
        | Inst::Index { .. }
        | Inst::VecCheckedIndex { .. } => extra(id),

        Inst::VecStrideFalseToNil(..)
        | Inst::MakeNullableValue { .. }
        | Inst::Store { .. }
        | Inst::StoreField { .. }
        | Inst::StoreIndex { .. }
        | Inst::StoreGlobal { .. }
        | Inst::ArrayPush { .. }
        | Inst::CoroSetInner(..)
        | Inst::CoroHook { .. }
        | Inst::MapNext { .. } => false,

        // Peeled above; a transparent conversion never reaches here.
        Inst::IntToFloat(..)
        | Inst::UintToFloat(..)
        | Inst::UintToInt(..)
        | Inst::NbIndexSlot(..) => unreachable!(),
    }
}

/// A value of this concrete type is never nil where it is read: scalars,
/// strings, tables, cdata and functions. `Unit` renders as nil, `Any` and
/// `Never` promise nothing, and `Nullable` is nil while absent. A nominal
/// name counts only when it is a known struct or enum: a newtype carries
/// whatever its underlying type does, and resolving the chain here is not
/// worth its cost, so it stays conservative (matching the emitter's HIR
/// walk, which also leaves newtypes out).
pub fn monoty_never_nil(ws: &bone_core::workspace::Workspace, ty: &crate::monoty::MonoTy) -> bool {
    use crate::monoty::{MonoCtor, MonoTy};
    match ty {
        MonoTy::Int
        | MonoTy::Uint
        | MonoTy::Float
        | MonoTy::Bool
        | MonoTy::String
        | MonoTy::Tuple(..)
        | MonoTy::Function { .. } => true,
        MonoTy::App { ctor, .. } => match ctor {
            MonoCtor::Array | MonoCtor::Map | MonoCtor::Ref => true,
            MonoCtor::Nominal(name) => {
                ws.struct_by_name.contains_key(name) || ws.enums_by_name.contains_key(name)
            }
        },
        MonoTy::Any | MonoTy::Unit | MonoTy::Never | MonoTy::Nullable(..) => false,
    }
}
