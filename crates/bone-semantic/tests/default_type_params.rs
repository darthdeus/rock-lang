//! Default type params work cross-module regardless of module order: the
//! defaults map is built in the pre-lowering pass (the `type_generic_arity`
//! pattern), so a use site lowering before its decl's module still pads.

use bone_semantic::prelude::*;

fn parse(name: &str, src: &str) -> skull_parser::Ast {
    skull_parser::parse_module_clean(src, Ustr::from(name)).expect("parse")
}

const DECL: &str = "struct Pair<T, U = int> {\n    a: T,\n    b: U,\n}\n";
const USE_SITE: &str = "fn second(p: Pair<string>) -> int {\n    p.b\n}\n";

#[test]
fn use_module_before_decl_module() {
    semantic_pass(&mut [parse("use.rock", USE_SITE), parse("decl.rock", DECL)])
        .expect("padded App typechecks with the use module lowered first");
}

#[test]
fn decl_module_before_use_module() {
    semantic_pass(&mut [parse("decl.rock", DECL), parse("use.rock", USE_SITE)])
        .expect("padded App typechecks with the decl module lowered first");
}
