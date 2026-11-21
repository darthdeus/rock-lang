use comfy_la_arena::Idx;
use skull_mir::mir::{Cell, Inst, MirFunc};
use skull_transform::scope::ScopeTree;
use ustr::Ustr;

fn converted(src: &str, edit: impl FnOnce(&mut MirFunc)) -> (MirFunc, String) {
    let (ws, interner, mut mf) = crate::support::lower_fixture(src, "<captured-cell-seed>", "f");
    edit(&mut mf);
    skull_transform::convert_closures(&mut mf, "f");
    let ctx = skull_codegen_lua::CodegenCtx::new(&ws);
    let mut lua = skull_codegen_lua::FN_MT_PRELUDE.to_string();
    lua.push_str(&skull_codegen_lua::emit_named(
        &ws, &ctx, &interner, "f", &mf, None,
    ));
    for nested in &mf.nested_fns {
        lua.push_str(&skull_codegen_lua::emit_named(
            &ws,
            &ctx,
            &interner,
            nested.name.as_str(),
            nested,
            None,
        ));
    }
    let mut artifact = skull_mir::code_identity::CodeArtifact::from_mir_tree_with_volatile_vars(
        &mf,
        &Default::default(),
    );
    artifact.set_body_names(&mf, "f");
    lua.push_str("__rock_code_slot_begin()\n");
    for entry in artifact.entries.values().filter(|e| e.escaping) {
        lua.push_str(&format!(
            "__rock_code_slot_stage({}, {}, {})\n",
            skull_codegen_lua::lua_string(&entry.identity.logical_key(&ws, &interner)),
            entry.body_name.expect("body"),
            skull_codegen_lua::lua_string(&entry.compatibility.call_abi)
        ));
    }
    lua.push_str("__rock_code_slot_publish()\n");
    (mf, lua)
}

fn boxes(mf: &MirFunc) -> Vec<&Vec<(Ustr, Idx<Inst>)>> {
    skull_mir::cfg::reachable_blocks_from(mf, mf.entry)
        .into_iter()
        .flat_map(|b| mf.blocks[b].insts.iter())
        .filter_map(|&id| match &mf.insts[id] {
            Inst::MakeEnv {
                fn_ref: None,
                fields,
            } => Some(fields),
            _ => None,
        })
        .collect()
}

/// The first lambda's captured cell and the entry position of its initializer.
fn captured_initializer(mf: &MirFunc) -> (ScopeTree, Idx<Cell>, usize) {
    let tree = ScopeTree::build(mf);
    let own = tree.analyze(mf);
    let cell = *own.captures[1].iter().next().unwrap();
    let init = mf.blocks[mf.entry]
        .insts
        .iter()
        .position(|&id| matches!(mf.insts[id], Inst::Store { cell: c, .. } if c == cell))
        .unwrap();
    (tree, cell, init)
}

fn execute(lua: &str, driver: &str) {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("luajit")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("luajit");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{lua}\n{driver}\n").as_bytes())
        .expect("write Lua");
    let output = child.wait_with_output().expect("Lua exit");
    assert!(
        output.status.success(),
        "{}\n{lua}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn branch_dependent_initialization_retains_the_entry_box() {
    let (mf, lua) = converted(
        "fn f(input: int, replacement: int, branch: bool) -> fn() -> int {\n\
        let n = input\n\
        if branch {\n\
            n = replacement\n\
            n = input\n\
        }\n\
        return () => n\n\
    }",
        |mf| {
            let (_, _, init) = captured_initializer(mf);
            mf.blocks[mf.entry].insts.remove(init);
        },
    );
    assert!(
        boxes(&mf)[0].is_empty(),
        "conditional stores cannot seed the cell: {lua}"
    );
    execute(
        &lua,
        "
        local input, replacement = 37, 91
        assert(f(input, replacement, true)() == input)
        assert(f(input, replacement, false)() == nil)
    ",
    );
}

#[test]
fn repeated_initializer_does_not_split_the_shared_cell() {
    let (mf, lua) = converted(
        "fn f(input: int, replacement: int, again: bool) -> fn() -> int {\n\
        let read = () => input\n\
        while again {\n\
            let n = input\n\
            read = () => {\n\
                n = replacement\n\
                return n\n\
            }\n\
        }\n\
        return read\n\
    }",
        |_| {},
    );
    assert!(
        boxes(&mf)[0].is_empty(),
        "a loop must not recreate a root-owned cell: {lua}"
    );
}

#[test]
fn initializer_after_a_join_can_seed_the_box() {
    let (mf, lua) = converted(
        "fn f(input: int, replacement: int, branch: bool) -> fn() -> int {\n\
        let n = if branch { input } else { replacement }\n\
        return () => {\n\
            let previous = n\n\
            n = replacement\n\
            return previous\n\
        }\n\
    }",
        |_| {},
    );
    assert_eq!(
        boxes(&mf)[0].len(),
        1,
        "the join dominates capture and use: {lua}"
    );
    execute(
        &lua,
        "
        local input, replacement = 37, 91
        local yes, no = f(input, replacement, true), f(input, replacement, false)
        assert(yes() == input)
        assert(no() == replacement)
        assert(yes() == replacement)
    ",
    );
}
