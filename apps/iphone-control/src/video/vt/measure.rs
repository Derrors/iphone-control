use std::collections::BTreeMap;

use super::*;

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    fn CMVideoFormatDescriptionCreateFromH264ParameterSets(
        a: CFAllocatorRef,
        count: usize,
        ptrs: *const *const u8,
        sizes: *const usize,
        nal_len: i32,
        out: *mut *mut c_void,
    ) -> OSStatus;
}

/// An H.264 decoder for the `avcC` (the page's side), giving NV12 of the frames' size
fn h264_decoder(avcc: &[u8], size: (u32, u32)) -> Decoder {
    // avcC: 5 bytes, SPS count & 0x1f, then length-prefixed SPS; PPS count, length-prefixed PPS
    let mut sets: Vec<&[u8]> = Vec::new();
    let mut i = 5;
    // the SPS list, then the PPS list
    for _ in 0..2 {
        let n = (avcc[i] & 0x1f) as usize;
        i += 1;
        for _ in 0..n {
            let l = u16::from_be_bytes([avcc[i], avcc[i + 1]]) as usize;
            sets.push(&avcc[i + 2..i + 2 + l]);
            i += 2 + l;
        }
    }
    let ptrs: Vec<*const u8> = sets.iter().map(|s| s.as_ptr()).collect();
    let sizes: Vec<usize> = sets.iter().map(|s| s.len()).collect();
    let mut desc = std::ptr::null_mut();
    let st = unsafe {
        CMVideoFormatDescriptionCreateFromH264ParameterSets(
            std::ptr::null(),
            sets.len(),
            ptrs.as_ptr(),
            sizes.as_ptr(),
            4,
            &mut desc,
        )
    };
    assert_eq!(st, 0, "H.264 format description");
    Decoder::with_desc(Cf(desc), size, size).unwrap()
}

/// The Y, Cb and Cr planes of an NV12 buffer
fn planes(pb: &Cf) -> (usize, usize, Vec<u8>, Vec<u8>, Vec<u8>) {
    unsafe {
        CVPixelBufferLockBaseAddress(pb.ptr(), 1);
        let (w, h) = (CVPixelBufferGetWidth(pb.ptr()), CVPixelBufferGetHeight(pb.ptr()));
        let (y, ys) =
            (CVPixelBufferGetBaseAddressOfPlane(pb.ptr(), 0), CVPixelBufferGetBytesPerRowOfPlane(pb.ptr(), 0));
        let (uv, uvs) =
            (CVPixelBufferGetBaseAddressOfPlane(pb.ptr(), 1), CVPixelBufferGetBytesPerRowOfPlane(pb.ptr(), 1));
        let mut yy = Vec::with_capacity(w * h);
        for r in 0..h {
            yy.extend_from_slice(std::slice::from_raw_parts(y.add(r * ys), w));
        }
        let (cw, ch) = (w / 2, h / 2);
        let (mut u, mut v) = (Vec::with_capacity(cw * ch), Vec::with_capacity(cw * ch));
        for r in 0..ch {
            let row = std::slice::from_raw_parts(uv.add(r * uvs), cw * 2);
            for p in row.as_chunks::<2>().0 {
                u.push(p[0]);
                v.push(p[1]);
            }
        }
        CVPixelBufferUnlockBaseAddress(pb.ptr(), 1);
        (w, h, yy, u, v)
    }
}

/// SSIM of one plane the way ffmpeg's `ssim` filter computes it: 8×8 windows every 4 pixels
fn ssim(a: &[u8], b: &[u8], w: usize, h: usize) -> f64 {
    const C1: f64 = 0.01 * 0.01 * 255.0 * 255.0;
    const C2: f64 = 0.03 * 0.03 * 255.0 * 255.0;
    let (mut total, mut n) = (0.0, 0usize);
    let mut y = 0;
    while y + 8 <= h {
        let mut x = 0;
        while x + 8 <= w {
            let (mut sa, mut sb, mut saa, mut sbb, mut sab) = (0f64, 0f64, 0f64, 0f64, 0f64);
            for r in y..y + 8 {
                for c in x..x + 8 {
                    let (p, q) = (f64::from(a[r * w + c]), f64::from(b[r * w + c]));
                    sa += p;
                    sb += q;
                    saa += p * p;
                    sbb += q * q;
                    sab += p * q;
                }
            }
            let (ma, mb) = (sa / 64.0, sb / 64.0);
            let (va, vb, cov) = (saa / 64.0 - ma * ma, sbb / 64.0 - mb * mb, sab / 64.0 - ma * mb);
            total += ((2.0 * ma * mb + C1) * (2.0 * cov + C2)) / ((ma * ma + mb * mb + C1) * (va + vb + C2));
            n += 1;
            x += 4;
        }
        y += 4;
    }
    total / n.max(1) as f64
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(f64::total_cmp);
    v[((v.len() - 1) as f64 * p).round() as usize]
}

