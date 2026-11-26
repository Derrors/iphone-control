use std::time::Duration;

use serde_json::Value;

use super::scope::{SAFARI, SPRINGBOARD, out_of_scope};
use super::store::read_events_prefix;
use super::task::gesture;
use super::*;

const PHONE: &str = "00008150-000A";

async fn log_of(t: &Task) -> (Vec<TaskEvent>, bool) {
    let sub = t.subscribe().await;
    (sub.events.into_iter().map(|(_, e)| e).collect(), sub.ended)
}

fn tasks() -> Tasks {
    Tasks::load(None)
}

fn open(ts: &Tasks) -> Arc<Task> {
    ts.open("看看设置", "test", Some(PHONE), Vec::new(), None).unwrap()
}

fn tap() -> Request {
    Request::Tap { x: 1.0, y: 2.0, count: 1 }
}

async fn admit(ts: &Tasks, req: &Request, token: Option<&str>) -> Result<(Admission, bool), Response> {
    ts.admit(req, token, &mut None, &mut None).await
}

fn message(r: Result<(Admission, bool), Response>) -> String {
    match r {
        Err(Response::Error { message, .. }) => message,
        Err(other) => panic!("{other:?}"),
        Ok(_) => panic!("admitted"),
    }
}

#[tokio::test]
async fn without_a_task_everything_goes_as_before() {
    let ts = tasks();
    assert!(matches!(admit(&ts, &tap(), None).await, Ok((Admission::Free, false))));
    let select = Request::SelectDevice { device: "auto".into() };
    assert!(matches!(admit(&ts, &select, None).await, Ok((Admission::Free, _))));
}

#[tokio::test]
async fn only_one_task_and_only_on_a_device() {
    let ts = tasks();
    assert!(ts.open("x", "", None, Vec::new(), None).is_err());
    assert!(ts.open(" ", "", Some(PHONE), Vec::new(), None).is_err());
    let t = open(&ts);
    assert!(ts.open("another", "", Some(PHONE), Vec::new(), None).err().unwrap().to_string().contains(&t.id));
    t.end(End::Completed, "");
    assert!(ts.open("another", "", Some(PHONE), Vec::new(), None).is_ok());
}

/// The point of D17: while a task runs, a caller without its token can look but not act
#[tokio::test]
async fn others_may_look_but_not_act_until_the_user_takes_over() {
    let ts = tasks();
    let t = open(&ts);
    assert!(message(admit(&ts, &tap(), None).await).contains(&t.id));
    assert!(admit(&ts, &Request::Resign { force: true }, None).await.is_err());
    let shot = Request::Screenshot { region: None };
    assert!(matches!(admit(&ts, &shot, None).await, Ok((Admission::Free, _))));
    let up = Request::Touch { phase: TouchPhase::Up, x: 0.0, y: 0.0 };
    assert!(admit(&ts, &up, None).await.is_ok(), "a finger that was down can always be lifted");
    let prepare = Request::Touch { phase: TouchPhase::Prepare, x: 0.0, y: 0.0 };
    assert!(matches!(admit(&ts, &prepare, None).await, Ok((Admission::Free, _))), "getting ready touches nothing");
    let select = Request::SelectDevice { device: "auto".into() };
    assert!(admit(&ts, &select, None).await.is_err());

    t.control("takeover", None, false, None).unwrap();
    assert!(matches!(admit(&ts, &tap(), None).await, Ok((Admission::Supervisor(_), _))));
    assert!(admit(&ts, &select, None).await.is_err(), "not even while taken over");
}

#[tokio::test]
async fn the_task_goes_to_its_own_device() {
    let ts = tasks();
    let t = open(&ts);
    let mut udid = Some("someone-else".to_owned());
    let r = ts.admit(&tap(), Some(&t.token), &mut udid, &mut None).await;
    assert!(matches!(r, Ok((Admission::Actor(_), false))));
    assert_eq!(udid.as_deref(), Some(PHONE));
    let select = Request::SelectDevice { device: "auto".into() };
    assert!(admit(&ts, &select, Some(&t.token)).await.is_err());
}

#[tokio::test]
async fn a_paused_task_holds_its_requests_until_resumed() {
    let ts = Arc::new(tasks());
    let t = open(&ts);
    t.control("pause", None, false, None).unwrap();
    let (ts2, token) = (ts.clone(), t.token.clone());
    let held = tokio::spawn(async move { admit(&ts2, &tap(), Some(&token)).await.map(|(_, w)| w) });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!held.is_finished());
    t.control("resume", None, false, None).unwrap();
    assert!(matches!(held.await.unwrap(), Ok(true)), "let through, and it says it waited");
}

#[tokio::test]
async fn a_held_request_whose_caller_left_is_dropped() {
    let ts = tasks();
    let t = open(&ts);
    t.control("pause", None, false, None).unwrap();
    let (tx, rx) = oneshot::channel();
    tx.send(()).unwrap();
    let r = ts.admit(&tap(), Some(&t.token), &mut None, &mut Some(rx)).await;
    assert!(message(r).contains("取消"));
}

