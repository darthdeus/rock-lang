//! The single-level 2D block-compressed subset of legacy DDS used for direct GPU uploads.
pub const HEADER_LEN: usize = 128;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BlockFormat {
    Bc1,
    Bc5,
}

impl BlockFormat {
    fn tag(self) -> &'static [u8; 4] {
        match self {
            Self::Bc1 => b"DXT1",
            Self::Bc5 => b"ATI2",
        }
    }
    pub fn bytes(self, width: u32, height: u32) -> u64 {
        (u64::from(width.div_ceil(4)) * u64::from(height.div_ceil(4))).saturating_mul(match self {
            Self::Bc1 => 8,
            Self::Bc5 => 16,
        })
    }

    pub fn dimensions(self, file: &[u8]) -> Option<(u32, u32)> {
        let h: &[u8; HEADER_LEN] = file.get(..HEADER_LEN)?.try_into().ok()?;
        let word = |i: usize| u32::from_le_bytes(h[i..i + 4].try_into().unwrap());
        let (w, hgt) = (word(16), word(12));
        // Ordinary 2D surface, no extra faces, slices or levels.
        if &h[..4] != b"DDS "
            || word(4) != 124
            || word(76) != 32
            || word(80) & 4 == 0
            || &h[84..88] != self.tag()
            || word(112) != 0
            || word(24) > 1
            || word(28) > 1
            || w == 0
            || hgt == 0
            || w > i32::MAX as u32
            || hgt > i32::MAX as u32
            || self.bytes(w, hgt) > i32::MAX as u64
        {
            return None;
        }
        Some((w, hgt))
    }

    pub fn blocks(self, file: &[u8]) -> Option<((u32, u32), &[u8])> {
        let (w, h) = self.dimensions(file)?;
        let blocks = &file[HEADER_LEN..];
        (blocks.len() as u64 == self.bytes(w, h)).then_some(((w, h), blocks))
    }

    pub fn header(self, width: u32, height: u32) -> Option<[u8; HEADER_LEN]> {
        let len = u32::try_from(self.bytes(width, height)).ok()?;
        let mut header = [0; HEADER_LEN];
        header[..4].copy_from_slice(b"DDS ");
        header[84..88].copy_from_slice(self.tag());
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
        self.dimensions(&header)?;
        Some(header)
    }
}
pub fn bc1_header(w: u32, h: u32) -> Option<[u8; HEADER_LEN]> {
    BlockFormat::Bc1.header(w, h)
}
pub fn bc5_header(w: u32, h: u32) -> Option<[u8; HEADER_LEN]> {
    BlockFormat::Bc5.header(w, h)
}
pub fn dimensions(file: &[u8]) -> Option<(u32, u32)> {
    match file.get(84..88)? {
        b"DXT1" => BlockFormat::Bc1.dimensions(file),
        b"ATI2" => BlockFormat::Bc5.dimensions(file),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn odd_extents_require_complete_blocks_and_single_image() {
        let mut dds = bc1_header(5, 3).unwrap().to_vec();
        dds.extend([0; 16]);
        assert_eq!(BlockFormat::Bc1.blocks(&dds).unwrap().0, (5, 3));
        assert!(BlockFormat::Bc1.blocks(&dds[..143]).is_none());
        dds.push(0);
        assert!(BlockFormat::Bc1.blocks(&dds).is_none());
        dds.pop();
        dds[112] = 1;
        assert!(BlockFormat::Bc1.blocks(&dds).is_none());
    }
}
