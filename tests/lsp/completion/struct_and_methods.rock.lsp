[[tests]]
name = "complete_struct_literal_fields"
kind = "completion"
position = [3, 22]
source = """
struct FixVec2 { x: float, y: float }

fn run() {
    let v = FixVec2 {\u0020
}"""
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["x: float", "y: float"] }]

[[tests]]
name = "complete_member_includes_fields_and_ufcs_methods"
kind = "completion"
position = [6, 6]
source = """
struct FixVec2 { x: float, y: float }

fn dist(a: FixVec2) -> float { a.x }

fn run() {
    let v = FixVec2 { x: 1.0, y: 2.0 }
    v.
}"""
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["x: float", "y: float", "dist"] }]

[[tests]]
name = "complete_member_ufcs_method_defined_later_in_file"
kind = "completion"
position = [4, 6]
source = """
struct FixVec2 { x: float, y: float }

fn run() {
    let v = FixVec2 { x: 1.0, y: 2.0 }
    v.
}

fn dist(a: FixVec2) -> float { a.x }"""
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["x: float", "y: float", "dist"] }]
