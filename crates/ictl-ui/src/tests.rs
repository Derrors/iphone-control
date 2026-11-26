use super::parse::{is_percentage, is_scroll_bar};
use super::*;

/// [`super::cover_sheet`] on WDA's XML, as the client reads it
fn cover_sheet(xml: &str) -> CoverSheet {
    super::cover_sheet(&Source::from_xml(xml))
}

fn touch_id_prompt(xml: &str) -> bool {
    super::lock::touch_id_prompt(&Source::from_xml(xml))
}

const SCREEN: ScreenSize = ScreenSize { width: 430.0, height: 932.0 };
const HOME: &str = include_str!("../tests/fixtures/home.xml");
const LOCK: &str = include_str!("../tests/fixtures/lockscreen.xml");
const LOCK_IOS27: &str = include_str!("../tests/fixtures/lockscreen-ios27.xml");
const PIN_IOS27: &str = include_str!("../tests/fixtures/passcode-view-ios27.xml");
const NC_IOS27: &str = include_str!("../tests/fixtures/notification-center-ios27.xml");
const PARTIAL_IOS27: &str = include_str!("../tests/fixtures/passcode-partial-ios27.xml");
const SETTINGS_IOS27: &str = include_str!("../tests/fixtures/settings-floating-search-ios27.xml");
const HOME_WIDGETS_IOS27: &str = include_str!("../tests/fixtures/home-widgets-ios27.xml");
const WALLET_IOS27: &str = include_str!("../tests/fixtures/settings-wallet-ios27.xml");
const IPAD_TOUCHID: &str = include_str!("../tests/fixtures/ipad-touchid-landscape.xml");
/// iPhone 15, iOS 27.2, German: Clock → Stopwatch, with four scroll bars labelled "… Rollbalken"
const CLOCK_DE_IOS27: &str = include_str!("../tests/fixtures/clock-stopwatch-de-ios27.xml");
const IPAD_PIN: &str = include_str!("../tests/fixtures/ipad-passcode-landscape.xml");
const IPAD_LOCK_P: &str = include_str!("../tests/fixtures/ipad-lockscreen-portrait.xml");
const IPAD_TOUCHID_P: &str = include_str!("../tests/fixtures/ipad-touchid-portrait.xml");
const IPAD_PIN_P: &str = include_str!("../tests/fixtures/ipad-passcode-portrait.xml");
const IPAD_TOUCHID_LEFT: &str = include_str!("../tests/fixtures/ipad-touchid-landscape-home-left.xml");
const IPAD_PIN_LEFT: &str = include_str!("../tests/fixtures/ipad-passcode-landscape-home-left.xml");
const IPAD_LAND: ScreenSize = ScreenSize { width: 1024.0, height: 768.0 };

fn land(o: Option<Rotation>) -> View {
    View { screen: IPAD_LAND, rotation: o }
}

/// iPad 横屏主屏：Application 是屏幕坐标，窗口是竖屏坐标
fn ipad_home(window: (u32, u32)) -> String {
    ipad_home_at(window, (386, 546), None)
}

/// `icon` 是「计算器」图标的原始坐标；`status_bar_x` 是竖屏坐标里状态栏竖条（32x1024）的 x
fn ipad_home_at(window: (u32, u32), icon: (u32, u32), status_bar_x: Option<u32>) -> String {
    let bar = status_bar_x.map_or(String::new(), |x| {
        format!(
            r#"<XCUIElementTypeWindow type="XCUIElementTypeWindow" x="0" y="0" width="768" height="1024">
<XCUIElementTypeStatusBar type="XCUIElementTypeStatusBar" x="{x}" y="0" width="32" height="1024"/></XCUIElementTypeWindow>"#
        )
    });
    format!(
        r#"<XCUIElementTypeApplication type="XCUIElementTypeApplication" x="0" y="0" width="1024" height="768">
<XCUIElementTypeWindow type="XCUIElementTypeWindow" x="0" y="0" width="{}" height="{}">
<XCUIElementTypeIcon type="XCUIElementTypeIcon" label="计算器" x="{}" y="{}" width="96" height="72"/>
</XCUIElementTypeWindow>{bar}</XCUIElementTypeApplication>"#,
        window.0, window.1, icon.0, icon.1
    )
}

#[test]
fn delete_key_found_by_position_on_real_keypad() {
    let els = parse(PARTIAL_IOS27, SCREEN);
    let pad = keypad(&els).unwrap();
    assert_eq!(delete_key(&els, &pad), Some((327, 857)));
    assert_eq!(passcode_field(&els), Some((6, 2)));
    // 「紧急情况」在左下，不能认成删除
    assert!(els.iter().any(|e| e.label == "紧急情况"));
}

