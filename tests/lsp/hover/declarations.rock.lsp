[[tests]]
name = "hover_struct_field_declaration_name"
kind = "hover"
position = [1, 4]
source = '''
struct Mob {
    pos: FixVec2,
    dir: int,
}

struct FixVec2 { x: float, y: float }'''
request = { transport = "cli", argv = ["hover", "$target"] }
checks = [{ op = "trimmed_equals", path = ["result", "hover", "contents"], value = "pos: FixVec2 (field of Mob)" }]

[[tests]]
name = "hover_enum_declaration_name_shows_full_body"
kind = "hover"
position = [0, 10]
source = '''
enum SpellState {
    Idle,
    Casting(SpellKind, float, float),
    Loaded(SpellKind),
}

enum SpellKind { Fire, Ice }'''
request = { transport = "cli", argv = ["hover", "$target"] }
checks = [{ op = "trimmed_equals", path = ["result", "hover", "contents"], value = "```rock\nenum SpellState {\n  Idle,\n  Casting(SpellKind, float, float),\n  Loaded(SpellKind),\n}\n```" }]

[[tests]]
name = "hover_enum_variant_declaration_name_shows_the_variant"
kind = "hover"
position = [2, 4]
source = { from_test = "hover_enum_declaration_name_shows_full_body" }
request = { transport = "cli", argv = ["hover", "$target"] }
checks = [{ op = "trimmed_equals", path = ["result", "hover", "contents"], value = "```rock\nSpellState::Casting(SpellKind, float, float)\n```" }]
