//! A minimal ZIP writer for `rock web --itch`.
//!
//! Only what a distribution archive needs: STORE and DEFLATE entries, a central
//! directory, and an end-of-central-directory record. No ZIP64, no encryption,
//! no directory entries (a zip reader creates parent directories from the entry
//! paths, and itch.io only requires `index.html` at the archive root).
//!
//! The format limits are ENFORCED rather than wrapped: past 4 GiB or 65535
//! entries the 32-bit fields in the central directory silently truncate, and a
//! truncated header produces an archive that some readers open and others
//! reject. Those cases reject here instead (`docs/js-deployment-plan.md`).

use crate::Result;
use std::io::Write;

/// 4 GiB - 1: the largest value the 32-bit size and offset fields can hold.
const MAX_ZIP32_BYTES: u64 = u32::MAX as u64;
/// The 16-bit entry count in the end-of-central-directory record.
const MAX_ZIP32_ENTRIES: usize = u16::MAX as usize;

const STORE: u16 = 0;
const DEFLATE: u16 = 8;
/// Unix `0o100644` (regular file, rw-r--r--) in the high half, where the spec
/// puts host-specific mode bits. Without it some extractors create entries with
/// no read permission.
const EXTERNAL_ATTRS: u32 = 0o100644 << 16;

struct Entry {
    name: String,
    crc: u32,
    compressed: u32,
    uncompressed: u32,
    offset: u32,
    method: u16,
}

pub struct ZipWriter {
    buf: Vec<u8>,
    entries: Vec<Entry>,
}

impl ZipWriter {
    pub fn new() -> Self {
        Self { buf: Vec::new(), entries: Vec::new() }
    }

    /// Add one file. `name` is the archive-relative path with `/` separators.
    pub fn add(&mut self, name: &str, data: &[u8]) -> Result<()> {
        if self.entries.len() >= MAX_ZIP32_ENTRIES {
            anyhow::bail!(
                "web --itch: {} entries exceeds the {} a non-ZIP64 archive can index",
                self.entries.len() + 1,
                MAX_ZIP32_ENTRIES
            );
        }
        if data.len() as u64 > MAX_ZIP32_BYTES {
            anyhow::bail!(
                "web --itch: `{name}` is {} bytes, past the {} a non-ZIP64 entry can record",
                data.len(),
                MAX_ZIP32_BYTES
            );
        }
        let offset = self.buf.len() as u64;
        if offset > MAX_ZIP32_BYTES {
            anyhow::bail!(
                "web --itch: archive passed {MAX_ZIP32_BYTES} bytes at `{name}`; \
                 a non-ZIP64 central directory cannot address it"
            );
        }

        let crc = crc32(data);
        let deflated = deflate(data)?;
        let (method, payload) = if deflated.len() < data.len() {
            (DEFLATE, deflated.as_slice())
        } else {
            (STORE, data)
        };

        let name_bytes = name.as_bytes();
        self.buf.extend_from_slice(&0x0403_4b50u32.to_le_bytes()); // local header
        self.buf.extend_from_slice(&20u16.to_le_bytes()); // version needed
        self.buf.extend_from_slice(&0u16.to_le_bytes()); // flags
        self.buf.extend_from_slice(&method.to_le_bytes());
        self.buf.extend_from_slice(&0u16.to_le_bytes()); // mod time
        self.buf.extend_from_slice(&0u16.to_le_bytes()); // mod date
        self.buf.extend_from_slice(&crc.to_le_bytes());
        self.buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        self.buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
        self.buf.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        self.buf.extend_from_slice(&0u16.to_le_bytes()); // extra len
        self.buf.extend_from_slice(name_bytes);
        self.buf.extend_from_slice(payload);

        self.entries.push(Entry {
            name: name.to_string(),
            crc,
            compressed: payload.len() as u32,
            uncompressed: data.len() as u32,
            offset: offset as u32,
            method,
        });
        Ok(())
    }

    /// The complete archive bytes: everything added, then the central directory.
    pub fn finish(mut self) -> Result<Vec<u8>> {
        let cd_start = self.buf.len() as u64;
        if cd_start > MAX_ZIP32_BYTES {
            anyhow::bail!(
                "web --itch: payload is {cd_start} bytes; a non-ZIP64 central \
                 directory offset cannot address it"
            );
        }
        for e in &self.entries {
            let name = e.name.as_bytes();
            self.buf.extend_from_slice(&0x0201_4b50u32.to_le_bytes()); // central header
            self.buf.extend_from_slice(&20u16.to_le_bytes()); // version made by
            self.buf.extend_from_slice(&20u16.to_le_bytes()); // version needed
            self.buf.extend_from_slice(&0u16.to_le_bytes()); // flags
            self.buf.extend_from_slice(&e.method.to_le_bytes());
            self.buf.extend_from_slice(&0u16.to_le_bytes()); // mod time
            self.buf.extend_from_slice(&0u16.to_le_bytes()); // mod date
            self.buf.extend_from_slice(&e.crc.to_le_bytes());
            self.buf.extend_from_slice(&e.compressed.to_le_bytes());
            self.buf.extend_from_slice(&e.uncompressed.to_le_bytes());
            self.buf.extend_from_slice(&(name.len() as u16).to_le_bytes());
            self.buf.extend_from_slice(&0u16.to_le_bytes()); // extra len
            self.buf.extend_from_slice(&0u16.to_le_bytes()); // comment len
            self.buf.extend_from_slice(&0u16.to_le_bytes()); // disk number
            self.buf.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
            self.buf.extend_from_slice(&EXTERNAL_ATTRS.to_le_bytes());
            self.buf.extend_from_slice(&e.offset.to_le_bytes());
            self.buf.extend_from_slice(name);
        }
        let cd_size = self.buf.len() as u64 - cd_start;
        let count = self.entries.len() as u16;
        self.buf.extend_from_slice(&0x0605_4b50u32.to_le_bytes()); // EOCD
        self.buf.extend_from_slice(&0u16.to_le_bytes()); // this disk
        self.buf.extend_from_slice(&0u16.to_le_bytes()); // cd start disk
        self.buf.extend_from_slice(&count.to_le_bytes());
        self.buf.extend_from_slice(&count.to_le_bytes());
        self.buf.extend_from_slice(&(cd_size as u32).to_le_bytes());
        self.buf.extend_from_slice(&(cd_start as u32).to_le_bytes());
        self.buf.extend_from_slice(&0u16.to_le_bytes()); // comment len
        Ok(self.buf)
    }
}

fn deflate(data: &[u8]) -> Result<Vec<u8>> {
    let mut enc =
        flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::best());
    enc.write_all(data)?;
    Ok(enc.finish()?)
}

fn crc32(data: &[u8]) -> u32 {
    let mut h = flate2::Crc::new();
    h.update(data);
    h.sum()
}
