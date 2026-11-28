//! 一个请求怎么走：分流（[`Lane`]）、准入（取消、过期、设备、任务范围）、拿锁，然后执行

use super::*;

impl Daemon {
    // ------------------------------------------------------------------ 请求处理

    /// 处理一个控制请求。`queued_at`：请求到达的时间，用来放弃排队太久的操作
    pub async fn handle(self: &Arc<Self>, req: Request, queued_at: Instant, mut ctx: CallCtx) -> Response {
        // 不需要设备锁的请求
        if matches!(req, Request::Daemon) {
            let mut s = self.snap.borrow().clone();
            let unlock = self.unlock_cfg(s.device.as_ref().map(|d| d.udid.as_str()));
            s.unlock_disabled = unlock.disabled_by_failure();
            s.auto_unlock = unlock.enabled();
            s.log_tail = self.log.tail(15);
            s.maintenance = crate::maintenance::active();
            s.instance = Some(self.identity());
            s.task_opening = self.task_opening.load(std::sync::atomic::Ordering::SeqCst);
            return Response::text(serde_json::to_string_pretty(&s).unwrap_or_default());
        }
        if let Err(e) = check_request(&req) {
            return Response::error(e).with_failure(Delivery::NotSent, FailureStage::Preflight);
        }
        let reset = matches!(req, Request::UnlockReset | Request::UnlockResetDevice { .. });
        // re-enabling auto-unlock is the user's decision, after checking the passcode: never an AI task's, which could
        // also name a device other than the one it is bound to
        if reset && ctx.task.is_some() {
            return Response::error(ictl_l10n::tr!(
                "AI 任务不能恢复自动解锁：要由人核对密码后在命令行里运行 iphone-control unlock-reset",
                "An AI task can't re-enable auto-unlock: a person runs iphone-control unlock-reset after checking the passcode"
            ))
            .with_failure(Delivery::NotSent, FailureStage::Preflight);
        }
        if let Some(r) = self.tasks.handle(&req, ctx.task.as_deref(), ctx.peer, &mut ctx.gone).await {
            return r;
        }
        // while a task runs, only it acts on the phone (or the user, after taking over): see `crate::tasks`
        let (admission, waited) =
            match self.tasks.admit(&req, ctx.task.as_deref(), &mut ctx.expect_udid, &mut ctx.gone).await {
                Ok(a) => a,
                Err(r) => return r.with_failure(Delivery::NotSent, FailureStage::Preflight),
            };
        if reset {
            return self.unlock_reset_now(&req, ctx.expect_udid.as_deref());
        }
        // a task limited to some apps: outside them, the action waits for the user (docs/task-control.md §2.6)
        let mut gated = Gated::default();
        if let crate::tasks::Admission::Actor(t) = &admission
            && t.scoped()
        {
            match self.scope_gate(t, &req, ctx.task_step, &mut ctx.gone).await {
                Ok(g) => gated = g,
                Err(r) => return r.with_failure(Delivery::NotSent, FailureStage::Preflight),
            }
        }
        // held while the task was paused or waiting for the user: the time in the queue starts now
        let queued_at = if waited || gated.asked { Instant::now() } else { queued_at };
        // an action of the task that no announced step covers is a step of its own
        let auto = match &admission {
            crate::tasks::Admission::Actor(t) if ctx.task_step.is_none() && req.is_device_action() => {
                Some((t.clone(), self.tasks.auto_step(t, &req)))
            }
            _ => None,
        };
        let t0 = Instant::now();
        let looks = crate::tasks::observes(&req);
        // boxed: the request's state machine is large, and a connection's task shouldn't carry it on the stack
        let resp = Box::pin(self.handle_admitted(req, queued_at, ctx, &admission, gated.unknown_approved)).await;
        if let Some((t, step)) = auto {
            self.tasks.auto_step_done(&t, step, &resp, t0.elapsed().as_millis() as u64);
        }
        // the agent has seen the screen only once its look succeeded
        if let crate::tasks::Admission::Actor(t) = &admission
            && looks
            && !matches!(resp, Response::Error { .. })
        {
            t.observed();
        }
        resp
    }

