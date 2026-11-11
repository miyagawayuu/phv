//! The memory image-webp takes for the prefix codes of a lossless (VP8L)
//! bitstream, found by reading the bitstream as far as its last code before
//! image-webp does (the 20th audit's R23-1).
//!
//! A VP8L bitstream may name up to 65,536 groups of five prefix codes in its
//! entropy image (a meta code is 16 bits), whatever the canvas, and
//! image-webp 0.2.4 builds a table (and, for codes longer than 10 bits, a
//! tree) for every code of every group, counted against nothing: the `image`
//! crate gives it no limit. A 1×1 WebP of 3.8 MB made it allocate 1.3 GB.
//!
//! So std.image reads the same bits first: the transforms and the entropy
//! image, decoded in full (their pixels say how many groups there are, and
//! where the codes start), then the lengths of every code, adding up the
//! tables image-webp will build from them. It stops as soon as the sum passes
//! what the script has left (`Stop::Memory`). A bitstream image-webp would
//! stop on is stopped on at the same place (`Stop::Damaged`); one whose codes
//! image-webp would build although they are not prefix codes is refused where
//! its pixels would have to be decoded with them.
//!
//! Nothing here is sized from what the file declares before it is counted:
//! the subimages are counted before they are allocated, and they are at most
//! as large as image-webp's own copies of them.

/// Why reading a bitstream stopped.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Stop {
    /// Decoding it takes more than the memory left.
    Memory,
    /// image-webp would stop on it too (or decode it with codes that are not
    /// prefix codes).
    Damaged(&'static str),
}

type Read<T> = Result<T, Stop>;

/// The order the lengths of the code-length code come in.
const ORDER: [usize; 19] = [17, 18, 0, 1, 2, 3, 4, 5, 16, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
/// The symbols of the five codes of a group (green's grows with the color cache).
const ALPHABET: [u32; 5] = [256 + 24, 256, 256, 256, 40];
/// Codes up to this length are looked up in image-webp's table; longer ones
/// go on through its tree.
const TABLE_BITS: u32 = 10;
/// Bytes an allocation takes besides its contents (the allocator's header and rounding).
const SLACK: u64 = 32;
/// Bytes of a group of five codes in image-webp's vector of groups (five
/// enums of two vectors and a mask).
const GROUP: u64 = 5 * 56;
/// A tree node of image-webp's (an enum holding a `usize`).
const NODE: u64 = 16;
/// What reading one code takes for a moment: its lengths and the code-length code.
const TRANSIENT: u64 = 16 << 10;

/// The distance codes 1 to 120: (x, y) offsets near the pixel.
#[rustfmt::skip]
const DISTANCES: [(i8, i8); 120] = [
    (0, 1),  (1, 0),  (1, 1),  (-1, 1), (0, 2),  (2, 0),  (1, 2),  (-1, 2),
    (2, 1),  (-2, 1), (2, 2),  (-2, 2), (0, 3),  (3, 0),  (1, 3),  (-1, 3),
    (3, 1),  (-3, 1), (2, 3),  (-2, 3), (3, 2),  (-3, 2), (0, 4),  (4, 0),
    (1, 4),  (-1, 4), (4, 1),  (-4, 1), (3, 3),  (-3, 3), (2, 4),  (-2, 4),
    (4, 2),  (-4, 2), (0, 5),  (3, 4),  (-3, 4), (4, 3),  (-4, 3), (5, 0),
    (1, 5),  (-1, 5), (5, 1),  (-5, 1), (2, 5),  (-2, 5), (5, 2),  (-5, 2),
    (4, 4),  (-4, 4), (3, 5),  (-3, 5), (5, 3),  (-5, 3), (0, 6),  (6, 0),
    (1, 6),  (-1, 6), (6, 1),  (-6, 1), (2, 6),  (-2, 6), (6, 2),  (-6, 2),
    (4, 5),  (-4, 5), (5, 4),  (-5, 4), (3, 6),  (-3, 6), (6, 3),  (-6, 3),
    (0, 7),  (7, 0),  (1, 7),  (-1, 7), (5, 5),  (-5, 5), (7, 1),  (-7, 1),
    (4, 6),  (-4, 6), (6, 4),  (-6, 4), (2, 7),  (-2, 7), (7, 2),  (-7, 2),
    (3, 7),  (-3, 7), (7, 3),  (-7, 3), (5, 6),  (-5, 6), (6, 5),  (-6, 5),
    (8, 0),  (4, 7),  (-4, 7), (7, 4),  (-7, 4), (8, 1),  (8, 2),  (6, 6),
    (-6, 6), (8, 3),  (5, 7),  (-5, 7), (7, 5),  (-7, 5), (8, 4),  (6, 7),
    (-6, 7), (7, 6),  (-7, 6), (8, 5),  (7, 7),  (-7, 7), (8, 6),  (8, 7),
];

/// Bits read least significant first, as VP8L stores them. Reading past the
/// end is an error, as it is for image-webp.
struct Bits<'a> {
    b: &'a [u8],
    at: usize,
}

