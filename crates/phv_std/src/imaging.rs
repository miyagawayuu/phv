//! Images (`std.image`): PNG, JPEG, GIF and WebP read with the `image` crate's
//! pure-Rust decoders (png, zune-jpeg, gif, image-webp), and written as PNG,
//! JPEG or lossless WebP.
//!
//! Images come from visitors, so:
//! - The format is found from the first bytes, never from a name or a type.
//! - Reading an image reads its header only. Its size is checked there,
//!   before anything is decoded: at most `MAX_SIDE` pixels a side and
//!   `MAX_PIXELS` in all, from at most `MAX_INPUT` bytes. A small file that
//!   declares a huge size is refused at once.
//! - The pixels are decoded when the image is written, after the memory they
//!   take (and the decoder's working memory) is reserved against the script's
//!   limit (`Vm::reserve`): an image too large for it ends the script with the
//!   usual memory error instead of an allocation failure that would end the
//!   process. The decoders that take a limit get what is left beyond that
//!   reservation (and a small fixed one while the header is read); so does
//!   the encoders' working memory.
//! - No decoder is asked for metadata: an allocation it would size from what
//!   the file declares (image-webp's EXIF chunk, png's ICC profile) is never
//!   made. The EXIF orientation is read in place from the file's bytes, a PNG
//!   is decoded without its ICC, text and EXIF chunks (`PngView`), and a WebP
//!   whose bitstream declares a size other than its canvas or frame is refused
//!   before image-webp decodes it at that size (`webp_layout`).
//! - JPEGs are read with zune-jpeg directly, so its own header parse says
//!   whether one is progressive, and its own count of scans is capped.
//! - Decoders and encoders run inside `catch_unwind`: a panic in one of them is
//!   a `ValueError`, not the end of the page (or of `phv run`), and is not
//!   printed (`guarded`).
//! - Decoding and encoding cannot be interrupted; the caps bound them (see the
//!   spec for the times measured). Resizing, cropping and turning check the
//!   time and memory limits row by row.
//! - Nothing of the source's metadata (EXIF, XMP, ICC, comments) is written.
//!   The EXIF orientation is applied when the image is read, as browsers do.
//!
//! An `Image` is a source and the steps that make it from it (`Pipeline`), so
//! a script that reads many uploads holds their bytes, not their pixels.

use crate::{int, s};
use base64::alphabet;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig, STANDARD};
use base64::engine::DecodePaddingMode;
use base64::Engine;
use image::codecs::gif::GifDecoder;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType, PngDecoder, PngEncoder};
use image::codecs::webp::{WebPDecoder, WebPEncoder};
use image::metadata::Orientation;
use image::{DynamicImage, ExtendedColorType, ImageDecoder, ImageEncoder, ImageError, Limits};
use phv_vm::{BuiltinFn, Value, Vm, VmError};
use std::cell::Cell;
use std::io::{BufRead, Cursor, Read, Seek, SeekFrom};
use std::ops::Range;
use std::rc::Rc;
use zune_jpeg::zune_core::bytestream::ZCursor;
use zune_jpeg::zune_core::colorspace::ColorSpace;
use zune_jpeg::zune_core::options::DecoderOptions;

/// The widest and tallest image read or made.
pub(crate) const MAX_SIDE: u32 = 16_384;
/// The most pixels an image read or made may have (a 48-megapixel photo fits).
pub(crate) const MAX_PIXELS: u64 = 50_000_000;
/// The largest file an image is read from.
pub(crate) const MAX_INPUT: usize = 32 << 20;
/// What a decoder may allocate while the header is read (the PNG decoder's
/// palette and line buffer; the rest of a header is read in place).
const HEADER_ALLOC: u64 = 1 << 20;
/// Scans × pixels a progressive JPEG may ask for: 20 scans at the pixel cap,
/// 83 for 12 megapixels (an encoder writes about 10), and at least 10, at most
/// 100 (zune-jpeg's own cap).
const MAX_SCAN_PIXELS: u64 = 1_000_000_000;
/// The most steps (resizes, crops, turns) an image is made with.
const MAX_STEPS: usize = 64;
/// Rows between two checks of the limits while pixels are worked on.
const ROWS_PER_CHECK: u32 = 16;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Format {
    Png,
    Jpeg,
    Gif,
    Webp,
}

impl Format {
    fn name(self) -> &'static str {
        match self {
            Format::Png => "png",
            Format::Jpeg => "jpeg",
            Format::Gif => "gif",
            Format::Webp => "webp",
        }
    }

    fn mime(self) -> &'static str {
        match self {
            Format::Png => "image/png",
            Format::Jpeg => "image/jpeg",
            Format::Gif => "image/gif",
            Format::Webp => "image/webp",
        }
    }

    /// The format of `b`, from its signature.
    fn sniff(b: &[u8]) -> Option<Format> {
        if b.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some(Format::Png)
        } else if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
            Some(Format::Jpeg)
        } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
            Some(Format::Gif)
        } else if b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
            Some(Format::Webp)
        } else {
            None
        }
    }
}

/// An image's encoded bytes and what its header says.
#[derive(Debug)]
struct Source {
    bytes: Vec<u8>,
    format: Format,
    /// The orientation its EXIF asks for, applied after decoding.
    orientation: Orientation,
    /// Bytes decoding takes: the decoder's buffer, the 8-bit pixels made from
    /// it, its working memory, and the turn of the orientation.
    need: u64,
    /// A JPEG's output color space and the most scans it may have (zune-jpeg
    /// counts them as it decodes).
    jpeg: Option<(ColorSpace, usize)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Resize(u32, u32),
    Crop(u32, u32, u32, u32),
    /// Quarter turns clockwise: 1, 2 or 3.
    Turn(u8),
    FlipH,
    FlipV,
}

/// What an `Image` is: a source and the steps that make it, with its size.
#[derive(Debug)]
struct Pipeline {
    src: Rc<Source>,
    ops: Vec<Op>,
    width: u32,
    height: u32,
}

/// Why making an image failed: the script's limits, or a refusal (`ValueError`).
enum Fail {
    Vm(VmError),
    Msg(String),
}

impl From<VmError> for Fail {
    fn from(e: VmError) -> Fail {
        Fail::Vm(e)
    }
}

impl From<String> for Fail {
    fn from(e: String) -> Fail {
        Fail::Msg(e)
    }
}

/// 8-bit pixels, row by row: 1 (gray), 2 (gray, alpha), 3 (RGB) or 4 (RGBA) channels.
#[derive(Clone, Debug, PartialEq)]
struct Pixels {
    w: u32,
    h: u32,
    ch: usize,
    data: Vec<u8>,
}

impl Pixels {
    fn alpha(&self) -> bool {
        self.ch == 2 || self.ch == 4
    }

    fn row(&self, y: u32) -> &[u8] {
        let n = self.w as usize * self.ch;
        &self.data[y as usize * n..(y as usize + 1) * n]
    }
}

/// `n` with a comma between thousands, as the spec writes the caps.
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn caps_text() -> String {
    format!("images may have at most {} pixels a side and {} pixels", thousands(MAX_SIDE as u64), thousands(MAX_PIXELS))
}

/// Whether `w` × `h` is within the caps.
fn within_caps(w: u64, h: u64) -> bool {
    (1..=MAX_SIDE as u64).contains(&w) && (1..=MAX_SIDE as u64).contains(&h) && w * h <= MAX_PIXELS
}

fn limits(max_alloc: u64) -> Limits {
    let mut l = Limits::default();
    l.max_image_width = Some(MAX_SIDE);
    l.max_image_height = Some(MAX_SIDE);
    l.max_alloc = Some(max_alloc);
    l
}