    /// A request's lock, once the tasks still let it through ([`crate::tasks::Tasks::still`]): it may have queued
    /// behind other requests while the user paused, took over or handed back. A task's request waits for the task to
    /// run again, without the lock
    /// `counted`: while it waits for the lock it counts in [`Daemon::waiting`] (background checks give way to it); not
    /// while it waits for a paused task, which may be for minutes: the page's checks go on meanwhile
    pub(super) async fn lock_admitted<'a, T>(
        &self,
        lock: &'a Mutex<T>,
        req: &Request,
        admission: &crate::tasks::Admission,
        queued_at: &mut Instant,
        gone: &mut Option<tokio::sync::oneshot::Receiver<()>>,
        counted: bool,
    ) -> Result<tokio::sync::MutexGuard<'a, T>, Response> {
        use crate::tasks::Still;
        loop {
            let waiting = counted.then(|| Waiting::new(self));
            let g = acquire(lock, gone).await?;
            drop(waiting);
            match self.tasks.still(admission, req) {
                Still::Admitted => return Ok(g),
                Still::Refused(r) => return Err(r),
                Still::Wait(t) => {
                    drop(g);
                    t.wait_running(gone).await?;
                    *queued_at = Instant::now();
                }
            }
        }
    }

    /// What an action of a scoped task touches, then [`crate::tasks::Task::check_scope`]. The device lock is only held
    /// to look it up, not while the user decides (the user may take over meanwhile)
    async fn scope_gate(
        &self,
        task: &crate::tasks::Task,
        req: &Request,
        step: Option<u64>,
        gone: &mut Option<tokio::sync::oneshot::Receiver<()>>,
    ) -> Result<Gated, Response> {
        use crate::tasks::{Check, Target};
        let Some(check) = crate::tasks::scope_check(req) else { return Ok(Gated::default()) };
        let target = match check {
            Check::Url(u) => Target::Url(u),
            // the page's app is checked once the page is picked, in the Web lane
            Check::Page => return Ok(Gated::default()),
            Check::Always { action, detail } => Target::Always { action, detail },
            Check::Launch(app) => {
                let mut g = self.lock_counted(gone).await?;
                Target::Launch(self.resolve_app(&mut g, &app).await.map_err(Response::error)?)
            }
            Check::Foreground => {
                // the lock is let go before the user is asked (below); the action checks again once it holds it
                let mut g = self.lock_counted(gone).await?;
                Target::Foreground(self.front_app(&mut g).await?)
            }
        };
        let asked = task.check_scope(&target, step, gone).await?;
        // an unknown screen is never let into the scope: the user allowed this action on it, nothing more
        let unknown_approved = asked && target == Target::Foreground(String::new());
        Ok(Gated { asked, unknown_approved })
    }

    /// The device lock for a short look-up on behalf of an action: background checks give way meanwhile
    async fn lock_counted(
        &self,
        gone: &mut Option<tokio::sync::oneshot::Receiver<()>>,
    ) -> Result<tokio::sync::MutexGuard<'_, Inner>, Response> {
        let _waiting = Waiting::new(self);
        acquire(&self.inner, gone).await
    }

    /// The app in front, for a scoped task's check. `Err`: why it can't be told, and so nothing is done
    async fn front_app(&self, g: &mut Inner) -> Result<String, Response> {
        // can't tell which app is in front: a scoped task doesn't act blind
        let unknown = |why: String| {
            Response::error(ictl_l10n::trf!(
                "看不出前台是哪个 App（{why}）；这个任务限定了 App 范围，所以没有操作。稍后截图再试",
                "Can't tell which app is in front ({why}); this task is limited to some apps, so nothing was done. Take a screenshot and try again shortly"
            ))
        };
        match self.route(g) {
            Route::Wda(w) => w.active_app().await.map_err(|e| unknown(e.to_string())),
            Route::Native => Box::pin(self.native_foreground(g)).await.map_err(|e| unknown(e.to_string())),
            Route::Hid | Route::Unavailable => Err(unknown(ictl_l10n::tr!("WDA 不可用", "WDA unavailable").into())),
        }
    }

    /// A scoped task's action on the app in front, checked again once it holds the device lock: the app may have
    /// changed since [`Self::scope_gate`] looked (it lets the lock go, and a request of the agent's or the user may
    /// come in between). Nobody is asked now (that would hold the lock): out of scope by now, it is refused.
    /// `unknown_approved`: the user just allowed this action on a screen whose app WDA didn't name ([`Gated`]); that
    /// screen, still unnamed, is let through, since an unknown app never joins the scope
    pub(super) async fn scope_recheck(
        &self,
        g: &mut Inner,
        req: &Request,
        admission: &crate::tasks::Admission,
        unknown_approved: bool,
    ) -> Result<(), Response> {
        let crate::tasks::Admission::Actor(t) = admission else { return Ok(()) };
        if !t.scoped() || !matches!(crate::tasks::scope_check(req), Some(crate::tasks::Check::Foreground)) {
            return Ok(());
        }
        let app = self.front_app(g).await?;
        recheck_front(t, &app, unknown_approved)
    }
}

/// [`Daemon::scope_recheck`]'s verdict on the app now in front (`""`: WDA didn't name it)
pub(super) fn recheck_front(t: &crate::tasks::Task, app: &str, unknown_approved: bool) -> Result<(), Response> {
    if app.is_empty() {
        return if unknown_approved {
            Ok(())
        } else {
            Err(crate::tasks::refuse(ictl_l10n::tr!(
                "看不出前台是哪个 App；这个任务限定了 App 范围，所以这一步没有执行。先截图看看再继续",
                "Can't tell which app is in front; this task is limited to some apps, so this step wasn't done. Take a screenshot before going on"
            )))
        };
    }
    if t.within_scope(&crate::tasks::Target::Foreground(app.into())) {
        return Ok(());
    }
    Err(crate::tasks::refuse(&ictl_l10n::trf!(
        "前台 App 刚变成了任务范围以外的 {app}，这一步没有执行。先截图看看再继续",
        "The app in front just became {app}, outside the task's scope; this step wasn't done. Take a screenshot before going on"
    )))
}

impl Daemon {
    /// A task's scope from names or bundle IDs: the bundle IDs, each once. `Err` names the app that isn't there
    pub(crate) async fn resolve_scope(&self, apps: &[String]) -> crate::error::Result<Vec<String>> {
        let apps: Vec<&str> = apps.iter().map(|a| a.trim()).filter(|a| !a.is_empty()).collect();
        if apps.is_empty() {
            return Ok(Vec::new());
        }
        let mut g = self.inner.lock().await;
        let mut out: Vec<String> = Vec::new();
        for a in apps {
            let b = self.resolve_app(&mut g, a).await?;
            if !out.iter().any(|x| x.eq_ignore_ascii_case(&b)) {
                out.push(b);
            }
        }
        Ok(out)
    }

