use crate::common::{activation_scope, allocation_scope, emit_mir, inline_off_guard};

#[test]
fn report_is_opt_in_non_invasive_and_renders_sites_and_routes() {
    let source = r#"
struct Carrier { value: int }
global RETAINED: Carrier? = null
global FRAME_VALUE: int = 0
@lifecycle(phase="update", owner="process") deco init tick {}
@lifecycle(phase="draw", owner="process") deco init paint {}
inline(lua) fn raw_add(x: int) -> int = "return x + 1"
inline(js) fn raw_add(x: int) -> int = "return x + 2"
fn consume(carrier: Carrier) -> int { return raw_add(carrier.value) }
fn direct() -> int { return consume(Carrier { value: 1 }) }
fn returned() -> Carrier { return Carrier { value: 2 } }
fn retain() { RETAINED = Carrier { value: 3 } }
fn alloc_leaf(i: int) -> [int] { return [i] }
fn left() -> int { return alloc_leaf(1).len() }
fn right() -> int { return alloc_leaf(2).len() }
fn update_root() -> int {
    let total = direct() + consume(returned())
    retain()
    for i in 0..2 { total += left() + right() }
    return total
}
@tick fn update_system() { FRAME_VALUE = update_root() }
@paint fn draw_system() { FRAME_VALUE += 1 }
"#;
    let ordinary = emit_mir(source);
    let empty = ordinary.allocation_report.render_text();
    assert!(empty.contains("0 bodies, 0 direct sites"));
    assert!(!ordinary.lua.contains("__rock_alloc_hit"));

    let reported = {
        let _report = allocation_scope(true);
        emit_mir(source)
    };
    let commented = {
        let _report = allocation_scope(true);
        emit_mir(&format!(
            "// Preserve allocation site identities\n\n{source}"
        ))
    };
    let manifest = |emit: &skull::mir_emit::MirEmit| {
        emit.allocation_report
            .bodies
            .iter()
            .flat_map(|(name, body)| body.direct_sites.iter().map(|site| (name.clone(), site.id)))
            .collect::<Vec<_>>()
    };
    assert_eq!(manifest(&reported), manifest(&commented));
    assert_eq!(ordinary.lua, reported.lua);
    let text = reported.allocation_report.render_text();
    assert!(text.contains("emitted site manifest:"));
    assert!(text.contains("\tstruct-construction\t"));
    for kind in ["projection", "opaque-call"] {
        assert!(
            text.lines().any(|line| (line.starts_with("pc-source\t")
                || line.starts_with("pc-line\t"))
                && line.contains(&format!("\t{kind}\t"))),
            "{kind}"
        );
    }
    let raw_lua_line = source
        .lines()
        .position(|line| line.starts_with("inline(lua) fn raw_add"))
        .unwrap()
        + 1;
    assert!(text.contains(&format!(
        "pc-source\traw_add_int\t0\tinline-lua\t<test>:{raw_lua_line}\n"
    )));
    assert!(!empty.contains("pc-source\t"));
    for use_kind in ["direct-call", "returned", "global-store"] {
        assert!(text.contains(&format!("\t{use_kind}\t1\t")), "{use_kind}");
    }
    assert!(text.contains("route frame.update:"));
    assert!(text.contains("route frame.draw:"));
    assert!(
        text.lines()
            .any(|line| { line.contains("vec-construction") && line.contains("loop_depth\t1") })
    );
    assert!(
        text.lines()
            .any(|line| { line.contains("vec-construction") && line.contains("callers\t") })
    );
}

