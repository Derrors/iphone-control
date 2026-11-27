use std::sync::Mutex;

use super::super::*;
use super::search::*;
use super::text::*;

/// Settings' list with 「隐私与安全」 at `y` (its center), between two other rows
fn settings_at(y: f64) -> Tree {
    let row = |label: &str, y: f64| {
        format!(
            r#"<XCUIElementTypeButton type="XCUIElementTypeButton" name="{label}" label="{label}" x="20" y="{}" width="400" height="51"/>"#,
            y - 25.5
        )
    };
    let rows = [row("SOS紧急联络", y - 52.0), row("隐私与安全", y), row("钱包与Apple Pay", y + 86.0)].concat();
    let xml = format!(
        r#"<XCUIElementTypeApplication type="XCUIElementTypeApplication" name="设置" label="设置" x="0" y="0" width="440" height="956"><XCUIElementTypeTable type="XCUIElementTypeTable" x="0" y="0" width="440" height="956">{rows}</XCUIElementTypeTable></XCUIElementTypeApplication>"#
    );
    elements::parse_tree(&xml, ScreenSize { width: 440.0, height: 956.0 })
}

/// What [`target_at_rest`] read (how many times) and where it left the target, the reads coming from `ys` (None:
/// a failed read)
async fn rest_from(first: f64, ys: &[Option<f64>]) -> (usize, Option<i64>) {
    let mut reads = ys.iter().copied();
    let mut n = 0;
    let pace = (Duration::from_millis(1), Duration::from_millis(200));
    let tree = target_at_rest(settings_at(first), ("隐私与安全", 0), pace, async || {
        n += 1;
        reads.next().flatten().map(settings_at)
    })
    .await;
    (n, target_place(&tree, "隐私与安全", 0).map(|(c, _)| c.1))
}

/// Found by scrolling, the row is tapped only once it is where it was a read before: a list still creeping by a
/// point or two counts as stopped for the scroll search, and a tap on it only stops it (Settings, iOS 27.2.1)
#[tokio::test]
async fn a_target_found_by_scrolling_is_tapped_at_rest() {
    // creeping 846 → 831, then still: tapped where it stopped
    let creeping = [846.0, 840.0, 836.0, 833.0, 831.0, 831.0].map(Some);
    assert_eq!(rest_from(850.0, &creeping).await, (6, Some(831)));
    // already at rest: one more read
    assert_eq!(rest_from(831.0, &[Some(831.0)]).await, (1, Some(831)));
    // a failed read: tapped where last seen
    assert_eq!(rest_from(840.0, &[Some(836.0), None]).await, (2, Some(836)));
    // gone from the screen (the list went on past it): no more waiting
    assert_eq!(rest_from(840.0, &[Some(-200.0), Some(-200.0)]).await, (1, None));
}

/// A list that never settles is waited for no longer than the limit
#[tokio::test]
async fn a_target_that_never_settles_is_waited_for_a_while_only() {
    let moving: Vec<_> = (0..10_000).map(|i| Some(800.0 - i as f64)).collect();
    let t0 = Instant::now();
    let (n, _) = rest_from(801.0, &moving).await;
    assert!((2..10_000).contains(&n), "{n}");
    assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
}

/// A WDA with one focused field
struct Fake(Mutex<Field>);

struct Field {
    keyboard: bool,
    kind: &'static str,
    placeholder: &'static str,
    value: String,
    /// characters the app drops from the end of the next keys
    drop_next: usize,
    /// what the app makes of typed text
    mangle: fn(&str) -> String,
    /// the field doesn't answer at all (WDA gone)
    gone: bool,
    /// its text is selected: the next keys replace it (a search field keeps its last search, selected)
    selected: bool,
    /// requests as `METHOD path`, and the text of every `/wda/keys`
    seen: Vec<String>,
    keys: Vec<String>,
}

impl Fake {
    fn new(kind: &'static str, value: &str) -> Self {
        Self(Mutex::new(Field {
            keyboard: true,
            kind,
            placeholder: "搜索",
            value: value.into(),
            drop_next: 0,
            mangle: str::to_owned,
            gone: false,
            selected: false,
            seen: vec![],
            keys: vec![],
        }))
    }
}

