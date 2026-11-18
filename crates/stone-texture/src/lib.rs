//! The single-level 2D BC1 subset of legacy DDS used for direct GPU uploads.
pub const HEADER_LEN: usize = 128;

pub fn bc1_bytes(width: u32, height: u32) -> u64 {
    u64::from(width.div_ceil(4)) * u64::from(height.div_ceil(4)) * 8
}

pub fn bc1_dimensions(file: &[u8]) -> Option<(u32, u32)> {
    let h: &[u8; HEADER_LEN] = file.get(..HEADER_LEN)?.try_into().ok()?;
    let word = |i: usize| u32::from_le_bytes(h[i..i + 4].try_into().unwrap());
    let (w, hgt) = (word(16), word(12));
    // FOURCC DXT1, ordinary 2D surface, no extra faces, slices or levels.
    if &h[..4] != b"DDS "
        || word(4) != 124
        || word(76) != 32
        || word(80) & 4 == 0
        || &h[84..88] != b"DXT1"
        || word(112) != 0
        || word(24) > 1
        || word(28) > 1
        || w == 0
        || hgt == 0
        || w > i32::MAX as u32
        || hgt > i32::MAX as u32
        || bc1_bytes(w, hgt) > i32::MAX as u64
    {
        return None;
    }
    Some((w, hgt))
}

pub fn bc1_blocks(file: &[u8]) -> Option<((u32, u32), &[u8])> {
    let (w, h) = bc1_dimensions(file)?;
    let blocks = &file[HEADER_LEN..];
    (blocks.len() as u64 == bc1_bytes(w, h)).then_some(((w, h), blocks))
}

pub fn bc1_header(width: u32, height: u32) -> Option<[u8; HEADER_LEN]> {
    let len = u32::try_from(bc1_bytes(width, height)).ok()?;
    let mut header = [0; HEADER_LEN];
    header[..4].copy_from_slice(b"DDS ");
    header[84..88].copy_from_slice(b"DXT1");
    for (offset, value) in [
        (4, 124),
        (8, 0x81007),
        (12, height),
        (16, width),
        (20, len),
        (76, 32),
        (80, 4),
        (108, 0x1000),
    ] {
        header[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    bc1_dimensions(&header)?;
    Some(header)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn odd_extents_require_complete_blocks_and_single_image() {
        let mut dds = bc1_header(5, 3).unwrap().to_vec();
        dds.extend([0; 16]);
        assert_eq!(bc1_blocks(&dds).unwrap().0, (5, 3));
        assert!(bc1_blocks(&dds[..143]).is_none());
        dds.push(0);
        assert!(bc1_blocks(&dds).is_none());
        dds.pop();
        dds[112] = 1;
        assert!(bc1_blocks(&dds).is_none());
    }
}
