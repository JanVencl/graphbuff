//! Reading gzip files (RFC 1952) for `LOAD CSV`: a DEFLATE decoder (RFC 1951), without
//! dependencies. Blocks are decoded one at a time, keeping the last 32 KiB as the window,
//! so a large file streams. Several members one after the other (as `cat a.gz b.gz`
//! makes) read as one; each member's CRC-32 and length are checked.

use std::io::{self, BufRead, Read};

/// Whether `bytes` start like a gzip file.
pub(crate) fn is_gzip(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0x1f, 0x8b])
}

/// The decompressed bytes of a gzip stream.
pub(crate) struct GzDecoder<R: BufRead> {
    bits: Bits<R>,
    /// Decoded bytes not yet read: `out[pos..]`.
    out: Vec<u8>,
    pos: usize,
    /// The last 32 KiB of output, for back-references.
    window: Vec<u8>,
    state: State,
    crc: u32,
    len: u32,
}

#[derive(PartialEq, Eq)]
enum State {
    Header,
    Blocks,
    Trailer,
    Done,
}

const WINDOW: usize = 32 * 1024;

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("gzip: {}", msg.into()))
}

impl<R: BufRead> GzDecoder<R> {
    pub(crate) fn new(input: R) -> Self {
        GzDecoder {
            bits: Bits { input, bit: 0, nbits: 0 },
            out: Vec::new(),
            pos: 0,
            window: Vec::with_capacity(2 * WINDOW),
            state: State::Header,
            crc: 0,
            len: 0,
        }
    }

    /// Decodes until there is output or the stream ends.
    fn fill(&mut self) -> io::Result<()> {
        self.out.clear();
        self.pos = 0;
        while self.out.is_empty() && self.state != State::Done {
            match self.state {
                State::Header => {
                    self.header()?;
                    self.crc = 0;
                    self.len = 0;
                    self.window.clear();
                    self.state = State::Blocks;
                }
                State::Blocks => {
                    let last = self.block()?;
                    if last {
                        self.state = State::Trailer;
                    }
                }
                State::Trailer => {
                    self.bits.align();
                    let crc = self.bits.u32_le()?;
                    let len = self.bits.u32_le()?;
                    if crc != self.crc || len != self.len {
                        return Err(bad("checksum mismatch: the file is damaged"));
                    }
                    // Another member, or the end.
                    self.state = if self.bits.at_end()? { State::Done } else { State::Header };
                }
                State::Done => {}
            }
        }
        Ok(())
    }

    fn header(&mut self) -> io::Result<()> {
        let b = &mut self.bits;
        if b.byte()? != 0x1f || b.byte()? != 0x8b {
            return Err(bad("not a gzip file"));
        }
        if b.byte()? != 8 {
            return Err(bad("unknown compression method"));
        }
        let flags = b.byte()?;
        for _ in 0..6 {
            b.byte()?; // mtime, xfl, os
        }
        if flags & 4 != 0 {
            let n = u16::from(b.byte()?) | (u16::from(b.byte()?) << 8);
            for _ in 0..n {
                b.byte()?;
            }
        }
        for flag in [8, 16] {
            if flags & flag != 0 {
                while b.byte()? != 0 {}
            }
        }
        if flags & 2 != 0 {
            b.byte()?;
            b.byte()?;
        }
        Ok(())
    }

    /// Decodes one block into `out`; returns whether it was the last.
    fn block(&mut self) -> io::Result<bool> {
        let last = self.bits.bits(1)? == 1;
        match self.bits.bits(2)? {
            0 => {
                self.bits.align();
                let len = self.bits.u16_le()?;
                let nlen = self.bits.u16_le()?;
                if len != !nlen {
                    return Err(bad("bad stored block length"));
                }
                for _ in 0..len {
                    let b = self.bits.byte()?;
                    self.out.push(b);
                }
            }
            1 => {
                let (lit, dist) = fixed_tables();
                self.codes(&lit, &dist)?;
            }
            2 => {
                let (lit, dist) = self.dynamic_tables()?;
                self.codes(&lit, &dist)?;
            }
            _ => return Err(bad("bad block type")),
        }
        self.crc = crc32(self.crc, &self.out);
        self.len = self.len.wrapping_add(self.out.len() as u32);
        self.window.extend_from_slice(&self.out);
        if self.window.len() > 2 * WINDOW {
            self.window.drain(..self.window.len() - WINDOW);
        }
        Ok(last)
    }

