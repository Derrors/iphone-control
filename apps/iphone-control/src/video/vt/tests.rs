use std::time::Duration;

use super::*;

#[test]
fn avcc_from_parameter_sets() {
    let sps = vec![0x67, 0x64, 0x00, 0x28, 0xAC];
    let pps = vec![0x68, 0xEE, 0x3C];
    let a = build_avcc(&[sps, pps], 4).unwrap();
    assert_eq!(a, [1, 0x64, 0x00, 0x28, 0xFF, 0xE1, 0, 5, 0x67, 0x64, 0x00, 0x28, 0xAC, 1, 0, 3, 0x68, 0xEE, 0x3C]);
    assert!(build_avcc(&[vec![0x68, 1]], 4).is_none(), "no SPS");
    // more than the record's fields hold
    let sps = vec![0x67, 0x64, 0x00, 0x28];
    assert!(build_avcc(&[[sps.clone(), vec![0; 70_000]].concat(), vec![0x68, 1]], 4).is_none(), "a set over 64 KB");
    let many: Vec<Vec<u8>> = std::iter::repeat_n(sps, 32).chain([vec![0x68, 1]]).collect();
    assert!(build_avcc(&many, 4).is_none(), "32 SPS");
}

#[test]
fn odd_sizes_grow_to_even() {
    assert_eq!(even((968, 2097)), (968, 2098));
    assert_eq!(even((481, 1041)), (482, 1042));
    assert_eq!(even((480, 1040)), (480, 1040));
}

#[test]
fn quality_follows_qp() {
    assert_eq!(quality_for(20), 0.75);
    assert!(quality_for(24) < quality_for(20) && quality_for(1) <= 1.0 && quality_for(51) >= 0.1);
}

/// The native encoder on this Mac, when VideoToolbox can encode here (a CI VM may not): a test picture of an
/// odd size (like the phone's 75 % frames) comes out as a keyframe after its `avcC`, the next one as a delta
/// frame, and each comes out as soon as it went in, without a following frame
#[tokio::test]
async fn encodes_natively_without_holding_frames() {
    let (w, h) = (481, 1041);
    let pics: Vec<Arc<Vec<u8>>> = match (0..3).map(|n| sample_jpeg(w, h, n)).collect::<Result<Vec<_>, _>>() {
        Ok(p) => p.into_iter().map(Arc::new).collect(),
        Err(e) => {
            eprintln!("VideoToolbox can't make JPEG here ({e}), skipped");
            return;
        }
    };
    let (itx, irx) = tokio::sync::oneshot::channel();
    let mut s = Session::start(w, h, 20, 0, Some(itx));
    let info = match tokio::time::timeout(Duration::from_secs(10), irx).await {
        Ok(Ok(i)) => i,
        _ => {
            eprintln!("no VideoToolbox H.264 encoder here ({}), skipped", s.error());
            return;
        }
    };
    eprintln!("{info:?}");
    let mut got = Vec::new();
    for p in &pics {
        s.push(p.clone());
        loop {
            let pk = tokio::time::timeout(Duration::from_secs(5), s.next())
                .await
                .expect("a frame within 5 s of its picture, with nothing pushed after it")
                .expect("session alive")
                .unwrap_or_else(|e| panic!("{e}"));
            let frame = matches!(pk, Packet::Frame { .. });
            got.push(pk);
            if frame {
                break;
            }
        }
    }
    let Packet::Config(avcc) = &got[0] else { panic!("config first: {got:?}") };
    assert!(super::super::flv::codec_string(avcc).unwrap().starts_with("avc1.64"), "High profile");
    assert_eq!(avcc[4] & 3, 3, "4-byte NAL lengths");
    assert!(matches!(got[1], Packet::Frame { key: true, .. }));
    assert!(matches!(got[2], Packet::Frame { key: false, .. }));
    assert!(matches!(got[3], Packet::Frame { key: false, .. }));
    assert_eq!(s.written(), 3);
    // every frame is NAL units with 4-byte lengths that add up
    for p in &got[1..] {
        let Packet::Frame { data, .. } = p else { unreachable!() };
        let mut i = 0;
        while i < data.len() {
            let n = u32::from_be_bytes(data[i..i + 4].try_into().unwrap()) as usize;
            i += 4 + n;
        }
        assert_eq!(i, data.len());
    }
}