impl Bits<'_> {
    fn left(&self) -> usize {
        self.b.len() * 8 - self.at
    }

    /// At least the next 57 bits (zeros past the end).
    fn peek(&self) -> u64 {
        let i = self.at / 8;
        let mut v = 0u64;
        for (k, &x) in self.b.get(i..).unwrap_or_default().iter().take(8).enumerate() {
            v |= (x as u64) << (8 * k);
        }
        v >> (self.at % 8)
    }

    fn skip(&mut self, n: usize) -> Read<()> {
        if n > self.left() {
            return Err(Stop::Damaged("its lossless bitstream ends too soon"));
        }
        self.at += n;
        Ok(())
    }

    fn read(&mut self, n: u32) -> Read<u32> {
        let v = (self.peek() & ((1u64 << n) - 1)) as u32;
        self.skip(n as usize)?;
        Ok(v)
    }
}

/// A prefix code that pixels are decoded with.
enum Code {
    /// One symbol, read with no bits.
    Single(u16),
    /// A simple code of two symbols: one bit.
    Two(u16, u16),
    /// A canonical code: the symbols by length (then by value), and how many
    /// have each length.
    Canonical { count: [u16; 16], sorted: Vec<u16> },
}

impl Code {
    fn read(&self, r: &mut Bits) -> Read<u16> {
        match self {
            Code::Single(s) => Ok(*s),
            Code::Two(zero, one) => Ok(if r.read(1)? == 0 { *zero } else { *one }),
            Code::Canonical { count, sorted } => {
                // The first bit read is the code's most significant one.
                let bits = r.peek();
                let (mut code, mut first, mut index) = (0i64, 0i64, 0i64);
                for (len, &n) in count.iter().enumerate().skip(1) {
                    code |= ((bits >> (len - 1)) & 1) as i64;
                    let n = n as i64;
                    if code >= first && code - first < n {
                        r.skip(len)?;
                        return Ok(sorted[(index + code - first) as usize]);
                    }
                    index += n;
                    first = (first + n) << 1;
                    code <<= 1;
                }
                // A complete code always ends within 15 bits.
                Err(Stop::Damaged("its lossless bitstream has a broken prefix code"))
            }
        }
    }
}

/// What a code's lengths make in image-webp (`HuffmanTree::build_implicit`).
enum Built {
    /// No symbols, or lengths that are no prefix code: image-webp stops (or
    /// panics, which std.image catches).
    Refused,
    /// One symbol.
    Single(u16),
    /// A complete prefix code.
    Complete,
    /// Lengths image-webp's 16-bit arithmetic takes for a complete code
    /// although they are not one: it builds a table and a tree from them.
    Wrapped,
}

/// How image-webp judges code lengths whose counts by length are `hist`,
/// with `first` the first symbol that has a length.
fn judge(hist: &[u32; 16], first: u16) -> Built {
    let symbols: u32 = hist[1..].iter().sum();
    if symbols == 0 {
        return Built::Refused;
    }
    if symbols == 1 {
        return Built::Single(first);
    }
    let longest = (1..16).rev().find(|&l| hist[l] > 0).unwrap_or(1);
    let kraft: u64 = (1..16).map(|l| (hist[l] as u64) << (15 - l)).sum();
    if kraft == 1 << 15 {
        return Built::Complete;
    }
    // image-webp counts in u16: an addition past 65,535 panics, and a shift
    // drops what goes past 16 bits (so a code of 15 bits passes when the
    // count wraps to 0).
    let mut curr: u16 = 0;
    for &n in &hist[1..=longest] {
        let Some(sum) = curr.checked_add(n as u16) else { return Built::Refused };
        curr = sum << 1;
    }
    if curr == 2u16 << longest {
        Built::Wrapped
    } else {
        Built::Refused
    }
}

