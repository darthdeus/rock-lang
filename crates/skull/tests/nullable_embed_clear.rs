use crate::common::{compile_and_run, emit_module, function_body, run_luajit};

const SRC: &str = r#"
nobox value Id { scene: int, arena: int, slot: int, generation: int }
enum Target { Player, Mob(Id), Named(string) }
nobox value Resolution { target: Target, active: bool }
nobox value Snapshot { lock: Resolution? }
struct Holder { resolution: Resolution? = null, snapshot: Snapshot }

@nomangle fn clear(h: Holder) { h.resolution = null }
@nomangle fn clear_nested(h: Holder) { h.snapshot.lock = null }
fn assign(h: Holder, r: Resolution?) {
    if let value = r { h.resolution = value } else { h.resolution = null }
}
fn payload(h: Holder) -> int {
    if let r = h.resolution {
        return match r.target {
            Target::Player => 0,
            Target::Mob(id) => id.scene + id.arena + id.slot + id.generation,
            Target::Named(name) => name.len(),
        }
    }
    return -1
}
inline(lua) fn cleared(h: Holder) -> bool = """
    for _, prefix in ipairs({'resolution', 'snapshot.lock'}) do
        for _, leaf in ipairs({'target.__tag', 'target._0', 'target._1',
                              'target._2', 'target._3', 'active'}) do
            if h[prefix .. '.' .. leaf] ~= nil then return false end
        end
        if h[prefix] then return false end
    end
    return true
"""
fn main() {
    let r = Resolution {
        target: Target::Mob(Id { scene: 1, arena: 2, slot: 3, generation: 4 }),
        active: true,
    }
    let h = Holder { resolution: r, snapshot: Snapshot { lock: r } }
    print("{payload(h)}")
    clear(h)
    clear_nested(h)
    clear(h)
    clear_nested(h)
    print("{payload(h)} {cleared(h)}")
    assign(h, Resolution { target: Target::Named("name"), active: false })
    print("{payload(h)}")
    assign(h, null)
    print("{payload(h)} {cleared(h)}")
}
"#;

#[test]
fn nullable_embed_clear_skips_absent_hash_slots_without_materializing() {
    let lua = emit_module(SRC);
    for name in ["clear", "clear_nested", "assign"] {
        let body = function_body(&lua, name);
        let prefix = if name == "clear_nested" {
            "snapshot.lock"
        } else {
            "resolution"
        };
        let read = format!(" = c0.{prefix}");
        let read_nested = format!(" = c0[\"{prefix}\"]");
        assert!(
            (body.contains(&read) || body.contains(&read_nested)) && body.contains("if "),
            "{name} must test stored presence before clearing missing hash slots:\n{body}"
        );
        assert!(
            body.contains(&format!("[\"{prefix}.target.__tag\"] = ")),
            "{body}"
        );
        for ctor in ["({", "_ct(", "_lct(", "_vt("] {
            assert!(
                !body.contains(ctor),
                "{name} materializes `{ctor}`:\n{body}"
            );
        }
    }
}

#[test]
fn nullable_embed_clear_releases_payload_and_can_be_repeated() {
    assert_eq!(compile_and_run(SRC), "10\n-1 true\n4\n-1 true");
}

#[test]
fn absent_nullable_embed_clear_does_not_insert_hash_keys() {
    let lua = emit_module(SRC);
    let driver = r#"
local writes = 0
local h = setmetatable({}, {__newindex = function(t, k, v)
    writes = writes + 1
    rawset(t, k, v)
end})
for _ = 1, 128 do
    clear(h)
    clear_nested(h)
end
assert(writes == 0, 'absent clears inserted ' .. writes .. ' hash keys')
io.write('ok')
"#;
    assert_eq!(run_luajit(&format!("{lua}\n{driver}")), "ok");
}

#[test]
fn nullable_embed_clear_normalizes_dynamic_absence_before_boxed_read() {
    assert_eq!(
        compile_and_run(
            r#"
struct Payload { n: int }
struct State { embed payload: Payload? }
fn absent() -> Payload? { return null }
fn clear_payload(s: State) { s.payload = null }
fn take(p: Payload?) -> int { if p == null { return -1 }; return p.n }
fn main() {
    let s = State { payload: Payload { n: 7 } }
    s.payload = absent()
    clear_payload(s)
    print("{take(s.payload)}")
}
"#
        ),
        "-1"
    );
}
