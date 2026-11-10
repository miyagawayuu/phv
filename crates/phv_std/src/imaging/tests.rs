use super::*;
use image::codecs::gif::GifEncoder;
use image::{Delay, Frame, RgbaImage};

/// A test pattern: each pixel's colors tell where it is.
fn pattern(w: u32, h: u32, ch: usize) -> Pixels {
    let mut data = Vec::new();
    for y in 0..h {
        for x in 0..w {
            let px = [(x * 37 % 256) as u8, (y * 53 % 256) as u8, ((x + y) * 11 % 256) as u8, 255 - (x % 7) as u8];
            match ch {
                1 => data.push(px[0]),
                2 => data.extend_from_slice(&[px[0], px[3]]),
                3 => data.extend_from_slice(&px[..3]),
                _ => data.extend_from_slice(&px),
            }
        }
    }
    Pixels { w, h, ch, data }
}

fn none() -> impl FnMut() -> Result<(), VmError> {
    || Ok(())
}

fn png(p: &Pixels) -> Vec<u8> {
    encode(p, Format::Png, 85).unwrap()
}

fn gif(frames: &[(u32, u32, [u8; 4])]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut e = GifEncoder::new(&mut out);
        for &(w, h, c) in frames {
            let img = RgbaImage::from_pixel(w, h, image::Rgba(c));
            e.encode_frame(Frame::from_parts(img, 0, 0, Delay::from_numer_denom_ms(100, 1))).unwrap();
        }
    }
    out
}

const BIG: u64 = 1 << 30;

fn read_back(bytes: &[u8]) -> Result<(Pixels, u32, u32, Format), String> {
    let (src, w, h) = probe(bytes.to_vec())?;
    let px = decode(&src, BIG)?;
    Ok((px, w, h, src.format))
}

#[test]
fn formats_are_found_from_the_bytes() {
    assert_eq!(Format::sniff(b"\x89PNG\r\n\x1a\nrest"), Some(Format::Png));
    assert_eq!(Format::sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), Some(Format::Jpeg));
    assert_eq!(Format::sniff(b"GIF89a..."), Some(Format::Gif));
    assert_eq!(Format::sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some(Format::Webp));
    for not in [&b"<svg xmlns"[..], b"BM\0\0", b"\0\0\0\x0cjP  ", b"", b"RIFF\0\0\0\0WAVE", b"\x89PNG"] {
        assert_eq!(Format::sniff(not), None, "{not:?}");
        assert!(probe(not.to_vec()).unwrap_err().contains("not a PNG, JPEG, GIF or WebP"));
    }
}

#[test]
fn png_and_webp_keep_every_pixel() {
    for ch in 1..=4 {
        let p = pattern(23, 17, ch);
        let (back, w, h, f) = read_back(&png(&p)).unwrap();
        assert_eq!((w, h, f), (23, 17, Format::Png));
        assert_eq!(back, p, "png, {ch} channels");
        let webp = encode(&p, Format::Webp, 85).unwrap();
        let (back, _, _, f) = read_back(&webp).unwrap();
        assert_eq!(f, Format::Webp);
        // WebP has no gray: gray comes back as RGB with the same values.
        let want = if ch < 3 { as_rgb(&p) } else { p };
        assert_eq!(back, want, "webp, {ch} channels");
    }
}

#[test]
fn jpeg_is_lossy_and_has_no_alpha() {
    let p = pattern(64, 48, 4);
    let low = encode(&p, Format::Jpeg, 20).unwrap();
    let high = encode(&p, Format::Jpeg, 95).unwrap();
    assert!(low.len() < high.len(), "{} {}", low.len(), high.len());
    let (back, w, h, f) = read_back(&high).unwrap();
    assert_eq!((w, h, f, back.ch), (64, 48, Format::Jpeg, 3));
    // A transparent pixel is written on white.
    let clear = Pixels { w: 8, h: 8, ch: 4, data: [10, 20, 30, 0].repeat(64) };
    let (back, ..) = read_back(&encode(&clear, Format::Jpeg, 90).unwrap()).unwrap();
    assert!(back.data.iter().all(|&v| v > 245), "{:?}", &back.data[..6]);
}

