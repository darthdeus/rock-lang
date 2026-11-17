TEST: field_ref_arg_offers_matching_field
TYPE: completion
---
struct Mob { health: int, attack_co: int?, name: string }
fn poll_slot(f: FieldRef<Mob, int?>) {}
fn main() {
    poll_slot(.«cursor»)
}
---
EXPECTED:
- .attack_co: int?

===

TEST: field_ref_arg_v_int_offers_int_field
TYPE: completion
SOURCE: field_ref_arg_offers_matching_field
REPLACE: ["fn poll_slot(f: FieldRef<Mob, int?>) {}", "fn want_int(f: FieldRef<Mob, int>) {}"]
REPLACE: ["    poll_slot(.)", "    want_int(.)"]
POSITION: line=3, character=14
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
EXPECTED:
- .co: int?

===

TEST: field_ref_empty_arg_offers_fields
TYPE: completion
SOURCE: field_ref_arg_offers_matching_field
REPLACE: ["struct Mob { health: int, attack_co: int?, name: string }", "struct Mob { hp: int, co: int? }"]
REPLACE: ["    poll_slot(.)", "    poll_slot()"]
POSITION: line=3, character=14
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
EXPECTED:
- kill_co

===

TEST: field_ref_method_surfaces_with_multiple_fields
TYPE: completion
SOURCE: field_ref_method_surfaces_on_generic_receiver
REPLACE: ["struct Mob { hp: int, attack_co: int? }", "struct Mob { hp: int, attack_co: int?, cast_co: int? }"]
POSITION: line=3, character=8
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
EXPECTED:
- .co: int?

===

TEST: write_field_ref_arg_offers_matching_field
TYPE: completion
SOURCE: field_ref_arg_offers_matching_field
REPLACE: ["fn poll_slot(f: FieldRef<Mob, int?>) {}", "fn clear_slot(f: WriteFieldRef<Mob, int?>) {}"]
REPLACE: ["    poll_slot(.)", "    clear_slot(.)"]
POSITION: line=3, character=16
EXPECTED:
- .attack_co: int?

===

TEST: write_field_ref_generic_t_from_sibling_arg
TYPE: completion
SOURCE: field_ref_generic_t_from_sibling_arg
REPLACE: ["fn poll_slot<T>(e: Entity<T>, f: FieldRef<T, int?>) {}", "fn stop<T>(e: Entity<T>, f: WriteFieldRef<T, int?>) {}"]
REPLACE: ["    poll_slot(e, .)", "    stop(e, .)"]
POSITION: line=5, character=13
EXPECTED:
- .co: int?
