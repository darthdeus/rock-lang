use bone_core::hir::BodyOwner;
use bone_semantic::prelude::*;

fn checked(source: &str) -> Workspace {
    let mut modules: Vec<_> = rock_stdlib::stdlib_files()
        .iter()
        .map(|file| {
            skull_parser::parse_module_clean(
                file.contents,
                ustr(&rock_stdlib::stdlib_file_id(file.name)),
            )
            .unwrap()
        })
        .collect();
    modules.push(skull_parser::parse_module_clean(source, ustr("<test>")).unwrap());
    bone_semantic::semantic_passes::semantic_pass(&mut modules).unwrap()
}

#[test]
fn relocatable_refs_cannot_reach_coroutine_frames() {
    for call in [
        "bump(xs[0])",
        "relay(xs[0])",
        "alias(xs[0])",
        "nested(xs[0])",
        "named(other: 0, c: xs[0])",
        "generic(xs[0])",
        "cycle(xs[0], true)",
        "returned(xs[0])",
        "conditional(xs[0], true)",
        "project(outer[0])",
        "bump(cells[0])",
    ] {
        let source = format!(
            "value Cell {{ w: int }}
             value Outer {{ inner: Cell }}
             co fn bump(c: ref Cell) -> Coroutine<int> {{ yield c.w }}
             fn relay(c: ref Cell) -> Coroutine<int> {{ return bump(c) }}
             fn alias(c: ref Cell) -> Coroutine<int> {{ let r = c; return relay(r) }}
             fn nested(c: ref Cell) -> Coroutine<int> {{ return alias(c) }}
             fn named(c: ref Cell, other: int) -> Coroutine<int> {{ return bump(c) }}
             fn project(c: ref Outer) -> Coroutine<int> {{ return bump(c.inner) }}
             co fn generic<T>(c: ref T) -> Coroutine<int> {{ yield 1 }}
             fn same(c: ref Cell) -> ref Cell {{ return c }}
             fn returned(c: ref Cell) -> Coroutine<int> {{ return bump(same(c)) }}
             fn conditional(c: ref Cell, flag: bool) -> Coroutine<int> {{
                 let alias = if flag {{ c }} else {{ c }}
                 return bump(alias)
             }}
             fn cycle(c: ref Cell, stop: bool) -> Coroutine<int> {{
                 if stop {{ return bump(c) }}
                 return cycle(c, true)
             }}
             fn main() {{
                 let xs = [Cell {{ w: 1 }}]
                 let outer = [Outer {{ inner: Cell {{ w: 2 }} }}]
                 let cells: Map<int, Cell> = map_new()
                 cells[0] = Cell {{ w: 3 }}
                 let task = {call}
             }}"
        );
        let ws = checked(&source);
        let module = ws
            .modules
            .iter()
            .find(|m| m.file == ustr("<test>"))
            .unwrap();
        let fid = module
            .functions
            .iter()
            .find(|(_, f)| f.name == "main")
            .unwrap()
            .0;
        let errors = &ws.inference(module.id).unwrap().errors;
        let errors = errors
            .get(&BodyOwner::Function(fid))
            .map_or(&[][..], Vec::as_slice);
        let error = errors
            .iter()
            .find(|error| {
                error
                    .message
                    .contains("cannot pass a reference into a relocatable Vec or Map element")
                    && error.message.contains("coroutine")
            })
            .unwrap_or_else(|| panic!("{call}: {errors:?}"));
        assert_eq!(error.location.file.as_str(), "<test>");
        assert_eq!(error.labels[0].span.offset_range(), {
            let arg = if call.contains("outer") {
                "outer[0]"
            } else if call.contains("cells") {
                "cells[0]"
            } else {
                "xs[0]"
            };
            let start = source.rfind(arg).unwrap();
            (start, start + arg.len())
        });
    }
}

#[test]
fn immediate_flat_refs_and_boxed_coroutine_refs_remain_admitted() {
    let ws = checked(
        "value Cell { w: int }
         struct Holder { cell: Cell }
         co fn bump(c: ref Cell) -> Coroutine<int> { yield c.w }
         fn immediate(c: ref Cell) -> int { return c.w }
         fn recursive(c: ref Cell, stop: bool) -> int {
             if stop { return c.w }
             return recursive(c, true)
         }
         fn mixed(c: ref Cell, boxed: ref Cell) -> Coroutine<int> {
             let _ = immediate(c)
             return bump(boxed)
         }
         fn main() {
             let xs = [Cell { w: 1 }]
             let holder = Holder { cell: Cell { w: 2 } }
             let _ = immediate(xs[0])
             let _ = recursive(xs[0], true)
             let _ = mixed(xs[0], holder.cell)
             let _ = bump(holder.cell)
         }",
    );
    for inference in ws.inferences.iter().flatten() {
        assert!(inference.errors.is_empty(), "{:?}", inference.errors);
    }
}
