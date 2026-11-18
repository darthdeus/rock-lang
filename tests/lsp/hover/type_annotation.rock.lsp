[[tests]]
name = "hover_param_type_shows_struct_def"
kind = "hover"
position = [4, 15]
source = '''
struct Mob {
    hp: int,
}

fn describe(m: Mob) {
}'''
request = { transport = "cli", argv = ["hover", "$target"] }
checks = [{ op = "trimmed_equals", path = ["result", "hover", "contents"], value = "struct Mob {\n  hp: int,\n}" }]
