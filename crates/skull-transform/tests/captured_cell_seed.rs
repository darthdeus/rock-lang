use comfy_la_arena::Idx;
use skull_mir::mir::{Inst, MirFunc};
use skull_transform::scope::ScopeTree;
use ustr::Ustr;

fn converted(src: &str, edit: impl FnOnce(&mut MirFunc)) -> (MirFunc, String) {
    let ast = skull_parser::parse_module_clean(src, Ustr::from("<captured-cell-seed>"))
        .expect("parse");
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut vec![ast]).expect("semantic");
    let mid = ws.module_ids[&Ustr::from("<captured-cell-seed>")];
    let (fid, _) = ws.module(mid).functions.iter().find(|(_, f)| f.name.as_str() == "f")
        .expect("function");
    let root = skull_mir::mir::FuncRef { module: mid, func: fid };
    let interner = std::sync::Arc::new(skull_mir::monoty::MonoInterner::new());
    let graph = skull_graph::instance_graph::build_in(&ws, vec![root], false, interner.clone());
    let concrete = skull_graph::concrete_body::build(&ws, &graph);
    let scx = skull_mir::lower::ScalarCtx::new(&ws).sharing(interner);
    let plan = &concrete.plans[&skull_mir::instance::Instance::base(root)];
    let mut mf = skull_mir::lower::lower_instance_with_plans(&ws, &scx, plan, &concrete.plans)
        .expect("lower");
    edit(&mut mf);
    skull_transform::convert_closures(&mut mf, "f");
    let ctx = skull_codegen_lua::CodegenCtx::new(&ws);
    let mut lua = skull_codegen_lua::FN_MT_PRELUDE.to_string();
    lua.push_str(&skull_codegen_lua::emit_named(&ws, &ctx, scx.interner(), "f", &mf, None));
    for nested in &mf.nested_fns {
        lua.push_str(&skull_codegen_lua::emit_named(
            &ws, &ctx, scx.interner(), nested.name.as_str(), nested, None,
        ));
    }
    let mut artifact = skull_mir::code_identity::CodeArtifact::from_mir_tree(&mf);
    artifact.set_body_names(&mf, "f");
    lua.push_str("__rock_code_slot_begin()\n");
    for entry in artifact.entries.values().filter(|e| e.escaping) {
        lua.push_str(&format!("__rock_code_slot_stage({}, {}, {})\n",
            skull_codegen_lua::lua_string(&entry.identity.logical_key(&ws, scx.interner())),
            entry.body_name.expect("body"),
            skull_codegen_lua::lua_string(&entry.compatibility.call_abi)));
    }
    lua.push_str("__rock_code_slot_publish()\n");
    (mf, lua)
}

fn boxes(mf: &MirFunc) -> Vec<&Vec<(Ustr, Idx<Inst>)>> {
    skull_mir::cfg::reachable_blocks_from(mf, mf.entry).into_iter()
        .flat_map(|b| mf.blocks[b].insts.iter()).filter_map(|&id| match &mf.insts[id] {
        Inst::MakeEnv { fn_ref: None, fields } => Some(fields),
        _ => None,
    }).collect()
}

fn execute(lua: &str, driver: &str) {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("luajit").arg("-").stdin(Stdio::piped())
        .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("luajit");
    child.stdin.take().unwrap().write_all(format!("{lua}\n{driver}\n").as_bytes())
        .expect("write Lua");
    let output = child.wait_with_output().expect("Lua exit");
    assert!(output.status.success(), "{}\n{lua}", String::from_utf8_lossy(&output.stderr));
}

