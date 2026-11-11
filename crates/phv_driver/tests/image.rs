//! `std.image` under the limits of a run: what an image takes counts toward the
//! memory limit before it is decoded, long work stops at the time limit,
//! files go through `[permissions]`, and `send()` replaces the response.
//!
//! `measure_image_costs` (ignored) prints the time and memory of a typical
//! upload and of the worst cases at the caps; run it with
//! `cargo test --release -p phv_driver --test image -- --ignored --nocapture`.

use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{ExtendedColorType, ImageEncoder};
use phv_driver::{compile, run, RunOptions};
use phv_vm::alloc::{reset_thread_peak, thread_allocated, thread_peak, CountingAlloc};
use phv_vm::{Host, HttpCtx, Limits, Permissions};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

/// A project with `data/` readable and `out/` writable.
fn project(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("phv-image-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("data")).unwrap();
    std::fs::create_dir_all(dir.join("out")).unwrap();
    dir
}

fn host(dir: &Path) -> Arc<Host> {
    Arc::new(Host {
        root: dir.to_path_buf(),
        permissions: Permissions { read: vec!["data/".into()], write: vec!["out/".into()], ..Permissions::default() },
        ..Host::default()
    })
}

struct Ran {
    out: String,
    peak: usize,
    took: Duration,
    http: HttpCtx,
}

fn run_in(dir: &Path, src: &str, limits: Limits) -> Ran {
    let entry = dir.join("main.phv");
    std::fs::write(&entry, src).unwrap();
    let c = compile(dir, &entry).unwrap_or_else(|e| panic!("{}", e.render(false)));
    let base = thread_allocated();
    reset_thread_peak();
    let start = Instant::now();
    let outcome = run(&c, RunOptions { html: false, limits, http: Some(HttpCtx::default()), host: Some(host(dir)) });
    let took = start.elapsed();
    let peak = (thread_peak() - base).max(0) as usize;
    let err = outcome.error.map(|e| e.render().lines().next().unwrap_or_default().to_string());
    Ran { out: format!("{}{}", outcome.output, err.unwrap_or_default()), peak, took, http: outcome.http }
}

/// Something like a photo: gradients with noise, so it compresses as photos do.
fn photo(w: u32, h: u32) -> Vec<u8> {
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut data = Vec::with_capacity(w as usize * h as usize * 3);
    for y in 0..h {
        for x in 0..w {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let n = (seed % 24) as u32;
            data.push(((x * 200 / w) + n) as u8);
            data.push(((y * 200 / h) + n) as u8);
            data.push((((x + y) * 100 / (w + h)) + n * 2) as u8);
        }
    }
    data
}

fn jpeg(w: u32, h: u32, quality: u8) -> Vec<u8> {
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, quality).write_image(&photo(w, h), w, h, ExtendedColorType::Rgb8).unwrap();
    out
}

fn png(w: u32, h: u32) -> Vec<u8> {
    let mut out = Vec::new();
    PngEncoder::new_with_quality(&mut out, CompressionType::Fast, FilterType::Adaptive)
        .write_image(&photo(w, h), w, h, ExtendedColorType::Rgb8)
        .unwrap();
    out
}

/// Gradients without noise: large images that still fit in 32 MB.
fn smooth(w: u32, h: u32) -> Vec<u8> {
    photo(w, h).into_iter().map(|v| v & 0xF0).collect()
}

fn png_smooth(w: u32, h: u32) -> Vec<u8> {
    let mut out = Vec::new();
    // Bands of equal rows: inflating it is all of a PNG's work.
    let mut data = Vec::with_capacity(w as usize * h as usize * 3);
    for y in 0..h {
        for x in 0..w {
            data.extend_from_slice(&[(x * 255 / w) as u8, (y / 16 * 16 * 255 / h) as u8, 128]);
        }
    }
    PngEncoder::new_with_quality(&mut out, CompressionType::Best, FilterType::Adaptive)
        .write_image(&data, w, h, ExtendedColorType::Rgb8)
        .unwrap();
    out
}

fn webp_smooth(w: u32, h: u32) -> Vec<u8> {
    let mut out = Vec::new();
    image::codecs::webp::WebPEncoder::new_lossless(&mut out)
        .encode(&smooth(w, h), w, h, ExtendedColorType::Rgb8)
        .unwrap();
    out
}

/// A GIF of 256 colors in a pattern.
fn gif_pattern(w: u32, h: u32) -> Vec<u8> {
    let mut img = image::RgbaImage::new(w, h);
    for (x, y, p) in img.enumerate_pixels_mut() {
        let v = ((x / 3) ^ (y / 5)) as u8;
        *p = image::Rgba([v, v.wrapping_mul(7), 255 - v, 255]);
    }
    let mut out = Vec::new();
    {
        let mut e = image::codecs::gif::GifEncoder::new_with_speed(&mut out, 30);
        e.encode_frame(image::Frame::new(img)).unwrap();
    }
    out
}

/// Writes bits of a JPEG scan, with the stuffed zero after every 0xFF.
struct Bits {
    out: Vec<u8>,
    acc: u32,
    n: u32,
}

impl Bits {
    fn put(&mut self, value: u32, len: u32) {
        for i in (0..len).rev() {
            self.acc = (self.acc << 1) | ((value >> i) & 1);
            self.n += 1;
            if self.n == 8 {
                self.out.push(self.acc as u8);
                if self.acc == 0xFF {
                    self.out.push(0);
                }
                self.acc = 0;
                self.n = 0;
            }
        }
    }

    fn end(mut self) -> Vec<u8> {
        while self.n != 0 {
            self.put(1, 1);
        }
        self.out
    }
}

fn segment(out: &mut Vec<u8>, marker: u8, body: &[u8]) {
    out.extend_from_slice(&[0xFF, marker]);
    out.extend_from_slice(&((body.len() + 2) as u16).to_be_bytes());
    out.extend_from_slice(body);
}

