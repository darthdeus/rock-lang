TEST: rename_param_from_caller_named_label
TYPE: rename
---
fn spawn_it(count: int, size: float) {
    print(count)
    print(size)
}

fn spawn_it(count: string) {
    print(count)
}

fn main() {
    spawn_it(«cursor»count: 3, size: 1.0)
    spawn_it(count: "a")
}
---
NEW_NAME: amount
EXPECTED:
fn spawn_it(amount: int, size: float) {
    print(amount)
    print(size)
}

fn spawn_it(count: string) {
    print(count)
}

fn main() {
    spawn_it(amount: 3, size: 1.0)
    spawn_it(count: "a")
}
===

TEST: rename_param_from_caller_named_label_other_overload
TYPE: rename
SOURCE: rename_param_from_caller_named_label
POSITION: line=11, character=13
NEW_NAME: label
EXPECTED:
fn spawn_it(count: int, size: float) {
    print(count)
    print(size)
}

fn spawn_it(label: string) {
    print(label)
}

fn main() {
    spawn_it(count: 3, size: 1.0)
    spawn_it(label: "a")
}
===

TEST: rename_macro_name_from_call_refused
TYPE: rename
---
macro double(x: expr) -> int { x + x }

fn main() {
    print(«cursor»double(5))
}
---
NEW_NAME: twice
EXPECTED:
REFUSED: cannot rename a macro
===

TEST: rename_macro_name_from_decl_refused
TYPE: rename
SOURCE: rename_macro_name_from_call_refused
POSITION: line=0, character=6
NEW_NAME: twice
EXPECTED:
REFUSED: cannot rename a macro
===

TEST: rename_local_used_through_caller_scope_macro_refused
TYPE: rename
---
macro bump() { ^count = ^count + 1 }

fn main() {
    let «cursor»count = 0
    bump()
    print(count)
}
---
NEW_NAME: total
EXPECTED:
REFUSED: .rock:5 that rename cannot rewrite
===

TEST: rename_global_used_through_macro_global_refused
TYPE: rename
---
global «cursor»SCORE: int = 0

macro bump() { _G.SCORE = _G.SCORE + 1 }

fn main() {
    bump()
    print(SCORE)
}
---
NEW_NAME: POINTS
EXPECTED:
REFUSED: .rock:6 that rename cannot rewrite
===

TEST: rename_macro_param
TYPE: rename
SOURCE: rename_macro_name_from_call_refused
POSITION: line=0, character=13
NEW_NAME: v
EXPECTED:
REPLACE: ["macro double(x: expr) -> int { x + x }", "macro double(v: expr) -> int { v + v }"]
===

TEST: rename_local_passed_as_macro_arg
TYPE: rename
---
macro double(x: expr) -> int { x + x }

fn main() {
    let «cursor»n = 5
    print(double(n))
}
---
NEW_NAME: m
EXPECTED:
REPLACE: ["    let n = 5", "    let m = 5"]
REPLACE: ["    print(double(n))", "    print(double(m))"]
===

TEST: rename_fn_called_inside_macro_body
TYPE: rename
---
fn «cursor»helper(x: int) -> int {
    return x + 1
}

macro twice(x: expr) -> int { helper(helper(x)) }

fn main() {
    print(twice(1))
    print(helper(2))
}
---
NEW_NAME: inc
EXPECTED:
REPLACE: ["helper", "inc"]
===