    /// [`Self::handle`] once the tasks let the request through
    async fn handle_admitted(
        self: &Arc<Self>,
        req: Request,
        mut queued_at: Instant,
        mut ctx: CallCtx,
        admission: &crate::tasks::Admission,
        unknown_approved: bool,
    ) -> Response {
        let text = |r: crate::error::Result<String>| {
            r.map(|t| Response::text_with(t, crate::protocol::Outcome::default())).unwrap_or_else(Response::error)
        };
        match lane(&req) {
            Lane::Snapshot => unreachable!("handled above"),
            Lane::Task => {
                return Response::error(ictl_l10n::tr!(
                    "task_open、task_events、wda_forward 是流式请求，要用 open_task / task_events / wda-bridge 打开",
                    "task_open, task_events and wda_forward are streaming requests: open them with open_task / task_events / wda-bridge"
                ));
            }
            Lane::Setup => {
                let Request::SetupStep { udid, step } = &req else { unreachable!("lane") };
                if ctx.task.is_some() {
                    return Response::error(ictl_l10n::tr!("AI 任务不能做设置步骤", "An AI task can't do setup steps"))
                        .with_failure(Delivery::NotSent, FailureStage::Preflight);
                }
                return text(self.setup_step(udid, *step).await);
            }
            Lane::Select if matches!(req, Request::PinCurrent { .. }) => {
                let Request::PinCurrent { udid, expect_token } = &req else { unreachable!("lane") };
                if ctx.task.is_some() {
                    return Response::error(ictl_l10n::tr!("AI 任务不能固定设备", "An AI task can't pin the device"))
                        .with_failure(Delivery::NotSent, FailureStage::Preflight);
                }
                return text(self.pin_current(udid, expect_token).await);
            }
            Lane::Select => {
                let Request::SelectDevice { device } = &req else { unreachable!("lane") };
                let controlled = self.inner.lock().await.device.as_ref().map(|d| d.udid.clone());
                if let Err(r) = admit(&req, queued_at, &ctx, controlled.as_deref()) {
                    return r.with_failure(Delivery::NotSent, FailureStage::Preflight);
                }
                return text(self.select(device, &mut ctx.gone).await);
            }
            // 重签自己分段持锁：签名、安装要几十秒，期间不挡住别的请求
            Lane::Resign => {
                let Request::Resign { force } = req else { unreachable!("lane") };
                return self
                    .resign_now(force, ctx.expect_udid.as_deref())
                    .await
                    .map(Response::text)
                    .unwrap_or_else(Response::error);
            }
            // Web Inspector talks to the page, not to WDA: its own lock, the device lock only to look up the device
            Lane::Web => {
                // a scoped task: the app that shows the page must be in its scope, whatever is in front
                let scoped = match admission {
                    crate::tasks::Admission::Actor(t) if t.scoped() => Some(t.clone()),
                    _ => None,
                };
                // the page's app the user allowed (asked without the lock held), for the next round
                let mut approved: Option<String> = None;
                loop {
                    let mut slot = match self
                        .lock_admitted(&self.web, &req, admission, &mut queued_at, &mut ctx.gone, false)
                        .await
                    {
                        Ok(s) => s,
                        Err(r) => return r.with_failure(Delivery::NotSent, FailureStage::Preflight),
                    };
                    let (device, _pass) = {
                        let g = self.inner.lock().await;
                        if let Err(r) = admit(&req, queued_at, &ctx, g.device.as_ref().map(|d| d.udid.as_str())) {
                            return r;
                        }
                        let Some(device) = g.device.clone() else {
                            return Response::error(ictl_l10n::tr!("手机未连接", "Phone not connected"));
                        };
                        match self.gate.admit(None) {
                            Ok(p) => (device, p),
                            Err(e) => {
                                return Response::error(e).with_failure(Delivery::NotSent, FailureStage::Preflight);
                            }
                        }
                    };
                    let (target, expression, timeout_ms) = match &req {
                        Request::WebTargets => return text(self.web_targets(&mut slot, &device).await),
                        Request::WebEval { target, expression, timeout_ms } => (target, expression, *timeout_ms),
                        _ => unreachable!("lane"),
                    };
                    let scope = scoped.as_deref().map(|task| crate::web::Scope { task, approved: approved.as_deref() });
                    let timeout = Duration::from_millis(timeout_ms);
                    match self.web_eval(&mut slot, &device, target, expression, timeout, scope).await {
                        Ok(v) => return text(Ok(v)),
                        Err(crate::web::EvalError::Failed(e)) => return text(Err(e)),
                        Err(crate::web::EvalError::Refused(r)) => {
                            return r.with_failure(Delivery::NotSent, FailureStage::Preflight);
                        }
                        // the user is asked without the Web Inspector held; then everything is looked up again: the
                        // task may have been paused or taken over, the page may have gone or changed
                        Err(crate::web::EvalError::Ask(app)) => {
                            drop(slot);
                            let t = scoped.as_ref().expect("asked only for a scoped task");
                            let target = crate::tasks::Target::Foreground(app.clone());
                            if let Err(r) = t.check_scope(&target, ctx.task_step, &mut ctx.gone).await {
                                return r.with_failure(Delivery::NotSent, FailureStage::Preflight);
                            }
                            approved = Some(app);
                            queued_at = Instant::now();
                        }
                    }
                }
            }
            // an install can take minutes and doesn't hold the device lock; installs queue among themselves
            Lane::Install => {
                if ctx.gone.as_mut().is_some_and(|g| g.try_recv().is_ok()) {
                    return cancelled();
                }
                loop {
                    match self.tasks.still(admission, &req) {
                        crate::tasks::Still::Admitted => break,
                        crate::tasks::Still::Refused(r) => return r,
                        crate::tasks::Still::Wait(t) => {
                            if let Err(r) = t.wait_running(&mut ctx.gone).await {
                                return r;
                            }
                            queued_at = Instant::now();
                        }
                    }
                }
                let (device, pass) = {
                    let g = self.inner.lock().await;
                    if let Err(r) = admit(&req, queued_at, &ctx, g.device.as_ref().map(|d| d.udid.as_str())) {
                        return r;
                    }
                    let Some(device) = g.device.clone() else {
                        return Response::error(ictl_l10n::tr!("手机未连接", "Phone not connected"));
                    };
                    // long: a switch is refused while it runs, rather than waiting minutes for it
                    match self.gate.admit(Some(crate::admission::Long::Install)) {
                        Ok(p) => (device, p),
                        Err(e) => return Response::error(e).with_failure(Delivery::NotSent, FailureStage::Preflight),
                    }
                };
                let Request::InstallApp { path } = req else { unreachable!("lane") };
                // install_app clears the app lists itself once installed
                return text(self.install_app(&device, &pass, &path).await);
            }
            // the checks of this machine first, without the device lock; then the phone's, with it
            Lane::Doctor => {
                let host = self.doctor_host().await;
                let mut g =
                    match self.lock_admitted(&self.inner, &req, admission, &mut queued_at, &mut ctx.gone, true).await {
                        Ok(g) => g,
                        Err(r) => return r.with_failure(Delivery::NotSent, FailureStage::Preflight),
                    };
                let g = &mut *g;
                if let Err(r) = admit(&req, queued_at, &ctx, g.device.as_ref().map(|d| d.udid.as_str())) {
                    return r.with_failure(Delivery::NotSent, FailureStage::Preflight);
                }
                if let Err(e) = self.gate.check_open() {
                    return Response::error(e).with_failure(Delivery::NotSent, FailureStage::Preflight);
                }
                return Response::text(self.doctor(g, host).await);
            }
            // lockdown services of their own (crash reports, uninstalling): taking seconds, they don't hold the
            // device lock, which taps and reads wait for; like installs
            Lane::Apart => {
                if ctx.gone.as_mut().is_some_and(|g| g.try_recv().is_ok()) {
                    return cancelled();
                }
                loop {
                    match self.tasks.still(admission, &req) {
                        crate::tasks::Still::Admitted => break,
                        crate::tasks::Still::Refused(r) => return r,
                        crate::tasks::Still::Wait(t) => {
                            if let Err(r) = t.wait_running(&mut ctx.gone).await {
                                return r;
                            }
                            queued_at = Instant::now();
                        }
                    }
                }
                let mut g = self.inner.lock().await;
                if let Err(r) = admit(&req, queued_at, &ctx, g.device.as_ref().map(|d| d.udid.as_str())) {
                    return r.with_failure(Delivery::NotSent, FailureStage::Preflight);
                }
                let Some(device) = g.device.clone() else {
                    return Response::error(ictl_l10n::tr!("手机未连接", "Phone not connected"));
                };
                let pass = match self.gate.admit(None) {
                    Ok(p) => p,
                    Err(e) => return Response::error(e).with_failure(Delivery::NotSent, FailureStage::Preflight),
                };
                return match req {
                    Request::CrashReports { filter, name } => {
                        drop(g);
                        text(self.crash_reports(&device, &filter, &name).await)
                    }
                    Request::UninstallApp { app } => {
                        // which app: from the lists, with the lock (they are cached there)
                        let bid = match self.uninstall_target_of(&mut g, &app).await {
                            Ok(b) => b,
                            Err(e) => return text(Err(e)),
                        };
                        drop(g);
                        let r = device.uninstall(&bid).await;
                        // an action on the phone: what was read of the screen and the apps is out of date
                        let mut g = self.inner.lock().await;
                        if pass.current() {
                            g.dev.apps.clear();
                            g.elements_cache = None;
                            g.swiped_tree = None;
                            g.swiped_snap = None;
                        }
                        drop(g);
                        *self.last_action.lock().unwrap_or_else(|p| p.into_inner()) = Some(Instant::now());
                        text(r.map(|()| ictl_l10n::trf!("已卸载 {bid}", "Uninstalled {bid}")).map_err(Into::into))
                    }
                    _ => unreachable!("lane"),
                };
            }
            Lane::Device => {}
        }

        // a check the page repeats anyway: only while nothing else wants the phone ([`ictl_protocol::BACKGROUND`])
        let background = ctx.background && matches!(req, Request::Status | Request::AlertCheck);
        // someone is acting on the phone: a slow check now would sit in WDA's queue before their next action (giving
        // up waiting for its answer doesn't stop WDA working on it)
        let acting = || {
            let last = *self.last_action.lock().unwrap_or_else(|p| p.into_inner());
            last.is_some_and(|t| t.elapsed() < quiet_before(&req))
        };
        let mut g = if background {
            if acting() {
                return gave_way();
            }
            match self.inner.try_lock() {
                Ok(g) if self.waiting.load(std::sync::atomic::Ordering::SeqCst) == 0 => g,
                _ => return gave_way(),
            }
        } else {
            // still let through now that it holds the lock: the task may have been paused or taken over meanwhile
            match self.lock_admitted(&self.inner, &req, admission, &mut queued_at, &mut ctx.gone, true).await {
                Ok(g) => g,
                Err(r) => return r.with_failure(Delivery::NotSent, FailureStage::Preflight),
            }
        };
        let g = &mut *g;
        if let Err(r) = admit(&req, queued_at, &ctx, g.device.as_ref().map(|d| d.udid.as_str())) {
            return r.with_failure(Delivery::NotSent, FailureStage::Preflight);
        }
        // a switch let go of the device lock to wait: nothing reaches the old device meanwhile (listing doesn't)
        if !matches!(req, Request::ListDevices)
            && let Err(e) = self.gate.check_open()
        {
            return Response::error(e).with_failure(Delivery::NotSent, FailureStage::Preflight);
        }
        if let Err(r) = self.scope_recheck(g, &req, admission, unknown_approved).await {
            return r.with_failure(Delivery::NotSent, FailureStage::Preflight);
        }
        let view = ctx.expect_view.as_deref();
        // 改变界面的操作做完后缓存的元素树就作废；只有调用方声明「不改变布局」的按名字点击（max_age_ms > 0）保留
        let keeps_cache = matches!(req, Request::TapElement { max_age_ms, .. } if max_age_ms > 0);
        let invalidates = req.is_device_action() && !keeps_cache;
        // WDA 卡死（经 XCTest 的请求接连没有应答）：先重启它，这个请求按 WDA 正在启动处理。工具调用接连不断时巡检抢不到设备，
        // 等不到它来重启
        if g.wda.as_ref().is_some_and(|w| w.stalled()) {
            self.restart_stalled_wda(g).await;
        }
        let mut may_continue = |req: &Request| {
            !ctx.gone.as_mut().is_some_and(|rx| rx.try_recv().is_ok())
                && matches!(self.tasks.still(admission, req), crate::tasks::Still::Admitted)
        };
        let response = if background {
            // an action that comes meanwhile gets the phone at once: the check is read-only, dropping it is safe
            let action = self.give_way.notified();
            tokio::pin!(action);
            action.as_mut().enable();
            if self.waiting.load(std::sync::atomic::Ordering::SeqCst) > 0 {
                gave_way()
            } else {
                // dropping it halfway may leave the accessibility session's stream mid-message: the session then
                // counts as closed and is opened again (`ictl_ax::Session`); a check doesn't restart the phone's
                // accessibility daemon either, that is for actions to decide
                let check =
                    BACKGROUND.scope(true, Box::pin(self.dispatch(g, req, view, ctx.watched, &mut may_continue)));
                tokio::select! {
                    r = check => r,
                    () = action => gave_way(),
                }
            }
        } else {
            let acted = req.is_device_action();
            if acted {
                // 这个操作之前读过的界面就是「操作前」；操作期间读的（按名字点击先读树）也是操作前，操作完都不再算
                g.dev.before_action = g.dev.seen.take_fresh();
                g.dev.repeats.start(&req);
                // 原生控制等界面稳定时，从这之后手机推来的树算起
                g.dev.acted_at = Some(Instant::now());
                // what an earlier swipe read is no starting point after this action (a tap right after a swipe would
                // be found settled on the screen before the tap); a swipe sets it again as it ends
                g.swiped_tree = None;
                g.swiped_snap = None;
            }
            // boxed, like the request in `handle`: two of its large state machine on the stack overflowed a test thread
            let r = Box::pin(self.dispatch(g, req, view, ctx.watched, &mut may_continue)).await;
            if acted {
                g.dev.seen = Default::default();
                *self.last_action.lock().unwrap_or_else(|p| p.into_inner()) = Some(Instant::now());
            }
            r
        };
        if invalidates {
            g.elements_cache = None;
        }
        self.publish(g);
        response
    }

