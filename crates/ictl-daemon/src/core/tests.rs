use super::dispatch::{Lane, Waiting, admit, check_request, lane, recheck_front, with_unlock_note};
use super::*;

/// What takes seconds without needing the device lock doesn't hold it: `doctor` checks this machine before
/// taking it, crash reports and uninstalling (lockdown services of their own) don't take it at all
#[test]
fn slow_checks_and_lockdown_tools_dont_hold_the_device_lock() {
    assert!(matches!(lane(&Request::Doctor), Lane::Doctor));
    assert!(matches!(lane(&Request::CrashReports { filter: String::new(), name: String::new() }), Lane::Apart));
    assert!(matches!(lane(&Request::UninstallApp { app: "x".into() }), Lane::Apart));
    assert!(matches!(
        lane(&Request::AppLogs {
            process: String::new(),
            grep: String::new(),
            since_secs: 10,
            limit: 10,
            errors_only: false
        }),
        Lane::Device
    ));
}

/// What a process is, is known for a while, then asked again: by then its number may be another process's
#[test]
fn what_a_process_is_is_asked_again_after_a_while() {
    let t0 = Instant::now();
    let mut c = PidCache::default();
    c.insert_at(42, "com.apple.Preferences".to_owned(), t0);
    assert_eq!(c.get_at(42, t0 + Duration::from_secs(60)).map(String::as_str), Some("com.apple.Preferences"));
    assert_eq!(c.get_at(42, t0 + PidCache::<String>::FOR), None, "too old to go by");
    c.remove(42);
    assert_eq!(c.get_at(42, t0), None);
    // full: the old ones go first, and only when all are recent does it start again
    for p in 0..PidCache::<String>::MAX as i64 {
        c.insert_at(p, String::new(), if p == 0 { t0 } else { t0 + PidCache::<String>::FOR });
    }
    c.insert_at(1000, String::new(), t0 + PidCache::<String>::FOR + Duration::from_secs(1));
    assert_eq!(c.entries.len(), PidCache::<String>::MAX, "the stale one made room");
    c.insert_at(5, String::new(), t0 + PidCache::<String>::FOR + Duration::from_secs(2));
    assert_eq!(c.entries.len(), PidCache::<String>::MAX, "one renewed: nothing cleared");
    c.insert_at(1001, String::new(), t0 + PidCache::<String>::FOR + Duration::from_secs(2));
    assert_eq!(c.entries.len(), 1, "all recent: cleared");
}

#[test]
fn raw_scroll_collect_validates_scope_and_bounds() {
    let mut scan = crate::protocol::ScrollCollect {
        down: true,
        max_scrolls: 1,
        filter: String::new(),
        until: String::new(),
        limit: 1,
        x: None,
    };
    let req = Request::ScrollCollect(scan.clone());
    assert!(check_request(&req).is_ok());
    assert_eq!(crate::tasks::scope_check(&req), Some(crate::tasks::Check::Foreground));
    assert!(!crate::tasks::observes(&req), "this cannot clear a takeover's observation requirement");
    let mut wire = serde_json::to_value(&req).unwrap();
    assert_eq!(serde_json::from_value::<Request>(wire.clone()).unwrap(), req);
    // a key this daemon doesn't know (a newer client's) is ignored, as for every other request
    wire["added_by_a_newer_client"] = serde_json::json!(true);
    assert_eq!(serde_json::from_value::<Request>(wire.clone()).unwrap(), req);
    // the scan's own fields are still checked: a wrong type is an error
    wire["max_scrolls"] = serde_json::json!("sideways");
    assert!(serde_json::from_value::<Request>(wire).is_err());
    for (max, limit) in [(0, 1), (16, 1), (1, 0), (1, 151)] {
        scan.max_scrolls = max;
        scan.limit = limit;
        assert!(check_request(&Request::ScrollCollect(scan.clone())).is_err());
    }
    (scan.max_scrolls, scan.limit) = (1, 1);
    for (x, ok) in [(Some(160.0), true), (Some(0.0), true), (Some(-1.0), false), (Some(f64::NAN), false)] {
        scan.x = x;
        assert_eq!(check_request(&Request::ScrollCollect(scan.clone())).is_ok(), ok, "{x:?}");
    }
}