/// Bytes image-webp keeps for a code whose counts by length are `hist`.
fn kept(hist: &[u32; 16], built: &Built) -> u64 {
    let longest = (1..16).rev().find(|&l| hist[l] > 0).unwrap_or(1) as u32;
    let table_bits = longest.min(TABLE_BITS);
    let long: u64 = hist[table_bits as usize + 1..].iter().map(|&n| n as u64).sum();
    let table = 4u64 << table_bits;
    let tree = match built {
        // Each subtree of a complete code holds twice its leaves less one:
        // within the capacity image-webp asks for.
        Built::Complete => 2 * long,
        // Without a prefix code, each long code may add a node and two for
        // every bit past the table, and the vector may double past them.
        _ => 2 * (2 * long).max(long * (1 + 2 * (15 - TABLE_BITS as u64))),
    };
    table + tree * NODE + 2 * SLACK
}

/// A bitstream being read, and the bytes counted so far.
struct Reader<'a> {
    r: Bits<'a>,
    spent: u64,
    budget: u64,
}

impl Reader<'_> {
    fn charge(&mut self, bytes: u64) -> Read<()> {
        self.spent = self.spent.saturating_add(bytes);
        if self.spent > self.budget {
            return Err(Stop::Memory);
        }
        Ok(())
    }

    fn color_cache(&mut self) -> Read<Option<u32>> {
        if self.r.read(1)? == 0 {
            return Ok(None);
        }
        let bits = self.r.read(4)?;
        if !(1..=11).contains(&bits) {
            return Err(Stop::Damaged("its lossless bitstream has a color cache of a size that cannot be"));
        }
        self.charge((4 << bits) + SLACK)?;
        Ok(Some(bits))
    }

    /// Reads one code of `alphabet` symbols: counts what image-webp keeps of
    /// it and, when `decode` is set, returns it to decode pixels with.
    fn code(&mut self, alphabet: u32, decode: bool) -> Read<Option<Code>> {
        if self.r.read(1)? == 1 {
            // A simple code: one or two symbols.
            let two = self.r.read(1)? == 1;
            let wide = self.r.read(1)?;
            let zero = self.r.read(1 + 7 * wide)? as u16;
            if zero as u32 >= alphabet {
                return Err(Stop::Damaged("its lossless bitstream has a symbol outside its code"));
            }
            if !two {
                return Ok(Some(Code::Single(zero)));
            }
            let one = self.r.read(8)? as u16;
            if one as u32 >= alphabet {
                return Err(Stop::Damaged("its lossless bitstream has a symbol outside its code"));
            }
            self.charge(3 * NODE + 8 + 2 * SLACK)?;
            return Ok(Some(Code::Two(zero, one)));
        }
        // The code-length code, then the lengths it codes.
        let mut cl = [0u8; 19];
        let n = 4 + self.r.read(4)? as usize;
        for &at in &ORDER[..n] {
            cl[at] = self.r.read(3)? as u8;
        }
        let cl_code = self.canonical(&cl)?;
        let mut max_symbol = if self.r.read(1)? == 1 {
            let bits = 2 + 2 * self.r.read(3)?;
            let most = self.r.read(bits)?;
            if most > alphabet - 2 {
                return Err(Stop::Damaged("its lossless bitstream has a code longer than its alphabet"));
            }
            2 + most
        } else {
            alphabet
        };
        let mut lengths = if decode { vec![0u8; alphabet as usize] } else { Vec::new() };
        let mut hist = [0u32; 16];
        let mut first = None;
        let (mut symbol, mut prev) = (0u32, 8u8);
        let mut put = |at: u32, n: u32, len: u8, hist: &mut [u32; 16], lengths: &mut Vec<u8>| {
            hist[len as usize] += n;
            if len != 0 && first.is_none() {
                first = Some(at as u16);
            }
            if let Some(s) = lengths.get_mut(at as usize..(at + n) as usize) {
                s.fill(len);
            }
        };
        while symbol < alphabet {
            if max_symbol == 0 {
                break;
            }
            max_symbol -= 1;
            // A code-length code of one symbol below 16 reads no bits: the
            // symbols it gives are counted at once.
            if let &Code::Single(len @ 0..=15) = &cl_code {
                let n = (max_symbol + 1).min(alphabet - symbol);
                put(symbol, n, len as u8, &mut hist, &mut lengths);
                if len != 0 {
                    prev = len as u8;
                }
                symbol += n;
                max_symbol -= n - 1;
                continue;
            }
            let len = cl_code.read(&mut self.r)?;
            if len < 16 {
                put(symbol, 1, len as u8, &mut hist, &mut lengths);
                symbol += 1;
                if len != 0 {
                    prev = len as u8;
                }
            } else {
                let (extra, offset) = match len {
                    16 => (2, 3),
                    17 => (3, 3),
                    18 => (7, 11),
                    _ => return Err(Stop::Damaged("its lossless bitstream has a code length that cannot be")),
                };
                let repeat = self.r.read(extra)? + offset;
                if symbol + repeat > alphabet {
                    return Err(Stop::Damaged("its lossless bitstream repeats a code length past its alphabet"));
                }
                put(symbol, repeat, if len == 16 { prev } else { 0 }, &mut hist, &mut lengths);
                symbol += repeat;
            }
        }
        let built = judge(&hist, first.unwrap_or(0));
        match built {
            Built::Refused => Err(Stop::Damaged("its lossless bitstream has a broken prefix code")),
            Built::Single(s) => Ok(Some(Code::Single(s))),
            Built::Complete | Built::Wrapped => {
                self.charge(kept(&hist, &built))?;
                match (decode, built) {
                    (false, _) => Ok(None),
                    (true, Built::Complete) => Ok(Some(canonical(&lengths, &hist))),
                    // image-webp would decode pixels with lengths that are no
                    // prefix code: not followed here.
                    _ => Err(Stop::Damaged("its lossless bitstream has a broken prefix code")),
                }
            }
        }
    }

    /// The code-length code of `cl`, as image-webp builds it.
    fn canonical(&mut self, cl: &[u8; 19]) -> Read<Code> {
        let mut hist = [0u32; 16];
        for &l in cl {
            hist[l as usize] += 1;
        }
        let first = cl.iter().position(|&l| l != 0).unwrap_or(0) as u16;
        match judge(&hist, first) {
            Built::Single(s) => Ok(Code::Single(s)),
            Built::Complete => Ok(canonical(cl, &hist)),
            _ => Err(Stop::Damaged("its lossless bitstream has a broken prefix code")),
        }
    }

    /// Reads an image stream of `w` × `h` pixels: its color cache, its codes
    /// and, unless it is the main image (`argb`), its pixels. Returns the
    /// pixels (as ARGB) of a subimage.
    fn image(&mut self, w: u32, h: u32, argb: bool) -> Read<Vec<u32>> {
        let cache_bits = self.color_cache()?;
        let mut groups = 1u32;
        if argb && self.r.read(1)? == 1 {
            // The entropy image: a meta code (red and green) for each block.
            let bits = self.r.read(3)? + 2;
            let (ew, eh) = (subsample(w, bits), subsample(h, bits));
            // image-webp keeps it as bytes and then as 16-bit codes.
            // (the bytes are counted with the subimage, the codes here).
            self.charge(ew as u64 * eh as u64 * 2 + SLACK)?;
            let entropy = self.image(ew, eh, false)?;
            groups = entropy.iter().map(|&p| (p >> 8) & 0xFFFF).max().unwrap_or(0) + 1;
        }
        // Every code of every group, in image-webp's vector of groups.
        self.charge(groups.max(4).next_power_of_two() as u64 * GROUP + SLACK)?;
        let mut codes = Vec::new();
        for _ in 0..groups {
            for (j, &size) in ALPHABET.iter().enumerate() {
                let alphabet = size + if j == 0 { cache_bits.map_or(0, |b| 1 << b) } else { 0 };
                self.charge(TRANSIENT)?;
                self.spent -= TRANSIENT;
                if let Some(code) = self.code(alphabet, !argb)? {
                    codes.push(code);
                }
            }
        }
        if argb {
            return Ok(Vec::new());
        }
        self.charge(w as u64 * h as u64 * 4 + SLACK)?;
        self.pixels(w, h, &codes, cache_bits)
    }

    /// Decodes the pixels of a subimage with the codes of its one group.
    fn pixels(&mut self, w: u32, h: u32, codes: &[Code], cache_bits: Option<u32>) -> Read<Vec<u32>> {
        let [green, red, blue, alpha, dist] = codes else {
            return Err(Stop::Damaged("its lossless bitstream has a broken prefix code"));
        };
        let total = w as usize * h as usize;
        let mut px: Vec<u32> = Vec::with_capacity(total);
        let mut cache = vec![0u32; cache_bits.map_or(0, |b| 1 << b)];
        let insert = |cache: &mut [u32], argb: u32| {
            if let Some(b) = cache_bits {
                cache[(0x1e35_a7bdu32.wrapping_mul(argb) >> (32 - b)) as usize] = argb;
            }
        };
        while px.len() < total {
            let g = green.read(&mut self.r)? as u32;
            if g < 256 {
                let r = red.read(&mut self.r)? as u32;
                let b = blue.read(&mut self.r)? as u32;
                let a = alpha.read(&mut self.r)? as u32;
                let argb = (a << 24) | (r << 16) | (g << 8) | b;
                px.push(argb);
                insert(&mut cache, argb);
            } else if g < 256 + 24 {
                let length = copy_distance(&mut self.r, g - 256)?;
                let prefix = dist.read(&mut self.r)? as u32;
                let code = copy_distance(&mut self.r, prefix)?;
                let distance = if code > 120 {
                    code - 120
                } else {
                    let (dx, dy) = DISTANCES[code - 1];
                    (dx as i64 + dy as i64 * w as i64).max(1) as usize
                };
                let at = px.len();
                if at < distance || total - at < length {
                    return Err(Stop::Damaged("its lossless bitstream copies pixels from outside its image"));
                }
                for i in at..at + length {
                    let argb = px[i - distance];
                    px.push(argb);
                    insert(&mut cache, argb);
                }
            } else {
                let Some(&argb) = cache.get((g - 280) as usize) else {
                    return Err(Stop::Damaged("its lossless bitstream uses a color cache it does not have"));
                };
                // Already where its color hashes to: inserting it changes nothing.
                px.push(argb);
            }
        }
        Ok(px)
    }
}