    async fn dispatch(
        &self,
        g: &mut Inner,
        req: Request,
        view: Option<&str>,
        watched: bool,
        may_continue: &mut dyn FnMut(&Request) -> bool,
    ) -> Response {
        // what the text reports, for clients that act on it (Response::Text.outcome)
        let mut outcome = crate::protocol::Outcome::default();
        // the same facts as data, for programs (Response::Text.data)
        let mut data = None;
        g.dev.unlocked = None;
        let r = match req {
            Request::Status => {
                let (text, status) = self.status(g).await;
                data = Some(crate::protocol::Data::Status(status));
                Ok(text)
            }
            // streams on its own connection (`lib::serve_conn` hands it to `screen::stream`); never queued here
            Request::ScreenStream => {
                Err("screen_stream is a streaming request: open it with open_screen_stream".into())
            }
            Request::MjpegStream => Err("mjpeg_stream is a streaming request: open it with open_mjpeg_stream".into()),
            Request::Screenshot { region } => {
                let r = self.screenshot(g, region).await.unwrap_or_else(Response::error);
                return with_unlock_note(r, g.dev.unlocked.take());
            }
            Request::ListElements { filter, limit, max_age_ms, keep_locked } => self
                .list_elements(g, &filter, limit, Duration::from_millis(max_age_ms), keep_locked)
                .await
                .map(|(text, elements)| {
                    data = Some(elements);
                    text
                }),
            Request::ScrollCollect(ref scan) => {
                self.scroll_collect(g, scan, || may_continue(&req)).await.map(|(text, collection)| {
                    data = Some(collection);
                    text
                })
            }
            Request::Tap { x, y, count } => self.tap(g, x, y, count, view).await,
            Request::TapSequence { points } => self.tap_sequence(g, &points).await,
            Request::TapElement { ref text, index, max_age_ms, scroll, ref selector } => {
                let target = crate::tools::TapTarget {
                    text,
                    index,
                    max_age: Duration::from_millis(max_age_ms),
                    scroll,
                    selector,
                };
                let r = self.tap_element(g, target, &mut || may_continue(&req)).await;
                r.map(|(t, missed)| {
                    outcome.tapped_nothing = missed;
                    // nothing was tapped: there is no "before the action" to compare the screen with, or the next
                    // settle would say the tap may not have taken effect
                    if missed {
                        g.dev.before_action = None;
                    }
                    t
                })
            }
            Request::LongPress { x, y, seconds } => self.long_press(g, x, y, seconds, view).await,
            Request::Swipe { from, to, seconds, hold } => {
                self.swipe(g, crate::tools::Stroke { from, to, seconds, hold }, view, watched).await
            }
            Request::TypeText { text, submit } => self.type_text(g, &text, submit, &mut outcome.field_verified).await,
            Request::ReplaceText { text, submit } => {
                Box::pin(self.replace_text(g, &text, submit)).await.map(|(message, delivery)| {
                    outcome.delivery = Some(delivery);
                    outcome.field_verified = Some(true);
                    message
                })
            }
            Request::PressButton { button } => self.press_button(g, &button, watched).await.map(|(t, alert)| {
                outcome.alert_reported = alert;
                t
            }),
            Request::Touch { phase, x, y } => {
                return self
                    .touch(g, phase, x, y, view)
                    .await
                    .map(|()| Response::text_with("", outcome))
                    .unwrap_or_else(|e| e);
            }
            Request::ListApps { include_system } => self.list_apps(g, include_system).await.map(|a| {
                let apps = a
                    .iter()
                    .map(|a| crate::protocol::AppData { name: a.name.clone(), bundle_id: a.bundle_id.clone() })
                    .collect();
                data = Some(crate::protocol::Data::Apps { apps });
                a.iter().map(|a| format!("{}\t{}", a.name, a.bundle_id)).collect::<Vec<_>>().join("\n")
            }),
            Request::LaunchApp { app } => self.launch_app(g, &app).await.map(|(t, alert)| {
                outcome.alert_reported = alert;
                t
            }),
            Request::TerminateApp { app } => self.terminate_app(g, &app).await,
            Request::OpenUrl { url } => self.open_url(g, &url).await.map(|(t, alert)| {
                outcome.alert_reported = alert;
                t
            }),
            Request::Settle { max_ms } => self.settle(g, Duration::from_millis(max_ms)).await,
            Request::AlertCheck => self.alert_check(g).await.map(|(text, alert)| {
                data = Some(crate::protocol::Data::Alert { text: alert });
                text
            }),
            Request::WaitFor { ref text, timeout_ms, ref selector } => {
                self.wait_for(g, text, selector, Duration::from_millis(timeout_ms), false, &mut || may_continue(&req))
                    .await
            }
            Request::WaitForDisappearance { ref text, timeout_ms, ref selector } => {
                self.wait_for(g, text, selector, Duration::from_millis(timeout_ms), true, &mut || may_continue(&req))
                    .await
            }
            // answered before the device lock is waited for (`Daemon::unlock_reset_now`)
            Request::UnlockReset | Request::UnlockResetDevice { .. } => unreachable!("unlock_reset_now"),
            Request::ListDevices => Ok(self.list_devices_text(g)),
            // its own lane: a switch lets go of the device lock while it waits ([`crate::switch`])
            Request::AppLogs { process, grep, since_secs, limit, errors_only } => {
                self.app_logs(g, &process, &grep, Duration::from_secs(since_secs), limit, errors_only).await
            }
            Request::Daemon
            | Request::Resign { .. }
            | Request::InstallApp { .. }
            | Request::Doctor
            | Request::CrashReports { .. }
            | Request::UninstallApp { .. }
            | Request::WebTargets
            | Request::WebEval { .. }
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
            | Request::TaskFinish { .. }
            | Request::SelectDevice { .. }
            | Request::SetupStep { .. }
            | Request::WdaForward { .. }
            | Request::PinCurrent { .. } => unreachable!("other lanes"),
        };
        let r = r.map(|t| Response::text_data(t, outcome, data)).unwrap_or_else(crate::error::Error::response);
        with_unlock_note(r, g.dev.unlocked.take())
    }
}

