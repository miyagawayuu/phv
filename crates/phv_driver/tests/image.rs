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
