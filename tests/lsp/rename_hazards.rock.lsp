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