    fn dynamic_tables(&mut self) -> io::Result<(Huffman, Huffman)> {
        let hlit = self.bits.bits(5)? as usize + 257;
        let hdist = self.bits.bits(5)? as usize + 1;
        let hclen = self.bits.bits(4)? as usize + 4;
        const ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];
        let mut cl = [0u8; 19];
        for &i in &ORDER[..hclen] {
            cl[i] = self.bits.bits(3)? as u8;
        }
        let cl = Huffman::new(&cl)?;
        let mut lengths = vec![0u8; hlit + hdist];
        let mut i = 0;
        while i < lengths.len() {
            let sym = cl.decode(&mut self.bits)?;
            let (value, repeat) = match sym {
                0..=15 => (sym as u8, 1),
                16 => {
                    let prev = *lengths[..i].last().ok_or_else(|| bad("repeat with no previous length"))?;
                    (prev, 3 + self.bits.bits(2)? as usize)
                }
                17 => (0, 3 + self.bits.bits(3)? as usize),
                _ => (0, 11 + self.bits.bits(7)? as usize),
            };
            if i + repeat > lengths.len() {
                return Err(bad("too many code lengths"));
            }
            lengths[i..i + repeat].fill(value);
            i += repeat;
        }
        Ok((Huffman::new(&lengths[..hlit])?, Huffman::new(&lengths[hlit..])?))
    }

    fn codes(&mut self, lit: &Huffman, dist: &Huffman) -> io::Result<()> {
        const LEN_BASE: [u16; 29] =
            [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
        const LEN_EXTRA: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
        const DIST_BASE: [u16; 30] = [
            1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097,
            6145, 8193, 12289, 16385, 24577,
        ];
        const DIST_EXTRA: [u8; 30] =
            [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];
        loop {
            let sym = lit.decode(&mut self.bits)?;
            match sym {
                0..=255 => self.out.push(sym as u8),
                256 => return Ok(()),
                257..=285 => {
                    let i = (sym - 257) as usize;
                    let len = LEN_BASE[i] as usize + self.bits.bits(LEN_EXTRA[i])? as usize;
                    let d = dist.decode(&mut self.bits)? as usize;
                    if d >= 30 {
                        return Err(bad("bad distance code"));
                    }
                    let distance = DIST_BASE[d] as usize + self.bits.bits(DIST_EXTRA[d])? as usize;
                    let available = self.window.len() + self.out.len();
                    if distance > available {
                        return Err(bad("distance reaches before the start"));
                    }
                    for _ in 0..len {
                        let total = self.window.len() + self.out.len();
                        let from = total - distance;
                        let b = if from < self.window.len() {
                            self.window[from]
                        } else {
                            self.out[from - self.window.len()]
                        };
                        self.out.push(b);
                    }
                }
                _ => return Err(bad("bad literal/length code")),
            }
        }
    }
}

