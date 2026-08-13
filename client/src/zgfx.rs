use anyhow::{bail, Result};

const DESCRIPTOR_SINGLE: u8 = 0xE0;
const DESCRIPTOR_MULTIPART: u8 = 0xE1;
const PACKET_COMPRESSED: u8 = 0x20;

/// Size of the shared LZ77-style match history, per MS-RDPEGFX §2.2.5.3 ("Maximum match
/// distance / minimum history size: 2,500,000 bytes"). One ring buffer per channel
/// lifetime — never reset in practice (matches FreeRDP: `zgfx_context_reset` exists but is
/// only ever called from context creation).
const HISTORY_SIZE: usize = 2_500_000;

struct TokenRow {
    len: u8,
    code: u16,
    value_bits: u8,
    is_match: bool,
    value_base: u32,
}

/// The RDP 8.0 bulk-compression token table (MS-RDPEGFX §3.1.9.1.2 sample code /
/// FreeRDP's `ZGFX_TOKEN_TABLE`). A prefix-free code: literal tokens (`is_match=false`)
/// yield an output byte (`value_base` directly if `value_bits==0`, else
/// `value_base + extra_bits`); match tokens yield a match distance the same way, with
/// `distance==0` meaning "unencoded run" rather than a real match (see `decompress_segment`).
#[rustfmt::skip]
const TOKEN_TABLE: &[TokenRow] = &[
    TokenRow { len: 1, code: 0b0,          value_bits: 8,  is_match: false, value_base: 0 },
    TokenRow { len: 5, code: 0b10001,      value_bits: 5,  is_match: true,  value_base: 0 },
    TokenRow { len: 5, code: 0b10010,      value_bits: 7,  is_match: true,  value_base: 32 },
    TokenRow { len: 5, code: 0b10011,      value_bits: 9,  is_match: true,  value_base: 160 },
    TokenRow { len: 5, code: 0b10100,      value_bits: 10, is_match: true,  value_base: 672 },
    TokenRow { len: 5, code: 0b10101,      value_bits: 12, is_match: true,  value_base: 1696 },
    TokenRow { len: 5, code: 0b11000,      value_bits: 0,  is_match: false, value_base: 0x00 },
    TokenRow { len: 5, code: 0b11001,      value_bits: 0,  is_match: false, value_base: 0x01 },
    TokenRow { len: 6, code: 0b101100,     value_bits: 14, is_match: true,  value_base: 5792 },
    TokenRow { len: 6, code: 0b101101,     value_bits: 15, is_match: true,  value_base: 22176 },
    TokenRow { len: 6, code: 0b110100,     value_bits: 0,  is_match: false, value_base: 0x02 },
    TokenRow { len: 6, code: 0b110101,     value_bits: 0,  is_match: false, value_base: 0x03 },
    TokenRow { len: 6, code: 0b110110,     value_bits: 0,  is_match: false, value_base: 0xFF },
    TokenRow { len: 7, code: 0b1011100,    value_bits: 18, is_match: true,  value_base: 54944 },
    TokenRow { len: 7, code: 0b1011101,    value_bits: 20, is_match: true,  value_base: 317088 },
    TokenRow { len: 7, code: 0b1101110,    value_bits: 0,  is_match: false, value_base: 0x04 },
    TokenRow { len: 7, code: 0b1101111,    value_bits: 0,  is_match: false, value_base: 0x05 },
    TokenRow { len: 7, code: 0b1110000,    value_bits: 0,  is_match: false, value_base: 0x06 },
    TokenRow { len: 7, code: 0b1110001,    value_bits: 0,  is_match: false, value_base: 0x07 },
    TokenRow { len: 7, code: 0b1110010,    value_bits: 0,  is_match: false, value_base: 0x08 },
    TokenRow { len: 7, code: 0b1110011,    value_bits: 0,  is_match: false, value_base: 0x09 },
    TokenRow { len: 7, code: 0b1110100,    value_bits: 0,  is_match: false, value_base: 0x0A },
    TokenRow { len: 7, code: 0b1110101,    value_bits: 0,  is_match: false, value_base: 0x0B },
    TokenRow { len: 7, code: 0b1110110,    value_bits: 0,  is_match: false, value_base: 0x3A },
    TokenRow { len: 7, code: 0b1110111,    value_bits: 0,  is_match: false, value_base: 0x3B },
    TokenRow { len: 7, code: 0b1111000,    value_bits: 0,  is_match: false, value_base: 0x3C },
    TokenRow { len: 7, code: 0b1111001,    value_bits: 0,  is_match: false, value_base: 0x3D },
    TokenRow { len: 7, code: 0b1111010,    value_bits: 0,  is_match: false, value_base: 0x3E },
    TokenRow { len: 7, code: 0b1111011,    value_bits: 0,  is_match: false, value_base: 0x3F },
    TokenRow { len: 7, code: 0b1111100,    value_bits: 0,  is_match: false, value_base: 0x40 },
    TokenRow { len: 7, code: 0b1111101,    value_bits: 0,  is_match: false, value_base: 0x80 },
    TokenRow { len: 8, code: 0b10111100,   value_bits: 20, is_match: true,  value_base: 1365664 },
    TokenRow { len: 8, code: 0b10111101,   value_bits: 21, is_match: true,  value_base: 2414240 },
    TokenRow { len: 8, code: 0b11111100,   value_bits: 0,  is_match: false, value_base: 0x0C },
    TokenRow { len: 8, code: 0b11111101,   value_bits: 0,  is_match: false, value_base: 0x38 },
    TokenRow { len: 8, code: 0b11111110,   value_bits: 0,  is_match: false, value_base: 0x39 },
    TokenRow { len: 8, code: 0b11111111,   value_bits: 0,  is_match: false, value_base: 0x66 },
    TokenRow { len: 9, code: 0b101111100,  value_bits: 22, is_match: true,  value_base: 4511392 },
    TokenRow { len: 9, code: 0b101111101,  value_bits: 23, is_match: true,  value_base: 8705696 },
    TokenRow { len: 9, code: 0b101111110,  value_bits: 24, is_match: true,  value_base: 17094304 },
];