/// The Touch ID prompt in a language whose words aren't known (here German): still a lock screen, and its first
/// button (Use Passcode) is the one to tap. The lock screen itself, with its two buttons in the corners, isn't it
#[test]
fn a_touch_id_prompt_in_any_language() {
    let german = |xml: &str| {
        xml.replace("使用密码", "Code verwenden")
            .replace("取消", "Abbrechen")
            .replace("触控 ID", "Touch ID")
            .replace("触控ID", "Touch ID")
    };
    let portrait = View { screen: ScreenSize { width: 768.0, height: 1024.0 }, rotation: Some(Rotation::Portrait) };
    let cases = [
        (IPAD_TOUCHID_P, portrait),
        (IPAD_TOUCHID, land(Some(Rotation::TopToLeft))),
        (IPAD_TOUCHID_LEFT, land(Some(Rotation::TopToLeft))),
    ];
    for (xml, view) in cases {
        let de = german(xml);
        assert!(!de.contains("使用密码"));
        assert_eq!(cover_sheet(&de), CoverSheet::Locked);
        assert_eq!(use_passcode_button(&parse(&de, view)), use_passcode_button(&parse(xml, view)), "the same button");
        assert!(use_passcode_button(&parse(xml, view)).is_some());
        // listed the other way round: still the upper one
        let mut reversed = parse(&de, view);
        reversed.reverse();
        assert_eq!(use_passcode_button(&reversed), use_passcode_button(&parse(xml, view)));
    }
    // the name on something that isn't a window: not the prompt
    let other = IPAD_TOUCHID.replace(
        "<XCUIElementTypeWindow type=\"XCUIElementTypeWindow\" name=\"SBCoverSheetWindow\"",
        "<XCUIElementTypeWindow type=\"XCUIElementTypeWindow\"><XCUIElementTypeOther name=\"SBCoverSheetWindow\"/",
    );
    assert!(!touch_id_prompt(&german(&other)));
    assert!(touch_id_prompt(&german(IPAD_TOUCHID)));
    assert_eq!(use_passcode_button(&parse(IPAD_LOCK_P, portrait)), None, "not a button on the lock screen");
    // the notification center has the same window, with the lock screen's date: not taken for the prompt
    assert_eq!(cover_sheet(NC_IOS27), CoverSheet::NotificationCenter);
    for xml in [HOME, SETTINGS_IOS27, HOME_WIDGETS_IOS27, WALLET_IOS27] {
        assert_eq!(cover_sheet(xml), CoverSheet::None);
    }
}

#[test]
fn cover_sheet_kinds_from_real_trees() {
    assert_eq!(cover_sheet(LOCK_IOS27), CoverSheet::Locked);
    // 密码键盘界面没有 lockscreen-date-view，也必须算锁屏（自动解锁靠它判断是否解开了）
    assert_eq!(cover_sheet(PIN_IOS27), CoverSheet::Locked);
    assert_eq!(cover_sheet(NC_IOS27), CoverSheet::NotificationCenter);
    assert_eq!(cover_sheet(HOME), CoverSheet::None);
    // 真机样本也要能正常解析
    for xml in [LOCK_IOS27, PIN_IOS27, NC_IOS27] {
        assert!(!parse(xml, SCREEN).is_empty());
    }
}

fn labels(els: &[Element]) -> Vec<String> {
    els.iter().map(|e| if e.label.is_empty() { e.name.clone() } else { e.label.clone() }).collect()
}

fn home() -> Vec<Element> {
    parse(HOME, SCREEN)
}

#[test]
fn full_screen_containers_are_dropped() {
    let h = home();
    assert!(!labels(&h).contains(&"Home screen icons".to_owned()));
    assert!(h.iter().all(|e| e.kind != "Application" && e.kind != "Window"));
}

#[test]
fn child_repeating_parent_label_and_rect_is_deduped() {
    let h = home();
    let l = labels(&h);
    assert_eq!(l.iter().filter(|s| *s == "设置").count(), 1);
    assert_eq!(h[l.iter().position(|s| s == "设置").unwrap()].kind, "Icon");
}

#[test]
fn offscreen_page_and_degenerate_elements_are_dropped() {
    let l = labels(&home());
    assert!(!l.contains(&"第二页的 App".to_owned()));
    assert!(!l.contains(&"零宽".to_owned()));
}

#[test]
fn unlabeled_elements_are_dropped() {
    assert!(home().iter().all(|e| !(e.label.is_empty() && e.name.is_empty() && e.value.is_empty())));
}

#[test]
fn center_is_clipped_to_visible_part() {
    let h = home();
    assert_eq!(h.iter().find(|e| e.label == "被顶出屏幕一半").unwrap().center, (150, 10));
}

#[test]
fn scroll_bars_are_dropped() {
    assert!(!labels(&home()).iter().any(|l| l.contains("滚动条")));
    assert!(!labels(&parse(SETTINGS_IOS27, SCREEN)).iter().any(|l| l.contains("滚动条")));
}

/// Scroll bars are recognised by shape and value, not by their label, so they are dropped in any
/// system language (here German, with "0\u{a0}%" as the value)
#[test]
fn scroll_bars_are_dropped_in_any_language() {
    let els = parse(CLOCK_DE_IOS27, ScreenSize { width: 393.0, height: 852.0 });
    let l = labels(&els);
    assert!(!l.iter().any(|l| l.contains("Rollbalken")), "{l:?}");
    assert!(l.iter().any(|l| l == "Stoppuhr") && l.iter().any(|l| l == "Start"), "{l:?}");
    assert!(els.iter().any(|e| e.kind == "PageIndicator"), "a percentage-free value stays");
}

#[test]
fn percentages_in_several_locales() {
    for v in ["0%", "73%", "100 %", "0\u{a0}%", "0\u{202f}%", "%40"] {
        assert!(is_percentage(v), "{v:?}");
    }
    for v in ["", "%", "1", "Seite 1 von 2", "50% off", "1,5 %"] {
        assert!(!is_percentage(v), "{v:?}");
    }
    let screen = (393.0, 852.0);
    assert!(!is_scroll_bar("Other", "Akku", "80 %", (60.0, 120.0), screen), "too wide for a scroll bar");
    assert!(!is_scroll_bar("Slider", "Lautstärke", "50 %", (300.0, 30.0), screen), "only Other");
    assert!(is_scroll_bar("Other", "Barre de défilement verticale, 1 page", "0\u{202f}%", (30.0, 700.0), screen));
    assert!(!is_scroll_bar("Other", "Download", "40 %", (350.0, 6.0), screen), "a thin progress strip stays");
    assert!(!is_scroll_bar("Other", "Download", "40 %", (120.0, 30.0), screen), "a short one too");
}

