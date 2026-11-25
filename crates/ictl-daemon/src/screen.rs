//! The phone's own screen video for the control page ([`protocol::Request::ScreenStream`]).
//!
//! The CoreDevice screen media stream (iOS 27 and later) is the phone's hardware-encoded HEVC at about 60 frames a
//! second and its native size. The HID session already owns it (HID events need it); here it is handed on:
//! `ictl_device` turns the RTP into frames ([`ictl_device::ScreenFeed`]), this sends them to the viewer, which passes
//! them to the page. The host encodes nothing, and WDA doesn't have to capture its MJPEG picture (at 60 frames a
//! second that cost WDA over 100 % of a core, and iOS kills a process above 80 % over a minute).
//!
//! While a page watches, the HID session stays open (`Inner::screen_viewers`): the phone keeps its screen on and
//! shows "viewed remotely" meanwhile, as it does during HID use.
//!
//! A page in low-bandwidth mode ([`crate::protocol::LOW_BANDWIDTH`]) gets the video capped at
//! [`ictl_device::LOW_BANDWIDTH_BITRATE`] while it watches. There is one stream per phone and its cap is set when it
//! starts, so the HID session is closed and opened again when the cap has to change: when the first such page comes,
//! and when the last one goes while others still watch (they get the full rate back). Every page's picture stops for
//! about half a second then, and a touch under way ends ([`crate::tools::video_bitrate`]). The video is always portrait; the rotation of the
//! interface is sent alongside ([`ScreenMsg::Info`]) and the page turns the picture.
//!
//! When the HID session closes under a stream (a bitrate change; unlocking natively, which reads the lock screen with
//! it closed seven or eight times, because iOS hides the keypad from accessibility while the screen is streamed),
//! the connection stays and waits for the next media stream ([`Daemon::reopen_feed`]): the page sees the picture
//! pause instead of losing it and reconnecting each time.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ictl_device::{Rotation, Tunnel};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::broadcast::error::RecvError;

use crate::core::{Daemon, close_hid, hid_supported};
use crate::protocol::{Response, SCREEN_KEYFRAME, ScreenMsg};
use crate::tools::video_bitrate;

/// How often the interface rotation is asked of WDA while streaming (about 20 ms each, only when the device lock is
/// free). Once for all the pages watching ([`Daemon::sample_rotation`]), not once per page
const ROTATION_EVERY: Duration = Duration::from_secs(1);

/// A media stream that closed under a page is waited for again after this long ([`Daemon::reopen_feed`])
const REOPEN_AFTER: Duration = Duration::from_millis(200);
/// … this many times in a row without a frame in between; then the page reconnects. Unlocking holds the device until
/// it is done, so a page waits for it once however often the session closes meanwhile
const REOPEN_TRIES: u32 = 10;

fn degrees(r: Rotation) -> u32 {
    match r {
        Rotation::Portrait => 0,
        Rotation::TopToRight => 90,
        Rotation::UpsideDown => 180,
        Rotation::TopToLeft => 270,
    }
}

/// One watching page: counted while it lives, so the HID session isn't closed under it. The flag: in low-bandwidth
/// mode (`Inner::low_bandwidth_viewers`)
struct Viewer(Arc<Daemon>, bool);

impl Drop for Viewer {
    fn drop(&mut self) {
        let (d, low) = (self.0.clone(), self.1);
        tokio::spawn(async move {
            let mut g = d.inner.lock().await;
            g.screen_viewers = g.screen_viewers.saturating_sub(1);
            if low {
                g.low_bandwidth_viewers = g.low_bandwidth_viewers.saturating_sub(1);
                // the last page in low-bandwidth mode is gone: the others get the full rate (their connections wait
                // for the new stream, see the module docs)
                let full = video_bitrate(&g);
                if g.low_bandwidth_viewers == 0
                    && g.screen_viewers > 0
                    && g.tunnel.as_mut().and_then(Tunnel::open_hid).is_some_and(|h| h.max_bitrate() < full)
                {
                    close_hid(&mut g).await;
                    tracing::info!(
                        "{}",
                        ictl_l10n::log_tr!(
                            "控制台上没有弱网模式的页面了：屏幕媒体流按原码率重新开始",
                            "No control page is in low-bandwidth mode any more: the screen media stream starts again at the full bitrate"
                        )
                    );
                }
            }
            // the idle countdown starts now
            g.hid_used = Some(Instant::now());
        });
    }
}