/// The page getting ready for a touch isn't an action: let through while the task runs, and while the user has
/// taken over it doesn't count as the user acting (a [`Admission::Supervisor`])
#[tokio::test]
async fn getting_ready_for_a_touch_is_not_acting() {
    let ts = tasks();
    let t = open(&ts);
    let prepare = Request::Touch { phase: TouchPhase::Prepare, x: 0.0, y: 0.0 };
    t.control("takeover", None, false, None).unwrap();
    assert!(matches!(admit(&ts, &prepare, None).await, Ok((Admission::Free, _))));
    assert!(matches!(admit(&ts, &tap(), None).await, Ok((Admission::Supervisor(_), _))), "a tap is acting");
    assert!(!prepare.is_device_action());
    assert!(Request::Touch { phase: TouchPhase::Down, x: 0.0, y: 0.0 }.is_device_action());
}

#[tokio::test]
async fn after_someone_else_acted_the_agent_looks_before_acting() {
    let ts = tasks();
    let t = open(&ts);
    t.control("takeover", None, false, None).unwrap();
    assert!(admit(&ts, &tap(), None).await.is_ok());
    t.control("handback", None, false, None).unwrap();
    assert!(message(admit(&ts, &tap(), Some(&t.token)).await).contains("重新看"));
    assert!(admit(&ts, &tap(), Some(&t.token)).await.is_err(), "until it has looked, not just once");
    assert!(admit(&ts, &Request::Status, Some(&t.token)).await.is_ok(), "status isn't acting");
    let look = Request::ListElements { filter: String::new(), limit: 10, max_age_ms: 0, keep_locked: false };
    assert!(admit(&ts, &look, Some(&t.token)).await.is_ok());
    assert!(admit(&ts, &tap(), Some(&t.token)).await.is_err(), "a look let through isn't a look that worked");
    t.observed();
    assert!(admit(&ts, &tap(), Some(&t.token)).await.is_ok());
}

/// A request let through before the user paused, took over or handed back is checked again once it holds its lock
#[tokio::test]
async fn an_admitted_request_is_checked_again_once_it_has_its_lock() {
    let ts = tasks();
    let t = open(&ts);
    let (agent, _) = admit(&ts, &tap(), Some(&t.token)).await.unwrap();
    assert!(matches!(ts.still(&agent, &tap()), Still::Admitted));
    t.control("pause", None, false, None).unwrap();
    assert!(matches!(ts.still(&agent, &tap()), Still::Wait(_)), "waits for the task to run again");
    t.control("takeover", None, false, None).unwrap();
    assert!(matches!(ts.still(&agent, &tap()), Still::Wait(_)));
    let (user, _) = admit(&ts, &tap(), None).await.unwrap();
    assert!(matches!(user, Admission::Supervisor(_)));
    t.control("handback", None, false, None).unwrap();
    assert!(matches!(ts.still(&user, &tap()), Still::Refused(_)), "the user's tap after handing back");
    assert!(matches!(ts.still(&agent, &tap()), Still::Refused(_)), "the agent looks first");
    let shot = Request::Screenshot { region: None };
    assert!(matches!(ts.still(&agent, &shot), Still::Admitted));
    t.end(End::Cancelled, "");
    assert!(matches!(ts.still(&agent, &shot), Still::Refused(_)));
    assert!(matches!(ts.still(&user, &tap()), Still::Admitted), "no task any more");
}

/// The agent may not answer its own questions or resume itself; it may give up
#[tokio::test]
async fn the_agent_cant_control_its_own_task() {
    let ts = tasks();
    let me = std::process::id();
    // this test process stands in for the agent: its parent opened the task
    let opener = crate::procs::parent(me).expect("the test has a parent");
    let t = ts.open("看看设置", "test", Some(PHONE), Vec::new(), Some(opener)).unwrap();
    let control = |action: &str, for_pid: Option<u32>| Request::TaskControl {
        id: t.id.clone(),
        action: action.into(),
        step: None,
        approve: false,
        by: Some("cli".into()),
        for_pid,
    };
    let refused = |r: Option<Response>| matches!(r, Some(Response::Error { .. }));
    let pause = |token: Option<&'static str>, peer: Option<u32>, for_pid: Option<u32>| {
        let (ts, req) = (&ts, control("pause", for_pid));
        let token = token.map(|_| t.token.clone());
        async move { ts.handle(&req, token.as_deref(), peer, &mut None).await }
    };
    let agent = |from: Option<u32>| {
        let (ts, token) = (&ts, t.token.clone());
        async move { ts.handle(&Request::TaskAgent, Some(&token), from, &mut None).await }
    };

    // with the task's token (the CLI in the agent's environment sends it)
    assert!(refused(pause(Some("token"), None, None).await));
    // before the opener says its children are the agent, they are anyone
    assert!(!refused(pause(None, Some(me), None).await));
    assert!(!refused(ts.handle(&control("resume", None), None, Some(me), &mut None).await));
    // only the opener says so
    assert!(refused(agent(Some(me)).await));
    assert!(refused(agent(None).await));
    assert!(!refused(agent(Some(opener)).await));
    assert!(refused(pause(None, Some(me), None).await), "the opener's child");
    assert!(refused(pause(None, None, Some(me)).await), "passed on for it");
    assert_eq!(t.info().state, "running");
    // the opener itself (the page passing on the user's click), or someone else (the user's own terminal)
    assert!(!refused(pause(None, Some(opener), None).await));
    assert!(!refused(ts.handle(&control("resume", None), None, Some(1), &mut None).await));
    let by: Vec<_> = log_of(&t).await.0.into_iter().filter(|e| e.event == "control").map(|e| e.data).collect();
    assert_eq!(by.len(), 4);
    assert_eq!((by[0]["action"].as_str(), by[0]["by"].as_str()), (Some("pause"), Some("cli")));
    // giving up is allowed from anywhere
    assert!(!refused(ts.handle(&control("stop", None), Some(&t.token), Some(me), &mut None).await));
    assert_eq!(t.info().state, "cancelled");
}