#[test]
fn a_gif_is_read_by_its_first_frame() {
    let bytes = gif(&[(5, 4, [255, 0, 0, 255]), (5, 4, [0, 0, 255, 255])]);
    let (back, w, h, f) = read_back(&bytes).unwrap();
    assert_eq!((w, h, f), (5, 4, Format::Gif));
    assert_eq!(&back.data[..4], &[255, 0, 0, 255]);
    assert!(encode(&back, Format::Gif, 85).is_err());
}

/// A JPEG with an EXIF block that gives its orientation.
fn jpeg_oriented(p: &Pixels, orientation: u16) -> Vec<u8> {
    let plain = encode(p, Format::Jpeg, 95).unwrap();
    let mut tiff = b"MM\0\x2a\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01".to_vec();
    tiff.extend_from_slice(&orientation.to_be_bytes());
    tiff.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    let mut app1 = b"Exif\0\0".to_vec();
    app1.extend_from_slice(&tiff);
    let mut out = vec![0xFF, 0xD8, 0xFF, 0xE1];
    out.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
    out.extend_from_slice(&app1);
    out.extend_from_slice(&plain[2..]);
    out
}

#[test]
fn the_exif_orientation_is_applied() {
    // A wide image whose left half is black and right half white.
    let mut p = Pixels { w: 32, h: 16, ch: 3, data: vec![0; 32 * 16 * 3] };
    for y in 0..16 {
        for x in 16..32 {
            let i = (y * 32 + x) * 3;
            p.data[i..i + 3].copy_from_slice(&[255, 255, 255]);
        }
    }
    // 6: turn 90° clockwise to show it: the white half goes to the bottom.
    let (back, w, h, _) = read_back(&jpeg_oriented(&p, 6)).unwrap();
    assert_eq!((w, h, back.w, back.h), (16, 32, 16, 32));
    assert!(back.row(2)[8 * 3] < 40 && back.row(29)[8 * 3] > 215);
    // 3: upside down: the white half goes to the left.
    let (back, w, h, _) = read_back(&jpeg_oriented(&p, 3)).unwrap();
    assert_eq!((w, h), (32, 16));
    assert!(back.row(8)[2 * 3] > 215 && back.row(8)[29 * 3] < 40);
    // A broken value is ignored.
    let (_, w, h, _) = read_back(&jpeg_oriented(&p, 77)).unwrap();
    assert_eq!((w, h), (32, 16));
}

#[test]
fn turns_and_flips_move_pixels_where_they_belong() {
    let p = pattern(3, 2, 1);
    let at = |p: &Pixels, x: u32, y: u32| p.data[(y * p.w + x) as usize];
    let r = turn(&p, 1, &mut none()).ok().unwrap();
    assert_eq!((r.w, r.h), (2, 3));
    // Clockwise: the top left goes to the top right.
    assert_eq!(at(&r, 1, 0), at(&p, 0, 0));
    assert_eq!(at(&r, 0, 2), at(&p, 2, 1));
    let r3 = turn(&p, 3, &mut none()).ok().unwrap();
    assert_eq!(at(&r3, 0, 2), at(&p, 0, 0));
    assert_eq!(turn(&turn(&p, 2, &mut none()).ok().unwrap(), 2, &mut none()).ok().unwrap(), p);
    assert_eq!(turn(&r, 3, &mut none()).ok().unwrap(), p);
    let h = flip_h(p.clone());
    assert_eq!(at(&h, 0, 0), at(&p, 2, 0));
    assert_eq!(flip_h(h), p);
    let v = flip_v(pattern(3, 3, 2));
    assert_eq!(v.row(0), pattern(3, 3, 2).row(2));
    assert_eq!(flip_v(v), pattern(3, 3, 2));
    let c = crop(&pattern(10, 10, 3), 2, 3, 4, 5);
    assert_eq!((c.w, c.h), (4, 5));
    assert_eq!(c.row(0)[..3], pattern(10, 10, 3).row(3)[6..9]);
}