/// A progressive JPEG of `w` × `h` whose coefficients are all zero: a DC scan
/// and then AC refinement scans over the whole band of one component after
/// another, up to `scans` in all (zune-jpeg reads at most 100). Each
/// refinement looks at every coefficient of every block of its component and
/// takes a few bytes: the most work a JPEG this small can ask for.
fn progressive_bomb(w: u16, h: u16, comps: u8, scans: usize) -> Vec<u8> {
    let mut out = vec![0xFF, 0xD8];
    let mut dqt = vec![0u8];
    dqt.extend_from_slice(&[1; 64]);
    segment(&mut out, 0xDB, &dqt);
    let mut sof = vec![8];
    sof.extend_from_slice(&h.to_be_bytes());
    sof.extend_from_slice(&w.to_be_bytes());
    sof.push(comps);
    for c in 1..=comps {
        sof.extend_from_slice(&[c, 0x11, 0]);
    }
    segment(&mut out, 0xC2, &sof);
    // DC table 0: one code ("0") for category 0.
    let mut dht = vec![0x00, 1];
    dht.extend_from_slice(&[0; 15]);
    dht.push(0);
    segment(&mut out, 0xC4, &dht);
    // AC table 0: EOB runs 0 to 14, four bits each.
    let mut dht = vec![0x10, 0, 0, 0, 15];
    dht.extend_from_slice(&[0; 12]);
    dht.extend((0..15u8).map(|r| r << 4));
    segment(&mut out, 0xC4, &dht);
    let blocks = (w as u32).div_ceil(8) * (h as u32).div_ceil(8);
    // The DC scan, all components interleaved: "0" per block.
    let mut sos = vec![comps];
    for c in 1..=comps {
        sos.extend_from_slice(&[c, 0x00]);
    }
    sos.extend_from_slice(&[0, 0, 0]);
    segment(&mut out, 0xDA, &sos);
    let mut bits = Bits { out: Vec::new(), acc: 0, n: 0 };
    for _ in 0..blocks * comps as u32 {
        bits.put(0, 1);
    }
    out.extend_from_slice(&bits.end());
    for i in 1..scans {
        let c = (i % comps as usize) as u8 + 1;
        // A first AC scan of each component, then refinements.
        let ah = if i <= comps as usize { 0x01 } else { 0x10 };
        segment(&mut out, 0xDA, &[1, c, 0x00, 1, 63, ah]);
        let mut bits = Bits { out: Vec::new(), acc: 0, n: 0 };
        let mut left = blocks;
        while left > 0 {
            let r = (31 - left.leading_zeros()).min(14);
            let run = left.min((1 << (r + 1)) - 1);
            bits.put(r, 4);
            bits.put(run - (1 << r), r);
            left -= run;
        }
        out.extend_from_slice(&bits.end());
    }
    out.extend_from_slice(&[0xFF, 0xD9]);
    out
}

const MB: usize = 1 << 20;