/// The decoder of a PNG, GIF or WebP (JPEGs are read with zune-jpeg itself).
fn decoder<'a>(bytes: &'a [u8], format: Format, limits: Limits) -> Result<Box<dyn ImageDecoder + 'a>, ImageError> {
    let r = Cursor::new(bytes);
    Ok(match format {
        Format::Png => Box::new(PngDecoder::with_limits(PngView::new(bytes), limits)?),
        Format::Jpeg => unreachable!("JPEGs are read with zune-jpeg"),
        Format::Gif => {
            let mut d = GifDecoder::new(r)?;
            d.set_limits(limits)?;
            Box::new(d)
        }
        Format::Webp => {
            let mut d = WebPDecoder::new(r)?;
            d.set_limits(limits)?;
            Box::new(d)
        }
    })
}

/// A decoder's error, as a script sees it (decoders name what is wrong, never
/// more than a few words of the file).
fn decode_error(format: Format, e: ImageError) -> String {
    match e {
        ImageError::Limits(_) => format!("the image is too large: {}", caps_text()),
        e => damaged(format, e.to_string()),
    }
}

fn damaged(format: Format, mut text: String) -> String {
    if text.len() > 200 {
        let mut end = 200;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    format!("the {} image is damaged or not supported ({text})", format.name().to_uppercase())
}

thread_local! {
    /// Whether this thread is inside `guarded`, whose panics are caught and
    /// reported as a `ValueError`.
    static QUIET: Cell<bool> = const { Cell::new(false) };
}

/// Installs, once, a panic hook that says nothing of a panic `guarded`
/// catches (the default hook would print it to stderr, as raw lines outside
/// the server's log format, once per crafted upload) and hands every other
/// panic to the hook that was there before.
fn quiet_caught_panics() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if !QUIET.try_with(Cell::get).unwrap_or(false) {
                previous(info);
            }
        }));
    });
}

/// Runs `f`, turning a panic into a refusal.
fn guarded<T>(what: &str, f: impl FnOnce() -> Result<T, Fail>) -> Result<T, Fail> {
    quiet_caught_panics();
    let was = QUIET.with(|q| q.replace(true));
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    QUIET.with(|q| q.set(was));
    match r {
        Ok(r) => r,
        Err(_) => Err(Fail::Msg(format!("{what}: the image could not be read (its decoder stopped on it)"))),
    }
}

/// The scans of a JPEG (its SOS markers), read over the segments and the
/// entropy-coded data between them.
fn jpeg_scans(b: &[u8]) -> usize {
    let (mut i, mut scans) = (2, 0);
    while i + 1 < b.len() {
        if b[i] != 0xFF {
            i += 1;
            continue;
        }
        match b[i + 1] {
            0xFF => i += 1,
            // A stuffed zero, a restart marker, TEM, SOI.
            0x00 | 0x01 | 0xD0..=0xD8 => i += 2,
            0xD9 => break,
            m => {
                let Some(len) = b.get(i + 2..i + 4) else { break };
                if m == 0xDA {
                    scans += 1;
                }
                i += 2 + u16::from_be_bytes([len[0], len[1]]) as usize;
            }
        }
    }
    scans
}

/// The scans a progressive JPEG of `px` pixels may have.
fn max_scans(px: u64) -> u64 {
    (MAX_SCAN_PIXELS / px.max(1)).clamp(10, 100)
}

/// zune-jpeg's options as `image` sets them (lenient, as browsers are), with
/// `side` as the largest side and `scans` as the most scans it decodes.
fn jpeg_options(side: u32, scans: usize) -> DecoderOptions {
    DecoderOptions::default()
        .set_strict_mode(false)
        .set_max_width(side as usize)
        .set_max_height(side as usize)
        .jpeg_set_max_scans(scans)
}

/// The color space zune-jpeg writes a JPEG in, as `image` asks for it: gray
/// and RGB as they are, anything else (YCbCr, CMYK, YCCK) as RGB.
fn jpeg_out(input: ColorSpace) -> ColorSpace {
    match input {
        ColorSpace::RGB | ColorSpace::RGBA | ColorSpace::Luma | ColorSpace::LumaA => input,
        _ => ColorSpace::RGB,
    }
}

/// What a JPEG's header says, read by the decoder that will decode it.
struct JpegHead {
    w: u32,
    h: u32,
    components: u64,
    progressive: bool,
    out: ColorSpace,
    orientation: Orientation,
}

/// Reads a JPEG's header with zune-jpeg itself, so that whether it is
/// progressive is what the decoder will find (it skips stray bytes between
/// segments, which a walk of our own read as the end of the header).
fn jpeg_head(b: &[u8]) -> Result<JpegHead, String> {
    // The sides are checked against the caps by the caller, with its message.
    let mut d = zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(b), jpeg_options(u16::MAX as u32, 100));
    d.decode_headers().map_err(|e| damaged(Format::Jpeg, e.to_string()))?;
    let info = d.info().ok_or_else(|| damaged(Format::Jpeg, "no header".into()))?;
    let input = d.input_colorspace().unwrap_or(ColorSpace::YCbCr);
    // A broken EXIF block is no reason to refuse the image.
    let orientation = d.exif().and_then(|e| Orientation::from_exif_chunk(e)).unwrap_or(Orientation::NoTransforms);
    Ok(JpegHead {
        w: info.width as u32,
        h: info.height as u32,
        components: info.components as u64,
        progressive: info.sof.is_progressive(),
        out: jpeg_out(input),
        orientation,
    })
}

/// The chunk of a RIFF (WebP) file at `at`: its name, where its data starts
/// and the length its header declares (which may run past the file).
fn riff_chunk(b: &[u8], at: usize) -> Option<([u8; 4], usize, usize)> {
    let head = b.get(at..at.checked_add(8)?)?;
    let len = u32::from_le_bytes([head[4], head[5], head[6], head[7]]) as usize;
    Some(([head[0], head[1], head[2], head[3]], at + 8, len))
}

/// Where the chunk after one whose data starts at `start` with `len` bytes is.
fn riff_next(start: usize, len: usize) -> usize {
    start.saturating_add(len).saturating_add(len & 1)
}

/// The size a VP8 key frame's header declares (as image-webp reads it).
fn vp8_size(d: &[u8]) -> Option<(u32, u32)> {
    let d = d.get(..10)?;
    let keyframe = d[0] & 1 == 0;
    if !keyframe || d[3..6] != [0x9d, 0x01, 0x2a] {
        return None;
    }
    let w = u16::from_le_bytes([d[6], d[7]]) & 0x3FFF;
    let h = u16::from_le_bytes([d[8], d[9]]) & 0x3FFF;
    Some((w as u32, h as u32))
}