/// Returns once the page's side of the connection closes. What it sends meanwhile (keyframe requests) is dropped: a
/// reopened stream asks for a keyframe anyway
async fn page_gone(reader: &mut tokio::io::BufReader<OwnedReadHalf>) {
    let mut b = [0u8; 64];
    while matches!(reader.read(&mut b).await, Ok(n) if n > 0) {}
}

impl Daemon {
    /// Serves one [`protocol::Request::ScreenStream`] connection until either side closes it
    pub(crate) async fn screen_stream(
        self: &Arc<Self>,
        mut reader: tokio::io::BufReader<OwnedReadHalf>,
        mut w: OwnedWriteHalf,
        expect_udid: Option<String>,
        low_bandwidth: bool,
    ) {
        let (feed, viewer) = match self.screen_feed(expect_udid.as_deref(), low_bandwidth).await {
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
        let mut feed = feed;
        // stopped already (the HID session closed meanwhile): the viewer reconnects
        let Some(mut frames) = feed.subscribe() else { return };
        feed.request_keyframe();
        // media streams that closed one after the other without a frame in between
        let mut reopened = 0;
        // nothing decodes before a keyframe (and the config message it brings)
        let mut need_key = true;
        let mut codec: Option<String> = None;
        let mut rotation: Option<u32> = None;
        let mut rotations = self.rotation.subscribe();
        // the rotation already known goes out first
        rotations.mark_changed();
        self.sample_rotation();
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        let mut buf = [0u8; 64];
        loop {
            let out = tokio::select! {
                r = frames.recv() => match r {
                    Ok(f) => {
                        reopened = 0;
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
                    // the HID session (and its media stream) closed: wait for the next one on this connection. Not
                    // forever: a stream that closes again and again without a frame is given up (the viewer reconnects)
                    Err(RecvError::Closed) => {
                        reopened += 1;
                        if reopened > REOPEN_TRIES {
                            return;
                        }
                        // the page may close meanwhile (an unlock holds the device for seconds): then don't open the
                        // stream again only to leave the phone "viewed" for nobody until the HID session idles out
                        let reopened_feed = tokio::select! {
                            r = self.reopen_feed(expect_udid.as_deref(), low_bandwidth) => r,
                            () = page_gone(&mut reader) => return,
                        };
                        let Some((f, sub)) = reopened_feed else { return };
                        (feed, frames) = (f, sub);
                        need_key = true;
                        feed.request_keyframe();
                        continue;
                    }
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
                    continue;
                }
                r = rotations.changed() => {
                    if r.is_err() {
                        return;
                    }
                    let Some(r) = *rotations.borrow_and_update() else { continue };
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

    /// Starts the one rotation sampler for all the watching pages unless it runs: it samples every
    /// [`ROTATION_EVERY`] and publishes to [`Daemon::rotation`] while anyone subscribes, then stops. Before, each page
    /// sampled for itself, N lock attempts a second for N pages
    fn sample_rotation(self: &Arc<Self>) {
        use std::sync::atomic::Ordering::SeqCst;
        if self.rotation_sampling.swap(true, SeqCst) {
            return;
        }
        // what an earlier sampler saw may be out of date by now
        self.rotation.send_replace(None);
        let d = self.clone();
        tokio::task::spawn_local(async move {
            loop {
                if d.rotation.receiver_count() == 0 {
                    d.rotation_sampling.store(false, SeqCst);
                    // a page that came just now saw the sampler still running: carry on for it
                    if d.rotation.receiver_count() == 0 || d.rotation_sampling.swap(true, SeqCst) {
                        return;
                    }
                }
                if let Some(r) = d.current_rotation().await {
                    d.rotation.send_if_modified(|v| v.replace(r) != Some(r));
                }
                tokio::time::sleep(ROTATION_EVERY).await;
            }
        });
    }

    /// The fresh interface rotation, if this poll could measure it.
    async fn current_rotation(&self) -> Option<u32> {
        let mut g = self.inner.try_lock().ok()?;
        // The native stream can observe rotation between supervisor ticks. Publish the new view
        // and release any old contact before sending that rotation with the video.
        let (rotation, changed) = self.sample_display(&mut g, false).await;
        if changed {
            self.publish(&g);
        }
        rotation.map(degrees)
    }

    /// The next media stream after the last one closed under a watching page (see the module docs): taken once the
    /// device is free (unlocking holds it until done), for the same device and rate. `None` when there is none to be
    /// had: the device changed or went, the HID session can't be opened
    async fn reopen_feed(
        self: &Arc<Self>,
        expect_udid: Option<&str>,
        low_bandwidth: bool,
    ) -> Option<(ictl_device::ScreenFeed, tokio::sync::broadcast::Receiver<ictl_device::ScreenFrame>)> {
        // whoever closed it may go on with the device for a moment (the next digit of the passcode)
        tokio::time::sleep(REOPEN_AFTER).await;
        let mut g = self.inner.lock().await;
        let feed = self.open_feed(&mut g, expect_udid, low_bandwidth).await.ok()?;
        g.hid_used = Some(Instant::now());
        let frames = feed.subscribe()?;
        Some((feed, frames))
    }

    /// The screen feed of the controlled device, opening the HID session for it if needed (again, at the lower rate,
    /// for a page in low-bandwidth mode: see the module docs)
    async fn screen_feed(
        self: &Arc<Self>,
        expect_udid: Option<&str>,
        low_bandwidth: bool,
    ) -> crate::error::Result<(ictl_device::ScreenFeed, Viewer)> {
        let mut g = self.inner.lock().await;
        let g = &mut *g;
        let feed = self.open_feed(g, expect_udid, low_bandwidth).await?;
        g.screen_viewers += 1;
        if low_bandwidth {
            g.low_bandwidth_viewers += 1;
        }
        g.hid_used = Some(Instant::now());
        Ok((feed, Viewer(self.clone(), low_bandwidth)))
    }

    /// [`Self::screen_feed`] without counting the page
    async fn open_feed(
        &self,
        g: &mut crate::core::Inner,
        expect_udid: Option<&str>,
        low_bandwidth: bool,
    ) -> crate::error::Result<ictl_device::ScreenFeed> {
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
        let rate = if low_bandwidth { ictl_device::LOW_BANDWIDTH_BITRATE } else { video_bitrate(g) };
        if g.tunnel.as_mut().and_then(Tunnel::open_hid).is_some_and(|h| h.max_bitrate() != rate) {
            close_hid(g).await;
            tracing::info!(
                "{}",
                if low_bandwidth {
                    ictl_l10n::log_tr!(
                        "控制台有页面开了弱网模式：屏幕媒体流降低码率重新开始",
                        "A control page is in low-bandwidth mode: the screen media stream starts again at a lower bitrate"
                    )
                } else {
                    ictl_l10n::log_tr!(
                        "控制台上没有弱网模式的页面了：屏幕媒体流按原码率重新开始",
                        "No control page is in low-bandwidth mode any more: the screen media stream starts again at the full bitrate"
                    )
                }
            );
        }
        Ok(self
            .hid_at(g, rate)
            .await?
            .screen_feed()
            .ok_or(ictl_l10n::tr!("屏幕媒体流已停止", "The screen media stream stopped"))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{DisplayGeometry, tests::test_daemon};
    use ictl_device::ScreenSize;

    #[test]
    fn rotation_degrees() {
        assert_eq!(
            [Rotation::Portrait, Rotation::TopToRight, Rotation::UpsideDown, Rotation::TopToLeft].map(degrees),
            [0, 90, 180, 270]
        );
    }

    /// One sampler however many pages watch; it stops once none does, and the next page starts it again
    #[tokio::test]
    async fn one_rotation_sampler_while_pages_watch() {
        use std::sync::atomic::Ordering::SeqCst;
        let (d, dir) = test_daemon("stream-rotation-sampler");
        tokio::task::LocalSet::new()
            .run_until(async {
                let pages: Vec<_> = (0..3).map(|_| d.rotation.subscribe()).collect();
                for _ in &pages {
                    d.sample_rotation();
                }
                assert!(d.rotation_sampling.load(SeqCst));
                drop(pages);
                tokio::time::timeout(ROTATION_EVERY * 3, async {
                    while d.rotation_sampling.load(SeqCst) {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                })
                .await
                .expect("stops with the last page");
                let _page = d.rotation.subscribe();
                d.sample_rotation();
                assert!(d.rotation_sampling.load(SeqCst));
            })
            .await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn native_video_does_not_announce_a_cached_rotation_after_wda_is_gone() {
        let (d, dir) = test_daemon("stream-stale-rotation");
        let mut g = d.inner.lock().await;
        let rotation = Rotation::UpsideDown;
        DisplayGeometry::update(&mut g.dev.display, Some((ScreenSize { width: 393., height: 852. }, rotation)));
        g.dev.display_verified_at = Some(Instant::now());
        g.dev.last_rotation = Some((Instant::now(), rotation));
        g.dev.touch_rotation = Some(rotation);
        drop(g);

        assert_eq!(d.current_rotation().await, None);
        let g = d.inner.lock().await;
        assert!(g.dev.display.is_none());
        assert!(g.dev.touch_rotation.is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
