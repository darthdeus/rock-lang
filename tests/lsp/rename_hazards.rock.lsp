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