/// Each device keeps its own background WDA: switching to another device and back attaches to it again, and
/// `service stop` sees all of them. The last one is still written the old way for older versions
#[test]
fn each_device_keeps_its_background_wda() {
    let mut p = Persisted::default();
    assert!(p.record_detached("IPHONE", 10));
    assert!(p.record_detached("IPAD", 20));
    assert!(!p.record_detached("ipad", 20), "the same again changes nothing");
    assert_eq!(p.detached_for("IPHONE"), Some(10), "switching to the iPad kept the iPhone's");
    assert_eq!(p.detached_for("iPad"), Some(20));
    assert_eq!(p.detached_wda, Some(DetachedWda { udid: "IPAD".into(), pid: 20 }), "the last one, for old versions");
    assert_eq!(p.detached_all(), vec![("IPAD".into(), 20), ("IPHONE".into(), 10)]);

    assert!(!p.forget_detached("IPHONE", Some(11)), "another pid: a newer one, kept");
    assert!(p.forget_detached("IPHONE", Some(10)));
    assert_eq!(p.detached_for("IPHONE"), None);
    assert!(p.forget_detached("ipad", None));
    assert_eq!((p.detached_all(), p.detached_wda), (vec![], None));
}

/// A background WDA launched before WDA was confined to the phone's loopback listens on every interface: it isn't
/// attached to again (launching anew ends it). Once this version records it, it is
#[test]
fn only_a_loopback_only_background_wda_is_attached_to_again() {
    let mut p: Persisted =
        serde_json::from_str(r#"{"detached_wdas":{"IPHONE":7},"detached_wda":{"udid":"IPHONE","pid":7}}"#).unwrap();
    assert_eq!(p.detached_for("IPHONE"), Some(7), "still known: ended when the phone goes native");
    assert_eq!(p.adoptable_for("IPHONE"), None, "an older version's, listening on every interface");

    assert!(p.record_detached("iphone", 9), "launched anew");
    assert_eq!(p.adoptable_for("IPHONE"), Some(9));
    assert!(!p.record_detached("IPHONE", 9), "attached to again: nothing changed");
    let json = serde_json::to_value(&p).unwrap();
    assert_eq!(json["loopback_wdas"], serde_json::json!({"IPHONE": 9}));
    assert_eq!(json["detached_wdas"], serde_json::json!({"IPHONE": 9}), "older versions still find it");

    // an older version launched another (it doesn't know loopback_wdas): not attached to by this one
    p.detached_wdas.insert("IPHONE".into(), 12);
    assert_eq!(p.adoptable_for("IPHONE"), None);

    assert!(p.forget_detached("IPHONE", None));
    assert!(p.loopback_wdas.is_empty());
    let empty = serde_json::to_value(&p).unwrap();
    assert!(empty.get("loopback_wdas").is_none(), "not written when empty");
}

/// A state.json from an older version has only the one record: it is still found, attached to and forgotten
#[test]
fn an_older_versions_background_wda_is_still_known() {
    let p: Persisted = serde_json::from_str(r#"{"detached_wda":{"udid":"IPHONE","pid":7}}"#).unwrap();
    assert_eq!(p.detached_for("iphone"), Some(7));
    assert_eq!(p.detached_for("IPAD"), None);
    assert_eq!(p.detached_all(), vec![("IPHONE".into(), 7)]);

    let mut q = p.clone();
    assert!(q.forget_detached("IPHONE", Some(7)));
    assert_eq!(q.detached_all(), vec![]);

    let mut r = p;
    assert!(r.record_detached("IPAD", 8));
    assert_eq!(r.detached_all(), vec![("IPAD".into(), 8), ("IPHONE".into(), 7)], "the old record is kept");
    let json = serde_json::to_value(&r).unwrap();
    assert_eq!(json["detached_wdas"], serde_json::json!({"IPAD": 8, "IPHONE": 7}));
    assert_eq!(json["detached_wda"], serde_json::json!({"udid": "IPAD", "pid": 8}));
}

/// 重签结果存成时间和成败，按看的人的语言显示；旧字段照旧写中文整句，旧版本的 state.json 照样读
#[test]
fn resign_result_is_shown_in_the_reader_language() {
    use ictl_l10n::{Lang, with_lang};
    let mut p = Persisted::default();
    p.record_resign(1_800_000_000, None);
    assert_eq!(p.last_resign.as_deref(), Some("2027-01-15 08:00:00 成功"), "for old versions: UTC, as they wrote it");
    // shown in local time with its offset (this machine's zone, whatever it is)
    let when = crate::eventlog::format_local(1_800_000_000);
    assert_eq!(with_lang(Lang::En, || p.last_resign_text()), Some(format!("{when} succeeded")));
    assert_eq!(with_lang(Lang::Zh, || p.last_resign_text()), Some(format!("{when} 成功")));
    p.record_resign(1_800_000_000, Some("boom".into()));
    assert_eq!(with_lang(Lang::En, || p.last_resign_text()), Some(format!("{when} failed: boom")));

    // 旧版本写的：只有中文整句，原样显示
    let old: Persisted = serde_json::from_str(r#"{"last_resign":"2026-11-07 01:11:54 成功"}"#).unwrap();
    assert_eq!(with_lang(Lang::En, || old.last_resign_text()).as_deref(), Some("2026-11-07 01:11:54 成功"));
    // 新版本写的，旧版本读：多出来的字段不影响（这里用只认旧字段的结构模拟）
    #[derive(serde::Deserialize)]
    struct OldPersisted {
        last_resign: Option<String>,
    }
    let json = serde_json::to_string(&p).unwrap();
    let o: OldPersisted = serde_json::from_str(&json).unwrap();
    assert_eq!(o.last_resign.as_deref(), Some("2027-01-15 08:00:00 失败：boom"));
}

pub(crate) fn test_daemon(tag: &str) -> (Arc<Daemon>, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("ictl-core-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    (Daemon::new(Config::from_env(&dir)), dir)
}

/// Without WDA, the route follows the device and the backend setting: iOS 27 natively by default, the HID fallback
/// when set to use WDA, nothing below iOS 27; an unknown version is given the HID fallback, as before
#[tokio::test]
async fn the_route_without_wda_follows_the_ios_version_and_backend() {
    let kind = |r: Route<'_>| match r {
        Route::Wda(_) => "wda",
        Route::Native => "native",
        Route::Hid => "hid",
        Route::Unavailable => "unavailable",
    };
    let info = |v: &str| ictl_device::DeviceInfo {
        udid: "u".into(),
        name: "n".into(),
        product_type: "iPhone16,2".into(),
        product_version: v.into(),
    };
    for (backend, version, want) in [
        (crate::config::IosBackend::Auto, Some("27.2.1"), "native"),
        (crate::config::IosBackend::Native, Some("27.0"), "native"),
        (crate::config::IosBackend::Wda, Some("27.2.1"), "hid"),
        (crate::config::IosBackend::Auto, Some("26.7.3"), "unavailable"),
        (crate::config::IosBackend::Wda, None, "hid"),
    ] {
        let dir = std::env::temp_dir().join(format!("ictl-core-route-{}-{version:?}-{backend:?}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = Config::from_env(&dir);
        cfg.ios_backend = backend;
        let d = Daemon::new(cfg);
        let mut g = d.inner.lock().await;
        g.info = version.map(info);
        assert_eq!(kind(d.route(&mut g)), want, "{backend:?} {version:?}");
        drop(g);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

/// A background check (the page's status and alert polls) gives way: it doesn't wait for the device lock, nor
/// start while an action waits for it; without the flag the same request waits as before
#[tokio::test]
async fn a_background_check_gives_way_to_actions() {
    let (d, dir) = test_daemon("background");
    let busy = |r: &Response| matches!(r, Response::Text { data: Some(crate::protocol::Data::Busy), .. });
    let bg = || CallCtx { background: true, ..Default::default() };
    // the lock taken (an action runs): at once, busy
    let held = d.inner.lock().await;
    let t = Instant::now();
    let r = d.handle(Request::AlertCheck, Instant::now(), bg()).await;
    assert!(busy(&r) && t.elapsed() < Duration::from_millis(100), "{r:?}");
    let plain = d.handle(Request::Status, Instant::now(), CallCtx::default());
    tokio::pin!(plain);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut plain).await.is_err(),
        "a check without the flag waits for the lock"
    );
    drop(held);
    assert!(!busy(&plain.await));
    // an action waiting for the lock: not started
    let waiting = Waiting::new(&d);
    assert!(busy(&d.handle(Request::Status, Instant::now(), bg()).await));
    drop(waiting);
    // nothing else wants the phone: it runs
    let r = d.handle(Request::Status, Instant::now(), bg()).await;
    assert!(matches!(r, Response::Text { data: Some(crate::protocol::Data::Status(_)), .. }), "{r:?}");
    // right after an action: not yet (the alert check waits longer than the status check)
    *d.last_action.lock().unwrap() = Some(Instant::now() - Duration::from_secs(2));
    assert!(d.acting_now(Duration::from_secs(3)) && !d.acting_now(Duration::from_secs(1)), "slow requests wait too");
    assert!(!busy(&d.handle(Request::Status, Instant::now(), bg()).await), "status: 1 s after");
    assert!(busy(&d.handle(Request::AlertCheck, Instant::now(), bg()).await), "the alert check: 4 s after");
    assert!(!busy(&d.handle(Request::AlertCheck, Instant::now(), CallCtx::default()).await), "unless asked outright");
    std::fs::remove_dir_all(dir).unwrap();
}

/// An action counts as waiting for the phone (background checks give way) only while it waits for the lock, not
/// while its task is paused: the page's checks went on giving way for as long as the pause lasted
#[tokio::test]
async fn waiting_for_a_paused_task_doesnt_hold_off_the_checks() {
    use std::sync::atomic::Ordering::SeqCst;
    let (d, dir) = test_daemon("waiting-paused");
    let t = d.tasks.open("x", "test", Some("phone"), Vec::new(), None).unwrap();
    let tap = Request::Tap { x: 1.0, y: 1.0, count: 1 };
    let admission = crate::tasks::Admission::Actor(t.clone());
    let (mut queued, mut gone) = (Instant::now(), None);
    {
        let held = d.inner.lock().await;
        let locked = d.lock_admitted(&d.inner, &tap, &admission, &mut queued, &mut gone, true);
        tokio::pin!(locked);
        assert!(tokio::time::timeout(Duration::from_millis(30), &mut locked).await.is_err());
        assert_eq!(d.waiting.load(SeqCst), 1, "waits for the lock: counted");
        t.control_as_user("pause").unwrap();
        drop(held);
        assert!(tokio::time::timeout(Duration::from_millis(30), &mut locked).await.is_err(), "the task is paused");
        assert_eq!(d.waiting.load(SeqCst), 0, "waits for the task: not counted");
        t.control_as_user("resume").unwrap();
        assert!(locked.await.is_ok());
        assert_eq!(d.waiting.load(SeqCst), 0);
    }
    // the Web lane's lock isn't the device's: never counted
    let held = d.web.lock().await;
    let web = d.lock_admitted(&d.web, &tap, &admission, &mut queued, &mut gone, false);
    tokio::pin!(web);
    assert!(tokio::time::timeout(Duration::from_millis(30), &mut web).await.is_err());
    assert_eq!(d.waiting.load(SeqCst), 0);
    drop(held);
    assert!(web.await.is_ok());
    std::fs::remove_dir_all(dir).unwrap();
}

/// A scoped task's action on the app in front is checked again with the lock held; it can't act blind. Unscoped
/// tasks, requests that don't act on the front app and scripts in pages (checked by the page's app) aren't
#[tokio::test]
async fn a_scoped_action_is_checked_again_with_the_lock_held() {
    let (d, dir) = test_daemon("scope-recheck");
    let scoped = d.tasks.open("x", "test", Some("phone"), vec!["com.apple.Preferences".into()], None).unwrap();
    let tap = Request::Tap { x: 1.0, y: 1.0, count: 1 };
    let mut g = d.inner.lock().await;
    let actor = crate::tasks::Admission::Actor(scoped.clone());
    let r = d.scope_recheck(&mut g, &tap, &actor, false).await;
    assert!(
        matches!(&r, Err(Response::Error { message, .. }) if message.contains("看不出前台") || message.contains("Can't tell")),
        "{r:?}"
    );
    let home = Request::PressButton { button: "home".into() };
    assert!(d.scope_recheck(&mut g, &home, &actor, false).await.is_ok(), "the way back is never held");
    let eval = Request::WebEval { target: String::new(), expression: "1".into(), timeout_ms: 1000 };
    assert!(d.scope_recheck(&mut g, &eval, &actor, false).await.is_ok(), "the page's app is checked in the Web lane");
    assert!(d.scope_recheck(&mut g, &tap, &crate::tasks::Admission::Free, false).await.is_ok());
    scoped.end(crate::tasks::End::Cancelled, "");
    let open = d.tasks.open("y", "test", Some("phone"), Vec::new(), None).unwrap();
    let r = d.scope_recheck(&mut g, &tap, &crate::tasks::Admission::Actor(open), false).await;
    assert!(r.is_ok(), "no scope: nothing to look up");
    drop(g);
    std::fs::remove_dir_all(dir).unwrap();
}

/// The app in front, read again with the lock held: within the scope it goes on; another one is refused and
/// named; an unknown one only when the user just allowed acting on an unknown screen (it never joins the scope)
#[test]
fn the_app_in_front_is_judged_again_without_asking() {
    let ts = crate::tasks::Tasks::load(None);
    let t = ts.open("x", "test", Some("phone"), vec!["com.apple.Preferences".into()], None).unwrap();
    let refused = |r: Result<(), Response>| match r {
        Err(Response::Error { message, .. }) => message,
        other => panic!("{other:?}"),
    };
    assert!(recheck_front(&t, "com.apple.Preferences", false).is_ok());
    assert!(recheck_front(&t, "com.apple.springboard", false).is_ok(), "the home screen is always in scope");
    let m = refused(recheck_front(&t, "com.tencent.xin", false));
    assert!(m.contains("com.tencent.xin"), "{m}");
    assert!(refused(recheck_front(&t, "com.tencent.xin", true)).contains("com.tencent.xin"), "allowed was the unknown");
    assert!(recheck_front(&t, "", true).is_ok(), "the user just allowed this on an unknown screen");
    let m = refused(recheck_front(&t, "", false));
    assert!(m.contains("看不出前台") || m.contains("Can't tell"), "{m}");
    assert!(!m.contains("的 ，"), "no empty name in the text: {m}");
}

/// Requests that can't run are refused before they take the device lock and drop the cached tree
#[test]
fn hopeless_requests_are_refused_up_front() {
    let bad = [
        Request::Tap { x: 1.0, y: 1.0, count: 0 },
        Request::Tap { x: 1.0, y: 1.0, count: crate::protocol::MAX_TAP_COUNT + 1 },
        Request::Tap { x: f64::NAN, y: 1.0, count: 1 },
        Request::LongPress { x: 1.0, y: f64::INFINITY, seconds: 1.0 },
        Request::Swipe { from: (1.0, f64::NAN), to: (1.0, 2.0), seconds: 0.3, hold: 0.0 },
        Request::TapSequence { points: vec![] },
        Request::TapSequence { points: vec![(1.0, 1.0); crate::protocol::MAX_SEQUENCE_POINTS + 1] },
        Request::TapSequence { points: vec![(1.0, f64::NAN)] },
        Request::PressButton { button: "power".into() },
    ];
    for r in bad {
        assert!(check_request(&r).is_err(), "{r:?}");
    }
    let good = [
        Request::Tap { x: 1.0, y: 1.0, count: crate::protocol::MAX_TAP_COUNT },
        Request::TapSequence { points: vec![(1.0, 1.0); crate::protocol::MAX_SEQUENCE_POINTS] },
        Request::PressButton { button: "home".into() },
        Request::PressButton { button: "volume_up".into() },
    ];
    for r in good {
        assert!(check_request(&r).is_ok(), "{r:?}");
    }
}

#[test]
fn a_tap_by_text_scrolls_within_bounds() {
    let tap = |max| Request::TapElement {
        text: "x".into(),
        index: 0,
        max_age_ms: 0,
        scroll: Some(crate::protocol::ScrollSearch { down: true, max }),
        selector: crate::protocol::ElementSelector::default(),
    };
    for (max, ok) in
        [(0, false), (1, true), (crate::protocol::MAX_SEARCH_SCROLLS, true), (16, false), (u32::MAX, false)]
    {
        assert_eq!(check_request(&tap(max)).is_ok(), ok, "{max}");
    }
    let plain = Request::TapElement {
        text: "x".into(),
        index: 0,
        max_age_ms: 0,
        scroll: None,
        selector: crate::protocol::ElementSelector::default(),
    };
    assert!(check_request(&plain).is_ok());
}

/// 调用方已经断开：还在排队等设备锁的请求不执行
#[tokio::test]
async fn queued_request_is_dropped_when_caller_is_gone() {
    let (d, dir) = test_daemon("gone");
    let busy = d.inner.lock().await;
    let (tx, rx) = tokio::sync::oneshot::channel();
    tx.send(()).unwrap();
    let r = d
        .handle(Request::Status, Instant::now(), CallCtx { gone: Some(rx), expect_udid: None, ..Default::default() })
        .await;
    drop(busy);
    assert!(matches!(&r, Response::Error { message, .. } if message.contains("已取消")), "{r:?}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn display_identity_expires_on_rotation_resize_and_reconnect() {
    let size = ScreenSize { width: 393.0, height: 852.0 };
    let portrait = Some((size, Rotation::Portrait));
    let mut display = None;
    assert!(DisplayGeometry::update(&mut display, portrait));
    let first = display.clone().unwrap().id;
    assert!(!DisplayGeometry::update(&mut display, portrait));
    assert_eq!(display.as_ref().unwrap().id, first);
    assert!(DisplayGeometry::update(&mut display, Some((size, Rotation::UpsideDown))));
    assert_ne!(display.as_ref().unwrap().id, first);
    let zoom = ScreenSize { width: 375.0, height: 812.0 };
    assert!(DisplayGeometry::update(&mut display, Some((zoom, Rotation::UpsideDown))));
    assert!(DisplayGeometry::update(&mut display, None));
    assert!(!DisplayGeometry::update(&mut display, None));
    assert!(DisplayGeometry::update(&mut display, portrait));
    assert_ne!(display.as_ref().unwrap().id, first);
}

#[test]
fn a_down_dropped_in_the_queue_cannot_be_replayed() {
    let req = Request::Touch { phase: crate::protocol::TouchPhase::Down, x: 1., y: 2. };
    let ctx = CallCtx { expect_udid: Some("phone".into()), expect_view: Some("portrait".into()), ..Default::default() };
    let expired = Instant::now() - STALE - Duration::from_secs(1);
    assert!(matches!(admit(&req, expired, &ctx, Some("phone")), Err(Response::Error { retry_as_gesture: false, .. })));
}

#[tokio::test]
async fn coordinate_from_a_previous_view_is_rejected_before_dispatch() {
    let (d, dir) = test_daemon("previous-view");
    let ctx = CallCtx { expect_view: Some("old-view".into()), ..Default::default() };
    let r = d.handle(Request::Tap { x: 3.0, y: 4.0, count: 1 }, Instant::now(), ctx).await;
    assert!(matches!(r, Response::Error { message, retry_as_gesture: false, .. } if message.contains("画面")));
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn transient_status_miss_keeps_the_view_and_finger_until_loss_is_confirmed() {
    let (d, dir) = test_daemon("status-miss-display");
    let mut g = d.inner.lock().await;
    let size = ScreenSize { width: 393., height: 852. };
    DisplayGeometry::update(&mut g.dev.display, Some((size, Rotation::Portrait)));
    let id = g.dev.display.as_ref().unwrap().id.clone();
    g.dev.touch_rotation = Some(Rotation::Portrait);
    for _ in 1..Misses::LIMIT {
        assert!(!d.confirm_wda_loss(&mut g, true).await);
        assert_eq!(g.dev.display.as_ref().unwrap().id, id);
        assert_eq!(g.dev.touch_rotation, Some(Rotation::Portrait));
    }
    assert!(d.confirm_wda_loss(&mut g, true).await);
    assert!(g.dev.display.is_none());
    assert!(g.dev.touch_rotation.is_none());
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn drag_continuations_only_validate_the_existing_view() {
    let (d, dir) = test_daemon("display-move-up");
    let mut g = d.inner.lock().await;
    DisplayGeometry::update(&mut g.dev.display, Some((ScreenSize { width: 393., height: 852. }, Rotation::Portrait)));
    let id = g.dev.display.as_ref().unwrap().id.clone();
    // No WDA in this fixture: resampling would invalidate the geometry before the HID path.
    for phase in [crate::protocol::TouchPhase::Move, crate::protocol::TouchPhase::Up] {
        g.dev.touch_rotation = Some(Rotation::Portrait);
        let _ = d.touch(&mut g, phase, 20., 50., Some(&id)).await;
        assert_eq!(g.dev.display.as_ref().unwrap().id, id);
        let stale = d.touch(&mut g, phase, 20., 50., Some("old")).await.unwrap_err();
        assert!(
            matches!(stale, Response::Error { message, retry_as_gesture: false, .. } if message.contains("画面") || message.contains("display changed"))
        );
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_known_view_losing_its_measurement_cannot_start_a_finger() {
    let (d, dir) = test_daemon("lost-display");
    let mut g = d.inner.lock().await;
    let size = ScreenSize { width: 393.0, height: 852.0 };
    DisplayGeometry::update(&mut g.dev.display, Some((size, Rotation::Portrait)));
    let ctx = CallCtx { expect_view: Some(g.dev.display.as_ref().unwrap().id.clone()), ..Default::default() };
    drop(g);
    let req = Request::Touch { phase: crate::protocol::TouchPhase::Down, x: 20.0, y: 50.0 };
    let response = d.handle(req, Instant::now(), ctx).await;
    assert!(matches!(response, Response::Error { message, retry_as_gesture: false, .. } if message.contains("画面")));
    let g = d.inner.lock().await;
    assert!(g.dev.display.is_none());
    assert!(g.dev.touch_rotation.is_none());
    std::fs::remove_dir_all(dir).unwrap();
}

/// 批量执行要求的设备不是正在控制的那台（例如拔线后改控了另一台）：不执行
#[tokio::test]
async fn request_for_another_device_is_refused() {
    let (d, dir) = test_daemon("expect");
    let ctx = CallCtx { gone: None, expect_udid: Some("00008130-AAAA".into()), ..Default::default() };
    let r = d.handle(Request::Status, Instant::now(), ctx).await;
    assert!(matches!(&r, Response::Error { message, .. } if message.contains("设备变了")), "{r:?}");
    std::fs::remove_dir_all(dir).unwrap();
}

/// 手机上的描述文件：CMS 外壳里包着 XML plist（同 ictl-sign 的解析方式）
fn profile(app_id: &str, devices: &[&str], expires: &str) -> Vec<u8> {
    let devs: String = devices.iter().map(|d| format!("<string>{d}</string>")).collect();
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict>
<key>Name</key><string>iOS Team Provisioning Profile</string>
<key>TeamIdentifier</key><array><string>TEAM1</string></array>
<key>ExpirationDate</key><date>{expires}</date>
<key>ProvisionedDevices</key><array>{devs}</array>
<key>Entitlements</key><dict><key>application-identifier</key><string>{app_id}</string></dict>
</dict></plist>"#
    );
    [&[0x30u8, 0x82][..], xml.as_bytes(), &[0x00, 0xa0]].concat()
}

#[test]
fn wda_down_when_no_device_does_not_promise_recovery() {
    assert!(wda_down(&DeviceState::Ready, None).starts_with("WDA 不可用(刚掉线)"));
    let m = wda_down(&DeviceState::Absent, None);
    assert!(m.contains("手机未连接") && !m.contains("1~2 分钟") && !m.contains("可照常用"), "{m}");
}

#[test]
fn wda_down_mentions_hid_fallback_only_from_ios_27() {
    let info = |v: &str| DeviceInfo { product_version: v.into(), ..Default::default() };
    let st = DeviceState::WdaStarting { last_error: None };
    assert!(wda_down(&st, Some(&info("27.0.1"))).contains("可照常用"));
    assert!(wda_down(&st, None).contains("可照常用"));
    let old = wda_down(&st, Some(&info("26.6.1")));
    assert!(old.contains("没有 HID 后备路线") && !old.contains("可照常用"), "{old}");
}

const WDA: &str = "com.facebook.WebDriverAgentRunner.xctrunner.TEAM1";

/// 和上面 XML 里的日期对应的时刻
fn at(s: &str) -> SystemTime {
    let secs = match s {
        "2026-11-10T15:35:00Z" => 1794324900,
        "2026-11-12T00:00:00Z" => 1794441600,
        _ => unreachable!(),
    };
    UNIX_EPOCH + Duration::from_secs(secs)
}

/// 真机情况：iPhone 上装的是 11-03 签的（11-10 到期），之后给 iPad 签的那份（11-13）只装到了 iPad 上
#[test]
fn wda_profile_is_the_one_on_this_phone() {
    let iphone = "00008130-0019295E012B803A";
    let ps = vec![
        profile(&format!("TEAM1.{WDA}"), &[iphone], "2026-11-10T15:35:00Z"),
        profile("TEAM1.com.example.other", &[iphone], "2027-01-01T00:00:00Z"),
        profile(&format!("TEAM2.{WDA}"), &[iphone], "2027-01-01T00:00:00Z"),
        profile(&format!("TEAM1.{WDA}"), &["00008020-000925043499002E"], "2026-11-13T12:26:31Z"),
        b"not a profile".to_vec(),
    ];
    assert_eq!(wda_profile_expiry(&ps, Some("TEAM1"), WDA, iphone), Some(at("2026-11-10T15:35:00Z")));
    // UDID 大小写不敏感；装过两次取晚的
    let mut twice = ps.clone();
    twice.push(profile(&format!("TEAM1.{WDA}"), &[&iphone.to_lowercase()], "2026-11-12T00:00:00Z"));
    assert_eq!(wda_profile_expiry(&twice, Some("TEAM1"), WDA, iphone), Some(at("2026-11-12T00:00:00Z")));
    // 没装过：没有包含这台手机的
    assert_eq!(wda_profile_expiry(&ps, Some("TEAM1"), WDA, "00008101-AAAA"), None);
    // 不知道团队 ID 时只看 Bundle ID
    assert!(wda_profile_expiry(&ps, None, WDA, iphone).is_some());
}

#[test]
fn device_profile_left_falls_back_to_signing_record_only_when_unknown() {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let p = Persisted { profile_expires: Some(now + 6 * 86400), ..Default::default() };
    let days = |d: Option<Duration>| d.map(|d| (d.as_secs() + 3600) / 86400);
    assert_eq!(days(DeviceProfile::Expires(now + 4 * 86400).left(&p)), Some(4));
    assert_eq!(days(DeviceProfile::Unknown("x".into()).left(&p)), Some(6));
    assert_eq!(days(DeviceProfile::NotChecked.left(&p)), Some(6));
    assert_eq!(DeviceProfile::Missing.left(&p), None);
    assert_eq!(DeviceProfile::Expires(now - 10).left(&p), Some(Duration::ZERO));
}

#[test]
fn gesture_seconds_are_bounded() {
    let swipe = |s| Request::Swipe { from: (0.0, 0.0), to: (1.0, 1.0), seconds: s, hold: 0.0 };
    assert!(check_request(&swipe(0.3)).is_ok());
    // a held drag counts the hold too, and the hold can't be negative
    let held = |s, h| Request::Swipe { from: (0.0, 0.0), to: (1.0, 1.0), seconds: s, hold: h };
    assert!(check_request(&held(0.7, 0.6)).is_ok());
    assert!(check_request(&held(5.0, 5.0)).is_ok());
    let too_long = check_request(&held(5.0, 5.5)).unwrap_err();
    assert!(too_long.contains("hold"), "{too_long}");
    // a negative seconds isn't made up for by the hold
    assert!(check_request(&held(-0.5, 1.0)).is_err());
    assert!(check_request(&held(0.3, 20.0)).is_err());
    assert!(check_request(&held(0.7, -0.1)).is_err());
    assert!(check_request(&held(0.7, f64::NAN)).is_err());
    assert!(check_request(&swipe(1e6)).is_err());
    assert!(check_request(&swipe(-1.0)).is_err());
    assert!(check_request(&swipe(f64::NAN)).is_err());
    assert!(check_request(&Request::LongPress { x: 0.0, y: 0.0, seconds: 10.0 }).is_ok());
}

#[test]
fn corrupt_state_file_loads_as_default() {
    let p = std::env::temp_dir().join(format!("ictl-state-{}.json", std::process::id()));
    std::fs::write(&p, b"{not json").unwrap();
    assert!(Persisted::load(&p).wda_bundle.is_none());
    std::fs::remove_file(&p).unwrap();
}

#[test]
fn an_unlock_first_is_said_on_the_answers_first_line() {
    let took = Some(Duration::from_millis(14_100));
    // an action with `then=elements`: the note goes on the action's line, the screen after it stays below
    let r = with_unlock_note(
        Response::text_with("已按 home\n--- 操作后的界面 ---\nButton \"设置\"", Default::default()),
        took,
    );
    let Response::Text { text, outcome, .. } = &r else { panic!("{r:?}") };
    let lines: Vec<_> = text.lines().collect();
    assert!(lines[0].starts_with("已按 home") && lines[0].contains("14.1"), "{text}");
    assert_eq!(lines[1..], ["--- 操作后的界面 ---", "Button \"设置\""]);
    assert!(outcome.is_some());
    // a screenshot's caption, and an action that failed after the unlock
    let r =
        with_unlock_note(Response::Image { jpeg_base64: "x".into(), caption: "430x932".into(), geometry: None }, took);
    assert!(
        matches!(&r, Response::Image { caption, .. } if caption.starts_with("430x932") && caption.contains("14.1"))
    );
    let r = with_unlock_note(Response::error("没找到"), took);
    assert!(matches!(&r, Response::Error { message, .. } if message.starts_with("没找到") && message.contains("14.1")));
    // no unlock: the answer as it was
    let plain = Response::text_with("已按 home", Default::default());
    assert_eq!(with_unlock_note(plain.clone(), None), plain);
}
