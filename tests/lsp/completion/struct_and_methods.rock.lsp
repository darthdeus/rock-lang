TEST: complete_struct_literal_fields
TYPE: completion
---
struct FixVec2 { x: float, y: float }

fn run() {
    let v = FixVec2 { «cursor»
}
---
OPERATION: completion
---
EXPECTED:
- x: float
- y: float
===

TEST: complete_member_includes_fields_and_ufcs_methods
TYPE: completion
---
struct FixVec2 { x: float, y: float }

fn dist(a: FixVec2) -> float { a.x }

fn run() {
    let v = FixVec2 { x: 1.0, y: 2.0 }
    v.«cursor»
}
---
OPERATION: completion
---
EXPECTED:
- x: float
- y: float
- dist
===

TEST: complete_member_ufcs_method_defined_later_in_file
TYPE: completion
---
struct FixVec2 { x: float, y: float }

fn run() {
    let v = FixVec2 { x: 1.0, y: 2.0 }
    v.«cursor»
}

fn dist(a: FixVec2) -> float { a.x }
---
OPERATION: completion
---
EXPECTED:
- x: float
- y: float
- dist
===