/// The size a VP8L bitstream's header declares.
fn vp8l_size(d: &[u8]) -> Option<(u32, u32)> {
    let d = d.get(..5)?;
    if d[0] != 0x2f {
        return None;
    }
    let bits = u32::from_le_bytes([d[1], d[2], d[3], d[4]]);
    Some(((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1))
}

/// Refuses an image chunk whose bitstream declares another size than `want`
/// (image-webp decodes a VP8 frame at its own size before it compares them).
fn webp_frame(name: &[u8; 4], data: &[u8], want: (u32, u32)) -> Result<(), String> {
    let size = match name {
        b"VP8 " => vp8_size(data),
        b"VP8L" => vp8l_size(data),
        _ => None,
    };
    match size {
        Some(s) if s != want => {
            Err(damaged(Format::Webp, format!("its image data is {}×{} pixels, not {}×{}", s.0, s.1, want.0, want.1)))
        }
        _ => Ok(()),
    }
}

/// What std.image reads of a WebP file itself, walking its chunks as
/// image-webp does: where its EXIF chunk is (image-webp would allocate the
/// size the chunk declares, up to 4 GB, before reading it) and the bytes
/// decoding takes besides the decoded image. Every bitstream (VP8, VP8L, and
/// the first chunk of each animation frame) must declare the size of its
/// canvas or frame.
fn webp_layout(b: &[u8], canvas: (u32, u32)) -> Result<(Option<Range<usize>>, u64), String> {
    let px = canvas.0 as u64 * canvas.1 as u64;
    let data = |start: usize, len: usize| &b[start.min(b.len())..start.saturating_add(len).min(b.len())];
    let Some((first, start, len)) = riff_chunk(b, 12) else { return Ok((None, px * 4)) };
    match &first {
        // Its canvas is its bitstream's size. A lossy frame is decoded into
        // planes (1.5 bytes a pixel, in whole macroblocks) from a copy of its
        // partitions; a lossless one without alpha as RGBA first.
        b"VP8 " => return Ok((None, px * 2 + b.len() as u64)),
        b"VP8X" => {}
        _ => return Ok((None, px * 4)),
    }
    let flags = data(start, len).first().copied().unwrap_or(0);
    let (alpha, animated) = (flags & 0x10 != 0, flags & 0x02 != 0);
    let riff = b.get(4..8).map_or(0, |s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]) as usize);
    let mut at = riff_next(start, len);
    let end = at.saturating_add(riff.saturating_sub(12));
    let (mut exif, mut lossless, mut first_frame) = (None, false, None);
    while at < end {
        let Some((name, start, len)) = riff_chunk(b, at) else { break };
        let d = data(start, len);
        match &name {
            b"EXIF" if exif.is_none() && start.saturating_add(len) <= b.len() => exif = Some(start..start + len),
            b"VP8 " | b"VP8L" => {
                lossless |= &name == b"VP8L";
                webp_frame(&name, d, canvas)?;
            }
            b"ANMF" if d.len() >= 16 => {
                let u24 = |i: usize| u32::from_le_bytes([d[i], d[i + 1], d[i + 2], 0]);
                let frame = (u24(6) + 1, u24(9) + 1);
                // The frame's bitstream; after an ALPH chunk, the next chunk
                // is decoded as VP8 whatever its name.
                let mut kind = None;
                if let Some((sub, s, l)) = riff_chunk(d, 16) {
                    kind = Some(sub);
                    if &sub == b"ALPH" {
                        if let Some((_, s, l)) = riff_chunk(d, riff_next(s, l)) {
                            webp_frame(b"VP8 ", &d[s.min(d.len())..s.saturating_add(l).min(d.len())], frame)?;
                        }
                    } else {
                        webp_frame(&sub, &d[s.min(d.len())..s.saturating_add(l).min(d.len())], frame)?;
                    }
                }
                if first_frame.is_none() {
                    first_frame = Some((frame.0 as u64 * frame.1 as u64, kind));
                }
            }
            _ => {}
        }
        at = riff_next(start, len);
    }
    let extra = if animated {
        // A canvas of RGBA, and the first frame: lossless as RGBA; lossy as
        // planes and RGB, or with alpha (decoded as RGBA, then one byte a
        // pixel) as RGBA.
        let (fpx, kind) = first_frame.unwrap_or((px, None));
        px * 4
            + match kind.as_ref() {
                Some(b"VP8L") => fpx * 4,
                Some(b"VP8 ") => fpx * 5,
                _ => fpx * 11,
            }
            + b.len() as u64
    } else if lossless {
        px * 4
    } else {
        px * 2 + if alpha { px * 5 } else { 0 } + b.len() as u64
    };
    Ok((exif, extra))
}

/// The EXIF block of a PNG (its first `eXIf` chunk before the image data), read
/// in place.
fn png_exif(b: &[u8]) -> Option<&[u8]> {
    let mut at = 8;
    while let Some(head) = b.get(at..at + 8) {
        let len = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
        let end = at.checked_add(12)?.checked_add(len)?;
        match &head[4..8] {
            b"IDAT" => return None,
            b"eXIf" => return b.get(at + 8..at + 8 + len),
            _ => at = end,
        }
    }
    None
}

/// A PNG as the decoder sees it: without its ICC profile (which the png crate
/// inflates as soon as it reads the header, up to its whole memory limit),
/// its text and its EXIF block, none of which std.image uses (the orientation
/// is read in place, `png_exif`). Whole chunks are left out, so what is left
/// is a PNG; a chunk cut short or too long for the file ends the walk, and
/// the rest is passed on as it is. Nothing is copied or allocated.
struct PngView<'a> {
    b: &'a [u8],
    /// Where the next byte comes from.
    at: usize,
    /// Up to where the bytes from `at` are passed on before the next chunk.
    until: usize,
    /// Bytes passed on so far.
    pos: u64,
}

impl<'a> PngView<'a> {
    fn new(b: &'a [u8]) -> PngView<'a> {
        // The signature, then chunk by chunk.
        PngView { b, at: 0, until: b.len().min(8), pos: 0 }
    }

    /// At the end of a chunk passed on: skips the chunks left out, and finds
    /// the end of the next one.
    fn next_chunk(&mut self) {
        while self.at == self.until && self.at < self.b.len() {
            let Some(head) = self.b.get(self.at..self.at + 8) else {
                self.until = self.b.len();
                return;
            };
            let len = u32::from_be_bytes([head[0], head[1], head[2], head[3]]);
            let end = (len <= i32::MAX as u32).then(|| self.at + 12 + len as usize).filter(|&end| end <= self.b.len());
            match end {
                Some(end) if matches!(&head[4..8], b"iCCP" | b"tEXt" | b"zTXt" | b"iTXt" | b"eXIf") => {
                    self.at = end;
                    self.until = end;
                }
                Some(end) => self.until = end,
                None => self.until = self.b.len(),
            }
        }
    }
}

impl Read for PngView<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = {
            let src = self.fill_buf()?;
            let n = src.len().min(buf.len());
            buf[..n].copy_from_slice(&src[..n]);
            n
        };
        self.consume(n);
        Ok(n)
    }
}

impl BufRead for PngView<'_> {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        self.next_chunk();
        Ok(&self.b[self.at..self.until])
    }

    fn consume(&mut self, n: usize) {
        let n = n.min(self.until - self.at);
        self.at += n;
        self.pos += n as u64;
    }
}

impl Seek for PngView<'_> {
    /// The png decoder only reads forward: telling where it is is all it may ask.
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        match to {
            SeekFrom::Current(0) => Ok(self.pos),
            SeekFrom::Start(p) if p == self.pos => Ok(self.pos),
            _ => Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "a PNG is read forward")),
        }
    }
}

fn turns_sides(o: Orientation) -> bool {
    matches!(
        o,
        Orientation::Rotate90 | Orientation::Rotate270 | Orientation::Rotate90FlipH | Orientation::Rotate270FlipH
    )
}

