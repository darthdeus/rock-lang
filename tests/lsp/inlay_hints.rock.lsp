[[tests]]
name = "inlay_hints_let_bindings"
kind = "inlay_hints"
source = '''
fn main() {
    let x = 42
    let y = "hello"
    let z = [1, 2, 3]
    let w = true
}'''
request = { transport = "cli", argv = ["inlay-hints", "$file"] }

[[tests.checks]]
op = "contains_all"
path = ["result", "hints"]
value = [
  { position = { line = 1 }, label = { text = "int", trim = true, strip_prefix = ":" } },
  { position = { line = 2 }, label = { text = "string", trim = true, strip_prefix = ":" } },
  { position = { line = 3 }, label = { text = "[int]", trim = true, strip_prefix = ":" } },
  { position = { line = 4 }, label = { text = "bool", trim = true, strip_prefix = ":" } },
]

[[tests]]
name = "inlay_hints_complex_types"
kind = "inlay_hints"
source = '''
struct Point {
    x: float,
    y: float,
}

fn main() {
    let p = Point { x: 1.0, y: 2.0 }
    let points = [p, Point { x: 3.0, y: 4.0 }]
    let map = map_new()
    map.insert("origin", p)
}'''
request = { transport = "cli", argv = ["inlay-hints", "$file"] }
checks = [{ op = "contains_all", path = ["result", "hints"], value = [{ position = { line = 6 }, label = { text = "Point", trim = true, strip_prefix = ":" } }, { position = { line = 7 }, label = { text = "[Point]", trim = true, strip_prefix = ":" } }, { position = { line = 8 }, label = { text = "Map<string, Point>", trim = true, strip_prefix = ":" } }] }]

[[tests]]
name = "inlay_hints_function_parameters"
kind = "inlay_hints"
source = '''
fn calculate(x: int, y: int, operation: string) -> int {
    if operation == "add" {
        return x + y
    } else {
        return x - y
    }
}

fn main() {
    let result = calculate(10, 20, "add")
}'''
request = { transport = "cli", argv = ["inlay-hints", "$file"] }

[[tests.checks]]
op = "contains_all"
path = ["result", "hints"]
value = [
  { position = { line = 9 }, label = { text = "int", trim = true, strip_prefix = ":" } },
  { position = { line = 9 }, label = { text = "x:", trim = true, strip_prefix = ":" } },
  { position = { line = 9 }, label = { text = "y:", trim = true, strip_prefix = ":" } },
  { position = { line = 9 }, label = { text = "operation:", trim = true, strip_prefix = ":" } },
]

[[tests]]
name = "inlay_hints_generic_types"
kind = "inlay_hints"
source = '''
fn identity<T>(value: T) -> T {
    return value
}

fn main() {
    let num = identity(42)
    let text = identity("hello")
    let list = identity([1, 2, 3])
}'''
request = { transport = "cli", argv = ["inlay-hints", "$file"] }

[[tests.checks]]
op = "contains_all"
path = ["result", "hints"]
value = [
  { position = { line = 5 }, label = { text = "int", trim = true, strip_prefix = ":" } },
  { position = { line = 5 }, label = { text = "value:", trim = true, strip_prefix = ":" } },
  { position = { line = 6 }, label = { text = "string", trim = true, strip_prefix = ":" } },
  { position = { line = 6 }, label = { text = "value:", trim = true, strip_prefix = ":" } },
  { position = { line = 7 }, label = { text = "[int]", trim = true, strip_prefix = ":" } },
]

[[tests]]
name = "inlay_hints_nullable_types"
kind = "inlay_hints"
source = '''
fn find_user(id: int) -> string? {
    if id == 1 {
        return "Alice"
    } else {
        return null
    }
}

fn main() {
    let user = find_user(1)
    let missing = find_user(999)
}'''
request = { transport = "cli", argv = ["inlay-hints", "$file"] }

[[tests.checks]]
op = "contains_all"
path = ["result", "hints"]
value = [
  { position = { line = 9 }, label = { text = "string?", trim = true, strip_prefix = ":" } },
  { position = { line = 9 }, label = { text = "id:", trim = true, strip_prefix = ":" } },
  { position = { line = 10 }, label = { text = "string?", trim = true, strip_prefix = ":" } },
  { position = { line = 10 }, label = { text = "id:", trim = true, strip_prefix = ":" } },
]

[[tests]]
name = "inlay_hints_closures"
kind = "inlay_hints"
source = '''
fn main() {
    let add = (a, b) => a + b
    let multiply = (x: int, y: int) => { return x * y }
    let greet = (name) => print("Hello, " + name)
}'''
request = { transport = "cli", argv = ["inlay-hints", "$file"] }
checks = [{ op = "contains_all", path = ["result", "hints"], value = [{ position = { line = 2 }, label = { text = "fn(int, int) -> int", trim = true, strip_prefix = ":" } }] }]

[[tests]]
name = "inlay_hints_for_loop_variable"
kind = "inlay_hints"
source = '''
struct Item { name: string }

fn main() {
    let items: [Item] = vec_new()
    for item in items {
        print(item.name)
    }
}'''
request = { transport = "cli", argv = ["inlay-hints", "$file"] }
checks = [{ op = "contains_all", path = ["result", "hints"], value = [{ position = { line = 4 }, label = { text = "Item", trim = true, strip_prefix = ":" } }] }]

[[tests]]
name = "inlay_hints_match_arm_enum_variant"
kind = "inlay_hints"
source = '''
struct Point { x: int, y: int }

enum Opt {
    None,
    Some(Point),
}

fn main() {
    let o = Opt::Some(Point { x: 1, y: 2 })
    match o {
        Opt::Some(p) => print(p.x),
        Opt::None => {},
    }
}'''
request = { transport = "cli", argv = ["inlay-hints", "$file"] }
checks = [{ op = "contains_all", path = ["result", "hints"], value = [{ position = { line = 8 }, label = { text = "Opt", trim = true, strip_prefix = ":" } }, { position = { line = 10 }, label = { text = "Point", trim = true, strip_prefix = ":" } }] }]

[[tests]]
name = "inlay_hints_dform_receiver"
kind = "inlay_hints"
source = '''
struct Mob { health: float }

fn Mob.take_damage(m, amount: float) {
    m.health = m.health - amount
}

fn Vec<T>.first(v) -> T {
    return v[0]
}'''
request = { transport = "cli", argv = ["inlay-hints", "$file"] }
checks = [{ op = "contains_all", path = ["result", "hints"], value = [{ position = { line = 2 }, label = { text = "Mob", trim = true, strip_prefix = ":" } }, { position = { line = 6 }, label = { text = "Vec<T>", trim = true, strip_prefix = ":" } }] }]
