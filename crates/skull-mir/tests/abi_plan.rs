//! The backend ABI of ONE EMITTED BODY (`docs/monomorphization-plan.md` cut
//! C+D, step A*).
//!
//! Pre-cutover, emission writes two populations of definition: one shared body
//! per declaration, plus a specialized body per `(FuncRef, SpecKey)` the
//! lowerer decided to serve. Those are the emitted bodies, and each has its own
//! backend ABI -- a generic that takes and returns `T` is one boxed slot on the
//! declaration and two float leaves at `T = V2`. The declaration's answer is
//! not a narrower version of the instantiation's, it is a different question.
//!
//! What this pins is that ONE oracle answers it for whichever body is being
//! emitted, so a definition's parameter list and the call that targets it
//! cannot come from two derivations that merely agree today.

use std::sync::LazyLock;

use skull_mir::lower::{BackendFnAbiPlan, EmittedBodyId, ScalarCtx, SpecKey};
use skull_mir::mir::FuncRef;
use skull_mir::monoty::{MonoCtor, MonoTy};
use ustr::Ustr;

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_host::stdlib_loader::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(format!("<stdlib:{}>", f.name).as_str());
            skull_parser::parse_module_clean(f.contents, id).expect("stdlib parse")
        })
        .collect()
});

fn ws_of(src: &str) -> bone_semantic::inference::Workspace {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let errs = skull_mir::verdict::body_errors(&ws);
    assert!(errs.is_empty(), "fixture must type-check: {errs:?}");
    ws
}

fn func_ref(ws: &bone_semantic::inference::Workspace, name: &str) -> FuncRef {
    let overloads = ws.functions_by_name.get(&Ustr::from(name)).expect("declared");
    assert_eq!(overloads.len(), 1, "`{name}` must be unambiguous in the fixture");
    let (module, func) = overloads[0];
    FuncRef { module, func }
}

fn nominal(name: &str) -> MonoTy {
    MonoTy::App { ctor: MonoCtor::Nominal(Ustr::from(name)), params: Vec::new() }
}

/// A generic whose value instantiation splats, plus a non-generic control that
/// carries the same ABI unconditionally.
const SRC: &str = "\
value V2 { x: float, y: float }

fn ident<T>(v: T) -> T {
    return v
}

fn shift(v: V2) -> V2 {
    return V2 { x: v.x + 1.0, y: v.y }
}

fn main() {
    let a = ident(V2 { x: 1.0, y: 2.0 })
    let b = shift(a)
    print(\"{b.x}\")
}
";

/// The two emitted bodies of one generic declaration have DIFFERENT ABIs, and
/// the oracle answers for whichever one is being emitted.
///
/// The declaration arm is the admission control: it must keep answering the
/// shared body's boxed ABI, so a plan that simply always specialized would fail
/// here rather than look like success.
#[test]
fn an_emitted_body_states_its_own_abi() {
    let ws = ws_of(SRC);
    let scx = ScalarCtx::new(&ws);
    let ident = func_ref(&ws, "ident");

    let shared = BackendFnAbiPlan::for_body(&ws, &scx, EmittedBodyId::Shared(ident));
    assert_eq!(shared.param_leaf_widths(), None, "the shared body keeps one boxed slot");
    assert_eq!(shared.result_lanes(), 1);

    let key: SpecKey = vec![Some(nominal("V2"))];
    let spec = BackendFnAbiPlan::for_body(&ws, &scx, EmittedBodyId::Specialized(ident, key));
    assert_eq!(
        spec.param_leaf_widths(),
        Some(vec![2]),
        "`ident<V2>` takes V2's two float leaves"
    );
    assert_eq!(spec.result_lanes(), 2, "and returns them on two lanes");
}

/// The control: a non-generic declaration has exactly one emitted body, and the
/// shared plan already states its scalarized ABI. Without this arm the test
/// above could pass on an oracle that answers only for generics.
#[test]
fn a_non_generic_body_states_its_abi_on_the_shared_id() {
    let ws = ws_of(SRC);
    let scx = ScalarCtx::new(&ws);
    let shift = func_ref(&ws, "shift");

    let plan = BackendFnAbiPlan::for_body(&ws, &scx, EmittedBodyId::Shared(shift));
    assert_eq!(plan.param_leaf_widths(), Some(vec![2]));
    assert_eq!(plan.result_lanes(), 2);
}