/// Reads the header of `bytes`: its format, upright size and orientation.
/// No decoder is asked for metadata, and a decoder that takes a limit gets
/// `HEADER_ALLOC`; a JPEG's header copies its metadata segments (at most
/// twice its size, which the caller reserves).
fn probe(bytes: Vec<u8>) -> Result<(Source, u32, u32), String> {
    let not_image = || "not a PNG, JPEG, GIF or WebP image".to_string();
    let format = Format::sniff(&bytes).ok_or_else(not_image)?;
    let too_large = |w: u32, h: u32| format!("the image is {w}×{h} pixels: {}", caps_text());
    let (w, h, mut need, orientation, jpeg, eight) = if format == Format::Jpeg {
        let head = jpeg_head(&bytes)?;
        let (w, h) = (head.w, head.h);
        if !within_caps(w as u64, h as u64) {
            return Err(too_large(w, h));
        }
        let px = w as u64 * h as u64;
        let most = max_scans(px);
        if head.progressive {
            // Each scan of a progressive JPEG goes over every block of its
            // components, and a scan can be a few bytes (found while fuzzing:
            // 100 scans of a 50-megapixel image took 2.5 seconds). Refused
            // here when its markers say so; zune-jpeg stops at the same count
            // whatever lies between them.
            let scans = jpeg_scans(&bytes) as u64;
            if scans > most {
                return Err(format!(
                    "the progressive JPEG has {scans} scans; one of {w}×{h} pixels may have at most {most}"
                ));
            }
        }
        let eight = px * head.out.num_components() as u64;
        // The pixels, the copies zune-jpeg makes of the metadata segments,
        // and for a progressive JPEG every coefficient of every component as
        // 16-bit numbers, in whole blocks.
        let mut need = eight + bytes.len() as u64;
        if head.progressive {
            let padded = (w as u64 + 16) * (h as u64 + 16);
            need += padded * head.components * 2 + px / 4;
        }
        (w, h, need, head.orientation, Some((head.out, most as usize)), eight)
    } else {
        let d = decoder(&bytes, format, limits(HEADER_ALLOC)).map_err(|e| match e {
            ImageError::Limits(_) => format!("the image is too large: {}", caps_text()),
            e => decode_error(format, e),
        })?;
        let (w, h) = d.dimensions();
        if !within_caps(w as u64, h as u64) {
            return Err(too_large(w, h));
        }
        let px = w as u64 * h as u64;
        let ch = channels(d.color_type()) as u64;
        let native = d.total_bytes();
        let eight = px * ch;
        let mut need = native + if native == eight { 0 } else { eight };
        let exif = match format {
            Format::Png => png_exif(&bytes),
            Format::Webp => {
                let (exif, extra) = webp_layout(&bytes, (w, h))?;
                need += extra;
                exif.map(|r| &bytes[r])
            }
            // An animation's first frame is drawn on a canvas of its own (a
            // frame larger than the screen is refused by the decoder's limit).
            _ => {
                need += px * 4;
                None
            }
        };
        // A broken EXIF block is no reason to refuse the image.
        let orientation = exif.and_then(Orientation::from_exif_chunk).unwrap_or(Orientation::NoTransforms);
        (w, h, need, orientation, None, eight)
    };
    if !matches!(orientation, Orientation::NoTransforms | Orientation::FlipHorizontal | Orientation::FlipVertical) {
        // The upright copy (the 8-bit pixels once more).
        need += eight;
    }
    let (w, h) = if turns_sides(orientation) { (h, w) } else { (w, h) };
    Ok((Source { bytes, format, orientation, need, jpeg }, w, h))
}

/// The channels of the 8-bit pixels made from a decoder's color type.
fn channels(c: image::ColorType) -> usize {
    use image::ColorType as C;
    match c {
        C::L8 | C::L16 => 1,
        C::La8 | C::La16 => 2,
        C::Rgb8 | C::Rgb16 | C::Rgb32F => 3,
        _ => 4,
    }
}

/// Decodes a JPEG with zune-jpeg, in `out`, stopping past `scans` scans.
fn decode_jpeg(bytes: &[u8], out: ColorSpace, scans: usize) -> Result<Pixels, String> {
    let options = jpeg_options(MAX_SIDE, scans).jpeg_set_out_colorspace(out);
    let mut d = zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(bytes), options);
    let data = d.decode().map_err(|e| damaged(Format::Jpeg, e.to_string()))?;
    let (w, h) = d.dimensions().unwrap_or((0, 0));
    let ch = out.num_components();
    if !within_caps(w as u64, h as u64) || data.len() != w * h * ch {
        return Err(damaged(Format::Jpeg, "its size changed while it was read".into()));
    }
    Ok(Pixels { w: w as u32, h: h as u32, ch, data })
}

/// What `decode` says when a decoder needs more than the memory left (a GIF
/// frame larger than its screen): the script ends with the memory error.
const OVER_MEMORY: &str = "decoding it takes more memory than is left";

/// Decodes a source into upright 8-bit pixels; `max_alloc` is what a decoder
/// that takes a limit may allocate besides what `Source::need` counts.
fn decode(src: &Source, max_alloc: u64) -> Result<Pixels, String> {
    let px = match src.jpeg {
        Some((out, scans)) => decode_jpeg(&src.bytes, out, scans)?,
        None => {
            // The caps were checked with the header: a limit the decoder
            // reaches now is the memory left (`OVER_MEMORY`).
            let failed = |e: ImageError| match e {
                ImageError::Limits(_) => OVER_MEMORY.to_string(),
                e => decode_error(src.format, e),
            };
            let d = decoder(&src.bytes, src.format, limits(max_alloc)).map_err(failed)?;
            let img = DynamicImage::from_decoder(d).map_err(failed)?;
            let (w, h) = (img.width(), img.height());
            let (ch, data) = match img {
                DynamicImage::ImageLuma8(b) => (1, b.into_raw()),
                DynamicImage::ImageLumaA8(b) => (2, b.into_raw()),
                DynamicImage::ImageRgb8(b) => (3, b.into_raw()),
                DynamicImage::ImageRgba8(b) => (4, b.into_raw()),
                img @ DynamicImage::ImageLuma16(_) => (1, img.to_luma8().into_raw()),
                img @ DynamicImage::ImageLumaA16(_) => (2, img.to_luma_alpha8().into_raw()),
                img @ (DynamicImage::ImageRgb16(_) | DynamicImage::ImageRgb32F(_)) => (3, img.to_rgb8().into_raw()),
                img => (4, img.to_rgba8().into_raw()),
            };
            Pixels { w, h, ch, data }
        }
    };
    // Bounded by the caps like decoding, and reserved with it (`Source::need`).
    let mut none = || -> Result<(), VmError> { Ok(()) };
    let oriented = match src.orientation {
        Orientation::NoTransforms => Ok(px),
        Orientation::Rotate90 => turn(&px, 1, &mut none),
        Orientation::Rotate180 => turn(&px, 2, &mut none),
        Orientation::Rotate270 => turn(&px, 3, &mut none),
        Orientation::FlipHorizontal => Ok(flip_h(px)),
        Orientation::FlipVertical => Ok(flip_v(px)),
        Orientation::Rotate90FlipH => turn(&px, 1, &mut none).map(flip_h),
        Orientation::Rotate270FlipH => turn(&px, 3, &mut none).map(flip_h),
    };
    oriented.map_err(|_| "the image could not be turned upright".to_string())
}

type Tick<'a> = dyn FnMut() -> Result<(), VmError> + 'a;

// --- Steps on pixels ------------------------------------------------------

fn crop(p: &Pixels, x: u32, y: u32, w: u32, h: u32) -> Pixels {
    let n = w as usize * p.ch;
    let mut data = Vec::with_capacity(n * h as usize);
    for row in y..y + h {
        let start = x as usize * p.ch;
        data.extend_from_slice(&p.row(row)[start..start + n]);
    }
    Pixels { w, h, ch: p.ch, data }
}

