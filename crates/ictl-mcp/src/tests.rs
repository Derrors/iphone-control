use super::*;

fn args(v: Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap()
}

#[tokio::test]
async fn input_delivery_is_independent_of_field_and_screen_readback() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let socket = PathBuf::from(format!("/tmp/ictl-single-input-{}.sock", std::process::id()));
    let server = McpServer { socket: socket.clone(), task: false, lease: None };
    let call = |name, args: Value| {
        let req = CallToolRequestParams::new(name).with_arguments(args.as_object().unwrap().clone());
        let server = &server;
        async move {
            let CallToolResponse::Complete(result) = server.call_tool_inner(req).await.unwrap() else { panic!() };
            result
        }
    };
    let tap = json!({"x": 1, "y": 2});
    let unavailable = call("iphone_tap", tap.clone()).await;
    let data = unavailable.structured_content.unwrap();
    assert_eq!(data["delivery"], "not_sent");
    assert_eq!(data["stage"], "connect");

    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let exchanges = [
        ("tap", None), // request reached daemon, reply lost
        ("tap", Some(Response::text("Tapped"))),
        ("settle", Some(Response::text("Settled"))),
        ("screenshot", Some(Response::error("controlled device changed to phone-B"))),
        ("tap", Some(Response::text("Tapped"))),
        ("settle", Some(Response::error("lost settle"))),
        (
            "list_elements",
            Some(Response::text_data(
                "items",
                Default::default(),
                Some(protocol::Data::Elements { elements: vec![], matched_count: Some(3), truncated: Some(true) }),
            )),
        ),
        (
            "type_text",
            Some(Response::text_with("Typed", protocol::Outcome { field_verified: Some(false), ..Default::default() })),
        ),
        ("alert_check", Some(Response::text(""))),
        ("type_text", Some(Response::error("field mismatch").with_failure(Delivery::Sent, FailureStage::Readback))),
        (
            "tap_element",
            Some(Response::text_with("No match", protocol::Outcome { tapped_nothing: true, ..Default::default() })),
        ),
        ("alert_check", Some(Response::text(""))),
    ];
    let daemon = async {
        let mut first_snapshot = true;
        for (op, reply) in exchanges {
            let mut peer = loop {
                let (mut peer, _) = listener.accept().await.unwrap();
                let mut line = String::new();
                BufReader::new(&mut peer).read_line(&mut line).await.unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                if request["op"] == "daemon" {
                    // Old daemon snapshot: only the controlled device's UDID is needed.
                    let snapshot = if std::mem::take(&mut first_snapshot) {
                        Response::text(r#"{"device":null}"#)
                    } else {
                        Response::text(r#"{"device":{"udid":"phone-A"}}"#)
                    };
                    peer.write_all(format!("{}\n", json!(snapshot)).as_bytes()).await.unwrap();
                    continue;
                }
                assert_eq!(request["op"], op);
                assert_eq!(request[protocol::EXPECT_UDID], "phone-A", "{request}");
                break peer;
            };
            if let Some(reply) = reply {
                peer.write_all(format!("{}\n", json!(reply)).as_bytes()).await.unwrap();
            }
        }
    };
    let actions = async {
        let missing = call("iphone_tap", tap.clone()).await;
        let data = missing.structured_content.unwrap();
        assert_eq!(data["delivery"], "not_sent");
        assert_eq!(data["stage"], "preflight");

        let lost = call("iphone_tap", tap).await;
        assert_eq!(lost.is_error, Some(true));
        let data = lost.structured_content.unwrap();
        assert_eq!(data["delivery"], "unknown");
        assert_eq!(data["stage"], "reply");

        let tapped = call("iphone_tap", json!({"x":1,"y":2,"then":"screenshot"})).await;
        assert_ne!(tapped.is_error, Some(true));
        let data = tapped.structured_content.unwrap();
        assert_eq!(data["delivery"], "sent");
        assert_eq!(data["stage"], "readback");
        assert_eq!(data["readback"], json!({"settle":"ok", "screen":"failed"}));

        let listed = call("iphone_tap", json!({"x":1,"y":2,"then":"elements"})).await;
        assert_ne!(listed.is_error, Some(true));
        let data = listed.structured_content.unwrap();
        assert_eq!(data["delivery"], "sent");
        assert_eq!(data["readback"], json!({"settle":"failed", "screen":"ok"}));
        assert_eq!(data["matched_count"], 3);
        assert_eq!(data["truncated"], true);

        let text = json!({"text":"x"});
        let typed = call("iphone_type_text", text.clone()).await.structured_content.unwrap();
        assert_eq!(typed["delivery"], "sent");
        assert_eq!(typed["readback"]["field"], "unverified");
        let mismatch = call("iphone_type_text", text.clone()).await;
        assert_eq!(mismatch.is_error, Some(true));
        let data = mismatch.structured_content.unwrap();
        assert_eq!(data["delivery"], "sent");
        assert_eq!(data["readback"]["field"], "failed");

        let missed = call("iphone_tap_element", text).await;
        assert_ne!(missed.is_error, Some(true));
        assert_eq!(missed.structured_content.unwrap()["delivery"], "not_sent");
    };
    tokio::time::timeout(Duration::from_secs(4), async { tokio::join!(daemon, actions) }).await.unwrap();
    std::fs::remove_file(socket).unwrap();
}

fn has_cjk(s: &str) -> bool {
    s.chars().any(|c| matches!(c, '\u{3000}'..='\u{303f}' | '\u{4e00}'..='\u{9fff}' | '\u{ff00}'..='\u{ffef}'))
}

/// With IPHONE_CONTROL_LANG=en nothing the agent reads up front is Chinese: instructions, tool
/// descriptions, titles and parameter docs (only examples of Chinese input may stay)
#[test]
fn delivery_hints_do_not_repeat_punctuation_or_existing_screen_guidance() {
    ictl_l10n::with_lang(ictl_l10n::Lang::En, || {
        let mut message = "Focus changed. Inspect the screen.".to_owned();
        add_delivery_hint(&mut message, Delivery::Sent);
        assert_eq!(message, "Focus changed. Inspect the screen. Input was sent.");
    });
    ictl_l10n::with_lang(ictl_l10n::Lang::Zh, || {
        let mut message = "请求超时，请先截图确认。".to_owned();
        add_delivery_hint(&mut message, Delivery::Unknown);
        assert_eq!(message, "请求超时，请先截图确认。输入可能已送达。");
    });
}

#[test]
fn english_tool_definitions_have_no_chinese() {
    use ictl_l10n::{Lang, with_lang};
    with_lang(Lang::En, || {
        assert!(!has_cjk(instructions()), "{}", instructions());
        for t in tools_with(true) {
            let json = serde_json::to_string(&t).unwrap();
            assert!(!has_cjk(&json), "{}: {json}", t.name);
        }
    });
    with_lang(Lang::Zh, || assert!(has_cjk(instructions())));
}

/// scroll 是可选的：不给就和以前一样只看当前屏幕；方向只认 down / up，次数 1 到上限
#[test]
fn tap_element_scroll_search_is_optional_and_checked() {
    let req = |v: Value| to_request("iphone_tap_element", &args(v));
    let plain = req(json!({"text": "隐私"})).unwrap();
    assert_eq!(plain, Request::TapElement { text: "隐私".into(), index: 0, max_age_ms: 0, scroll: None });
    let down = req(json!({"text": "隐私", "scroll": "down"})).unwrap();
    let s = |down, max| Some(protocol::ScrollSearch { down, max });
    assert_eq!(down, Request::TapElement { text: "隐私".into(), index: 0, max_age_ms: 0, scroll: s(true, 8) });
    let up = req(json!({"text": "隐私", "scroll": "up", "max_scrolls": 3})).unwrap();
    assert_eq!(up, Request::TapElement { text: "隐私".into(), index: 0, max_age_ms: 0, scroll: s(false, 3) });
    for bad in [
        json!({"text": "x", "scroll": "left"}),
        json!({"text": "x", "scroll": "down", "max_scrolls": 0}),
        json!({"text": "x", "scroll": "down", "max_scrolls": 16}),
        json!({"text": "x", "scroll": true}),
    ] {
        assert!(req(bad).is_err());
    }
    // reused in a batch, the scroll search stays
    let steps = batch_steps(&args(
        json!({"reuse_elements": true, "steps": [{"tool": "iphone_tap_element", "text": "x", "scroll": "down"}]}),
    ));
    assert!(matches!(&steps.unwrap()[0], Step::Request { req: Request::TapElement { scroll: Some(_), .. }, .. }));
}

/// hold defaults to 0 (a plain swipe, same request as before) and is passed through when given
#[test]
fn swipe_hold_defaults_to_zero() {
    let plain = to_request("iphone_swipe", &args(json!({"from_x": 1, "from_y": 2, "to_x": 3, "to_y": 4}))).unwrap();
    assert_eq!(plain, Request::Swipe { from: (1.0, 2.0), to: (3.0, 4.0), seconds: 0.3, hold: 0.0 });
    let held = json!({"from_x": 1, "from_y": 2, "to_x": 3, "to_y": 4, "seconds": 0.7, "hold": 0.6});
    let held = to_request("iphone_swipe", &args(held)).unwrap();
    assert_eq!(held, Request::Swipe { from: (1.0, 2.0), to: (3.0, 4.0), seconds: 0.7, hold: 0.6 });
    assert!(
        to_request("iphone_swipe", &args(json!({"from_x": 1, "from_y": 2, "to_x": 3, "to_y": 4, "hold": "x"})))
            .is_err()
    );
}

/// one_gesture 把全是坐标点击的批量并成一个 TapSequence；混了别的步骤就报错，一步都不执行
#[test]
fn one_gesture_turns_taps_into_one_sequence() {
    let steps = batch_steps(&args(json!({"one_gesture": true, "steps": [
        {"tool": "iphone_tap", "x": 60, "y": 548},
        {"tool": "iphone_tap", "x": 23, "y": 548.5}
    ]})));
    let req = Request::TapSequence { points: vec![(60.0, 548.0), (23.0, 548.5)] };
    assert_eq!(steps, Ok(vec![Step::Request { tool: "iphone_tap".into(), req }]));
    for other in [
        json!({"tool": "iphone_tap_element", "text": "1"}),
        json!({"tool": "wait", "seconds": 0.1}),
        json!({"tool": "iphone_type_text", "text": "1"}),
    ] {
        let steps = json!({"one_gesture": true, "steps": [{"tool": "iphone_tap", "x": 1, "y": 2}, other]});
        assert!(batch_steps(&args(steps)).is_err());
    }
    // without it a batch of taps stays step by step
    let steps = batch_steps(&args(json!({"steps": [{"tool": "iphone_tap", "x": 1, "y": 2}]}))).unwrap();
    assert_eq!(
        steps,
        vec![Step::Request { tool: "iphone_tap".into(), req: Request::Tap { x: 1.0, y: 2.0, count: 1 } }]
    );
    // a step's count can't go into one gesture: an error, not one tap
    let steps = json!({"one_gesture": true, "steps": [{"tool": "iphone_tap", "x": 1, "y": 2, "count": 2}]});
    assert!(batch_steps(&args(steps)).is_err());
}

/// iphone_tap 不带 count 是点一下；count 要是 1 到 MAX_TAP_COUNT 的整数，给错了报错、不点，不悄悄改成 1
#[test]
fn tap_count_defaults_to_one_and_rejects_out_of_range() {
    let tap = |v: Value| to_request("iphone_tap", &args(v));
    assert_eq!(tap(json!({"x": 1, "y": 2})), Ok(Request::Tap { x: 1.0, y: 2.0, count: 1 }));
    assert_eq!(tap(json!({"x": 1, "y": 2, "count": 5})), Ok(Request::Tap { x: 1.0, y: 2.0, count: 5 }));
    for bad in [json!(0), json!(11), json!(2.5), json!(-1), json!("two")] {
        assert!(tap(json!({"x": 1, "y": 2, "count": bad})).is_err(), "{bad}");
    }
    // a batch step takes it too
    let steps = batch_steps(&args(json!({"steps": [{"tool": "iphone_tap", "x": 1, "y": 2, "count": 2}]})));
    let req = Request::Tap { x: 1.0, y: 2.0, count: 2 };
    assert_eq!(steps, Ok(vec![Step::Request { tool: "iphone_tap".into(), req }]));
}

#[test]
fn scroll_collection_is_bounded_and_changes_the_viewport() {
    let req = |v| {
        let args = args(v);
        check_args("iphone_scroll_collect", &args, &[], true)?;
        to_request("iphone_scroll_collect", &args)
    };
    let r = req(json!({"scroll": "up", "max_scrolls": 1, "filter": "Cell", "until": "General", "limit": 3})).unwrap();
    assert_eq!(
        r,
        Request::ScrollCollect(protocol::ScrollCollect {
            down: false,
            max_scrolls: 1,
            filter: "Cell".into(),
            until: "General".into(),
            limit: 3,
            x: None,
        })
    );
    // an iPad split view's sidebar
    let Request::ScrollCollect(scan) = req(json!({"x": 160})).unwrap() else { panic!() };
    assert_eq!(scan.x, Some(160.0));
    assert!(r.is_device_action());
    assert!(req(json!({})).is_ok());
    for bad in [
        json!({"scroll":"left"}),
        json!({"max_scrolls":0}),
        json!({"max_scrolls":16}),
        json!({"limit":0}),
        json!({"limit":151}),
        json!({"limit":2.5}),
        json!({"x":-1}),
        json!({"x":"sidebar"}),
        json!({"max_scroll":2}),
    ] {
        assert!(req(bad.clone()).is_err(), "{bad}");
    }
    let result = to_result(Response::text_data(
        "row limit reached",
        Default::default(),
        Some(protocol::Data::ScrollCollection {
            stop_reason: protocol::ScrollStop::Limit,
            scrolls: 2,
            rows: vec![protocol::ScrollRow {
                kind: "Cell".into(),
                label: "One".into(),
                name: "".into(),
                value: "".into(),
            }],
            matched: None,
        }),
    ));
    let data = result.structured_content.unwrap();
    assert_eq!(data["stop_reason"], "limit");
    assert_eq!(data["rows"][0]["label"], "One");
    assert!(data["matched"].is_null());
}

#[test]
fn alert_check_after_actions_but_not_buttons_or_reads() {
    assert!(wants_alert_check(&Request::Swipe { from: (0.0, 0.0), to: (1.0, 1.0), seconds: 0.3, hold: 0.0 }));
    assert!(wants_alert_check(&Request::LaunchApp { app: "微信".into() }));
    assert!(!wants_alert_check(&Request::PressButton { button: "home".into() }));
    assert!(!wants_alert_check(&Request::TerminateApp { app: "微信".into() }));
    assert!(!wants_alert_check(&Request::Status));
}

#[test]
fn toolkit_tools_plus_wait_for_batch_and_devices() {
    let names: Vec<_> = tools().iter().map(|t| t.name.to_string()).collect();
    assert_eq!(names.len(), 18);
    // the developer tools only with IPHONE_CONTROL_DEV_TOOLS
    let dev: Vec<_> = tools_with(true).iter().skip(18).map(|t| t.name.to_string()).collect();
    assert_eq!(dev, DEV_TOOLS);
    assert_eq!(tools_with(false).len(), 18);
    for n in [
        "iphone_status",
        "iphone_screenshot",
        "iphone_scroll_collect",
        "iphone_tap_element",
        "iphone_open_url",
        "iphone_list_devices",
        "iphone_select_device",
    ] {
        assert!(names.contains(&n.to_owned()), "{n}");
    }
    for t in tools_with(true).into_iter().filter(|t| t.name != "iphone_batch") {
        let no_required = t.input_schema.get("required").unwrap().as_array().unwrap().is_empty();
        assert_eq!(to_request(&t.name, &Map::new()).is_ok(), no_required, "{}", t.name);
    }
}

/// 注解与工具实际行为一致：看界面的只读，改界面的不是；每个参数都有说明
#[test]
fn annotations_and_param_docs() {
    for t in tools_with(true) {
        let a = t.annotations.as_ref().unwrap_or_else(|| panic!("{} 没有注解", t.name));
        assert!(a.title.is_some(), "{}", t.name);
        let probe = || {
            let props = t.input_schema["properties"].as_object().unwrap();
            let mut m = Map::new();
            for k in t.input_schema["required"].as_array().unwrap() {
                let k = k.as_str().unwrap();
                let v = match props[k]["type"].as_str() {
                    Some("number") => json!(1),
                    Some("array") => json!([{"tool": "wait", "seconds": 0}]),
                    _ if k == "button" => json!("home"),
                    _ => json!("x"),
                };
                m.insert(k.to_owned(), v);
            }
            m
        };
        let read_only = match to_request(&t.name, &probe()) {
            Ok(r) => !r.is_device_action(),
            Err(_) => false, // iphone_batch
        };
        assert_eq!(a.read_only_hint, Some(read_only), "{}", t.name);
        for (k, v) in t.input_schema["properties"].as_object().unwrap() {
            let d = v["description"].as_str().unwrap_or_default();
            assert!(!d.is_empty(), "{}.{k} 缺少说明", t.name);
        }
    }
}

#[test]
fn defaults_match_toolkit() {
    assert_eq!(
        to_request("iphone_list_elements", &Map::new()).unwrap(),
        Request::ListElements { filter: String::new(), limit: 150, max_age_ms: 0, keep_locked: false }
    );
    assert_eq!(
        to_request("iphone_swipe", &args(json!({"from_x": 1, "from_y": 2, "to_x": 3, "to_y": 4}))).unwrap(),
        Request::Swipe { from: (1.0, 2.0), to: (3.0, 4.0), seconds: 0.3, hold: 0.0 }
    );
    assert_eq!(
        to_request("iphone_long_press", &args(json!({"x": 1, "y": 2}))).unwrap(),
        Request::LongPress { x: 1.0, y: 2.0, seconds: 1.0 }
    );
    assert_eq!(
        to_request("iphone_type_text", &args(json!({"text": "设置"}))).unwrap(),
        Request::TypeText { text: "设置".into(), submit: false }
    );
}

#[test]
fn then_is_offered_on_actions_only() {
    let has_then = |name: &str| {
        let t = tools().into_iter().find(|t| t.name == name).unwrap();
        t.input_schema["properties"].get("then").is_some()
    };
    for n in
        ["iphone_tap", "iphone_tap_element", "iphone_swipe", "iphone_type_text", "iphone_launch_app", "iphone_batch"]
    {
        assert!(has_then(n), "{n}");
    }
    for n in ["iphone_status", "iphone_screenshot", "iphone_list_elements", "iphone_wait_for"] {
        assert!(!has_then(n), "{n}");
    }
    let then = |v: Value| parse_then(&args(v)).map(|o| o.then);
    assert_eq!(parse_then(&Map::new()).unwrap(), ThenOpts { then: Then::None, settle_ms: THEN_SETTLE_MS });
    assert_eq!(then(json!({"then": "elements", "then_filter": "设置"})).unwrap(), Then::Elements("设置".into()));
    assert_eq!(then(json!({"then": "screenshot"})).unwrap(), Then::Screenshot(None));
    assert_eq!(
        then(json!({"then": "screenshot", "then_region": [0, 295, 430, 460]})).unwrap(),
        Then::Screenshot(Some((0.0, 295.0, 430.0, 460.0)))
    );
    assert!(then(json!({"then": "screenshot", "then_region": [0, 1, 2]})).is_err());
    assert!(then(json!({"then": "screenshot", "then_region": [0, 1, 0, 5]})).is_err());
    assert!(then(json!({"then": "video"})).is_err());
    assert_eq!(parse_then(&args(json!({"then": "screenshot", "then_settle": 5}))).unwrap().settle_ms, 5000);
    assert!(parse_then(&args(json!({"then_settle": 30}))).is_err());
}

/// #158: an argument the tool doesn't take is refused, not dropped (a wait for disappearance ran as a wait for
/// appearance and answered at once)
#[test]
fn unknown_arguments_are_refused() {
    // an argument from another version (here: misspelt) is refused and the tool's own are listed
    let e = check_args("iphone_wait_for", &args(json!({"text": "飞行模式", "until": "disappeared"})), &[], true)
        .unwrap_err();
    assert!(e.contains("until") && e.contains("state") && e.contains("timeout"), "{e}");
    assert!(check_args("iphone_wait_for", &args(json!({"text": "x", "state": "disappeared"})), &[], true).is_ok());
    assert!(check_args("iphone_type_text", &args(json!({"text": "", "replace": true})), &[], true).is_ok());
    // misspelt
    assert!(check_args("iphone_type_text", &args(json!({"text": "a", "sumbit": true})), &[], true).is_err());
    // every documented argument, then on an action, null for a default
    assert!(
        check_args(
            "iphone_type_text",
            &args(json!({"text": "a", "submit": true, "then": "elements",
        "then_filter": "x", "then_settle": 1})),
            &[],
            true
        )
        .is_ok()
    );
    assert!(check_args("iphone_list_elements", &args(json!({"limit": null})), &[], true).is_ok());
    // then only where the schema lists it
    assert!(check_args("iphone_screenshot", &args(json!({"then": "elements"})), &[], true).is_err());
    assert!(check_args("iphone_status", &args(json!({})), &[], true).is_ok());
    assert!(check_args("iphone_batch", &args(json!({"steps": [], "then": "screenshot"})), &[], true).is_ok());
    // developer tools and the confirm tool have their own; a tool this server doesn't know is left alone
    assert!(check_args("iphone_web_eval", &args(json!({"expression": "1", "timeout": 2})), &[], true).is_ok());
    assert!(check_args(task::CONFIRM_TOOL, &args(json!({"action": "a", "detail": "b", "x": 1})), &[], false).is_err());
    assert!(check_args("no_such_tool", &args(json!({"x": 1})), &[], true).is_ok());
}

#[test]
fn batch_steps_refuse_unknown_arguments() {
    let bad = |step: Value| batch_steps(&args(json!({"steps": [step]}))).unwrap_err();
    let e = bad(json!({"tool": "iphone_wait_for", "text": "x", "until": "disappeared"}));
    assert!(e.contains("第 1 步") && e.contains("until"), "{e}");
    // a step's then would be ignored (the batch's then applies): refused, not dropped
    assert!(bad(json!({"tool": "iphone_tap", "x": 1, "y": 2, "then": "elements"})).contains("then"));
    assert!(bad(json!({"tool": "wait", "seconds": 1, "secs": 2})).contains("secs"));
}

#[test]
fn element_lists_declare_their_output_schema() {
    let tools = standard_tools();
    let declared: Vec<_> = tools.iter().filter(|t| t.output_schema.is_some()).map(|t| &*t.name).collect();
    assert_eq!(declared, ["iphone_list_elements"]);
    let schema = tools.iter().find(|t| t.name == "iphone_list_elements").and_then(|t| t.output_schema.clone());
    assert_eq!(schema.unwrap()["required"], json!(["elements"]));
}

#[test]
fn batch_steps_are_checked_before_anything_runs() {
    let steps = batch_steps(&args(json!({"steps": [
        {"tool": "iphone_tap_element", "text": "One"},
        {"tool": "wait", "seconds": 0.5},
        {"tool": "iphone_tap", "x": 1, "y": 2},
        {"tool": "iphone_wait_for", "text": "完成", "timeout": 3}
    ]})))
    .unwrap();
    assert_eq!(steps.len(), 4);
    assert_eq!(steps[1], Step::Wait(std::time::Duration::from_millis(500)));
    assert_eq!(
        steps[3],
        Step::Request {
            tool: "iphone_wait_for".into(),
            req: Request::WaitFor { text: "完成".into(), timeout_ms: 3000 }
        }
    );
    // 第 2 步参数错：一步都不执行，报出是第几步
    let e =
        batch_steps(&args(json!({"steps": [{"tool": "iphone_tap", "x": 1, "y": 2}, {"tool": "iphone_tap", "x": 1}]})))
            .unwrap_err();
    assert!(e.contains("第 2 步"), "{e}");
    // 看界面的工具、未知工具、批量套批量都不行
    for bad in ["iphone_screenshot", "iphone_batch", "rm"] {
        assert!(batch_steps(&args(json!({"steps": [{"tool": bad}]}))).is_err(), "{bad}");
    }
    assert!(batch_steps(&args(json!({"steps": [{"tool": "wait", "seconds": 9}]}))).is_err());
    assert!(batch_steps(&args(json!({"steps": []}))).is_err());
    let many: Vec<Value> = (0..21).map(|_| json!({"tool": "wait", "seconds": 0})).collect();
    assert!(batch_steps(&args(json!({"steps": many}))).is_err());
}

#[test]
fn reuse_elements_only_affects_tap_element_steps() {
    let steps = |reuse: bool| {
        batch_steps(&args(json!({"reuse_elements": reuse, "steps": [
            {"tool": "iphone_tap_element", "text": "One"},
            {"tool": "iphone_tap", "x": 1, "y": 2}
        ]})))
        .unwrap()
    };
    assert_eq!(
        steps(true)[0],
        Step::Request {
            tool: "iphone_tap_element".into(),
            req: Request::TapElement { text: "One".into(), index: 0, max_age_ms: BATCH_REUSE_MS, scroll: None }
        }
    );
    assert!(matches!(&steps(false)[0], Step::Request { req: Request::TapElement { max_age_ms: 0, .. }, .. }));
    assert_eq!(steps(true)[1], steps(false)[1]);
}

#[test]
fn missed_tap_texts_are_recognised() {
    // 旧版本守护进程：没有 outcome，只能认中文开头
    assert!(tap_missed("没找到包含 \"x\" 的元素,先用 iphone_list_elements 或截图看看屏幕", None));
    assert!(tap_missed("只匹配到 1 个:\nButton \"x\"", None));
    assert!(!tap_missed("已点击 Button \"x\"", None));
    assert!(tap_missed("被挡住:Button \"x\" 整个被「浮层」盖住,没有点", None));
    assert!(!tap_missed("已点击 Button \"x\";它的中心被「搜索」盖住,改点没被盖住的 (215,848)", None));
    // 新版本：看 outcome，与语言无关
    let missed = protocol::Outcome { tapped_nothing: true, ..Default::default() };
    assert!(tap_missed("Covered: Button \"x\" is completely covered", Some(missed)));
    assert!(!tap_missed("Tapped Button \"Not found\"", Some(protocol::Outcome::default())));
}

#[test]
fn wait_for_timeout_is_bounded() {
    assert_eq!(
        to_request("iphone_wait_for", &args(json!({"text": "x"}))).unwrap(),
        Request::WaitFor { text: "x".into(), timeout_ms: 5000 }
    );
    assert!(to_request("iphone_wait_for", &args(json!({"text": "x", "timeout": 60}))).is_err());
}

#[test]
fn text_replacement_is_forwarded_only_when_requested() {
    let changed = to_request("iphone_type_text", &args(json!({"text": "蓝牙", "replace": true}))).unwrap();
    assert_eq!(serde_json::to_value(changed).unwrap()["op"], "replace_text");
    let default = to_request("iphone_type_text", &args(json!({"text": "蓝牙"}))).unwrap();
    assert_eq!(serde_json::to_value(default).unwrap()["op"], "type_text");
}

#[test]
fn wait_for_accepts_disappeared_and_rejects_unknown_state() {
    let req = to_request("iphone_wait_for", &args(json!({"text": "设置", "state": "disappeared"}))).unwrap();
    assert_eq!(serde_json::to_value(req).unwrap()["op"], "wait_for_disappearance");
    assert!(to_request("iphone_wait_for", &args(json!({"text": "设置", "state": "any"}))).is_err());
}

#[test]
fn element_limit_is_reported_in_structured_content() {
    let reply: Response = serde_json::from_value(json!({
        "kind": "text", "text": "one element, 16 more",
        "data": {
            "type": "elements",
            "elements": [{"type":"Button","label":"设置","x":16,"y":16,"width":44,"height":44}],
            "matched_count": 17, "truncated": true
        }
    }))
    .unwrap();
    let result = serde_json::to_value(to_result(reply)).unwrap();
    assert_eq!(result["structuredContent"]["elements"].as_array().unwrap().len(), 1);
    assert_eq!(result["structuredContent"]["matched_count"], 17);
    assert_eq!(result["structuredContent"]["truncated"], true);
}

#[test]
fn wrong_typed_parameters_are_errors_not_defaults() {
    let args = |v: Value| v.as_object().unwrap().clone();
    assert!(to_request("iphone_tap_element", &args(json!({"text": "设置", "index": -1}))).is_err());
    assert!(to_request("iphone_list_elements", &args(json!({"limit": 1.5}))).is_err());
    assert!(to_request("iphone_swipe", &args(json!({"from_x": 1, "from_y": 2, "to_x": "x", "to_y": 4}))).is_err());
    // 写成字符串的数字照收（toolkit 的 pydantic 也收）
    assert_eq!(
        to_request("iphone_tap_element", &args(json!({"text": "设置", "index": "2"}))).unwrap(),
        Request::TapElement { text: "设置".into(), index: 2, max_age_ms: 0, scroll: None }
    );
    assert_eq!(
        to_request("iphone_list_elements", &args(json!({"limit": null}))).unwrap(),
        Request::ListElements { filter: String::new(), limit: 150, max_age_ms: 0, keep_locked: false }
    );
}

#[test]
fn screenshot_region_needs_all_four() {
    assert_eq!(to_request("iphone_screenshot", &Map::new()).unwrap(), Request::Screenshot { region: None });
    assert_eq!(
        to_request("iphone_screenshot", &args(json!({"x": 0, "y": 55, "width": 430, "height": 40}))).unwrap(),
        Request::Screenshot { region: Some((0.0, 55.0, 430.0, 40.0)) }
    );
    assert_eq!(
        to_request("iphone_screenshot", &args(json!({"x": 10, "y": 10}))).unwrap_err(),
        "放大区域要同时给出 x、y、width、height"
    );
}

/// 回归用例 #17：预期内的错误要作为工具错误、带可读文字返回
#[test]
fn errors_become_tool_errors_with_text() {
    let r = to_result(Response::error("button 只能是 home / volume_up / volume_down / lock"));
    assert_eq!(r.is_error, Some(true));
    assert!(matches!(&r.content[0], ContentBlock::Text(t) if t.text.contains("button 只能是")));
    let img = to_result(Response::Image { jpeg_base64: "AAAA".into(), caption: "430x932 点".into() });
    assert_eq!(img.content.len(), 2);
    assert!(to_request("iphone_nope", &Map::new()).is_err());
}
