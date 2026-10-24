use skull_mir::mir::{Inst, Lit, MirFunc, Term};
use ustr::Ustr;

#[test]
fn native_view_reads_without_a_canonical_check_retain_their_error() {
    let source = "@nomangle inline(lua) fn __nb_checked_index(buf: any, i: int) -> int = \"return i\"";
    let ast = skull_parser::parse_module_clean(source, Ustr::from("<test>")).unwrap();
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut [ast]).unwrap();
    let &(module, func) = &ws.functions_by_name[&Ustr::from("__nb_checked_index")][0];
    for with_user_check in [false, true] {
        let mut body = MirFunc::new(Ustr::from("view_read"));
        let block = body.entry;
        let base = body.emit(block, Inst::Const(Lit::Nil));
        let index = body.emit(block, Inst::Const(Lit::Int(0)));
        if with_user_check {
            body.emit(block, Inst::Call { func: skull_mir::mir::FuncRef { module, func }, args: vec![base, index] });
        }
        let read = body.emit(block, Inst::Field { base, field: skull_mir::lower::leaf_view_field("double") });
        body.set_term(block, Term::Return(vec![]));
        skull_transform::cleanup_with_nobox(&ws, &mut body).unwrap();
        assert!(body.blocks[block].insts.contains(&read), "a missing or user-spelled check cannot prove the read safe");
    }
}
