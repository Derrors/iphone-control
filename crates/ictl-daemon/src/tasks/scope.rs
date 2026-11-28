//! App-scope checks for a task: what each request touches, whether it stays in scope, and user confirmation when
//! it leaves.

use tokio::sync::oneshot;

use crate::protocol::{Request, Response, TouchPhase};

use super::{Asked, Task, cancelled, ended_message, refuse};

/// The home screen, the lock screen and notifications: always in a task's scope (getting back from elsewhere)
pub(super) const SPRINGBOARD: &str = "com.apple.springboard";
/// Where http(s) links open
pub(super) const SAFARI: &str = "com.apple.mobilesafari";

/// What a request of a scoped task needs looked up before it may run ([`scope_check`])
#[derive(Debug, PartialEq)]
pub enum Check {
    /// Launches this app (a name or a bundle ID, to resolve)
    Launch(String),
    /// Opens this URL
    Url(String),
    /// Acts on whatever app is in front
    Foreground,
    /// Runs script in a web page: checked against the app that shows the page, which needn't be the one in front, once
    /// the page is known (`Daemon::web_eval`)
    Page,
    /// Always asks, whatever the scope
    Always { action: String, detail: String },
}

/// What a scoped task's action touches, once looked up
#[derive(Debug, PartialEq)]
pub enum Target {
    Launch(String),
    Url(String),
    /// The foreground app's bundle ID; empty when WDA didn't say
    Foreground(String),
    Always {
        action: String,
        detail: String,
    },
}

/// What a request does to the app scope. Pressing buttons and quitting apps aren't checked: they are how an agent
/// gets back after the user said no. Every variant is listed (no `_`), so a new request type has to be decided here
pub fn scope_check(req: &Request) -> Option<Check> {
    match req {
        Request::LaunchApp { app } => Some(Check::Launch(app.clone())),
        Request::OpenUrl { url } => Some(Check::Url(url.clone())),
        Request::Tap { .. }
        | Request::TapSequence { .. }
        | Request::ScrollCollect(_)
        | Request::TapElement { .. }
        | Request::LongPress { .. }
        | Request::Swipe { .. }
        | Request::TypeText { .. }
        | Request::ReplaceText { .. }
        | Request::Touch { phase: TouchPhase::Down | TouchPhase::Move, .. } => Some(Check::Foreground),
        Request::WebEval { .. } => Some(Check::Page),
        Request::InstallApp { path } => {
            Some(Check::Always { action: ictl_l10n::tr!("安装 App", "Install an app").into(), detail: path.clone() })
        }
        Request::UninstallApp { app } => Some(Check::Always {
            action: ictl_l10n::tr!("卸载 App（连同它的数据）", "Uninstall an app (with its data)").into(),
            detail: app.clone(),
        }),
        Request::Touch { phase: TouchPhase::Prepare | TouchPhase::Up, .. }
        | Request::PressButton { .. }
        | Request::TerminateApp { .. }
        | Request::SelectDevice { .. }
        | Request::Status
        | Request::ScreenStream
        | Request::MjpegStream
        | Request::Screenshot { .. }
        | Request::ListElements { .. }
        | Request::ListApps { .. }
        | Request::Settle { .. }
        | Request::AlertCheck
        | Request::WaitFor { .. }
        | Request::WaitForDisappearance { .. }
        | Request::Daemon
        | Request::Doctor
        | Request::Resign { .. }
        | Request::UnlockReset
        | Request::UnlockResetDevice { .. }
        | Request::SetupStep { .. }
        | Request::WdaForward { .. }
        | Request::PinCurrent { .. }
        | Request::ListDevices
        | Request::AppLogs { .. }
        | Request::CrashReports { .. }
        | Request::WebTargets
        | Request::TaskOpen { .. }
        | Request::TaskEvents { .. }
        | Request::TaskList
        | Request::TaskControl { .. }
        | Request::TaskAgent
        | Request::TaskCheck
        | Request::TaskStep { .. }
        | Request::TaskStepDone { .. }
        | Request::TaskConfirm { .. }
        | Request::TaskNote { .. }
        | Request::TaskFinish { .. } => None,
    }
}

fn in_scope(scope: &[String], bundle: &str) -> bool {
    bundle.eq_ignore_ascii_case(SPRINGBOARD) || scope.iter().any(|s| s.eq_ignore_ascii_case(bundle))
}

