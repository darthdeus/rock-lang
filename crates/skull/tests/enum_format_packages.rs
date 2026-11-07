//! Package-qualified enum, value-record and handle text on the Lua backend. The structural
//! contract lives in `tests/strings/enum_format.rock`; this holder pins the
//! package and module segments a packaged program names.

use crate::common::{V1Fragment, emit_v1_package, run_luajit};

const MAIN: &str = r#"
enum MobKind { Skeleton, Zombie }

enum ScenarioMoveAdmission {
    Movement(movement::MovementStepAdmission),
    Finished,
}

enum DamageSource { Mob(actor_identity::MobId), Environment }

struct FormatRow { v: int }

fn format_paralyzed() -> status::StatusEffect<DamageSource> {
    return status::StatusEffect::Paralyzed { remaining: 1.5, duration: 2.0 }
}

fn mob_id() -> actor_identity::MobId {
    return actor_identity::MobId { scene_id: 11, arena_id: 23, slot: 4, generation: 7 }
}

fn main() {
    print("{MobKind::Skeleton}")
    print("{status::PoisonLevel::Rotting}")
    let ad = ScenarioMoveAdmission::Movement(movement::MovementStepAdmission::Blocked)
    print("{ad}")
    let source = DamageSource::Mob(mob_id())
    print("{source}")
    print("{mob_id()}")
    print("{format_paralyzed()}")
    let rows: Arena<FormatRow> = arena_new()
    let e = rows.insert(FormatRow { v: 1 })
    print("{e}")
}
"#;

const STATUS: &str = r#"
mod status;

pub enum PoisonLevel { Rotting }

pub enum StatusEffect<Source> {
    Paralyzed { remaining: float, duration: float },
    Poisoned { source: Source },
}
"#;

const ACTOR_IDENTITY: &str = r#"
mod actor_identity;

pub value MobId { scene_id: int, arena_id: int, slot: int, generation: int }
"#;

const MOVEMENT: &str = r#"
mod movement;

pub enum MovementStepAdmission { Accepted, Blocked }
"#;

fn package() -> skull::mir_emit::MirEmit {
    emit_v1_package(
        "pkgenum",
        &[
            V1Fragment {
                file: "main.rock",
                src: MAIN,
            },
            V1Fragment {
                file: "status.rock",
                src: STATUS,
            },
            V1Fragment {
                file: "actor_identity.rock",
                src: ACTOR_IDENTITY,
            },
            V1Fragment {
                file: "movement.rock",
                src: MOVEMENT,
            },
        ],
    )
}

fn lines(tag: &str) -> Vec<String> {
    let emitted = package();
    run_luajit(&format!("{}\nmain()\n", emitted.lua), tag)
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn packaged_payload_free_enum_prints_its_path() {
    let out = lines("enum_format_packages_free");
    assert_eq!(out[0], "pkgenum::MobKind::Skeleton");
    assert_eq!(out[1], "pkgenum::status::PoisonLevel::Rotting");
}

#[test]
fn packaged_nested_enum_prints_each_path() {
    let out = lines("enum_format_packages_nested");
    assert_eq!(
        out[2],
        "pkgenum::ScenarioMoveAdmission::Movement(pkgenum::movement::MovementStepAdmission::Blocked)"
    );
}

#[test]
fn packaged_value_record_payload_prints_its_path() {
    let out = lines("enum_format_packages_record");
    let id =
        "pkgenum::actor_identity::MobId { scene_id: 11, arena_id: 23, slot: 4, generation: 7 }";
    assert_eq!(out[3], format!("pkgenum::DamageSource::Mob({id})"));
    assert_eq!(out[4], id);
}

#[test]
fn packaged_generic_enum_prints_its_path() {
    let out = lines("enum_format_packages_generic");
    assert_eq!(
        out[5],
        "pkgenum::status::StatusEffect::Paralyzed { remaining: 1.5, duration: 2 }"
    );
}

#[test]
fn packaged_handle_names_its_target_path() {
    let out = lines("enum_format_packages_handle");
    let text = &out[6];
    assert!(
        text.starts_with("Entity<pkgenum::"),
        "handle target is package-qualified: {text}"
    );
}