impl<R: BufRead> Read for GzDecoder<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos == self.out.len() {
            if self.state == State::Done {
                return Ok(0);
            }
            self.fill()?;
        }
        let n = buf.len().min(self.out.len() - self.pos);
        buf[..n].copy_from_slice(&self.out[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// Reads bits least significant first, as DEFLATE packs them.
struct Bits<R: BufRead> {
    input: R,
    bit: u32,
    nbits: u8,
}

impl<R: BufRead> Bits<R> {
    fn raw_byte(&mut self) -> io::Result<u8> {
        let buf = self.input.fill_buf()?;
        let Some(&b) = buf.first() else {
            return Err(bad("the file ends too early"));
        };
        self.input.consume(1);
        Ok(b)
    }

    fn bits(&mut self, n: u8) -> io::Result<u32> {
        while self.nbits < n {
            self.bit |= u32::from(self.raw_byte()?) << self.nbits;
            self.nbits += 8;
        }
        let v = self.bit & ((1u32 << n) - 1);
        self.bit >>= n;
        self.nbits -= n;
        Ok(v)
    }

    fn align(&mut self) {
        let drop = self.nbits % 8;
        self.bit >>= drop;
        self.nbits -= drop;
    }

    /// A whole byte (after [`align`](Self::align), or at a byte boundary).
    fn byte(&mut self) -> io::Result<u8> {
        if self.nbits >= 8 {
            return Ok(self.bits(8)? as u8);
        }
        self.raw_byte()
    }

    fn u16_le(&mut self) -> io::Result<u16> {
        Ok(u16::from(self.byte()?) | (u16::from(self.byte()?) << 8))
    }

    fn u32_le(&mut self) -> io::Result<u32> {
        Ok(u32::from(self.u16_le()?) | (u32::from(self.u16_le()?) << 16))
    }

    /// Whether nothing (or only zero padding) follows.
    fn at_end(&mut self) -> io::Result<bool> {
        if self.nbits > 0 {
            return Ok(false);
        }
        loop {
            let buf = self.input.fill_buf()?;
            match buf.first() {
                None => return Ok(true),
                Some(0) => {
                    let n = buf.iter().take_while(|&&b| b == 0).count();
                    self.input.consume(n);
                }
                Some(_) => return Ok(false),
            }
        }
    }
}

/// A canonical Huffman code, decoded a bit at a time with counts per length (as zlib's
/// "puff" does).
struct Huffman {
    counts: [u16; 16],
    symbols: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> io::Result<Huffman> {
        let mut counts = [0u16; 16];
        for &l in lengths {
            counts[l as usize] += 1;
        }
        counts[0] = 0;
        let mut left: i32 = 1;
        for &c in &counts[1..] {
            left = (left << 1) - i32::from(c);
            if left < 0 {
                return Err(bad("over-subscribed Huffman code"));
            }
        }
        let mut offs = [0u16; 16];
        for len in 1..15 {
            offs[len + 1] = offs[len] + counts[len];
        }
        let mut symbols = vec![0u16; lengths.len()];
        for (sym, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbols[offs[l as usize] as usize] = sym as u16;
                offs[l as usize] += 1;
            }
        }
        Ok(Huffman { counts, symbols })
    }

    fn decode<R: BufRead>(&self, bits: &mut Bits<R>) -> io::Result<u16> {
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for len in 1..16 {
            code |= bits.bits(1)? as i32;
            let count = i32::from(self.counts[len]);
            if code - count < first {
                return Ok(self.symbols[(index + (code - first)) as usize]);
            }
            index += count;
            first += count;
            first <<= 1;
            code <<= 1;
        }
        Err(bad("bad Huffman code"))
    }
}

fn fixed_tables() -> (Huffman, Huffman) {
    let mut lit = [0u8; 288];
    lit[..144].fill(8);
    lit[144..256].fill(9);
    lit[256..280].fill(7);
    lit[280..].fill(8);
    (Huffman::new(&lit).expect("fixed code"), Huffman::new(&[5u8; 30]).expect("fixed code"))
}

/// Continues the CRC-32 (IEEE) `crc` over `data`.
fn crc32(crc: u32, data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, e) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *e = c;
        }
        t
    });
    let mut c = !crc;
    for &b in data {
        c = table[((c ^ u32::from(b)) & 0xff) as usize] ^ (c >> 8);
    }
    !c
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gunzip(bytes: &[u8]) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        GzDecoder::new(bytes).read_to_end(&mut out)?;
        Ok(out)
    }

    /// `printf 'name,age\nAnn,31\nBob,42\n' | gzip -9n` (fixed Huffman codes).
    const SMALL: &[u8] = &[
        0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0xff, 0xcb, 0x4b, 0xcc, 0x4d, 0xd5, 0x49, 0x4c, 0x4f,
        0xe5, 0x72, 0xcc, 0xcb, 0xd3, 0x31, 0x36, 0xe4, 0x72, 0xca, 0x4f, 0xd2, 0x31, 0x31, 0xe2, 0x02, 0x00, 0x56,
        0x83, 0x28, 0xf8, 0x17, 0x00, 0x00, 0x00,
    ];

    #[test]
    fn crc() {
        assert_eq!(crc32(0, b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(crc32(0, b"1234"), b"56789"), 0xCBF4_3926);
    }

    #[test]
    fn fixed_and_multi_member() {
        assert_eq!(gunzip(SMALL).unwrap(), b"name,age\nAnn,31\nBob,42\n");
        let two = [SMALL, SMALL].concat();
        assert_eq!(gunzip(&two).unwrap(), b"name,age\nAnn,31\nBob,42\nname,age\nAnn,31\nBob,42\n");
        let mut damaged = SMALL.to_vec();
        damaged[SMALL.len() - 6] ^= 1;
        assert!(gunzip(&damaged).is_err());
        assert!(gunzip(&SMALL[..20]).is_err());
        assert!(gunzip(b"plain text").is_err());
    }

    #[test]
    fn stored_blocks() {
        // A stored block by hand: header, BFINAL=1 BTYPE=00, LEN/NLEN, data, trailer.
        let data = b"hello, stored";
        let mut gz = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff, 0x01];
        gz.extend_from_slice(&(data.len() as u16).to_le_bytes());
        gz.extend_from_slice(&(!(data.len() as u16)).to_le_bytes());
        gz.extend_from_slice(data);
        gz.extend_from_slice(&crc32(0, data).to_le_bytes());
        gz.extend_from_slice(&(data.len() as u32).to_le_bytes());
        assert_eq!(gunzip(&gz).unwrap(), data);
    }
}