TEST: rename_inline_lua_param_refused
TYPE: rename
SOURCE: rename_macro_name_from_call_refused
REPLACE: ["macro double(x: expr) -> int { x + x }", "inline(lua) fn probe(x: int) -> int = \"return x + 1\""]
REPLACE: ["    print(double(5))", "    print(probe(1))"]
POSITION: line=0, character=21
NEW_NAME: y
EXPECTED:
REFUSED: cannot rename `x`: the opaque inline(lua) body of `probe` (
===

TEST: rename_inline_js_param_refused
TYPE: rename
SOURCE: rename_inline_lua_param_refused
REPLACE: ["inline(lua) fn probe(x: int) -> int = \"return x + 1\"", "inline(js) fn probe(x: int) -> int = \"return x + 1;\""]
POSITION: line=0, character=20
NEW_NAME: y
EXPECTED:
REFUSED: the opaque inline(js) body of `probe`
===

TEST: rename_nomangle_fn_called_from_inline_body_refused
TYPE: rename
---
@nomangle
fn «cursor»helper(x: int) -> int {
    return x * 2
}

inline(lua) fn call_helper(x: int) -> int = "return helper(x)"

fn main() {
    print(call_helper(1))
}
---
NEW_NAME: helper2
EXPECTED:
REFUSED: cannot rename `helper`: `@nomangle` keeps its emitted name
===

TEST: rename_nomangle_fn_from_call_site_refused
TYPE: rename
---
@nomangle
fn helper(x: int) -> int {
    return x * 2
}

fn main() {
    print(«cursor»helper(1))
}
---
NEW_NAME: helper2
EXPECTED:
REFUSED: `@nomangle` keeps its emitted name, which code outside Rock calls
===

TEST: rename_field_read_by_inline_body_refused
TYPE: rename
---
struct Mob {
    «cursor»hp: int,
}

inline(lua) fn peek(m: Mob) -> int = "return m.hp"

fn main() {
    let m = Mob { hp: 4 }
    print(peek(m))
}
---
NEW_NAME: health
EXPECTED:
REFUSED: the opaque inline(lua) body of `peek`
===

TEST: rename_global_read_by_inline_body_refused
TYPE: rename
---
global «cursor»SCORE: int = 0

inline(lua) fn peek() -> int = "return SCORE"

fn main() {
    print(peek())
}
---
NEW_NAME: POINTS
EXPECTED:
REFUSED: the opaque inline(lua) body of `peek`
===

TEST: rename_struct_named_by_inline_type_tag_refused
TYPE: rename
---
struct «cursor»Mob {
    hp: int,
}

inline(lua) fn is_mob(v: any) -> bool = "return v.__ty == 'Mob'"

fn main() {
    print(is_mob(Mob { hp: 1 }))
}
---
NEW_NAME: Beast
EXPECTED:
REFUSED: the opaque inline(lua) body of `is_mob`
===

TEST: rename_inline_fn_name_is_fine
TYPE: rename
SOURCE: rename_inline_lua_param_refused
POSITION: line=0, character=15
NEW_NAME: probe2
EXPECTED:
REPLACE: ["probe", "probe2"]
===

TEST: rename_field_not_named_by_any_inline_body
TYPE: rename
---
struct Mob {
    «cursor»hp: int,
    mana: int,
}

inline(lua) fn peek(m: Mob) -> int = "return m.mana"

fn main() {
    let m = Mob { hp: 4, mana: 2 }
    print(m.hp + peek(m))
}
---
NEW_NAME: health
EXPECTED:
REPLACE: ["hp", "health"]
===

TEST: rename_param_sharing_a_word_with_another_inline_body
TYPE: rename
SOURCE: rename_inline_fn_name_is_fine
REPLACE: ["fn main() {", "fn twice(x: int) -> int {"]
REPLACE: ["    print(probe(1))", "    return probe(x) * 2"]
POSITION: line=2, character=9
NEW_NAME: n
EXPECTED:
REPLACE: ["fn twice(x: int) -> int {", "fn twice(n: int) -> int {"]
REPLACE: ["    return probe(x) * 2", "    return probe(n) * 2"]
===

TEST: rename_fn_named_in_a_string_literal
TYPE: rename
---
fn «cursor»helper(x: int) -> int {
    return x * 2
}

fn main() {
    print("helper")
    print(helper(1))
}
---
NEW_NAME: helper2
EXPECTED:
REPLACE: ["fn helper(x: int) -> int {", "fn helper2(x: int) -> int {"]
REPLACE: ["    print(helper(1))", "    print(helper2(1))"]
===

TEST: rename_global_read_by_inline_body_in_another_file_refused
TYPE: rename
FILES:
---
FILE: state.rock
---
global «start»SCORE«end»: int = 0
---
FILE: main.rock
---
inline(lua) fn peek() -> int = "return SCORE"

fn main() {
    SCORE = 2
    print(peek())
}
---
FILE: stick.toml
---
[package]
name = "test"
---
OPERATION: rename
NEW_NAME: POINTS
---
EXPECTED:
REFUSED: the opaque inline(lua) body of `peek` (main.rock:1)
===

TEST: rename_global_across_files_without_inline_reader
TYPE: rename
FILES:
---
FILE: state.rock
---
global «start»SCORE«end»: int = 0
---
FILE: main.rock
---
inline(lua) fn peek() -> int = "return 1"

fn main() {
    SCORE = 2
    print(peek())
}
---
FILE: stick.toml
---
[package]
name = "test"
---
OPERATION: rename
NEW_NAME: POINTS
---
EXPECTED:
FILE: state.rock
---
global POINTS: int = 0
---
FILE: main.rock
---
inline(lua) fn peek() -> int = "return 1"

fn main() {
    POINTS = 2
    print(peek())
}
---
FILE: stick.toml
---
[package]
name = "test"
===

TEST: rename_shadowing_inner_local
TYPE: rename
---
fn main() {
    let x = 1
    if true {
        let «cursor»x = 2
        print(x)
    }
    print(x)
}
---
NEW_NAME: y
EXPECTED:
REPLACE: ["        let x = 2", "        let y = 2"]
REPLACE: ["        print(x)", "        print(y)"]
===

TEST: rename_shadowed_local_in_same_block
TYPE: rename
---
fn main() {
    let «cursor»x = 1
    let x = x + 1
    print(x)
}
---
NEW_NAME: y
EXPECTED:
REPLACE: ["    let x = 1", "    let y = 1"]
REPLACE: ["    let x = x + 1", "    let x = y + 1"]
===

TEST: rename_fn_from_ufcs_call
TYPE: rename
---
struct Circle {
    radius: float,
}

fn area(c: Circle) -> float {
    return c.radius * c.radius
}

fn main() {
    let c = Circle { radius: 1.0 }
    print(c.«cursor»area())
    print(area(c))
}
---
NEW_NAME: size
EXPECTED:
REPLACE: ["area", "size"]
===

TEST: rename_receiver_form_fn
TYPE: rename
---
struct Point {
    x: int,
}

fn Point.«cursor»shift(p, dx: int) -> int {
    return p.x + dx
}

fn main() {
    let p = Point { x: 1 }
    print(p.shift(1))
    print(shift(p, 2))
}
---
NEW_NAME: nudge
EXPECTED:
REPLACE: ["shift", "nudge"]
===

TEST: rename_field_leaves_same_named_field_of_other_struct
TYPE: rename
---
struct A {
    «cursor»hp: int,
}

struct B {
    hp: int,
}

fn main() {
    let a = A { hp: 1 }
    let b = B { hp: 2 }
    print(a.hp + b.hp)
}
---
NEW_NAME: health
EXPECTED:
struct A {
    health: int,
}

struct B {
    hp: int,
}

fn main() {
    let a = A { health: 1 }
    let b = B { hp: 2 }
    print(a.health + b.hp)
}
===

TEST: rename_local_inside_string_interpolation
TYPE: rename
---
fn main() {
    let «cursor»n = 5
    print("n = {n}")
}
---
NEW_NAME: count
EXPECTED:
REPLACE: ["    let n = 5", "    let count = 5"]
REPLACE: ["    print(\"n = {n}\")", "    print(\"n = {count}\")"]
===

TEST: rename_lambda_param
TYPE: rename
---
fn main() {
    let f = («cursor»a: int) => a + 1
    print(f(1))
}
---
NEW_NAME: b
EXPECTED:
REPLACE: ["    let f = (a: int) => a + 1", "    let f = (b: int) => b + 1"]
===

TEST: rename_struct_generic_param_reaches_its_uses
TYPE: rename
---
struct Box<«cursor»T> {
    value: T,
}

fn main() {
    let b = Box { value: 1 }
    print(b.value)
}
---
NEW_NAME: U
EXPECTED:
REPLACE: ["T", "U"]
===