/// Turned clockwise by `q` quarter turns.
fn turn(p: &Pixels, q: u8, tick: &mut Tick) -> Result<Pixels, VmError> {
    let ch = p.ch;
    let (w, h) = (p.w as usize, p.h as usize);
    if q == 2 {
        let mut data = Vec::with_capacity(p.data.len());
        for (i, px) in p.data.chunks_exact(ch).rev().enumerate() {
            data.extend_from_slice(px);
            if i.is_multiple_of(w * ROWS_PER_CHECK as usize) {
                tick()?;
            }
        }
        return Ok(Pixels { w: p.w, h: p.h, ch, data });
    }
    let mut data = vec![0u8; p.data.len()];
    // The new image is h wide and w tall.
    for y in 0..h {
        if y.is_multiple_of(ROWS_PER_CHECK as usize) {
            tick()?;
        }
        for x in 0..w {
            let (nx, ny) = if q == 1 { (h - 1 - y, x) } else { (y, w - 1 - x) };
            let from = (y * w + x) * ch;
            let to = (ny * h + nx) * ch;
            data[to..to + ch].copy_from_slice(&p.data[from..from + ch]);
        }
    }
    Ok(Pixels { w: p.h, h: p.w, ch, data })
}

fn flip_h(mut p: Pixels) -> Pixels {
    let (n, ch) = (p.w as usize * p.ch, p.ch);
    for row in p.data.chunks_exact_mut(n) {
        let w = row.len() / ch;
        for x in 0..w / 2 {
            for c in 0..ch {
                row.swap(x * ch + c, (w - 1 - x) * ch + c);
            }
        }
    }
    p
}

fn flip_v(mut p: Pixels) -> Pixels {
    let n = p.w as usize * p.ch;
    let h = p.h as usize;
    for y in 0..h / 2 {
        let (top, bottom) = p.data.split_at_mut((h - 1 - y) * n);
        top[y * n..(y + 1) * n].swap_with_slice(&mut bottom[..n]);
    }
    p
}

// --- Resampling -------------------------------------------------------------

/// Lanczos with 3 lobes.
fn lanczos3(x: f64) -> f64 {
    fn sinc(x: f64) -> f64 {
        if x == 0.0 {
            1.0
        } else {
            let a = x * std::f64::consts::PI;
            a.sin() / a
        }
    }
    if x.abs() < 3.0 {
        sinc(x) * sinc(x / 3.0)
    } else {
        0.0
    }
}

/// The source pixels each output pixel of one direction is made of, with
/// their weights (as Pillow computes them).
struct Weights {
    start: Vec<u32>,
    len: Vec<u32>,
    stride: usize,
    w: Vec<f32>,
}

impl Weights {
    fn new(src: u32, dst: u32) -> Weights {
        let scale = src as f64 / dst as f64;
        let fscale = scale.max(1.0);
        let support = 3.0 * fscale;
        let stride = (support * 2.0).ceil() as usize + 2;
        let mut start = Vec::with_capacity(dst as usize);
        let mut len = Vec::with_capacity(dst as usize);
        let mut w = vec![0f32; stride * dst as usize];
        let mut ks = vec![0f64; stride];
        for i in 0..dst as usize {
            let center = (i as f64 + 0.5) * scale;
            let lo = ((center - support + 0.5).floor().max(0.0) as u32).min(src - 1);
            let hi = ((center + support + 0.5).floor() as u32).clamp(lo + 1, src);
            let n = ((hi - lo) as usize).min(stride);
            let mut sum = 0.0;
            for (j, k) in ks.iter_mut().enumerate().take(n) {
                *k = lanczos3((j as f64 + lo as f64 - center + 0.5) / fscale);
                sum += *k;
            }
            for j in 0..n {
                w[i * stride + j] = if sum != 0.0 { (ks[j] / sum) as f32 } else { (1.0 / n as f64) as f32 };
            }
            start.push(lo);
            len.push(n as u32);
        }
        Weights { start, len, stride, w }
    }

    fn of(&self, i: usize) -> (usize, &[f32]) {
        let n = self.len[i] as usize;
        (self.start[i] as usize, &self.w[i * self.stride..i * self.stride + n])
    }
}

/// Rows of numbers `resample` keeps at once: output rows being added to when
/// it shrinks vertically, source rows resampled horizontally when it grows.
/// A Lanczos-3 window spans 6 rows of the smaller side, so a few at most.
const OPEN_ROWS: u64 = 10;

/// Bytes `resample` holds besides its source and result.
fn resample_need(sw: u32, sh: u32, dw: u32, dh: u32, ch: usize) -> u64 {
    let rows = OPEN_ROWS.min(dh.min(sh) as u64);
    let weights = (sw as u64 + sh as u64) * 4 * 8 + (dw as u64 + dh as u64) * 16;
    (rows + 2) * dw as u64 * ch as u64 * 4 + sw as u64 * ch as u64 * 4 + weights
}

/// One source row as numbers, with the colors multiplied by alpha.
fn premultiplied(row: &[u8], ch: usize, alpha: bool, out: &mut [f32]) {
    if !alpha {
        for (o, &v) in out.iter_mut().zip(row) {
            *o = v as f32;
        }
        return;
    }
    for (o, px) in out.chunks_exact_mut(ch).zip(row.chunks_exact(ch)) {
        let a = px[ch - 1] as f32;
        for c in 0..ch - 1 {
            o[c] = px[c] as f32 * a / 255.0;
        }
        o[ch - 1] = a;
    }
}

/// A row resampled horizontally.
fn horizontal(src: &[f32], ch: usize, wx: &Weights, out: &mut [f32]) {
    for (x, o) in out.chunks_exact_mut(ch).enumerate() {
        let (lo, ws) = wx.of(x);
        o.fill(0.0);
        for (j, &k) in ws.iter().enumerate() {
            let s = &src[(lo + j) * ch..(lo + j + 1) * ch];
            for c in 0..ch {
                o[c] += s[c] * k;
            }
        }
    }
}

/// Numbers back to 8-bit pixels, the colors divided by alpha again.
fn to_bytes(src: &[f32], ch: usize, alpha: bool, out: &mut [u8]) {
    let clamp = |v: f32| v.round().clamp(0.0, 255.0) as u8;
    if !alpha {
        for (o, &v) in out.iter_mut().zip(src) {
            *o = clamp(v);
        }
        return;
    }
    for (o, px) in out.chunks_exact_mut(ch).zip(src.chunks_exact(ch)) {
        let a = px[ch - 1].clamp(0.0, 255.0);
        for c in 0..ch - 1 {
            o[c] = if a > 0.0 { clamp(px[c] * 255.0 / a) } else { 0 };
        }
        o[ch - 1] = clamp(a);
    }
}

