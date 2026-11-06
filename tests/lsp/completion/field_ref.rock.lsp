TEST: field_ref_arg_offers_matching_field
TYPE: completion
---
struct Mob { health: int, attack_co: int?, name: string }
fn poll_slot(f: FieldRef<Mob, int?>) {}
fn main() {
    poll_slot(.«cursor»)
}
---
OPERATION: completion
---
EXPECTED:
- .attack_co: int?

===

TEST: field_ref_arg_v_int_offers_int_field
TYPE: completion
---
struct Mob { health: int, attack_co: int?, name: string }
fn want_int(f: FieldRef<Mob, int>) {}
fn main() {
    want_int(.«cursor»)
}
---
OPERATION: completion
---
EXPECTED:
- .health: int

===

TEST: field_ref_generic_t_from_sibling_arg
TYPE: completion
---
struct Mob { hp: int, co: int? }
struct Entity<T> { i: int }
fn poll_slot<T>(e: Entity<T>, f: FieldRef<T, int?>) {}
fn main() {
    let e: Entity<Mob> = Entity { i: 0 }
    poll_slot(e, .«cursor»)
}
---
OPERATION: completion
---
EXPECTED:
- .co: int?

===

TEST: field_ref_empty_arg_offers_fields
TYPE: completion
---
struct Mob { hp: int, co: int? }
fn poll_slot(f: FieldRef<Mob, int?>) {}
fn main() {
    poll_slot(«cursor»)
}
---
OPERATION: completion
---
EXPECTED:
- .co: int?

===

TEST: field_ref_method_surfaces_on_generic_receiver
TYPE: completion
---
struct Mob { hp: int, attack_co: int? }
fn kill_co<T>(o: T, f: FieldRef<T, int?>) {}
fn r(mob: Mob) {
    mob.«cursor»
}
---
OPERATION: completion
---
EXPECTED:
- kill_co

===

TEST: field_ref_method_surfaces_with_multiple_fields
TYPE: completion
---
struct Mob { hp: int, attack_co: int?, cast_co: int? }
fn kill_co<T>(o: T, f: FieldRef<T, int?>) {}
fn r(mob: Mob) {
    mob.«cursor»
}
---
OPERATION: completion
---
EXPECTED:
- kill_co

===

TEST: field_ref_method_surfaces_on_entity_receiver
TYPE: completion
---
struct Mob { hp: int, co: int? }
struct Entity<T> { i: int }
fn run_into<T>(e: Entity<T>, f: FieldRef<T, int?>, next: int?) {}
fn r(e: Entity<Mob>) {
    e.«cursor»
}
---
OPERATION: completion
---
EXPECTED:
- run_into

===

TEST: field_ref_ufcs_generic_receiver_arg_offers_field
TYPE: completion
---
struct Mob { hp: int, co: int? }
fn stop<T>(o: T, f: FieldRef<T, int?>) {}
fn r(mob: Mob) {
    mob.stop(.«cursor»)
}
---
OPERATION: completion
---
EXPECTED:
- .co: int?

===

TEST: field_ref_ufcs_entity_receiver_arg_offers_element_field
TYPE: completion
---
struct Mob { hp: int, co: int? }
struct Entity<T> { i: int }
fn run_into<T>(o: T, f: FieldRef<T, int?>, next: int?) {}
fn run_into<T>(e: Entity<T>, f: FieldRef<T, int?>, next: int?) {}
fn r(e: Entity<Mob>) {
    e.run_into(.«cursor»)
}
---
OPERATION: completion
---
EXPECTED:
- .co: int?

===

TEST: write_field_ref_arg_offers_matching_field
TYPE: completion
---
struct Mob { health: int, attack_co: int?, name: string }
fn clear_slot(f: WriteFieldRef<Mob, int?>) {}
fn main() {
    clear_slot(.«cursor»)
}
---
OPERATION: completion
---
EXPECTED:
- .attack_co: int?

===

TEST: write_field_ref_generic_t_from_sibling_arg
TYPE: completion
---
struct Mob { hp: int, co: int? }
struct Entity<T> { i: int }
fn stop<T>(e: Entity<T>, f: WriteFieldRef<T, int?>) {}
fn main() {
    let e: Entity<Mob> = Entity { i: 0 }
    stop(e, .«cursor»)
}
---
OPERATION: completion
---
EXPECTED:
- .co: int?