#[test]
fn resampling_keeps_flat_colors_and_does_not_bleed_transparent_ones() {
    let flat = Pixels { w: 300, h: 200, ch: 3, data: [40, 120, 200].repeat(300 * 200) };
    for (w, h) in [(100, 50), (37, 199), (640, 480), (1, 1), (300, 7)] {
        let r = resample(&flat, w, h, &mut none()).ok().unwrap();
        assert_eq!((r.w, r.h, r.data.len()), (w, h, (w * h * 3) as usize));
        assert!(r.data.as_chunks::<3>().0.iter().all(|c| c == &[40, 120, 200]), "{w}×{h}");
    }
    // Left: invisible red; right: blue. No red shows at the edge.
    let mut p = Pixels { w: 64, h: 8, ch: 4, data: Vec::new() };
    for _ in 0..8 {
        for x in 0..64 {
            p.data.extend_from_slice(if x < 32 { &[255, 0, 0, 0] } else { &[0, 0, 255, 255] });
        }
    }
    let r = resample(&p, 16, 2, &mut none()).ok().unwrap();
    for px in r.data.as_chunks::<4>().0.iter().filter(|px| px[3] > 0) {
        assert!(px[0] < 10 && px[2] > 245, "{px:?}");
    }
}

#[test]
fn resampling_stops_when_the_limits_say_so() {
    let p = pattern(64, 640, 3);
    let mut calls = 0;
    let mut tick = || {
        calls += 1;
        if calls > 3 {
            Err(VmError::Fatal("maximum execution time".into()))
        } else {
            Ok(())
        }
    };
    assert!(resample(&p, 32, 320, &mut tick).is_err());
    let mut calls = 0;
    let mut tick = || {
        calls += 1;
        if calls > 3 {
            Err(VmError::Fatal("maximum execution time".into()))
        } else {
            Ok(())
        }
    };
    assert!(resample(&p, 128, 1280, &mut tick).is_err());
}

fn base(w: u32, h: u32) -> Pipeline {
    let bytes = png(&pattern(w, h, 3));
    let (src, w, h) = probe(bytes).unwrap();
    Pipeline { src: Rc::new(src), ops: Vec::new(), width: w, height: h }
}

#[test]
fn steps_compute_sizes_and_refuse_what_cannot_be() {
    let p = base(400, 300);
    let size = |r: Result<Pipeline, String>| r.map(|p| (p.width, p.height));
    assert_eq!(size(step(&p, "fit", 200, 200, 0, 0)), Ok((200, 150)));
    assert_eq!(size(step(&p, "fit", 100, 0, 0, 0)), Ok((100, 75)));
    // Not enlarged.
    assert_eq!(size(step(&p, "fit", 1000, 1000, 0, 0)), Ok((400, 300)));
    assert!(step(&p, "fit", 1000, 1000, 0, 0).unwrap().ops.is_empty());
    assert_eq!(size(step(&p, "cover", 100, 100, 0, 0)), Ok((100, 100)));
    assert_eq!(step(&p, "cover", 100, 100, 0, 0).unwrap().ops, vec![Op::Crop(50, 0, 300, 300), Op::Resize(100, 100)]);
    assert_eq!(size(step(&p, "cover", 800, 100, 0, 0)), Ok((800, 100)));
    assert_eq!(size(step(&p, "resize", 10, 900, 0, 0)), Ok((10, 900)));
    assert_eq!(size(step(&p, "crop", 10, 20, 30, 40)), Ok((30, 40)));
    assert_eq!(size(step(&p, "rotate", -90, 0, 0, 0)), Ok((300, 400)));
    assert_eq!(step(&p, "rotate", 450, 0, 0, 0).unwrap().ops, vec![Op::Turn(1)]);
    assert_eq!(step(&p, "rotate", -90, 0, 0, 0).unwrap().ops, vec![Op::Turn(3)]);
    assert!(step(&p, "rotate", 360, 0, 0, 0).unwrap().ops.is_empty());
    for (op, a, b, c, d) in [
        ("fit", 0, 10, 0, 0),
        ("fit", 10, -1, 0, 0),
        ("cover", 0, 10, 0, 0),
        ("resize", 16_385, 1, 0, 0),
        ("resize", 10_000, 10_000, 0, 0),
        ("resize", -5, 10, 0, 0),
        ("crop", 390, 0, 20, 10),
        ("crop", -1, 0, 10, 10),
        ("crop", 0, 0, 0, 10),
        ("crop", i64::MAX, 0, i64::MAX, 1),
        ("rotate", 45, 0, 0, 0),
    ] {
        assert!(step(&p, op, a, b, c, d).is_err(), "{op} {a} {b} {c} {d}");
    }
    let mut q = p;
    for _ in 0..MAX_STEPS {
        q = step(&q, "fliph", 0, 0, 0, 0).unwrap();
    }
    assert!(step(&q, "fliph", 0, 0, 0, 0).unwrap_err().contains("at most 64 steps"));
}

fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 };
        }
    }
    !c
}

/// A PNG that declares `w` × `h` and holds one small deflate stream.
fn png_declaring(w: u32, h: u32) -> Vec<u8> {
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut chunk = |kind: &[u8], data: &[u8]| {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend_from_slice(data);
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc32(&body).to_be_bytes());
    };
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(b"IHDR", &ihdr);
    // A zlib stream of zeros: what a bomb's IDAT would be, cut short.
    chunk(b"IDAT", &[0x78, 0x9C, 0x63, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01]);
    chunk(b"IEND", &[]);
    out
}

/// A JPEG whose frame header declares `w` × `h`.
fn jpeg_declaring(w: u16, h: u16) -> Vec<u8> {
    let mut j = encode(&pattern(8, 8, 3), Format::Jpeg, 80).unwrap();
    let sof = j.windows(2).position(|m| m == [0xFF, 0xC0]).unwrap();
    j[sof + 5..sof + 7].copy_from_slice(&h.to_be_bytes());
    j[sof + 7..sof + 9].copy_from_slice(&w.to_be_bytes());
    j
}

#[test]
fn bombs_are_refused_by_their_header() {
    let start = std::time::Instant::now();
    for (w, h) in [(20_000, 100), (100, 20_000), (10_000, 10_000), (16_384, 16_384), (0x7FFF_FFFF, 0x7FFF_FFFF)] {
        let e = probe(png_declaring(w, h)).err().unwrap_or_default();
        assert!(e.contains("too large") || e.contains("pixels:"), "{w}×{h}: {e}");
    }
    for (w, h) in [(65_000, 65_000), (16_385, 8), (8_000, 8_000)] {
        let e = probe(jpeg_declaring(w, h)).err().unwrap_or_default();
        assert!(e.contains("too large") || e.contains("pixels:"), "{w}×{h}: {e}");
    }
    // A GIF whose screen is 65535 × 65535.
    let mut g = gif(&[(2, 2, [1, 2, 3, 255])]);
    g[6..10].copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
    assert!(probe(g).is_err());
    assert!(start.elapsed() < std::time::Duration::from_secs(2), "{:?}", start.elapsed());
    // Within the caps the header is fine, and the decoder refuses what lacks.
    let (src, w, h) = probe(png_declaring(5000, 5000)).unwrap();
    assert_eq!((w, h), (5000, 5000));
    assert!(src.need >= 100_000_000);
    assert!(decode(&src, BIG).is_err());
    // With little memory left, the decoder's own limit refuses it too.
    assert!(decode(&src, 1 << 20).is_err());
}

