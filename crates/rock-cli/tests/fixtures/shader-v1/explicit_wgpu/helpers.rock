private mod helpers;

pub @pure fn __op_add(a: Vec2, b: Vec2) -> Vec2 {
    return Vec2 { x: a.x + b.x, y: a.y + b.y }
}

pub @intrinsic @pure fn dot(a: Vec2, b: Vec2) -> float {
    return a.x * b.x + a.y * b.y
}

pub fn shader_helper(tc: Vec2, scale: float) -> Vec2 {
    let mixed = tc.lerp(Vec2 { x: 0.25, y: 0.75 }, scale)
    let normalized = mixed.normalize()
    let custom = shader_probe::helpers::__op_add(normalized, Vec2 { x: 0.1, y: 0.2 })
    return custom + 0.0
}