/// A length or a distance from its prefix symbol and extra bits.
fn copy_distance(r: &mut Bits, prefix: u32) -> Read<usize> {
    if prefix < 4 {
        return Ok(prefix as usize + 1);
    }
    let extra = (prefix - 2) >> 1;
    let offset = (2 + (prefix as usize & 1)) << extra;
    Ok(offset + r.read(extra)? as usize + 1)
}

/// `size` divided by 2^`bits`, rounded up.
fn subsample(size: u32, bits: u32) -> u32 {
    (size + (1 << bits) - 1) >> bits
}

/// The canonical code of complete lengths `lengths` (counted by length in `hist`).
fn canonical(lengths: &[u8], hist: &[u32; 16]) -> Code {
    let mut count = [0u16; 16];
    for l in 1..16 {
        count[l] = hist[l] as u16;
    }
    let mut sorted = Vec::with_capacity(hist[1..].iter().sum::<u32>() as usize);
    for l in 1..16u8 {
        sorted.extend(lengths.iter().enumerate().filter(|&(_, &x)| x == l).map(|(s, _)| s as u16));
    }
    Code::Canonical { count, sorted }
}

/// The bytes image-webp takes, besides the image it decodes into, to decode
/// the lossless bitstream `b`: one with its own header (a VP8L chunk), or
/// with `implicit` as its size (an ALPH chunk's). Stops at `budget`.
pub(super) fn need(b: &[u8], implicit: Option<(u32, u32)>, budget: u64) -> Result<u64, Stop> {
    let mut rd = Reader { r: Bits { b, at: 0 }, spent: 0, budget };
    let (w, h) = match implicit {
        Some(size) => size,
        None => {
            if rd.r.read(8)? != 0x2f {
                return Err(Stop::Damaged("its lossless bitstream has no signature"));
            }
            let w = rd.r.read(14)? + 1;
            let h = rd.r.read(14)? + 1;
            rd.r.read(1)?;
            if rd.r.read(3)? != 0 {
                return Err(Stop::Damaged("its lossless bitstream has an unknown version"));
            }
            (w, h)
        }
    };
    // The transforms, each with its subimage (kept until the image is decoded).
    let mut seen = [false; 4];
    let mut xsize = w;
    while rd.r.read(1)? == 1 {
        let kind = rd.r.read(2)? as usize;
        if std::mem::replace(&mut seen[kind], true) {
            return Err(Stop::Damaged("its lossless bitstream has a transform twice"));
        }
        match kind {
            0 | 1 => {
                let bits = rd.r.read(3)? + 2;
                rd.image(subsample(xsize, bits), subsample(h, bits), false)?;
            }
            3 => {
                let colors = rd.r.read(8)? + 1;
                rd.image(colors, 1, false)?;
                let bits = match colors {
                    0..=2 => 3,
                    3..=4 => 2,
                    5..=16 => 1,
                    _ => 0,
                };
                xsize = subsample(xsize, bits);
            }
            _ => {}
        }
    }
    rd.image(xsize, h, true)?;
    Ok(rd.spent)
}
