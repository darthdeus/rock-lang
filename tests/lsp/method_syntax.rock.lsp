[[tests]]
name = "hover_dform_receiver_param"
kind = "hover"
position = [3, 11]
source = '''
struct Mob { hp: int }

fn Mob.take_damage(m, amount: int) -> int {
    return m.hp - amount
}'''
request = { transport = "cli", argv = ["hover", "$target"] }
checks = [{ op = "trimmed_equals", path = ["result", "hover", "contents"], value = "Type: struct Mob" }]

[[tests]]
name = "goto_definition_dform_call"
kind = "goto_definition"
position = [8, 15]
source = '''
struct Mob { hp: int }

fn Mob.take_damage(m, amount: int) -> int {
    return m.hp - amount
}

fn main() {
    let mob = Mob { hp: 10 }
    print(mob.take_damage(3))
}'''
request = { transport = "cli", argv = ["definition", "$target"] }
checks = [{ op = "equals", path = ["result", "definitions", 0, "range", "start", "line"], value = 2 }, { op = "equals", path = ["result", "definitions", 0, "range", "start", "character"], value = 7 }]