/// MSB-first bit reader over one ZGFX segment's encoded bytes (trailer byte included —
/// see `new`). Mirrors FreeRDP's `zgfx_GetBits` bit-accumulator exactly.
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    /// Index of the trailer byte; reading never consumes it (padding continues with zeros
    /// once `pos` reaches this point, per the spec's fixed `cBitsRemaining` bit count).
    end: usize,
    bits_current: u32,
    n_bits_current: u32,
    bits_remaining: i64,
}

impl<'a> BitReader<'a> {
    fn new(encoded: &'a [u8]) -> Result<Self> {
        if encoded.is_empty() {
            bail!("ZGFX compressed segment has no trailer byte");
        }
        let end = encoded.len() - 1;
        let last_byte = encoded[end] as i64;
        let total_bits = 8 * end as i64;
        Ok(Self {
            data: encoded,
            pos: 0,
            end,
            bits_current: 0,
            n_bits_current: 0,
            bits_remaining: total_bits - last_byte,
        })
    }

    fn get_bits(&mut self, count: u32) -> u32 {
        while self.n_bits_current < count {
            self.bits_current <<= 8;
            if self.pos < self.end {
                self.bits_current += self.data[self.pos] as u32;
                self.pos += 1;
            }
            self.n_bits_current += 8;
        }
        self.bits_remaining -= count as i64;
        self.n_bits_current -= count;
        let result = self.bits_current >> self.n_bits_current;
        self.bits_current &= (1u32 << self.n_bits_current) - 1;
        result
    }

    /// Discards any unread bits in the current partial byte, per the "unencoded run"
    /// rule: "any bits remaining in the current input byte are ignored, and the
    /// unencoded run will begin on a whole-byte boundary."
    fn byte_align(&mut self) {
        self.bits_remaining -= self.n_bits_current as i64;
        self.n_bits_current = 0;
        self.bits_current = 0;
    }
}

fn decode_token(reader: &mut BitReader) -> Result<&'static TokenRow> {
    let mut code: u16 = 0;
    for len in 1..=9u8 {
        code = (code << 1) | reader.get_bits(1) as u16;
        if let Some(row) = TOKEN_TABLE.iter().find(|r| r.len == len && r.code == code) {
            return Ok(row);
        }
    }
    bail!("invalid ZGFX huffman code (no match within 9 bits)");
}

/// Decodes a match length: `0` -> 3; otherwise a unary run of `1` bits (each doubling the
/// base and widening the extra-bits field by one) terminated by `0`, then that many extra
/// bits added on top. Matches FreeRDP's `zgfx_decompress_segment` length loop exactly.
fn decode_length(reader: &mut BitReader) -> u32 {
    if reader.get_bits(1) == 0 {
        return 3;
    }
    let mut count = 4u32;
    let mut extra = 2u32;
    while reader.get_bits(1) == 1 {
        count *= 2;
        extra += 1;
    }
    count + reader.get_bits(extra)
}

