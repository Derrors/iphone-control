use super::*;
use std::collections::VecDeque;
use std::sync::Mutex;

/// 按顺序返回预设响应，并记录收到的请求
#[derive(Default)]
struct Mock {
    replies: Mutex<VecDeque<(u16, String)>>,
    seen: Mutex<Vec<(String, String, Option<String>)>>,
}

impl Mock {
    fn with(replies: &[(u16, &str)]) -> Self {
        Self { replies: Mutex::new(replies.iter().map(|(s, b)| (*s, b.to_string())).collect()), ..Default::default() }
    }
}

impl Transport for &Mock {
    async fn send(
        &self,
        method: &str,
        path: &str,
        body: Option<Vec<u8>>,
        _: Duration,
    ) -> std::io::Result<(u16, Vec<u8>)> {
        self.seen.lock().unwrap().push((method.into(), path.into(), body.map(|b| String::from_utf8(b).unwrap())));
        let (s, b) = self.replies.lock().unwrap().pop_front().ok_or_else(|| std::io::Error::other("连接被拒绝"))?;
        Ok((s, b.into_bytes()))
    }
}

#[tokio::test]
async fn display_poll_remeasures_size_only_when_needed() {
    let session = (200, r#"{"sessionId":"A"}"#);
    let size = (200, r#"{"value":{"screenSize":{"width":393,"height":852}}}"#);
    let portrait = (200, r#"{"value":{"z":0}}"#);
    let landscape = (200, r#"{"value":{"z":90}}"#);
    let m = Mock::with(&[session, portrait, size, portrait, landscape, size, landscape, landscape, landscape, size]);
    let mut w = WdaClient::new(&m);
    // (previous rotation, a poll that may refresh the size, size older than SCREEN_RECHECK, expected rotation)
    for (previous, refresh, stale, expected) in [
        (None, true, false, Rotation::Portrait),
        (Some(0), true, false, Rotation::Portrait),
        // rotated: measured whatever the caller
        (Some(0), false, false, Rotation::TopToRight),
        // a touch-down with a recent size: rotation only
        (Some(90), false, false, Rotation::TopToRight),
        // a touch-down with an old size: still rotation only (0.95 s on an iPad mini 5)
        (Some(90), false, true, Rotation::TopToRight),
        // a quiet poll with an old size: measured again (Display Zoom)
        (Some(90), true, true, Rotation::TopToRight),
    ] {
        if stale {
            w.screen_at = Some(Instant::now() - SCREEN_RECHECK);
        }
        let (size, rotation) = w.display_sample(previous, refresh, async { panic!("unexpected fallback") }).await;
        assert_eq!(rotation, Some(expected));
        assert_eq!(size.unwrap().width, 393.);
    }
    let paths: Vec<_> = m.seen.lock().unwrap().iter().map(|s| s.1.clone()).collect();
    assert_eq!(
        paths,
        [
            "/session",
            "/session/A/rotation",
            "/wda/screen",
            "/session/A/rotation",
            "/session/A/rotation",
            "/wda/screen",
            "/session/A/rotation",
            "/session/A/rotation",
            "/session/A/rotation",
            "/wda/screen"
        ]
    );
}

#[tokio::test]
async fn legacy_http_404_is_distinct_from_a_transient_rotation_failure() {
    let m = Mock::with(&[
        (200, r#"{"sessionId":"A"}"#),
        (500, r#"{"value":"runner busy"}"#),
        (200, r#"{"value":{"z":90}}"#),
        (404, r#"{"value":{"message":"Unhandled endpoint"}}"#),
    ]);
    let mut w = WdaClient::new(&m);
    assert_eq!(w.interface_rotation(async { panic!("500 is not a missing route") }).await, None);
    assert_eq!(w.interface_rotation(async { panic!("supported route") }).await, Some(Rotation::TopToRight));
    assert_eq!(w.interface_rotation(async { Some(Rotation::UpsideDown) }).await, Some(Rotation::UpsideDown));
}

#[tokio::test]
async fn legacy_wda_uses_a_fresh_interface_rotation_without_retrying_the_missing_route() {
    let m = Mock::with(&[
        (200, r#"{"sessionId":"A"}"#),
        (404, r#"{"value":{"error":"unknown command"}}"#),
        (200, r#"{"value":{"screenSize":{"width":393,"height":852}}}"#),
    ]);
    let mut w = WdaClient::new(&m);
    let (size, rotation) = w.display_sample(None, false, async { Some(Rotation::UpsideDown) }).await;
    assert_eq!(size.unwrap().width, 393.);
    assert_eq!(rotation, Some(Rotation::UpsideDown));
    assert_eq!(w.interface_rotation(async { Some(Rotation::TopToLeft) }).await, Some(Rotation::TopToLeft));
    assert_eq!(w.interface_rotation(async { None }).await, None);
    assert_eq!(m.seen.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn session_is_recreated_once_after_invalid_session() {
    let m = Mock::with(&[
        (200, r#"{"sessionId":"A","value":{}}"#),
        (404, r#"{"value":{"error":"invalid session id","message":"gone\nmore"}}"#),
        (200, r#"{"value":{"sessionId":"B"}}"#),
        (200, r#"{"value":null}"#),
    ]);
    let mut c = WdaClient::new(&m);
    c.press_button("home").await.unwrap();
    let paths: Vec<_> = m.seen.lock().unwrap().iter().map(|s| s.1.clone()).collect();
    assert_eq!(paths, ["/session", "/session/A/wda/pressButton", "/session", "/session/B/wda/pressButton"]);
}

#[tokio::test]
async fn hid_buttons_send_consumer_page_events() {
    let ok = r#"{"value":null}"#;
    let m = Mock::with(&[(200, r#"{"sessionId":"A"}"#), (200, ok), (200, ok)]);
    let mut c = WdaClient::new(&m);
    c.press_home_hid().await.unwrap();
    c.press_power_hid().await.unwrap();
    let seen = m.seen.lock().unwrap().clone();
    assert_eq!(seen[1].1, "/session/A/wda/performIoHidEvent");
    let body = |i: usize| serde_json::from_str::<Value>(seen[i].2.as_deref().unwrap()).unwrap();
    assert_eq!((body(1)["page"].as_u64(), body(1)["usage"].as_u64()), (Some(0x0C), Some(0x223)));
    assert_eq!((body(2)["page"].as_u64(), body(2)["usage"].as_u64()), (Some(0x0C), Some(0x30)));
    // 0.1 秒在 iPhone 15 Pro Max、iPad mini 5 上锁不了屏，0.5 秒在 iPhone 18 Pro Max 上锁不了（见 POWER_PRESS_SECS）
    assert_eq!(body(2)["duration"], 0.15);
}

#[tokio::test]
async fn tap_is_one_gesture_with_quick_waits_then_restores_baseline() {
    let ok = r#"{"value":null}"#;
    let m = Mock::with(&[
        (200, r#"{"sessionId":"A"}"#),
        (200, r#"{"value":{"waitForIdleTimeout":10,"animationCoolOffTimeout":2}}"#),
        (200, ok),
        (200, ok),
        (200, ok),
        // 第二次点击不再读设置
        (200, ok),
        (200, ok),
        (200, ok),
    ]);
    let mut c = WdaClient::new(&m);
    c.tap(10.0, 20.0).await.unwrap();
    c.tap(30.0, 40.0).await.unwrap();
    let seen = m.seen.lock().unwrap().clone();
    let paths: Vec<_> = seen.iter().map(|s| s.1.as_str()).collect();
    assert_eq!(
        paths,
        [
            "/session",
            "/session/A/appium/settings",
            "/session/A/appium/settings",
            "/session/A/actions",
            "/session/A/appium/settings",
            "/session/A/appium/settings",
            "/session/A/actions",
            "/session/A/appium/settings",
        ]
    );
    let body = |i: usize| serde_json::from_str::<Value>(seen[i].2.as_deref().unwrap()).unwrap();
    assert_eq!(body(2)["settings"]["animationCoolOffTimeout"], 0.0);
    assert_eq!(body(4)["settings"]["animationCoolOffTimeout"], 2.0);
    let a = body(3);
    let steps = a["actions"][0]["actions"].as_array().unwrap();
    assert_eq!(steps.iter().filter(|s| s["type"] == "pointerDown").count(), 1);
}

/// 启动不经 XCTest（`launchUnattached`），发出就返回；系统 App 第一次打开时它回 failed to launch，前台是隐私说明页，
/// 马上报被挡住，不再等 60 秒
#[tokio::test]
async fn a_first_launch_privacy_notice_is_reported_at_once() {
    let fail = r#"{"value":{"error":"unknown error","message":"LSApplicationWorkspace failed to launch app"}}"#;
    let m = Mock::with(&[(500, fail), (200, r#"{"value":{"bundleId":"com.apple.PDUIApp"}}"#)]);
    let mut w = WdaClient::new(&m);
    let t0 = std::time::Instant::now();
    let r = w.launch_app("com.apple.mobilenotes").await.unwrap();
    assert!(matches!(r, Launch::Blocked(ref t) if !t.is_empty()), "{r:?}");
    assert!(t0.elapsed() < Duration::from_secs(1));
    let seen = m.seen.lock().unwrap();
    assert_eq!(seen[0].1, "/wda/apps/launchUnattached");
    assert!(seen.iter().all(|(_, p, _)| !p.ends_with("/wda/apps/launch")));
}

/// 旧版 WDA 没有 launchUnattached：照旧经会话启动
#[tokio::test]
async fn an_older_wda_without_unattached_launch_launches_as_before() {
    let m = Mock::with(&[
        (404, r#"{"value":{"error":"unknown command","message":"Unhandled endpoint"}}"#),
        (200, r#"{"sessionId":"A"}"#),
        (200, r#"{"value":null}"#),
        (200, r#"{"value":{"bundleId":"com.apple.PDUIApp"}}"#),
    ]);
    let mut w = WdaClient::new(&m);
    assert!(matches!(w.launch_app("com.apple.mobilenotes").await.unwrap(), Launch::Blocked(_)));
    let seen = m.seen.lock().unwrap();
    assert_eq!(seen[2].1, "/session/A/wda/apps/launch");
}

/// 别的错误照样报出来，不当成启动了
#[tokio::test]
async fn other_launch_errors_are_errors() {
    let m = Mock::with(&[(500, r#"{"value":{"error":"invalid argument","message":"bad bundle"}}"#)]);
    let mut w = WdaClient::new(&m);
    assert!(matches!(w.launch_app("x").await, Err(WdaError::Remote { .. })));
}

/// 依次点几个点：每个点一根手指，第 k 根先空等前面的点按完；各手指的节拍数对得上，按下抬起一个接一个、不重叠
#[test]
fn a_sequence_is_one_finger_per_point_in_turn() {
    let body = tap_sequence_body(&[(10.0, 20.0), (30.0, 40.0), (50.0, 60.0)], 50, 40);
    let fingers = body["actions"].as_array().unwrap();
    assert_eq!(fingers.len(), 3);
    for (k, f) in fingers.iter().enumerate() {
        let a = f["actions"].as_array().unwrap();
        assert_eq!(a.len(), 5 * k + 4, "finger {k}");
        // pauses only until its own turn, then move, down, hold, up at its point
        assert!(a[..5 * k].iter().all(|s| s["type"] == "pause"));
        assert_eq!(a[5 * k]["type"], "pointerMove");
        assert_eq!(a[5 * k]["x"].as_f64(), Some(10.0 + 20.0 * k as f64));
        assert_eq!(a[5 * k + 1]["type"], "pointerDown");
        assert_eq!(a[5 * k + 3]["type"], "pointerUp");
        assert_ne!(fingers[0]["id"], fingers[k.max(1)]["id"]);
    }
    // finger k waits 5 ticks per earlier point: move, down, hold, up, gap
    assert_eq!(fingers[1]["actions"][2]["duration"], 50);
    assert_eq!(fingers[1]["actions"][4]["duration"], 40);
}

/// 连点是一个 actions 请求：一根手指只移动一次，在同一点按下抬起 N 次，两下之间停 MULTI_TAP_GAP_MS；
/// 5 下在 0.8 秒内点完
#[tokio::test]
async fn multi_tap_is_one_request_at_one_point() {
    let ok = r#"{"value":null}"#;
    let m = Mock::with(&[
        (200, r#"{"sessionId":"A"}"#),
        (200, r#"{"value":{"waitForIdleTimeout":10,"animationCoolOffTimeout":2}}"#),
        (200, ok),
        (200, ok),
        (200, ok),
    ]);
    let mut c = WdaClient::new(&m);
    c.multi_tap(10.0, 20.0, 5).await.unwrap();
    let seen = m.seen.lock().unwrap().clone();
    let actions: Vec<_> = seen.iter().filter(|s| s.1.ends_with("/actions")).collect();
    assert_eq!(actions.len(), 1);
    let body = serde_json::from_str::<Value>(actions[0].2.as_deref().unwrap()).unwrap();
    let steps = body["actions"][0]["actions"].as_array().unwrap();
    let count = |t: &str| steps.iter().filter(|s| s["type"] == t).count();
    assert_eq!((count("pointerMove"), count("pointerDown"), count("pointerUp")), (1, 5, 5));
    assert_eq!((steps[0]["x"].as_f64(), steps[0]["y"].as_f64()), (Some(10.0), Some(20.0)));
    let ms: u64 = steps.iter().filter_map(|s| s["duration"].as_u64()).sum();
    assert_eq!(ms, 5 * TAP_PRESS_MS + 4 * MULTI_TAP_GAP_MS);
    assert!(ms < 800);
}

#[tokio::test]
async fn remote_error_keeps_first_line_and_unreachable_maps_to_unavailable() {
    let m = Mock::with(&[
        (200, r#"{"sessionId":"A"}"#),
        (500, r#"{"value":{"error":"unknown error","message":"boom\nstack"}}"#),
    ]);
    let mut c = WdaClient::new(&m);
    match c.press_button("home").await {
        Err(WdaError::Remote { kind, message }) => {
            assert_eq!((kind.as_str(), message.as_str()), ("unknown error", "boom"))
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(c.active_app().await, Err(WdaError::Unavailable(_))));
}

/// 传输层报的错误按「请求送没送到」分类：只有没送到的才是 Unavailable（调用方会退到 HID 重做）
#[tokio::test]
async fn errors_after_sending_are_not_unavailable() {
    struct Fails(std::io::ErrorKind);
    impl Transport for Fails {
        async fn send(&self, _: &str, _: &str, _: Option<Vec<u8>>, _: Duration) -> std::io::Result<(u16, Vec<u8>)> {
            Err(std::io::Error::new(self.0, "x"))
        }
    }
    let kind = |k| async move { WdaClient::new(Fails(k)).active_app().await };
    assert!(matches!(kind(crate::http::AFTER_SENT).await, Err(WdaError::Interrupted(_))));
    assert!(matches!(kind(crate::http::WRITE_FAILED).await, Err(WdaError::Unavailable(_))));
    let client = WdaClient::new(Fails(crate::http::WRITE_FAILED));
    for (path, acts) in [("/session/1/element", false), ("/session/1/wda/keys", true)] {
        let error = client.send("POST", path, None, Duration::from_secs(1)).await.unwrap_err();
        assert!(if acts { matches!(error, WdaError::Write(_)) } else { matches!(error, WdaError::Unavailable(_)) });
    }
    assert!(matches!(kind(std::io::ErrorKind::TimedOut).await, Err(WdaError::Timeout(_))));
    assert!(matches!(kind(std::io::ErrorKind::NotConnected).await, Err(WdaError::Unavailable(_))));
    // 应答了但不是 JSON：WDA 收到了请求
    let m = Mock::with(&[(200, "<html>")]);
    assert!(matches!(WdaClient::new(&m).active_app().await, Err(WdaError::Interrupted(_))));
}

/// 经 XCTest 的请求（工具调用和探测一样算）连续 3 次没有应答就判定卡死：之后不再发，直接报不可用；
/// `/status` 不经 XCTest，既不算数也不受影响；中间有一次应答（哪怕是错误）就从头数。
/// 长请求没应答时马上问一次前台 App（5 秒）：它也没应答就立刻判定卡死，它应答了只是慢
#[tokio::test]
async fn three_unanswered_requests_mean_stalled() {
    type Reply = std::io::Result<(u16, Vec<u8>)>;
    struct Scripted {
        replies: Mutex<VecDeque<Reply>>,
        sent: Mutex<Vec<String>>,
    }
    impl Transport for &Scripted {
        async fn send(&self, _: &str, path: &str, _: Option<Vec<u8>>, _: Duration) -> std::io::Result<(u16, Vec<u8>)> {
            self.sent.lock().unwrap().push(path.to_owned());
            self.replies.lock().unwrap().pop_front().expect("unexpected request")
        }
    }
    let timeout = || Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "t"));
    let ok = |b: &str| Ok((200, b.as_bytes().to_vec()));
    let t = Scripted {
        replies: Mutex::new(VecDeque::from([
            // a long request unanswered, the quick question after it answered: slow, not stuck
            timeout(),
            ok(r#"{"value":{}}"#),
            timeout(),
            ok(r#"{"value":{}}"#),
            ok(r#"{"value":{"error":"unknown error","message":"busy"}}"#),
            // the probe itself is short: no question after it
            timeout(),
            ok(r#"{"value":{"ready":true}}"#),
            // a long request and the quick question both unanswered: stuck at once
            timeout(),
            timeout(),
            ok(r#"{"value":{"ready":true}}"#),
        ])),
        sent: Mutex::default(),
    };
    let c = WdaClient::new(&t);
    assert!(c.wda_reports_locked().await.unwrap_err().no_answer());
    assert!(c.active_app().await.unwrap_err().no_answer());
    assert_eq!(c.unanswered(), 0, "两次短探测均获得回答，应清零");
    // 回了错误：还活着，从头数
    assert!(matches!(c.active_app().await, Err(WdaError::Remote { .. })));
    assert_eq!(c.unanswered(), 0);
    assert!(c.xctest_responsive().await.unwrap_err().no_answer());
    // /status 应答了也不清零
    assert!(c.ready().await.unwrap());
    assert_eq!(c.unanswered(), 1);
    assert!(!c.stalled());
    assert!(c.wda_reports_locked().await.is_err());
    assert!(c.stalled(), "the quick question went unanswered too");
    assert!(t.replies.lock().unwrap().len() == 1, "every scripted reply but /status was used");
    // 卡死后经 XCTest 的请求不再发出，直接报不可用（调用方可以退到 HID）；/status 照常
    let sent = t.sent.lock().unwrap().len();
    assert!(matches!(c.screenshot_png().await, Err(WdaError::Unavailable(_))));
    assert!(matches!(c.xctest_responsive().await, Err(WdaError::Unavailable(_))));
    assert_eq!(t.sent.lock().unwrap().len(), sent);
    assert!(c.ready().await.unwrap());
    assert!(c.stalled());
}

/// 较长请求超时后，短探测的正常回答或远端错误都证明 WDA 仍有响应，应重新计数。
#[tokio::test]
async fn answered_probe_resets_stall_count_after_each_slow_request() {
    for (status, body) in [
        (200, r#"{"value":{}}"#),
        (500, r#"{"value":{"error":"unknown error","message":"busy"}}"#),
        (404, r#"{"message":"unknown route"}"#),
        (502, "temporarily unavailable"),
    ] {
        let replies = (0..4).flat_map(|_| [timed_out(), Ok((status, body.as_bytes().to_vec()))]).collect();
        let t = Script::new(replies);
        let c = WdaClient::new(&t);
        for _ in 0..4 {
            assert!(c.wda_reports_locked().await.unwrap_err().no_answer());
            assert_eq!(c.unanswered(), 0, "探测返回 {status} {body}");
            assert!(!c.stalled());
        }
        assert_eq!(t.sent().len(), 8, "每个慢请求后都应继续做短探测");
    }
}

/// 连不上不是收到回答，不能抹掉之前已经发生的无应答。
#[tokio::test]
async fn connection_error_does_not_reset_unanswered_count() {
    let t = Script::new(vec![
        timed_out(),
        Err(std::io::Error::new(std::io::ErrorKind::NotConnected, "offline")),
        Ok((500, br#"{"value":{"error":"unknown error","message":"busy"}}"#.to_vec())),
    ]);
    let c = WdaClient::new(&t);
    assert!(c.xctest_responsive().await.unwrap_err().no_answer());
    assert_eq!(c.unanswered(), 1);
    assert!(matches!(c.xctest_responsive().await, Err(WdaError::Unavailable(_))));
    assert_eq!(c.unanswered(), 1);
    assert!(matches!(c.xctest_responsive().await, Err(WdaError::Remote { .. })));
    assert_eq!(c.unanswered(), 0);
    assert!(!c.stalled());
}

/// 两个请求未应答即可触发快速恢复，但实际计数和原因不能伪装成三次无应答。
#[tokio::test]
async fn long_request_and_failed_probe_keep_the_actual_count_and_reason() {
    let t = Script::new(vec![timed_out(), timed_out(), ok(r#"{"value":{"ready":true}}"#)]);
    let c = WdaClient::new(&t);
    assert!(c.wda_reports_locked().await.unwrap_err().no_answer());
    assert_eq!(c.unanswered(), 2);
    assert_eq!(c.stall_reason(), Some(StallReason::LongRequestAndProbe));
    assert!(c.stalled());
    let why = c.stall_reason().unwrap().describe();
    assert!(why.contains("/wda/activeAppInfo"));
    assert!(!why.contains("3 次"));
    // 本地 /status 的回答不应把已记录的判定抹掉。
    assert!(c.ready().await.unwrap());
    assert_eq!(c.stall_reason(), Some(StallReason::LongRequestAndProbe));
    assert_eq!(t.sent(), ["GET /wda/locked", "GET /wda/activeAppInfo", "GET /status"]);
}

#[tokio::test]
async fn three_short_unanswered_requests_report_a_consecutive_stall() {
    let t = Script::new(vec![timed_out(), timed_out(), timed_out()]);
    let c = WdaClient::new(&t);
    for n in 1..=STALL_LIMIT {
        assert!(c.xctest_responsive().await.unwrap_err().no_answer());
        assert_eq!(c.unanswered(), n);
        assert_eq!(c.stalled(), n == STALL_LIMIT);
    }
    assert_eq!(c.stall_reason(), Some(StallReason::ConsecutiveRequests(STALL_LIMIT)));
    // 后续调用仍立即拒绝，不再发送请求。
    assert!(matches!(c.xctest_responsive().await, Err(WdaError::Unavailable(_))));
    assert_eq!(t.sent().len(), STALL_LIMIT as usize);
}

/// 结束会话的回答只用于清理，不应代替真正的前台响应探测。
#[tokio::test]
async fn answered_session_delete_does_not_mask_a_failed_probe() {
    let t = Script::new(vec![ok(r#"{"sessionId":"A"}"#), timed_out(), ok(r#"{"value":null}"#), timed_out()]);
    let mut c = WdaClient::new(&t);
    assert!(matches!(c.press_button("home").await, Err(WdaError::Timeout(_))));
    assert_eq!(c.unanswered(), 2);
    assert_eq!(c.stall_reason(), Some(StallReason::LongRequestAndProbe));
    assert_eq!(t.sent()[2], "DELETE /session/A");
}

/// 经 XCTest 的探测：只有「送到了却没有应答」算卡死的迹象；WDA 回了错误说明它还在办事，连不上归 `/status` 管
#[tokio::test]
async fn xctest_probe_counts_only_missing_answers() {
    struct Fails(std::io::ErrorKind);
    impl Transport for Fails {
        async fn send(&self, _: &str, _: &str, _: Option<Vec<u8>>, t: Duration) -> std::io::Result<(u16, Vec<u8>)> {
            assert_eq!(t, XCTEST_PROBE_TIMEOUT);
            Err(std::io::Error::new(self.0, "x"))
        }
    }
    let probe = |k| async move { WdaClient::new(Fails(k)).xctest_responsive().await };
    assert!(probe(std::io::ErrorKind::TimedOut).await.unwrap_err().no_answer());
    assert!(probe(crate::http::AFTER_SENT).await.unwrap_err().no_answer());
    assert!(!probe(std::io::ErrorKind::NotConnected).await.unwrap_err().no_answer());
    let m = Mock::with(&[
        (200, r#"{"value":{"bundleId":"com.apple.springboard","pid":60}}"#),
        (500, r#"{"value":{"error":"unknown error","message":"busy"}}"#),
    ]);
    let c = WdaClient::new(&m);
    assert!(c.xctest_responsive().await.is_ok());
    assert!(!c.xctest_responsive().await.unwrap_err().no_answer());
    assert_eq!(m.seen.lock().unwrap()[0].1, "/wda/activeAppInfo");
}

#[tokio::test]
async fn screen_size_is_cached_and_status_ready() {
    let unknown = r#"{"value":{"error":"unknown command","message":"no"}}"#;
    let m = Mock::with(&[
        (200, r#"{"value":{"screenSize":{"width":430,"height":932},"scale":3}}"#),
        (200, r#"{"sessionId":"S"}"#),
        (404, unknown),
        (404, unknown),
        (200, r#"{"value":{"ready":true}}"#),
    ]);
    let mut c = WdaClient::new(&m);
    // 问不到方向：按缓存的尺寸
    assert_eq!(c.screen_size().await.unwrap(), ScreenSize { width: 430.0, height: 932.0 });
    assert_eq!(c.screen_size().await.unwrap().width, 430.0);
    assert!(c.ready().await.unwrap());
    let paths: Vec<_> = m.seen.lock().unwrap().iter().map(|s| s.1.clone()).collect();
    assert_eq!(paths.iter().filter(|p| *p == "/wda/screen").count(), 1);
}

#[tokio::test]
async fn rotation_reads_z_angle() {
    let m = Mock::with(&[
        (200, r#"{"sessionId":"S"}"#),
        (200, r#"{"value":{"x":0,"y":0,"z":270}}"#),
        (200, r#"{"value":{"x":0,"y":0,"z":12}}"#),
    ]);
    let mut c = WdaClient::new(&m);
    assert_eq!(c.rotation().await.unwrap(), ictl_device::Rotation::TopToLeft);
    assert!(c.rotation().await.is_err());
    assert_eq!(m.seen.lock().unwrap()[1].1, "/session/S/rotation");
}

/// WDA 在横屏时启动，缓存的是 1024x768；转成竖屏后要给 768x1024
#[tokio::test]
async fn screen_size_follows_orientation_after_rotation() {
    let m = Mock::with(&[
        (200, r#"{"value":{"screenSize":{"width":1024,"height":768},"scale":2}}"#),
        (200, r#"{"sessionId":"S"}"#),
        (200, r#"{"value":"LANDSCAPE"}"#),
        (200, r#"{"value":"PORTRAIT"}"#),
    ]);
    let mut c = WdaClient::new(&m);
    let v = c.view().await.unwrap();
    assert_eq!(
        (v.screen, v.rotation),
        (ScreenSize { width: 1024.0, height: 768.0 }, Some(ictl_device::Rotation::TopToLeft))
    );
    assert_eq!(c.screen_size().await.unwrap(), ScreenSize { width: 768.0, height: 1024.0 });
    assert_eq!(m.seen.lock().unwrap()[2].1, "/session/S/orientation");
}

/// 从主屏打开网址：等 Safari 到前台、元素树稳定才返回，不是发完请求就走；一闪而过的系统服务不算
#[tokio::test]
async fn open_url_waits_for_the_opening_app() {
    let m = Mock::with(&[
        (200, r#"{"value":{"bundleId":"com.apple.springboard"}}"#),
        (200, r#"{"sessionId":"S"}"#),
        (200, r#"{"value":null}"#),
        (200, r#"{"value":{"bundleId":"com.apple.DocumentManager.DockFolderViewService"}}"#),
        (200, r#"{"value":{"bundleId":"com.apple.mobilesafari"}}"#),
        (200, r#"{"value":{"bundleId":"com.apple.mobilesafari"}}"#),
        (200, r#"{"value":"<a/>"}"#),
        (200, r#"{"value":"<a/>"}"#),
    ]);
    let mut c = WdaClient::new(&m);
    let t0 = std::time::Instant::now();
    let (r, app) = c.open_url("https://example.com").await.unwrap();
    assert_eq!((r, app.as_str()), (Launch::Foreground, "com.apple.mobilesafari"));
    assert!(t0.elapsed() >= LAUNCH_MIN);
    let paths: Vec<_> = m.seen.lock().unwrap().iter().map(|s| s.1.clone()).collect();
    assert_eq!(paths[2], "/session/S/url");
    assert!(paths[6].starts_with("/source"), "{paths:?}");
}

#[tokio::test]
async fn source_request_excludes_slow_attributes() {
    let m = Mock::with(&[(200, r#"{"value":"<x/>"}"#)]);
    let mut c = WdaClient::new(&m);
    c.source_xml().await.unwrap();
    let path = m.seen.lock().unwrap()[0].1.clone();
    assert!(path.starts_with("/source?format=xml&excluded_attributes=visible,"), "{path}");
}

#[tokio::test]
async fn passcode_field_reads_secure_text_field_value() {
    let m = Mock::with(&[
        (200, r#"{"sessionId":"S"}"#),
        (200, r#"{"value":[{"ELEMENT":"E1","element-6066-11e4-a52e-4f735466cecf":"E1"}]}"#),
        (200, r#"{"value":"共6位数，已输入2位"}"#),
    ]);
    let mut c = WdaClient::new(&m);
    assert_eq!(c.passcode_field().await.unwrap(), Some((6, 2)));
    assert_eq!(m.seen.lock().unwrap()[2].1, "/session/S/element/E1/attribute/value");
}

#[tokio::test]
async fn focused_field_reads_its_type_and_placeholder() {
    let m = Mock::with(&[
        (200, r#"{"sessionId":"S"}"#),
        (200, r#"{"value":{"ELEMENT":"F1","element-6066-11e4-a52e-4f735466cecf":"F1"}}"#),
        (200, r#"{"value":"XCUIElementTypeSearchField"}"#),
        (200, r#"{"value":"搜索"}"#),
        // empty: reads its placeholder
        (200, r#"{"value":"搜索"}"#),
        (200, r#"{"value":"wifi"}"#),
    ]);
    let mut c = WdaClient::new(&m);
    let f = c.focused_field().await.unwrap().unwrap();
    assert_eq!(f, FocusedField { id: "F1".into(), kind: "SearchField".into(), placeholder: "搜索".into() });
    assert_eq!(c.field_text(&f).await.unwrap(), "");
    assert_eq!(c.field_text(&f).await.unwrap(), "wifi");
    let paths: Vec<_> = m.seen.lock().unwrap().iter().map(|s| s.1.clone()).collect();
    assert_eq!(
        paths[1..4],
        [
            "/session/S/element/active",
            "/session/S/element/F1/attribute/type",
            "/session/S/element/F1/attribute/placeholderValue"
        ]
    );
}

#[tokio::test]
async fn no_focused_element_is_none() {
    let m = Mock::with(&[
        (200, r#"{"sessionId":"S"}"#),
        (404, r#"{"value":{"error":"no such element","message":"No element is in focus"}}"#),
    ]);
    assert_eq!(WdaClient::new(&m).focused_field().await.unwrap(), None);
}

/// Finding elements reads; clearing one changes the phone (its session is ended when it doesn't answer)
#[test]
fn element_finds_read_and_element_edits_act() {
    assert!(!acts("POST", "/elements"));
    assert!(!acts("POST", "/element"));
    assert!(!acts("POST", "/element/E1/elements"));
    assert!(!acts("GET", "/element/active"));
    assert!(acts("POST", "/element/E1/clear"));
    assert!(acts("POST", "/wda/keys"));
}

#[test]
fn actions_body_is_one_pointer_with_steps() {
    let b = actions_body(&tap_steps(10.0, 20.0, 80));
    let a = &b["actions"][0];
    assert_eq!(a["parameters"]["pointerType"], "touch");
    let types: Vec<_> = a["actions"].as_array().unwrap().iter().map(|s| s["type"].as_str().unwrap()).collect();
    assert_eq!(types, ["pointerMove", "pointerDown", "pause", "pointerUp"]);
    assert_eq!(a["actions"][2]["duration"], 80);
    let s = swipe_steps((1.0, 2.0), (3.0, 4.0), 300);
    assert_eq!(s[3], PointerStep::Move { x: 3.0, y: 4.0, ms: 300 });
}

/// 按住再拖：按下后原地 move 按住（不是 pause，pause 拖不动图标），再移到终点；不按住就是普通滑动
#[test]
fn a_held_drag_holds_with_a_move_in_place() {
    let s = drag_steps((1.0, 2.0), (3.0, 4.0), 600, 700);
    assert_eq!(
        s,
        [
            PointerStep::Move { x: 1.0, y: 2.0, ms: 0 },
            PointerStep::Down,
            PointerStep::Move { x: 1.0, y: 2.0, ms: 600 },
            PointerStep::Move { x: 3.0, y: 4.0, ms: 700 },
            PointerStep::Up,
        ]
    );
    assert_eq!(drag_steps((1.0, 2.0), (3.0, 4.0), 0, 300), swipe_steps((1.0, 2.0), (3.0, 4.0), 300));
}

#[tokio::test]
async fn locked_checks_tree_until_unlocked_once_then_trusts_wda() {
    let lock_xml = serde_json::to_string(include_str!("../../tests/fixtures/lockscreen-ios27.xml")).unwrap();
    let home_xml = serde_json::to_string(include_str!("../../tests/fixtures/home.xml")).unwrap();
    let sb = r#"{"value":{"bundleId":"com.apple.springboard"}}"#;
    let no = r#"{"value":false}"#;
    let (lock_src, home_src) = (format!(r#"{{"value":{lock_xml}}}"#), format!(r#"{{"value":{home_xml}}}"#));
    let m = Mock::with(&[
        // 1. WDA 在锁屏时启动：/wda/locked 误报 false，元素树是锁屏
        (200, no),
        (200, sb),
        (200, &lock_src),
        // 2. 人工解锁后：主屏幕，确认一次
        (200, no),
        (200, sb),
        (200, &home_src),
        // 3. 之后 WDA 说没锁就直接信
        (200, no),
        // 4. WDA 说锁了：核对一次，确实是锁屏
        (200, r#"{"value":true}"#),
        (200, sb),
        (200, &lock_src),
    ]);
    let mut c = WdaClient::new(&m);
    assert!(c.locked().await.unwrap());
    assert!(!c.locked().await.unwrap());
    assert!(!c.locked().await.unwrap());
    assert!(c.locked().await.unwrap());
    let paths: Vec<String> = m.seen.lock().unwrap().iter().map(|(_, p, _)| p.clone()).collect();
    assert_eq!(paths.iter().filter(|p| p.starts_with("/source")).count(), 3, "{paths:?}");
}

#[tokio::test]
async fn notification_center_is_not_locked_even_if_wda_says_so() {
    let nc = serde_json::to_string(include_str!("../../tests/fixtures/notification-center-ios27.xml")).unwrap();
    let pin = serde_json::to_string(include_str!("../../tests/fixtures/passcode-view-ios27.xml")).unwrap();
    let sb = r#"{"value":{"bundleId":"com.apple.springboard"}}"#;
    let yes = r#"{"value":true}"#;
    let (nc, pin) = (format!(r#"{{"value":{nc}}}"#), format!(r#"{{"value":{pin}}}"#));
    let m = Mock::with(&[(200, yes), (200, sb), (200, &nc), (200, yes), (200, sb), (200, &pin), (200, yes)]);
    let mut c = WdaClient::new(&m);
    assert!(!c.locked().await.unwrap(), "通知中心");
    assert!(c.locked().await.unwrap(), "密码键盘界面");
    // 核对失败时按 WDA 的结果
    assert!(c.locked().await.unwrap());
}

#[tokio::test]
async fn locked_falls_back_to_wda_when_tree_check_fails() {
    // 核对失败（例如取元素树超时）不能把可用的手机误判成锁屏，也不能就此信任 WDA
    let m = Mock::with(&[(200, r#"{"value":false}"#)]);
    let mut c = WdaClient::new(&m);
    assert!(!c.locked().await.unwrap());
    assert!(!c.lock_state_trusted);
}

/// 传输层给出的一个结果：状态码和响应体，或者超时之类的错误
type Reply = std::io::Result<(u16, Vec<u8>)>;

/// 按顺序给出预设结果（可以是超时），记下每个请求的方法、路径和请求体
#[derive(Default)]
struct Script {
    replies: Mutex<VecDeque<Reply>>,
    sent: Mutex<Vec<(String, String, Option<Value>)>>,
}

impl Script {
    fn new(replies: Vec<Reply>) -> Self {
        Self { replies: Mutex::new(replies.into()), ..Default::default() }
    }

    fn sent(&self) -> Vec<String> {
        self.sent.lock().unwrap().iter().map(|(m, p, _)| format!("{m} {p}")).collect()
    }

    fn body(&self, i: usize) -> Value {
        self.sent.lock().unwrap()[i].2.clone().expect("a body")
    }
}

impl Transport for &Script {
    async fn send(
        &self,
        method: &str,
        path: &str,
        body: Option<Vec<u8>>,
        _: Duration,
    ) -> std::io::Result<(u16, Vec<u8>)> {
        let body = body.map(|b| serde_json::from_slice(&b).unwrap());
        self.sent.lock().unwrap().push((method.into(), path.into(), body));
        self.replies.lock().unwrap().pop_front().expect("unexpected request")
    }
}

fn timed_out() -> Reply {
    Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "t"))
}

fn ok(body: &str) -> Reply {
    Ok((200, body.as_bytes().to_vec()))
}

/// 动作没有应答：先结束会话（WDA 不再执行这个会话里还在排队的请求），再问一次前台 App 判断是不是卡死；
/// 下一个动作开新会话
#[tokio::test]
async fn an_unanswered_action_ends_its_session() {
    let t = Script::new(vec![
        ok(r#"{"sessionId":"A"}"#),
        timed_out(),
        ok(r#"{"value":null}"#),
        ok(r#"{"value":{}}"#),
        ok(r#"{"sessionId":"B"}"#),
        ok(r#"{"value":null}"#),
    ]);
    let mut c = WdaClient::new(&t);
    assert!(matches!(c.press_button("home").await, Err(WdaError::Timeout(_))));
    c.press_button("home").await.unwrap();
    assert_eq!(
        t.sent(),
        [
            "POST /session",
            "POST /session/A/wda/pressButton",
            "DELETE /session/A",
            "GET /wda/activeAppInfo",
            "POST /session",
            "POST /session/B/wda/pressButton",
        ]
    );
}

/// 读没有应答不结束会话：排在读后面的不会动手机，结束了只会让下一次开会话也排到它后面
#[tokio::test]
async fn an_unanswered_read_keeps_its_session() {
    let t = Script::new(vec![
        ok(r#"{"sessionId":"A"}"#),
        timed_out(),
        ok(r#"{"value":{}}"#),
        timed_out(),
        ok(r#"{"value":{}}"#),
        ok(r#"{"value":null}"#),
    ]);
    let mut c = WdaClient::new(&t);
    assert!(c.alert_text().await.unwrap_err().no_answer());
    assert!(c.keyboard_shown().await.unwrap_err().no_answer());
    c.press_button("home").await.unwrap();
    assert_eq!(
        t.sent(),
        [
            "POST /session",
            "GET /session/A/alert/text",
            "GET /wda/activeAppInfo",
            "POST /session/A/elements",
            "GET /wda/activeAppInfo",
            "POST /session/A/wda/pressButton",
        ]
    );
}

#[test]
fn snapshot_limits_tighten_when_slow_and_loosen_when_fast() {
    let slow = Some(SLOW_SOURCE);
    let fast = Some(Duration::from_millis(300));
    let fair = Some(Duration::from_secs(3));
    assert_eq!(next_snapshot_level(0, slow), 1);
    assert_eq!(next_snapshot_level(1, None), 2);
    assert_eq!(next_snapshot_level(3, None), 3, "never tighter than the last level");
    assert_eq!(next_snapshot_level(2, fair), 2);
    assert_eq!(next_snapshot_level(2, fast), 1);
    assert_eq!(next_snapshot_level(0, fast), 0);
    assert_eq!(next_snapshot_level(0, fair), 0);
    // 先限子元素再减层数，都有下限
    assert_eq!(SNAPSHOT_LEVELS[1], SnapshotLimits { depth: 50, children: Some(150) });
    assert!(SNAPSHOT_LEVELS.windows(2).all(|w| w[1].depth <= w[0].depth));
    assert_eq!(SNAPSHOT_LEVELS[3], SnapshotLimits { depth: 12, children: Some(60) });
}

/// 读树超时后下一次先设上限再读，读得快了再放宽；新会话里 WDA 是不限的，不为读树专门设
#[tokio::test]
async fn a_timed_out_tree_is_read_again_with_limits() {
    let xml = r#"{"value":"<XCUIElementTypeApplication/>"}"#;
    let t = Script::new(vec![
        timed_out(),
        ok(r#"{"value":{}}"#),
        ok(r#"{"sessionId":"A"}"#),
        ok(r#"{"value":null}"#),
        ok(xml),
        ok(r#"{"value":null}"#),
        ok(xml),
        ok(xml),
    ]);
    let mut c = WdaClient::new(&t);
    assert!(matches!(c.source_xml().await, Err(WdaError::Timeout(_))));
    assert_eq!(c.snapshot_note(), None, "nothing was read with limits yet");
    assert_eq!(c.source_xml().await.unwrap(), "<XCUIElementTypeApplication/>");
    assert!(c.snapshot_note().unwrap().contains("150"));
    c.source_xml().await.unwrap();
    assert_eq!(c.snapshot_note(), None);
    c.source_xml().await.unwrap();
    let sent = t.sent();
    assert!(sent[0].starts_with("GET /source?"));
    assert_eq!(sent[1..4], ["GET /wda/activeAppInfo", "POST /session", "POST /session/A/appium/settings"]);
    assert_eq!(t.body(3)["settings"], json!({"snapshotMaxDepth": 50, "snapshotMaxChildren": 150}));
    assert_eq!(sent[5], "POST /session/A/appium/settings");
    assert_eq!(t.body(5)["settings"]["snapshotMaxChildren"], i32::MAX);
    assert_eq!(sent.len(), 8, "the last read needs no settings: {sent:?}");
}

#[test]
fn the_detection_point_is_below_banners_in_either_orientation() {
    assert_eq!(detection_point(ScreenSize { width: 430.0, height: 932.0 }), (215, 215));
    assert_eq!(detection_point(ScreenSize { width: 932.0, height: 430.0 }), (215, 215));
    assert_eq!(detection_point(ScreenSize { width: 744.0, height: 1133.0 }), (372, 372));
}

/// 第一次读元素树时挪一次判断前台 App 的点，之后不再设
#[tokio::test]
async fn the_first_tree_read_moves_the_detection_point_once() {
    let xml = r#"{"value":"<XCUIElementTypeApplication/>"}"#;
    let t = Script::new(vec![
        ok(r#"{"value":{"screenSize":{"width":430,"height":932}}}"#),
        ok(r#"{"sessionId":"A"}"#),
        ok(r#"{"value":"PORTRAIT"}"#),
        ok(r#"{"value":null}"#),
        ok(xml),
        ok(r#"{"value":"PORTRAIT"}"#),
        ok(xml),
    ]);
    let mut c = WdaClient::new(&t);
    c.tree().await.unwrap();
    c.tree().await.unwrap();
    let sent = t.sent();
    assert_eq!(sent[3], "POST /session/A/appium/settings", "{sent:?}");
    assert_eq!(t.body(3)["settings"], json!({"activeAppDetectionPoint": "215,215"}));
    assert_eq!(sent.len(), 7, "{sent:?}");
    assert!(sent[6].starts_with("GET /source?"));
}
