[[tests]]
name = "hover_struct_name_in_literal"
kind = "hover"
position = [6, 12]
source = '''
struct Player {
    name: string,
    health: int,
}

fn main() {
    let p = Player { name: "foo", health: 100 }
}'''
request = { transport = "cli", argv = ["hover", "$target"] }
checks = [{ op = "trimmed_equals", path = ["result", "hover", "contents"], value = "struct Player {\n  name: string,\n  health: int,\n}" }]

[[tests]]
name = "hover_field_in_struct_literal"
kind = "hover"
position = [6, 21]
source = { from_test = "hover_struct_name_in_literal" }
request = { transport = "cli", argv = ["hover", "$target"] }
checks = [{ op = "trimmed_equals", path = ["result", "hover", "contents"], value = "name: string (field of Player)" }]

[[tests]]
name = "hover_second_field_in_struct_literal"
kind = "hover"
position = [6, 34]
source = { from_test = "hover_field_in_struct_literal" }
request = { transport = "cli", argv = ["hover", "$target"] }
checks = [{ op = "trimmed_equals", path = ["result", "hover", "contents"], value = "health: int (field of Player)" }]
