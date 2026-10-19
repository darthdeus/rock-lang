pub @nomangle value Vertex {
    pos: Vec2f,
    uv: Vec2f,
    color: Vec4f,
    pivot_y: f32,
    entity_id: f32,
    colorize: f32,
}

pub value EngineU {
    uProjection: Mat4,
    uView: Mat4,
    uTextureVFlip: uint,
}
