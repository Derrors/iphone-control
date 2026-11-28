//! Admission (docs/dev/multi-instance-plan.md 4.1): work on the device that goes on without the device lock
//! (`Inner`) holds a [`Pass`] for as long as it runs: crash reports and uninstalling, installs, re-signing, Web
//! Inspector, screen streams. A pass belongs to the lease it was taken under (its generation, moved on with every
//! lease taken).
//!
//! Switching to another device closes the gate (nothing new is let in, "switching"), ends the streams
//! ([`Pass::revoked`]: their clients connect again), then waits a bounded time for the passes to go
//! ([`Gate::drain`]), without holding the device lock: a pass's work may need it to finish. Long work (a re-signing
//! takes up to 15 minutes, an install minutes) isn't waited for: the switch is refused at once, saying what is under
//! way ([`Gate::close`]).
//!
//! Requests that hold the device lock throughout need no pass: the switch closes the gate under that lock, and a
//! request that gets the lock afterwards finds it closed ([`Gate::check_open`]).

use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Work that takes long enough not to be waited for by a switch
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Long {
    Resign,
    Install,
}

impl Long {
    pub fn describe(self) -> &'static str {
        match self {
            Self::Resign => ictl_l10n::tr!("正在重签 WDA", "re-signing WDA"),
            Self::Install => ictl_l10n::tr!("正在安装 App", "installing an app"),
        }
    }
}

#[derive(Debug, Default)]
struct State {
    generation: u64,
    /// The lease of this generation, held by every pass taken under it ([`crate::lease::Hold`])
    hold: Option<crate::lease::Hold>,
    closed: bool,
    short: usize,
    long: Vec<(u64, Long)>,
    next: u64,
}

#[derive(Debug)]
pub struct Gate {
    state: Mutex<State>,
    released: tokio::sync::Notify,
    /// Moved on to end the streams of the passes taken before ([`Pass::revoked`])
    revoked: tokio::sync::watch::Sender<u64>,
}

impl Default for Gate {
    fn default() -> Self {
        Self {
            state: Mutex::default(),
            released: tokio::sync::Notify::new(),
            revoked: tokio::sync::watch::channel(0).0,
        }
    }
}

/// Held by work on the device that goes on without the device lock
#[derive(Debug)]
pub struct Pass {
    gate: Arc<Gate>,
    generation: u64,
    epoch: u64,
    long: Option<u64>,
    /// The lease the work runs under stays held until it ends, even once the instance let go of it
    _hold: Option<crate::lease::Hold>,
}

/// The answer for work refused while switching devices
pub fn switching() -> String {
    ictl_l10n::tr!(
        "正在切换设备，这个操作没有执行；切换完成后再试",
        "Switching devices; this wasn't carried out. Try again once the switch is done"
    )
    .into()
}

impl Gate {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The lease generation now
    pub fn generation(&self) -> u64 {
        self.lock().generation
    }

    /// A new lease was taken (`hold`), or the one held let go of (`None`): what is let in from now on belongs to it
    pub fn advance(&self, hold: Option<crate::lease::Hold>) -> u64 {
        let mut s = self.lock();
        s.generation += 1;
        s.hold = hold;
        s.generation
    }

    /// `Err`: a switch is under way. For requests that hold the device lock throughout
    pub fn check_open(&self) -> Result<(), String> {
        if self.lock().closed { Err(switching()) } else { Ok(()) }
    }

    /// Lets work in, unless a switch is under way. Taken with the device lock held, after the device was looked up
    pub fn admit(self: &Arc<Self>, long: Option<Long>) -> Result<Pass, String> {
        let mut s = self.lock();
        if s.closed {
            return Err(switching());
        }
        let id = long.map(|what| {
            s.next += 1;
            let id = s.next;
            s.long.push((id, what));
            id
        });
        if id.is_none() {
            s.short += 1;
        }
        let hold = s.hold.clone();
        Ok(Pass { gate: self.clone(), generation: s.generation, epoch: *self.revoked.borrow(), long: id, _hold: hold })
    }