/// Which path a request takes through [`Daemon::handle`]. Every variant is listed (no `_`), so a new request type
/// doesn't compile until it is given a lane; a lane decides the lock, and every lane but the snapshot and re-signing
/// goes through [`admit`] (cancelled callers, stale actions, the expected device)
pub(super) enum Lane {
    /// `status`'s snapshot: no lock
    Snapshot,
    /// re-signing takes the device lock in short steps
    Resign,
    /// the Web Inspector connection's own lock
    Web,
    /// no device lock; installs queue among themselves
    Install,
    /// `doctor`: this machine's checks without the device lock, the phone's with it
    Doctor,
    /// lockdown services of their own (crash reports, uninstalling), without the device lock
    Apart,
    /// the device lock
    Device,
    /// tasks: the streams are served by the connection (`lib::serve_conn`), the rest before any lane
    Task,
    /// changing the device: the selection latch, the device lock in steps ([`crate::switch`])
    Select,
    /// `setup`'s device steps for the device held: the selection latch, then the device lock ([`crate::setup`])
    Setup,
}

pub(super) fn lane(req: &Request) -> Lane {
    match req {
        Request::Daemon => Lane::Snapshot,
        Request::Resign { .. } => Lane::Resign,
        Request::WebTargets | Request::WebEval { .. } => Lane::Web,
        Request::InstallApp { .. } => Lane::Install,
        Request::Doctor => Lane::Doctor,
        Request::CrashReports { .. } | Request::UninstallApp { .. } => Lane::Apart,
        Request::TaskOpen { .. }
        | Request::TaskEvents { .. }
        | Request::TaskList
        | Request::TaskControl { .. }
        | Request::TaskAgent
        | Request::TaskCheck
        | Request::TaskStep { .. }
        | Request::TaskStepDone { .. }
        | Request::TaskConfirm { .. }
        | Request::TaskNote { .. }
        | Request::TaskFinish { .. } => Lane::Task,
        Request::Status
        | Request::ScreenStream
        | Request::MjpegStream
        | Request::Screenshot { .. }
        | Request::ListElements { .. }
        | Request::Tap { .. }
        | Request::TapSequence { .. }
        | Request::ScrollCollect(_)
        | Request::TapElement { .. }
        | Request::LongPress { .. }
        | Request::Swipe { .. }
        | Request::TypeText { .. }
        | Request::ReplaceText { .. }
        | Request::PressButton { .. }
        | Request::Touch { .. }
        | Request::ListApps { .. }
        | Request::LaunchApp { .. }
        | Request::TerminateApp { .. }
        | Request::OpenUrl { .. }
        | Request::Settle { .. }
        | Request::AlertCheck
        | Request::WaitFor { .. }
        | Request::WaitForDisappearance { .. }
        | Request::UnlockReset
        | Request::UnlockResetDevice { .. }
        | Request::ListDevices
        | Request::AppLogs { .. } => Lane::Device,
        Request::SelectDevice { .. } => Lane::Select,
        Request::SetupStep { .. } => Lane::Setup,
        Request::PinCurrent { .. } => Lane::Select,
        // a stream, served by the connection (`lib::serve_conn`)
        Request::WdaForward { .. } => Lane::Task,
    }
}

