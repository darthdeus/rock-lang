use crate::common::{emit_module, function_body, run_luajit, run_main};

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
