TEST: hover_field_ref_shows_the_referenced_field
TYPE: hover
---
struct Mob { health: int, attack_co: int? }
fn kill_co<T>(o: T, f: FieldRef<T, int?>) {}
fn r(mob: Mob) {
    kill_co(mob, .«cursor»attack_co)
}
---
OPERATION: hover
---
EXPECTED:
attack_co: int? (field of Mob)
