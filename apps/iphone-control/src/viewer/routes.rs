//! A logged-in request, by its method and path: the page, the API, the picture and task streams, `/mcp`.

use std::sync::Arc;

use hyper::body::Incoming;
use serde_json::{Value, json};

use super::server::{BodyError, ConnState, Reply, json_reply, read_body, reply, reply_with};
use super::{
    Ctx, Head, MAX_BODY, Pass, Request, Response, SHORT, action_allowed, agent, api::*, auth, peer, post, protocol,
    task_events_id, updates,
};

pub(super) async fn route(
    ctx: Arc<Ctx>,
    conn: Arc<ConnState>,
    head: Head,
    parts: http::request::Parts,
    body: Incoming,
    pass: Pass,
) -> Reply {
    let path = head.path.as_str();
    match (head.method.as_str(), path) {
        ("GET", "/") => {
            let csp = ctx.page_csp.clone();
            let extra = [("Content-Security-Policy", csp.as_str()), ("Vary", "Accept-Encoding")];
            // compressed once at start; a page loaded over a slow link (the LAN, a remote viewer) comes 3 times faster
            if head.gzip && !ctx.page_gz.is_empty() {
                let extra = [extra[0], extra[1], ("Content-Encoding", "gzip")];
                return reply_with(200, "text/html; charset=utf-8", ctx.page_gz.clone(), &extra);
            }
            reply_with(200, "text/html; charset=utf-8", ctx.page.clone(), &extra)
        }
        ("GET", "/stream" | "/video") => {
            let feed = match head.query("feed").filter(|id| !id.is_empty() && id.len() <= 80) {
                Some(id) => match super::FeedGuard::register(&ctx, id) {
                    Some(g) => Some(g),
                    None => return reply(409, "text/plain", "duplicate stream"),
                },
                None => None,
            };
            super::stream::picture(ctx, &head, feed, &pass).await
        }
        ("GET", p) if task_events_id(p).is_some() => {
            let id = task_events_id(p).unwrap_or_default();
            match protocol::task_events(&ctx.socket, id).await {
                Ok(stream) => super::stream::events(ctx.clone(), &head, &pass, stream),
                Err(m) => json_reply(404, &json!({"message": m})),
            }
        }
        ("GET", p) if p.starts_with("/api/") => {
            let v = match &p[5..] {
                // always an answer: the page falls back to its offline demo when this one fails
                "info" => {
                    let mut v =
                        info(&ctx).await.unwrap_or_else(|m| json!({"width": null, "height": null, "warning": m}));
                    if let Some(id) = head.query("feed") {
                        v["feed_open"] = json!(ctx.feeds.lock().unwrap_or_else(|p| p.into_inner()).contains(&id));
                    }
                    // the task list, for a page that wants to pick up a task under way: with this poll instead of
                    // one of its own (absent when it couldn't be read: the page asks again next time)
                    if head.query("tasks").as_deref() == Some("1")
                        && let Ok(t) = protocol::task_list(&ctx.socket).await
                    {
                        v["tasks"] = json!(t);
                    }
                    // who the page is logged in as: a read-only token hides what it can't do
                    if let Some(w) = &pass.who {
                        v["login"] = json!({"name": w.name, "role": w.role.name()});
                    }
                    Ok(v)
                }
                "updates" => updates(&ctx).await,
                "daemon" => daemon(&ctx).await,
                "status" => status(&ctx).await,
                "devices" => devices(&ctx).await,
                "apps" => apps(&ctx, head.query("system").is_some_and(|v| v == "1")).await,
                "elements" => elements(&ctx, &head.query("filter").unwrap_or_default(), pass.watches()).await,
                "alert" => alert(&ctx, pass.watches()).await,
                "agent/backends" => Ok(json!({"backends": agent::backends()})),
                // every task the daemon knows, also those started elsewhere (`iphone-control task run`)
                "agent/tasks" => protocol::task_list(&ctx.socket).await.map(|t| json!({"tasks": t})),
                _ => return reply(404, "text/plain", "not found"),
            };
            match v {
                Ok(v) => json_reply(200, &v),
                Err(m) => json_reply(502, &json!({"message": m})),
            }
        }
        ("POST", "/api/agent/tasks/open" | "/api/agent/tasks/finish") => {
            let b = match action_body(&ctx, &head, &pass, body).await {
                Ok(b) => b,
                Err(answered) => return *answered,
            };
            if head.path == "/api/agent/tasks/open" {
                let who = pass.who.as_ref().map_or("remote", |w| w.name.as_str()).to_owned();
                return super::stream::open_task(ctx.clone(), &head, &pass, &b, &who).await;
            }
            let Some(token) = &head.task_token else {
                let m = ictl_l10n::tr!("结束任务要带任务口令", "Ending a task needs its token");
                return json_reply(400, &json!({"ok": false, "message": m}));
            };
            let req = Request::TaskFinish {
                ok: b["ok"].as_bool().unwrap_or(true),
                summary: b["summary"].as_str().unwrap_or_default().to_owned(),
            };
            let r = protocol::as_task(Some(token.clone()), None, protocol::call(&ctx.socket, &req, Some(SHORT))).await;
            let (code, v) = match r {
                Ok(Response::Text { .. }) => (200, json!({"ok": true})),
                Ok(Response::Error { message, .. }) => (409, json!({"ok": false, "message": message})),
                Ok(Response::Image { caption, .. }) => (409, json!({"ok": false, "message": caption})),
                Err(e) => (502, json!({"ok": false, "message": e.to_string()})),
            };
            json_reply(code, &v)
        }
        ("GET" | "POST" | "DELETE", "/mcp") if ctx.mcp.is_some() => {
            // a browser says where it comes from: only the page itself. Other clients send no Origin
            if head.origin.as_deref().is_some_and(|o| !ctx.access.origin_allowed(o, ctx.port, ctx.tls.is_some())) {
                return reply(403, "text/plain", "forbidden");
            }
            // a length or chunked, not both (a request read two ways is how requests are smuggled past proxies),
            // and no other transfer coding
            if head.chunked && (head.content_length.is_some() || !head.transfer_chunked_only) {
                return reply(400, "text/plain", "bad request");
            }
            if head.method == "POST" && head.content_length.is_none() && !head.chunked {
                return reply(411, "text/plain", "length required");
            }
            if head.content_length.is_some_and(|n| n > super::mcp::MAX_BODY) {
                return reply(413, "text/plain", "too large");
            }
            let body = match read_body(body, super::mcp::MAX_BODY, super::server::BODY_TIMEOUT).await {
                Ok(b) => b,
                Err(BodyError::TooLarge) => return reply(413, "text/plain", "too large"),
                Err(BodyError::Broken) => return reply(400, "text/plain", "bad body"),
            };
            let svc = ctx.mcp.as_ref().expect("checked above");
            super::mcp::serve(svc, parts, body).await
        }
        ("POST", p) if p.starts_with("/api/") => {
            let b = match action_body(&ctx, &head, &pass, body).await {
                Ok(b) => b,
                Err(answered) => return *answered,
            };
            // controlling a task: who, for its log, and which local process (the daemon refuses the task's agent)
            let caller = if p.starts_with("/api/agent/tasks/") {
                Caller::of(&conn, pass.who.as_ref()).await
            } else {
                Caller::default()
            };
            let (code, v) = post(&ctx, &p[5..], &b, head.device.as_deref(), caller).await;
            json_reply(code, &v)
        }
        _ => reply(404, "text/plain", "not found"),
    }
}

