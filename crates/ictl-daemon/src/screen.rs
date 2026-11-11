//! The phone's own screen video for the control page ([`protocol::Request::ScreenStream`]).
//!
//! The CoreDevice screen media stream (iOS 27 and later) is the phone's hardware-encoded HEVC at about 60 frames a
//! second and its native size. The HID session already owns it (HID events need it); here it is handed on:
//! `ictl_device` turns the RTP into frames ([`ictl_device::ScreenFeed`]), this sends them to the viewer, which passes
//! them to the page. The host encodes nothing, and WDA doesn't have to capture its MJPEG picture (at 60 frames a
//! second that cost WDA over 100 % of a core, and iOS kills a process above 80 % over a minute).
//!
//! While a page watches, the HID session stays open (`Inner::screen_viewers`): the phone keeps its screen on and
//! shows "viewed remotely" meanwhile, as it does during HID use. The video is always portrait; the rotation of the
//! interface is sent alongside ([`ScreenMsg::Info`]) and the page turns the picture.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ictl_device::Rotation;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::broadcast::error::RecvError;

use crate::core::{Daemon, hid_supported};
use crate::protocol::{Response, SCREEN_KEYFRAME, ScreenMsg};

/// How often the interface rotation is asked of WDA while streaming (about 20 ms each, only when the device lock is
/// free)
const ROTATION_EVERY: Duration = Duration::from_secs(1);

fn degrees(r: Rotation) -> u32 {
    match r {
        Rotation::Portrait => 0,
        Rotation::TopToRight => 90,
        Rotation::UpsideDown => 180,
        Rotation::TopToLeft => 270,
    }
}

/// One watching page: counted while it lives, so the HID session isn't closed under it
struct Viewer(Arc<Daemon>);

impl Drop for Viewer {
    fn drop(&mut self) {
        let d = self.0.clone();
        tokio::spawn(async move {
            let mut g = d.inner.lock().await;
            g.screen_viewers = g.screen_viewers.saturating_sub(1);
            // the idle countdown starts now
            g.hid_used = Some(Instant::now());
        });
    }
}

impl Daemon {
    /// Serves one [`protocol::Request::ScreenStream`] connection until either side closes it
    pub(crate) async fn screen_stream(
        self: &Arc<Self>,
        mut reader: tokio::io::BufReader<OwnedReadHalf>,
        mut w: OwnedWriteHalf,
        expect_udid: Option<String>,
    ) {
        let (feed, viewer) = match self.screen_feed(expect_udid.as_deref()).await {
            Ok(f) => f,
            Err(e) => {
                let mut line = serde_json::to_string(&Response::error(e)).unwrap_or_default();
                line.push('\n');
                let _ = w.write_all(line.as_bytes()).await;
                return;
            }
        };
        let mut line = serde_json::to_string(&Response::text("streaming")).unwrap_or_default();
        line.push('\n');
        if w.write_all(line.as_bytes()).await.is_err() {
            return;
        }
        let _viewer = viewer;
        let mut frames = feed.subscribe();
        feed.request_keyframe();
        // nothing decodes before a keyframe (and the config message it brings)
        let mut need_key = true;
        let mut codec: Option<String> = None;
        let mut rotation: Option<u32> = None;
        let mut asked_rotation = Instant::now() - ROTATION_EVERY;
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        let mut buf = [0u8; 64];
        loop {
            let out = tokio::select! {
                r = frames.recv() => match r {
                    Ok(f) => {
                        if need_key && !f.key {
                            continue;
                        }
                        let mut out = Vec::new();
                        if f.key {
                            need_key = false;
                            if let Some(c) = ictl_device::hevc_codec(&f.data)
                                && codec.as_deref() != Some(c.as_str())
                            {
                                out.extend(ScreenMsg::Config.frame(serde_json::json!({ "codec": c }).to_string().as_bytes()));
                                codec = Some(c);
                            }
                        }
                        if codec.is_none() {
                            continue;
                        }
                        out.extend(if f.key { ScreenMsg::Key } else { ScreenMsg::Delta }.frame(&f.data));
                        out
                    }
                    // this connection fell more than the feed's buffer behind: go on from the next keyframe
                    Err(RecvError::Lagged(_)) => {
                        need_key = true;
                        feed.request_keyframe();
                        continue;
                    }
                    // the HID session (and its media stream) closed: the viewer reconnects
                    Err(RecvError::Closed) => return,
                },
                r = reader.read(&mut buf) => match r {
                    Ok(0) | Err(_) => return,
                    Ok(n) => {
                        if buf[..n].contains(&SCREEN_KEYFRAME) {
                            feed.request_keyframe();
                        }
                        continue;
                    }
                },
                _ = tick.tick() => {
                    // still waiting for a keyframe: ask again. One asked for right as the stream starts can be
                    // ignored by the phone, and the stream's own first keyframe may have passed before subscribing
                    if need_key {
                        feed.request_keyframe();
                    }
                    if asked_rotation.elapsed() < ROTATION_EVERY {
                        continue;
                    }
                    asked_rotation = Instant::now();
                    let Some(r) = self.current_rotation().await else { continue };
                    if rotation == Some(r) {
                        continue;
                    }
                    rotation = Some(r);
                    ScreenMsg::Info.frame(serde_json::json!({ "rotation": r }).to_string().as_bytes())
                }
            };
            if w.write_all(&out).await.is_err() {
                return;
            }
        }
    }

    /// The interface rotation in degrees: asked of WDA when the device lock is free, else the last one known
    async fn current_rotation(&self) -> Option<u32> {
        let mut g = self.inner.try_lock().ok()?;
        self.refresh_hid_rotation(&mut g).await;
        g.dev.wda_rotation.map(|(_, r)| degrees(r))
    }

    /// The screen feed of the controlled device, opening the HID session for it if needed
    async fn screen_feed(
        self: &Arc<Self>,
        expect_udid: Option<&str>,
    ) -> Result<(ictl_device::ScreenFeed, Viewer), String> {
        let mut g = self.inner.lock().await;
        let g = &mut *g;
        let udid =
            g.device.as_ref().map(|d| d.udid.clone()).ok_or(ictl_l10n::tr!("手机未连接", "Phone not connected"))?;
        if expect_udid.is_some_and(|u| !u.eq_ignore_ascii_case(&udid)) {
            return Err(ictl_l10n::tr!("正在控制的设备变了", "The controlled device changed").into());
        }
        if !hid_supported(g.info.as_ref()) {
            return Err(
                ictl_l10n::tr!("屏幕媒体流要 iOS 27 及以上", "The screen media stream needs iOS 27 or later").into()
            );
        }
        let feed = self
            .hid(g)
            .await?
            .screen_feed()
            .ok_or(ictl_l10n::tr!("屏幕媒体流已停止", "The screen media stream stopped"))?;
        g.screen_viewers += 1;
        g.hid_used = Some(Instant::now());
        Ok((feed, Viewer(self.clone())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_degrees() {
        assert_eq!(
            [Rotation::Portrait, Rotation::TopToRight, Rotation::UpsideDown, Rotation::TopToLeft].map(degrees),
            [0, 90, 180, 270]
        );
    }
}