/// Says in a request's answer that the phone was unlocked for it first, and how long that took: on the first line,
/// like the other notes about what was done. An action that had to unlock took seconds longer than usual (14-15 s
/// on an iPad through WDA, 8-9 s natively on an iPhone), and nothing in the answer said why
pub(super) fn with_unlock_note(r: Response, unlocked: Option<Duration>) -> Response {
    let Some(took) = unlocked else { return r };
    let note = ictl_l10n::trf!(
        "(手机锁着,先自动解锁了,用时 {:.1} 秒)",
        " (the phone was locked and was unlocked first, which took {:.1} s)",
        took.as_secs_f64()
    );
    let add = |s: String| match s.split_once('\n') {
        Some((first, rest)) => format!("{first}{note}\n{rest}"),
        None => s + &note,
    };
    match r {
        Response::Text { text, outcome, data } => Response::Text { text: add(text), outcome, data },
        Response::Image { jpeg_base64, caption, geometry } => {
            Response::Image { jpeg_base64, caption: add(caption), geometry }
        }
        Response::Error { message, retry_as_gesture, failure } => {
            Response::Error { message: add(message), retry_as_gesture, failure }
        }
    }
}

fn cancelled() -> Response {
    Response::error(ictl_l10n::tr!(
        "调用方已取消,请求没有执行",
        "The caller cancelled; the request was not carried out"
    ))
}

