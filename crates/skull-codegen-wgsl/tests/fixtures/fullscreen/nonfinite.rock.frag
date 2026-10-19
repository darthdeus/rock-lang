pub struct SrcTex { src_tex: Sampler2D }

pub @shader(fragment)
fn nonfinite_frag(@textures t: SrcTex, tc: Vec2) -> Vec4 {
    let color = texture(t.src_tex, tc)
    let bad = bvec3(
        isinf(color.r) || isnan(color.r),
        isinf(color.g) || isnan(color.g),
        isinf(color.b) || isnan(color.b),
    )
    if any(bad) {
        return v(to_vec3(bad), 1.0)
    } else if any(greater_than(color.rgb.abs(), v(60000.0, 60000.0, 60000.0))) {
        return v(1.0, 1.0, 0.0, 1.0)
    } else {
        let luma = color.rgb.dot(v(0.2126, 0.7152, 0.0722))
        let dim = luma * 0.05
        return v(dim, dim, dim, 1.0)
    }
}
