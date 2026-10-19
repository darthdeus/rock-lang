pub value SpriteVary {
    position: Vec4,
    TexCoord: Vec2,
    Color: Vec4,
    Colorize: float,
    WorldPosition: Vec2,
    @flat EntityId: float,
}

pub value SpriteFragU {
    uLinearWorld: bool = false,
    uTextureSrgb: bool = false,
    uColorFiltered: bool = false,
    uLightsEnabled: bool,
    uShadowsEnabled: bool,
    uUnlit: bool,
    uLightCount: int,
    uAmbientColor: Vec4,
    uGlowEnabled: bool,
    uHasNormalTex: bool,
    uHasSurfaceMask: bool,
    uNormalLightParams: Vec2,
    uNormalRelief: float,
}

pub value SpriteBatchOut {
    FragColor: Vec4,
    FragId: int,
}

pub struct SpriteBatchTex {
    uTexture: Sampler2D,
    uShadowmapTex: Sampler2D,
    uGlowTex: Sampler2D,
    uNormalTex: Sampler2D,
    uSurfaceTex: Sampler2D,
}

fn fixture_position_value(value: Vec2) -> float {
    return value.x + value.y
}

fn fixture_color_value(value: Vec4) -> float {
    return value.x + value.y + value.z + value.w
}

@shader(vertex)
fn sprite_batch_vert(@attr a: Vertex, @engine e: EngineU) -> SpriteVary {
    let clip = e.uProjection * e.uView * v(a.pos.to_f64(), 0.0, 1.0)
    return SpriteVary {
        position: clip,
        TexCoord: a.uv.to_f64(),
        Color: a.color.to_f64(),
        Colorize: a.colorize,
        WorldPosition: a.pos.to_f64(),
        EntityId: a.entity_id,
    }
}

@shader(fragment)
fn sprite_batch_frag(
    vary: SpriteVary,
    @uniform u: SpriteFragU,
    @textures tex: SpriteBatchTex,
    @size(16) uLightTypes: UniformArray<int>,
    @size(16) uLightPositions: UniformArray<Vec2>,
    @size(16) uLightColors: UniformArray<Vec4>,
    @size(16) uLightRadii: UniformArray<float>,
    @size(16) uLightStrengths: UniformArray<float>,
    @size(16) uLightDirections: UniformArray<Vec2>,
    @size(16) uLightAngles: UniformArray<float>,
) -> SpriteBatchOut {
    let tex_color = texture(tex.uTexture, vary.TexCoord)
    if tex_color.w < 0.1 { discard() }

    let coord = frag_coord()
    let shadow_coord = coord.xy / texture_size(tex.uShadowmapTex)
    let shadow = texture(tex.uShadowmapTex, shadow_coord)
    let glow = texture(tex.uGlowTex, shadow_coord)
    let normal = texture(tex.uNormalTex, vary.TexCoord)
    let surface = texture(tex.uSurfaceTex, vary.TexCoord)

    let linear = if u.uLinearWorld { 1.0 } else { 0.0 }
    let encoded = if u.uTextureSrgb { 1.0 } else { 0.0 }
    let filtered = if u.uColorFiltered { 1.0 } else { 0.0 }
    let shadow_factor = if u.uShadowsEnabled { shadow.x } else { 1.0 }
    let normal_factor = if u.uHasNormalTex { normal.x } else { 0.0 }
    let glow_factor = if u.uGlowEnabled { glow.x } else { 0.0 }
    let feature_factor = linear + encoded + filtered + normal_factor + glow_factor
        + u.uNormalLightParams.x + u.uNormalRelief
    let entity_id = vary.EntityId as int
    if u.uHasSurfaceMask { entity_id = int(surface.y) }

    let lighting = u.uAmbientColor.x + feature_factor
    if u.uUnlit { lighting = 1.0 }
    if u.uLightsEnabled {
        for i in 0..min(u.uLightCount, 16) {
            let light = int_to_float(at(uLightTypes, i))
                + fixture_position_value(at(uLightPositions, i))
                + fixture_color_value(at(uLightColors, i))
                + at(uLightRadii, i) + at(uLightStrengths, i)
                + fixture_position_value(at(uLightDirections, i))
                + at(uLightAngles, i)
            lighting += light * shadow_factor
        }
    }

    return SpriteBatchOut {
        FragColor: tex_color * vary.Color * lighting,
        FragId: entity_id,
    }
}
