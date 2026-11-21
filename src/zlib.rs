//! zlib streams (RFC 1950) around a from-scratch DEFLATE codec (RFC 1951).
//! Inflate handles stored, fixed-Huffman and dynamic-Huffman blocks, which
//! covers everything git clients emit. Deflate emits fixed-Huffman blocks
//! with a small hash-chain match search: good ratios for text blobs, and
//! fast enough that pack building never dominates a request.

// ---------------------------------------------------------------- inflate

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    bits: u32,
    bit_count: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        BitReader { data, pos: 0, bits: 0, bit_count: 0 }
    }

    fn take(&mut self, count: u32) -> Option<u32> {
        while self.bit_count < count {
            if self.pos >= self.data.len() {
                return None;
            }
            self.bits |= (self.data[self.pos] as u32) << self.bit_count;
            self.pos += 1;
            self.bit_count += 8;
        }
        let value = self.bits & ((1u32 << count) - 1);
        self.bits >>= count;
        self.bit_count -= count;
        Some(value)
    }

    fn align_byte(&mut self) {
        let drop = self.bit_count % 8;
        self.bits >>= drop;
        self.bit_count -= drop;
    }

    /// Bytes of input the stream occupies: everything pulled from the
    /// buffer except whole bytes still sitting unconsumed in the bit tank.
    fn consumed(&self) -> usize {
        self.pos - (self.bit_count as usize) / 8
    }
}

/// A canonical Huffman decoder built from code lengths.
struct Huffman {
    counts: [u16; 16],
    symbols: Vec<u16>,
}

impl Huffman {
    fn from_lengths(lengths: &[u8]) -> Option<Huffman> {
        let mut counts = [0u16; 16];
        for &len in lengths {
            if len > 15 {
                return None;
            }
            counts[len as usize] += 1;
        }
        counts[0] = 0;
        let mut offsets = [0u16; 16];
        for len in 1..16 {
            offsets[len] = offsets[len - 1] + counts[len - 1];
        }
        let mut symbols = vec![0u16; lengths.iter().filter(|&&l| l > 0).count()];
        for (symbol, &len) in lengths.iter().enumerate() {
            if len > 0 {
                symbols[offsets[len as usize] as usize] = symbol as u16;
                offsets[len as usize] += 1;
            }
        }
        Some(Huffman { counts, symbols })
    }