/// 真机：设置首页往上滑后，「钱包与Apple Pay」那一行（832～886）被底部悬浮搜索栏盖住：
/// 栏的底板从 856 起，实际吞点击的范围再往上约 20 点，露出来的部分点不中
#[test]
fn tap_point_avoids_floating_search_bar() {
    let t = parse_tree(SETTINGS_IOS27, SCREEN);
    let wallet = t.elements.iter().find(|e| e.label == "钱包与Apple Pay").unwrap();
    assert_eq!(wallet.center, (215, 859));
    assert_eq!(t.tap_point(wallet), TapPoint::Covered { by: "搜索".into() });
    // 中心在栏上方、下半截被盖住的：改点上半截
    let mut half = wallet.clone();
    half.visible = Rect { y: 810.0, ..half.visible };
    half.center = (215, 837);
    match t.tap_point(&half) {
        TapPoint::Moved { at, by } => {
            assert!(at.1 < 832.0 && at.1 >= 810.0, "{at:?}");
            assert_eq!(by, "搜索");
        }
        other => panic!("{other:?}"),
    }
    // 没被挡的照常点中心
    let wallpaper = t.elements.iter().find(|e| e.label == "相机").unwrap();
    assert_eq!(t.tap_point(wallpaper), TapPoint::Center((wallpaper.center.0 as f64, wallpaper.center.1 as f64)));
    // 搜索栏自己（在最上层）不算被挡
    let search = t.elements.iter().find(|e| e.kind == "SearchField").unwrap();
    assert!(matches!(t.tap_point(search), TapPoint::Center(_)));
}

/// 真机：设置 › 钱包与Apple Pay。单元格里的背景层排在按钮后面、滚到导航栏底下的行排在导航栏后面，都不算盖住
#[test]
fn decorations_and_content_under_nav_bar_do_not_cover() {
    let t = parse_tree(WALLET_IOS27, SCREEN);
    for label in ["通知、隐式推送", "设置"] {
        let e = t.elements.iter().find(|e| e.label == label).unwrap();
        assert!(matches!(t.tap_point(e), TapPoint::Center(_)), "{label}");
    }
}

/// 真机：天气的定位服务弹窗，按钮后面排着弹窗自己的背景层（没有文字）
#[test]
fn alert_buttons_are_not_covered_by_their_own_alert() {
    let xml = r#"<XCUIElementTypeApplication type="XCUIElementTypeApplication" x="0" y="0" width="430" height="932">
        <XCUIElementTypeAlert type="XCUIElementTypeAlert" label="打开定位服务" x="55" y="404" width="320" height="149">
          <XCUIElementTypeButton type="XCUIElementTypeButton" label="取消" x="219" y="488" width="140" height="49"/>
          <XCUIElementTypeOther type="XCUIElementTypeOther" x="55" y="404" width="320" height="149"/>
        </XCUIElementTypeAlert>
    </XCUIElementTypeApplication>"#;
    let t = parse_tree(xml, SCREEN);
    let cancel = t.elements.iter().find(|e| e.label == "取消").unwrap();
    assert!(matches!(t.tap_point(cancel), TapPoint::Center(_)));
}

/// iPad mini 5 / iPadOS 26.6.1 横屏的设置：浮动侧栏的列表后面跟着几层和侧栏一样大、没有文字的容器。
/// 它们不是盖在列表上的东西，侧栏里的「通用」要点得到
#[test]
fn ipad_floating_sidebar_rows_are_not_covered_by_sidebar_backdrop() {
    let xml = include_str!("../tests/fixtures/ipad-settings-sidebar-landscape.xml");
    let tree = parse_tree(xml, IPAD_LAND);
    for text in ["通用", "电池", "辅助功能", "关于本机"] {
        let e = find(&tree.elements, text)[0];
        assert!(matches!(tree.tap_point(e), TapPoint::Center(_)), "{text}: {:?}", tree.tap_point(e));
    }
}

#[test]
fn tap_point_reports_fully_covered() {
    let xml = r#"<XCUIElementTypeApplication type="XCUIElementTypeApplication" x="0" y="0" width="430" height="932">
        <XCUIElementTypeScrollView type="XCUIElementTypeScrollView" x="0" y="0" width="430" height="932">
          <XCUIElementTypeButton type="XCUIElementTypeButton" label="底下的按钮" x="100" y="500" width="100" height="40"/>
        </XCUIElementTypeScrollView>
        <XCUIElementTypeOther type="XCUIElementTypeOther" x="0" y="450" width="430" height="200">
          <XCUIElementTypeStaticText type="XCUIElementTypeStaticText" label="浮层" x="10" y="460" width="60" height="20"/>
        </XCUIElementTypeOther>
    </XCUIElementTypeApplication>"#;
    let t = parse_tree(xml, SCREEN);
    let b = t.elements.iter().find(|e| e.label == "底下的按钮").unwrap();
    assert_eq!(t.tap_point(b), TapPoint::Covered { by: "浮层".into() });
}

/// 真机：主屏天气小组件里的文字，WDA 给的是相对小组件的坐标
#[test]
fn widget_contents_are_moved_to_screen_coordinates() {
    let els = parse(HOME_WIDGETS_IOS27, SCREEN);
    let city = els.iter().find(|e| e.label == "示例市").unwrap();
    // 小组件内容区在 (29,84)，文字在其中 (19,19) 起 52x13
    assert_eq!(city.center, (74, 110));
    // 主屏图标本来就是屏幕坐标，不受影响
    let clock = els.iter().find(|e| e.label == "时钟" && e.kind == "Icon").unwrap();
    assert!(clock.center.1 > 400);
}

