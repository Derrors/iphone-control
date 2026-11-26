//! The answers that go on: the phone's picture, a task's events, a task's lease. Each checks what it needs first
//! (and answers an error as an ordinary answer), then streams from a handler on a [`Pipe`] ([`super::pipe`]), which
//! ends with the connection, and when the token it logged in with goes (the answer is cut off, see
//! [`super::server`]).

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

use super::pipe::{Pipe, streamed};
use super::server::{Reply, in_lang, json_reply, reply, reply_with, stream};
use super::{Ctx, FeedGuard, Head, protocol, sse};

/// `GET /stream` (MJPEG) or `/video` (H.264, or the phone's own HEVC with `src=device`) of the device the page shows
pub(super) async fn picture(ctx: Arc<Ctx>, head: &Head, feed: Option<FeedGuard>) -> Reply {
    // the device the page shows (`?udid=`), so a page that hasn't noticed a switch keeps its own picture instead of
    // silently showing another phone. Looked up again each time: after a replug usbmuxd gives the device a new id
    let udid = head.query("udid").filter(|u| !u.is_empty()).unwrap_or_else(|| ctx.udid());
    if udid.is_empty() {
        return reply(503, "text/plain", "no device yet");
    }
    // the page's low-bandwidth mode (`?low=1`): the phone's own video at a lower bitrate (for every page on that
    // phone, see `ictl_daemon::protocol::LOW_BANDWIDTH`), the H.264 picture at a higher quantizer (this page's only)
    let low = head.query("low").as_deref() == Some("1");
    let lang = head.lang;
    if head.path == "/video" && head.query("src").as_deref() == Some("device") {
        if !ctx.device_video_allowed {
            return reply(404, "text/plain", "turned off by IPHONE_CONTROL_VIEWER_CODEC");
        }
        return match crate::video::device::open(&ctx.socket, &udid, low).await {
            // the page takes 404 as "not this way" and falls back to H.264 or MJPEG
            Err(e) => reply(404, "text/plain; charset=utf-8", e),
            Ok((src, w)) => {
                let acks = ctx.acks.clone();
                octet_stream(lang, move |pipe| async move {
                    let _feed = feed;
                    crate::video::device::serve(src, w, acks, pipe).await;
                })
            }
        };
    }
    let video = if head.path == "/video" {
        match ctx.video.clone().filter(|e| e.label().is_some()) {
            Some(e) => Some(e),
            None => return reply(404, "text/plain", "no H.264 encoder"),
        }
    } else {
        None
    };
    // shared with the other pages on this phone: WDA captures a picture for each connection to its MJPEG port. The
    // daemon opens it (the viewer doesn't reach for the phone itself, D2) and passes WDA's answer on as it comes
    let socket = ctx.socket.clone();
    let connect = || async {
        match protocol::open_mjpeg_stream(&socket, Some(&udid)).await? {
            Ok(stream) => anyhow::Ok(stream),
            Err(why) => anyhow::bail!(why),
        }
    };
    let remote = match tokio::time::timeout(crate::video::STREAM_IDLE, ctx.mjpeg.subscribe(&udid, connect)).await {
        Ok(Ok(r)) => r,
        // no WDA (iOS 27 driven natively, or WDA down): the phone's own HEVC video transcoded to H.264, for a page
        // that can't decode HEVC
        _ if ctx.device_video_allowed && video.is_some() => {
            let Some(enc) = video else { unreachable!("checked") };
            // one ffmpeg per page: past the limit the page retries shortly (it shows "retrying")
            let Some(slot) = ctx.transcoders.take() else {
                return reply_with(
                    503,
                    "text/plain",
                    "too many pages transcoding the phone's video",
                    &[("Retry-After", "10")],
                );
            };
            let (src, w) = match crate::video::device::open(&ctx.socket, &udid, low).await {
                Ok(s) => s,
                Err(e) => return reply(502, "text/plain; charset=utf-8", e),
            };
            // the screen in points, to cut the video's padding off by
            let points =
                crate::client::snapshot(&ctx.socket).await.ok().and_then(|s| s.display).map(|d| (d.width, d.height));
            let qp = if low { enc.qp().max(crate::video::LOW_BANDWIDTH_QP) } else { enc.qp() };
            let acks = ctx.acks.clone();
            return octet_stream(lang, move |pipe| async move {
                let (_feed, _slot) = (feed, slot);
                crate::video::transcode::serve(enc, qp, points, acks, src, w, pipe).await;
            });
        }
        _ => return reply(502, "text/plain", "device feed unavailable"),
    };
    let acks = ctx.acks.clone();
    match video {
        Some(enc) => {
            let qp = if low { enc.qp().max(crate::video::LOW_BANDWIDTH_QP) } else { enc.qp() };
            octet_stream(lang, move |pipe| async move {
                let _feed = feed;
                crate::video::serve(enc, qp, acks, remote, pipe).await;
            })
        }
        None => stream(
            "multipart/x-mixed-replace; boundary=ictl",
            streamed(move |pipe| {
                in_lang(lang, async move {
                    let _feed = feed;
                    crate::video::serve_mjpeg(remote, pipe, crate::video::STREAM_IDLE).await;
                })
            }),
        ),
    }
}

