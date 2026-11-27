use super::ax::*;
use super::hid::*;
use super::unlock::*;
use super::*;

/// A field that can't be read (a password, a web page's) is done with once the paste prompt was answered and
/// had a moment, not looked at until the time is up; before the answer the paste hasn't gone in yet
#[test]
fn an_unreadable_field_is_done_once_the_prompt_was_answered() {
    let ms = Duration::from_millis;
    assert_eq!(paste_outcome(Pasted::Unreadable, None, false), None, "not asked yet");
    assert_eq!(paste_outcome(Pasted::Unreadable, Some(ms(300)), false), None, "just answered");
    assert_eq!(paste_outcome(Pasted::Unreadable, Some(PASTE_SAME_SETTLE), false), Some(Pasted::Unreadable));
    assert_eq!(paste_outcome(Pasted::Unreadable, None, true), Some(Pasted::Unreadable), "never asked: at the end");
    assert_eq!(paste_outcome(Pasted::Unchanged, None, true), Some(Pasted::Missing));
    assert_eq!(paste_outcome(Pasted::Unchanged, Some(PASTE_SAME_SETTLE), false), Some(Pasted::Unchanged));
    assert_eq!(paste_outcome(Pasted::Typed, None, false), Some(Pasted::Typed));
    assert_eq!(paste_outcome(Pasted::Missing, Some(PASTE_SAME_SETTLE), false), None, "keeps looking");
    assert_eq!(paste_outcome(Pasted::Missing, Some(PASTE_SAME_SETTLE), true), Some(Pasted::Missing));
}

/// Only a time-out in the tunnel itself drops it; the accessibility daemon's is restarted instead
#[test]
fn a_stuck_tunnel_is_dropped_not_the_accessibility_daemon() {
    use ictl_device::DeviceError;
    assert!(tunnel_timed_out::<()>(&Err(DeviceError::Timeout("connect".into()))));
    assert!(!tunnel_timed_out::<()>(&Err(DeviceError::AxUnanswered("request".into()))));
    assert!(!tunnel_timed_out::<()>(&Err(DeviceError::InvalidInput("x".into()))));
    assert!(!tunnel_timed_out(&Ok(())));
}