/// A mutation fuzzer: seeds of every format, truncated and with bytes
/// changed, inserted and repeated, must give an image or an error, never a
/// panic. `PHV_IMAGE_FUZZ=<runs>` runs it longer.
#[test]
fn damaged_images_never_panic() {
    let runs: usize = std::env::var("PHV_IMAGE_FUZZ").ok().and_then(|v| v.parse().ok()).unwrap_or(1500);
    let p = pattern(37, 29, 4);
    let progressive = {
        // zune-jpeg reads progressive JPEGs; the encoder writes baseline ones,
        // so one is made by marking a baseline frame as progressive.
        let mut j = encode(&pattern(16, 16, 3), Format::Jpeg, 80).unwrap();
        let sof = j.windows(2).position(|m| m == [0xFF, 0xC0]).unwrap();
        j[sof + 1] = 0xC2;
        j
    };
    let seeds: Vec<Vec<u8>> = vec![
        png(&p),
        png(&pattern(9, 7, 1)),
        encode(&p, Format::Jpeg, 75).unwrap(),
        encode(&pattern(31, 17, 1), Format::Jpeg, 50).unwrap(),
        jpeg_oriented(&pattern(12, 10, 3), 8),
        progressive,
        encode(&p, Format::Webp, 85).unwrap(),
        encode(&pattern(20, 20, 3), Format::Webp, 85).unwrap(),
        gif(&[(6, 5, [9, 8, 7, 255]), (6, 5, [1, 2, 3, 0])]),
        png_declaring(64, 64),
    ];
    // xorshift, seeded so a failure can be repeated.
    let mut state: u64 = std::env::var("PHV_IMAGE_FUZZ_SEED").ok().and_then(|v| v.parse().ok()).unwrap_or(0x9E37_79B9);
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut panics = Vec::new();
    let (mut decoded, mut refused) = (0, 0);
    for run in 0..runs {
        let seed = &seeds[run % seeds.len()];
        let mut b = seed.clone();
        let edits = 1 + next() % 8;
        for _ in 0..edits {
            let at = (next() as usize) % b.len().max(1);
            match next() % 6 {
                0 => b.truncate(at.max(8)),
                1 => b[at] = next() as u8,
                2 => b[at] ^= 1 << (next() % 8),
                3 => b.insert(at, next() as u8),
                4 => {
                    let n = (next() as usize % 64).min(b.len() - at);
                    let piece = b[at..at + n].to_vec();
                    b.splice(at..at, piece);
                }
                _ => {
                    // A 16- or 32-bit field set to a large value (sizes, lengths).
                    let v = [0xFFu8, 0xFF, 0xFF, 0x7F];
                    let n = (2 + next() as usize % 3).min(b.len() - at);
                    b[at..at + n].copy_from_slice(&v[..n]);
                }
            }
            if b.is_empty() {
                break;
            }
        }
        let bytes = b.clone();
        let r = std::panic::catch_unwind(move || -> Result<(), String> {
            let (src, _, _) = probe(bytes)?;
            if src.need > 256 << 20 {
                return Err("too large".into());
            }
            let px = decode(&src, 256 << 20)?;
            let small = resample(&px, (px.w / 2).max(1), (px.h / 2).max(1), &mut none()).map_err(|_| "x")?;
            encode(&small, Format::Png, 85)?;
            Ok(())
        });
        match r {
            Ok(Ok(())) => decoded += 1,
            Ok(Err(_)) => refused += 1,
            Err(_) => panics.push((run, b)),
        }
    }
    eprintln!("fuzzed {runs} images: {decoded} decoded, {refused} refused, {} panics", panics.len());
    if let Some((run, b)) = panics.first() {
        let dir = std::env::temp_dir().join("phv-image-fuzz");
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join(format!("panic-{run}.bin")), b);
    }
    // A panic is caught (`guarded`), so it is no crash; still, count them.
    let allowed: usize = std::env::var("PHV_IMAGE_FUZZ_PANICS").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    assert!(panics.len() <= allowed, "{} decoder panics", panics.len());
}

#[test]
fn guarded_turns_a_panic_into_a_refusal() {
    let r: Result<(), Fail> = guarded("send()", || panic!("a decoder bug"));
    assert!(matches!(r, Err(Fail::Msg(m)) if m.contains("could not be read")));
}