impl ictl_wda::http::Transport for &Fake {
    async fn send(
        &self,
        method: &str,
        path: &str,
        body: Option<Vec<u8>>,
        _: Duration,
    ) -> std::io::Result<(u16, Vec<u8>)> {
        let mut f = self.0.lock().unwrap();
        if f.gone {
            return Err(std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused"));
        }
        f.seen.push(format!("{method} {path}"));
        let value = |v: serde_json::Value| Ok((200, serde_json::json!({ "value": v }).to_string().into_bytes()));
        let path = path.strip_prefix("/session/S").unwrap_or(path);
        match (method, path) {
            ("POST", "/session") => Ok((200, br#"{"sessionId":"S"}"#.to_vec())),
            ("POST", "/elements") => {
                value(if f.keyboard { serde_json::json!([{"ELEMENT": "K"}]) } else { serde_json::json!([]) })
            }
            ("GET", "/element/active") => value(serde_json::json!({"ELEMENT": "F"})),
            ("GET", "/element/F/attribute/type") => value(format!("XCUIElementType{}", f.kind).into()),
            ("GET", "/element/F/attribute/placeholderValue") => value(f.placeholder.into()),
            // an empty field reads its placeholder
            ("GET", "/element/F/attribute/value") => {
                value(if f.value.is_empty() { f.placeholder.into() } else { f.value.clone().into() })
            }
            ("POST", "/element/F/clear") => {
                f.value.clear();
                value(serde_json::Value::Null)
            }
            ("POST", "/wda/keys") => {
                let body: serde_json::Value = serde_json::from_slice(&body.unwrap()).unwrap();
                let typed: String = body["value"].as_array().unwrap().iter().map(|c| c.as_str().unwrap()).collect();
                f.keys.push(typed.clone());
                if typed != "\n" {
                    if std::mem::take(&mut f.selected) {
                        f.value.clear();
                    }
                    let keep = typed.chars().count().saturating_sub(std::mem::take(&mut f.drop_next));
                    let typed: String = typed.chars().take(keep).collect();
                    let typed = (f.mangle)(&typed);
                    f.value.push_str(&typed);
                }
                value(serde_json::Value::Null)
            }
            _ => Ok((404, br#"{"value":{"error":"unknown command","message":"no route"}}"#.to_vec())),
        }
    }
}

async fn replace(f: &Fake, text: &str, submit: bool) -> Result<String, String> {
    let mut w = WdaClient::new(f);
    wda_replace(&mut w, text, submit).await.map(|(message, _)| message).map_err(|e| match e {
        WdaReplace::Untouched(e) => format!("untouched: {e}"),
        WdaReplace::Failed(e) => e.to_string(),
    })
}

#[tokio::test]
async fn a_single_line_field_never_receives_a_second_return_during_repair() {
    // a newline dropped, or turned into a space (Spotlight does that with pasted text)
    let mangles: [fn(&str) -> String; 2] = [|s| s.trim_end().to_owned(), |s| s.replace('\n', " ")];
    for mangle in mangles {
        let f = Fake::new("TextField", "");
        f.0.lock().unwrap().mangle = mangle;
        let mut w = WdaClient::new(&f);
        let before = readable_focus(&mut w).await.unwrap();
        w.keys("hi\n").await.unwrap();
        assert_eq!(verify_typed(&mut w, Some(&before), "hi\n").await, Typed::Unknown);
        assert_eq!(f.0.lock().unwrap().keys, ["hi\n"]);
    }

    let f = Fake::new("TextField", "");
    f.0.lock().unwrap().mangle = |s| s.trim_end().to_owned();
    assert!(replace(&f, "hi\n", false).await.is_err());
    assert_eq!(f.0.lock().unwrap().keys, ["hi\n"]);
}

/// Typing over a selection can't be told apart from the text having been there, and reading again can't tell
/// either: one reread of the field already known (which element has focus, its value) answers, with no wait and no
/// second read (they made typing into Spotlight about 1.8 s slower through WDA on an iPad mini 5)
#[tokio::test]
async fn typing_over_a_selection_is_judged_after_one_reread() {
    for (old, text) in [("设置", "设置"), ("wifi", "设置")] {
        let f = Fake::new("SearchField", old);
        f.0.lock().unwrap().selected = true;
        let mut w = WdaClient::new(&f);
        let before = readable_focus(&mut w).await.unwrap();
        w.keys(text).await.unwrap();
        let asked = f.0.lock().unwrap().seen.len();
        assert_eq!(verify_typed(&mut w, Some(&before), text).await, Typed::Unknown);
        let field = f.0.into_inner().unwrap();
        assert_eq!(field.value, text);
        assert_eq!(field.seen[asked..], ["GET /session/S/element/active", "GET /session/S/element/F/attribute/value"]);
    }
}

#[tokio::test]
async fn replaces_the_focused_field_then_presses_return() {
    let f = Fake::new("SearchField", "wifi");
    assert_eq!(replace(&f, "蓝牙", true).await.unwrap(), "已替换为 2 个字符，并按了回车");
    let f = f.0.into_inner().unwrap();
    assert_eq!(f.value, "蓝牙");
    assert_eq!(f.keys, ["蓝牙", "\n"]);
    assert!(f.seen.contains(&"POST /session/S/element/F/clear".to_owned()), "{:?}", f.seen);
}

#[tokio::test]
async fn empty_text_empties_the_field() {
    let f = Fake::new("TextField", "wifi");
    assert_eq!(replace(&f, "", false).await.unwrap(), "已清空输入框");
    let f = f.0.into_inner().unwrap();
    assert_eq!((f.value.as_str(), f.keys.len()), ("", 0));
}

#[tokio::test]
async fn replacement_reports_noop_and_field_mismatch_separately() {
    let f = Fake::new("TextField", "same");
    let mut w = WdaClient::new(&f);
    for (text, submit, expected) in
        [("same", false, Delivery::NotSent), ("same", true, Delivery::Sent), ("", false, Delivery::Sent)]
    {
        let Ok((_, delivery)) = wda_replace(&mut w, text, submit).await else { panic!("replacement failed") };
        assert_eq!(delivery, expected);
    }
    f.0.lock().unwrap().mangle = |s| s.to_uppercase();
    let Err(WdaReplace::Failed(e)) = wda_replace(&mut w, "mismatch", true).await else {
        panic!("expected a verification failure")
    };
    let response = e.response();
    let req = crate::protocol::Request::ReplaceText { text: "mismatch".into(), submit: true };
    assert_eq!(response.delivery(&req), Delivery::Sent);
    assert_eq!(response.field_readback(&req), Some("failed"));
}

/// The field already holds the text (or, emptying, reads its placeholder): nothing is cleared or typed
#[tokio::test]
async fn a_field_holding_the_text_is_left_alone() {
    for (value, text) in [("wifi", "wifi"), ("", "")] {
        let f = Fake::new("SearchField", value);
        replace(&f, text, true).await.unwrap();
        let f = f.0.into_inner().unwrap();
        assert!(!f.seen.iter().any(|s| s.ends_with("/clear")), "{:?}", f.seen);
        assert_eq!(f.keys, ["\n"]);
    }
}

#[tokio::test]
async fn dropped_last_characters_are_typed_again() {
    let f = Fake::new("TextField", "old");
    f.0.lock().unwrap().drop_next = 2;
    let r = replace(&f, "故宫博物院", false).await.unwrap();
    assert!(r.contains("2 个字没输进去"), "{r}");
    assert_eq!(f.0.into_inner().unwrap().value, "故宫博物院");
}

/// A field that doesn't read back as the text: Return isn't pressed, and the old text is said to be gone
#[tokio::test]
async fn a_mismatch_is_refused_without_return() {
    let f = Fake::new("TextField", "wifi");
    f.0.lock().unwrap().mangle = |s| s.to_uppercase();
    let e = replace(&f, "bluetooth", true).await.unwrap_err();
    assert!(e.contains("原来的文字已经删掉了") && e.contains("BLUETOOTH") && e.contains("没有按回车"), "{e}");
    assert!(!f.0.into_inner().unwrap().keys.contains(&"\n".to_owned()));
}

#[tokio::test]
async fn password_fields_and_missing_keyboards_are_refused_untouched() {
    let f = Fake::new("SecureTextField", "secret");
    assert!(replace(&f, "x", true).await.unwrap_err().contains("密码框"));
    let f = f.0.into_inner().unwrap();
    assert_eq!((f.value.as_str(), f.keys.len()), ("secret", 0));

    let f = Fake::new("TextField", "wifi");
    f.0.lock().unwrap().keyboard = false;
    assert!(replace(&f, "x", false).await.unwrap_err().starts_with("屏幕上没有键盘"));
    assert!(!f.0.into_inner().unwrap().seen.iter().any(|s| s.contains("/element/active")));
}

/// WDA unreachable before anything was done: another way may still replace the text
#[tokio::test]
async fn an_unreachable_wda_leaves_the_field_untouched() {
    let f = Fake::new("TextField", "wifi");
    f.0.lock().unwrap().gone = true;
    assert!(replace(&f, "x", false).await.unwrap_err().starts_with("untouched"));
}