/// What lets an agent on another machine use the viewer's `/mcp` by its task's token alone
#[tokio::test]
async fn a_task_s_token_is_checked_while_it_is_active() {
    let ts = tasks();
    let t = open(&ts);
    let check = |token: Option<String>| {
        let ts = &ts;
        async move { ts.handle(&Request::TaskCheck, token.as_deref(), None, &mut None).await.unwrap() }
    };
    assert!(matches!(check(Some(t.token.clone())).await, Response::Text { text, .. } if text == t.id));
    assert!(matches!(check(Some("nobody's".into())).await, Response::Error { .. }));
    assert!(matches!(check(None).await, Response::Error { .. }));
    t.control("stop", None, false, None).unwrap();
    assert!(matches!(check(Some(t.token.clone())).await, Response::Error { .. }), "ended");
}

#[tokio::test]
async fn an_ended_or_unknown_token_is_refused_for_everything() {
    let ts = Arc::new(tasks());
    let t = open(&ts);
    t.control("pause", None, false, None).unwrap();
    let (ts2, token) = (ts.clone(), t.token.clone());
    let held = tokio::spawn(async move { admit(&ts2, &Request::Status, Some(&token)).await.map(|_| ()) });
    tokio::time::sleep(Duration::from_millis(20)).await;
    t.control("stop", None, false, None).unwrap();
    assert!(held.await.unwrap().is_err(), "a held request is released with a refusal");
    assert!(message(admit(&ts, &Request::Status, Some(&t.token)).await).contains("结束"));
    assert!(message(admit(&ts, &Request::Status, Some("nobody")).await).contains("结束"));
    assert!(
        ts.handle(&Request::TaskNote { text: "x".into() }, Some(&t.token), None, &mut None)
            .await
            .is_some_and(|r| matches!(r, Response::Error { .. }))
    );
    assert!(matches!(admit(&ts, &tap(), None).await, Ok((Admission::Free, _))), "the phone is free again");
}