    fn decode(&self, reader: &mut BitReader) -> Option<u16> {
        let mut code: i32 = 0;
        let mut first: i32 = 0;
        let mut index: i32 = 0;
        for len in 1..16 {
            code |= reader.take(1)? as i32;
            let count = self.counts[len] as i32;
            if code - first < count {
                return self.symbols.get((index + (code - first)) as usize).copied();
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        None
    }
}

const LENGTH_BASE: [usize; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u32; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [usize; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u32; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13,
];

fn inflate_block(
    reader: &mut BitReader,
    literals: &Huffman,
    distances: &Huffman,
    out: &mut Vec<u8>,
) -> Option<()> {
    loop {
        let symbol = literals.decode(reader)?;
        match symbol {
            0..=255 => out.push(symbol as u8),
            256 => return Some(()),
            257..=285 => {
                let li = (symbol - 257) as usize;
                if li >= 29 {
                    return None;
                }
                let length = LENGTH_BASE[li] + reader.take(LENGTH_EXTRA[li])? as usize;
                let dsym = distances.decode(reader)? as usize;
                if dsym >= 30 {
                    return None;
                }
                let dist = DIST_BASE[dsym] + reader.take(DIST_EXTRA[dsym])? as usize;
                if dist == 0 || dist > out.len() {
                    return None;
                }
                let start = out.len() - dist;
                for i in 0..length {
                    let byte = out[start + i];
                    out.push(byte);
                }
            }
            _ => return None,
        }
    }
}

fn fixed_tables() -> (Huffman, Huffman) {
    let mut lit_lengths = [0u8; 288];
    for (i, len) in lit_lengths.iter_mut().enumerate() {
        *len = match i {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    let dist_lengths = [5u8; 30];
    (
        Huffman::from_lengths(&lit_lengths).expect("fixed literal table"),
        Huffman::from_lengths(&dist_lengths).expect("fixed distance table"),
    )
}

fn dynamic_tables(reader: &mut BitReader) -> Option<(Huffman, Huffman)> {
    let hlit = reader.take(5)? as usize + 257;
    let hdist = reader.take(5)? as usize + 1;
    let hclen = reader.take(4)? as usize + 4;
    const ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];
    let mut code_lengths = [0u8; 19];
    for i in 0..hclen {
        code_lengths[ORDER[i]] = reader.take(3)? as u8;
    }
    let code_table = Huffman::from_lengths(&code_lengths)?;
    let total = hlit + hdist;
    let mut lengths = vec![0u8; total];
    let mut i = 0;
    while i < total {
        let symbol = code_table.decode(reader)?;
        match symbol {
            0..=15 => {
                lengths[i] = symbol as u8;
                i += 1;
            }
            16 => {
                if i == 0 {
                    return None;
                }
                let prev = lengths[i - 1];
                let repeat = 3 + reader.take(2)? as usize;
                for _ in 0..repeat {
                    if i >= total {
                        return None;
                    }
                    lengths[i] = prev;
                    i += 1;
                }
            }
            17 => {
                i += 3 + reader.take(3)? as usize;
                if i > total {
                    return None;
                }
            }
            18 => {
                i += 11 + reader.take(7)? as usize;
                if i > total {
                    return None;
                }
            }
            _ => return None,
        }
    }
    let literals = Huffman::from_lengths(&lengths[..hlit])?;
    let distances = Huffman::from_lengths(&lengths[hlit..])?;
    Some((literals, distances))
}

/// Inflate the DEFLATE payload starting at `data[0]`; returns the output
/// and the payload's byte length (rounded up from the final bit position).
fn inflate_deflate(data: &[u8]) -> Option<(Vec<u8>, usize)> {
    let mut reader = BitReader::new(data);
    let mut out: Vec<u8> = Vec::with_capacity(256);
    loop {
        let last = reader.take(1)? == 1;
        match reader.take(2)? {
            0 => {
                reader.align_byte();
                if reader.pos + 4 > data.len() {
                    return None;
                }
                let len = u16::from_le_bytes([data[reader.pos], data[reader.pos + 1]]) as usize;
                let nlen = u16::from_le_bytes([data[reader.pos + 2], data[reader.pos + 3]]) as usize;
                if len != (!nlen & 0xffff) {
                    return None;
                }
                reader.pos += 4;
                if reader.pos + len > data.len() {
                    return None;
                }
                out.extend_from_slice(&data[reader.pos..reader.pos + len]);
                reader.pos += len;
                reader.bits = 0;
                reader.bit_count = 0;
            }
            1 => {
                let (literals, distances) = fixed_tables();
                inflate_block(&mut reader, &literals, &distances, &mut out)?;
            }
            2 => {
                let (literals, distances) = dynamic_tables(&mut reader)?;
                inflate_block(&mut reader, &literals, &distances, &mut out)?;
            }
            _ => return None,
        }
        if last {
            return Some((out, reader.consumed()));
        }
    }
}

/// Inflate a zlib stream. Returns the payload and the total number of
/// input bytes the stream occupies (packs store objects back to back, so
/// the caller needs to know where this stream ended).
pub fn inflate(data: &[u8]) -> Option<(Vec<u8>, usize)> {
    if data.len() < 6 {
        return None;
    }
    let (cmf, flg) = (data[0], data[1]);
    if cmf & 0x0f != 8 || ((cmf as u32) << 8 | flg as u32) % 31 != 0 {
        return None;
    }
    if flg & 0x20 != 0 {
        return None; // preset dictionaries are never used by git
    }
    let (out, deflate_len) = inflate_deflate(&data[2..])?;
    let consumed = 2 + deflate_len + 4; // header + deflate + adler32
    if consumed > data.len() {
        return None;
    }
    Some((out, consumed))
}

/// Inflate when the caller already holds exactly one stream.
pub fn inflate_exact(data: &[u8]) -> Option<Vec<u8>> {
    inflate(data).map(|(out, _)| out)
}

// ---------------------------------------------------------------- deflate

struct BitWriter {
    out: Vec<u8>,
    bits: u32,
    bit_count: u32,
}

impl BitWriter {
    fn new() -> Self {
        BitWriter { out: Vec::new(), bits: 0, bit_count: 0 }
    }

    fn push_bits(&mut self, value: u32, count: u32) {
        self.bits |= value << self.bit_count;
        self.bit_count += count;
        while self.bit_count >= 8 {
            self.out.push((self.bits & 0xff) as u8);
            self.bits >>= 8;
            self.bit_count -= 8;
        }
    }

    /// Huffman codes are emitted most-significant-bit first.
    fn push_code(&mut self, code: u32, count: u32) {
        let mut reversed = 0u32;
        for i in 0..count {
            if code & (1 << i) != 0 {
                reversed |= 1 << (count - 1 - i);
            }
        }
        self.push_bits(reversed, count);
    }

    fn finish(mut self) -> Vec<u8> {
        if self.bit_count > 0 {
            self.out.push((self.bits & 0xff) as u8);
        }
        self.out
    }
}

/// Canonical code for a symbol in the fixed literal/length alphabet.
fn fixed_literal_code(symbol: u16) -> (u32, u32) {
    match symbol {
        0..=143 => (0x30 + symbol as u32, 8),
        144..=255 => (0x190 + (symbol as u32 - 144), 9),
        256..=279 => (symbol as u32 - 256, 7),
        _ => (0xC0 + (symbol as u32 - 280), 8),
    }
}

fn length_symbol(length: usize) -> (u16, u32, u32) {
    for (i, &base) in LENGTH_BASE.iter().enumerate() {
        let extra = LENGTH_EXTRA[i];
        let span = if extra == 0 { 1usize } else { 1usize << extra };
        if length >= base && length < base + span {
            return (257 + i as u16, (length - base) as u32, extra);
        }
    }
    (285, 0, 0)
}

fn dist_symbol(dist: usize) -> (u16, u32, u32) {
    for (i, &base) in DIST_BASE.iter().enumerate() {
        let extra = DIST_EXTRA[i];
        let span = if extra == 0 { 1usize } else { 1usize << extra };
        if dist >= base && dist < base + span {
            return (i as u16, (dist - base) as u32, extra);
        }
    }
    (29, 0, 0)
}

fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in data.chunks(5552) {
        for &byte in chunk {
            a += byte as u32;
            b += a;
        }
        a %= MOD;
        b %= MOD;
    }
    (b << 16) | a
}

/// Compress into a zlib stream: fixed-Huffman blocks of at most 64 KiB
/// with a greedy hash-chain match search inside each block.
pub fn deflate(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() / 2 + 64);
    out.push(0x78);
    out.push(0x9c);
    if data.is_empty() {
        let mut writer = BitWriter::new();
        writer.push_bits(1, 1);
        writer.push_bits(1, 2);
        let (code, bits) = fixed_literal_code(256);
        writer.push_code(code, bits);
        out.extend_from_slice(&writer.finish());
    } else {
        let chunks: Vec<&[u8]> = data.chunks(65535).collect();
        for (index, chunk) in chunks.iter().enumerate() {
            out.extend_from_slice(&deflate_block(chunk, index + 1 == chunks.len()));
        }
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn deflate_block(data: &[u8], last: bool) -> Vec<u8> {
    let mut writer = BitWriter::new();
    writer.push_bits(if last { 1 } else { 0 }, 1);
    writer.push_bits(1, 2); // fixed Huffman block type
    const HASH_BITS: u32 = 15;
    const HASH_SIZE: usize = 1 << HASH_BITS;
    let mut head = vec![-1i32; HASH_SIZE];
    let mut prev = vec![-1i32; data.len()];
    let hash_at = |i: usize| -> usize {
        let v = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8) | data[i + 2] as u32;
        (v.wrapping_mul(0x9e3779b1) >> (32 - HASH_BITS)) as usize
    };
    let mut i = 0usize;
    while i < data.len() {
        let mut best_len = 0usize;
        let mut best_dist = 0usize;
        if i + 3 <= data.len() {
            let h = hash_at(i);
            let mut candidate = head[h];
            let mut chain = 0;
            while candidate >= 0 && chain < 24 {
                let c = candidate as usize;
                if i - c > 32768 {
                    break;
                }
                let mut len = 0usize;
                while i + len < data.len() && len < 258 && data[c + len] == data[i + len] {
                    len += 1;
                }
                if len >= 3 && len > best_len {
                    best_len = len;
                    best_dist = i - c;
                    if len >= 258 {
                        break;
                    }
                }
                candidate = prev[c];
                chain += 1;
            }
        }
        if best_len >= 3 {
            let (symbol, extra_value, extra_bits) = length_symbol(best_len);
            let (code, bits) = fixed_literal_code(symbol);
            writer.push_code(code, bits);
            if extra_bits > 0 {
                writer.push_bits(extra_value, extra_bits);
            }
            let (dsymbol, dextra_value, dextra_bits) = dist_symbol(best_dist);
            writer.push_code(dsymbol as u32, 5);
            if dextra_bits > 0 {
                writer.push_bits(dextra_value, dextra_bits);
            }
            let end = i + best_len;
            while i < end {
                if i + 3 <= data.len() {
                    let h = hash_at(i);
                    prev[i] = head[h];
                    head[h] = i as i32;
                }
                i += 1;
            }
        } else {
            let (code, bits) = fixed_literal_code(data[i] as u16);
            writer.push_code(code, bits);
            if i + 3 <= data.len() {
                let h = hash_at(i);
                prev[i] = head[h];
                head[h] = i as i32;
            }
            i += 1;
        }
    }
    let (code, bits) = fixed_literal_code(256);
    writer.push_code(code, bits);
    writer.finish()
}
