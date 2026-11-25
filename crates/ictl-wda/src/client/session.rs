//! [`WdaClient`]: The transport, the XCTest session, stall detection and the idle-wait settings

use super::*;

impl<T: Transport> WdaClient<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            session: None,
            screen: None,
            screen_at: None,
            rotation_unsupported: false,
            lock_state_trusted: false,
            idle_baseline: None,
            idle_current: None,
            fast_stream: false,
            unanswered: AtomicU32::new(0),
            snapshot_level: 0,
            snapshot_applied: None,
            snapshot_read: 0,
            detection_point_set: false,
        }
    }

    pub(super) async fn call(&self, method: &str, path: &str, body: Option<Value>, timeout: Duration) -> Result<Value> {
        self.call_or_abandon(method, path, body, timeout, None).await
    }

    /// [`Self::call`]；`abandon` 是会话号时，请求没有应答就先结束这个会话，再判断 WDA 是不是卡死了（见 [`Self::in_session`]）
    pub(super) async fn call_or_abandon(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        timeout: Duration,
        abandon: Option<&str>,
    ) -> Result<Value> {
        // `/status` 不经 XCTest：WDA 半死时它照样应答，不能拿它算 WDA 还活着
        if path == "/status" {
            return self.send(method, path, body, timeout).await;
        }
        // 已判定卡死：不再发、不再一次次等超时（一个工具调用里接连几个请求，每个都等满时限的话，
        // 设备会被占好几分钟，守护进程没机会重启 WDA）
        if let Some(reason) = self.stall_reason() {
            let why = reason.describe();
            return Err(WdaError::Unavailable(ictl_l10n::trf!(
                "WDA 卡死判定：{why}，等守护进程重启它",
                "WDA is considered stuck: {why}; waiting for the daemon to restart it"
            )));
        }
        let r = self.send(method, path, body, timeout).await;
        match &r {
            Err(e) if e.no_answer() => {
                if let Some(sid) = abandon {
                    self.delete_session(sid).await;
                }
                let n = self.unanswered.fetch_add(1, Ordering::Relaxed) + 1;
                // 长请求未应答，补一次短探测；保留实际计数，不把两个请求写成三次无应答。
                if n < STALL_LIMIT && timeout > XCTEST_PROBE_TIMEOUT {
                    match self.send("GET", "/wda/activeAppInfo", None, XCTEST_PROBE_TIMEOUT).await {
                        Err(p) if p.no_answer() => {
                            self.unanswered.store((n + 1) | UNANSWERED_PROBE, Ordering::Relaxed);
                        }
                        reply if answered(&reply) => self.unanswered.store(0, Ordering::Relaxed),
                        _ => {}
                    }
                }
            }
            _ if answered(&r) => self.unanswered.store(0, Ordering::Relaxed),
            _ => {}
        }
        r
    }

    /// 连续三次无应答，或长请求和其短探测均无应答时，后续请求不再发送，等待守护进程恢复。
    pub fn stalled(&self) -> bool {
        self.stall_reason().is_some()
    }

    /// 读取一致的计数与判定原因；`/status` 的响应不参与此状态。
    pub fn stall_reason(&self) -> Option<StallReason> {
        let state = self.unanswered.load(Ordering::Relaxed);
        if state & UNANSWERED_PROBE != 0 {
            Some(StallReason::LongRequestAndProbe)
        } else if state >= STALL_LIMIT {
            Some(StallReason::ConsecutiveRequests(state))
        } else {
            None
        }
    }

    /// 实际连续无应答次数，不含判定标记。
    pub fn unanswered(&self) -> u32 {
        self.unanswered.load(Ordering::Relaxed) & !UNANSWERED_PROBE
    }

    pub(super) async fn send(&self, method: &str, path: &str, body: Option<Value>, timeout: Duration) -> Result<Value> {
        let body = body.map(|b| b.to_string().into_bytes());
        let (status, raw) = self.transport.send(method, path, body, timeout).await.map_err(|e| match e.kind() {
            std::io::ErrorKind::TimedOut => WdaError::Timeout(e.to_string()),
            crate::http::WRITE_FAILED if acts(method, path) => WdaError::Write(e.to_string()),
            crate::http::AFTER_SENT => WdaError::Interrupted(e.to_string()),
            _ => WdaError::Unavailable(e.to_string()),
        })?;
        let v: Value = match serde_json::from_slice(&raw) {
            Ok(v) => v,
            Err(_) if status >= 400 => {
                return Err(WdaError::Remote { kind: "http".into(), message: status.to_string() });
            }
            Err(e) => {
                return Err(WdaError::Interrupted(ictl_l10n::trf!("响应不是 JSON：{e}", "response is not JSON: {e}")));
            }
        };
        if let Some(err) = v.get("value").and_then(|x| x.get("error")).and_then(|e| e.as_str()) {
            let msg = v["value"].get("message").and_then(|m| m.as_str()).unwrap_or_default();
            return Err(WdaError::Remote {
                kind: err.to_owned(),
                message: msg.lines().next().unwrap_or_default().to_owned(),
            });
        }
        // Older runners may return a JSON 404 without a W3C value.error.
        if status >= 400 {
            return Err(WdaError::Remote { kind: "http".into(), message: status.to_string() });
        }
        Ok(v)
    }

    pub(super) async fn get(&self, path: &str) -> Result<Value> {
        Ok(self.call("GET", path, None, DEFAULT_TIMEOUT).await?["value"].take())
    }

    pub(super) async fn session_id(&mut self) -> Result<String> {
        if let Some(s) = &self.session {
            return Ok(s.clone());
        }
        let r =
            self.call("POST", "/session", Some(json!({"capabilities": {"alwaysMatch": {}}})), DEFAULT_TIMEOUT).await?;
        let id = r
            .get("sessionId")
            .and_then(|s| s.as_str())
            .or_else(|| r["value"].get("sessionId").and_then(|s| s.as_str()))
            .ok_or_else(|| {
                WdaError::Unavailable(
                    ictl_l10n::tr!("创建会话没有返回 sessionId", "creating a session returned no sessionId").into(),
                )
            })?
            .to_owned();
        self.session = Some(id.clone());
        self.idle_current = None;
        self.snapshot_applied = Some(0);
        if self.fast_stream {
            // fixOrientation explicitly off: a WDA left running in the background keeps what an
            // older version set
            let settings = json!({
                "mjpegServerFramerate": MJPEG_FPS,
                "mjpegScalingFactor": MJPEG_SCALE,
                "mjpegFixOrientation": false,
            });
            let path = format!("/session/{id}/appium/settings");
            if let Err(e) = self.call("POST", &path, Some(json!({"settings": settings})), DEFAULT_TIMEOUT).await {
                tracing::debug!("MJPEG settings: {e}");
            }
        }
        Ok(id)
    }

    /// 带会话的请求；会话失效时重建一次再试。
    ///
    /// 改变手机状态的请求（[`acts`]）没有应答时，马上结束这个会话（`DELETE /session/:id`，WDA 16.15.3 不排队处理它）：
    /// WDA 在主线程上一个一个处理请求，我们不等了它还会接着做。排在慢请求后面的点击、滑动会在报了失败之后很久才落到屏幕上
    /// （Poki 在真机上见过）。结束会话后，这个会话里还在排队的请求 WDA 直接回「会话不存在」，不再执行；正在执行的那个照样做完。
    /// 下一个请求开新会话
    pub async fn in_session(
        &mut self,
        method: &str,
        path: &str,
        body: Option<Value>,
        timeout: Duration,
    ) -> Result<Value> {
        for attempt in 0..2 {
            let input = acts(method, path);
            let sid = self.session_id().await.map_err(|e| if input { e.before_input() } else { e })?;
            let abandon = input.then_some(sid.as_str());
            let r =
                self.call_or_abandon(method, &format!("/session/{sid}{path}"), body.clone(), timeout, abandon).await;
            match r {
                Err(WdaError::Remote { kind, .. }) if attempt == 0 && kind == "invalid session id" => {
                    self.session = None
                }
                Err(e) if abandon.is_some() && e.no_answer() => {
                    self.forget_session();
                    return Err(e);
                }
                other => return other.map(|mut v| v["value"].take()),
            }
        }
        unreachable!()
    }

    /// 结束会话，不管结果：结束不了的会话 WDA 下次开会话时也会结束它
    pub(super) async fn delete_session(&self, sid: &str) {
        // send 而不是 call：WDA 卡死时也要发，也不算进卡死计数
        if let Err(e) = self.send("DELETE", &format!("/session/{sid}"), None, SESSION_DELETE_TIMEOUT).await {
            tracing::debug!("DELETE /session/{sid}: {e}");
        }
    }

    /// 会话已经结束（或要当作结束）：下一个请求开新会话，WDA 那时把设置重置成默认
    pub(super) fn forget_session(&mut self) {
        self.session = None;
        self.idle_current = None;
    }

    pub(super) async fn post(&mut self, path: &str, body: Value) -> Result<Value> {
        self.in_session("POST", path, Some(body), DEFAULT_TIMEOUT).await
    }

    pub async fn ready(&self) -> Result<bool> {
        Ok(self.call("GET", "/status", None, Duration::from_secs(5)).await?["value"]["ready"] == true)
    }

    /// WDA 还能不能经 XCTest 办事：问一次前台 App，限时 [`XCTEST_PROBE_TIMEOUT`]。
    /// `/status` 不经 XCTest，WDA 半死（iPad 真机：`/status` 照常 200，前台 App、截图、读元素、`/wda/locked` 全部超时）时
    /// 它照样报 ready；这一问才看得出来
    pub async fn xctest_responsive(&self) -> Result<()> {
        self.call("GET", "/wda/activeAppInfo", None, XCTEST_PROBE_TIMEOUT).await.map(drop)
    }

    pub async fn idle_settings(&mut self) -> Result<IdleSettings> {
        let v = self.in_session("GET", "/appium/settings", None, DEFAULT_TIMEOUT).await?;
        Ok(IdleSettings {
            wait_for_idle_timeout: v["waitForIdleTimeout"].as_f64().unwrap_or(10.0),
            animation_cool_off_timeout: v["animationCoolOffTimeout"].as_f64().unwrap_or(2.0),
        })
    }

    pub async fn set_idle_settings(&mut self, s: IdleSettings) -> Result<()> {
        if self.idle_current == Some(s) {
            return Ok(());
        }
        // not known if this fails
        self.idle_current = None;
        let body = json!({"settings": {"waitForIdleTimeout": s.wait_for_idle_timeout, "animationCoolOffTimeout": s.animation_cool_off_timeout}});
        self.post("/appium/settings", body).await?;
        self.idle_current = Some(s);
        Ok(())
    }
}