/// Decompresses data received on the MS-RDPEGFX graphics dynamic virtual channel. The
/// server wraps every PDU it sends in this container (RDP 8.0 bulk compression, informally
/// "ZGFX" per FreeRDP); the client does not need to wrap its own outgoing PDUs — real
/// clients (FreeRDP) send those raw, and this server accepts that.
pub struct ZgfxContext {
    history: Vec<u8>,
    /// Monotonically increasing write count; actual buffer index is `index % HISTORY_SIZE`.
    index: usize,
}

impl ZgfxContext {
    pub fn new() -> Self {
        Self { history: vec![0u8; HISTORY_SIZE], index: 0 }
    }

    fn write_byte(&mut self, b: u8, out: &mut Vec<u8>) {
        self.history[self.index % HISTORY_SIZE] = b;
        self.index += 1;
        out.push(b);
    }

    fn copy_match(&mut self, distance: usize, count: usize, out: &mut Vec<u8>) -> Result<()> {
        if distance == 0 || distance > HISTORY_SIZE {
            bail!("invalid ZGFX match distance {distance}");
        }
        for _ in 0..count {
            let src = (self.index + HISTORY_SIZE - distance) % HISTORY_SIZE;
            let b = self.history[src];
            self.write_byte(b, out);
        }
        Ok(())
    }

    fn decompress_segment(&mut self, segment: &[u8]) -> Result<Vec<u8>> {
        let (&flags, data) = segment.split_first().ok_or_else(|| anyhow::anyhow!("empty ZGFX segment"))?;
        let mut out = Vec::new();

        if flags & PACKET_COMPRESSED == 0 {
            for &b in data {
                self.write_byte(b, &mut out);
            }
            return Ok(out);
        }

        let mut reader = BitReader::new(data)?;
        while reader.bits_remaining > 0 {
            let row = decode_token(&mut reader)?;
            if !row.is_match {
                let byte = if row.value_bits > 0 {
                    (row.value_base + reader.get_bits(row.value_bits as u32)) as u8
                } else {
                    row.value_base as u8
                };
                self.write_byte(byte, &mut out);
                continue;
            }

            let distance = (row.value_base + reader.get_bits(row.value_bits as u32)) as usize;
            if distance == 0 {
                // Unencoded run escape (MS-RDPEGFX §3.1.9.1.2): 15-bit count, then raw
                // bytes starting on a whole-byte boundary.
                reader.byte_align();
                let count = reader.get_bits(15) as usize;
                for _ in 0..count {
                    let b = reader.get_bits(8) as u8;
                    self.write_byte(b, &mut out);
                }
            } else {
                let count = decode_length(&mut reader) as usize;
                self.copy_match(distance, count, &mut out)?;
            }
        }
        Ok(out)
    }

    /// Decompresses one full DVC-layer data message (already reassembled from DVC
    /// Data/Data-First fragmentation) into the concatenated raw RDPGFX PDU bytes it
    /// contains.
    pub fn decompress(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        let (&descriptor, rest) = data.split_first().ok_or_else(|| anyhow::anyhow!("empty ZGFX data"))?;
        match descriptor {
            DESCRIPTOR_SINGLE => self.decompress_segment(rest),
            DESCRIPTOR_MULTIPART => {
                if rest.len() < 6 {
                    bail!("ZGFX multipart header truncated");
                }
                let segment_count = u16::from_le_bytes([rest[0], rest[1]]) as usize;
                // uncompressedSize (rest[2..6]) is a hint only; not needed for decoding.
                let mut pos = 6;
                let mut out = Vec::new();
                for _ in 0..segment_count {
                    if pos + 4 > rest.len() {
                        bail!("ZGFX segment size truncated");
                    }
                    let seg_size = u32::from_le_bytes([rest[pos], rest[pos + 1], rest[pos + 2], rest[pos + 3]]) as usize;
                    pos += 4;
                    if pos + seg_size > rest.len() {
                        bail!("ZGFX segment truncated");
                    }
                    out.extend(self.decompress_segment(&rest[pos..pos + seg_size])?);
                    pos += seg_size;
                }
                Ok(out)
            }
            other => bail!("unknown ZGFX descriptor {other:#04x}"),
        }
    }
}
