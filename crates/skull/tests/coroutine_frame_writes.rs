use crate::common;
use rustc_hash::{FxHashMap, FxHashSet};
use skull_codegen_lua::{CodegenCtx, FrameCtx};
use skull_mir::{
    mir::{Inst, Lit, MirFunc, Term},
    monoty::MonoInterner,
};
use ustr::Ustr;

const FRAME_WRITES: &str = r#"
enum FrameYield { Empty, Pair(int, int) }
struct UserBox { value: int? = null }
global calls: int = 0

fn next_frame_value(i: int) -> FrameYield {
    calls += 1
    if i % 2 == 0 { return FrameYield::Pair(calls, calls + 10) }
    return FrameYield::Empty
}

co fn frame_values() -> Coroutine<FrameYield> {
    for i in 0..4 { yield next_frame_value(i) }
}

inline(lua) fn frame_payload_empty(c: Coroutine<FrameYield>) -> bool = """
    return c.__y1 == nil and c.__y2 == nil
"""
inline(lua) fn restore_frame(c: Coroutine<FrameYield>) -> Coroutine<FrameYield> = """
    local copy = {}
    for k, v in pairs(c) do copy[k] = v end
    return copy
"""
inline(lua) fn observe_user_writes(b: UserBox) = """
    b.__hits = 0
    setmetatable(b, { __newindex = function(t, k, v)
        t.__hits = t.__hits + 1
        rawset(t, k, v)
    end })
"""
inline(lua) fn user_writes(b: UserBox) -> int = """
    return b.__hits
"""
fn clear_user_box(b: UserBox) { b.value = null }

fn main() {
    let c = frame_values()
    print(c.resume() == FrameYield::Pair(1, 11))
    print(c.resume() == FrameYield::Empty)
    print(frame_payload_empty(c))
    c = restore_frame(c)
    print(c.resume() == FrameYield::Pair(3, 13))
    print(c.resume() == FrameYield::Empty)
    print(frame_payload_empty(c))
    print(calls)
    let b = UserBox {}
    observe_user_writes(b)
    clear_user_box(b)
    clear_user_box(b)
    print(user_writes(b))
}
"#;

#[test]
fn spilled_int_to_float_alias_reads_a_valid_slot_after_resume() {
    let ws = bone_semantic::prelude::Workspace::new();
    let ctx = CodegenCtx::new(&ws);
    let interner = MonoInterner::default();
    let mut func = MirFunc::new(Ustr::from("spill_alias"));
    func.is_coroutine = true;
    let n = func.new_cell(Some(Ustr::from("n")));
    func.params.push(n);
    func.resume_arg = Some(func.new_cell(Some(Ustr::from("__resume_arg"))));
    let entry = func.entry();
    let resume = func.new_block();
    let done = func.new_block();
    let ln = func.emit(entry, Inst::Load(n));
    let k2 = func.emit(entry, Inst::Const(Lit::Int(2)));
    let m = func.emit(entry, Inst::IntMul(ln, k2));
    let conv = func.emit(entry, Inst::IntToFloat(m));
    let k1 = func.emit(entry, Inst::Const(Lit::Int(1)));
    let q = func.emit(entry, Inst::IntAdd(m, k1));
    let qa = func.new_cell(Some(Ustr::from("qa")));
    let qb = func.new_cell(Some(Ustr::from("qb")));
    func.emit(entry, Inst::Store { cell: qa, value: q });
    func.emit(entry, Inst::Store { cell: qb, value: q });
    func.set_term(
        entry,
        Term::Yield {
            values: vec![conv],
            resume,
        },
    );
    let s = func.emit(resume, Inst::FloatAdd(conv, m));
    func.set_term(
        resume,
        Term::Yield {
            values: vec![s],
            resume: done,
        },
    );
    func.set_term(done, Term::Return(vec![]));

    let lowered = skull_transform::transform_coroutine(&func, None, false);
    let frame = FrameCtx {
        s_cell: lowered.s_cell,
        fields: lowered.frame_plan.fields().iter().copied().collect(),
        slots: lowered.frame_plan.slots().clone(),
    };
    let lua = skull_codegen_lua::emit_named_framed(
        &ws,
        &ctx,
        &interner,
        "spill_alias__step",
        &lowered.step,
        Some(&frame),
        None,
    );
    assert!(lowered.frame_plan.spill_values().contains(&conv));
    let n_slot = lowered
        .frame_plan
        .slots()
        .get(&n)
        .copied()
        .expect("param is a frame field");
    let layout = lowered.frame_plan.layout_hash();
    let driver = format!(
        "{lua}\nlocal s = {{__state = 0, __layout = {layout}, c{n_slot} = 5}}\nprint(spill_alias__step(s))\nprint(spill_alias__step(s))\n"
    );
    assert_eq!(common::run_luajit(&driver), "10\n20");
}

#[test]
fn a_resumed_v1_enum_box_carries_the_types_runtime_tag() {
    let emitted = common::emit_v1_package(
        "v1yield",
        &[(
            "main.rock",
            r#"
enum Step { Go(string), Done }
co fn steps() -> Coroutine<Step> { yield Step::Go("one") }
inline(lua) fn erased_resume_tag(c: Coroutine<Step>) -> string = "return __rock_get_type(c.__resume(c))"
inline(lua) fn unit_tag(v: Step) -> string = "return __rock_get_type(v)"
pub fn probe() -> string {
    return "{erased_resume_tag(steps())} {unit_tag(Step::Done)}"
}
fn main() {}
"#,
        )],
    );
    let probe = common::mangled_name(&emitted.lua, "probe");
    let out = common::run_luajit(&format!("{}\nprint({probe}())\n", emitted.lua));
    let (resumed, built) = out.split_once(' ').expect("two tags");
    assert_eq!(resumed, built);
    assert!(!resumed.contains("::"));
}
