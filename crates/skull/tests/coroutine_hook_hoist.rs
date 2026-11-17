use crate::common::{emit_module, mangled_name, run_luajit};

const ON_KILLED: &str = r#"
enum Ev { Tick(int) }
global LOG: int = 0
fn note(x: int) { LOG = x }

co fn worker(seed: int) -> Coroutine<Ev> {
    let base = seed + 100
    on_killed(() => note(base))
    yield Ev::Tick(base)
}

fn main() {
    let c = worker(0)
    let _ = c.resume()
    c.kill()
}
"#;

#[test]
fn on_killed_hook_hoisted_to_shared_toplevel_fn() {
    let lua = emit_module(ON_KILLED);
    let hook = format!("{}__on_killed", mangled_name(&lua, "worker"));
    assert!(lua.contains(&format!("function {hook}(")));
    assert!(lua.contains("__on_killed = __rock_code_slot_ref("));
    assert!(!lua.contains("__on_killed = function"));
}

#[test]
fn hook_fn_shared_across_constructions_and_still_fires() {
    let lua = emit_module(ON_KILLED);
    let w = mangled_name(&lua, "worker");
    let driver = format!(
        "\n\
        local h1 = {w}(5)\n\
        local h2 = {w}(6)\n\
        {w}__resume(h1, nil)\n\
        local fresh = {{}}\n\
        for k, v in pairs(h1) do\n\
        \x20 if type(v) == \"function\" and not rawequal(v, h2[k]) then fresh[#fresh + 1] = k end\n\
        end\n\
        table.sort(fresh)\n\
        print(#fresh == 0 and \"shared\" or table.concat(fresh, \",\"))\n\
        __rock_coroutine_kill(h1)\n\
        local log_key = __rock_global_entry(\"LOG\").slots[1]\n\
        print(__rock_globals[log_key])\n"
    );
    let out = run_luajit(&format!("{lua}{driver}"));
    assert_eq!(out, "shared\n105");
}