#[test]
fn only_a_recognisable_unlocked_springboard_can_confirm_absence() {
    let screen = |springboard_front| ictl_device::ax::AxScreen {
        app: Vec::new(),
        app_complete: true,
        overlays: Vec::new(),
        overlays_complete: true,
        app_pid: 1,
        springboard_front,
        at: Instant::now(),
    };
    let cover = |markers: &str, springboard_front| {
        let xml = format!(r#"<App>{markers}</App>"#);
        let src = elements::Source::from_xml(&xml);
        let tree = Tree::of(&src, ScreenSize { width: 390.0, height: 844.0 });
        native_cover(&src, &screen(springboard_front), &tree)
    };
    let icon = r#"<XCUIElementTypeIcon type="Icon" label="Clock" x="10" y="10" width="54" height="54"/>"#;
    // the home screen
    assert_eq!(cover(icon, true), Cover::None);
    // the lock screen by its marks, in front or not
    assert_eq!(cover(r#"<Other name="proud-lock"/>"#, true), Cover::Locked);
    assert_eq!(cover(r#"<Other name="proud-lock"/>"#, false), Cover::Locked);
    // notification center pulled over an app
    assert_eq!(cover(&format!(r#"{icon}<Other name="lockscreen-date-view"/>"#), false), Cover::Other);
    // the lock screen as read natively: no marks, SpringBoard in front, no icons
    let lock = r#"<XCUIElementTypeStaticText label="22:53" x="120" y="120" width="200" height="90"/>
<XCUIElementTypeButton label="手电筒" x="40" y="860" width="50" height="50"/>"#;
    assert_eq!(cover(lock, true), Cover::Other);
    // an app in front: its own screen, whatever is on it
    assert_eq!(cover(lock, false), Cover::None);
    // the sleep lock screen: another process in front, SpringBoard's flashlight and camera over it
    let sleep = r#"<XCUIElementTypeButton label="无闹钟" name="UIA.SleepLockScreen.Alarm.Button" x="100" y="300" width="200" height="50"/>
<XCUIElementTypeStaticText label="00:04" x="120" y="120" width="200" height="90"/>
<XCUIElementTypeButton label="手电筒" name="flashlight-orb-button" x="40" y="760" width="50" height="50"/>
<XCUIElementTypeButton label="相机" name="system-orb-button-com.apple.camera.deeplink.button" x="350" y="760" width="50" height="50"/>"#;
    assert_eq!(cover(sleep, false), Cover::Other);
    let orb =
        r#"<XCUIElementTypeButton label="手电筒" name="flashlight-orb-button" x="40" y="760" width="50" height="50"/>"#;
    assert_eq!(cover(orb, false), Cover::Other);
}

#[test]
fn replacing_text_requires_one_readable_focused_field() {
    let mut screen = ictl_device::ax::AxScreen {
        app: vec![
            ictl_ax::Element { xc_type: 49, value: "old".into(), visible: true, enabled: true, ..Default::default() },
            ictl_ax::Element {
                xc_type: 49,
                value: " 蓝牙 ".into(),
                focused: true,
                visible: true,
                enabled: true,
                ..Default::default()
            },
        ],
        app_complete: true,
        overlays: Vec::new(),
        overlays_complete: true,
        app_pid: 1,
        springboard_front: false,
        at: Instant::now(),
    };
    assert_eq!(focused_text_field(&screen).map(|f| f.value.as_str()), Some(" 蓝牙 "));
    let target = ictl_ax::ObjectRef { pid: 1, id: 2, is_app: false };
    screen.app[1].object = Some(target);
    screen.app[1].value = "wifi".into();
    screen.app[0].value = "wi".into();
    screen.app_complete = false;
    assert_eq!(replacement_seen(&screen, target, "wi"), Pasted::Missing);
    screen.app[1].value = "wi".into();
    assert_eq!(replacement_seen(&screen, target, "wi"), Pasted::Typed);
    assert_eq!(replacement_seen(&screen, ictl_ax::ObjectRef { id: 3, ..target }, "wi"), Pasted::Missing);
    // emptied: the field reads its placeholder as its value
    screen.app[1].placeholder = "搜索".into();
    screen.app[1].value = "搜索".into();
    assert_eq!(replacement_seen(&screen, target, ""), Pasted::Typed);
    screen.app[1].value = "搜索结果".into();
    assert_eq!(replacement_seen(&screen, target, ""), Pasted::Missing);
    screen.app[1].value = "wi".into();
    screen.app[1].placeholder = String::new();
    screen.app[0].focused = true;
    assert_eq!(focused_text_field(&screen).map(|f| f.value.as_str()), None);
    screen.app[0].focused = false;
    screen.app[1].value = "••••".into();
    assert_eq!(focused_text_field(&screen).map(|f| f.value.as_str()), Some("••••"));
    screen.app[1].xc_type = 50; // SecureTextField
    assert_eq!(focused_text_field(&screen).map(|f| f.value.as_str()), None);
}

/// The HID session isn't closed right after a touch (the phone takes the touch back): the rest of
/// `TOUCH_LANDS` is waited out first, and nothing when no touch was sent or it was long enough ago
#[test]
fn the_hid_session_stays_open_until_the_last_touch_has_landed() {
    let ms = Duration::from_millis;
    assert_eq!(touch_lands_in(Some(ms(0))), Some(TOUCH_LANDS));
    assert_eq!(touch_lands_in(Some(ms(100))), Some(TOUCH_LANDS - ms(100)));
    assert_eq!(touch_lands_in(Some(TOUCH_LANDS)), None);
    assert_eq!(touch_lands_in(Some(ms(5000))), None);
    assert_eq!(touch_lands_in(None), None);
}

fn info(version: &str) -> DeviceInfo {
    DeviceInfo {
        udid: "u".into(),
        name: "n".into(),
        product_type: "iPhone16,2".into(),
        product_version: version.into(),
    }
}

#[test]
fn ios_27_and_later_are_native_unless_wda_is_asked_for() {
    assert!(native_for(IosBackend::Auto, Some(&info("27.2.1"))));
    assert!(native_for(IosBackend::Native, Some(&info("28.0"))));
    assert!(!native_for(IosBackend::Wda, Some(&info("27.2.1"))));
    // no HID below 27
    assert!(!native_for(IosBackend::Native, Some(&info("26.7.3"))));
    assert!(!native_for(IosBackend::Auto, None));
}

#[test]
fn the_paste_prompt_is_answered_only_for_our_paste() {
    let xml = |alert: &str| {
        format!(
            r#"<XCUIElementTypeApplication type="XCUIElementTypeApplication" label="备忘录" x="0" y="0" width="430" height="932"><XCUIElementTypeAlert type="XCUIElementTypeAlert" label="{alert}" x="55" y="390" width="320" height="175"><XCUIElementTypeButton type="XCUIElementTypeButton" label="允许粘贴" x="71" y="501" width="140" height="49"/><XCUIElementTypeButton type="XCUIElementTypeButton" label="不允许粘贴" x="219" y="501" width="140" height="49"/></XCUIElementTypeAlert></XCUIElementTypeApplication>"#
        )
    };
    let screen = ScreenSize { width: 430.0, height: 932.0 };
    let ours = elements::parse_tree(&xml("“备忘录”想从“dtpasteboardd”粘贴"), screen);
    assert_eq!(paste_allow(&ours), Some((141.0, 526.0)));
    let other = elements::parse_tree(&xml("“备忘录”想从“微信”粘贴"), screen);
    assert_eq!(paste_allow(&other), None);
}

/// In a language whose button text isn't known, "Allow Paste" is the first of the prompt's two buttons: on the
/// left side by side, on top stacked; never a button of the app under it, and nothing with more buttons
#[test]
fn the_paste_prompt_is_answered_in_any_language() {
    let button = |label: &str, (x, y, w, h): (i32, i32, i32, i32)| {
        format!(
            r#"<XCUIElementTypeButton type="XCUIElementTypeButton" label="{label}" x="{x}" y="{y}" width="{w}" height="{h}"/>"#
        )
    };
    let xml = |app: &str, buttons: &str| {
        format!(
            r#"<XCUIElementTypeApplication type="XCUIElementTypeApplication" label="Notizen" x="0" y="0" width="430" height="932">{app}<XCUIElementTypeAlert type="XCUIElementTypeAlert" label="„Notizen“ möchte aus „dtpasteboardd“ einsetzen" x="55" y="390" width="320" height="175">{buttons}</XCUIElementTypeAlert></XCUIElementTypeApplication>"#
        )
    };
    let screen = ScreenSize { width: 430.0, height: 932.0 };
    let app = button("Fertig", (340, 60, 80, 40));
    let side = button("Einsetzen erlauben", (71, 501, 140, 49)) + &button("Nicht erlauben", (219, 501, 140, 49));
    assert_eq!(paste_allow(&elements::parse_tree(&xml(&app, &side), screen)), Some((141.0, 526.0)));
    let stacked = button("Einsetzen erlauben", (71, 470, 288, 44)) + &button("Nicht erlauben", (71, 514, 288, 44));
    assert_eq!(paste_allow(&elements::parse_tree(&xml(&app, &stacked), screen)), Some((215.0, 492.0)));
    let three = side.clone() + &button("Mehr", (71, 450, 288, 40));
    assert_eq!(paste_allow(&elements::parse_tree(&xml(&app, &three), screen)), None);
}

#[test]
fn a_paste_that_replaced_the_same_text_is_told_apart_from_one_that_missed() {
    let mut f = ictl_ax::Element {
        object: Some(ictl_ax::ObjectRef { id: 1, pid: 2, is_app: false }),
        value: "设置".into(),
        ..Default::default()
    };
    let original = f.object.unwrap();
    assert_eq!(append_seen(Some(&f), original, "", "设置"), Pasted::Typed);
    f.object.as_mut().unwrap().id += 1;
    assert_eq!(append_seen(Some(&f), original, "", "设置"), Pasted::Missing);
    assert_eq!(paste_seen(Some("设置"), "", "设置"), Pasted::Typed);
    assert_eq!(paste_seen(Some("设置设置"), "设置", "设置"), Pasted::Typed);
    assert_eq!(paste_seen(Some("设置"), "设置", "设置"), Pasted::Unchanged);
    assert_eq!(paste_seen(Some("时钟"), "时钟", "设置"), Pasted::Missing);
    assert_eq!(paste_seen(Some("11"), "1", "11"), Pasted::Missing);
    assert_eq!(paste_seen(None, "", "设置"), Pasted::Unreadable);
    assert_eq!(paste_seen(Some("北京"), "", "北京\n"), Pasted::Trimmed);
    assert_eq!(paste_seen(Some("北京"), "", " 北京 "), Pasted::Trimmed);
    // what Spotlight's single-line field makes of a pasted newline (iPhone 15 Pro Max, iOS 27.2.1): a space
    assert_eq!(paste_seen(Some("北京 "), "", "北京\n"), Pasted::Trimmed);
    assert_eq!(paste_seen(Some("北京 上海"), "", "北京\n上海"), Pasted::Trimmed);
    assert_eq!(paste_seen(Some("abc def "), "abc", " def\n"), Pasted::Trimmed);
    assert_eq!(paste_seen(Some("北京 "), "设置", "北京\n"), Pasted::Unreadable);
    assert_eq!(paste_seen(Some("北京"), "北京", "北京 "), Pasted::Unchanged);
    assert_eq!(paste_seen(Some(""), "", " "), Pasted::Missing);
}