/// 真机（iOS 26，App 内浏览器）：WebView 在原生导航栏下面 (0,100)，WKWebView 内部那层报 (0,0) 且和 WebView
/// 一样大，看着像小组件；但网页元素给的是屏幕坐标，不能再加 WebView 的偏移
#[test]
fn web_contents_keep_screen_coordinates() {
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
    <XCUIElementTypeApplication type="XCUIElementTypeApplication" name="App" label="App" x="0" y="0" width="402" height="874">
      <XCUIElementTypeWindow type="XCUIElementTypeWindow" x="0" y="0" width="402" height="874">
        <XCUIElementTypeOther type="XCUIElementTypeOther" x="0" y="0" width="402" height="874">
          <XCUIElementTypeNavigationBar type="XCUIElementTypeNavigationBar" name="Page" x="0" y="56" width="402" height="44"/>
          <XCUIElementTypeOther type="XCUIElementTypeOther" x="0" y="100" width="402" height="774">
            <XCUIElementTypeWebView type="XCUIElementTypeWebView" x="0" y="100" width="402" height="697">
              <XCUIElementTypeWebView type="XCUIElementTypeWebView" x="0" y="100" width="402" height="697">
                <XCUIElementTypeOther type="XCUIElementTypeOther" x="0" y="0" width="402" height="697">
                  <XCUIElementTypeOther type="XCUIElementTypeOther" x="0" y="100" width="402" height="2990">
                    <XCUIElementTypeStaticText type="XCUIElementTypeStaticText" value="Getting started" name="Getting started" label="Getting started" x="19" y="375" width="80" height="27"/>
                  </XCUIElementTypeOther>
                </XCUIElementTypeOther>
              </XCUIElementTypeWebView>
            </XCUIElementTypeWebView>
          </XCUIElementTypeOther>
        </XCUIElementTypeOther>
      </XCUIElementTypeWindow>
    </XCUIElementTypeApplication>"#;
    let els = parse(xml, SCREEN);
    let text = els.iter().find(|e| e.label == "Getting started").unwrap();
    // 截图上就在这里，不是错开 100 点的 (59,488)
    assert_eq!(text.center, (59, 388));
}

