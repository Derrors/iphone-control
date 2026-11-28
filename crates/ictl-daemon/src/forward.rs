//! `wda-bridge` through the daemon (docs/dev/multi-instance-plan.md 4.5): each connection to WDA's port is relayed by
//! the instance that controls the device ([`crate::protocol::Request::WdaForward`]), never opened by the CLI itself.
//!
//! A forward is refused while a task is active (a task's scope and its pause can't be kept by a bridge), and opening
//! a task ends every forward first and waits for them to close ([`Forwards::end_all`]); both under the selection
//! latch, so neither comes in between the other's check and its start. A forward holds an admission pass: switching
//! devices ends it.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

use crate::core::Daemon;
use crate::protocol::Response;

/// How long opening a task waits for the forwards to close
const END_WAIT: Duration = Duration::from_secs(5);

/// The forwards open now
#[derive(Debug)]
pub(crate) struct Forwards {
    open: AtomicUsize,
    closed: tokio::sync::Notify,
    /// Moved on to end them all
    end: tokio::sync::watch::Sender<u64>,
}

impl Default for Forwards {
    fn default() -> Self {
        Self { open: AtomicUsize::new(0), closed: tokio::sync::Notify::new(), end: tokio::sync::watch::channel(0).0 }
    }
}

/// One open forward, counted until dropped
struct Open(Arc<Forwards>, u64);

impl Drop for Open {
    fn drop(&mut self) {
        self.0.open.fetch_sub(1, Ordering::SeqCst);
        self.0.closed.notify_waiters();
    }
}

impl Forwards {
    fn open(self: &Arc<Self>) -> Open {
        self.open.fetch_add(1, Ordering::SeqCst);
        Open(self.clone(), *self.end.borrow())
    }

    /// Ends every forward and waits (a bounded time) for them to close. With the selection latch held
    pub(crate) async fn end_all(&self) {
        self.end.send_modify(|e| *e += 1);
        let deadline = tokio::time::Instant::now() + END_WAIT;
        loop {
            let closed = self.closed.notified();
            tokio::pin!(closed);
            closed.as_mut().enable();
            if self.open.load(Ordering::SeqCst) == 0 || tokio::time::timeout_at(deadline, closed).await.is_err() {
                return;
            }
        }
    }

    async fn ended(&self, from: u64) {
        let _ = self.end.subscribe().wait_for(|e| *e != from).await;
    }
}

impl Daemon {
    /// Serves one [`crate::protocol::Request::WdaForward`] connection until either side closes it
    pub(crate) async fn wda_forward(&self, mut reader: BufReader<OwnedReadHalf>, mut w: OwnedWriteHalf, udid: &str) {
        let opened = self.open_forward(udid).await;
        let (phone, pass, open) = match opened {
            Ok(o) => o,
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
        let (mut from_phone, mut to_phone) = tokio::io::split(phone);
        let forwards = self.forwards.clone();
        tokio::select! {
            () = pass.revoked() => {}
            () = forwards.ended(open.1) => {}
            _ = tokio::io::copy(&mut from_phone, &mut w) => {}
            _ = async {
                let mut b = [0u8; 16 * 1024];
                while let Ok(n) = reader.read(&mut b).await {
                    if n == 0 || to_phone.write_all(&b[..n]).await.is_err() {
                        break;
                    }
                }
            } => {}
        }
        drop(open);
    }

    async fn open_forward(
        &self,
        udid: &str,
    ) -> Result<(impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin, crate::admission::Pass, Open), String> {
        let _latch = self.select_latch.lock().await;
        if self.tasks.active().is_some() {
            return Err(ictl_l10n::tr!(
                "有任务进行中，不开 WDA 桥接（任务的范围、暂停和接管管不到桥接上的操作）；任务结束后再试",
                "A task is active, so no WDA bridge is opened (the task's scope, pause and takeover can't reach what goes through a bridge); try again once it ends"
            )
            .into());
        }
        let (device, pass) = {
            let g = self.inner.lock().await;
            let device = g
                .device
                .clone()
                .filter(|d| d.udid.eq_ignore_ascii_case(udid) && g.lease.as_ref().is_some_and(|l| l.is_for(udid)))
                .ok_or_else(|| {
                    ictl_l10n::trf!("这个实例没有在控制 {udid}", "This instance isn't controlling {udid}")
                })?;
            (device, self.gate.admit(None)?)
        };
        let open = self.forwards.open();
        let phone = device.connect_port(ictl_wda::runner::WDA_PORT).await.map_err(|e| e.to_string())?;
        Ok((phone, pass, open))
    }
}