#[test]
fn base64_and_data_urls_are_read() {
    let decode = |text: &str| from_base64(text, |_| Ok(()));
    let bytes = png(&pattern(3, 3, 3));
    let b64 = STANDARD.encode(&bytes);
    assert_eq!(decode(&b64).ok().unwrap(), bytes);
    assert_eq!(decode(b64.trim_end_matches('=')).ok().unwrap(), bytes);
    assert_eq!(decode(&format!("data:image/png;base64,{b64}")).ok().unwrap(), bytes);
    let wrapped: String = b64.as_bytes().chunks(10).map(|c| format!("{}\n", std::str::from_utf8(c).unwrap())).collect();
    assert_eq!(decode(&wrapped).ok().unwrap(), bytes);
    assert!(decode("data:image/png,abc").is_err());
    assert!(decode("not base64!").is_err());
    assert!(matches!(decode(&"A".repeat(MAX_INPUT / 3 * 4 + 8192)), Err(Fail::Msg(m)) if m.contains("at most 32 MB")));
}

#[test]
fn wrapped_base64_counts_both_buffers_before_decoding() {
    let result = from_base64("YW Jj\n", |bytes| {
        assert_eq!(bytes, 7);
        Err(VmError::Fatal("budget refused".into()))
    });
    assert!(matches!(result, Err(Fail::Vm(VmError::Fatal(message))) if message == "budget refused"));
}

#[test]
fn a_conversion_releases_its_slot_on_success_and_error() {
    use std::sync::Arc;

    let (src, width, height) = probe(png(&pattern(3, 3, 3))).unwrap();
    let mut pipe = Pipeline { src: Rc::new(src), ops: Vec::new(), width, height };
    let mut vm = Vm::new(Arc::default(), Arc::from([]), false, phv_vm::Limits::default());
    let slots = Arc::new(AtomicUsize::new(0));
    vm.http.image_slots = Some(slots.clone());
    let busy = render(&vm, "send()", &pipe, Format::Png, 85);
    assert!(matches!(busy, Err(Fail::Vm(VmError::Fatal(message))) if message.contains("slots are busy")));
    slots.store(1, Ordering::Relaxed);
    assert!(render(&vm, "send()", &pipe, Format::Png, 85).is_ok());
    assert_eq!(slots.load(Ordering::Relaxed), 1);
    Rc::get_mut(&mut pipe.src).unwrap().bytes.clear();
    assert!(render(&vm, "send()", &pipe, Format::Png, 85).is_err());
    assert_eq!(slots.load(Ordering::Relaxed), 1);
}

#[test]
fn progressive_jpegs_are_told_apart() {
    let j = encode(&pattern(8, 8, 3), Format::Jpeg, 80).unwrap();
    assert!(!jpeg_head(&j).unwrap().progressive);
    let mut p = j.clone();
    let sof = p.windows(2).position(|m| m == [0xFF, 0xC0]).unwrap();
    p[sof + 1] = 0xC2;
    assert!(jpeg_head(&p).unwrap().progressive);
    // A stray byte between two segments is skipped by the decoder, so the
    // frame after it is still found (it was taken for the end of the header).
    let mut stray = p.clone();
    let dqt = stray.windows(2).position(|m| m == [0xFF, 0xDB]).unwrap();
    let len = u16::from_be_bytes([stray[dqt + 2], stray[dqt + 3]]) as usize;
    stray.insert(dqt + 2 + len, 0x00);
    assert!(jpeg_head(&stray).unwrap().progressive);
    assert!(jpeg_head(&[0xFF, 0xD8, 0xFF]).is_err());
    assert_eq!(jpeg_scans(&j), 1);
    // A scan whose data holds a stuffed 0xFF and a restart marker, twice.
    let mut two = vec![0xFF, 0xD8];
    for _ in 0..2 {
        two.extend_from_slice(&[0xFF, 0xDA, 0, 8, 1, 1, 0, 0, 63, 0, 0x12, 0xFF, 0x00, 0x34, 0xFF, 0xD0, 0x56]);
    }
    two.extend_from_slice(&[0xFF, 0xD9]);
    assert_eq!(jpeg_scans(&two), 2);
    assert_eq!(jpeg_scans(&two[..20]), 1);
}

