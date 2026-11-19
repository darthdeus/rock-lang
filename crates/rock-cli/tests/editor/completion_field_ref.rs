use super::*;

const FIELD_REF_ARG_OFFERS_MATCHING_FIELD: &str = r#"struct Mob { health: int, attack_co: int?, name: string }
fn poll_slot(f: FieldRef<Mob, int?>) {}
fn main() {
    poll_slot(.)
}"#;

#[test]
fn field_ref_arg_offers_matching_field() {
    let source = FIELD_REF_ARG_OFFERS_MATCHING_FIELD;
    let project = Project::single("field_ref_arg_offers_matching_field", source);
    let reply = project.cli("completion", Some((3, 15)), &[]);
    completion_labels(&reply, &[".attack_co: int?"]);
}

#[test]
fn field_ref_arg_v_int_offers_int_field() {
    let source = &FIELD_REF_ARG_OFFERS_MATCHING_FIELD
        .replace(
            "fn poll_slot(f: FieldRef<Mob, int?>) {}",
            "fn want_int(f: FieldRef<Mob, int>) {}",
        )
        .replace("    poll_slot(.)", "    want_int(.)");
    let project = Project::single("field_ref_arg_v_int_offers_int_field", source);
    let reply = project.cli("completion", Some((3, 14)), &[]);
    completion_labels(&reply, &[".health: int"]);
}

const FIELD_REF_GENERIC_T_FROM_SIBLING_ARG: &str = r#"struct Mob { hp: int, co: int? }
struct Entity<T> { i: int }
fn poll_slot<T>(e: Entity<T>, f: FieldRef<T, int?>) {}
fn main() {
    let e: Entity<Mob> = Entity { i: 0 }
    poll_slot(e, .)
}"#;

#[test]
fn field_ref_generic_t_from_sibling_arg() {
    let source = FIELD_REF_GENERIC_T_FROM_SIBLING_ARG;
    let project = Project::single("field_ref_generic_t_from_sibling_arg", source);
    let reply = project.cli("completion", Some((5, 18)), &[]);
    completion_labels(&reply, &[".co: int?"]);
}

#[test]
fn field_ref_empty_arg_offers_fields() {
    let source = &FIELD_REF_ARG_OFFERS_MATCHING_FIELD
        .replace(
            "struct Mob { health: int, attack_co: int?, name: string }",
            "struct Mob { hp: int, co: int? }",
        )
        .replace("    poll_slot(.)", "    poll_slot()");
    let project = Project::single("field_ref_empty_arg_offers_fields", source);
    let reply = project.cli("completion", Some((3, 14)), &[]);
    completion_labels(&reply, &[".co: int?"]);
}

const FIELD_REF_METHOD_SURFACES_ON_GENERIC_RECEIVER: &str = r#"struct Mob { hp: int, attack_co: int? }
fn kill_co<T>(o: T, f: FieldRef<T, int?>) {}
fn r(mob: Mob) {
    mob.
}"#;

#[test]
fn field_ref_method_surfaces_on_generic_receiver() {
    let source = FIELD_REF_METHOD_SURFACES_ON_GENERIC_RECEIVER;
    let project = Project::single("field_ref_method_surfaces_on_generic_receiver", source);
    let reply = project.cli("completion", Some((3, 8)), &[]);
    completion_labels(&reply, &["kill_co"]);
}

#[test]
fn field_ref_method_surfaces_with_multiple_fields() {
    let source = &FIELD_REF_METHOD_SURFACES_ON_GENERIC_RECEIVER.replace(
        "struct Mob { hp: int, attack_co: int? }",
        "struct Mob { hp: int, attack_co: int?, cast_co: int? }",
    );
    let project = Project::single("field_ref_method_surfaces_with_multiple_fields", source);
    let reply = project.cli("completion", Some((3, 8)), &[]);
    completion_labels(&reply, &["kill_co"]);
}

#[test]
fn field_ref_method_surfaces_on_entity_receiver() {
    let source = r#"struct Mob { hp: int, co: int? }
struct Entity<T> { i: int }
fn run_into<T>(e: Entity<T>, f: FieldRef<T, int?>, next: int?) {}
fn r(e: Entity<Mob>) {
    e.
}"#;
    let project = Project::single("field_ref_method_surfaces_on_entity_receiver", source);
    let reply = project.cli("completion", Some((4, 6)), &[]);
    completion_labels(&reply, &["run_into"]);
}

#[test]
fn field_ref_ufcs_generic_receiver_arg_offers_field() {
    let source = r#"struct Mob { hp: int, co: int? }
fn stop<T>(o: T, f: FieldRef<T, int?>) {}
fn r(mob: Mob) {
    mob.stop(.)
}"#;
    let project = Project::single("field_ref_ufcs_generic_receiver_arg_offers_field", source);
    let reply = project.cli("completion", Some((3, 14)), &[]);
    completion_labels(&reply, &[".co: int?"]);
}

#[test]
fn field_ref_ufcs_entity_receiver_arg_offers_element_field() {
    let source = r#"struct Mob { hp: int, co: int? }
struct Entity<T> { i: int }
fn run_into<T>(o: T, f: FieldRef<T, int?>, next: int?) {}
fn run_into<T>(e: Entity<T>, f: FieldRef<T, int?>, next: int?) {}
fn r(e: Entity<Mob>) {
    e.run_into(.)
}"#;
    let project = Project::single(
        "field_ref_ufcs_entity_receiver_arg_offers_element_field",
        source,
    );
    let reply = project.cli("completion", Some((5, 16)), &[]);
    completion_labels(&reply, &[".co: int?"]);
}

#[test]
fn write_field_ref_arg_offers_matching_field() {
    let source = &FIELD_REF_ARG_OFFERS_MATCHING_FIELD
        .replace(
            "fn poll_slot(f: FieldRef<Mob, int?>) {}",
            "fn clear_slot(f: WriteFieldRef<Mob, int?>) {}",
        )
        .replace("    poll_slot(.)", "    clear_slot(.)");
    let project = Project::single("write_field_ref_arg_offers_matching_field", source);
    let reply = project.cli("completion", Some((3, 16)), &[]);
    completion_labels(&reply, &[".attack_co: int?"]);
}

#[test]
fn write_field_ref_generic_t_from_sibling_arg() {
    let source = &FIELD_REF_GENERIC_T_FROM_SIBLING_ARG
        .replace(
            "fn poll_slot<T>(e: Entity<T>, f: FieldRef<T, int?>) {}",
            "fn stop<T>(e: Entity<T>, f: WriteFieldRef<T, int?>) {}",
        )
        .replace("    poll_slot(e, .)", "    stop(e, .)");
    let project = Project::single("write_field_ref_generic_t_from_sibling_arg", source);
    let reply = project.cli("completion", Some((5, 13)), &[]);
    completion_labels(&reply, &[".co: int?"]);
}