/// A binary stream (H.264, or the phone's own video) written by `handler`
fn octet_stream<F, Fut>(lang: Option<ictl_l10n::Lang>, handler: F) -> Reply
where
    F: FnOnce(Pipe) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    stream("application/octet-stream", streamed(move |pipe| in_lang(lang, handler(pipe))))
}

/// The task's events so far, then new ones as they come (from the daemon), as server-sent events, until it is done
pub(super) fn events(head: &Head, mut tasks: protocol::TaskStream) -> Reply {
    let lang = head.lang;
    stream(
        "text/event-stream",
        streamed(move |mut pipe| {
            in_lang(lang, async move {
                loop {
                    tokio::select! {
                        e = tasks.next() => match e {
                            Some(e) => {
                                if pipe.write_all(sse(&e).as_bytes()).await.is_err() || e.event == "done" {
                                    return;
                                }
                            }
                            // the daemon went away (restarting): the page reconnects
                            None => return,
                        },
                        // a comment now and then finds a page that went away
                        _ = tokio::time::sleep(Duration::from_secs(15)) => {
                            if pipe.write_all(b": ping\n\n").await.is_err() {
                                return;
                            }
                        }
                    }
                }
            })
        }),
    )
}

/// `POST /api/agent/tasks/open` (docs/task-control.md 7.6): a task opened from another machine, as `task_open` on
/// the socket. The answer is server-sent events: `opened` with `{id, token}` first, then the task's events until it
/// ends. The connection is the task's lease: when the client goes away before the task ended, it is abandoned (the
/// handler, holding the lease, is dropped with the connection). `by`: the login token's name, the task's owner unless
/// the request names one
pub(super) async fn open_task(ctx: &Ctx, head: &Head, b: &Value, by: &str) -> Reply {
    let title = b["title"].as_str().unwrap_or_default();
    let owner = b["owner"].as_str().map(str::trim).filter(|o| !o.is_empty()).unwrap_or(by);
    let apps: Vec<String> = match &b["apps"] {
        Value::Null => Vec::new(),
        Value::Array(a) if a.iter().all(Value::is_string) => {
            a.iter().filter_map(Value::as_str).map(str::to_owned).collect()
        }
        _ => return json_reply(400, &json!({"ok": false, "message": "apps must be a list of strings"})),
    };
    let (opened, mut lease) = match protocol::open_task(&ctx.socket, title, owner, &apps).await {
        Ok(o) => o,
        Err(m) => return json_reply(409, &json!({"ok": false, "message": m})),
    };
    let first = format!("event: opened\ndata: {}\n\n", json!({"id": opened.id, "token": opened.token}));
    let lang = head.lang;
    stream(
        "text/event-stream",
        streamed(move |mut pipe| {
            in_lang(lang, async move {
                if pipe.write_all(first.as_bytes()).await.is_err() {
                    return;
                }
                loop {
                    tokio::select! {
                        e = lease.next() => match e {
                            Some(e) => {
                                if pipe.write_all(sse(&e).as_bytes()).await.is_err() || e.event == "done" {
                                    return;
                                }
                            }
                            // the daemon went away (restarting): the task was interrupted, which the client learns
                            // from the end
                            None => return,
                        },
                        // a comment now and then keeps proxies from closing a quiet connection
                        _ = tokio::time::sleep(Duration::from_secs(15)) => {
                            if pipe.write_all(b": ping\n\n").await.is_err() {
                                return;
                            }
                        }
                    }
                }
            })
        }),
    )
}
