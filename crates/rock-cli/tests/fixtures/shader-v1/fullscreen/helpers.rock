private mod helpers;

pub @intrinsic @pure fn dot(a: Vec2, b: Vec2) -> float {
    return a.x * b.x + a.y * b.y
}

pub fn shader_helper(tc: Vec2, scale: float) -> Vec2 {
    let mixed = tc.lerp(Vec2 { x: 0.25, y: 0.75 }, scale)
    let normalized = mixed.normalize()
    return normalized + Vec2 { x: 0.1, y: 0.2 }
}