#[tokio::test]
async fn a_rejected_confirmation_ends_the_task_as_rejected() {
    let ts = Arc::new(tasks());
    let t = open(&ts);
    let (ts2, token) = (ts.clone(), t.token.clone());
    let waiting = tokio::spawn(async move {
        let req = Request::TaskConfirm { action: "发送消息".into(), detail: "给妈妈".into() };
        ts2.handle(&req, Some(&token), None, &mut None).await.unwrap()
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(t.info().state, "waiting");
    assert!(t.control("confirm", Some(99), false, None).is_err(), "another step");
    t.control("confirm", Some(1), false, None).unwrap();
    assert!(matches!(waiting.await.unwrap(), Response::Error { message, .. } if message.contains("拒绝")));
    assert_eq!(t.info().state, "running");
    t.finish(true, "done");
    assert_eq!(t.info().state, "rejected");
}

fn ask(ts: &Arc<Tasks>, t: &Task) -> tokio::task::JoinHandle<Response> {
    let (ts, token) = (ts.clone(), t.token.clone());
    tokio::spawn(async move {
        let req = Request::TaskConfirm { action: "发送消息".into(), detail: "给妈妈".into() };
        ts.handle(&req, Some(&token), None, &mut None).await.unwrap()
    })
}

/// A question doesn't replace the user's pause or takeover: it waits until the task runs again
#[tokio::test]
async fn a_question_waits_while_the_user_paused_or_took_over() {
    let ts = Arc::new(tasks());
    let t = open(&ts);
    t.control("pause", None, false, None).unwrap();
    let asked = ask(&ts, &t);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(t.info().state, "paused");
    t.control("takeover", None, false, None).unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(t.info().state, "takeover");
    assert!(admit(&ts, &tap(), None).await.is_ok(), "the user still acts while taken over");
    t.control("handback", None, false, None).unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(t.info().state, "waiting");
    t.control("confirm", None, true, None).unwrap();
    assert!(matches!(asked.await.unwrap(), Response::Text { .. }));
    assert_eq!(t.info().state, "running");
}

/// `done` is the last event: a step under way is stopped before it, and nothing that comes after is written
#[tokio::test]
async fn nothing_follows_done() {
    let ts = Arc::new(tasks());
    let t = open(&ts);
    let asked = ask(&ts, &t);
    tokio::time::sleep(Duration::from_millis(20)).await;
    t.end(End::Cancelled, "stopped");
    let _ = asked.await.unwrap();
    t.observed();
    let (log, ended) = log_of(&t).await;
    assert!(ended);
    let names: Vec<&str> = log.iter().map(|e| e.event.as_str()).collect();
    assert_eq!(names.last(), Some(&"done"), "{names:?}");
    let stopped = &log[log.len() - 2];
    assert_eq!((stopped.event.as_str(), stopped.data["status"].as_str()), ("step", Some("stopped")));
}

/// A tool call asking to leave the task's scope is two steps under way (the call, and the question); one the
/// agent made alongside is a third. Stopping the task ends them all before `done`, and the call's result coming in
/// after it is not written
#[tokio::test]
async fn every_step_under_way_is_stopped_before_done() {
    let ts = Arc::new(tasks());
    let t = ts.open("改设置", "test", Some(PHONE), scope(&["com.apple.Preferences"]), None).unwrap();
    let req = |tool: &str| Request::TaskStep { tool: tool.into(), args: json!({}) };
    let r = ts.handle(&req("iphone_screenshot"), Some(&t.token), None, &mut None).await;
    assert_eq!(r, Some(Response::text("1")));
    let r = ts.handle(&req("iphone_launch_app"), Some(&t.token), None, &mut None).await;
    assert_eq!(r, Some(Response::text("2")));
    let asking = {
        let t = t.clone();
        tokio::spawn(
            async move { t.check_scope(&Target::Launch("com.apple.MobileSMS".into()), Some(2), &mut None).await },
        )
    };
    while t.info().state != "waiting" {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    t.end(End::Cancelled, "stopped");
    assert!(asking.await.unwrap().is_err());
    t.step_done(2, false, 5, "refused", None);
    t.step_done(1, true, 5, "late", None);
    let (log, _) = log_of(&t).await;
    assert_eq!(log.last().map(|e| e.event.as_str()), Some("done"));
    let last_status =
        |id: u64| log.iter().rev().find(|e| e.event == "step" && e.data["id"] == id).map(|e| e.data["status"].clone());
    for id in 1..=3 {
        assert_eq!(last_status(id), Some(json!("stopped")), "step {id}: {log:?}");
    }
}

/// While a step waits for the user, they can take over (to look or change something first): the question stays,
/// answered under takeover (the task stays with the user) or after handing back (it waits again)
#[tokio::test]
async fn the_user_can_take_over_while_a_step_waits_for_them() {
    let ts = Arc::new(tasks());
    let t = open(&ts);
    let asked = ask(&ts, &t);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(t.info().state, "waiting");
    t.control("takeover", None, false, None).unwrap();
    assert_eq!(t.info().state, "takeover");
    assert!(matches!(admit(&ts, &tap(), None).await, Ok((Admission::Supervisor(_), _))));
    t.control("confirm", None, true, None).unwrap();
    assert!(matches!(asked.await.unwrap(), Response::Text { .. }));
    assert_eq!(t.info().state, "takeover", "answered, but still the user's until handed back");
    t.control("handback", None, false, None).unwrap();
    assert_eq!(t.info().state, "running");

    // handed back before answering: the question is still there
    let asked = ask(&ts, &t);
    tokio::time::sleep(Duration::from_millis(20)).await;
    t.control("takeover", None, false, None).unwrap();
    t.control("handback", None, false, None).unwrap();
    assert_eq!(t.info().state, "waiting");
    t.control("confirm", None, false, None).unwrap();
    assert!(matches!(asked.await.unwrap(), Response::Error { .. }), "rejected");
}

/// Two questions at once are asked one after the other, each answered on its own
#[tokio::test]
async fn two_questions_at_once_are_asked_in_turn() {
    let ts = Arc::new(tasks());
    let t = open(&ts);
    let (a, b) = (ask(&ts, &t), ask(&ts, &t));
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(t.info().state, "waiting");
    t.control("confirm", None, true, None).unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(t.info().state, "waiting", "the second question, now");
    t.control("confirm", None, true, None).unwrap();
    assert!(matches!(a.await.unwrap(), Response::Text { .. }));
    assert!(matches!(b.await.unwrap(), Response::Text { .. }));
    assert_eq!(t.info().state, "running");
    t.finish(true, "done");
    assert_eq!(t.info().state, "completed", "nobody said no");
}

#[tokio::test]
async fn steps_get_thoughts_results_and_marks() {
    let ts = tasks();
    let t = open(&ts);
    let req = |tool: &str, args: Value| Request::TaskStep { tool: tool.into(), args };
    t.thought("先点「通用」");
    let r = ts.handle(&req("iphone_tap_element", json!({"text": "通用"})), Some(&t.token), None, &mut None).await;
    assert_eq!(r, Some(Response::text("1")));
    t.step_done(1, true, 120, "已点击「通用」 @(200,300) 100x40", None);
    // a thought arriving just after its step started joins it
    let r = ts.handle(&req("iphone_tap", json!({"x": 5, "y": 6})), Some(&t.token), None, &mut None).await;
    assert_eq!(r, Some(Response::text("2")));
    t.thought("点一下");
    let n = ts.auto_step(&t, &Request::Swipe { from: (1.0, 2.0), to: (3.0, 4.0), seconds: 0.3, hold: 0.0 });
    assert_eq!(n, 3);
    let (log, _) = log_of(&t).await;
    let steps: Vec<&Value> = log.iter().filter(|e| e.event == "step").map(|e| &e.data).collect();
    assert_eq!(steps[0]["thought"], "先点「通用」");
    assert_eq!(steps[1]["gesture"], json!({"kind": "tap", "x": 200.0, "y": 300.0}));
    assert_eq!(steps[2]["gesture"]["kind"], "tap");
    assert_eq!(steps[3]["thought"], "点一下");
    assert_eq!(steps[4]["tool"], "swipe");
    assert_eq!(steps[4]["gesture"]["x2"], 3.0);
}

/// A result that lists elements is full of `@(x,y)`: only a tap by text is marked from what its result says
#[tokio::test]
async fn only_a_tap_by_text_gets_a_mark_from_its_result() {
    let listed = "Button \"通用\" @(200,300) 100x40\nButton \"蓝牙\" @(200,350) 100x40";
    assert_eq!(gesture("iphone_list_elements", &json!({}), listed), None);
    assert_eq!(gesture("iphone_wait_for", &json!({"text": "通用"}), "已出现 Button \"通用\" @(200,300) 100x40"), None);
    assert_eq!(gesture("iphone_tap_element", &json!({"text": "x"}), "没找到包含 \"x\" 的元素"), None);
    let ts = tasks();
    let t = open(&ts);
    let req = |tool: &str, args: Value| Request::TaskStep { tool: tool.into(), args };
    ts.handle(&req("iphone_list_elements", json!({})), Some(&t.token), None, &mut None).await;
    t.step_done(1, true, 80, listed, None);
    ts.handle(&req("iphone_tap_element", json!({"text": "蓝牙"})), Some(&t.token), None, &mut None).await;
    t.step_done(2, true, 90, "已点击 Button \"蓝牙\" @(200,350) 100x40", None);
    let (log, _) = log_of(&t).await;
    let done: Vec<&Value> =
        log.iter().filter(|e| e.event == "step" && e.data.get("ms").is_some()).map(|e| &e.data).collect();
    assert!(done[0].get("gesture").is_none(), "{}", done[0]);
    assert_eq!(done[1]["gesture"], json!({"kind": "tap", "x": 200.0, "y": 350.0}));
}

#[tokio::test]
async fn another_device_interrupts_the_task() {
    let ts = tasks();
    let t = open(&ts);
    ts.device_is(None);
    ts.device_is(Some(&PHONE.to_lowercase()));
    assert!(t.is_active(), "unplugged, or the same phone: the task waits");
    ts.device_is(Some("00008150-000B"));
    assert_eq!(t.info().state, "interrupted");
}

/// A task with a file holds none of its events in memory (the daemon keeps 50 tasks, each log of any size): they
/// are read from the file, the same and in the same order as they were sent. Once writing the file fails, the
/// events are kept in memory from then on, those before included
#[tokio::test]
async fn a_task_s_events_are_kept_on_disk_not_in_memory() {
    use std::os::unix::fs::PermissionsExt;
    // root writes a read-only file all the same
    if crate::protocol::running_as_root() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("ictl-tasks-disk-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let ts = Tasks::load(Some(dir.clone()));
    let t = open(&ts);
    let mut rx = t.subscribe().await.rx;
    t.thought("看一下");
    t.step_done(1, true, 5, "ok", None);
    assert!(t.lock().log.is_empty() && t.lock().on_disk);
    let mut sent = Vec::new();
    while let Ok((_, e)) = rx.try_recv() {
        sent.push(e);
    }
    let (log, _) = log_of(&t).await;
    assert_eq!(log.first().map(|e| e.event.as_str()), Some("task"));
    assert_eq!(log[1..], sent[..], "what was sent, from the file");

    // the file can't be written any more: from here on in memory, with what came before
    let file = t.file.clone().unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o444)).unwrap();
    t.end(End::Completed, "好了");
    let (after, ended) = log_of(&t).await;
    assert!(ended && !t.lock().on_disk);
    assert_eq!(after[..log.len()], log[..]);
    assert_eq!(after.last().map(|e| e.event.as_str()), Some("done"));
    std::fs::remove_dir_all(dir).unwrap();
}

/// A log cut short in the middle of its last line (a power cut) loses that half line when it is loaded: the task,
/// interrupted then, ends with its `done`, read back like every other event, instead of glued to the half line
#[tokio::test]
async fn a_log_cut_short_still_ends_with_its_end() {
    let dir = std::env::temp_dir().join(format!("ictl-tasks-cut-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let ts = Tasks::load(Some(dir.clone()));
    let t = open(&ts);
    t.step_done(1, true, 5, "ok", None);
    let file = t.file.clone().unwrap();
    drop(ts);
    let mut f = std::fs::OpenOptions::new().append(true).open(&file).unwrap();
    std::io::Write::write_all(&mut f, br#"{"t":1,"event":"step","data":{"id":2,"sta"#).unwrap();
    drop(f);
    let ts = Tasks::load(Some(dir.clone()));
    let (log, ended) = log_of(&ts.get(&t.id).unwrap()).await;
    assert!(ended);
    let names: Vec<&str> = log.iter().map(|e| e.event.as_str()).collect();
    assert_eq!(names, ["task", "step", "done"], "{log:?}");
    assert_eq!(log[2].data["outcome"], "interrupted");
    assert!(std::fs::read_to_string(&file).unwrap().ends_with('\n'));
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn logs_survive_a_restart_and_unfinished_tasks_are_interrupted() {
    let dir = std::env::temp_dir().join(format!("ictl-tasks-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let ts = Tasks::load(Some(dir.clone()));
    let done = open(&ts);
    done.thought("看一下");
    done.step_done(1, true, 5, "ok", None);
    done.end(End::Completed, "好了");
    let running = open(&ts);
    drop(ts);

    let ts = Tasks::load(Some(dir.clone()));
    let infos = ts.infos();
    assert_eq!(infos.len(), 2);
    assert_eq!((infos[0].id.as_str(), infos[0].state.as_str()), (done.id.as_str(), "completed"));
    assert_eq!((infos[1].id.as_str(), infos[1].state.as_str()), (running.id.as_str(), "interrupted"));
    assert!(ts.active().is_none());
    assert!(ts.by_token(&running.token).is_none(), "the old token means nothing now");
    let (log, ended) = log_of(&ts.get(&running.id).unwrap()).await;
    assert!(ended && log.last().unwrap().data["outcome"] == "interrupted");
    // and it stays that way on the next start
    let ts = Tasks::load(Some(dir.clone()));
    let (log, _) = log_of(&ts.get(&running.id).unwrap()).await;
    assert_eq!(log.iter().filter(|e| e.event == "done").count(), 1);
    std::fs::remove_dir_all(dir).unwrap();
}

/// Tasks opened within the same millisecond (a fast machine, a test) still come back in the order they were opened
#[test]
fn tasks_keep_their_order_across_a_restart_even_within_a_millisecond() {
    let dir = std::env::temp_dir().join(format!("ictl-tasks-order-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let ts = Tasks::load(Some(dir.clone()));
    let ids: Vec<String> = (0..8)
        .map(|_| {
            let t = open(&ts);
            t.end(End::Completed, "");
            t.id.clone()
        })
        .collect();
    let started: Vec<u64> = ts.infos().iter().map(|t| t.started).collect();
    assert!(started.windows(2).all(|w| w[0] < w[1]), "{started:?}");
    drop(ts);
    let again: Vec<String> = Tasks::load(Some(dir.clone())).infos().into_iter().map(|t| t.id).collect();
    assert_eq!(again, ids);
    std::fs::remove_dir_all(dir).unwrap();
}

fn scope(apps: &[&str]) -> Vec<String> {
    apps.iter().map(|a| a.to_string()).collect()
}

#[test]
fn what_each_request_checks_against_the_scope() {
    assert_eq!(scope_check(&tap()), Some(Check::Foreground));
    assert_eq!(scope_check(&Request::LaunchApp { app: "设置".into() }), Some(Check::Launch("设置".into())));
    assert_eq!(scope_check(&Request::OpenUrl { url: "tel:10086".into() }), Some(Check::Url("tel:10086".into())));
    assert!(matches!(scope_check(&Request::UninstallApp { app: "x".into() }), Some(Check::Always { .. })));
    let eval = Request::WebEval { target: "1".into(), expression: "1".into(), timeout_ms: 1000 };
    assert_eq!(scope_check(&eval), Some(Check::Page), "the page's app, not the one in front");
    // the ways back after a refusal, and looking, are never held
    assert_eq!(scope_check(&Request::PressButton { button: "home".into() }), None);
    assert_eq!(scope_check(&Request::TerminateApp { app: "x".into() }), None);
    assert_eq!(scope_check(&Request::Screenshot { region: None }), None);
    assert_eq!(scope_check(&Request::Touch { phase: TouchPhase::Up, x: 0.0, y: 0.0 }), None);
}

#[test]
fn what_is_outside_the_scope() {
    let prefs = scope(&["com.apple.Preferences"]);
    let fg = |b: &str| Target::Foreground(b.into());
    assert_eq!(out_of_scope(&[], &fg("com.tencent.xin")), None, "no scope: no limit");
    assert_eq!(out_of_scope(&prefs, &fg("com.apple.preferences")), None, "bundle IDs ignore case");
    assert_eq!(out_of_scope(&prefs, &fg(SPRINGBOARD)), None, "the home screen is always fine");
    let (_, _, adds) = out_of_scope(&prefs, &fg("com.tencent.xin")).unwrap();
    assert_eq!(adds.as_deref(), Some("com.tencent.xin"));
    let (_, _, adds) = out_of_scope(&prefs, &fg("")).unwrap();
    assert_eq!(adds, None, "an unknown app is asked about, never let in");
    assert_eq!(out_of_scope(&prefs, &Target::Launch("com.apple.Preferences".into())), None);
    assert!(out_of_scope(&prefs, &Target::Launch("com.apple.MobileSMS".into())).is_some());
    // links: http(s) is Safari; other schemes open who knows what
    let web = Target::Url("https://example.com".into());
    assert_eq!(out_of_scope(&prefs, &web).unwrap().2.as_deref(), Some(SAFARI));
    assert_eq!(out_of_scope(&scope(&["com.apple.Preferences", SAFARI]), &web), None);
    let tel = Target::Url("tel:10086".into());
    assert_eq!(out_of_scope(&scope(&[SAFARI]), &tel).unwrap().2, None);
    let always = Target::Always { action: "卸载".into(), detail: "x".into() };
    assert!(out_of_scope(&prefs, &always).is_some());
}

async fn answer_soon(t: &Arc<Task>, approve: bool) {
    let t = t.clone();
    tokio::spawn(async move {
        while t.info().state != "waiting" {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        t.control("confirm", None, approve, None).unwrap();
    });
}

#[tokio::test]
async fn an_approved_app_joins_the_scope_for_the_rest_of_the_task() {
    let ts = tasks();
    let t = ts.open("改设置", "test", Some(PHONE), scope(&["com.apple.Preferences"]), None).unwrap();
    assert!(t.scoped());
    let wechat = Target::Foreground("com.tencent.xin".into());
    assert!(!t.within_scope(&wechat), "outside, and nobody is asked");
    assert_eq!(t.info().state, "running");
    answer_soon(&t, true).await;
    assert_eq!(t.check_scope(&wechat, None, &mut None).await.ok(), Some(true), "asked, then let through");
    assert!(t.within_scope(&wechat));
    assert_eq!(t.check_scope(&wechat, None, &mut None).await.ok(), Some(false), "not asked again");
    assert_eq!(t.info().apps, scope(&["com.apple.Preferences", "com.tencent.xin"]));
    assert_eq!(t.info().state, "running");
}

#[tokio::test]
async fn a_rejected_app_refuses_that_step_but_the_task_can_go_back() {
    let ts = tasks();
    let t = ts.open("改设置", "test", Some(PHONE), scope(&["com.apple.Preferences"]), None).unwrap();
    answer_soon(&t, false).await;
    // the tool call that tried it (step 7) shows as rejected, not failed; another failing call stays an error
    let r = t.check_scope(&Target::Launch("com.apple.MobileSMS".into()), Some(7), &mut None).await;
    assert!(matches!(r, Err(Response::Error { message, .. }) if message.contains("com.apple.Preferences")));
    t.step_done(7, false, 5, "refused", None);
    t.step_done(8, false, 5, "WDA unavailable", None);
    let (log, _) = log_of(&t).await;
    let status = |id: u64| {
        log.iter()
            .rev()
            .find(|e| e.event == "step" && e.data["id"] == id && e.data.get("ms").is_some())
            .map(|e| e.data["status"].clone())
    };
    assert_eq!((status(7), status(8)), (Some(json!("rejected")), Some(json!("error"))));
    assert_eq!(t.info().state, "running");
    assert_eq!(t.info().apps, scope(&["com.apple.Preferences"]));
    t.finish(true, "改好了");
    assert_eq!(t.info().state, "completed", "a scope refusal isn't a rejected task");
}

#[tokio::test]
async fn a_question_nobody_waits_for_leaves_the_page() {
    let ts = tasks();
    let t = ts.open("改设置", "test", Some(PHONE), scope(&["com.apple.Preferences"]), None).unwrap();
    let (tx, rx) = oneshot::channel();
    let t2 = t.clone();
    let asking = tokio::spawn(async move {
        t2.check_scope(&Target::Foreground("com.tencent.xin".into()), None, &mut Some(rx)).await
    });
    while t.info().state != "waiting" {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    tx.send(()).unwrap();
    assert!(matches!(asking.await.unwrap(), Err(Response::Error { message, .. }) if message.contains("取消")));
    assert_eq!(t.info().state, "running");
    assert!(t.control("confirm", None, true, None).is_err(), "nothing waits for an answer any more");
}

#[test]
fn the_scope_survives_a_restart() {
    let dir = std::env::temp_dir().join(format!("ictl-tasks-scope-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let ts = Tasks::load(Some(dir.clone()));
    let t = ts.open("改设置", "test", Some(PHONE), scope(&["com.apple.Preferences"]), None).unwrap();
    t.end(End::Completed, "");
    let ts = Tasks::load(Some(dir.clone()));
    assert_eq!(ts.get(&t.id).unwrap().info().apps, scope(&["com.apple.Preferences"]));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn only_the_newest_logs_are_kept() {
    let dir = std::env::temp_dir().join(format!("ictl-tasks-keep-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let ts = Tasks::load(Some(dir.clone()));
    for _ in 0..KEEP + 3 {
        open(&ts).end(End::Completed, "");
    }
    assert_eq!(ts.infos().len(), KEEP);
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), KEEP);
    std::fs::remove_dir_all(dir).unwrap();
}

/// A log cut short (power cut) is removed instead of lying there forever, outside the limit
#[test]
fn unreadable_logs_are_removed() {
    let dir = std::env::temp_dir().join(format!("ictl-tasks-bad-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("cut.jsonl"), "{\"t\": 1, \"event\": \"ta").unwrap();
    std::fs::write(dir.join("empty.jsonl"), "").unwrap();
    std::fs::write(dir.join("notes.txt"), "not a log").unwrap();
    let ts = Tasks::load(Some(dir.clone()));
    open(&ts).end(End::Completed, "");
    drop(ts);
    let ts = Tasks::load(Some(dir.clone()));
    assert_eq!(ts.infos().len(), 1);
    let mut left: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name()).collect();
    left.sort();
    assert_eq!(left.len(), 2, "{left:?}");
    assert!(left.iter().any(|n| n == "notes.txt"), "only task logs are touched");
    std::fs::remove_dir_all(dir).unwrap();
}

/// Ordinary events stop being persisted once the per-task event count limit is reached, while `task` and terminal
/// `done` are always written so restart recovery sees the real outcome instead of `interrupted`.
#[tokio::test]
async fn task_log_caps_ordinary_events_but_keeps_task_and_done_by_count() {
    let dir = std::env::temp_dir().join(format!("ictl-tasks-cap-count-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let ts = Tasks::load(Some(dir.clone()));
    let t = open(&ts);
    t.set_log_limits(5, 16 * 1024 * 1024);
    for i in 1..=20 {
        t.step_done(i, true, 1, "ok", None);
    }
    assert!(t.lock().capped);
    assert_eq!(t.lock().stored_events, 5, "1 task header + 4 ordinary events");
    t.end(End::Completed, "done");

    let sub = t.subscribe().await;
    assert!(sub.ended);
    assert_eq!(sub.events.len(), 6, "5 stored before cap + mandatory done");
    assert_eq!(sub.events.first().unwrap().1.event, "task");
    assert_eq!(sub.events.last().unwrap().1.event, "done");
    assert_eq!(sub.events.last().unwrap().0, sub.snap_seq, "done carries its real monotonic seq past skipped events");
    assert!(sub.snap_seq > 20);

    // Reloading from disk must see `completed`, not `interrupted`.
    drop(ts);
    let reloaded = Tasks::load(Some(dir.clone()));
    let info = reloaded.infos().into_iter().find(|i| i.id == t.id).unwrap();
    assert_eq!(info.state, "completed");
    let (log, ended) = log_of(&reloaded.get(&t.id).unwrap()).await;
    assert!(ended);
    assert_eq!(log.last().unwrap().event, "done");
    assert_eq!(log.last().unwrap().data["outcome"], "completed");
    std::fs::remove_dir_all(dir).unwrap();
}

/// Ordinary events stop being persisted once the byte limit would be exceeded, while terminal `done` is still written.
#[tokio::test]
async fn task_log_caps_ordinary_events_by_bytes_and_still_writes_done() {
    let dir = std::env::temp_dir().join(format!("ictl-tasks-cap-bytes-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let ts = Tasks::load(Some(dir.clone()));
    let t = open(&ts);
    let initial_bytes = t.lock().stored_bytes;
    // Allow room for only a couple of ordinary events beyond the header.
    let cap_bytes = initial_bytes + 300;
    t.set_log_limits(10_000, cap_bytes);
    let payload = "x".repeat(120);
    for i in 1..=20 {
        t.step_done(i, true, 1, &payload, None);
    }
    assert!(t.lock().capped);
    let before_done_bytes = t.lock().stored_bytes;
    let before_done_count = t.lock().stored_events;
    assert!(before_done_bytes <= cap_bytes, "{before_done_bytes} <= {cap_bytes}");
    // Once capped, even a tiny ordinary event must not slip in behind skipped events.
    t.step_done(21, true, 1, "tiny", None);
    assert_eq!(t.lock().stored_bytes, before_done_bytes);
    assert_eq!(t.lock().stored_events, before_done_count);
    t.end(End::Completed, "finished");

    let file = t.file.clone().unwrap();
    let on_disk_len = std::fs::metadata(&file).unwrap().len();
    assert_eq!(on_disk_len, t.lock().stored_bytes);
    assert!(on_disk_len > before_done_bytes, "done was still appended past the byte cap");
    let (log, ended) = log_of(&t).await;
    assert!(ended);
    assert_eq!(log.last().unwrap().event, "done");

    drop(ts);
    let reloaded = Tasks::load(Some(dir.clone()));
    let info = reloaded.infos().into_iter().find(|i| i.id == t.id).unwrap();
    assert_eq!(info.state, "completed");
    std::fs::remove_dir_all(dir).unwrap();
}

/// Reading the exact file prefix captured under the lock ignores events appended concurrently after the lock is
/// released; those newer events arrive through the broadcast receiver with strictly higher sequence numbers.
#[tokio::test]
async fn async_subscribe_reads_exact_prefix_without_duplicates_under_concurrent_writes() {
    let dir = std::env::temp_dir().join(format!("ictl-tasks-prefix-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let ts = Tasks::load(Some(dir.clone()));
    let t = open(&ts);
    t.step_done(1, true, 1, "first", None);

    // Simulate capturing (file_len, snap_seq, rx) under lock, releasing lock, then writing more before disk read.
    let (path, snap_len, snap_seq, mut rx) = {
        let st = t.lock();
        (t.file.clone().unwrap(), st.stored_bytes, st.next_seq, t.tx.subscribe())
    };
    t.step_done(2, true, 1, "second", None);
    t.end(End::Completed, "all done");

    let prefix = read_events_prefix(&path, snap_len);
    assert_eq!(prefix.last().unwrap().0, snap_seq, "prefix stops exactly at snap_len");
    let mut combined: Vec<(u64, String)> = prefix.into_iter().map(|(s, e)| (s, e.event)).collect();
    let mut last_seq = snap_seq;
    while let Ok((seq, e)) = rx.try_recv() {
        if seq > last_seq {
            last_seq = seq;
            combined.push((seq, e.event));
        }
    }
    let seqs: Vec<u64> = combined.iter().map(|(s, _)| *s).collect();
    assert_eq!(seqs, vec![1, 2, 3, 4], "every event seen once with strictly increasing seq");
    assert_eq!(combined.last().unwrap().1, "done");
    std::fs::remove_dir_all(dir).unwrap();
}

/// Pre-existing `.jsonl` task logs written without `"seq"` fields still restore and replay with monotonic numbers.
#[tokio::test]
async fn legacy_log_without_seq_fields_replays_and_restores() {
    let dir = std::env::temp_dir().join(format!("ictl-tasks-legacy-seq-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("legacy123456.jsonl");
    let raw = concat!(
        r#"{"t":1000,"event":"task","data":{"id":"legacy123456","title":"old","owner":"test","udid":"u1","started":1000,"apps":[]}}"#,
        "\n",
        r#"{"t":1001,"event":"step","data":{"id":1,"status":"ok","ms":5}}"#,
        "\n",
        r#"{"t":1002,"event":"done","data":{"ok":true,"stopped":false,"outcome":"completed","summary":"ok"}}"#,
        "\n",
    );
    std::fs::write(&file, raw).unwrap();

    let ts = Tasks::load(Some(dir.clone()));
    let t = ts.get("legacy123456").unwrap();
    let sub = t.subscribe().await;
    assert!(sub.ended);
    let pairs: Vec<(u64, &str)> = sub.events.iter().map(|(s, e)| (*s, e.event.as_str())).collect();
    assert_eq!(pairs, vec![(1, "task"), (2, "step"), (3, "done")]);
    assert_eq!(sub.snap_seq, 3);
    std::fs::remove_dir_all(dir).unwrap();
}