#[tokio::test]
#[ignore]
async fn measure_recording() {
    let dir = std::env::var(ictl_daemon::protocol::env::RECORDING).expect("ICTL_RECORDING=<dir of <ms>.jpg>");
    let mut files: Vec<(u64, std::path::PathBuf)> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            let ms = p.file_stem()?.to_str()?.parse().ok()?;
            (p.extension()? == "jpg").then_some((ms, p))
        })
        .collect();
    files.sort();
    let frames: Vec<(u64, Arc<Vec<u8>>)> =
        files.iter().map(|(ms, p)| (*ms, Arc::new(std::fs::read(p).unwrap()))).collect();
    let size = super::super::mjpeg::jpeg_size(&frames[0].1).unwrap();
    let frames: Vec<_> = frames.into_iter().filter(|(_, j)| super::super::mjpeg::jpeg_size(j) == Some(size)).collect();
    let span = frames.last().unwrap().0 - frames[0].0;
    let jpeg_bytes: usize = frames.iter().map(|(_, j)| j.len()).sum();
    eprintln!(
        "recording: {} frames of {}x{} over {:.1} s; MJPEG (repeats dropped) {:.0} KB/s on average",
        frames.len(),
        size.0,
        size.1,
        span as f64 / 1000.0,
        jpeg_bytes as f64 / 1024.0 / (span as f64 / 1000.0)
    );
    let qps: Vec<u8> = std::env::var(ictl_daemon::protocol::env::QPS)
        .unwrap_or_else(|_| "20".into())
        .split(',')
        .filter_map(|q| q.trim().parse().ok())
        .collect();
    for qp in qps {
        let (itx, irx) = tokio::sync::oneshot::channel();
        let mut s = Session::start(size.0, size.1, qp, 0, Some(itx));
        let info = irx.await.unwrap();
        let mut reference = Decoder::new(size.0, size.1).unwrap();
        let mut avcc = Vec::new();
        let mut page: Option<Decoder> = None;
        let (mut ssims, mut ssims_y, mut times, mut sizes) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let mut per_second: BTreeMap<u64, (usize, usize)> = BTreeMap::new();
        let mut keys = 0;
        for (ms, j) in &frames {
            let t = Instant::now();
            s.push(j.clone());
            let (key, data) = loop {
                match s.next().await.unwrap().unwrap() {
                    Packet::Config(c) => avcc = c,
                    Packet::Frame { key, data } => break (key, data),
                }
            };
            times.push(t.elapsed().as_secs_f64() * 1000.0);
            sizes.push(data.len() as f64);
            keys += usize::from(key);
            let e = per_second.entry((ms - frames[0].0) / 1000).or_default();
            e.0 += data.len();
            e.1 += j.len();
            let dec = page.get_or_insert_with(|| h264_decoder(&avcc, even(size)));
            let shown = planes(&dec.decode(&data).unwrap());
            let given = planes(&reference.decode(j).unwrap());
            let (w, h) = (shown.0, shown.1);
            let sy = ssim(&given.2, &shown.2, w, h);
            let su = ssim(&given.3, &shown.3, w / 2, h / 2);
            let sv = ssim(&given.4, &shown.4, w / 2, h / 2);
            ssims_y.push(sy);
            // ffmpeg's "All" for 4:2:0
            ssims.push((4.0 * sy + su + sv) / 6.0);
        }
        let rates: Vec<String> =
            per_second.iter().map(|(sec, (h264, jpeg))| format!("{sec}s {}/{}", h264 / 1024, jpeg / 1024)).collect();
        let mut busy: Vec<f64> = per_second.values().map(|(b, _)| *b as f64 / 1024.0).collect();
        let total: usize = sizes.iter().map(|s| *s as usize).sum();
        eprintln!("\nqp {qp} ({info:?})");
        eprintln!(
            "  H.264 {:.0} KB in all = {:.1}% of MJPEG; {:.0} KB/s on average, per second median {:.0} max {:.0} KB/s",
            total as f64 / 1024.0,
            100.0 * total as f64 / jpeg_bytes as f64,
            total as f64 / 1024.0 / (span as f64 / 1000.0),
            pct(&mut busy.clone(), 0.5),
            pct(&mut busy, 1.0)
        );
        eprintln!(
            "  frames {} ({} key), bytes median {:.0} p90 {:.0} max {:.0}",
            sizes.len(),
            keys,
            pct(&mut sizes.clone(), 0.5),
            pct(&mut sizes.clone(), 0.9),
            pct(&mut sizes, 1.0)
        );
        eprintln!(
            "  SSIM all median {:.4} p10 {:.4} min {:.4}; Y median {:.4} min {:.4}",
            pct(&mut ssims.clone(), 0.5),
            pct(&mut ssims.clone(), 0.1),
            pct(&mut ssims, 0.0),
            pct(&mut ssims_y.clone(), 0.5),
            pct(&mut ssims_y, 0.0)
        );
        eprintln!(
            "  push to frame: median {:.1} ms, p90 {:.1}, max {:.1}",
            pct(&mut times.clone(), 0.5),
            pct(&mut times.clone(), 0.9),
            pct(&mut times, 1.0)
        );
        eprintln!("  KB/s H.264/MJPEG each second: {}", rates.join(", "));
    }
}