#[test]
fn a_decoded_image_counts_toward_the_memory_limit() {
    let dir = project("memory");
    // 2000 × 2000 is 12 MB of pixels, past a 16 MB limit with the rest.
    std::fs::write(dir.join("data/big.png"), png(2000, 2000)).unwrap();
    let src = r#"<?phv
import std.image.Image;
$img = Image::open("data/big.png");
echo $img->width, "×", $img->height, "\n";
echo strlen($img->fit(100)->toBase64("png")), "\n";
"#;
    let r = run_in(&dir, src, Limits { max_memory: Some(16 * MB), ..Limits::default() });
    assert!(r.out.starts_with("2000×2000\n"), "{}", r.out);
    assert!(r.out.contains("allowed memory size of 16 MB exhausted"), "{}", r.out);
    // Refused before decoding: the file (about 6 MB) is held, not its pixels.
    assert!(r.peak < 24 * MB, "held {} MB", r.peak / MB);
    // With room, it is made.
    let r = run_in(&dir, src, Limits { max_memory: Some(128 * MB), ..Limits::default() });
    assert!(!r.out.contains("exhausted"), "{}", r.out);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_long_resize_stops_at_the_time_limit() {
    let dir = project("time");
    std::fs::write(dir.join("data/small.png"), png(300, 200)).unwrap();
    // 7000 × 7000: seconds of work in a debug build, longer than the limit.
    let src = r#"<?phv
import std.image.Image;
echo strlen(Image::open("data/small.png")->resize(7000, 7000)->rotate(90)->toBase64("png"));
"#;
    let limit = Duration::from_millis(300);
    let r = run_in(&dir, src, Limits { max_time: Some(limit), max_memory: Some(1024 * MB), ..Limits::default() });
    assert!(r.out.contains("maximum execution time"), "{}", r.out);
    // The rows are checked every few: the run ends soon after its limit.
    assert!(r.took < limit + Duration::from_millis(1500), "{:?}", r.took);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn files_are_read_and_written_through_the_permissions() {
    let dir = project("files");
    std::fs::write(dir.join("data/photo.jpg"), jpeg(640, 480, 85)).unwrap();
    std::fs::write(dir.join("data/fake.png"), b"\x89PNG\r\n\x1a\n but not really").unwrap();
    std::fs::write(dir.join("secret.png"), png(4, 4)).unwrap();
    let r = run_in(
        &dir,
        r#"<?phv
import std.image.Image;
$img = Image::open("data/photo.jpg");
echo $img->format, " ", $img->width, "×", $img->height, "\n";
$img->cover(64, 64)->saveTo("out/thumb.webp");
$img->fit(100)->saveTo("out/small.jpg", quality: 60);
$img->fit(100)->saveTo("out/as-png", "png");
$back = Image::open("out/thumb.webp");
echo $back->format, " ", $back->width, "×", $back->height, "\n";
echo Image::open("out/as-png")->format, "\n";
foreach (["out/x.gif", "data/x.png", "out/x.bmp", "secret.png"] as $p) {
    try {
        $img->saveTo($p);
        echo "saved ", $p, "\n";
    } catch (PermissionError $e) {
        echo "refused: ", $e->getMessage(), "\n";
    } catch (ValueError $e) {
        echo "refused: ", $e->getMessage(), "\n";
    }
}
foreach (["secret.png", "data/fake.png", "data/none.png"] as $p) {
    try {
        Image::open($p);
    } catch (Exception $e) {
        echo "not read: ", $e->getMessage(), "\n";
    }
}
"#,
        Limits::default(),
    );
    let lines: Vec<&str> = r.out.lines().collect();
    assert_eq!(&lines[..3], &["jpeg 640×480", "webp 64×64", "png"], "{}", r.out);
    assert!(lines[3].contains("does not end in .png, .jpg, .jpeg or .webp"), "{}", r.out);
    assert!(lines[4].contains("write access to \"data/x.png\" is not allowed"), "{}", r.out);
    assert!(lines[5].contains("does not end in"), "{}", r.out);
    assert!(lines[6].contains("write access to \"secret.png\""), "{}", r.out);
    assert!(lines[7].contains("read access to \"secret.png\""), "{}", r.out);
    assert!(lines[8].contains("damaged or not supported"), "{}", r.out);
    assert!(lines[9].contains("not read: "), "{}", r.out);
    // What is written is the image alone: no EXIF, no comments.
    let small = std::fs::read(dir.join("out/small.jpg")).unwrap();
    assert!(small.starts_with(&[0xFF, 0xD8]) && !small.windows(4).any(|w| w == b"Exif"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn metadata_is_not_written() {
    let dir = project("metadata");
    // A JPEG with an EXIF block holding a GPS tag and a comment.
    let plain = jpeg(64, 48, 90);
    let mut exif = b"Exif\0\0MM\0\x2a\0\0\0\x08\0\x01\x88\x25\0\x04\0\0\0\x01\0\0\0\0\0\0\0\0".to_vec();
    exif.extend_from_slice(b"GPS 35.6 139.7 home");
    let mut tagged = vec![0xFF, 0xD8];
    segment(&mut tagged, 0xE1, &exif);
    segment(&mut tagged, 0xFE, b"taken at home");
    tagged.extend_from_slice(&plain[2..]);
    std::fs::write(dir.join("data/tagged.jpg"), &tagged).unwrap();
    let r = run_in(
        &dir,
        r#"<?phv
import std.image.Image;
$img = Image::open("data/tagged.jpg");
$img->saveTo("out/a.jpg");
$img->saveTo("out/a.png");
$img->saveTo("out/a.webp");
echo "ok";
"#,
        Limits::default(),
    );
    assert_eq!(r.out, "ok");
    for name in ["a.jpg", "a.png", "a.webp"] {
        let b = std::fs::read(dir.join("out").join(name)).unwrap();
        for secret in [&b"Exif"[..], b"GPS", b"home"] {
            assert!(!b.windows(secret.len()).any(|w| w == secret), "{name} holds {secret:?}");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn send_replaces_the_response() {
    let dir = project("send");
    std::fs::write(dir.join("data/a.png"), png(50, 40)).unwrap();
    let r = run_in(
        &dir,
        r#"<?phv
import std.image.Image;
echo "this is replaced";
Image::open("data/a.png")->fit(10)->send("webp");
"#,
        Limits::default(),
    );
    assert_eq!(r.out, "this is replaced");
    assert_eq!(r.http.content_type.as_deref(), Some("image/webp"));
    let body = r.http.body_bytes.expect("a body");
    assert!(body.starts_with(b"RIFF") && &body[8..12] == b"WEBP");
    // response()->file() after it wins, and the reverse.
    let r = run_in(
        &dir,
        r#"<?phv
import std.http.response;
import std.image.Image;
Image::open("data/a.png")->send();
response()->file("data/a.png");
"#,
        Limits::default(),
    );
    assert!(r.http.body_bytes.is_none() && r.http.body_file.is_some(), "{}", r.out);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_progressive_jpeg_with_many_scans_is_bounded() {
    let dir = project("scans");
    let bomb = progressive_bomb(1024, 1024, 3, 100);
    std::fs::write(dir.join("data/bomb.jpg"), &bomb).unwrap();
    std::fs::write(dir.join("data/more.jpg"), progressive_bomb(256, 256, 1, 150)).unwrap();
    // 16 megapixels may have 62 scans.
    std::fs::write(dir.join("data/large.jpg"), progressive_bomb(4000, 4000, 3, 63)).unwrap();
    std::fs::write(dir.join("data/fine.jpg"), progressive_bomb(4000, 4000, 3, 62)).unwrap();
    let r = run_in(
        &dir,
        r#"<?phv
import std.image.Image;
$img = Image::open("data/bomb.jpg");
echo $img->width, "×", $img->height, " ", strlen($img->fit(8)->toBase64("png")) > 0 ? "decoded" : "", "\n";
foreach (["more", "large", "fine"] as $name) {
    try {
        echo Image::open("data/{$name}.jpg")->width, "\n";
    } catch (ValueError $e) {
        echo $e->getMessage(), "\n";
    }
}
"#,
        Limits { max_memory: Some(128 * MB), ..Limits::default() },
    );
    let lines: Vec<&str> = r.out.lines().collect();
    assert_eq!(lines[0], "1024×1024 decoded", "{}", r.out);
    // Refused by their header, before anything is decoded.
    assert!(lines[1].ends_with("has 150 scans; one of 256×256 pixels may have at most 100"), "{}", r.out);
    assert!(lines[2].ends_with("has 63 scans; one of 4000×4000 pixels may have at most 62"), "{}", r.out);
    assert_eq!(lines[3], "4000", "{}", r.out);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A WebP of 1×1 (lossless) whose EXIF chunk declares `declared` bytes and holds 10.
fn webp_exif_declaring(declared: u32) -> Vec<u8> {
    let mut simple = Vec::new();
    image::codecs::webp::WebPEncoder::new_lossless(&mut simple)
        .encode(&[1, 2, 3, 4], 1, 1, ExtendedColorType::Rgba8)
        .unwrap();
    let mut body = b"WEBP".to_vec();
    body.extend_from_slice(b"VP8X\x0a\0\0\0\x18\0\0\0\0\0\0\0\0\0");
    body.extend_from_slice(&simple[12..]);
    body.extend_from_slice(b"EXIF");
    body.extend_from_slice(&declared.to_le_bytes());
    body.extend_from_slice(b"Exif\0\0MM\0\x2a");
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
    out.extend_from_slice(&body);
    out
}

/// A WebP whose VP8X canvas is 16×16 and whose VP8 frame declares `side` × `side`.
fn webp_vp8_declaring(side: u16) -> Vec<u8> {
    let mut vp8 = vec![0x10, 0x02, 0x00, 0x9d, 0x01, 0x2a];
    vp8.extend_from_slice(&side.to_le_bytes());
    vp8.extend_from_slice(&side.to_le_bytes());
    vp8.extend_from_slice(&[0; 4096]);
    let mut body = b"WEBP".to_vec();
    body.extend_from_slice(b"VP8X\x0a\0\0\0\0\0\0\0\x0f\0\0\x0f\0\0");
    body.extend_from_slice(b"VP8 ");
    body.extend_from_slice(&(vp8.len() as u32).to_le_bytes());
    body.extend_from_slice(&vp8);
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

#[test]
fn a_webp_exif_chunk_declaring_a_gigabyte_is_not_allocated() {
    let dir = project("webp-exif");
    std::fs::write(dir.join("data/exif.webp"), webp_exif_declaring(0x4000_0000)).unwrap();
    std::fs::write(dir.join("data/vp8.webp"), webp_vp8_declaring(16_383)).unwrap();
    let r = run_in(
        &dir,
        r#"<?phv
import std.image.Image;
$i = Image::open("data/exif.webp");
echo $i->width, "×", $i->height, " ", strlen($i->toBase64("png")) > 0 ? "written" : "", "\n";
try {
    echo Image::open("data/vp8.webp")->width, "\n";
} catch (ValueError $e) {
    echo $e->getMessage(), "\n";
}
"#,
        Limits { max_memory: Some(128 * MB), ..Limits::default() },
    );
    let lines: Vec<&str> = r.out.lines().collect();
    assert_eq!(lines[0], "1×1 written", "{}", r.out);
    // Refused by its header, before image-webp decodes 400 MB of planes.
    assert!(lines[1].contains("16383×16383 pixels, not 16×16"), "{}", r.out);
    // Reading them held the files and a few buffers, not what they declare.
    assert!(r.peak < 4 * MB, "held {} MB", r.peak / MB);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `jpeg` with a stray byte after its first segment (zune-jpeg skips it).
fn with_stray_byte(mut jpeg: Vec<u8>) -> Vec<u8> {
    let len = u16::from_be_bytes([jpeg[4], jpeg[5]]) as usize;
    jpeg.insert(4 + len, 0x00);
    jpeg
}

#[test]
fn a_stray_byte_does_not_hide_a_progressive_jpeg() {
    let dir = project("stray");
    // 7000 × 6000 with four components: its coefficients alone are 336 MB.
    std::fs::write(dir.join("data/cmyk.jpg"), with_stray_byte(progressive_bomb(7000, 6000, 4, 2))).unwrap();
    // More scans than 1024 × 1024 may have, found by the header.
    std::fs::write(dir.join("data/scans.jpg"), with_stray_byte(progressive_bomb(1024, 1024, 1, 150))).unwrap();
    let r = run_in(
        &dir,
        r#"<?phv
import std.image.Image;
try {
    echo Image::open("data/scans.jpg")->width, "\n";
} catch (ValueError $e) {
    echo $e->getMessage(), "\n";
}
$i = Image::open("data/cmyk.jpg");
echo $i->width, "×", $i->height, "\n";
echo strlen($i->fit(64)->toBase64("png"));
"#,
        Limits { max_memory: Some(128 * MB), ..Limits::default() },
    );
    let lines: Vec<&str> = r.out.lines().collect();
    assert!(lines[0].ends_with("has 150 scans; one of 1024×1024 pixels may have at most 100"), "{}", r.out);
    assert_eq!(lines[1], "7000×6000", "{}", r.out);
    // Refused before decoding, not after 477 MB.
    assert!(lines[2].contains("allowed memory size of 128 MB exhausted"), "{}", r.out);
    assert!(r.peak < 16 * MB, "held {} MB", r.peak / MB);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A deflate stream of `n` zeros, about 2 bytes for each 258 (fixed codes).
fn deflate_zeros(n: usize) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let (mut acc, mut bits) = (0u64, 0u32);
    let mut put = |out: &mut Vec<u8>, code: u64, len: u32, reversed: bool| {
        let code = if reversed { (code.reverse_bits() >> (64 - len)) & ((1 << len) - 1) } else { code };
        acc |= code << bits;
        bits += len;
        while bits >= 8 {
            out.push(acc as u8);
            acc >>= 8;
            bits -= 8;
        }
    };
    // A final block with fixed codes: a zero, then (258, 1) pairs, then the end.
    put(&mut out, 0b011, 3, false);
    put(&mut out, 0b0011_0000, 8, true);
    for _ in 0..(n - 1) / 258 {
        put(&mut out, 0b1100_0101, 8, true);
        put(&mut out, 0, 5, true);
    }
    put(&mut out, 0, 7, true);
    put(&mut out, 0, 7, false);
    out
}

fn png_chunk(out: &mut Vec<u8>, name: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(name);
    out.extend_from_slice(data);
    // The decoder is told to check the CRC; this one is computed.
    let mut c = !0u32;
    for &b in name.iter().chain(data) {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 };
        }
    }
    out.extend_from_slice(&(!c).to_be_bytes());
}

#[test]
fn an_icc_profile_bomb_is_not_inflated() {
    let dir = project("iccp");
    let small = png(16, 16);
    let mut bomb = small[..33].to_vec();
    let mut iccp = b"bomb\0\0".to_vec();
    iccp.extend_from_slice(&deflate_zeros(512 * MB));
    png_chunk(&mut bomb, b"iCCP", &iccp);
    bomb.extend_from_slice(&small[33..]);
    std::fs::write(dir.join("data/bomb.png"), &bomb).unwrap();
    let r = run_in(
        &dir,
        r#"<?phv
import std.image.Image;
$i = Image::open("data/bomb.png");
echo $i->width, "×", $i->height, " ", strlen($i->toBase64("png")) > 0 ? "written" : "", "\n";
"#,
        Limits { max_memory: Some(128 * MB), ..Limits::default() },
    );
    assert_eq!(r.out, "16×16 written\n");
    // The file (about 1 MB) is held; the profile (512 MB) is never inflated.
    assert!(r.peak < 8 * MB, "held {} MB", r.peak / MB);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A GIF whose screen is `screen` pixels square and whose first frame is
/// `frame` square (its data ends at once).
fn gif_frame_larger_than_screen(screen: u16, frame: u16) -> Vec<u8> {
    let mut g = b"GIF89a".to_vec();
    g.extend_from_slice(&screen.to_le_bytes());
    g.extend_from_slice(&screen.to_le_bytes());
    g.extend_from_slice(&[0x80, 0, 0, 0, 0, 0, 255, 255, 255]);
    g.extend_from_slice(&[0x2C, 0, 0, 0, 0]);
    g.extend_from_slice(&frame.to_le_bytes());
    g.extend_from_slice(&frame.to_le_bytes());
    // LZW of 2-bit codes: a clear code and the end.
    g.extend_from_slice(&[0, 2, 1, 0x2C, 0, 0x3B]);
    g
}

#[test]
fn a_gif_frame_larger_than_its_screen_stays_within_the_limit() {
    let dir = project("gif-frame");
    // The screen takes 18 MB with what decoding it takes; the frame 25 MB more.
    std::fs::write(dir.join("data/big.gif"), gif_frame_larger_than_screen(1500, 2500)).unwrap();
    let r = run_in(
        &dir,
        r#"<?phv
import std.image.Image;
$i = Image::open("data/big.gif");
echo $i->width, "\n";
try {
    echo strlen($i->toBase64("png")), "\n";
} catch (ValueError $e) {
    echo $e->getMessage(), "\n";
}
"#,
        Limits { max_memory: Some(32 * MB), ..Limits::default() },
    );
    assert!(r.out.starts_with("1500\n"), "{}", r.out);
    // The decoder's limit is the memory left: the usual memory error.
    assert!(r.out.contains("allowed memory size of 32 MB exhausted"), "{}", r.out);
    assert!(r.peak < 32 * MB, "held {} MB", r.peak / MB);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn encoding_counts_what_the_encoders_take() {
    let dir = project("encode");
    // 1500 × 1500 of noise: 6.75 MB of pixels, and a PNG about as large.
    let mut out = Vec::new();
    PngEncoder::new_with_quality(&mut out, CompressionType::Fast, FilterType::Adaptive)
        .write_image(&noise(1500 * 1500 * 3), 1500, 1500, ExtendedColorType::Rgb8)
        .unwrap();
    std::fs::write(dir.join("data/noise.png"), &out).unwrap();
    for format in ["png", "webp", "jpeg"] {
        let src = format!(
            "<?phv\nimport std.image.Image;\necho strlen(Image::open(\"data/noise.png\")->toBase64(\"{format}\", 100));\n"
        );
        let limit = 32 * MB;
        let r = run_in(&dir, &src, Limits { max_memory: Some(limit), ..Limits::default() });
        // Refused before encoding (PNG and WebP take about 4 times the
        // pixels, JPEG 2.5), or written within the limit.
        assert!(r.peak < limit, "{format}: held {} MB: {}", r.peak / MB, r.out);
        if format != "jpeg" {
            assert!(r.out.contains("exhausted"), "{format}: {}", r.out);
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Noise: what compresses least.
fn noise(n: usize) -> Vec<u8> {
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    (0..n)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed as u8
        })
        .collect()
}

/// Prints the memory the encoders take besides their input, as a multiple
/// of it (what `encode_need` in std.image reserves).
#[test]
#[ignore]
fn measure_encoder_memory() {
    let (w, h) = (2000u32, 1500u32);
    for (name, data) in [
        ("noise", noise(w as usize * h as usize * 4)),
        ("smooth", {
            let mut d = smooth(w, h * 4 / 3);
            d.truncate(w as usize * h as usize * 4);
            d
        }),
    ] {
        for (ch, color) in [(3usize, ExtendedColorType::Rgb8), (4, ExtendedColorType::Rgba8)] {
            let px = &data[..w as usize * h as usize * ch];
            for format in ["png", "webp", "jpeg"] {
                if format == "jpeg" && ch == 4 {
                    continue;
                }
                let base = thread_allocated();
                reset_thread_peak();
                let mut out = Vec::new();
                match format {
                    "png" => PngEncoder::new_with_quality(&mut out, CompressionType::Fast, FilterType::Adaptive)
                        .write_image(px, w, h, color)
                        .unwrap(),
                    "webp" => image::codecs::webp::WebPEncoder::new_lossless(&mut out).encode(px, w, h, color).unwrap(),
                    _ => JpegEncoder::new_with_quality(&mut out, 100).write_image(px, w, h, color).unwrap(),
                }
                let peak = (thread_peak() - base).max(0) as f64;
                eprintln!(
                    "{name:<7} {ch} ch {format:<5} peak {:>6.2} × input, output {:>5.2} × input",
                    peak / px.len() as f64,
                    out.len() as f64 / px.len() as f64
                );
            }
        }
    }
}

/// Prints the time and memory of a typical upload and of the worst cases.
#[test]
#[ignore]
fn measure_image_costs() {
    let dir = project("measure");
    let t = Instant::now();
    std::fs::write(dir.join("data/photo.jpg"), jpeg(4000, 3000, 90)).unwrap();
    // 50,000,000 pixels: the most an image may have.
    std::fs::write(dir.join("data/cap.jpg"), jpeg(8660, 5773, 95)).unwrap();
    // The most scans a JPEG at the pixel cap may have.
    std::fs::write(dir.join("data/scans.jpg"), progressive_bomb(8660, 5773, 3, 20)).unwrap();
    std::fs::write(dir.join("data/scans1.jpg"), progressive_bomb(8660, 5773, 1, 20)).unwrap();
    // 12 megapixels: 83 scans.
    std::fs::write(dir.join("data/scans12.jpg"), progressive_bomb(4000, 3000, 3, 83)).unwrap();
    // Little to compress: the largest PNG and WebP under 32 MB of 50 megapixels.
    std::fs::write(dir.join("data/cap.png"), png_smooth(8660, 5773)).unwrap();
    std::fs::write(dir.join("data/cap.webp"), webp_smooth(8660, 5773)).unwrap();
    std::fs::write(dir.join("data/cap.gif"), gif_pattern(8660, 5773)).unwrap();
    std::fs::write(dir.join("data/tiny.png"), png(2, 2)).unwrap();
    eprintln!("(inputs made in {:?})", t.elapsed());
    for f in ["photo.jpg", "cap.jpg", "cap.png", "cap.webp", "cap.gif", "scans.jpg", "scans1.jpg", "scans12.jpg"] {
        eprintln!("  {f}: {} KB", std::fs::metadata(dir.join("data").join(f)).unwrap().len() / 1024);
    }
    let cases: &[(&str, &str)] = &[
        ("header only, 4000×3000 JPEG", r#"$i = Image::open("data/photo.jpg"); echo $i->width;"#),
        (
            "4000×3000 JPEG to 800 wide JPEG q80",
            r#"echo strlen(Image::open("data/photo.jpg")->fit(800)->toBase64("jpeg", 80));"#,
        ),
        (
            "4000×3000 JPEG to 800 wide WebP",
            r#"echo strlen(Image::open("data/photo.jpg")->fit(800)->toBase64("webp"));"#,
        ),
        (
            "4000×3000 JPEG to 256×256 cover PNG",
            r#"echo strlen(Image::open("data/photo.jpg")->cover(256, 256)->toBase64("png"));"#,
        ),
        (
            "4000×3000 JPEG, three sizes",
            r#"$i = Image::open("data/photo.jpg"); foreach ([1600, 800, 200] as $w) { echo strlen($i->fit($w)->toBase64("jpeg")), " "; }"#,
        ),
        ("decode only: 50 MP JPEG", r#"echo strlen(Image::open("data/cap.jpg")->crop(0, 0, 8, 8)->toBase64("png"));"#),
        ("decode only: 50 MP PNG", r#"echo strlen(Image::open("data/cap.png")->crop(0, 0, 8, 8)->toBase64("png"));"#),
        ("decode only: 50 MP WebP", r#"echo strlen(Image::open("data/cap.webp")->crop(0, 0, 8, 8)->toBase64("png"));"#),
        ("decode only: 50 MP GIF", r#"echo strlen(Image::open("data/cap.gif")->crop(0, 0, 8, 8)->toBase64("png"));"#),
        (
            "decode only: progressive 50 MP, 20 scans",
            r#"echo strlen(Image::open("data/scans.jpg")->crop(0, 0, 8, 8)->toBase64("png"));"#,
        ),
        ("50 MP JPEG to 800 wide JPEG", r#"echo strlen(Image::open("data/cap.jpg")->fit(800)->toBase64("jpeg"));"#),
        ("50 MP PNG to 800 wide JPEG", r#"echo strlen(Image::open("data/cap.png")->fit(800)->toBase64("jpeg"));"#),
        (
            "50 MP WebP (lossless) to 800 wide JPEG",
            r#"echo strlen(Image::open("data/cap.webp")->fit(800)->toBase64("jpeg"));"#,
        ),
        ("50 MP GIF to 800 wide JPEG", r#"echo strlen(Image::open("data/cap.gif")->fit(800)->toBase64("jpeg"));"#),
        (
            "progressive 50 MP, 20 scans (3 comp)",
            r#"echo strlen(Image::open("data/scans.jpg")->fit(800)->toBase64("jpeg"));"#,
        ),
        (
            "progressive 12 MP, 83 scans (3 comp)",
            r#"echo strlen(Image::open("data/scans12.jpg")->fit(800)->toBase64("jpeg"));"#,
        ),
        (
            "progressive 50 MP, 20 scans (gray)",
            r#"echo strlen(Image::open("data/scans1.jpg")->fit(800)->toBase64("jpeg"));"#,
        ),
        (
            "2×2 to 7071×7071 JPEG q100",
            r#"echo strlen(Image::open("data/tiny.png")->resize(7071, 7071)->toBase64("jpeg", 100));"#,
        ),
        ("2×2 to 7071×7071 PNG", r#"echo strlen(Image::open("data/tiny.png")->resize(7071, 7071)->toBase64("png"));"#),
        ("50 MP JPEG to 50 MP PNG", r#"echo strlen(Image::open("data/cap.jpg")->toBase64("png"));"#),
        ("50 MP JPEG to 50 MP WebP", r#"echo strlen(Image::open("data/cap.jpg")->toBase64("webp"));"#),
        ("50 MP JPEG to 50 MP JPEG q100", r#"echo strlen(Image::open("data/cap.jpg")->toBase64("jpeg", 100));"#),
        (
            "50 MP JPEG to 16384×3051 resize",
            r#"echo strlen(Image::open("data/cap.jpg")->resize(16384, 3051)->toBase64("jpeg"));"#,
        ),
        (
            "50 MP JPEG rotate 90 + flip",
            r#"echo strlen(Image::open("data/cap.jpg")->rotate(90)->flipHorizontal()->toBase64("jpeg"));"#,
        ),
    ];
    eprintln!("{:<42} {:>9} {:>9}  output", "case", "seconds", "peak MB");
    let only = std::env::var("PHV_IMAGE_CASES").unwrap_or_default();
    for (name, body) in cases.iter().filter(|(name, _)| name.contains(only.as_str())) {
        let src = format!("<?phv\nimport std.image.Image;\n{body}\n");
        let r = run_in(&dir, &src, Limits { max_memory: Some(2048 * MB), ..Limits::default() });
        eprintln!("{name:<42} {:>9.3} {:>9}  {}", r.took.as_secs_f64(), r.peak / MB, r.out.replace('\n', " "));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Bits written least significant first, as a lossless WebP holds them.
#[derive(Default)]
struct LsbBits {
    out: Vec<u8>,
    acc: u64,
    n: u32,
}

impl LsbBits {
    fn put(&mut self, v: u32, bits: u32) {
        self.acc |= (v as u64 & ((1 << bits) - 1)) << self.n;
        self.n += bits;
        while self.n >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
    }

    /// A prefix code, most significant bit first.
    fn code(&mut self, code: u32, len: u32) {
        for i in (0..len).rev() {
            self.put((code >> i) & 1, 1);
        }
    }
}

/// A 1×1 lossless WebP whose entropy image names `groups` groups of five
/// prefix codes, each with a table of 1,024 entries; with `long`, each green
/// code also has 2,048 symbols of 15 bits (a tree of 4,096 nodes). These are
/// the 20th audit's g65536 and l4096 (R23-1): 3.8 MB made image-webp take
/// 1.3 GB, uncounted.
fn webp_code_groups(groups: u32, long: bool) -> Vec<u8> {
    const ORDER: [usize; 19] = [17, 18, 0, 1, 2, 3, 4, 5, 16, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
    let mut b = LsbBits::default();
    // The header (1×1), no transform, and a color cache of 11 bits for `long`.
    for (v, n) in [(0x2F, 8), (0, 14), (0, 14), (0, 1), (0, 3), (0, 1)] {
        b.put(v, n);
    }
    if long {
        b.put(1, 1);
        b.put(11, 4);
    } else {
        b.put(0, 1);
    }
    // Meta codes in blocks of 4; the entropy image of codes of one symbol.
    b.put(1, 1);
    b.put(0, 3);
    b.put(0, 1);
    let meta = groups - 1;
    for s in [meta & 0xFF, meta >> 8, 0, 0, 0] {
        b.put(1, 1);
        b.put(0, 1);
        b.put(1, 1);
        b.put(s, 8);
    }
    // The code-length codes: lengths 1 to 6 in 3 bits, 7 to 10 in 4 (codes
    // 0b000.. in order); for `long`, 16 in 1 bit, 1, 2 and 15 in 3, 3 and 4 in 4.
    let short_cl = |l: usize| if l <= 6 { (l as u32 - 1, 3) } else { (12 + l as u32 - 7, 4) };
    let long_cl = |s: usize| match s {
        16 => (0, 1),
        1 => (4, 3),
        2 => (5, 3),
        15 => (6, 3),
        3 => (14, 4),
        _ => (15, 4),
    };
    for _ in 0..groups {
        for t in 0..5 {
            b.put(0, 1);
            if long && t == 0 {
                b.put(19 - 4, 4);
                for at in ORDER {
                    let l = match at {
                        16 => 1,
                        1 | 2 | 15 => 3,
                        3 | 4 => 4,
                        _ => 0,
                    };
                    b.put(l, 3);
                }
                let mut reads: Vec<(usize, Option<u32>)> = vec![(1, None), (2, None), (3, None), (4, None), (15, None)];
                reads.extend(std::iter::repeat_n((16, Some(3)), 340));
                reads.extend([(16, Some(1)), (16, Some(0))]);
                b.put(1, 1);
                b.put(4, 3);
                b.put(reads.len() as u32 - 2, 10);
                for (s, extra) in reads {
                    let (c, n) = long_cl(s);
                    b.code(c, n);
                    if let Some(e) = extra {
                        b.put(e, 2);
                    }
                }
            } else {
                b.put(14 - 4, 4);
                for at in &ORDER[..14] {
                    b.put(
                        match at {
                            1..=6 => 3,
                            7..=10 => 4,
                            _ => 0,
                        },
                        3,
                    );
                }
                b.put(1, 1);
                b.put(1, 3);
                b.put(11 - 2, 4);
                for l in [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 10] {
                    let (c, n) = short_cl(l);
                    b.code(c, n);
                }
            }
        }
    }
    b.put(0, 4);
    let mut data = std::mem::take(&mut b.out);
    data.push(b.acc as u8);
    data.extend_from_slice(&[0; 64]);
    if data.len() % 2 == 1 {
        data.push(0);
    }
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&(data.len() as u32 + 12).to_le_bytes());
    out.extend_from_slice(b"WEBPVP8L");
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&data);
    out
}

/// A WebP that is not an animation, with a top-level VP8 chunk of its
/// canvas, whose only ANMF frame holds the VP8L bitstream of `lossless` (or,
/// with `alph`, that bitstream as a lossless ALPH chunk): image-webp copies the
/// frame's sub-chunks into its chunk map and decodes them as the image or its
/// alpha (the re-attack of the 20th audit's fixes: they went uncounted).
fn webp_lossless_in_frame(lossless: &[u8], alph: bool) -> Vec<u8> {
    fn chunk(out: &mut Vec<u8>, name: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(name);
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(data);
        if data.len() % 2 == 1 {
            out.push(0);
        }
    }
    assert_eq!(&lossless[12..16], b"VP8L");
    let n = u32::from_le_bytes([lossless[16], lossless[17], lossless[18], lossless[19]]) as usize;
    let vp8l = &lossless[20..20 + n];
    let bits = u32::from_le_bytes([vp8l[1], vp8l[2], vp8l[3], vp8l[4]]);
    let (w, h) = ((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1);
    let u24 = |v: u32| [v as u8, (v >> 8) as u8, (v >> 16) as u8];
    let mut vp8x = vec![if alph { 0x10 } else { 0 }, 0, 0, 0];
    vp8x.extend_from_slice(&u24(w - 1));
    vp8x.extend_from_slice(&u24(h - 1));
    let mut vp8 = vec![0, 0, 0, 0x9D, 0x01, 0x2A];
    vp8.extend_from_slice(&(w as u16).to_le_bytes());
    vp8.extend_from_slice(&(h as u16).to_le_bytes());
    let mut sub = Vec::new();
    if alph {
        let mut a = vec![1];
        a.extend_from_slice(&vp8l[5..]);
        chunk(&mut sub, b"ALPH", &a);
    } else {
        chunk(&mut sub, b"VP8L", vp8l);
    }
    let mut anmf = Vec::new();
    for v in [0, 0, w - 1, h - 1, 100] {
        anmf.extend_from_slice(&u24(v));
    }
    anmf.push(0);
    anmf.extend_from_slice(&sub);
    let mut body = b"WEBP".to_vec();
    chunk(&mut body, b"VP8X", &vp8x);
    chunk(&mut body, b"VP8 ", &vp8);
    chunk(&mut body, b"ANMF", &anmf);
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

#[test]
fn a_lossless_webp_with_many_code_groups_stays_within_the_limit() {
    let dir = project("webp-groups");
    std::fs::write(dir.join("data/g65536.webp"), webp_code_groups(65_536, false)).unwrap();
    std::fs::write(dir.join("data/l4096.webp"), webp_code_groups(4096, true)).unwrap();
    std::fs::write(dir.join("data/g16.webp"), webp_code_groups(16, false)).unwrap();
    let frame = |groups, long, alph| webp_lossless_in_frame(&webp_code_groups(groups, long), alph);
    std::fs::write(dir.join("data/frame_g65536.webp"), frame(65_536, false, false)).unwrap();
    std::fs::write(dir.join("data/frame_alph.webp"), frame(65_536, false, true)).unwrap();
    std::fs::write(dir.join("data/frame_l4096.webp"), frame(4096, true, false)).unwrap();
    for (file, ok) in [
        ("g16.webp", true),
        ("g65536.webp", false),
        ("l4096.webp", false),
        ("frame_g65536.webp", false),
        ("frame_alph.webp", false),
        ("frame_l4096.webp", false),
    ] {
        let r = run_in(
            &dir,
            &format!(
                r#"<?phv
import std.image.Image;
$i = Image::open("data/{file}");
echo $i->width, "×", $i->height, " ", strlen($i->toBase64("png")) > 0 ? "written" : "", "\n";
"#
            ),
            Limits { max_memory: Some(128 * MB), ..Limits::default() },
        );
        if ok {
            assert_eq!(r.out, "1×1 written\n", "{file}");
        } else {
            // 1,272 MB and 345 MB in 1.3.2, past a limit of 128 MB.
            assert!(r.out.contains("allowed memory size of 128 MB exhausted"), "{file}: {}", r.out);
        }
        assert!(r.peak < 128 * MB, "{file}: held {} MB", r.peak / MB);
        assert!(r.took < Duration::from_secs(1), "{file}: {:?}", r.took);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A sequential (baseline) JPEG of `w` × `h` with `comps` components whose
/// first scan holds the first component only, so zune-jpeg keeps every
/// coefficient of every component until the last scan (the 20th audit's
/// R23-7: 162 bytes declaring 8000×5000 took 369 MB under a limit of 128).
fn jpeg_first_scan_partial(w: u16, h: u16, comps: u8) -> Vec<u8> {
    let mut j = vec![0xFF, 0xD8];
    let mut q = vec![0];
    q.extend_from_slice(&[1; 64]);
    segment(&mut j, 0xDB, &q);
    let mut sof = vec![8];
    sof.extend_from_slice(&h.to_be_bytes());
    sof.extend_from_slice(&w.to_be_bytes());
    sof.push(comps);
    for c in 1..=comps {
        sof.extend_from_slice(&[c, 0x11, 0]);
    }
    segment(&mut j, 0xC0, &sof);
    // DC and AC tables: one symbol (0) with a 1-bit code.
    let mut table = vec![0, 1];
    table.extend_from_slice(&[0; 15]);
    table.push(0);
    segment(&mut j, 0xC4, &table);
    table[0] = 0x10;
    segment(&mut j, 0xC4, &table);
    segment(&mut j, 0xDA, &[1, 1, 0, 0, 63, 0]);
    j.extend_from_slice(&[0; 16]);
    j.extend_from_slice(&[0xFF, 0xD9]);
    j
}

#[test]
fn a_sequential_jpeg_whose_first_scan_lacks_components_counts_its_coefficients() {
    let dir = project("jpeg-first-scan");
    std::fs::write(dir.join("data/rgb.jpg"), jpeg_first_scan_partial(8000, 5000, 3)).unwrap();
    std::fs::write(dir.join("data/cmyk.jpg"), jpeg_first_scan_partial(8000, 5000, 4)).unwrap();
    std::fs::write(dir.join("data/small.jpg"), jpeg_first_scan_partial(64, 64, 3)).unwrap();
    for (file, ok) in [("small.jpg", true), ("rgb.jpg", false), ("cmyk.jpg", false)] {
        let r = run_in(
            &dir,
            &format!(
                r#"<?phv
import std.image.Image;
$i = Image::open("data/{file}");
try {{
    echo $i->width, " ", strlen($i->toBase64("png")) > 0 ? "written" : "", "\n";
}} catch (ValueError $e) {{
    echo $e->getMessage(), "\n";
}}
"#
            ),
            Limits { max_memory: Some(128 * MB), ..Limits::default() },
        );
        if ok {
            assert!(r.out.starts_with("64 "), "{file}: {}", r.out);
        } else {
            // Refused by the reservation, before zune-jpeg allocates 240 MB.
            assert!(r.out.contains("allowed memory size of 128 MB exhausted"), "{file}: {}", r.out);
        }
        assert!(r.peak < 128 * MB, "{file}: held {} MB", r.peak / MB);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A 16×16 GIF with an XMP and an ICC application extension of `mb` MB each
/// before its image (the 20th audit's R23-17: the gif crate collected them,
/// unreserved, at the header and again at decoding).
fn gif_with_metadata(mb: usize) -> Vec<u8> {
    let plain = gif_pattern(16, 16);
    let table = if plain[10] & 0x80 != 0 { 3 << ((plain[10] & 7) + 1) } else { 0 };
    let mut g = plain[..13 + table].to_vec();
    for name in [b"XMP DataXMP", b"ICCRGBG1012"] {
        g.extend_from_slice(&[0x21, 0xFF, 11]);
        g.extend_from_slice(name);
        for _ in 0..mb * 4096 {
            g.push(255);
            g.extend_from_slice(&[b'x'; 255]);
        }
        g.push(0);
    }
    g.extend_from_slice(&plain[13 + table..]);
    g
}

#[test]
fn gif_metadata_extensions_are_not_collected() {
    let dir = project("gif-xmp");
    let gif = gif_with_metadata(8);
    let len = gif.len();
    std::fs::write(dir.join("data/xmp.gif"), gif).unwrap();
    let r = run_in(
        &dir,
        r#"<?phv
import std.image.Image;
$i = Image::open("data/xmp.gif");
echo $i->width, "×", $i->height, " ", strlen($i->toBase64("png")) > 0 ? "written" : "", "\n";
"#,
        Limits { max_memory: Some(128 * MB), ..Limits::default() },
    );
    assert_eq!(r.out, "16×16 written\n");
    // The file is held (16 MB); its 16 MB of metadata are read past, not copied.
    assert!(r.peak < len + 2 * MB, "held {} MB for a file of {} MB", r.peak / MB, len / MB);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Prints what decoding lossless WebPs of many code groups takes (`measure_image_costs` for R23-1).
#[test]
#[ignore]
fn measure_webp_code_groups() {
    let dir = project("measure-groups");
    for (groups, long) in [(1024, false), (4096, false), (256, true), (1024, true)] {
        std::fs::write(dir.join("data/g.webp"), webp_code_groups(groups, long)).unwrap();
        let r = run_in(
            &dir,
            r#"<?phv
import std.image.Image;
echo strlen(Image::open("data/g.webp")->toBase64("png"));
"#,
            Limits { max_memory: Some(4096 * MB), ..Limits::default() },
        );
        eprintln!(
            "{groups} groups, long {long}: peak {} bytes ({} a group), {:?}: {}",
            r.peak,
            r.peak / groups as usize,
            r.took,
            r.out
        );
    }
    for (w, h) in [(16_383, 8), (16_383, 64), (4000, 8)] {
        std::fs::write(dir.join("data/wide.jpg"), jpeg(w, h, 90)).unwrap();
        let r = run_in(
            &dir,
            r#"<?phv
import std.image.Image;
echo strlen(Image::open("data/wide.jpg")->crop(0, 0, 8, 8)->toBase64("png"));
"#,
            Limits { max_memory: Some(4096 * MB), ..Limits::default() },
        );
        let file = std::fs::metadata(dir.join("data/wide.jpg")).unwrap().len() as usize;
        eprintln!("jpeg {w}×{h}: peak {} bytes, pixels {}, file {file}", r.peak, w * h * 3);
    }
    let _ = std::fs::remove_dir_all(&dir);
}