#[test]
fn unconditional_mutable_value_initializer_seeds_the_box() {
    let (mf, lua) = converted(
        "value V { a: int }\n\
         fn f(input: V, replacement: V) -> fn() -> V {\n\
             let n = input\n\
             return () => {\n\
                 let previous = n\n\
                 n = replacement\n\
                 return previous\n\
             }\n\
         }", |_| {},
    );
    let boxes = boxes(&mf);
    assert_eq!(boxes.len(), 1, "one shared mutable cell: {lua}");
    assert_eq!(boxes[0].len(), 1, "initializer must seed the shared box: {lua}");
    assert_eq!(boxes[0][0].0, Ustr::from("v"));
    assert!(lua.contains("{v = "), "seeded Lua constructor: {lua}");
    execute(&lua, "
        function V_ct(a) return {a = a} end
        local input, replacement = {a = 37}, {a = 91}
        local other_input, other_replacement = {a = -12}, {a = 6}
        local first = f(input.a, replacement.a)
        local second = f(other_input.a, other_replacement.a)
        assert(first().a == input.a)
        assert(second().a == other_input.a)
        assert(first().a == replacement.a)
        assert(second().a == other_replacement.a)
        assert(first().a == replacement.a)
    ");
}

const MUTABLE_LOCAL: &str = "fn f(input: int, replacement: int) -> fn() -> int {\n\
    let n = input\n\
    let read = () => n\n\
    n = replacement\n\
    return read\n\
}";

#[test]
fn pre_initialization_read_retains_the_entry_box() {
    let (mf, lua) = converted(MUTABLE_LOCAL, |mf| {
        let tree = ScopeTree::build(mf);
        let own = tree.analyze(mf);
        let cell = *own.captures[1].iter().next().unwrap();
        let init = mf.blocks[mf.entry].insts.iter().position(|&id|
            matches!(mf.insts[id], Inst::Store { cell: c, .. } if c == cell)).unwrap();
        let read = mf.alloc_inst(Inst::Load(cell));
        mf.blocks[mf.entry].insts.insert(init, read);
    });
    assert!(boxes(&mf)[0].is_empty(), "pre-initialization read needs an entry cell: {lua}");
}

#[test]
fn branch_dependent_initialization_retains_the_entry_box() {
    let (mf, lua) = converted("fn f(input: int, replacement: int, branch: bool) -> fn() -> int {\n\
        let n = input\n\
        if branch {\n\
            n = replacement\n\
            n = input\n\
        }\n\
        return () => n\n\
    }", |mf| {
        let tree = ScopeTree::build(mf);
        let own = tree.analyze(mf);
        let cell = *own.captures[1].iter().next().unwrap();
        let init = mf.blocks[mf.entry].insts.iter().position(|&id|
            matches!(mf.insts[id], Inst::Store { cell: c, .. } if c == cell)).unwrap();
        mf.blocks[mf.entry].insts.remove(init);
    });
    assert!(boxes(&mf)[0].is_empty(), "conditional stores cannot seed the cell: {lua}");
    execute(&lua, "
        local input, replacement = 37, 91
        assert(f(input, replacement, true)() == input)
        assert(f(input, replacement, false)() == nil)
    ");
}

#[test]
fn repeated_initializer_does_not_split_the_shared_cell() {
    let (mf, lua) = converted("fn f(input: int, replacement: int, again: bool) -> fn() -> int {\n\
        let read = () => input\n\
        while again {\n\
            let n = input\n\
            read = () => {\n\
                n = replacement\n\
                return n\n\
            }\n\
        }\n\
        return read\n\
    }", |_| {});
    assert!(boxes(&mf)[0].is_empty(), "a loop must not recreate a root-owned cell: {lua}");
}

#[test]
fn initializer_after_a_join_can_seed_the_box() {
    let (mf, lua) = converted("fn f(input: int, replacement: int, branch: bool) -> fn() -> int {\n\
        let n = if branch { input } else { replacement }\n\
        return () => {\n\
            let previous = n\n\
            n = replacement\n\
            return previous\n\
        }\n\
    }", |_| {});
    assert_eq!(boxes(&mf)[0].len(), 1, "the join dominates capture and use: {lua}");
    execute(&lua, "
        local input, replacement = 37, 91
        local yes, no = f(input, replacement, true), f(input, replacement, false)
        assert(yes() == input)
        assert(no() == replacement)
        assert(yes() == replacement)
    ");
}

#[test]
fn forward_capture_retains_the_entry_box() {
    // Construct a forward capture directly: the MIR transform must preserve
    // its shared cell even when source definite-assignment disallows the shape.
    let (mf, lua) = converted(
        "fn f(input: int, replacement: int) -> fn() -> int {\n\
             let n = input\n\
             let read = () => n\n\
             n = replacement\n\
             return read\n\
         }", |mf| {
            let tree = ScopeTree::build(mf);
            let own = tree.analyze(mf);
            let cell = *own.captures[1].iter().next().unwrap();
            let lambda = tree.scopes[1].lambda.unwrap();
            let init = mf.blocks[mf.entry].insts.iter().position(|&id|
                matches!(mf.insts[id], Inst::Store { cell: c, .. } if c == cell)).unwrap();
            mf.blocks[mf.entry].insts.retain(|&id| id != lambda);
            mf.blocks[mf.entry].insts.insert(init, lambda);
        },
    );
    let boxes = boxes(&mf);
    assert_eq!(boxes.len(), 1);
    assert!(boxes[0].is_empty(), "forward capture needs its pre-initialization cell: {lua}");
    execute(&lua, "
        local input, replacement = 37, 91
        local read = f(input, replacement)
        assert(read() == replacement)
    ");
}
