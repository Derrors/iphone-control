use serde::Deserialize;

use super::*;

#[test]
fn empty_input_only_sends_a_key_with_submit() {
    let reply = Response::text("ok");
    for (submit, delivery) in [(false, Delivery::NotSent), (true, Delivery::Sent)] {
        assert_eq!(reply.delivery(&Request::TypeText { text: String::new(), submit }), delivery);
    }
}

/// 不带 count 的点击（旧版本客户端）按 1 下；count 为 1 时不写进请求，旧版本守护进程收到的和以前一样
#[test]
fn a_tap_without_count_is_one_tap_on_the_wire() {
    let old: Request = serde_json::from_str(r#"{"op":"tap","x":1.0,"y":2.0}"#).unwrap();
    assert_eq!(old, Request::Tap { x: 1.0, y: 2.0, count: 1 });
    assert_eq!(serde_json::to_string(&old).unwrap(), r#"{"op":"tap","x":1.0,"y":2.0}"#);
    let five = Request::Tap { x: 1.0, y: 2.0, count: 5 };
    assert_eq!(serde_json::to_string(&five).unwrap(), r#"{"op":"tap","x":1.0,"y":2.0,"count":5}"#);
}

/// A daemon that is restarting (it left the marker just now) is waited for; one that isn't running is reported
/// at once
#[tokio::test]
async fn clients_wait_for_a_restarting_daemon_only() {
    let dir = std::env::temp_dir().join(format!("ictl-connect-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("ctl.sock");
    let t = std::time::Instant::now();
    assert_eq!(connect(&socket).await.unwrap_err().kind(), std::io::ErrorKind::NotFound);
    assert!(t.elapsed() < std::time::Duration::from_millis(100), "not running: at once");

    std::fs::write(stopping_marker(&socket), b"").unwrap();
    let up = {
        let socket = socket.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(700)).await;
            let l = tokio::net::UnixListener::bind(&socket).unwrap();
            let _ = std::fs::remove_file(stopping_marker(&socket));
            l.accept().await.unwrap()
        })
    };
    let t = std::time::Instant::now();
    connect(&socket).await.expect("connects once it is back");
    assert!(t.elapsed() >= std::time::Duration::from_millis(600));
    up.await.unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn requests_round_trip_as_tagged_json() {
    let r = Request::TapElement {
        text: "x".into(),
        index: 0,
        max_age_ms: 0,
        scroll: None,
        selector: ElementSelector::default(),
    };
    // without a scroll search or selector the request is what it always was on the wire
    assert_eq!(serde_json::to_string(&r).unwrap(), r#"{"op":"tap_element","text":"x","index":0,"max_age_ms":0}"#);
    let r = Request::TapElement {
        text: "x".into(),
        index: 0,
        max_age_ms: 0,
        scroll: Some(ScrollSearch { down: true, max: 8 }),
        selector: ElementSelector { kind: Some("Button".into()), id: Some("ok-btn".into()), mode: MatchMode::Exact },
    };
    assert_eq!(serde_json::from_str::<Request>(&serde_json::to_string(&r).unwrap()).unwrap(), r);
    // empty text is allowed when kind or id is provided, and rejected when both are absent
    let id_only = Request::WaitFor {
        text: String::new(),
        timeout_ms: 5000,
        selector: ElementSelector { id: Some("done".into()), ..Default::default() },
    };
    assert!(id_only.check_required_text().is_ok());
    let empty_wait = Request::WaitFor { text: String::new(), timeout_ms: 5000, selector: ElementSelector::default() };
    assert!(empty_wait.check_required_text().is_err());
    let r = Request::Swipe { from: (1.0, 2.0), to: (3.0, 4.0), seconds: 0.3, hold: 0.0 };
    let s = serde_json::to_string(&r).unwrap();
    // a plain swipe is what it always was on the wire: older daemons and clients see no change
    assert_eq!(s, r#"{"op":"swipe","from":[1.0,2.0],"to":[3.0,4.0],"seconds":0.3}"#);
    assert_eq!(serde_json::from_str::<Request>(&s).unwrap(), r);
    let r = Request::Swipe { from: (1.0, 2.0), to: (3.0, 4.0), seconds: 0.7, hold: 0.6 };
    let s = serde_json::to_string(&r).unwrap();
    assert!(s.ends_with(r#""hold":0.6}"#), "{s}");
    assert_eq!(serde_json::from_str::<Request>(&s).unwrap(), r);
    let r = Request::TapSequence { points: vec![(60.0, 548.0), (23.0, 548.0)] };
    let s = serde_json::to_string(&r).unwrap();
    assert_eq!(s, r#"{"op":"tap_sequence","points":[[60.0,548.0],[23.0,548.0]]}"#);
    assert_eq!(serde_json::from_str::<Request>(&s).unwrap(), r);
    let e: Response = serde_json::from_str(r#"{"kind":"error","message":"x"}"#).unwrap();
    assert_eq!(e, Response::error("x"));
}

/// Outcome travels as fields; flags that are off and a missing outcome (older daemons) stay off the wire,
/// and older clients that don't know the field ignore it
#[test]
fn outcome_is_optional_on_the_wire() {
    let s = serde_json::to_string(&Response::text("x")).unwrap();
    assert_eq!(s, r#"{"kind":"text","text":"x"}"#);
    let s = serde_json::to_string(&Response::text_with("x", Outcome::default())).unwrap();
    assert_eq!(s, r#"{"kind":"text","text":"x","outcome":{}}"#);
    let missed = Response::text_with("Not found", Outcome { tapped_nothing: true, ..Default::default() });
    let s = serde_json::to_string(&missed).unwrap();
    assert_eq!(serde_json::from_str::<Response>(&s).unwrap(), missed);

    #[derive(Deserialize)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    enum OldResponse {
        Text { text: String },
    }
    let OldResponse::Text { text } = serde_json::from_str(&s).unwrap();
    assert_eq!(text, "Not found");
}

/// Data travels tagged by its type, and only when there is some
#[test]
fn data_travels_with_the_text() {
    let status = Data::Status(StatusData {
        wda_ready: true,
        locked: Some(false),
        foreground: Some("com.apple.springboard".into()),
        width: Some(430.0),
        height: Some(932.0),
        ..Default::default()
    });
    let r = Response::text_data("WDA ready: True", Outcome::default(), Some(status));
    let s = serde_json::to_string(&r).unwrap();
    assert!(s.contains(r#""data":{"type":"status","wda_ready":true"#), "{s}");
    assert_eq!(serde_json::from_str::<Response>(&s).unwrap(), r);
    let el = ElementData { kind: "Button".into(), label: "OK".into(), x: 1.0, y: 2.0, width: 3.0, height: 4.0 };
    let v = serde_json::to_value(Data::Elements {
        elements: vec![el.clone()],
        matched_count: Some(1),
        truncated: Some(false),
        incomplete: None,
        incomplete_reason: None,
    })
    .unwrap();
    assert_eq!(v["elements"][0]["type"], "Button", "the page reads `type`");
    assert!(v.get("incomplete").is_none(), "complete trees omit incomplete metadata");
    let inc = serde_json::to_value(Data::Elements {
        elements: vec![el],
        matched_count: Some(1),
        truncated: Some(false),
        incomplete: Some(true),
        incomplete_reason: Some("element_limit".into()),
    })
    .unwrap();
    assert_eq!(inc["incomplete"], true);
    assert_eq!(inc["incomplete_reason"], "element_limit");
    assert!(!serde_json::to_string(&Response::text_with("x", Outcome::default())).unwrap().contains("data"));
}

/// Delivery follows [`Outcome::tapped_nothing`], never the human-readable wording
#[test]
fn outcome_decides_delivery_not_text() {
    let tap = Request::TapElement {
        text: "x".into(),
        index: 0,
        max_age_ms: 0,
        scroll: None,
        selector: ElementSelector::default(),
    };
    let missed = Response::text_with("Not found", Outcome { tapped_nothing: true, ..Default::default() });
    assert_eq!(missed.delivery(&tap), Delivery::NotSent);
    let sent_cn = Response::text_with("没找到包含 \"x\" 的元素", Outcome::default());
    assert_eq!(sent_cn.delivery(&tap), Delivery::Sent);
    let no_outcome = Response::Text { text: "没找到包含 \"x\" 的元素".into(), outcome: None, data: None };
    assert_eq!(no_outcome.delivery(&tap), Delivery::Sent);
}

/// 旧版本守护进程直接反序列化请求：多出来的 cancel_on_close、lang 键不能让它报错（含没有字段的请求）
#[test]
fn extra_cancel_key_is_ignored_when_parsing() {
    for s in [
        r#"{"op":"status","cancel_on_close":true}"#,
        r#"{"op":"tap","x":1.0,"y":2.0,"cancel_on_close":true}"#,
        r#"{"op":"status","cancel_on_close":true,"lang":"en"}"#,
    ] {
        serde_json::from_str::<Request>(s).unwrap_or_else(|e| panic!("{s}: {e}"));
    }
}

/// `unlock_reset` without a device is what it always was on the wire, so older daemons and clients see no change
#[test]
fn unlock_reset_names_its_device_only_when_given() {
    let plain = Request::UnlockReset { udid: None };
    assert_eq!(serde_json::to_string(&plain).unwrap(), r#"{"op":"unlock_reset"}"#);
    assert_eq!(serde_json::from_str::<Request>(r#"{"op":"unlock_reset"}"#).unwrap(), plain);
    let one = Request::UnlockReset { udid: Some("00008130-0019295E012B803A".into()) };
    assert_eq!(serde_json::from_str::<Request>(&serde_json::to_string(&one).unwrap()).unwrap(), one);
}
