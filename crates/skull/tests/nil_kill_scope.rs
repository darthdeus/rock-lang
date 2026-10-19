//! The nil-kill pass and the local-declaration loop must agree on WHICH cells
//! this body renders as bare `c<color>` locals.
//!
//! They did not. `referenced_cells_in_region` skips instructions the emitter
//! suppresses, so a multi-return call whose destination cells feed the trailing
//! arguments of the next call (`call_arg_forwards`) declares no locals for those
//! cells -- while `nil_kills` still saw the suppressed producer's defs, gave
//! them slots, and emitted `c<n> = nil` at their death point. That statement
//! assigns a GLOBAL of that name: it does nothing for LuaJIT snapshot pressure
//! (there is no local to drop) and, because skull installs an `_G` metatable for
//! Rock state globals, it costs a `__newindex` metamethod dispatch every time it
//! runs. Measured on an application's combat bench before the fix: 278 such statements in
//! the artifact, ~68,000 metamethod dispatches per run.
//!
//! The invariant is checked over the WHOLE emitted artifact, stdlib included:
//! any `<local> = nil` kill must name something the enclosing definition
//! declares.

use crate::common::emit_module;

/// `noise` is big enough that the emitter calls it rather than inlining it, and
/// takes a scalarised `value` struct, so each `noise(v(..))` is a multi-return
/// producer whose destination cells feed the call's trailing arguments. This is
/// the shape `application`/`host`'s `mote_curl` has, where the defect was found.
const SRC: &str = r#"
@pure fn h3(x: float, y: float, z: float) -> float {
    let h = sin(x * 127.1 + y * 311.7 + z * 74.7) * 43758.5453
    return h - floor(h)
}

@pure fn noise(p: Vec3) -> float {
    let ix = floor(p.x)
    let iy = floor(p.y)
    let iz = floor(p.z)
    let fx = p.x - ix
    let fy = p.y - iy
    let fz = p.z - iz
    let ux = fx * fx * (3.0 - 2.0 * fx)
    let uy = fy * fy * (3.0 - 2.0 * fy)
    let uz = fz * fz * (3.0 - 2.0 * fz)

    let c000 = h3(ix, iy, iz)
    let c100 = h3(ix + 1.0, iy, iz)
    let c010 = h3(ix, iy + 1.0, iz)
    let c110 = h3(ix + 1.0, iy + 1.0, iz)
    let c001 = h3(ix, iy, iz + 1.0)
    let c101 = h3(ix + 1.0, iy, iz + 1.0)
    let c011 = h3(ix, iy + 1.0, iz + 1.0)
    let c111 = h3(ix + 1.0, iy + 1.0, iz + 1.0)

    let x00 = c000 + (c100 - c000) * ux
    let x10 = c010 + (c110 - c010) * ux
    let x01 = c001 + (c101 - c001) * ux
    let x11 = c011 + (c111 - c011) * ux
    let y0 = x00 + (x10 - x00) * uy
    let y1 = x01 + (x11 - x01) * uy
    return y0 + (y1 - y0) * uz
}

@pure fn curl(p: Vec3) -> Vec3 {
    let eps = 0.1

    let dp3dy = noise(v(p.x + 83.1, p.y + 9.7 + eps, p.z + 55.5)) - noise(v(p.x + 83.1, p.y + 9.7 - eps, p.z + 55.5))
    let dp2dz = noise(v(p.x + 31.4, p.y + 47.2, p.z + 12.9 + eps)) - noise(v(p.x + 31.4, p.y + 47.2, p.z + 12.9 - eps))
    let dp1dz = noise(v(p.x, p.y, p.z + eps)) - noise(v(p.x, p.y, p.z - eps))
    let dp3dx = noise(v(p.x + 83.1 + eps, p.y + 9.7, p.z + 55.5)) - noise(v(p.x + 83.1 - eps, p.y + 9.7, p.z + 55.5))
    let dp2dx = noise(v(p.x + 31.4 + eps, p.y + 47.2, p.z + 12.9)) - noise(v(p.x + 31.4 - eps, p.y + 47.2, p.z + 12.9))
    let dp1dy = noise(v(p.x, p.y + eps, p.z)) - noise(v(p.x, p.y - eps, p.z))

    let inv = 1.0 / (2.0 * eps)
    return v((dp3dy - dp2dz) * inv, (dp1dz - dp3dx) * inv, (dp2dx - dp1dy) * inv)
}