#[test]
fn activation_instruments_manifest_sites_in_rock_and_coroutine_bodies() {
    let _inline = inline_off_guard();
    let _activation = activation_scope(true);
    let emit = emit_mir(
        r#"
enum Event { Text(string) }
value FfiRecord { n: int }
extern(ffi, "native") fn get_record() -> FfiRecord
global SINK: string = ""
fn render(items: [string], a: int) { items.push("value={a}") }
co fn worker(seed: int) -> Coroutine<Event> {
    defer { SINK = "cleanup={seed}" }
    on_killed(() => { SINK = "killed={seed}" })
    yield Event::Text("value={seed}")
}
fn make_worker() -> Coroutine<Event> { return worker(7) }
fn main() {
    let items: [string] = []
    render(items, 1)
    let task = make_worker()
}
"#,
    );
    let text = emit.allocation_report.render_text();
    for kind in ["coroutine-frame", "ffi-array", "ffi-return-value"] {
        let row = text
            .lines()
            .find(|line| line.contains(&format!("\t{kind}\t")))
            .expect("generated allocation");
        let id = row.split_whitespace().next().unwrap();
        assert!(
            emit.lua.contains(&format!("__rock_alloc_site[\"{id}\"]")),
            "{kind}: {id}"
        );
    }
    let rows: Vec<Vec<&str>> = text
        .lines()
        .filter(|line| line.contains("\tstring-concat\t"))
        .map(|line| line.trim().split('\t').collect())
        .collect();
    let render = rows
        .iter()
        .find(|row| row[3].contains("render"))
        .expect("render concat");
    let step = rows
        .iter()
        .find(|row| row[3].contains("worker") && row[3].ends_with("__step"))
        .expect("coroutine step concat");

    for row in [render, step] {
        let id = row[0];
        assert!(emit.lua.contains(&format!("__rock_alloc_site[\"{id}\"]")));
    }
    let function = format!("function {}", render[3]);
    let function_line = emit
        .lua
        .lines()
        .position(|line| line.starts_with(&function))
        .expect("render body")
        + 1;
    let hit = format!("__rock_alloc_site[\"{}\"]", render[0]);
    let emitted_line = emit
        .lua
        .lines()
        .position(|line| line.contains(&hit))
        .expect("render allocation")
        + 1;
    assert_eq!(
        render[4].parse::<u32>().expect("manifest line"),
        (emitted_line - function_line + 1) as u32
    );
    for runtime in [
        "function __rock_alloc_hit(id, value)",
        "function __rock_allocation_activation_begin()",
        "function __rock_allocation_activation_frame_begin()",
        "function __rock_allocation_activation_frame_end()",
    ] {
        assert!(emit.lua.contains(runtime), "{runtime}");
    }
    assert!(text.contains("route coroutine.resume:"));
}

fn wide_value_fixture(fields: usize, pads: usize) -> String {
    let pad_params = (0..pads)
        .map(|i| format!(", p{i}: int"))
        .collect::<String>();
    let pad_sum = (0..pads).map(|i| format!(" + p{i}")).collect::<String>();
    let decl = (0..fields)
        .map(|i| format!("f{i}: int"))
        .collect::<Vec<_>>()
        .join(", ");
    let init = (0..fields)
        .map(|i| format!("f{i}: seed + {i}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"
nobox value Wide {{ {decl} }}
fn make_wide(seed: int) -> Wide {{ return Wide {{ {init} }} }}
fn step(value: Wide, tag: [int]) -> Wide {{
    let w = value
    return make_wide(w.f0 + tag.len())
}}
fn run(seed: int{pad_params}) -> int {{
    let a = make_wide(seed)
    let b = step(a, [seed])
    let c = step(b, [seed, 1])
    return a.f1 + b.f2 + c.f3{pad_sum}
}}
"#
    )
}

fn max_stack_slots(lua: &str, tag: &str) -> Result<u32, String> {
    use std::process::Command;
    let prefix = format!("skull_slots_{}_{tag}", std::process::id());
    let path = std::env::temp_dir().join(format!("{prefix}.lua"));
    let probe_path = std::env::temp_dir().join(format!("{prefix}_probe.lua"));
    std::fs::write(&path, lua).expect("write chunk");
    std::fs::write(
        &probe_path,
        r#"
local f, err = loadfile(arg[1])
if not f then io.write("LOAD ", err) os.exit(0) end
local util, best = require("jit.util"), 0
local function walk(fn)
  best = math.max(best, util.funcinfo(fn).stackslots)
  local i = -1
  while true do
    local child = util.funck(fn, i)
    if child == nil then break end
    if type(child) == "proto" then walk(child) end
    i = i - 1
  end
end
walk(f)
io.write("SLOTS ", best)
"#,
    )
    .expect("write probe");
    let output = Command::new("luajit")
        .arg(&probe_path)
        .arg(&path)
        .output()
        .expect("spawn luajit");
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(probe_path);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());
    stdout
        .strip_prefix("SLOTS ")
        .map(|slots| slots.parse().expect("slot count"))
        .ok_or_else(|| stdout.to_string())
}

#[test]
fn activation_does_not_add_register_slots_to_a_wide_luajit_frame() {
    let _inline = inline_off_guard();
    let source = wide_value_fixture(60, 119);
    let ordinary = emit_mir(&source);
    let instrumented = {
        let _activation = activation_scope(true);
        emit_mir(&source)
    };
    let slots = max_stack_slots(&ordinary.lua, "ordinary").expect("wide frame loads");
    assert!((245..250).contains(&slots), "{slots}");
    assert_eq!(max_stack_slots(&instrumented.lua, "activated"), Ok(slots));
}
