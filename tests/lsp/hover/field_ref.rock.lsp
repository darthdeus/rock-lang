[[tests]]
name = "hover_field_ref_shows_the_referenced_field"
kind = "hover"
position = [3, 18]
source = '''
struct Mob { health: int, attack_co: int? }
fn kill_co<T>(o: T, f: FieldRef<T, int?>) {}
fn r(mob: Mob) {
    kill_co(mob, .attack_co)
}'''
request = { transport = "cli", argv = ["hover", "$target"] }
checks = [{ op = "trimmed_equals", path = ["result", "hover", "contents"], value = "attack_co: int? (field of Mob)" }]