fn main() {
    let r = curl(v(1.0, 2.0, 3.0))
    print("{r.x}")
}
"#;

/// The bare local names a line declares: every `function(<params>)` parameter
/// list on it (the top-level head and any nested `return function(..)` alike),
/// plus a `local a, b, c` declaration list.
fn declared_on(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(at) = rest.find("function") {
        rest = &rest[at + "function".len()..];
        let Some(open) = rest.find('(') else { break };
        // Only a parameter list: anything but a name between `function` and `(`.
        if rest[..open].contains(')') || rest[..open].contains(',') {
            continue;
        }
        let Some(close) = rest[open..].find(')') else { break };
        for p in rest[open + 1..open + close].split(',') {
            let p = p.trim();
            if !p.is_empty() {
                out.push(p.to_string());
            }
        }
        rest = &rest[open + close..];
    }
    let trimmed = line.trim_start();
    if let Some(decl) = trimmed.strip_prefix("local ") {
        let decl = decl.split('=').next().unwrap_or("");
        for name in decl.split(',') {
            let name = name.trim();
            if !name.is_empty() {
                out.push(name.to_string());
            }
        }
    }
    out
}

/// `Some(name)` when the line is a top-level definition head: `function N(`,
/// `local function N(` or `N = function(`, all at column 0.
fn definition_head(line: &str) -> Option<&str> {
    if line.starts_with(char::is_whitespace) {
        return None;
    }
    if let Some(rest) =
        line.strip_prefix("local function ").or_else(|| line.strip_prefix("function "))
    {
        return rest.split('(').next();
    }
    let (name, _) = line.split_once(" = function(")?;
    (!name.contains(' ')).then_some(name)
}

/// `(definition, line number, killed name)` for every `<local> = nil` statement
/// whose enclosing definition declares no such local.
fn undeclared_kills(lua: &str) -> Vec<(String, usize, String)> {
    let mut out = Vec::new();
    let mut current: Option<String> = None;
    let mut declared: Vec<String> = Vec::new();
    let mut pending: Vec<(usize, String)> = Vec::new();
    let flush = |current: &Option<String>, declared: &Vec<String>, pending: &mut Vec<(usize, String)>, out: &mut Vec<(String, usize, String)>| {
        let Some(name) = current else {
            pending.clear();
            return;
        };
        for (line_no, killed) in pending.drain(..) {
            if !declared.iter().any(|d| *d == killed) {
                out.push((name.clone(), line_no, killed));
            }
        }
    };
    for (index, line) in lua.lines().enumerate() {
        if let Some(name) = definition_head(line) {
            flush(&current, &declared, &mut pending, &mut out);
            current = Some(name.to_string());
            declared = Vec::new();
        }
        declared.extend(declared_on(line));
        let trimmed = line.trim();
        if let Some(name) = trimmed.strip_suffix(" = nil") {
            let scalar = name.starts_with(['r', 'c'])
                && name.len() > 1
                && name[1..].bytes().all(|b| b.is_ascii_digit());
            if scalar {
                pending.push((index + 1, name.to_string()));
            }
        }
    }
    flush(&current, &declared, &mut pending, &mut out);
    out
}

/// Every `<local> = nil` kill in the artifact names a local its own definition
/// declares. A kill of an undeclared name is a global store, which is never what
/// the pass wants.
#[test]
fn every_nil_kill_names_a_declared_local() {
    let lua = emit_module(SRC);
    // Guard: the defect needs the FORWARDED call shape, `noise(v(..))` rendered
    // as one nested call with the `v` results as trailing arguments. If the
    // inliner ever swallows `noise`, this test stops exercising the path and
    // must say so rather than pass vacuously.
    let forwarded = lua.matches("noise_Vec3(v_float_float_float(").count();
    assert!(
        forwarded >= 4,
        "fixture no longer emits the forwarded `noise(v(..))` shape ({forwarded} sites); \
         the nil-kill scope defect cannot arise and this test proves nothing"
    );
    let bad = undeclared_kills(&lua);
    assert!(
        bad.is_empty(),
        "these `= nil` kills name no local of their definition, so they assign a global:\n{}",
        bad.iter()
            .map(|(f, l, n)| format!("  {n} at line {l} in `{f}`"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
