use crate::common::{V1Fragment, emit_v1_package, run_main};

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

#[test]
fn packaged_types_print_their_paths() {
    let emitted = package();
    let output = run_main(&emitted.lua);
    let out: Vec<_> = output.lines().collect();
    let id =
        "pkgenum::actor_identity::MobId { scene_id: 11, arena_id: 23, slot: 4, generation: 7 }";
    for (line, expected) in [
        "pkgenum::MobKind::Skeleton".to_string(),
        "pkgenum::status::PoisonLevel::Rotting".to_string(),
        "pkgenum::ScenarioMoveAdmission::Movement(pkgenum::movement::MovementStepAdmission::Blocked)".to_string(),
        format!("pkgenum::DamageSource::Mob({id})"),
        id.to_string(),
        "pkgenum::status::StatusEffect::Paralyzed { remaining: 1.5, duration: 2 }".to_string(),
    ].iter().enumerate() {
        assert_eq!(out[line], expected);
    }
    let text = out[6];
    assert!(text.starts_with("Entity<pkgenum::"));
}