/// A chunk of a RIFF file whose header declares `declared` bytes (its length when `None`).
fn riff(out: &mut Vec<u8>, name: &[u8; 4], data: &[u8], declared: Option<u32>) {
    out.extend_from_slice(name);
    out.extend_from_slice(&declared.unwrap_or(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
    if data.len() % 2 == 1 {
        out.push(0);
    }
}

/// An extended WebP: a VP8X header of `w` × `h` with `flags`, then `chunks`.
fn webp_x(w: u32, h: u32, flags: u8, chunks: &[(&[u8; 4], Vec<u8>, Option<u32>)]) -> Vec<u8> {
    let mut body = b"WEBP".to_vec();
    let mut x = vec![flags, 0, 0, 0];
    x.extend_from_slice(&(w - 1).to_le_bytes()[..3]);
    x.extend_from_slice(&(h - 1).to_le_bytes()[..3]);
    riff(&mut body, b"VP8X", &x, None);
    for (name, data, declared) in chunks {
        riff(&mut body, name, data, *declared);
    }
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

/// The VP8L bitstream of a lossless WebP of `p`.
fn vp8l(p: &Pixels) -> Vec<u8> {
    let simple = encode(p, Format::Webp, 85).unwrap();
    assert_eq!(&simple[12..16], b"VP8L");
    let len = u32::from_le_bytes(simple[16..20].try_into().unwrap()) as usize;
    simple[20..20 + len].to_vec()
}

/// The start of a VP8 key frame that declares `w` × `h`.
fn vp8_declaring(w: u16, h: u16) -> Vec<u8> {
    let mut d = vec![0x10, 0x02, 0x00, 0x9d, 0x01, 0x2a];
    d.extend_from_slice(&w.to_le_bytes());
    d.extend_from_slice(&h.to_le_bytes());
    d.extend_from_slice(&[0; 22]);
    d
}

/// An EXIF block (TIFF, big-endian) giving `orientation`.
fn exif(orientation: u16) -> Vec<u8> {
    let mut tiff = b"MM\0\x2a\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01".to_vec();
    tiff.extend_from_slice(&orientation.to_be_bytes());
    tiff.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    tiff
}

#[test]
fn a_webp_exif_chunk_is_read_in_place() {
    let p = pattern(4, 2, 4);
    // Orientation 6: turned a quarter clockwise, so it is 2 wide and 4 tall.
    let turned = webp_x(4, 2, 0x18, &[(b"VP8L", vp8l(&p), None), (b"EXIF", exif(6), None)]);
    let (src, w, h) = probe(turned).unwrap();
    assert_eq!((w, h, src.orientation), (2, 4, Orientation::Rotate90));
    assert_eq!(decode(&src, BIG).unwrap(), turn(&p, 1, &mut none()).ok().unwrap());
    // A chunk that declares a gigabyte and holds a few bytes: image-webp
    // allocated what it declares before reading it (an 82-byte file made
    // `phv serve` commit 4 GB). It is ignored, as a broken EXIF block is.
    let bomb = webp_x(4, 2, 0x18, &[(b"VP8L", vp8l(&p), None), (b"EXIF", exif(6), Some(0x4000_0000))]);
    let (src, w, h) = probe(bomb).unwrap();
    assert_eq!((w, h, src.orientation), (4, 2, Orientation::NoTransforms));
    assert_eq!(decode(&src, BIG).unwrap(), p);
}

#[test]
fn a_webp_bitstream_of_another_size_than_its_canvas_is_refused() {
    // image-webp decodes a VP8 frame at the size its own header gives (here
    // 400 MB of planes) before it compares it with the canvas.
    let big = webp_x(16, 16, 0, &[(b"VP8 ", vp8_declaring(16_383, 16_383), None)]);
    let e = probe(big).err().unwrap_or_default();
    assert!(e.contains("16383×16383 pixels, not 16×16"), "{e}");
    let small = webp_x(16, 16, 0x10, &[(b"VP8L", vp8l(&pattern(4, 2, 4)), None)]);
    let e = probe(small).err().unwrap_or_default();
    assert!(e.contains("4×2 pixels, not 16×16"), "{e}");
    // The first frame of an animation: after an ALPH chunk, the next chunk is
    // decoded as VP8 whatever it is named.
    let mut anmf = vec![0, 0, 0, 0, 0, 0, 15, 0, 0, 15, 0, 0, 100, 0, 0, 0];
    riff(&mut anmf, b"ALPH", &[0], None);
    riff(&mut anmf, b"XXXX", &vp8_declaring(16_383, 16_383), None);
    let animated = webp_x(16, 16, 0x12, &[(b"ANIM", vec![0; 6], None), (b"ANMF", anmf, None)]);
    let e = probe(animated).err().unwrap_or_default();
    assert!(e.contains("16383×16383 pixels, not 16×16"), "{e}");
    // A frame of the right size is read.
    let p = pattern(16, 16, 4);
    let (src, w, h) = probe(webp_x(16, 16, 0x10, &[(b"VP8L", vp8l(&p), None)])).unwrap();
    assert_eq!((w, h), (16, 16));
    assert_eq!(decode(&src, BIG).unwrap(), p);
}

/// `png` with a chunk added after its header.
fn png_with(png: &[u8], name: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = png[..33].to_vec();
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let mut body = name.to_vec();
    body.extend_from_slice(data);
    out.extend_from_slice(&body);
    out.extend_from_slice(&crc32(&body).to_be_bytes());
    out.extend_from_slice(&png[33..]);
    out
}

#[test]
fn png_metadata_chunks_are_left_out_and_the_orientation_kept() {
    let p = pattern(5, 4, 3);
    let plain = png(&p);
    let mut tagged = png_with(&plain, b"iCCP", b"icc\0\0\x78\x9c\x03\0\0\0\0\x01");
    tagged = png_with(&tagged, b"tEXt", b"Comment\0taken at home");
    tagged = png_with(&tagged, b"eXIf", &exif(6));
    tagged = png_with(&tagged, b"zTXt", b"k\0\0\x78\x9c\x03\0\0\0\0\x01");
    // What the decoder reads is the PNG without them.
    let mut seen = Vec::new();
    PngView::new(&tagged).read_to_end(&mut seen).unwrap();
    assert_eq!(seen, plain);
    let (src, w, h) = probe(tagged).unwrap();
    assert_eq!((w, h), (4, 5));
    assert_eq!(decode(&src, BIG).unwrap(), turn(&p, 1, &mut none()).ok().unwrap());
    // A chunk cut short ends the walk: the rest goes to the decoder as it is.
    let mut cut = png_with(&plain, b"iCCP", b"icc\0\0");
    cut.truncate(40);
    let mut seen = Vec::new();
    PngView::new(&cut).read_to_end(&mut seen).unwrap();
    assert_eq!(seen, cut);
}

#[test]
fn a_caught_decoder_panic_is_not_printed() {
    assert!(!QUIET.with(Cell::get));
    let inner: Result<bool, Fail> = guarded("x", || {
        let nested: Result<(), Fail> = guarded("y", || Ok(()));
        assert!(nested.is_ok());
        Ok(QUIET.with(Cell::get))
    });
    assert!(matches!(inner, Ok(true)));
    assert!(!QUIET.with(Cell::get));
    let r: Result<(), Fail> = guarded("send()", || panic!("a decoder bug"));
    assert!(r.is_err());
    // Outside `guarded`, panics are printed as before.
    assert!(!QUIET.with(Cell::get));
}

#[test]
fn encoding_reserves_what_the_encoders_take() {
    let p = pattern(100, 100, 3);
    let n = p.data.len() as u64;
    assert!(encode_need(&p, Format::Png) >= n * 37 / 10);
    assert!(encode_need(&p, Format::Webp) >= n * 39 / 10);
    assert!(encode_need(&p, Format::Jpeg) >= n * 23 / 10);
    // Gray is made RGB for WebP first, alpha flattened for JPEG.
    let gray = pattern(100, 100, 1);
    assert!(encode_need(&gray, Format::Webp) >= 3 * gray.data.len() as u64 * 49 / 10);
    let rgba = pattern(100, 100, 4);
    assert!(encode_need(&rgba, Format::Jpeg) >= 30_000 * 33 / 10);
}
