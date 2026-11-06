TEST: type_def_local_binding_to_struct
TYPE: goto_type_definition
---
struct Mob {
    health: int
}

fn spawn() -> Mob {
    return Mob { health: 100 }
}

fn main() {
    let «cursor»mob = spawn()
}
---
OPERATION: goto_type_definition
---
EXPECTED:
file: main.rock
line: 0
character: 7
===

TEST: type_def_variable_use_site
TYPE: goto_type_definition
---
struct Mob {
    health: int
}

fn spawn() -> Mob {
    return Mob { health: 100 }
}

fn main() {
    let mob = spawn()
    let other = «cursor»mob
}
---
OPERATION: goto_type_definition
---
EXPECTED:
file: main.rock
line: 0
character: 7
===

TEST: type_def_field_access_type
TYPE: goto_type_definition
---
struct Inner { value: int }
struct Outer { inner: Inner }

fn main() {
    let o = Outer { inner: Inner { value: 1 } }
    let i = o.«cursor»inner
}
---
OPERATION: goto_type_definition
---
EXPECTED:
file: main.rock
line: 0
character: 7
===

TEST: type_def_nullable_peeled_to_struct
TYPE: goto_type_definition
---
struct Mob {
    health: int
}

fn find() -> Mob? {
    return null
}

fn main() {
    let m = «cursor»find()
}
---
OPERATION: goto_type_definition
---
EXPECTED:
file: main.rock
line: 0
character: 7
===

TEST: type_def_enum_value
TYPE: goto_type_definition
---
enum Color { Red, Green, Blue }

fn main() {
    let «cursor»c = Color::Red
}
---
OPERATION: goto_type_definition
---
EXPECTED:
file: main.rock
line: 0
character: 5
===

TEST: type_def_vec_builtin_jumps_to_stdlib
TYPE: goto_type_definition
---
fn main() {
    let «cursor»xs = [1, 2, 3]
}
---
OPERATION: goto_type_definition
---
EXPECTED:
file: unified.rock
===

TEST: type_def_primitive_returns_none
TYPE: goto_type_definition
---
fn main() {
    let «cursor»n = 42
}
---
OPERATION: goto_type_definition
---
EXPECTED:
none
===

TEST: type_def_coroutine_jumps_to_yield_type
TYPE: goto_type_definition
---
struct CoFrame { wait: bool }

co fn maker() -> Coroutine<CoFrame> {
    yield CoFrame { wait: true }
}

fn main() {
    let «cursor»c = maker()
}
---
OPERATION: goto_type_definition
---
EXPECTED:
file: main.rock
line: 0
character: 7
===

TEST: type_def_coroutine_of_primitive_returns_none
TYPE: goto_type_definition
---
co fn nums() -> Coroutine<int> {
    yield 1
}

fn main() {
    let «cursor»c = nums()
}
---
OPERATION: goto_type_definition
---
EXPECTED:
none
===
