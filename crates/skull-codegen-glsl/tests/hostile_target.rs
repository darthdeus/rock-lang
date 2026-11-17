//! Sampler declarations and units cross the compiler-to-host ABI.

use crate::support::{entry_emission, pipeline_emission};
use ustr::Ustr;

#[test]
fn bare_and_grouped_samplers_use_distinct_host_unit_contracts() {
    let bare = r#"
@shader(fragment)
fn bare_frag(uTexture: Sampler2D, tc: Vec2) -> Vec4 {
    return texture(uTexture, tc)
}
"#;
    let bare = entry_emission(bare, "bare_frag").expect("a bare sampler entry must emit");
    assert!(bare.abi.texture_units.is_empty());
    assert!(bare.source.contains("uniform sampler2D uTexture;"));

    let grouped = r#"
struct Tex { atlas: Sampler2D }
@shader(fragment)
fn grouped_frag(@textures t: Tex, tc: Vec2) -> Vec4 {
    return texture(t.atlas, tc)
}
"#;
    let grouped =
        entry_emission(grouped, "grouped_frag").expect("a grouped sampler entry must emit");
    assert_eq!(grouped.abi.texture_units, vec![Ustr::from("atlas")]);
    assert!(grouped.source.contains("uniform sampler2D atlas;"));
}

#[test]
fn a_sampler_is_declared_in_each_stage_that_uses_it() {
    let src = r#"
value QuadVert { pos: Vec2, texcoord: Vec2 }
struct Tex { height: Sampler2D }
value Vary { position: Vec4, uv: Vec2 }

@shader(vertex)
fn tv(@attr a: QuadVert, @textures t: Tex) -> Vary {
    let h = texture(t.height, a.texcoord)
    return Vary { position: v(a.pos.x, a.pos.y + h.r, 0.0, 1.0), uv: a.texcoord }
}

@shader(fragment)
fn tf(vary: Vary, @textures t: Tex) -> Vec4 {
    return texture(t.height, vary.uv)
}
"#;
    let emission =
        pipeline_emission(src, "tv", "tf").expect("a pipeline sampling in both stages must emit");
    assert_eq!(emission.abi.texture_units, vec![Ustr::from("height")]);

    let vertex_start = emission
        .source
        .find("#ifdef VERTEX")
        .expect("a vertex section");
    let pixel_start = emission
        .source
        .find("#ifdef PIXEL")
        .expect("a pixel section");
    let (vertex, fragment) = if vertex_start < pixel_start {
        (
            &emission.source[vertex_start..pixel_start],
            &emission.source[pixel_start..],
        )
    } else {
        (
            &emission.source[vertex_start..],
            &emission.source[pixel_start..vertex_start],
        )
    };
    for (name, section) in [("vertex", vertex), ("fragment", fragment)] {
        assert!(
            section.contains("uniform sampler2D height;"),
            "the {name} stage must declare the sampler it reads:\n{section}"
        );
    }
}
