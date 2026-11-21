use crate::common::{emit_without_stdlib, run_luajit};

fn global_slot(lua: &str, name: &str) -> u32 {
    crate::common::global_slots(lua, &format!("global:<t>::{name}"))[0]
}