/// `None` when `target` is within `scope` (empty: no limit); otherwise what to ask the user (action, detail) and
/// the app that approving lets the task into for the rest of it
pub(super) fn out_of_scope(scope: &[String], target: &Target) -> Option<(String, String, Option<String>)> {
    if scope.is_empty() {
        return None;
    }
    let within = || ictl_l10n::trf!("任务范围：{}", "Task scope: {}", scope.join(", "));
    match target {
        Target::Launch(b) if !in_scope(scope, b) => Some((
            ictl_l10n::trf!("打开任务范围以外的 App：{b}", "Open an app outside the task's scope: {b}"),
            within(),
            Some(b.clone()),
        )),
        Target::Foreground(b) if b.is_empty() => Some((
            ictl_l10n::tr!("在不知道是哪个 App 的界面上操作", "Act on a screen whose app is unknown").into(),
            within(),
            None,
        )),
        Target::Foreground(b) if !in_scope(scope, b) => Some((
            ictl_l10n::trf!("在任务范围以外的 App 里操作：{b}", "Act in an app outside the task's scope: {b}"),
            within(),
            Some(b.clone()),
        )),
        Target::Url(u) => {
            let web = u
                .split_once(':')
                .is_some_and(|(s, _)| s.eq_ignore_ascii_case("http") || s.eq_ignore_ascii_case("https"));
            match (web, in_scope(scope, SAFARI)) {
                (true, true) => None,
                (true, false) => Some((
                    ictl_l10n::trf!("在 Safari 里打开链接：{u}", "Open a link in Safari: {u}"),
                    within(),
                    Some(SAFARI.into()),
                )),
                // tel:, sms:, an app's own scheme…: which app it opens, and what it starts there, isn't known
                (false, _) => Some((
                    ictl_l10n::trf!("打开链接：{u}", "Open the link {u}"),
                    ictl_l10n::trf!(
                        "不知道它会打开哪个 App、在那里做什么。{}",
                        "Which app it opens, and what it does there, isn't known. {}",
                        within()
                    ),
                    None,
                )),
            }
        }
        Target::Always { action, detail } => Some((action.clone(), detail.clone(), None)),
        Target::Launch(_) | Target::Foreground(_) => None,
    }
}

impl Task {
    /// Whether the task is limited to some apps
    pub fn scoped(&self) -> bool {
        !self.lock().scope.is_empty()
    }

    /// Whether `target` is within the scope now, without asking anyone
    pub fn within_scope(&self, target: &Target) -> bool {
        out_of_scope(&self.lock().scope, target).is_none()
    }

    /// Before an action of a scoped task: within its scope it goes on; outside it waits for the user. Approving lets
    /// the task into that app for the rest of it; rejecting refuses this action only, so the agent can go back
    /// (unlike a rejected `Self::confirm`, the task doesn't end as rejected). `step`: the tool call the action
    /// belongs to, shown as rejected when the user says no. Returns whether it waited
    pub async fn check_scope(
        &self,
        target: &Target,
        step: Option<u64>,
        gone: &mut Option<oneshot::Receiver<()>>,
    ) -> Result<bool, Response> {
        let ask = {
            let st = self.lock();
            out_of_scope(&st.scope, target).map(|a| (a, st.scope.join(", ")))
        };
        let Some(((action, detail, adds), scope)) = ask else { return Ok(false) };
        match self.ask_user(&action, &detail, gone).await {
            Asked::Approved => {
                if let Some(a) = adds {
                    let mut st = self.lock();
                    if !in_scope(&st.scope, &a) {
                        st.scope.push(a);
                    }
                }
                Ok(true)
            }
            Asked::Rejected => Err({
                if let Some(s) = step {
                    self.lock().declined.push(s);
                }
                refuse(&ictl_l10n::trf!(
                    "用户不同意超出任务范围（{action}），这一步没有执行。回到任务范围内的 App（{scope}）再继续，做不到就结束任务并说明原因",
                    "The user didn't allow going outside the task's scope ({action}); this step wasn't done. Go back to an app within the scope ({scope}) and carry on there, or if you can't, finish and say why"
                ))
            }),
            Asked::Ended => Err(refuse(ended_message())),
            Asked::Gone => Err(cancelled()),
        }
    }
}
