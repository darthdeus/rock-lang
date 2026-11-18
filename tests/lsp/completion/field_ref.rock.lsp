[[tests]]
name = "field_ref_arg_offers_matching_field"
kind = "completion"
position = [3, 15]
source = '''
struct Mob { health: int, attack_co: int?, name: string }
fn poll_slot(f: FieldRef<Mob, int?>) {}
fn main() {
    poll_slot(.)
}'''
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = [".attack_co: int?"] }]

[[tests]]
name = "field_ref_arg_v_int_offers_int_field"
kind = "completion"
position = [3, 14]
source = { from_test = "field_ref_arg_offers_matching_field", replace = [["fn poll_slot(f: FieldRef<Mob, int?>) {}", "fn want_int(f: FieldRef<Mob, int>) {}"], ["    poll_slot(.)", "    want_int(.)"]] }
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = [".health: int"] }]

[[tests]]
name = "field_ref_generic_t_from_sibling_arg"
kind = "completion"
position = [5, 18]
source = '''
struct Mob { hp: int, co: int? }
struct Entity<T> { i: int }
fn poll_slot<T>(e: Entity<T>, f: FieldRef<T, int?>) {}
fn main() {
    let e: Entity<Mob> = Entity { i: 0 }
    poll_slot(e, .)
}'''
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = [".co: int?"] }]

[[tests]]
name = "field_ref_empty_arg_offers_fields"
kind = "completion"
position = [3, 14]
source = { from_test = "field_ref_arg_offers_matching_field", replace = [["struct Mob { health: int, attack_co: int?, name: string }", "struct Mob { hp: int, co: int? }"], ["    poll_slot(.)", "    poll_slot()"]] }
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = [".co: int?"] }]

[[tests]]
name = "field_ref_method_surfaces_on_generic_receiver"
kind = "completion"
position = [3, 8]
source = '''
struct Mob { hp: int, attack_co: int? }
fn kill_co<T>(o: T, f: FieldRef<T, int?>) {}
fn r(mob: Mob) {
    mob.
}'''
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["kill_co"] }]

[[tests]]
name = "field_ref_method_surfaces_with_multiple_fields"
kind = "completion"
position = [3, 8]
source = { from_test = "field_ref_method_surfaces_on_generic_receiver", replace = [["struct Mob { hp: int, attack_co: int? }", "struct Mob { hp: int, attack_co: int?, cast_co: int? }"]] }
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["kill_co"] }]

[[tests]]
name = "field_ref_method_surfaces_on_entity_receiver"
kind = "completion"
position = [4, 6]
source = '''
struct Mob { hp: int, co: int? }
struct Entity<T> { i: int }
fn run_into<T>(e: Entity<T>, f: FieldRef<T, int?>, next: int?) {}
fn r(e: Entity<Mob>) {
    e.
}'''
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["run_into"] }]

[[tests]]
name = "field_ref_ufcs_generic_receiver_arg_offers_field"
kind = "completion"
position = [3, 14]
source = '''
struct Mob { hp: int, co: int? }
fn stop<T>(o: T, f: FieldRef<T, int?>) {}
fn r(mob: Mob) {
    mob.stop(.)
}'''
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = [".co: int?"] }]

[[tests]]
name = "field_ref_ufcs_entity_receiver_arg_offers_element_field"
kind = "completion"
position = [5, 16]
source = '''
struct Mob { hp: int, co: int? }
struct Entity<T> { i: int }
fn run_into<T>(o: T, f: FieldRef<T, int?>, next: int?) {}
fn run_into<T>(e: Entity<T>, f: FieldRef<T, int?>, next: int?) {}
fn r(e: Entity<Mob>) {
    e.run_into(.)
}'''
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = [".co: int?"] }]

[[tests]]
name = "write_field_ref_arg_offers_matching_field"
kind = "completion"
position = [3, 16]
source = { from_test = "field_ref_arg_offers_matching_field", replace = [["fn poll_slot(f: FieldRef<Mob, int?>) {}", "fn clear_slot(f: WriteFieldRef<Mob, int?>) {}"], ["    poll_slot(.)", "    clear_slot(.)"]] }
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = [".attack_co: int?"] }]

[[tests]]
name = "write_field_ref_generic_t_from_sibling_arg"
kind = "completion"
position = [5, 13]
source = { from_test = "field_ref_generic_t_from_sibling_arg", replace = [["fn poll_slot<T>(e: Entity<T>, f: FieldRef<T, int?>) {}", "fn stop<T>(e: Entity<T>, f: WriteFieldRef<T, int?>) {}"], ["    poll_slot(e, .)", "    stop(e, .)"]] }
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = [".co: int?"] }]