/// 真机（iPhone 15 Pro Max，iOS 27.2.1，原生控制）：设置首页底部悬浮的搜索栏在扁平的树里只是一个 SearchField，
/// 也要像工具栏一样挡住下面的行。它吞掉输入框（866）往上 58 点以内的点击：806 还能进下面那一行，812 就不行
#[test]
fn a_native_floating_search_field_covers_the_rows_under_it() {
    const XML: &str = include_str!("../tests/fixtures/settings-floating-search-native-ios27.xml");
    let row = |t: &Tree, label: &str| t.elements.iter().find(|e| e.label == label).cloned().unwrap();
    let t = parse_tree(XML, SCREEN);
    // 766..818: clear of it, tapped at its center
    assert!(matches!(t.tap_point(&row(&t, "相机")), TapPoint::Center(_)));
    // 818..870: all of it is where the bar takes taps
    assert!(matches!(t.tap_point(&row(&t, "主屏幕与App资源库")), TapPoint::Covered { .. }));
    // 790..842: tapped above 808
    let lower = XML.replace(r#"y="766" width="390" height="52""#, r#"y="790" width="390" height="52""#);
    let t = parse_tree(&lower, SCREEN);
    match t.tap_point(&row(&t, "相机")) {
        TapPoint::Moved { at, .. } => assert!(at.1 <= 806.0, "{at:?}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn close_to_tolerates_small_drift_only() {
    let t = parse_tree(SETTINGS_IOS27, SCREEN);
    let mut moved = t.clone();
    for e in &mut moved.elements {
        e.center.1 += 2;
    }
    assert!(t.close_to(&moved, 2));
    assert!(!t.close_to(&moved, 1));
    moved.elements.pop();
    assert!(!t.close_to(&moved, 2));
}

#[test]
fn still_along_ignores_values_and_the_other_axis() {
    let t = parse_tree(SETTINGS_IOS27, SCREEN);
    let mut carousel = t.clone();
    carousel.elements[0].value = "第 2 页".into();
    carousel.elements[0].center.0 += 100;
    assert!(t.still_along(&carousel, true, 3));
    assert!(!t.still_along(&carousel, false, 3));
    let mut scrolled = t.clone();
    scrolled.elements[0].center.1 += 10;
    assert!(!t.still_along(&scrolled, true, 3));
}

#[test]
fn noise_is_dropped_but_tap_targets_stay() {
    let h = parse(HOME_WIDGETS_IOS27, SCREEN);
    let l = labels(&h);
    assert!(!l.iter().any(|s| s == "label-view"));
    // 状态栏（时间、信号、电量）
    assert!(!h.iter().any(|e| e.label.contains("电池电量")));
    assert!(h.iter().any(|e| e.kind == "Icon" && e.label == "设置"));
    let s = parse(SETTINGS_IOS27, SCREEN);
    // 按钮里重复按钮文字的 StaticText 和右边的箭头图标不再单列
    assert_eq!(s.iter().filter(|e| e.label == "钱包与Apple Pay").count(), 1);
    assert!(!s.iter().any(|e| e.label == "chevron.forward"));
    assert!(s.iter().any(|e| e.kind == "Button" && e.label == "通知"));
}

#[test]
fn long_text_and_ids_are_clipped_in_description() {
    let e = Element {
        kind: "Button".into(),
        label: "很长".repeat(100),
        name: format!("Open.{}", "X".repeat(100)),
        ..Default::default()
    };
    let d = e.describe();
    assert!(d.contains("…(共 200 字)") && d.chars().count() < 160, "{d}");
    assert!(filter(std::slice::from_ref(&e), "很长很长").len() == 1, "筛选仍按全文");
}

#[test]
fn filter_matches_type_but_find_does_not() {
    let s = parse(SETTINGS_IOS27, SCREEN);
    assert_eq!(filter(&s, "searchfield").len(), 1);
    assert!(find(&s, "SearchField").is_empty());
}

#[test]
fn find_shown_ignores_text_typed_into_fields() {
    let els = vec![
        Element {
            kind: "TextView".into(),
            label: "红烧肉做法".into(),
            value: "红烧肉做法".into(),
            ..Default::default()
        },
        Element { kind: "SearchField".into(), label: "搜索".into(), value: "红烧肉".into(), ..Default::default() },
    ];
    assert!(find_shown(&els, "红烧肉").is_empty());
    assert_eq!(find_shown(&els, "搜索").len(), 1);
    let mut more = els.clone();
    more.push(Element { kind: "StaticText".into(), label: "家常红烧肉".into(), ..Default::default() });
    assert_eq!(find_shown(&more, "红烧肉")[0].label, "家常红烧肉");
}

#[test]
fn field_values_skip_password_forms() {
    let field = |kind: &str, v: &str| Element { kind: kind.into(), value: v.into(), ..Default::default() };
    assert_eq!(field_values(&[field("SearchField", "故宫博")]), Some(vec!["故宫博".to_owned()]));
    assert_eq!(field_values(&[field("TextField", "me"), field("SecureTextField", "••")]), None);
}

#[test]
fn loading_indicator_and_alert_are_reported() {
    let xml = r#"<XCUIElementTypeApplication type="XCUIElementTypeApplication" x="0" y="0" width="430" height="932">
        <XCUIElementTypeActivityIndicator type="XCUIElementTypeActivityIndicator" x="200" y="450" width="30" height="30"/>
        <XCUIElementTypeAlert type="XCUIElementTypeAlert" label="“微信”想给你发送通知" x="55" y="390" width="320" height="173"/>
    </XCUIElementTypeApplication>"#;
    let t = parse_tree(xml, SCREEN);
    assert!(t.loading);
    assert_eq!(t.alert().unwrap().label, "“微信”想给你发送通知");
    assert!(!parse_tree(HOME, SCREEN).loading);
}

#[test]
fn type_prefix_is_removed() {
    let kinds: Vec<_> = home().iter().map(|e| e.kind.clone()).collect();
    for k in ["Icon", "Button", "StaticText"] {
        assert!(kinds.contains(&k.to_owned()));
    }
}

#[test]
fn describe_matches_toolkit_format() {
    let h = home();
    let d = |l: &str| h.iter().find(|e| e.label == l).unwrap().describe();
    assert_eq!(
        d("显示与亮度"),
        r#"Button "显示与亮度" id="com.apple.settings.displayAndBrightness" @(215,676) 390x53"#
    );
    assert!(d("设置").contains(r#"value="1个新项目""#));
}

#[test]
fn find_prefers_exact_over_substring() {
    let h = home();
    let names = |v: Vec<&Element>| v.into_iter().map(|e| e.label.clone()).collect::<Vec<_>>();
    assert_eq!(names(find(&h, "时钟")), ["时钟"]);
    assert_eq!(names(find(&h, "亮度")), ["显示与亮度"]);
    assert!(find(&h, "不存在").is_empty());
    assert_eq!(names(find(&h, "com.apple.settings.displayAndBrightness")), ["显示与亮度"]);
}

#[test]
fn filter_is_case_insensitive_and_checks_value() {
    let h = home();
    assert_eq!(filter(&h, "新项目").iter().map(|e| e.label.as_str()).collect::<Vec<_>>(), ["设置"]);
}

fn key(label: &str, at: (i64, i64), kind: &str) -> Element {
    Element {
        kind: kind.into(),
        label: label.into(),
        name: label.into(),
        center: at,
        size: (80, 80),
        ..Default::default()
    }
}

/// 电话键盘上数字 d 的位置：1 2 3 / 4 5 6 / 7 8 9 / 0 在 8 下面，间距 100
fn phone(d: i64) -> (i64, i64) {
    if d == 0 { (200, 400) } else { ((d - 1) % 3 * 100 + 100, (d - 1) / 3 * 100 + 100) }
}

#[test]
fn keypad_needs_all_ten_digits() {
    let keys: Vec<_> = (0..10).map(|d| key(&d.to_string(), phone(d), "Key")).collect();
    let pad = keypad(&keys).unwrap();
    assert_eq!(pad[0], (200, 400));
    assert_eq!(pad[9], (300, 300));
    assert!(keypad(&keys[..9]).is_none());
}

#[test]
fn keypad_accepts_letters_after_digit_but_not_multi_digit_labels() {
    let keys: Vec<_> = (0..10).map(|d| key(&format!("{d} ABC"), phone(d), "Button")).collect();
    assert!(keypad(&keys).is_some());
    let mut keys: Vec<_> = (0..9).map(|d| key(&d.to_string(), phone(d), "Key")).collect();
    keys.push(key("10", phone(0), "Key"));
    assert!(keypad(&keys).is_none());
    // 锁屏通知上的「3条通知」不能占掉 3 的位置
    let mut keys = vec![key("3条通知", (999, 50), "Button")];
    keys.extend((0..10).map(|d| key(&d.to_string(), phone(d), "Key")));
    assert_eq!(keypad(&keys).unwrap()[3], (300, 100));
}

#[test]
fn keypad_ignores_non_key_types() {
    let keys: Vec<_> = (0..10).map(|d| key(&d.to_string(), phone(d), "StaticText")).collect();
    assert!(keypad(&keys).is_none());
}

/// 横屏时竖屏坐标没换算：1 2 3 竖着排。不能当成键盘去按（会按错数字）
#[test]
fn keypad_with_wrong_layout_is_rejected() {
    let rotated: Vec<_> = (0..10).map(|d| key(&d.to_string(), (phone(d).1, phone(d).0), "Key")).collect();
    assert!(keypad(&rotated).is_none());
    let mut swapped: Vec<_> = (0..10).map(|d| key(&d.to_string(), phone(d), "Key")).collect();
    swapped[1].center = phone(3);
    swapped[3].center = phone(1);
    assert!(keypad(&swapped).is_none());
    let mut zero_on_top: Vec<_> = (0..10).map(|d| key(&d.to_string(), phone(d), "Key")).collect();
    zero_on_top[0].center = (200, 0);
    assert!(keypad(&zero_on_top).is_none());
}

/// 回归用例 #1：键盘收起时 Key 元素仍在树里。只能证明"树里有"，解锁逻辑不能拿它判断键盘已显示
#[test]
fn hidden_keypad_still_present_in_tree() {
    let els = parse(LOCK, SCREEN);
    let pad = keypad(&els).expect("隐藏的键盘仍能解析出 10 个键");
    assert_eq!(pad[1], (103, 371));
    assert_eq!(pad[0], (215, 695));
    let field = els.iter().find(|e| e.kind == "SecureTextField").unwrap();
    assert_eq!(parse_passcode_value(&field.value), Some((6, 0)));
}

#[test]
fn passcode_value_variants() {
    assert_eq!(parse_passcode_value("共6位数，已输入0位"), Some((6, 0)));
    assert_eq!(parse_passcode_value("共6位数，已输入2位"), Some((6, 2)));
    assert_eq!(parse_passcode_value("6-digit passcode, 3 entered"), Some((6, 3)));
    // 英、法、德、西语先说已输入几位（#174）
    assert_eq!(parse_passcode_value("0 of 6 values entered"), Some((6, 0)));
    assert_eq!(parse_passcode_value("2 of 6 values entered"), Some((6, 2)));
    assert_eq!(parse_passcode_value("0 valeur saisie sur 6"), Some((6, 0)));
    assert_eq!(parse_passcode_value("0 von 6 Werten eingegeben"), Some((6, 0)));
    assert_eq!(parse_passcode_value("0 de 4 valores introducidos"), Some((4, 0)));
    assert_eq!(parse_passcode_value("6 of 6 values entered"), Some((6, 6)));
    assert_eq!(parse_passcode_value("0 0"), None);
    assert_eq!(parse_passcode_value("共4位数"), None);
    assert_eq!(parse_passcode_value(""), None);
}

#[test]
fn orientation_from_wda_and_screen_size() {
    assert_eq!(Rotation::from_wda_orientation("LANDSCAPE"), Some(Rotation::TopToLeft));
    assert_eq!(Rotation::from_wda_orientation("UIA_DEVICE_ORIENTATION_LANDSCAPERIGHT"), Some(Rotation::TopToRight));
    assert_eq!(
        Rotation::from_wda_orientation("UIA_DEVICE_ORIENTATION_PORTRAIT_UPSIDEDOWN"),
        Some(Rotation::UpsideDown)
    );
    assert_eq!(Rotation::from_wda_orientation("PORTRAIT"), Some(Rotation::Portrait));
    assert_eq!(Rotation::from_wda_orientation("UNKNOWN"), None);
    let portrait = ScreenSize { width: 768.0, height: 1024.0 };
    assert_eq!(Rotation::TopToRight.orient(portrait), IPAD_LAND);
    assert_eq!(Rotation::Portrait.orient(IPAD_LAND), portrait);
    assert_eq!(Rotation::UpsideDown.orient(portrait), portrait);
}

/// 真机：摄像头在左（z=270）时，HID 按竖放发 (265,314) 落在了界面的 (314,165)，反过来要换算回 (265,314)
#[test]
fn rotation_maps_screen_points_back_to_portrait() {
    let p = ScreenSize { width: 932.0, height: 430.0 }.portrait();
    assert_eq!(p, ScreenSize { width: 430.0, height: 932.0 });
    assert_eq!(Rotation::from_degrees(270.0), Some(Rotation::TopToLeft));
    assert_eq!(Rotation::from_degrees(90.0), Some(Rotation::TopToRight));
    assert_eq!(Rotation::from_degrees(-90.0), Some(Rotation::TopToLeft));
    assert_eq!(Rotation::from_degrees(0.0), Some(Rotation::Portrait));
    assert_eq!(Rotation::from_degrees(45.0), None);
    for z in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.4, 90.1] {
        assert_eq!(Rotation::from_degrees(z), None);
    }
    assert_eq!(Rotation::TopToLeft.to_portrait((314.0, 165.0), p), (265.0, 314.0));
    // 顶边在右：界面左上角是竖放时的左下角
    assert_eq!(Rotation::TopToRight.to_portrait((0.0, 0.0), p), (0.0, 932.0));
    assert_eq!(Rotation::TopToRight.to_portrait((226.0, 97.0), p), (97.0, 706.0));
    assert_eq!(Rotation::UpsideDown.to_portrait((0.0, 0.0), p), (430.0, 932.0));
    assert_eq!(Rotation::Portrait.to_portrait((12.0, 34.0), p), (12.0, 34.0));
}

/// 真机：横屏主屏上「计算器」树里在 (434,582)，截图里在 (582,334) 附近
#[test]
fn ipad_landscape_springboard_is_rotated_to_screen_coordinates_by_orientation_without_status_bar() {
    let left = parse(&ipad_home((768, 1024)), land(Some(Rotation::TopToLeft)));
    assert_eq!((left[0].center, left[0].size), ((582, 334), (72, 96)));
    let right = parse(&ipad_home((768, 1024)), land(Some(Rotation::TopToRight)));
    assert_eq!((right[0].center, right[0].size), ((442, 434), (72, 96)));
}

/// App 里（窗口和屏幕一样是横的）坐标本来就是屏幕坐标，不转
#[test]
fn ipad_landscape_app_tree_is_not_rotated() {
    let els = parse(&ipad_home((1024, 768)), land(Some(Rotation::TopToLeft)));
    assert_eq!((els[0].center, els[0].size), ((434, 582), (96, 72)));
}

/// 竖屏时屏幕下半截（y > 768）的元素也要在：转屏前缓存的横屏尺寸会把它们当成屏幕外
#[test]
fn ipad_portrait_keeps_elements_below_768() {
    let xml = r#"<XCUIElementTypeApplication type="XCUIElementTypeApplication" x="0" y="0" width="768" height="1024">
<XCUIElementTypeWindow type="XCUIElementTypeWindow" x="0" y="0" width="768" height="1024">
<XCUIElementTypeIcon type="XCUIElementTypeIcon" label="Safari浏览器" x="40" y="940" width="69" height="69"/>
</XCUIElementTypeWindow>
<XCUIElementTypeWindow type="XCUIElementTypeWindow" x="0" y="0" width="1024" height="768"/>
</XCUIElementTypeApplication>"#;
    let portrait = View { screen: ScreenSize { width: 768.0, height: 1024.0 }, rotation: Some(Rotation::Portrait) };
    assert_eq!(parse(xml, portrait)[0].center, (74, 974));
}

/// 横屏锁屏：转好之后键盘是电话键盘的样子，「取消/删除」和 0 同一行，「使用密码」在提示页中间（真机截图对得上）
#[test]
fn ipad_landscape_lock_screen_keypad_and_use_passcode() {
    let els = parse(IPAD_PIN, land(Some(Rotation::TopToLeft)));
    let pad = keypad(&els).expect("转好坐标后应能认出键盘");
    let want = [
        (512, 586),
        (406, 282),
        (512, 282),
        (618, 282),
        (406, 384),
        (512, 384),
        (618, 384),
        (406, 484),
        (512, 484),
        (618, 484),
    ];
    assert_eq!(pad, want);
    assert_eq!(delete_key(&els, &pad), Some((618, 586)));
    assert_eq!(passcode_field(&els), Some((6, 0)));
    assert_eq!(use_passcode_button(&els), None);

    let bouncer = parse(IPAD_TOUCHID, land(Some(Rotation::TopToLeft)));
    assert_eq!(use_passcode_button(&bouncer), Some((512, 430)));
    assert!(keypad(&bouncer).is_none());
    assert_eq!(passcode_field(&bouncer), None);
}

/// 没有状态栏、也不知道方向就不转；没转的竖屏坐标排不成电话键盘，认不出键盘，一位都不会按
#[test]
fn ipad_landscape_keypad_without_status_bar_or_orientation_is_not_used() {
    let no_bar = regex::Regex::new(r"<XCUIElementTypeStatusBar [^>]*/>").unwrap().replace(IPAD_PIN, "");
    assert!(!no_bar.contains("StatusBar"));
    assert!(keypad(&parse(&no_bar, land(None))).is_none());
    assert!(keypad(&parse(&no_bar, land(Some(Rotation::Portrait)))).is_none());
    // 有状态栏就按它转，不用方向
    assert!(keypad(&parse(IPAD_PIN, land(None))).is_some());
}

/// Touch ID 的 iPad 锁屏、上滑后的提示、数字键盘都要认成锁屏：WDA 在锁屏时启动时 `/wda/locked` 报 false，
/// 认不出就会当成没锁（真机上发生过：工具照常执行，停在锁屏上）
#[test]
fn ipad_touch_id_lock_screens_are_locked() {
    for xml in [IPAD_LOCK_P, IPAD_TOUCHID_P, IPAD_PIN_P, IPAD_TOUCHID, IPAD_PIN] {
        assert_eq!(cover_sheet(xml), CoverSheet::Locked);
    }
    assert_eq!(cover_sheet(NC_IOS27), CoverSheet::NotificationCenter);
    assert_eq!(cover_sheet(HOME), CoverSheet::None);
}

/// 真机竖屏：提示页点「使用密码」，键盘页按键是电话键盘、「取消/删除」和 0 同一行
#[test]
fn ipad_portrait_touch_id_and_keypad_from_real_trees() {
    let portrait = View { screen: ScreenSize { width: 768.0, height: 1024.0 }, rotation: Some(Rotation::Portrait) };
    let bouncer = parse(IPAD_TOUCHID_P, portrait);
    assert_eq!(use_passcode_button(&bouncer), Some((384, 533)));
    assert!(keypad(&bouncer).is_none());
    let els = parse(IPAD_PIN_P, portrait);
    let pad = keypad(&els).unwrap();
    assert_eq!((pad[1], pad[0]), ((278, 410), (384, 714)));
    assert_eq!(delete_key(&els, &pad), Some((490, 714)));
    assert_eq!(passcode_field(&els), Some((6, 0)));
}

/// 真机两个横屏方向 WDA 都报 `LANDSCAPE`，「计算器」都在截图的 (582,334)：
/// Home 键在右时原始框 (386,546)、状态栏贴树的右边；Home 键在左时原始框 (286,406)、状态栏贴树的左边（x=0）
#[test]
fn ipad_landscape_direction_comes_from_status_bar_not_orientation() {
    let o = land(Some(Rotation::TopToLeft));
    let home_right = parse(&ipad_home_at((768, 1024), (386, 546), Some(736)), o);
    assert_eq!((home_right[0].center, home_right[0].size), ((582, 334), (72, 96)));
    let home_left = parse(&ipad_home_at((768, 1024), (286, 406), Some(0)), o);
    assert_eq!((home_left[0].center, home_left[0].size), ((582, 334), (72, 96)));
    // 状态栏说了算，方向报反了也一样
    let reported_right = land(Some(Rotation::TopToRight));
    assert_eq!(parse(&ipad_home_at((768, 1024), (386, 546), Some(736)), reported_right)[0].center, (582, 334));
    // 状态栏不是贴边的竖条：按方向
    assert_eq!(parse(&ipad_home_at((768, 1024), (386, 546), Some(300)), o)[0].center, (582, 334));
}

/// Home 键在左的横屏锁屏：WDA 照样报 LandscapeLeft，按状态栏换算后键位、「使用密码」和 Home 键在右时一样
#[test]
fn ipad_landscape_home_left_lock_screen_matches_home_right() {
    let o = land(Some(Rotation::TopToLeft));
    let right = parse(IPAD_PIN, o);
    let left = parse(IPAD_PIN_LEFT, o);
    let pad = keypad(&left).expect("Home 键在左也应认出键盘");
    assert_eq!(Some(pad), keypad(&right));
    assert_eq!(delete_key(&left, &pad), Some((618, 586)));
    assert_eq!(use_passcode_button(&parse(IPAD_TOUCHID_LEFT, o)), Some((512, 430)));
    assert_eq!(cover_sheet(IPAD_PIN_LEFT), CoverSheet::Locked);
}

#[test]
fn a_list_that_stopped_is_still_though_cells_came_and_went() {
    let tree = |rows: &[(&str, i64)]| {
        let cells: String = rows
            .iter()
            .map(|(l, y)| format!(r#"<XCUIElementTypeButton type="XCUIElementTypeButton" label="{l}" x="20" y="{y}" width="390" height="44"/>"#))
            .collect();
        parse_tree(
            &format!(
                r#"<XCUIElementTypeApplication type="XCUIElementTypeApplication" label="A" x="0" y="0" width="430" height="932">{cells}</XCUIElementTypeApplication>"#
            ),
            View { screen: ScreenSize { width: 430.0, height: 932.0 }, rotation: None },
        )
    };
    let a = tree(&[("通用", 100), ("辅助功能", 150), ("墙纸", 200), ("相机", 250), ("搜索", 300)]);
    // one cell more at the bottom edge, the rest where they were: stopped
    let b = tree(&[("通用", 100), ("辅助功能", 150), ("墙纸", 200), ("相机", 250), ("搜索", 300), ("待机显示", 850)]);
    assert!(b.still_along_common(&a, true, 3));
    assert!(!b.still_along(&a, true, 3), "the strict check waits for the same list");
    // still moving
    let c = tree(&[("辅助功能", 120), ("墙纸", 170), ("相机", 220), ("搜索", 270), ("待机显示", 320)]);
    assert!(!c.still_along_common(&a, true, 3));
    // too little in common to tell
    let d = tree(&[("墙纸", 200), ("x", 400), ("y", 450), ("z", 500), ("w", 550), ("v", 600)]);
    assert!(!d.still_along_common(&a, true, 3));
}

/// Every device sample, read, written out and read again, is the same tree
#[test]
fn a_source_written_as_xml_reads_back_the_same() {
    for xml in [
        HOME,
        LOCK,
        LOCK_IOS27,
        PIN_IOS27,
        NC_IOS27,
        PARTIAL_IOS27,
        SETTINGS_IOS27,
        HOME_WIDGETS_IOS27,
        WALLET_IOS27,
        IPAD_TOUCHID,
        CLOCK_DE_IOS27,
        IPAD_PIN,
        IPAD_LOCK_P,
        IPAD_TOUCHID_P,
        IPAD_PIN_P,
        IPAD_TOUCHID_LEFT,
        IPAD_PIN_LEFT,
    ] {
        let src = Source::from_xml(xml);
        assert!(!src.incomplete && src.roots.len() == 1);
        assert_eq!(Source::from_xml(&src.to_xml()), src);
    }
}

/// XML cut off keeps what came, nested as it was, and says it is incomplete; a node's kind is its `type`, or its tag
#[test]
fn a_cut_source_keeps_what_came() {
    let src = Source::from_xml(
        r#"<App label=" a "><XCUIElementTypeButton type="XCUIElementTypeCell" label="b"/><Text label="c">"#,
    );
    assert!(src.incomplete);
    let kinds: Vec<(&str, &str)> = src.nodes().map(|n| (n.kind.as_str(), n.label.as_str())).collect();
    assert_eq!(kinds, [("App", "a"), ("Cell", "b"), ("Text", "c")]);
    assert_eq!(src.roots[0].children.len(), 2);
    assert!(!Source::from_xml("<App><B/></App>").incomplete);
}

/// Deeper than any real tree: not read (and said so), so a hostile one can't run the stack out
#[test]
fn a_source_too_deep_is_cut() {
    let xml = "<A>".repeat(2000) + "<B label=\"deep\"/>" + &"</A>".repeat(2000) + "<C label=\"after\"/>";
    let src = Source::from_xml(&xml);
    assert!(src.incomplete);
    assert_eq!(src.nodes().count(), 513);
    assert!(src.nodes().all(|n| n.label != "deep"));
    assert_eq!(src.roots.last().map(|n| n.label.as_str()), Some("after"));
}

/// A cut-off read can't show the Touch ID prompt: it needs exactly two buttons and no lock screen date, which only
/// the whole window can tell. The notification center cut after its first two buttons isn't taken for it
#[test]
fn a_cut_read_is_no_touch_id_prompt() {
    let window = NC_IOS27.find("SBCoverSheetWindow").expect("the cover sheet window");
    let mut at = window;
    for _ in 0..3 {
        at += NC_IOS27[at..].find("<XCUIElementTypeButton ").expect("three buttons") + 1;
    }
    let cut = &NC_IOS27[..at - 1];
    assert!(!cut.contains("lockscreen-date-view"));
    assert!(Source::from_xml(cut).incomplete);
    assert!(!touch_id_prompt(cut));
    assert_ne!(cover_sheet(cut), CoverSheet::Locked);
}