/// An action's JSON body (`POST /api/…`), once it may be sent from where it comes ([`action_allowed`]); otherwise
/// the answer (refused, too large, not JSON)
async fn action_body(ctx: &Ctx, head: &Head, pass: &Pass, body: Incoming) -> Result<Value, Box<Reply>> {
    if !action_allowed(head, &ctx.access, ctx.port, ctx.tls.is_some(), pass.program()) {
        return Err(Box::new(reply(403, "text/plain", "forbidden")));
    }
    if head.content_length.unwrap_or(0) > MAX_BODY {
        return Err(Box::new(reply(413, "text/plain", "too large")));
    }
    let bad = || Box::new(json_reply(400, &json!({"ok": false, "message": "bad request"})));
    let bytes = match read_body(body, MAX_BODY, super::server::BODY_TIMEOUT).await {
        Ok(b) => b,
        Err(BodyError::TooLarge) => return Err(Box::new(reply(413, "text/plain", "too large"))),
        Err(BodyError::Broken) => return Err(bad()),
    };
    serde_json::from_slice::<Value>(&bytes).map_err(|_| bad())
}

/// Who controls a task from the page: the token's name for its log, and the local process when it is one
#[derive(Default)]
pub(super) struct Caller {
    pub by: String,
    pub pid: Option<u32>,
}

impl Caller {
    /// `who`: the token it logged in with, by name in the task's log
    async fn of(conn: &ConnState, who: Option<&auth::Who>) -> Self {
        let (peer, local) = (conn.peer, conn.local);
        // reads every process's open files: not on the runtime's threads
        let pid = tokio::task::spawn_blocking(move || peer::local_client(peer, local)).await.ok().flatten();
        let mut by = who.map_or_else(|| "viewer".to_owned(), |w| format!("viewer:{}", w.name));
        if !peer.ip().to_canonical().is_loopback() {
            by = format!("{by} {}", peer.ip());
        }
        Self { by, pid }
    }
}
