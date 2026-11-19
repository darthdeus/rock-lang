//! [`constructed_never_nil`] may delete code, so it admits only values this
//! body just constructed. [`inst_never_nil_with`] only drops the Lua emitter's
//! nil-write guard, where a wrong answer costs a rehash, not a wrong value.

use crate::mir::{Inst, Lit, MirFunc};
use comfy_la_arena::Idx;

/// The only tier a consumer may use to delete a test. SROA forwards field reads
/// to their constructor first; anything broader could delete a live branch.
pub fn constructed_never_nil(func: &MirFunc, id: Idx<Inst>) -> bool {
    matches!(
        &func.insts[id],
        Inst::MakeValueStruct { .. }
            | Inst::MakeStruct { .. }
            | Inst::MakeEnum { .. }
            | Inst::MakeTuple(_)
    )
}

/// `extra` answers loads, calls and field/element reads, and receives the inst
/// left after transparent conversions are peeled. Must not decide a branch.
pub fn inst_never_nil_with(
    func: &MirFunc,
    id: Idx<Inst>,
    extra: &dyn Fn(Idx<Inst>) -> bool,
) -> bool {
    // Int-to-float conversions render as their operand on both backends, so they
    // are peeled. Operands only reference earlier insts, so this terminates.
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

        // Arithmetic raises or produces a number, never nil.
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
        // These conversions materialize: a nil operand raises (or becomes a number on
        // JS) rather than passing through.
        | Inst::FloatToInt(..)
        | Inst::FloatToF32(..)
        | Inst::IntToUint(..)
        | Inst::FloatToUint(..)
        | Inst::UintBitcastInt(..)
        | Inst::IntBitcastUint(..)
        | Inst::VecIndexSlot(..)
        | Inst::Len { .. }
        | Inst::StringConcat(..)
        | Inst::ToString(..)
        | Inst::Format { .. }
        | Inst::FormatDyn { .. }
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
        // `MaterializeValueStruct` emits as `MakeValueStruct`; `VecStrideNilToFalse`
        // replaces nil with a false sentinel.
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

        // An absent nullable value is nil by construction, and effects are not values:
        // those stay `false` whatever `extra` says.
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

/// `Unit` renders as nil, `Any` and `Never` promise nothing, and a newtype is
/// conservatively excluded, matching the emitter's HIR walk.
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