/// Takes `lock`, unless the caller goes away first (see [`CallCtx::gone`])
async fn acquire<'a, T>(
    lock: &'a Mutex<T>,
    gone: &mut Option<tokio::sync::oneshot::Receiver<()>>,
) -> Result<tokio::sync::MutexGuard<'a, T>, Response> {
    match gone {
        Some(gone) => tokio::select! {
            biased;
            g = lock.lock() => Ok(g),
            Ok(()) = &mut *gone => Err(cancelled()),
        },
        None => Ok(lock.lock().await),
    }
}

/// How long after an action a background check waits. The alert check takes WDA about 0.8 s on the home screen
/// (it snapshots SpringBoard; not waiting for idle doesn't help) and WDA does one thing at a time, so a tap or a Home
/// press right after it waited that long; the status check is about 0.2 s
fn quiet_before(req: &Request) -> Duration {
    match req {
        Request::AlertCheck => Duration::from_secs(4),
        _ => Duration::from_secs(1),
    }
}

/// A background check that didn't run, or stopped: an action wanted the phone ([`ictl_protocol::Data::Busy`])
fn gave_way() -> Response {
    Response::text_data(
        ictl_l10n::tr!(
            "手机正在执行操作，这次没查，稍后再查",
            "The phone is busy with an action; not checked this time"
        ),
        crate::protocol::Outcome::default(),
        Some(crate::protocol::Data::Busy),
    )
}

/// What [`Daemon::scope_gate`] found: whether the user was asked, and whether what they allowed was an action on a
/// screen whose app is unknown (it doesn't join the scope, so [`Daemon::scope_recheck`] needs to know)
#[derive(Debug, Default, Clone, Copy)]
struct Gated {
    asked: bool,
    unknown_approved: bool,
}

tokio::task_local! {
    /// A background check is being dispatched ([`ictl_protocol::BACKGROUND`]): it may be dropped halfway for an action
    /// and has no business with side effects such as restarting the phone's accessibility daemon
    static BACKGROUND: bool;
}

/// Whether the request being dispatched is a background check ([`BACKGROUND`])
pub(crate) fn in_background() -> bool {
    BACKGROUND.try_with(|b| *b).unwrap_or(false)
}

/// An action waiting for the device lock, counted in [`Daemon::waiting`] until it has it
pub(super) struct Waiting<'a>(&'a Daemon);

