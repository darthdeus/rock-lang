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
OPERATION: rename
NEW_NAME: amount
---
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
---
fn spawn_it(count: int, size: float) {
    print(count)
    print(size)
}

fn spawn_it(count: string) {
    print(count)
}

fn main() {
    spawn_it(count: 3, size: 1.0)
    spawn_it(«cursor»count: "a")
}
---
OPERATION: rename
NEW_NAME: label
---
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
OPERATION: rename
NEW_NAME: twice
---
EXPECTED:
REFUSED: cannot rename a macro
===

TEST: rename_macro_name_from_decl_refused
TYPE: rename
---
macro «cursor»double(x: expr) -> int { x + x }

fn main() {
    print(double(5))
}
---
OPERATION: rename
NEW_NAME: twice
---
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
OPERATION: rename
NEW_NAME: total
---
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
OPERATION: rename
NEW_NAME: POINTS
---
EXPECTED:
REFUSED: .rock:6 that rename cannot rewrite
===

TEST: rename_macro_param
TYPE: rename
---
macro double(«cursor»x: expr) -> int { x + x }

fn main() {
    print(double(5))
}
---
OPERATION: rename
NEW_NAME: v
---
EXPECTED:
macro double(v: expr) -> int { v + v }

fn main() {
    print(double(5))
}
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
OPERATION: rename
NEW_NAME: m
---
EXPECTED:
macro double(x: expr) -> int { x + x }

fn main() {
    let m = 5
    print(double(m))
}
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
OPERATION: rename
NEW_NAME: inc
---
EXPECTED:
fn inc(x: int) -> int {
    return x + 1
}

macro twice(x: expr) -> int { inc(inc(x)) }

fn main() {
    print(twice(1))
    print(inc(2))
}
===

TEST: rename_inline_lua_param_refused
TYPE: rename
---
inline(lua) fn probe(«cursor»x: int) -> int = "return x + 1"

fn main() {
    print(probe(1))
}
---
OPERATION: rename
NEW_NAME: y
---
EXPECTED:
REFUSED: cannot rename `x`: the opaque inline(lua) body of `probe` (
===

TEST: rename_inline_js_param_refused
TYPE: rename
---
inline(js) fn probe(«cursor»x: int) -> int = "return x + 1;"

fn main() {
    print(probe(1))
}
---
OPERATION: rename
NEW_NAME: y
---
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
OPERATION: rename
NEW_NAME: helper2
---
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
OPERATION: rename
NEW_NAME: helper2
---
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
OPERATION: rename
NEW_NAME: health
---
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
OPERATION: rename
NEW_NAME: POINTS
---
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
OPERATION: rename
NEW_NAME: Beast
---
EXPECTED:
REFUSED: the opaque inline(lua) body of `is_mob`
===

TEST: rename_inline_fn_name_is_fine
TYPE: rename
---
inline(lua) fn «cursor»probe(x: int) -> int = "return x + 1"

fn main() {
    print(probe(1))
}
---
OPERATION: rename
NEW_NAME: probe2
---
EXPECTED:
inline(lua) fn probe2(x: int) -> int = "return x + 1"

fn main() {
    print(probe2(1))
}
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
OPERATION: rename
NEW_NAME: health
---
EXPECTED:
struct Mob {
    health: int,
    mana: int,
}

inline(lua) fn peek(m: Mob) -> int = "return m.mana"

fn main() {
    let m = Mob { health: 4, mana: 2 }
    print(m.health + peek(m))
}
===

TEST: rename_param_sharing_a_word_with_another_inline_body
TYPE: rename
---
inline(lua) fn probe(x: int) -> int = "return x + 1"

fn twice(«cursor»x: int) -> int {
    return probe(x) * 2
}
---
OPERATION: rename
NEW_NAME: n
---
EXPECTED:
inline(lua) fn probe(x: int) -> int = "return x + 1"

fn twice(n: int) -> int {
    return probe(n) * 2
}
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
OPERATION: rename
NEW_NAME: helper2
---
EXPECTED:
fn helper2(x: int) -> int {
    return x * 2
}

fn main() {
    print("helper")
    print(helper2(1))
}
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
FILE: project.toml
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
FILE: project.toml
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
FILE: project.toml
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
OPERATION: rename
NEW_NAME: y
---
EXPECTED:
fn main() {
    let x = 1
    if true {
        let y = 2
        print(y)
    }
    print(x)
}
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
OPERATION: rename
NEW_NAME: y
---
EXPECTED:
fn main() {
    let y = 1
    let x = y + 1
    print(x)
}
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
OPERATION: rename
NEW_NAME: size
---
EXPECTED:
struct Circle {
    radius: float,
}

fn size(c: Circle) -> float {
    return c.radius * c.radius
}

fn main() {
    let c = Circle { radius: 1.0 }
    print(c.size())
    print(size(c))
}
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
OPERATION: rename
NEW_NAME: nudge
---
EXPECTED:
struct Point {
    x: int,
}

fn Point.nudge(p, dx: int) -> int {
    return p.x + dx
}

fn main() {
    let p = Point { x: 1 }
    print(p.nudge(1))
    print(nudge(p, 2))
}
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
OPERATION: rename
NEW_NAME: health
---
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
OPERATION: rename
NEW_NAME: count
---
EXPECTED:
fn main() {
    let count = 5
    print("n = {count}")
}
===

TEST: rename_lambda_param
TYPE: rename
---
fn main() {
    let f = («cursor»a: int) => a + 1
    print(f(1))
}
---
OPERATION: rename
NEW_NAME: b
---
EXPECTED:
fn main() {
    let f = (b: int) => b + 1
    print(f(1))
}
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
OPERATION: rename
NEW_NAME: U
---
EXPECTED:
struct Box<U> {
    value: U,
}

fn main() {
    let b = Box { value: 1 }
    print(b.value)
}
===