/// `p` resampled to `dw` × `dh` with a Lanczos-3 filter, in two passes, row
/// by row. When it shrinks vertically, each source row is resampled
/// horizontally and added to the output rows it weighs in, and an output row
/// is written once the last of its source rows has passed; when it grows,
/// each output row is made from the few source rows it needs, resampled
/// horizontally once each. Either way only a few rows of numbers are kept, and
/// the limits are checked every few rows.
fn resample(p: &Pixels, dw: u32, dh: u32, tick: &mut Tick) -> Result<Pixels, VmError> {
    if (dw, dh) == (p.w, p.h) {
        return Ok(p.clone());
    }
    let (ch, alpha) = (p.ch, p.alpha());
    let (sh, dh) = (p.h as usize, dh as usize);
    let wx = Weights::new(p.w, dw);
    let wy = Weights::new(p.h, dh as u32);
    let row_out = dw as usize * ch;
    let mut src_row = vec![0f32; p.w as usize * ch];
    let mut out = vec![0u8; row_out * dh];
    let end = |oy: usize| (wy.start[oy] + wy.len[oy]) as usize;
    if dh <= sh {
        // How many output rows are open at once (their windows hold `y`).
        let mut open = 1;
        let mut first = 0;
        for y in 0..sh {
            while first < dh && end(first) <= y {
                first += 1;
            }
            let last = (first..dh).take_while(|&oy| wy.start[oy] as usize <= y).count();
            open = open.max(last);
        }
        let mut acc = vec![0f32; open * row_out];
        let mut hrow = vec![0f32; row_out];
        let mut first = 0;
        let finish = |oy: usize, acc: &mut [f32], out: &mut [u8]| {
            let slot = &mut acc[(oy % open) * row_out..(oy % open + 1) * row_out];
            to_bytes(slot, ch, alpha, &mut out[oy * row_out..(oy + 1) * row_out]);
            slot.fill(0.0);
        };
        for y in 0..sh {
            if (y as u32).is_multiple_of(ROWS_PER_CHECK) {
                tick()?;
            }
            while first < dh && end(first) <= y {
                finish(first, &mut acc, &mut out);
                first += 1;
            }
            if first >= dh || (wy.start[first] as usize) > y {
                continue;
            }
            premultiplied(p.row(y as u32), ch, alpha, &mut src_row);
            horizontal(&src_row, ch, &wx, &mut hrow);
            let mut oy = first;
            while oy < dh && (wy.start[oy] as usize) <= y {
                let (lo, ws) = wy.of(oy);
                if let Some(&k) = ws.get(y - lo) {
                    let slot = &mut acc[(oy % open) * row_out..(oy % open + 1) * row_out];
                    for (a, &v) in slot.iter_mut().zip(&hrow) {
                        *a += v * k;
                    }
                }
                oy += 1;
            }
        }
        while first < dh {
            finish(first, &mut acc, &mut out);
            first += 1;
        }
    } else {
        // Source rows resampled horizontally, kept by row number modulo `keep`.
        let keep = (0..dh).map(|oy| wy.len[oy] as usize).max().unwrap_or(1);
        let mut rows = vec![0f32; keep * row_out];
        let mut made = 0;
        let mut vrow = vec![0f32; row_out];
        for oy in 0..dh {
            if (oy as u32).is_multiple_of(ROWS_PER_CHECK) {
                tick()?;
            }
            let (lo, ws) = wy.of(oy);
            made = made.max(lo);
            while made < lo + ws.len() {
                premultiplied(p.row(made as u32), ch, alpha, &mut src_row);
                horizontal(&src_row, ch, &wx, &mut rows[(made % keep) * row_out..(made % keep + 1) * row_out]);
                made += 1;
            }
            vrow.fill(0.0);
            for (j, &k) in ws.iter().enumerate() {
                let r = (lo + j) % keep;
                for (a, &v) in vrow.iter_mut().zip(&rows[r * row_out..(r + 1) * row_out]) {
                    *a += v * k;
                }
            }
            to_bytes(&vrow, ch, alpha, &mut out[oy * row_out..(oy + 1) * row_out]);
        }
    }
    Ok(Pixels { w: dw, h: dh as u32, ch, data: out })
}

// --- Writing ------------------------------------------------------------------

fn color(ch: usize) -> ExtendedColorType {
    match ch {
        1 => ExtendedColorType::L8,
        2 => ExtendedColorType::La8,
        3 => ExtendedColorType::Rgb8,
        _ => ExtendedColorType::Rgba8,
    }
}

/// The pixels on white, without alpha (for JPEG).
fn on_white(p: &Pixels) -> Pixels {
    let ch = p.ch - 1;
    let mut data = Vec::with_capacity(p.w as usize * p.h as usize * ch);
    for px in p.data.chunks_exact(p.ch) {
        let a = px[ch] as u32;
        for &c in &px[..ch] {
            data.push(((c as u32 * a + 255 * (255 - a) + 127) / 255) as u8);
        }
    }
    Pixels { w: p.w, h: p.h, ch, data }
}

/// Gray pixels as RGB (for WebP, whose encoder takes RGB or RGBA).
fn as_rgb(p: &Pixels) -> Pixels {
    let ch = p.ch + 2;
    let mut data = Vec::with_capacity(p.w as usize * p.h as usize * ch);
    for px in p.data.chunks_exact(p.ch) {
        data.extend_from_slice(&[px[0], px[0], px[0]]);
        if p.ch == 2 {
            data.push(px[1]);
        }
    }
    Pixels { w: p.w, h: p.h, ch, data }
}

/// Bytes encoding `p` takes besides `p`, as measured (`measure_encoder_memory`
/// in the driver's image tests; the most of noise, which compresses least):
/// the PNG encoder 3.7 times the pixels it is given (the filtered rows, the
/// compressed stream, a stored copy when that is smaller, the output
/// growing), the lossless WebP encoder 3.9 times, the JPEG encoder 2.3 times
/// at quality 100; and the copy made first for WebP (gray as RGB) and JPEG
/// (alpha on white).
fn encode_need(p: &Pixels, format: Format) -> u64 {
    let n = p.data.len() as u64;
    let px = n / p.ch as u64;
    let extra = match format {
        Format::Png => n * 4,
        Format::Webp if p.ch < 3 => {
            let rgb = px * (p.ch as u64 + 2);
            rgb + rgb * 4
        }
        Format::Webp => n * 4,
        Format::Jpeg if p.alpha() => {
            let flat = px * (p.ch as u64 - 1);
            flat + flat * 5 / 2
        }
        Format::Jpeg => n * 5 / 2,
        Format::Gif => 0,
    };
    extra + (64 << 10)
}

fn encode(p: &Pixels, format: Format, quality: u8) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let failed = |e: ImageError| format!("the image could not be written as {}: {e}", format.name().to_uppercase());
    match format {
        Format::Jpeg => {
            let flat;
            let p = if p.alpha() {
                flat = on_white(p);
                &flat
            } else {
                p
            };
            JpegEncoder::new_with_quality(&mut out, quality)
                .write_image(&p.data, p.w, p.h, color(p.ch))
                .map_err(failed)?
        }
        Format::Png => PngEncoder::new_with_quality(&mut out, CompressionType::Fast, FilterType::Adaptive)
            .write_image(&p.data, p.w, p.h, color(p.ch))
            .map_err(failed)?,
        Format::Webp => {
            let rgb;
            let p = if p.ch < 3 {
                rgb = as_rgb(p);
                &rgb
            } else {
                p
            };
            WebPEncoder::new_lossless(&mut out).encode(&p.data, p.w, p.h, color(p.ch)).map_err(failed)?
        }
        Format::Gif => return Err("GIF is not written; use \"png\"".into()),
    }
    Ok(out)
}

// --- The steps of an Image ----------------------------------------------------

