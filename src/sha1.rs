//! SHA-1 written from the FIPS 180-1 description, plus the object-id
//! helpers the rest of the forge uses (20-byte ids, hex conversion).

/// A git object id: the raw 20 bytes of a SHA-1 digest.
pub type Oid = [u8; 20];

/// The all-zero id git uses for "no object" in ref commands.
pub const ZERO_OID: Oid = [0u8; 20];

/// Streaming SHA-1 state. Feed bytes with `update`, read the digest with
/// `finish`. Blocks are compressed as soon as 64 bytes are buffered.
pub struct Sha1 {
    state: [u32; 5],
    length: u64,
    pending: [u8; 64],
    pending_len: usize,
}

impl Sha1 {
    pub fn new() -> Self {
        Sha1 {
            state: [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476, 0xc3d2e1f0],
            length: 0,
            pending: [0u8; 64],
            pending_len: 0,
        }
    }

    fn compress(state: &mut [u32; 5], block: &[u8]) {
        debug_assert_eq!(block.len(), 64);
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([block[4 * i], block[4 * i + 1], block[4 * i + 2], block[4 * i + 3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) =
            (state[0], state[1], state[2], state[3], state[4]);
        for (i, &word) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5a827999u32),
                20..=39 => (b ^ c ^ d, 0x6ed9eba1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1bbcdc),
                _ => (b ^ c ^ d, 0xca62c1d6),
            };
            let next = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = next;
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.length = self.length.wrapping_add(data.len() as u64);
        if self.pending_len > 0 {
            let take = (64 - self.pending_len).min(data.len());
            self.pending[self.pending_len..self.pending_len + take].copy_from_slice(&data[..take]);
            self.pending_len += take;
            data = &data[take..];
            if self.pending_len == 64 {
                let block = self.pending;
                Self::compress(&mut self.state, &block);
                self.pending_len = 0;
            }
        }
        while data.len() >= 64 {
            Self::compress(&mut self.state, &data[..64]);
            data = &data[64..];
        }
        if !data.is_empty() {
            self.pending[..data.len()].copy_from_slice(data);
            self.pending_len = data.len();
        }
    }

    pub fn finish(mut self) -> Oid {
        let bit_length = self.length.wrapping_mul(8);
        self.update(&[0x80]);
        while self.pending_len != 56 {
            self.update(&[0]);
        }
        // The length append must not count toward the message length, so
        // push it through the block machinery manually.
        self.pending[56..64].copy_from_slice(&bit_length.to_be_bytes());
        let block = self.pending;
        Self::compress(&mut self.state, &block);
        let mut out = [0u8; 20];
        for (i, word) in self.state.iter().enumerate() {
            out[4 * i..4 * i + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

/// One-shot digest of a byte string.
pub fn sha1(data: &[u8]) -> Oid {
    let mut hasher = Sha1::new();
    hasher.update(data);
    hasher.finish()
}

/// The digest git assigns an object: SHA-1 over `"<type> <len>\0"` + body.
pub fn object_id(kind: &str, body: &[u8]) -> Oid {
    let mut hasher = Sha1::new();
    hasher.update(kind.as_bytes());
    hasher.update(b" ");
    hasher.update(body.len().to_string().as_bytes());
    hasher.update(&[0]);
    hasher.update(body);
    hasher.finish()
}

const HEX: &[u8; 16] = b"0123456789abcdef";

/// Lowercase hex for an object id (the only spelling git accepts back).
pub fn oid_hex(oid: &Oid) -> String {
    let mut out = String::with_capacity(40);
    for byte in oid {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Hex for arbitrary bytes (request ids, digests in messages).
pub fn hex_of(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Parse a 40-character hex object id; anything else is not an id.
pub fn parse_oid(text: &str) -> Option<Oid> {
    let bytes = text.as_bytes();
    if bytes.len() != 40 {
        return None;
    }
    let mut out = [0u8; 20];
    for i in 0..20 {
        let hi = hex_value(bytes[2 * i])?;
        let lo = hex_value(bytes[2 * i + 1])?;
        out[i] = (hi << 4) | lo;
    }
    Some(out)
}

/// True when the text is exactly 40 hex digits.
pub fn is_oid_text(text: &str) -> bool {
    parse_oid(text).is_some()
}