impl<'a> Waiting<'a> {
    pub(super) fn new(d: &'a Daemon) -> Self {
        d.waiting.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        d.give_way.notify_waiters();
        Self(d)
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.waiting.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Checks done once a request got its lock: the caller's expected device ([`CallCtx::expect_udid`]) and, for actions,
/// how long it queued ([`STALE`])
pub(super) fn admit(
    req: &Request,
    queued_at: Instant,
    ctx: &CallCtx,
    controlled: Option<&str>,
) -> Result<(), Response> {
    if let Some(want) = &ctx.expect_udid
        && controlled != Some(want.as_str())
    {
        return Err(Response::error(ictl_l10n::trf!(
            "正在控制的设备变了(原来是 {want},现在{}),没有执行;先用 iphone_list_devices 确认",
            "The controlled device changed (it was {want}, now {}); not carried out. Check with iphone_list_devices first",
            controlled.map_or(ictl_l10n::tr!("没有设备", "there is no device").into(), |u| ictl_l10n::trf!(
                "是 {u}",
                "it is {u}"
            ))
        )));
    }
    if req.is_device_action() && queued_at.elapsed() > STALE {
        return Err(Response::error(ictl_l10n::trf!(
            "守护进程正忙,这个操作排队超过 {} 秒,已放弃执行(以免落到已经变化的屏幕上);先截图看看再重试",
            "The daemon is busy and this action waited in the queue for more than {} seconds, so it was dropped (it could have landed on a changed screen); take a screenshot and try again",
            STALE.as_secs()
        )));
    }
    Ok(())
}

/// 参数检查（与设备无关，不必拿锁）
pub(super) fn check_request(req: &Request) -> Result<(), String> {
    req.check_required_text()?;
    match req {
        Request::ScrollCollect(scan)
            if !(1..=crate::protocol::MAX_SEARCH_SCROLLS).contains(&scan.max_scrolls)
                || !(1..=crate::protocol::MAX_SCROLL_ROWS).contains(&scan.limit) =>
        {
            return Err(ictl_l10n::tr!("超出滚动或行数上限", "scroll or row limit is out of range").into());
        }
        Request::TapElement { scroll: Some(s), .. } if !(1..=crate::protocol::MAX_SEARCH_SCROLLS).contains(&s.max) => {
            return Err(ictl_l10n::trf!(
                "scroll 最多滑 1 到 {} 次",
                "scroll can swipe 1 to {} times",
                crate::protocol::MAX_SEARCH_SCROLLS
            ));
        }
        Request::ScrollCollect(scan) if scan.x.is_some_and(|x| !x.is_finite() || x < 0.0) => {
            return Err(ictl_l10n::tr!("x 不能是负数", "x can't be negative").into());
        }
        Request::WebEval { expression, timeout_ms, .. }
            if expression.trim().is_empty() || *timeout_ms > MAX_WEB_EVAL_MS =>
        {
            return Err(ictl_l10n::trf!(
                "expression 不能为空,timeout 最多 {} 秒",
                "expression can't be empty, and timeout can be at most {} seconds",
                MAX_WEB_EVAL_MS / 1000
            ));
        }
        Request::Settle { max_ms } if *max_ms > MAX_WAIT_MS => {
            return Err(ictl_l10n::trf!("max_ms 不能超过 {MAX_WAIT_MS}", "max_ms can't exceed {MAX_WAIT_MS}"));
        }
        Request::WaitFor { timeout_ms, .. } | Request::WaitForDisappearance { timeout_ms, .. }
            if *timeout_ms > MAX_WAIT_MS =>
        {
            return Err(ictl_l10n::trf!(
                "timeout 不能超过 {} 秒",
                "timeout can't exceed {} seconds",
                MAX_WAIT_MS / 1000
            ));
        }
        Request::Touch { x, y, .. } | Request::Tap { x, y, .. } | Request::LongPress { x, y, .. }
            if !x.is_finite() || !y.is_finite() =>
        {
            return Err(ictl_l10n::tr!("x、y 要是有效的数", "x and y must be valid numbers").into());
        }
        Request::Swipe { from, to, .. } if ![from.0, from.1, to.0, to.1].iter().all(|v| v.is_finite()) => {
            return Err(
                ictl_l10n::tr!("起点、终点要是有效的数", "the start and end points must be valid numbers").into()
            );
        }
        // checked before the device lock is taken and the element cache dropped: a request that can't run doesn't
        // cost the next one its cached tree
        Request::Tap { count, .. } if !(1..=crate::protocol::MAX_TAP_COUNT).contains(count) => {
            return Err(ictl_l10n::trf!(
                "count 要是 1 到 {} 的整数",
                "count must be a whole number from 1 to {}",
                crate::protocol::MAX_TAP_COUNT
            ));
        }
        Request::TapSequence { points }
            if points.is_empty()
                || points.len() > crate::protocol::MAX_SEQUENCE_POINTS
                || !points.iter().all(|(x, y)| x.is_finite() && y.is_finite()) =>
        {
            return Err(ictl_l10n::trf!(
                "一个手势要点 1 到 {} 个点，坐标要是有效的数",
                "a gesture takes 1 to {} points, with valid numbers",
                crate::protocol::MAX_SEQUENCE_POINTS
            ));
        }
        Request::PressButton { button } if ictl_device::hid::Button::parse(button).is_none() => {
            return Err(ictl_l10n::tr!(
                "button 只能是 home / volume_up / volume_down / lock",
                "button must be home / volume_up / volume_down / lock"
            )
            .into());
        }
        Request::AppLogs { since_secs, limit, .. } if *since_secs > 3600 || *limit == 0 || *limit > 1000 => {
            return Err(ictl_l10n::tr!(
                "since 最多 3600 秒,limit 要在 1 到 1000 之间",
                "since can be at most 3600 seconds, and limit must be between 1 and 1000"
            )
            .into());
        }
        _ => {}
    }
    let seconds = match req {
        Request::LongPress { seconds, .. } => *seconds,
        Request::Swipe { seconds, hold, .. } => {
            if !hold.is_finite() || *hold < 0.0 {
                return Err(ictl_l10n::trf!("hold 要是不小于 0 的秒数", "hold must be a number of seconds, 0 or more"));
            }
            // 按住加移动一共多久：HID 两段都要逐个发采样。seconds 自己的范围下面照常查，负数不能靠 hold 凑过去
            if seconds.is_finite() && *seconds >= 0.0 && seconds + hold > MAX_GESTURE_SECONDS {
                return Err(ictl_l10n::trf!(
                    "seconds 加 hold 不能超过 {MAX_GESTURE_SECONDS} 秒",
                    "seconds plus hold must be at most {MAX_GESTURE_SECONDS} s"
                ));
            }
            *seconds
        }
        _ => return Ok(()),
    };
    if !seconds.is_finite() || !(0.0..=MAX_GESTURE_SECONDS).contains(&seconds) {
        return Err(ictl_l10n::trf!(
            "seconds 要在 0 到 {MAX_GESTURE_SECONDS} 之间",
            "seconds must be between 0 and {MAX_GESTURE_SECONDS}"
        ));
    }
    Ok(())
}