/// Adds a step to `p`, checking its arguments.
fn step(p: &Pipeline, op: &str, a: i64, b: i64, c: i64, d: i64) -> Result<Pipeline, String> {
    let (w, h) = (p.width as i64, p.height as i64);
    let mut ops = p.ops.clone();
    let size = |what: &str, nw: i64, nh: i64| -> Result<(u32, u32), String> {
        if within_caps(nw.max(0) as u64, nh.max(0) as u64) {
            Ok((nw as u32, nh as u32))
        } else {
            Err(format!("{what}(): {nw}×{nh} is not a size an image may have: {}", caps_text()))
        }
    };
    let (nw, nh) = match op {
        "fit" => {
            // `b` is 0 for no height (null).
            if a < 1 || b < 0 {
                let (what, n) = if a < 1 { ("width", a) } else { ("height", b) };
                return Err(format!("fit(): the {what} must be at least 1, not {n}"));
            }
            let mut s = (a as f64 / w as f64).min(1.0);
            if b > 0 {
                s = s.min(b as f64 / h as f64);
            }
            let nw = ((w as f64 * s).round() as i64).clamp(1, w.min(a));
            let nh = ((h as f64 * s).round() as i64).clamp(1, if b > 0 { h.min(b) } else { h });
            if (nw, nh) != (w, h) {
                ops.push(Op::Resize(nw as u32, nh as u32));
            }
            (nw, nh)
        }
        "cover" => {
            let (tw, th) = size("cover", a, b)?;
            // The middle part with the box's proportions, then scaled to it.
            let (cw, chh) = if w * th as i64 > h * tw as i64 {
                (((h as f64 * tw as f64 / th as f64).round() as i64).clamp(1, w), h)
            } else {
                (w, ((w as f64 * th as f64 / tw as f64).round() as i64).clamp(1, h))
            };
            if (cw, chh) != (w, h) {
                ops.push(Op::Crop(((w - cw) / 2) as u32, ((h - chh) / 2) as u32, cw as u32, chh as u32));
            }
            if (tw as i64, th as i64) != (cw, chh) {
                ops.push(Op::Resize(tw, th));
            }
            (tw as i64, th as i64)
        }
        "resize" => {
            let (nw, nh) = size("resize", a, b)?;
            if (nw as i64, nh as i64) != (w, h) {
                ops.push(Op::Resize(nw, nh));
            }
            (nw as i64, nh as i64)
        }
        "crop" => {
            let inside = a >= 0 && b >= 0 && c >= 1 && d >= 1 && a.saturating_add(c) <= w && b.saturating_add(d) <= h;
            if !inside {
                return Err(format!("crop(): the part {c}×{d} at ({a}, {b}) is not inside the image of {w}×{h}"));
            }
            if (c, d) != (w, h) {
                ops.push(Op::Crop(a as u32, b as u32, c as u32, d as u32));
            }
            (c, d)
        }
        "rotate" => {
            if a % 90 != 0 {
                return Err(format!("rotate(): {a} is not a multiple of 90 degrees"));
            }
            let q = (a / 90).rem_euclid(4) as u8;
            if q != 0 {
                ops.push(Op::Turn(q));
            }
            if q % 2 == 1 {
                (h, w)
            } else {
                (w, h)
            }
        }
        "fliph" => {
            ops.push(Op::FlipH);
            (w, h)
        }
        "flipv" => {
            ops.push(Op::FlipV);
            (w, h)
        }
        _ => return Err(format!("unknown step {op}")),
    };
    if ops.len() > MAX_STEPS {
        return Err(format!(
            "an image may be made with at most {MAX_STEPS} steps; write it (toBase64()) and read it again"
        ));
    }
    Ok(Pipeline { src: p.src.clone(), ops, width: nw as u32, height: nh as u32 })
}

/// Bytes a step takes while it runs, besides its input.
fn step_need(p: &Pixels, op: Op) -> u64 {
    let bytes = |w: u32, h: u32| w as u64 * h as u64 * p.ch as u64;
    match op {
        Op::Resize(w, h) => bytes(w, h) + resample_need(p.w, p.h, w, h, p.ch),
        Op::Crop(_, _, w, h) => bytes(w, h),
        Op::Turn(_) => bytes(p.w, p.h),
        Op::FlipH | Op::FlipV => 0,
    }
}

fn apply(p: Pixels, op: Op, tick: &mut Tick) -> Result<Pixels, VmError> {
    Ok(match op {
        Op::Resize(w, h) => resample(&p, w, h, tick)?,
        Op::Crop(x, y, w, h) => crop(&p, x, y, w, h),
        Op::Turn(q) => turn(&p, q, tick)?,
        Op::FlipH => flip_h(p),
        Op::FlipV => flip_v(p),
    })
}

/// Decodes the source of `pipe`, runs its steps and encodes the result, within
/// the script's limits.
fn render(vm: &Vm, what: &str, pipe: &Pipeline, format: Format, quality: u8) -> Result<Vec<u8>, Fail> {
    vm.check_limits()?;
    let need = pipe.src.need;
    vm.reserve(need.min(usize::MAX as u64) as usize)?;
    // `reserve` only checks: the decoders that take a limit get what is left
    // beyond what was reserved for them (a GIF frame larger than its screen,
    // png's buffers).
    let left = (vm.memory_left() as u64).saturating_sub(need);
    let mut px = guarded(what, || {
        decode(&pipe.src, left).map_err(|m| match m.as_str() {
            // The usual memory error, as when the reservation fails.
            OVER_MEMORY => Fail::Vm(vm.reserve(usize::MAX).err().unwrap_or_else(|| vm.throw("ValueError", m))),
            _ => Fail::Msg(m),
        })
    })?;
    vm.check_limits()?;
    let mut tick = || vm.check_limits();
    for &op in &pipe.ops {
        vm.reserve(step_need(&px, op).min(usize::MAX as u64) as usize)?;
        px = guarded(what, || apply(px, op, &mut tick).map_err(Fail::Vm))?;
        vm.check_limits()?;
    }
    vm.reserve(encode_need(&px, format).min(usize::MAX as u64) as usize)?;
    let out = guarded(what, || encode(&px, format, quality).map_err(Fail::Msg))?;
    drop(px);
    vm.check_limits()?;
    Ok(out)
}

// --- Builtins -----------------------------------------------------------------

fn add(vm: &mut Vm, p: Pipeline) -> Value {
    vm.resources.push(Some(Box::new(p)));
    Value::Int(vm.resources.len() as i64 - 1)
}

fn get(vm: &Vm, h: &Value) -> Result<(Rc<Source>, Vec<Op>, u32, u32), VmError> {
    let p = usize::try_from(int(h))
        .ok()
        .and_then(|i| vm.resources.get(i))
        .and_then(|r| r.as_ref())
        .and_then(|b| b.downcast_ref::<Pipeline>())
        .ok_or_else(|| vm.throw("ValueError", "not an image"))?;
    Ok((p.src.clone(), p.ops.clone(), p.width, p.height))
}

fn pipeline(vm: &Vm, h: &Value) -> Result<Pipeline, VmError> {
    let (src, ops, width, height) = get(vm, h)?;
    Ok(Pipeline { src, ops, width, height })
}

fn fail(vm: &Vm, e: Fail) -> VmError {
    match e {
        Fail::Vm(e) => e,
        Fail::Msg(m) => vm.throw("ValueError", m),
    }
}

/// Reads the header of `bytes` (which `vm` holds already) into a new image.
fn read(vm: &mut Vm, what: &str, bytes: Vec<u8>) -> Result<Value, VmError> {
    if bytes.len() > MAX_INPUT {
        return Err(vm.throw("ValueError", format!("{what}: images are read from at most {} MB", MAX_INPUT >> 20)));
    }
    // zune-jpeg copies a JPEG's metadata segments while it reads the header,
    // and its header is copied once more (`jpeg_head`): at most twice the file.
    if Format::sniff(&bytes) == Some(Format::Jpeg) {
        vm.reserve(bytes.len().saturating_mul(2))?;
    }
    let probed = guarded(what, || probe(bytes).map_err(Fail::Msg));
    let (src, width, height) = probed.map_err(|e| match e {
        Fail::Msg(m) if !m.starts_with(what) => vm.throw("ValueError", format!("{what}: {m}")),
        e => fail(vm, e),
    })?;
    Ok(add(vm, Pipeline { src: Rc::new(src), ops: Vec::new(), width, height }))
}