    /// Closes the gate for a switch. Returns the long work under way: a switch doesn't wait for it
    pub fn close(&self) -> Vec<Long> {
        let mut s = self.lock();
        s.closed = true;
        s.long.iter().map(|(_, w)| *w).collect()
    }

    /// Opens it again (the switch went back, or is done)
    pub fn reopen(&self) {
        self.lock().closed = false;
    }

    /// Ends the streams of the passes held now
    pub fn revoke_streams(&self) {
        self.revoked.send_modify(|e| *e += 1);
    }

    /// The work let in that is still going on, short and long
    pub fn in_progress(&self) -> usize {
        let s = self.lock();
        s.short + s.long.len()
    }

    /// The short work let in that is still going on
    fn short(&self) -> usize {
        self.lock().short
    }

    /// Waits up to `within` for the short work's passes to go. Long work isn't waited for: a switch is refused while
    /// there is any ([`Self::close`]), a daemon stopping doesn't wait for it. `Err`: how many are still held
    pub async fn drain(&self, within: Duration) -> Result<(), usize> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let released = self.released.notified();
            tokio::pin!(released);
            released.as_mut().enable();
            if self.short() == 0 {
                return Ok(());
            }
            if tokio::time::timeout_at(deadline, released).await.is_err() {
                return Err(self.short());
            }
        }
    }
}

impl Pass {
    /// Whether the lease it was taken under is still the one held: a result worked out under an earlier one is
    /// dropped rather than written over the new device's state
    pub fn current(&self) -> bool {
        self.gate.generation() == self.generation
    }

    /// Resolves once the streams of this pass are to end ([`Gate::revoke_streams`])
    pub async fn revoked(&self) {
        let mut rx = self.gate.revoked.subscribe();
        let _ = rx.wait_for(|e| *e != self.epoch).await;
    }
}

impl Drop for Pass {
    fn drop(&mut self) {
        let mut s = self.gate.lock();
        match self.long {
            Some(id) => s.long.retain(|(i, _)| *i != id),
            None => s.short = s.short.saturating_sub(1),
        }
        drop(s);
        self.gate.released.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Closed, nothing new gets in; a long piece of work is named; draining waits for the short ones and gives up at
    /// its deadline
    #[tokio::test]
    async fn a_closed_gate_drains_with_a_deadline() {
        let gate = Arc::new(Gate::default());
        let short = gate.admit(None).unwrap();
        let resign = gate.admit(Some(Long::Resign)).unwrap();
        assert_eq!(gate.close(), [Long::Resign]);
        assert!(gate.admit(None).is_err() && gate.check_open().is_err());
        let p = gate.clone();
        let t = tokio::spawn(async move { p.drain(Duration::from_secs(5)).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!t.is_finished(), "waits for the short one, not for the long one");
        drop(resign);
        assert_eq!(gate.close(), [], "gone once dropped");
        t.abort();
        assert_eq!(gate.drain(Duration::from_millis(100)).await, Err(1), "the short one is still held");
        let g = gate.clone();
        let t = tokio::spawn(async move { g.drain(Duration::from_secs(15)).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(short);
        assert_eq!(t.await.unwrap(), Ok(()));
        gate.reopen();
        gate.check_open().unwrap();
    }

    /// A pass knows when its lease is no longer the one held, and when its stream is to end
    #[tokio::test]
    async fn a_pass_follows_its_lease_and_its_stream() {
        let gate = Arc::new(Gate::default());
        let p = gate.admit(None).unwrap();
        assert!(p.current());
        let ended = tokio::time::timeout(Duration::from_millis(50), p.revoked()).await;
        assert!(ended.is_err(), "not revoked yet");
        gate.revoke_streams();
        tokio::time::timeout(Duration::from_secs(1), p.revoked()).await.unwrap();
        gate.advance(None);
        assert!(!p.current());
        let later = gate.admit(None).unwrap();
        assert!(later.current());
        tokio::time::timeout(Duration::from_millis(50), later.revoked()).await.expect_err("taken after the revocation");
    }
}
