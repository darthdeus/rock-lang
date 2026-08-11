//! The dependency-light physical WGSL registration contract.
//!
//! This crate is the only owner of the version, wire records, checksum and
//! parser shared by the WGSL producer and the selected-WGPU consumer. It does
//! not depend on Rock's parser, MIR, compiler or backend runtime.

use std::fmt;

pub const WGSL_ABI_V2: &str = "WGSL_ABI_V2";
const MAX_RECORDS: u64 = 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WgslGeometryKind {
    Fullscreen,
    Triangles,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WgslPrimitiveTopology {
    TriangleStrip,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WgslVisibility {
    pub vertex: bool,
    pub fragment: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WgslFullscreenVertex {
    pub topology: WgslPrimitiveTopology,
    pub vertex_count: u32,
    pub varying_location: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WgslFragmentIo {
    pub uv_location: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WgslUniformMember {
    pub name: String,
    pub kind: u32,
    pub offset: u32,
    pub align: u32,
    pub size: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WgslUniformBlock {
    pub role: String,
    pub struct_name: String,
    pub binding: u32,
    pub size: u32,
    pub visibility: WgslVisibility,
    pub members: Vec<WgslUniformMember>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WgslTextureKind {
    Float,
    Int,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WgslTextureBinding {
    pub slot: u32,
    pub name: String,
    pub kind: WgslTextureKind,
    pub texture_binding: u32,
    pub sampler_binding: u32,
    pub visibility: WgslVisibility,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WgslFragmentOutput {
    pub name: String,
    pub location: u32,
    pub kind: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WgslArrayMember {
    pub name: String,
    pub elem: u32,
    pub capacity: u32,
    pub offset: u32,
    pub stride: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WgslArrayBlock {
    pub stage: String,
    pub logical_name: String,
    pub struct_name: String,
    pub binding: u32,
    pub size: u32,
    pub visibility: WgslVisibility,
    pub members: Vec<WgslArrayMember>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WgslVertexAttribute {
    pub location: u32,
    pub name: String,
    pub kind: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WgslVertexInput {
    pub index: u32,
    pub struct_name: String,
    pub rate: String,
    pub attrs: Vec<WgslVertexAttribute>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WgslVarying {
    pub location: u32,
    pub name: String,
    pub kind: u32,
    pub flat: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WgslAbi {
    pub semantic_fingerprint: u64,
    pub bind_group: u32,
    pub geometry: WgslGeometryKind,
    pub vertex_entry: String,
    pub fragment_entry: String,
    pub vertex_inputs: Vec<WgslVertexInput>,
    pub varyings: Vec<WgslVarying>,
    pub fullscreen_vertex: WgslFullscreenVertex,
    pub fragment: WgslFragmentIo,
    pub fragment_outputs: Vec<WgslFragmentOutput>,
    pub fullscreen_uniform: WgslUniformBlock,
    pub uniform_blocks: Vec<WgslUniformBlock>,
    pub array_blocks: Vec<WgslArrayBlock>,
    pub textures: Vec<WgslTextureBinding>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AbiError {
    message: String,
}

impl AbiError {
    fn new(message: impl Into<String>) -> Self {
        Self { message: message.into() }
    }
}

impl fmt::Display for AbiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for AbiError {}

impl WgslAbi {
    pub fn parse(input: &str) -> Result<Self, AbiError> {
        if input.contains('\r') {
            return Err(wire_error("carriage returns are not part of the wire"));
        }
        let mut lines = input.split_inclusive('\n');
        let version = lines
            .next()
            .ok_or_else(|| wire_error("missing WGSL ABI version"))?
            .strip_suffix('\n')
            .ok_or_else(|| wire_error("WGSL ABI version is missing a newline"))?;
        if version == "WGSL_ABI_V1" {
            return Err(wire_error("WGSL_ABI_V1 is rejected; expected WGSL_ABI_V2"));
        }
        if version != WGSL_ABI_V2 {
            return Err(wire_error(format!("expected {WGSL_ABI_V2}, got {version}")));
        }

        let checksum_line = lines
            .next()
            .ok_or_else(|| wire_error("missing checksum line"))?;
        let checksum = fields(checksum_line, "checksum", 2)?;
        let expected_checksum = parse_u64(checksum[1], "checksum")?;
        let payload: String = lines.collect();
        if payload.is_empty() || !payload.ends_with('\n') {
            return Err(wire_error("payload must end with a newline"));
        }
        if wire_checksum(payload.as_bytes()) != expected_checksum {
            return Err(wire_error("checksum does not cover the supplied ABI"));
        }

        let mut payload_lines = payload.lines();
        let semantic = fields(next_line(&mut payload_lines, "semantic")?, "semantic", 2)?;
        let semantic_fingerprint = parse_u64(semantic[1], "semantic fingerprint")?;
        let group = fields(next_line(&mut payload_lines, "group")?, "group", 2)?;
        let bind_group = parse_u32(group[1], "bind group")?;
        let geometry_line = fields(next_line(&mut payload_lines, "geometry")?, "geometry", 2)?;
        let geometry = match geometry_line[1] {
            "fullscreen" => WgslGeometryKind::Fullscreen,
            "triangles" => WgslGeometryKind::Triangles,
            other => return Err(wire_error(format!("unknown geometry {other}"))),
        };

        let entries = fields(next_line(&mut payload_lines, "entries")?, "entries", 3)?;
        let vertex_entry = identifier(entries[1], "vertex entry")?;
        let fragment_entry = identifier(entries[2], "fragment entry")?;

        let (fullscreen_vertex, fragment, fullscreen_uniform, vertex_inputs, varyings) =
            match geometry {
                WgslGeometryKind::Fullscreen => {
                    let vertex = fields(
                        next_line(&mut payload_lines, "fullscreen_vertex")?,
                        "fullscreen_vertex",
                        4,
                    )?;
                    let topology = match vertex[1] {
                        "triangle_strip" => WgslPrimitiveTopology::TriangleStrip,
                        other => return Err(wire_error(format!("unknown fullscreen topology {other}"))),
                    };
                    let vertex_count = parse_u32(vertex[2], "fullscreen vertex count")?;
                    let varying_location = parse_u32(vertex[3], "fullscreen varying location")?;
                    let fragment_line = fields(
                        next_line(&mut payload_lines, "fullscreen_fragment")?,
                        "fullscreen_fragment",
                        2,
                    )?;
                    let uv_location = parse_u32(fragment_line[1], "fullscreen UV location")?;
                    let uniform = parse_uniform_block(
                        &mut payload_lines,
                        "fullscreen_uniform",
                        "fullscreen",
                        0,
                    )?;
                    (
                        WgslFullscreenVertex { topology, vertex_count, varying_location },
                        WgslFragmentIo { uv_location },
                        uniform,
                        Vec::new(),
                        Vec::new(),
                    )
                }
                WgslGeometryKind::Triangles => {
                    let input_count = parse_count(
                        fields(next_line(&mut payload_lines, "vertex_inputs")?, "vertex_inputs", 2)?[1],
                        "vertex input count",
                    )?;
                    let mut inputs = Vec::with_capacity(input_count);
                    for expected_index in 0..input_count {
                        let input = fields(
                            next_line(&mut payload_lines, "vertex_input")?,
                            "vertex_input",
                            5,
                        )?;
                        let index = parse_u32(input[1], "vertex input index")?;
                        if index != expected_index as u32 {
                            return Err(wire_error("vertex inputs are not in order"));
                        }
                        let rate = match input[3] {
                            "vertex" | "instance" => input[3].to_string(),
                            other => return Err(wire_error(format!("unknown vertex input rate {other}"))),
                        };
                        let attr_count = parse_count(input[4], "vertex attribute count")?;
                        let mut attrs = Vec::with_capacity(attr_count);
                        for expected_location in 0..attr_count {
                            let attr = fields(next_line(&mut payload_lines, "attr")?, "attr", 4)?;
                            let location = parse_u32(attr[1], "vertex attribute location")?;
                            if location != expected_location as u32 {
                                return Err(wire_error("vertex attributes are not in order"));
                            }
                            attr_kind(attr[3])?;
                            attrs.push(WgslVertexAttribute {
                                location,
                                name: identifier(attr[2], "vertex attribute name")?,
                                kind: attr[3].to_string(),
                            });
                        }
                        inputs.push(WgslVertexInput {
                            index,
                            struct_name: identifier(input[2], "vertex input name")?,
                            rate,
                            attrs,
                        });
                    }

                    let varying_count = parse_count(
                        fields(next_line(&mut payload_lines, "varyings")?, "varyings", 2)?[1],
                        "varying count",
                    )?;
                    let mut varyings = Vec::with_capacity(varying_count);
                    for expected_location in 0..varying_count {
                        let varying = fields(next_line(&mut payload_lines, "varying")?, "varying", 5)?;
                        let location = parse_u32(varying[1], "varying location")?;
                        if location != expected_location as u32 {
                            return Err(wire_error("varyings are not in order"));
                        }
                        let flat = match varying[4] {
                            "0" => false,
                            "1" => true,
                            _ => return Err(wire_error("varying flat flag must be 0 or 1")),
                        };
                        varyings.push(WgslVarying {
                            location,
                            name: identifier(varying[2], "varying name")?,
                            kind: parse_kind(varying[3], "varying kind")?,
                            flat,
                        });
                    }

                    (
                        WgslFullscreenVertex {
                            topology: WgslPrimitiveTopology::TriangleStrip,
                            vertex_count: 0,
                            varying_location: 0,
                        },
                        WgslFragmentIo { uv_location: 0 },
                        empty_uniform_block(),
                        inputs,
                        varyings,
                    )
                }
            };

        let output_count = parse_count(
            fields(next_line(&mut payload_lines, "outputs")?, "outputs", 2)?[1],
            "fragment output count",
        )?;
        if output_count == 0 {
            return Err(wire_error("WGSL ABI must declare a fragment output"));
        }
        let mut fragment_outputs = Vec::with_capacity(output_count);
        for expected_location in 0..output_count {
            let output = fields(next_line(&mut payload_lines, "output")?, "output", 4)?;
            let location = parse_u32(output[1], "fragment output location")?;
            if location != expected_location as u32 {
                return Err(wire_error("fragment outputs are not in location order"));
            }
            fragment_outputs.push(WgslFragmentOutput {
                location,
                name: identifier(output[2], "fragment output name")?,
                kind: parse_kind(output[3], "fragment output kind")?,
            });
        }

        let array_count = parse_count(
            fields(next_line(&mut payload_lines, "array_blocks")?, "array_blocks", 2)?[1],
            "uniform array block count",
        )?;
        let mut array_blocks = Vec::with_capacity(array_count);
        for _ in 0..array_count {
            let block = fields(next_line(&mut payload_lines, "array_block")?, "array_block", 8)?;
            let binding = parse_u32(block[4], "uniform array binding")?;
            let member_count = parse_count(block[7], "uniform array member count")?;
            if member_count == 0 {
                return Err(wire_error("uniform array block has no members"));
            }
            let mut members = Vec::with_capacity(member_count);
            for _ in 0..member_count {
                let member = fields(next_line(&mut payload_lines, "array_member")?, "array_member", 6)?;
                members.push(WgslArrayMember {
                    name: identifier(member[1], "uniform array member name")?,
                    elem: parse_kind(member[2], "uniform array element kind")?,
                    capacity: parse_u32(member[3], "uniform array capacity")?,
                    offset: parse_u32(member[4], "uniform array offset")?,
                    stride: parse_u32(member[5], "uniform array stride")?,
                });
            }
            stage_name(block[1])?;
            array_blocks.push(WgslArrayBlock {
                stage: block[1].to_string(),
                logical_name: identifier(block[2], "uniform array logical name")?,
                struct_name: identifier(block[3], "uniform array block name")?,
                binding,
                size: parse_u32(block[5], "uniform array block size")?,
                visibility: parse_visibility(block[6])?,
                members,
            });
        }

        let block_count = parse_count(
            fields(next_line(&mut payload_lines, "blocks")?, "blocks", 2)?[1],
            "uniform block count",
        )?;
        let expected_block_start = if geometry == WgslGeometryKind::Fullscreen { 1 } else { 0 };
        let mut uniform_blocks = Vec::with_capacity(block_count);
        for expected_index in 0..block_count {
            uniform_blocks.push(parse_uniform_block(
                &mut payload_lines,
                "block",
                "ordinary",
                expected_index as u32 + expected_block_start,
            )?);
        }

        let texture_count = parse_count(
            fields(next_line(&mut payload_lines, "textures")?, "textures", 2)?[1],
            "texture count",
        )?;
        let texture_base = expected_block_start
            .checked_add(block_count as u32)
            .and_then(|base| base.checked_add(array_count as u32))
            .ok_or_else(|| wire_error("texture binding base overflows"))?;
        let mut textures = Vec::with_capacity(texture_count);
        for expected_slot in 0..texture_count {
            let texture = fields(next_line(&mut payload_lines, "texture")?, "texture", 7)?;
            let slot = parse_u32(texture[1], "texture slot")?;
            if slot != expected_slot as u32 {
                return Err(wire_error("textures are not in slot order"));
            }
            let base = texture_base
                .checked_add((expected_slot as u32).checked_mul(2).ok_or_else(|| wire_error("texture binding overflows"))?)
                .ok_or_else(|| wire_error("texture binding overflows"))?;
            let texture_binding = parse_u32(texture[4], "texture view binding")?;
            let sampler_binding = parse_u32(texture[5], "sampler binding")?;
            if (texture_binding, sampler_binding) != (base, base + 1) {
                return Err(wire_error("texture bindings are not in physical order"));
            }
            textures.push(WgslTextureBinding {
                slot,
                name: identifier(texture[2], "texture name")?,
                kind: texture_kind(texture[3])?,
                texture_binding,
                sampler_binding,
                visibility: parse_visibility(texture[6])?,
            });
        }
        if next_line(&mut payload_lines, "end")? != "end" {
            return Err(wire_error("missing ABI end record"));
        }
        if next_line_optional(&mut payload_lines)?.is_some() {
            return Err(wire_error("trailing ABI records"));
        }

        let abi = Self {
            semantic_fingerprint,
            bind_group,
            geometry,
            vertex_entry,
            fragment_entry,
            vertex_inputs,
            varyings,
            fullscreen_vertex,
            fragment,
            fragment_outputs,
            fullscreen_uniform,
            uniform_blocks,
            array_blocks,
            textures,
        };
        abi.validate()?;
        Ok(abi)
    }

    pub fn serialize(&self) -> Result<String, AbiError> {
        self.validate()?;
        let mut payload = String::new();
        push_line(&mut payload, format_args!("semantic {}", self.semantic_fingerprint));
        push_line(&mut payload, format_args!("group {}", self.bind_group));
        push_line(
            &mut payload,
            format_args!(
                "geometry {}",
                match self.geometry {
                    WgslGeometryKind::Fullscreen => "fullscreen",
                    WgslGeometryKind::Triangles => "triangles",
                }
            ),
        );
        push_line(&mut payload, format_args!("entries {} {}", self.vertex_entry, self.fragment_entry));
        match self.geometry {
            WgslGeometryKind::Fullscreen => {
                push_line(
                    &mut payload,
                    format_args!(
                        "fullscreen_vertex triangle_strip {} {}",
                        self.fullscreen_vertex.vertex_count,
                        self.fullscreen_vertex.varying_location
                    ),
                );
                push_line(&mut payload, format_args!("fullscreen_fragment {}", self.fragment.uv_location));
                write_uniform_block(&mut payload, "fullscreen_uniform", &self.fullscreen_uniform)?;
            }
            WgslGeometryKind::Triangles => {
                push_line(&mut payload, format_args!("vertex_inputs {}", self.vertex_inputs.len()));
                for input in &self.vertex_inputs {
                    push_line(
                        &mut payload,
                        format_args!(
                            "vertex_input {} {} {} {}",
                            input.index,
                            input.struct_name,
                            input.rate,
                            input.attrs.len()
                        ),
                    );
                    for attr in &input.attrs {
                        push_line(
                            &mut payload,
                            format_args!("attr {} {} {}", attr.location, attr.name, attr.kind),
                        );
                    }
                }
                push_line(&mut payload, format_args!("varyings {}", self.varyings.len()));
                for varying in &self.varyings {
                    push_line(
                        &mut payload,
                        format_args!(
                            "varying {} {} {} {}",
                            varying.location,
                            varying.name,
                            varying.kind,
                            if varying.flat { 1 } else { 0 }
                        ),
                    );
                }
            }
        }
        push_line(&mut payload, format_args!("outputs {}", self.fragment_outputs.len()));
        for output in &self.fragment_outputs {
            push_line(
                &mut payload,
                format_args!("output {} {} {}", output.location, output.name, output.kind),
            );
        }
        push_line(&mut payload, format_args!("array_blocks {}", self.array_blocks.len()));
        for block in &self.array_blocks {
            push_line(
                &mut payload,
                format_args!(
                    "array_block {} {} {} {} {} {} {}",
                    block.stage,
                    block.logical_name,
                    block.struct_name,
                    block.binding,
                    block.size,
                    visibility_wire(block.visibility),
                    block.members.len()
                ),
            );
            for member in &block.members {
                push_line(
                    &mut payload,
                    format_args!(
                        "array_member {} {} {} {} {}",
                        member.name, member.elem, member.capacity, member.offset, member.stride
                    ),
                );
            }
        }
        push_line(&mut payload, format_args!("blocks {}", self.uniform_blocks.len()));
        for block in &self.uniform_blocks {
            write_uniform_block(&mut payload, "block", block)?;
        }
        push_line(&mut payload, format_args!("textures {}", self.textures.len()));
        for texture in &self.textures {
            push_line(
                &mut payload,
                format_args!(
                    "texture {} {} {} {} {} {}",
                    texture.slot,
                    texture.name,
                    texture_kind_wire(texture.kind),
                    texture.texture_binding,
                    texture.sampler_binding,
                    visibility_wire(texture.visibility)
                ),
            );
        }
        push_line(&mut payload, format_args!("end"));
        Ok(format!(
            "{}\nchecksum {}\n{}",
            WGSL_ABI_V2,
            wire_checksum(payload.as_bytes()),
            payload
        ))
    }

    pub fn validate(&self) -> Result<(), AbiError> {
        if self.bind_group != 0 {
            return Err(wire_error("WGSL artifacts must use bind group 0"));
        }
        identifier(&self.vertex_entry, "vertex entry")?;
        identifier(&self.fragment_entry, "fragment entry")?;
        if self.fragment_outputs.is_empty() {
            return Err(wire_error("WGSL ABI must declare a fragment output"));
        }
        for (location, output) in self.fragment_outputs.iter().enumerate() {
            if output.location != location as u32 {
                return Err(wire_error("fragment outputs are not in location order"));
            }
            identifier(&output.name, "fragment output name")?;
            validate_fragment_output_kind(output.kind)?;
        }

        match self.geometry {
            WgslGeometryKind::Fullscreen => {
                if self.fullscreen_vertex.topology != WgslPrimitiveTopology::TriangleStrip
                    || self.fullscreen_vertex.vertex_count != 4
                    || self.fullscreen_vertex.varying_location != 0
                    || self.fragment.uv_location != 0
                {
                    return Err(wire_error("unsupported fullscreen geometry metadata"));
                }
                if !self.vertex_inputs.is_empty()
                    || !self.varyings.is_empty()
                    || !self.array_blocks.is_empty()
                {
                    return Err(wire_error("fullscreen ABI carries explicit-only records"));
                }
                if self.fragment_outputs.len() != 1 {
                    return Err(wire_error("fullscreen ABI requires exactly one output"));
                }
                validate_uniform_block(&self.fullscreen_uniform, 0, "FullscreenU")?;
                if self.fullscreen_uniform.visibility != (WgslVisibility { vertex: true, fragment: false }) {
                    return Err(wire_error("fullscreen uniform has unsupported visibility"));
                }
                if self.uniform_blocks.iter().any(|block| {
                    block.visibility != (WgslVisibility { vertex: false, fragment: true })
                }) {
                    return Err(wire_error("fullscreen ordinary uniform has unsupported visibility"));
                }
                if self.textures.iter().any(|texture| {
                    texture.visibility != (WgslVisibility { vertex: false, fragment: true })
                }) {
                    return Err(wire_error("fullscreen texture has unsupported visibility"));
                }
                validate_binding_order(&self.uniform_blocks, &[], 1)?;
            }
            WgslGeometryKind::Triangles => {
                if self.fullscreen_uniform.size != 0 || !self.fullscreen_uniform.members.is_empty() {
                    return Err(wire_error("explicit ABI carries a fullscreen uniform"));
                }
                for (index, input) in self.vertex_inputs.iter().enumerate() {
                    if input.index != index as u32 {
                        return Err(wire_error("vertex inputs are not in order"));
                    }
                    identifier(&input.struct_name, "vertex input name")?;
                    if input.rate != "vertex" && input.rate != "instance" {
                        return Err(wire_error(format!("unknown vertex input rate {}", input.rate)));
                    }
                    for (location, attr) in input.attrs.iter().enumerate() {
                        if attr.location != location as u32 {
                            return Err(wire_error("vertex attributes are not in order"));
                        }
                        identifier(&attr.name, "vertex attribute name")?;
                        attr_kind(&attr.kind)?;
                    }
                }
                for (location, varying) in self.varyings.iter().enumerate() {
                    if varying.location != location as u32 {
                        return Err(wire_error("varyings are not in order"));
                    }
                    identifier(&varying.name, "varying name")?;
                    validate_kind(varying.kind, "varying kind")?;
                }
            }
        }

        for block in &self.array_blocks {
            stage_name(&block.stage)?;
            identifier(&block.logical_name, "uniform array logical name")?;
            identifier(&block.struct_name, "uniform array block name")?;
            if block.members.is_empty() {
                return Err(wire_error("uniform array block has no members"));
            }
            validate_array_block(block)?;
        }
        for block in &self.uniform_blocks {
            validate_uniform_block(block, block.binding, "ordinary")?;
        }
        let expected_block_start = if self.geometry == WgslGeometryKind::Fullscreen { 1 } else { 0 };
        validate_binding_order(&self.uniform_blocks, &self.array_blocks, expected_block_start)?;

        let texture_base = expected_block_start
            .checked_add(self.uniform_blocks.len() as u32)
            .and_then(|base| base.checked_add(self.array_blocks.len() as u32))
            .ok_or_else(|| wire_error("texture binding base overflows"))?;
        for (slot, texture) in self.textures.iter().enumerate() {
            if texture.slot != slot as u32 {
                return Err(wire_error("textures are not in slot order"));
            }
            identifier(&texture.name, "texture name")?;
            let base = texture_base
                .checked_add((slot as u32).checked_mul(2).ok_or_else(|| wire_error("texture binding overflows"))?)
                .ok_or_else(|| wire_error("texture binding overflows"))?;
            if (texture.texture_binding, texture.sampler_binding) != (base, base + 1) {
                return Err(wire_error("texture bindings are not in physical order"));
            }
        }
        Ok(())
    }
}

fn write_uniform_block(
    payload: &mut String,
    tag: &str,
    block: &WgslUniformBlock,
) -> Result<(), AbiError> {
    let role = if tag == "fullscreen_uniform" { "fullscreen" } else { &block.role };
    push_line(
        payload,
        format_args!(
            "{} {} {} {} {} {} {}",
            tag,
            role,
            block.struct_name,
            block.binding,
            block.size,
            visibility_wire(block.visibility),
            block.members.len()
        ),
    );
    for member in &block.members {
        push_line(
            payload,
            format_args!(
                "member {} {} {} {} {}",
                member.name, member.kind, member.offset, member.align, member.size
            ),
        );
    }
    Ok(())
}

fn parse_uniform_block<'a>(
    lines: &mut impl Iterator<Item = &'a str>,
    tag: &str,
    expected_role: &str,
    expected_binding: u32,
) -> Result<WgslUniformBlock, AbiError> {
    let block = fields(next_line(lines, tag)?, tag, 7)?;
    let role = if tag == "fullscreen_uniform" {
        if block[1] != expected_role {
            return Err(wire_error("fullscreen uniform has the wrong role"));
        }
        block[1].to_string()
    } else {
        let role = block[1];
        if expected_role == "ordinary" {
            if !matches!(role, "engine" | "fragment" | "vertex") {
                return Err(wire_error(format!("unknown uniform role {role}")));
            }
        }
        role.to_string()
    };
    let binding = parse_u32(block[3], "uniform binding")?;
    if binding != expected_binding {
        return Err(wire_error(format!("uniform binding {binding} is not {expected_binding}")));
    }
    let member_count = parse_count(block[6], "uniform member count")?;
    let mut members = Vec::with_capacity(member_count);
    for _ in 0..member_count {
        let member = fields(next_line(lines, "member")?, "member", 6)?;
        members.push(WgslUniformMember {
            name: identifier(member[1], "uniform member name")?,
            kind: parse_kind(member[2], "uniform member kind")?,
            offset: parse_u32(member[3], "uniform member offset")?,
            align: parse_u32(member[4], "uniform member alignment")?,
            size: parse_u32(member[5], "uniform member size")?,
        });
    }
    Ok(WgslUniformBlock {
        role,
        struct_name: identifier(block[2], "uniform block name")?,
        binding,
        size: parse_u32(block[4], "uniform block size")?,
        visibility: parse_visibility(block[5])?,
        members,
    })
}

fn validate_uniform_block(
    block: &WgslUniformBlock,
    expected_binding: u32,
    name: &str,
) -> Result<(), AbiError> {
    identifier(&block.struct_name, "uniform block name")?;
    if name == "ordinary" && !matches!(block.role.as_str(), "engine" | "fragment" | "vertex") {
        return Err(wire_error(format!("unknown uniform role {}", block.role)));
    }
    if name == "FullscreenU" && block.role != "fullscreen" {
        return Err(wire_error("fullscreen uniform has the wrong role"));
    }
    if block.binding != expected_binding {
        return Err(wire_error(format!("{name} uniform binding is out of order")));
    }
    if block.members.is_empty() && block.size != 0 {
        return Err(wire_error(format!("{name} uniform block has no members")));
    }
    let mut cursor = 0u64;
    for member in &block.members {
        identifier(&member.name, "uniform member name")?;
        if block.members.iter().filter(|candidate| candidate.name == member.name).count() != 1 {
            return Err(wire_error(format!("uniform member {} is repeated", member.name)));
        }
        let (align, size) = uniform_layout(member.kind)?;
        cursor = round_up(cursor, align as u64)?;
        if member.offset as u64 != cursor || member.align != align || member.size != size {
            return Err(wire_error(format!("uniform member {} has inconsistent physical layout", member.name)));
        }
        cursor = cursor.checked_add(size as u64).ok_or_else(|| wire_error("uniform layout overflows"))?;
    }
    if round_up(cursor, 16)? != block.size as u64 {
        return Err(wire_error(format!("uniform block {} size does not match members", block.struct_name)));
    }
    Ok(())
}

fn validate_array_block(block: &WgslArrayBlock) -> Result<(), AbiError> {
    let mut cursor = 0u64;
    for member in &block.members {
        identifier(&member.name, "uniform array member name")?;
        if block.members.iter().filter(|candidate| candidate.name == member.name).count() != 1 {
            return Err(wire_error(format!("uniform array member {} is repeated", member.name)));
        }
        validate_array_kind(member.elem)?;
        if member.capacity == 0 || member.stride != 16 {
            return Err(wire_error("uniform array capacity or stride is invalid"));
        }
        if member.offset as u64 != cursor {
            return Err(wire_error(format!("uniform array member {} has an inconsistent offset", member.name)));
        }
        let bytes = (member.capacity as u64)
            .checked_mul(member.stride as u64)
            .ok_or_else(|| wire_error("uniform array member size overflows"))?;
        cursor = cursor.checked_add(bytes).ok_or_else(|| wire_error("uniform array block size overflows"))?;
    }
    if cursor != block.size as u64 {
        return Err(wire_error("uniform array block size does not match members"));
    }
    Ok(())
}

fn validate_binding_order(
    ordinary: &[WgslUniformBlock],
    arrays: &[WgslArrayBlock],
    start: u32,
) -> Result<(), AbiError> {
    for (index, block) in ordinary.iter().enumerate() {
        if block.binding != start + index as u32 {
            return Err(wire_error("uniform blocks are not in binding order"));
        }
    }
    let array_start = start
        .checked_add(ordinary.len() as u32)
        .ok_or_else(|| wire_error("uniform binding base overflows"))?;
    for (index, block) in arrays.iter().enumerate() {
        if block.binding != array_start + index as u32 {
            return Err(wire_error("uniform array blocks are not in binding order"));
        }
    }
    Ok(())
}

fn empty_uniform_block() -> WgslUniformBlock {
    WgslUniformBlock {
        role: "fullscreen".into(),
        struct_name: "FullscreenU".into(),
        binding: 0,
        size: 0,
        visibility: WgslVisibility { vertex: false, fragment: false },
        members: Vec::new(),
    }
}

fn uniform_layout(kind: u32) -> Result<(u32, u32), AbiError> {
    match kind {
        0 | 4 | 5 => Ok((4, 4)),
        1 => Ok((8, 8)),
        2 => Ok((16, 12)),
        3 => Ok((16, 16)),
        6 => Ok((16, 64)),
        _ => Err(wire_error(format!("unsupported uniform kind {kind}"))),
    }
}

fn validate_array_kind(kind: u32) -> Result<(), AbiError> {
    match kind {
        0 | 1 | 3 | 4 => Ok(()),
        _ => Err(wire_error(format!("unsupported uniform array element kind {kind}"))),
    }
}

fn validate_kind(kind: u32, what: &str) -> Result<(), AbiError> {
    match kind {
        0..=6 => Ok(()),
        _ => Err(wire_error(format!("{what} {kind} is unsupported"))),
    }
}

fn validate_fragment_output_kind(kind: u32) -> Result<(), AbiError> {
    match kind {
        3 | 4 => Ok(()),
        _ => Err(wire_error(format!("fragment output kind {kind} is unsupported"))),
    }
}

fn round_up(value: u64, alignment: u64) -> Result<u64, AbiError> {
    let adjusted = value
        .checked_add(alignment - 1)
        .ok_or_else(|| wire_error("layout rounding overflows"))?;
    Ok(adjusted / alignment * alignment)
}

fn push_line(output: &mut String, args: fmt::Arguments<'_>) {
    use fmt::Write as _;
    output.write_fmt(args).expect("String formatting cannot fail");
    output.push('\n');
}

fn wire_error(message: impl Into<String>) -> AbiError {
    AbiError::new(message)
}

fn next_line<'a>(lines: &mut impl Iterator<Item = &'a str>, what: &str) -> Result<&'a str, AbiError> {
    lines.next().ok_or_else(|| wire_error(format!("missing {what} record")))
}

fn next_line_optional<'a>(lines: &mut impl Iterator<Item = &'a str>) -> Result<Option<&'a str>, AbiError> {
    Ok(lines.next())
}

fn fields<'a>(line: &'a str, tag: &str, count: usize) -> Result<Vec<&'a str>, AbiError> {
    let line = line.strip_suffix('\n').unwrap_or(line);
    let fields: Vec<_> = line.split_whitespace().collect();
    if fields.len() != count || fields.first().copied() != Some(tag) {
        return Err(wire_error(format!("malformed {tag} record {line}")));
    }
    Ok(fields)
}

fn parse_u32(value: &str, what: &str) -> Result<u32, AbiError> {
    let value = value
        .parse::<u64>()
        .map_err(|_| wire_error(format!("{what} {value} is not a non-negative integer")))?;
    u32::try_from(value).map_err(|_| wire_error(format!("{what} {value} does not fit in u32")))
}

fn parse_u64(value: &str, what: &str) -> Result<u64, AbiError> {
    value
        .parse()
        .map_err(|_| wire_error(format!("{what} {value} is not a u64")))
}

fn parse_count(value: &str, what: &str) -> Result<usize, AbiError> {
    let wide = parse_u64(value, what)?;
    if wide > MAX_RECORDS {
        return Err(wire_error(format!("{what} {wide} exceeds the record budget")));
    }
    usize::try_from(wide).map_err(|_| wire_error(format!("{what} {wide} does not fit in usize")))
}

fn parse_kind(value: &str, what: &str) -> Result<u32, AbiError> {
    let kind = parse_u32(value, what)?;
    validate_kind(kind, what)?;
    Ok(kind)
}

fn attr_kind(value: &str) -> Result<(), AbiError> {
    match value {
        "f32" | "f32x2" | "f32x3" | "f32x4" | "i32" => Ok(()),
        _ => Err(wire_error(format!("unknown vertex attribute kind {value}"))),
    }
}

fn stage_name(value: &str) -> Result<(), AbiError> {
    match value {
        "vertex" | "fragment" => Ok(()),
        _ => Err(wire_error(format!("unknown shader stage {value}"))),
    }
}

fn texture_kind(value: &str) -> Result<WgslTextureKind, AbiError> {
    match value {
        "float" => Ok(WgslTextureKind::Float),
        "int" => Ok(WgslTextureKind::Int),
        _ => Err(wire_error(format!("unknown texture kind {value}"))),
    }
}

fn texture_kind_wire(kind: WgslTextureKind) -> &'static str {
    match kind {
        WgslTextureKind::Float => "float",
        WgslTextureKind::Int => "int",
    }
}

fn visibility_wire(visibility: WgslVisibility) -> &'static str {
    match (visibility.vertex, visibility.fragment) {
        (false, false) => "none",
        (true, false) => "v",
        (false, true) => "f",
        (true, true) => "vf",
    }
}

fn parse_visibility(value: &str) -> Result<WgslVisibility, AbiError> {
    match value {
        "none" => Ok(WgslVisibility { vertex: false, fragment: false }),
        "v" => Ok(WgslVisibility { vertex: true, fragment: false }),
        "f" => Ok(WgslVisibility { vertex: false, fragment: true }),
        "vf" => Ok(WgslVisibility { vertex: true, fragment: true }),
        _ => Err(wire_error(format!("unknown visibility {value}"))),
    }
}

fn identifier(value: &str, what: &str) -> Result<String, AbiError> {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return Err(wire_error(format!("{what} is empty")));
    };
    if !(first == '_' || first.is_ascii_alphabetic())
        || !chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
    {
        return Err(wire_error(format!("{what} {value} is not an identifier")));
    }
    Ok(value.to_string())
}

fn wire_checksum(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fullscreen() -> WgslAbi {
        WgslAbi {
            semantic_fingerprint: 7,
            bind_group: 0,
            geometry: WgslGeometryKind::Fullscreen,
            vertex_entry: "fullscreen_vertex".into(),
            fragment_entry: "fragment_main".into(),
            vertex_inputs: Vec::new(),
            varyings: Vec::new(),
            fullscreen_vertex: WgslFullscreenVertex {
                topology: WgslPrimitiveTopology::TriangleStrip,
                vertex_count: 4,
                varying_location: 0,
            },
            fragment: WgslFragmentIo { uv_location: 0 },
            fragment_outputs: vec![WgslFragmentOutput {
                name: "color".into(),
                location: 0,
                kind: 3,
            }],
            fullscreen_uniform: WgslUniformBlock {
                role: "fullscreen".into(),
                struct_name: "FullscreenU".into(),
                binding: 0,
                size: 16,
                visibility: WgslVisibility { vertex: true, fragment: false },
                members: vec![
                    WgslUniformMember { name: "uv0".into(), kind: 1, offset: 0, align: 8, size: 8 },
                    WgslUniformMember { name: "uv1".into(), kind: 1, offset: 8, align: 8, size: 8 },
                ],
            },
            uniform_blocks: Vec::new(),
            array_blocks: Vec::new(),
            textures: Vec::new(),
        }
    }

    fn triangles() -> WgslAbi {
        WgslAbi {
            semantic_fingerprint: 11,
            bind_group: 0,
            geometry: WgslGeometryKind::Triangles,
            vertex_entry: "vertex_main".into(),
            fragment_entry: "fragment_main".into(),
            vertex_inputs: vec![WgslVertexInput {
                index: 0,
                struct_name: "Vertex".into(),
                rate: "vertex".into(),
                attrs: vec![WgslVertexAttribute {
                    location: 0,
                    name: "pos".into(),
                    kind: "f32x2".into(),
                }],
            }],
            varyings: vec![WgslVarying {
                location: 0,
                name: "uv".into(),
                kind: 1,
                flat: false,
            }],
            fullscreen_vertex: WgslFullscreenVertex {
                topology: WgslPrimitiveTopology::TriangleStrip,
                vertex_count: 0,
                varying_location: 0,
            },
            fragment: WgslFragmentIo { uv_location: 0 },
            fragment_outputs: vec![
                WgslFragmentOutput { name: "FragColor".into(), location: 0, kind: 3 },
                WgslFragmentOutput { name: "FragId".into(), location: 1, kind: 4 },
            ],
            fullscreen_uniform: empty_uniform_block(),
            uniform_blocks: vec![WgslUniformBlock {
                role: "engine".into(),
                struct_name: "EngineU".into(),
                binding: 0,
                size: 16,
                visibility: WgslVisibility { vertex: true, fragment: true },
                members: vec![WgslUniformMember {
                    name: "projection".into(),
                    kind: 3,
                    offset: 0,
                    align: 16,
                    size: 16,
                }],
            }],
            array_blocks: vec![WgslArrayBlock {
                stage: "fragment".into(),
                logical_name: "Frag".into(),
                struct_name: "Frag_Arrays".into(),
                binding: 1,
                size: 32,
                visibility: WgslVisibility { vertex: false, fragment: true },
                members: vec![WgslArrayMember {
                    name: "lights".into(),
                    elem: 0,
                    capacity: 2,
                    offset: 0,
                    stride: 16,
                }],
            }],
            textures: vec![WgslTextureBinding {
                slot: 0,
                name: "scene".into(),
                kind: WgslTextureKind::Float,
                texture_binding: 2,
                sampler_binding: 3,
                visibility: WgslVisibility { vertex: false, fragment: true },
            }],
        }
    }

    fn replace_once(source: &str, old: &str, new: &str) -> String {
        assert_eq!(source.matches(old).count(), 1, "mutation subject is unique");
        source.replacen(old, new, 1)
    }

    fn rechecksum(wire: &str) -> String {
        let (version, rest) = wire.split_once('\n').expect("version");
        let (_, payload) = rest.split_once('\n').expect("checksum");
        let mut hash = 0xcbf29ce484222325u64;
        for byte in payload.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        format!("{version}\nchecksum {hash}\n{payload}")
    }

    #[test]
    fn v2_round_trips_and_v1_is_explicitly_rejected() {
        let wire = fullscreen().serialize().expect("serialize");
        assert!(wire.starts_with("WGSL_ABI_V2\n"));
        assert_eq!(WgslAbi::parse(&wire).expect("parse"), fullscreen());
        assert!(WgslAbi::parse(&wire.replacen("WGSL_ABI_V2", "WGSL_ABI_V1", 1)).is_err());
    }

    #[test]
    fn checksum_and_trailing_records_are_rejected() {
        let wire = fullscreen().serialize().expect("serialize");
        assert!(WgslAbi::parse(&rechecksum(&(wire.clone() + "unknown nope\n"))).is_err());
        assert!(WgslAbi::parse(&rechecksum(&(wire.clone() + "\n"))).is_err());
        let missing = wire.replace("fullscreen_fragment 0\n", "");
        assert!(WgslAbi::parse(&rechecksum(&missing)).is_err());
        let reordered = wire.replace("semantic 7\ngroup 0", "group 0\nsemantic 7");
        assert!(WgslAbi::parse(&rechecksum(&reordered)).is_err());
    }

    #[test]
    fn physical_v2_mutations_are_rejected_after_rechecksum() {
        let wire = triangles().serialize().expect("serialize");
        assert!(WgslAbi::parse(&wire.replacen("WGSL_ABI_V2", "WGSL_ABI_V1", 1)).is_err());
        let (version, rest) = wire.split_once('\n').expect("version");
        let (checksum, payload) = rest.split_once('\n').expect("checksum");
        let checksum = checksum
            .strip_prefix("checksum ")
            .expect("checksum label")
            .parse::<u64>()
            .expect("checksum value");
        let wrong_checksum = format!("{version}\nchecksum {}\n{payload}", checksum.wrapping_add(1));
        assert!(WgslAbi::parse(&wrong_checksum).is_err());

        let mutations = [
            (
                "output 0 FragColor 3\noutput 1 FragId 4",
                "output 1 FragId 4\noutput 0 FragColor 3",
            ),
            ("output 1 FragId 4", "output 2 FragId 4"),
            ("output 0 FragColor 3", "output 0 FragColor 0"),
            (
                "array_block fragment Frag Frag_Arrays 1 32 f 1",
                "array_block fragment Frag Frag_Arrays 2 32 f 1",
            ),
            ("array_member lights 0 2 0 16", "array_member lights 0 2 16 16"),
            ("array_member lights 0 2 0 16", "array_member lights 0 2 0 8"),
            ("array_member lights 0 2 0 16", "array_member lights 0 3 0 16"),
        ];
        for (old, new) in mutations {
            let mutated = rechecksum(&replace_once(&wire, old, new));
            assert!(WgslAbi::parse(&mutated).is_err(), "mutation `{old}` was accepted");
        }
    }
}