/// The format and quality to write in: `name`, or the source's (a GIF as PNG).
fn out_format(vm: &Vm, what: &str, name: &str, src: Format, quality: i64) -> Result<(Format, u8), VmError> {
    let format = match name.to_ascii_lowercase().as_str() {
        "" if src == Format::Gif => Format::Png,
        "" => src,
        "png" => Format::Png,
        "jpeg" | "jpg" => Format::Jpeg,
        "webp" => Format::Webp,
        "gif" => return Err(vm.throw("ValueError", format!("{what}: GIF is not written; use \"png\""))),
        _ => {
            return Err(vm.throw(
                "ValueError",
                format!("{what}: unknown format \"{}\" (\"png\", \"jpeg\" or \"webp\")", name.escape_debug()),
            ))
        }
    };
    if !(1..=100).contains(&quality) {
        return Err(vm.throw("ValueError", format!("{what}: the quality must be 1 to 100, not {quality}")));
    }
    Ok((format, quality as u8))
}

fn written(vm: &Vm, what: &str, h: &Value, format: &str, quality: i64) -> Result<(Vec<u8>, Format), VmError> {
    let p = pipeline(vm, h)?;
    let (format, quality) = out_format(vm, what, format, p.src.format, quality)?;
    let bytes = render(vm, what, &p, format, quality).map_err(|e| match e {
        Fail::Msg(m) if !m.starts_with(what) => vm.throw("ValueError", format!("{what}: {m}")),
        e => fail(vm, e),
    })?;
    Ok((bytes, format))
}

/// Text from Base64 or a `data:` URL, decoded.
fn from_base64(text: &str) -> Result<Vec<u8>, String> {
    let data = match text.strip_prefix("data:") {
        Some(rest) => {
            let (head, data) = rest.split_once(',').ok_or("the data: URL has no data")?;
            if !head.to_ascii_lowercase().ends_with(";base64") {
                return Err("the data: URL is not in Base64".into());
            }
            data
        }
        None => text,
    };
    if data.len() > MAX_INPUT / 3 * 4 + 4096 {
        return Err(format!("images are read from at most {} MB", MAX_INPUT >> 20));
    }
    let clean: Vec<u8> = data.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    const LENIENT: GeneralPurposeConfig =
        GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent);
    const STD: GeneralPurpose = GeneralPurpose::new(&alphabet::STANDARD, LENIENT);
    const URL: GeneralPurpose = GeneralPurpose::new(&alphabet::URL_SAFE, LENIENT);
    STD.decode(&clean).or_else(|_| URL.decode(&clean)).map_err(|_| "the text is not Base64".to_string())
}

/// The file at the project path `rel`, read within the caps.
fn read_file(vm: &Vm, rel: &str) -> Result<Vec<u8>, VmError> {
    let (dir, name) = crate::sys::resolve_path(vm, rel, false)?;
    let f = dir.open(&name, false).map_err(|e| crate::sys::fs_error(vm, rel, "", e))?;
    let size = f.metadata().map(|m| m.len()).unwrap_or(0);
    let too_large =
        || vm.throw("ValueError", format!("Image::open(): images are read from at most {} MB", MAX_INPUT >> 20));
    if size > MAX_INPUT as u64 {
        return Err(too_large());
    }
    vm.reserve(size as usize)?;
    let mut bytes = Vec::with_capacity(size as usize);
    f.take(MAX_INPUT as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| vm.throw("RuntimeException", format!("cannot read \"{rel}\": {}", e.kind())))?;
    if bytes.len() > MAX_INPUT {
        return Err(too_large());
    }
    Ok(bytes)
}

pub(crate) fn lookup(name: &str) -> Option<BuiltinFn> {
    let f: BuiltinFn = match name {
        "__img_upload" => |vm, a| {
            let what = "Image::fromUpload()";
            let len = usize::try_from(int(&a[0])).ok().and_then(|i| vm.http.files.get(i)).map(|f| f.data.len());
            let Some(len) = len else { return Err(vm.throw("ValueError", format!("{what}: no such upload"))) };
            if len > MAX_INPUT {
                return Err(
                    vm.throw("ValueError", format!("{what}: images are read from at most {} MB", MAX_INPUT >> 20))
                );
            }
            vm.reserve(len)?;
            let bytes = vm.http.files[int(&a[0]) as usize].data.clone();
            read(vm, what, bytes)
        },
        "__img_open" => |vm, a| {
            let bytes = read_file(vm, s(&a[0]))?;
            read(vm, "Image::open()", bytes)
        },
        "__img_base64" => |vm, a| {
            let what = "Image::fromBase64()";
            vm.reserve(s(&a[0]).len())?;
            let bytes = from_base64(s(&a[0])).map_err(|e| vm.throw("ValueError", format!("{what}: {e}")))?;
            read(vm, what, bytes)
        },
        "__img_size" => |vm, a| {
            let (_, _, w, h) = get(vm, &a[0])?;
            Ok(Value::list(vec![Value::Int(w as i64), Value::Int(h as i64)]))
        },
        "__img_format" => |vm, a| Ok(Value::str(get(vm, &a[0])?.0.format.name())),
        "__img_op" => |vm, a| {
            let p = pipeline(vm, &a[0])?;
            let next = step(&p, s(&a[1]), int(&a[2]), int(&a[3]), int(&a[4]), int(&a[5]))
                .map_err(|e| vm.throw("ValueError", e))?;
            Ok(add(vm, next))
        },
        "__img_send" => |vm, a| {
            crate::web::unsent(vm, "the response")?;
            let (bytes, format) = written(vm, "send()", &a[0], s(&a[1]), int(&a[2]))?;
            crate::set_type(vm, format.mime())?;
            vm.http.body_file = None;
            vm.http.body_bytes = Some(bytes);
            Ok(Value::Null)
        },
        "__img_save" => |vm, a| {
            let what = "saveTo()";
            let rel = s(&a[1]).to_string();
            let mut format = s(&a[2]).to_string();
            if format.is_empty() {
                let ext = rel.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
                if !matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "webp") {
                    return Err(vm.throw(
                        "ValueError",
                        format!("{what}: \"{rel}\" does not end in .png, .jpg, .jpeg or .webp; give the format"),
                    ));
                }
                format = ext;
            }
            // Refused before the work when it may not be written.
            let (dir, name) = crate::sys::resolve_path(vm, &rel, true)?;
            let (bytes, _) = written(vm, what, &a[0], &format, int(&a[3]))?;
            let mut f = dir.open(&name, true).map_err(|e| crate::sys::fs_error(vm, &rel, "", e))?;
            std::io::Write::write_all(&mut f, &bytes)
                .map_err(|e| vm.throw("RuntimeException", format!("cannot write \"{rel}\": {}", e.kind())))?;
            Ok(Value::Null)
        },
        "__img_to_base64" => |vm, a| {
            let (bytes, _) = written(vm, "toBase64()", &a[0], s(&a[1]), int(&a[2]))?;
            vm.reserve(bytes.len() / 3 * 4 + 4)?;
            Ok(Value::str(STANDARD.encode(&bytes)))
        },
        "__img_data_url" => |vm, a| {
            let (bytes, format) = written(vm, "toDataUrl()", &a[0], s(&a[1]), int(&a[2]))?;
            vm.reserve(bytes.len() / 3 * 4 + 64)?;
            // Base64 and the media type hold nothing that HTML escaping changes.
            Ok(Value::html(format!("data:{};base64,{}", format.mime(), STANDARD.encode(&bytes))))
        },
        _ => return None,
    };
    Some(f)
}

#[cfg(test)]
mod tests;
