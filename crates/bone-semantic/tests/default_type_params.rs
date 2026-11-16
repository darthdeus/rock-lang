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

fn check_numeric_defaults(default: &str, expected: &str) {
    let decl = format!(
        "struct Holder<T = {default}> {{ value: T }}\n\
         enum Choice<T = {default}> {{ Value(T), Empty }}\n\
         newtype Wrapped<T = {default}>(T)\n"
    );
    let use_site = format!(
        "fn read_holder(h: Holder) -> {expected} {{ h.value }}\n\
         fn read_choice(c: Choice) -> {expected} {{\n\
             match c {{ Choice::Value(v) => v, Choice::Empty => 0 }}\n\
         }}\n\
         fn read_wrapped(w: Wrapped) -> {expected} {{ w.0 }}\n"
    );
    for use_first in [false, true] {
        let mut modules = [parse("decl.rock", &decl), parse("use.rock", &use_site)];
        if use_first {
            modules.swap(0, 1);
        }
        semantic_pass(&mut modules).expect("builtin defaults agree with explicit result types");
    }
}

#[test]
fn uint_defaults_across_declarations_and_module_orders() {
    check_numeric_defaults("uint", "uint");
}

#[test]
fn f64_defaults_across_declarations_and_module_orders() {
    check_numeric_defaults("f64", "float");
}
