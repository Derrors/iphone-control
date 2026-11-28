//! The account directory as several processes share it (docs/dev/multi-instance-plan.md 4.2): every daemon instance
//! and the CLI's `login` read and write the same `account/`.
//!
//! - **Account lock** (`account/.lock`): held by whatever may write the account's files: saving the credentials
//!   (`login`), writing a new signing key (signing). Waited for with a deadline ([`LOCK_WAIT`]), polled so a daemon's
//!   single thread isn't blocked. Not held while the user types a verification code. The anisette state has a lock
//!   of its own, taken only around provisioning (`omnisette::state_file`), never while this one is waited for.
//! - **Credentials generation** (`account/commit.json`): a count that changes whenever the credentials do, so a
//!   result worked out with the credentials of before (a session check) can tell it is stale. An Apple ID signed in
//!   again gets new tokens under the same name and team, so neither can stand for it. The count is written after
//!   `accounts.json`, with its SHA-256: a crash between the two is seen as a digest that doesn't match, and the count
//!   is moved on then. Credentials that changed always change the count, at worst twice.
//! - **Anisette origin**: the address the anisette state was provisioned with, kept in the state itself
//!   (`omnisette::state_file`). A state from an older version has none; it is used as before by a lone daemon, but
//!   not while other instances share the directory ([`AccountDir::require_known_origin`]). A state provisioned with
//!   another address than the one configured is never sent to the configured one.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::{AccountDir, Result, SignError};

pub use omnisette::state_file::Origin;

/// How long a process waits for another one done with the account (a new signing certificate takes seconds)
pub const LOCK_WAIT: Duration = Duration::from_secs(30);
const LOCK_FILE: &str = ".lock";
const COMMIT_FILE: &str = "commit.json";

/// Held while the account's files are changed
pub struct AccountLock(#[allow(dead_code)] File);

#[derive(Debug, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
struct Commit {
    generation: u64,
    accounts_sha256: String,
}

/// Which anisette state is on disk: where it came from and a digest of the state itself. Two states provisioned with
/// the same address differ by the digest
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateIdentity {
    pub origin: OriginCheck,
    digest: Option<String>,
}

/// What [`AccountDir::anisette_origin`] found, against the address configured now
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OriginCheck {
    /// No provisioned state yet: the next use provisions one with the configured address
    Fresh,
    /// Provisioned with the configured address
    Known,
    /// Provisioned by an older version, address not recorded
    Unknown,
    /// Provisioned with another address than the configured one
    Mismatch { recorded: String, configured: String },
}

impl AccountDir {
    /// The address the anisette state is sent to
    pub fn anisette_address(&self) -> String {
        self.anisette().anisette_url_v3().clone()
    }

    /// Where the anisette state came from, against the configured address
    pub fn anisette_origin(&self) -> Result<OriginCheck> {
        let origin = omnisette::state_file::origin(&self.dir).map_err(|e| SignError::Transient(e.to_string()))?;
        Ok(self.check_of(origin))
    }

    fn check_of(&self, origin: Origin) -> OriginCheck {
        let configured = self.anisette_address();
        match origin {
            Origin::Missing | Origin::Unprovisioned => OriginCheck::Fresh,
            Origin::Unknown => OriginCheck::Unknown,
            Origin::Url(u) if omnisette::state_file::same_address(&u, &configured) => OriginCheck::Known,
            Origin::Url(recorded) => OriginCheck::Mismatch { recorded, configured },
        }
    }

    /// The anisette state on disk now ([`StateIdentity`])
    pub fn anisette_identity(&self) -> Result<StateIdentity> {
        // one read: the digest and the origin of the same bytes
        let snap = omnisette::state_file::snapshot(&self.dir).map_err(|e| SignError::Transient(e.to_string()))?;
        Ok(match snap {
            None => StateIdentity { origin: OriginCheck::Fresh, digest: None },
            Some((bytes, origin)) => {
                StateIdentity { origin: self.check_of(origin), digest: Some(hex(&Sha256::digest(&bytes))) }
            }
        })
    }

    /// `f`'s result with the anisette state it used: the state on disk is the same before and after (provisioning
    /// writes a new one, so the same bytes both times mean `f` read that one), tried again a few times when it
    /// changed meanwhile
    pub(crate) async fn with_state_used<T, F, Fut>(&self, mut f: F) -> Result<(T, StateIdentity)>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        for _ in 0..3 {
            let before = self.anisette_identity()?;
            let r = f().await?;
            if self.anisette_identity()? == before {
                return Ok((r, before));
            }
        }
        Err(SignError::Transient(
            "the anisette state kept changing while it was used (another process provisioning it)".into(),
        ))
    }

    /// Refuses to use the anisette state with the configured address when it came from another one, or from an
    /// unknown one while that isn't allowed ([`Self::require_known_origin`])
    pub(crate) fn check_origin(&self) -> Result<()> {
        match self.anisette_origin()? {
            OriginCheck::Fresh | OriginCheck::Known => Ok(()),
            OriginCheck::Unknown if !self.require_known_origin => Ok(()),
            OriginCheck::Unknown => Err(SignError::AnisetteOriginUnknown),
            OriginCheck::Mismatch { recorded, configured } => {
                Err(SignError::AnisetteOriginMismatch { recorded, configured })
            }
        }
    }

    /// `login --confirm-anisette-origin`: the user says the anisette state was provisioned with the configured
    /// address. `Ok(false)`: there is no provisioned state to confirm
    pub async fn confirm_anisette_origin(&self) -> Result<bool> {
        let url = self.anisette_address();
        omnisette::state_file::confirm_origin(&self.dir, &url).await.map_err(|e| SignError::Transient(e.to_string()))
    }

    /// `login --reprovision-anisette`: the anisette state is discarded and provisioned again with the configured
    /// address, under the account lock
    pub async fn reprovision_anisette(&self) -> Result<()> {
        crate::restrict_dir(&self.dir)?;
        let _lock = self.lock().await?;
        omnisette::state_file::discard(&self.dir).await.map_err(|e| SignError::Transient(e.to_string()))?;
        self.fetch_anisette_with(&crate::SESSION_RETRY_DELAYS, crate::SESSION_ATTEMPT_TIMEOUT, tokio::time::sleep)
            .await?;
        crate::restrict_tree(&self.dir)?;
        Ok(())
    }

    /// The account lock, waited for up to [`LOCK_WAIT`]
    pub async fn lock(&self) -> Result<AccountLock> {
        self.lock_within(LOCK_WAIT).await
    }

    pub(crate) async fn lock_within(&self, wait: Duration) -> Result<AccountLock> {
        crate::restrict_dir(&self.dir)?;
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(self.dir.join(LOCK_FILE))?;
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            // SAFETY: a valid descriptor, owned by `f` for as long as the lock is used
            if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(AccountLock(f));
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::WouldBlock {
                return Err(e.into());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(SignError::AccountBusy);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// The credentials generation now ([`self`](crate::account) explains it). Moves it on, under the account lock,
    /// when the credentials on disk aren't the ones it was counted for
    pub async fn credentials_generation(&self) -> Result<u64> {
        let (commit, digest) = self.read_commit()?;
        if commit.accounts_sha256 == digest {
            return Ok(commit.generation);
        }
        let _lock = self.lock().await?;
        self.commit_locked()
    }

    /// The credentials generation now, with the account lock held (moved on, as [`Self::credentials_generation`] does,
    /// when the credentials on disk aren't the ones it was counted for): it can't change while the lock is
    pub fn generation_locked(&self, _lock: &AccountLock) -> Result<u64> {
        self.commit_locked()
    }

    /// After the credentials were written, with the account lock held: the generation moved on, written with the
    /// digest of what is on disk now. Also when nothing changed since the last count, so it's never behind
    pub(crate) fn commit_locked(&self) -> Result<u64> {
        let (commit, digest) = self.read_commit()?;
        if commit.accounts_sha256 == digest && commit.generation > 0 {
            return Ok(commit.generation);
        }
        let next = Commit { generation: commit.generation + 1, accounts_sha256: digest };
        write_whole(&self.dir, COMMIT_FILE, &serde_json::to_vec_pretty(&next).map_err(io::Error::other)?)?;
        Ok(next.generation)
    }

    fn read_commit(&self) -> Result<(Commit, String)> {
        let accounts = match std::fs::read(self.dir.join("accounts.json")) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        let digest = hex(&Sha256::digest(&accounts));
        let commit = match std::fs::read(self.dir.join(COMMIT_FILE)) {
            Ok(b) => serde_json::from_slice(&b).unwrap_or_default(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Commit::default(),
            Err(e) => return Err(e.into()),
        };
        Ok((commit, digest))
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// `name` in `dir` replaced whole by `data`, private, on disk (data and name)
fn write_whole(dir: &Path, name: &str, data: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!(".{name}.tmp-{}-{n}", std::process::id()));
    let r = (|| {
        let mut f = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        f.write_all(data)?;
        f.sync_all()?;
        std::fs::rename(&tmp, dir.join(name))?;
        File::open(dir)?.sync_all()
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn acct(require_known_origin: bool) -> (AccountDir, PathBuf) {
        let dir = std::env::temp_dir().join(format!("ictl-account-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        (
            AccountDir { dir: dir.clone(), anisette_url: Some("https://ani.example.com".into()), require_known_origin },
            dir,
        )
    }

    /// The count moves on whenever the credentials change, also when the commit was never written (a crash between
    /// the two writes); unchanged credentials keep it
    #[tokio::test]
    async fn the_generation_follows_the_credentials() {
        let (a, dir) = acct(false);
        let g0 = a.credentials_generation().await.unwrap();
        assert_eq!(a.credentials_generation().await.unwrap(), g0, "nothing changed");
        std::fs::write(dir.join("accounts.json"), b"{\"a\":1}").unwrap();
        let g1 = a.credentials_generation().await.unwrap();
        assert!(g1 > g0, "written without its commit (cut short): counted when next read");
        let _l = a.lock().await.unwrap();
        assert_eq!(a.commit_locked().unwrap(), g1, "already counted");
        // signed in again: new tokens, same account
        std::fs::write(dir.join("accounts.json"), b"{\"a\":2}").unwrap();
        let g2 = a.commit_locked().unwrap();
        assert!(g2 > g1);
        drop(_l);
        assert_eq!(a.credentials_generation().await.unwrap(), g2);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The lock is one holder at a time, across separate opens (as in two processes); waiting has a deadline
    #[tokio::test]
    async fn the_account_lock_has_one_holder_and_a_deadline() {
        let (a, dir) = acct(false);
        let held = a.lock().await.unwrap();
        let t = std::time::Instant::now();
        assert!(matches!(a.lock_within(Duration::from_millis(300)).await, Err(SignError::AccountBusy)));
        assert!(t.elapsed() >= Duration::from_millis(300));
        assert!(SignError::AccountBusy.is_transient(), "never read as an expired session");
        drop(held);
        a.lock_within(Duration::from_millis(300)).await.unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The anisette state's origin: none yet or provisioned with the configured address is used; one from another
    /// address never; one without a record (an older version's) only while it may be
    #[tokio::test]
    async fn an_anisette_state_is_used_only_with_the_address_it_came_from() {
        let state = |by: Option<&str>| {
            let by = by.map(|u| format!("<key>provisioned_by</key><string>{u}</string>")).unwrap_or_default();
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict>\
                 <key>keychain_identifier</key><data>AAAAAAAAAAAAAAAAAAAAAA==</data>\
                 <key>adi_pb</key><data>AQID</data>{by}</dict></plist>"
            )
        };
        let (lone, dir) = acct(false);
        assert_eq!(lone.anisette_origin().unwrap(), OriginCheck::Fresh);
        lone.check_origin().unwrap();
        std::fs::write(dir.join("state.plist"), state(None)).unwrap();
        assert_eq!(lone.anisette_origin().unwrap(), OriginCheck::Unknown);
        lone.check_origin().unwrap();
        let shared = AccountDir { require_known_origin: true, ..lone.clone() };
        assert!(matches!(shared.check_origin(), Err(SignError::AnisetteOriginUnknown)));
        assert!(shared.confirm_anisette_origin().await.unwrap());
        assert_eq!(shared.anisette_origin().unwrap(), OriginCheck::Known);
        shared.check_origin().unwrap();
        std::fs::write(dir.join("state.plist"), state(Some("https://other.example.com/"))).unwrap();
        assert!(matches!(lone.check_origin(), Err(SignError::AnisetteOriginMismatch { .. })));
        assert!(!SignError::AnisetteOriginUnknown.is_transient());
        std::fs::write(dir.join("state.plist"), state(Some("https://ani.example.com/"))).unwrap();
        assert_eq!(lone.anisette_origin().unwrap(), OriginCheck::Known, "a trailing slash is the same address");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The provider itself checks the state it is about to send (docs/dev/multi-instance-plan.md 10.5): a state of
    /// another address, or of none recorded while one is required, is refused before anything is sent, even when
    /// the check made earlier on the disk saw another state
    #[tokio::test]
    async fn the_state_sent_is_the_one_checked() {
        use omnisette::anisette_headers_provider::AnisetteHeadersProvider;
        let state = |by: Option<&str>| {
            let by = by.map(|u| format!("<key>provisioned_by</key><string>{u}</string>")).unwrap_or_default();
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict>\
                 <key>keychain_identifier</key><data>AAAAAAAAAAAAAAAAAAAAAA==</data>\
                 <key>adi_pb</key><data>AQID</data>{by}</dict></plist>"
            )
        };
        let (a, dir) = acct(true);
        // what the earlier check saw: none yet. Then another process wrote one of another address
        a.check_origin().unwrap();
        std::fs::write(dir.join("state.plist"), state(Some("https://other.example.com"))).unwrap();
        let provider = |required: bool| {
            omnisette::remote_anisette_v3::RemoteAnisetteProviderV3::new(a.anisette_address(), dir.clone(), "0".into())
                .requiring_known_origin(required)
        };
        let e = provider(false).get_anisette_headers(false).await.unwrap_err();
        assert!(matches!(e, omnisette::AnisetteError::OriginMismatch { .. }), "{e}");
        std::fs::write(dir.join("state.plist"), state(None)).unwrap();
        let e = provider(true).get_anisette_headers(false).await.unwrap_err();
        assert!(matches!(e, omnisette::AnisetteError::OriginUnknown), "{e}");
        let mapped = SignError::from(plume_core::Error::Anisette(e));
        assert!(matches!(mapped, SignError::AnisetteOriginUnknown) && mapped.is_configuration());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A login that waited (for its verification code) while the same Apple ID was signed in again elsewhere doesn't
    /// write its older result over the newer one; another Apple ID's merges as it is
    #[tokio::test]
    async fn an_older_login_doesn_t_overwrite_a_newer_one() {
        use plume_store::{AccountStore, GsaAccount};
        let (a, dir) = acct(false);
        let path = dir.join("accounts.json");
        let gsa =
            |email: &str, token: &str| GsaAccount::new(email.into(), "A".into(), "id".into(), token.into(), "T".into());
        let add = |g: GsaAccount| {
            let mut s = AccountStore::load_sync(&Some(path.clone())).unwrap_or_else(|_| {
                std::fs::write(&path, br#"{"selected_account":null,"accounts":{}}"#).unwrap();
                AccountStore::load_sync(&Some(path.clone())).unwrap()
            });
            s.accounts_add_sync(g).unwrap();
        };
        add(gsa("a@b.c", "old"));
        let origin = a.anisette_identity().unwrap();
        // A begins: what it saw. B signs the same Apple ID in again meanwhile (new token, same team)
        let seen = a.credentials_of("a@b.c").unwrap();
        add(gsa("a@b.c", "newer"));
        let e = a.commit_login(gsa("a@b.c", "older-login"), &seen, &origin).await.unwrap_err();
        assert!(matches!(e, SignError::LoginSuperseded), "{e}");
        assert_eq!(a.credentials_of("a@b.c").unwrap().unwrap().1, "newer", "kept");
        assert!(!e.is_transient() && !e.is_configuration());
        // another Apple ID: merged, the first one's kept
        let none = a.credentials_of("x@y.z").unwrap();
        a.commit_login(gsa("x@y.z", "t"), &none, &origin).await.unwrap();
        assert_eq!(a.credentials_of("a@b.c").unwrap().unwrap().1, "newer");
        assert!(a.credentials_of("x@y.z").unwrap().is_some());
        // the anisette state changed meanwhile (provisioned again): refused too
        let now = a.credentials_of("x@y.z").unwrap();
        std::fs::write(
            dir.join("state.plist"),
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict>\
             <key>keychain_identifier</key><data>AAAAAAAAAAAAAAAAAAAAAA==</data>\
             <key>adi_pb</key><data>AQID</data></dict></plist>",
        )
        .unwrap();
        assert!(matches!(a.commit_login(gsa("x@y.z", "t2"), &now, &origin).await, Err(SignError::LoginSuperseded)));
        // the same address both times, the state provisioned again meanwhile: another state, refused too
        let state = |pb: &str| {
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict>\
                 <key>keychain_identifier</key><data>AAAAAAAAAAAAAAAAAAAAAA==</data>\
                 <key>adi_pb</key><data>{pb}</data>\
                 <key>provisioned_by</key><string>https://ani.example.com</string></dict></plist>"
            )
        };
        std::fs::write(dir.join("state.plist"), state("AQID")).unwrap();
        let used = a.anisette_identity().unwrap();
        assert_eq!(used.origin, OriginCheck::Known);
        let now = a.credentials_of("x@y.z").unwrap();
        std::fs::write(dir.join("state.plist"), state("BAUG")).unwrap();
        assert_eq!(a.anisette_identity().unwrap().origin, OriginCheck::Known, "the same address");
        assert!(matches!(a.commit_login(gsa("x@y.z", "t3"), &now, &used).await, Err(SignError::LoginSuperseded)));
        std::fs::write(dir.join("state.plist"), state("AQID")).unwrap();
        a.commit_login(gsa("x@y.z", "t3"), &now, &used).await.unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The state recorded for a login is the one its request used: one provisioned in between (the same address) makes
    /// it read again, and what is recorded is the state that was there all through
    #[tokio::test]
    async fn the_state_recorded_is_the_one_used() {
        let (a, dir) = acct(false);
        let state = |pb: &str| {
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict>\
                 <key>keychain_identifier</key><data>AAAAAAAAAAAAAAAAAAAAAA==</data>\
                 <key>adi_pb</key><data>{pb}</data>\
                 <key>provisioned_by</key><string>https://ani.example.com</string></dict></plist>"
            )
        };
        std::fs::write(dir.join("state.plist"), state("AQID")).unwrap();
        let s1 = a.anisette_identity().unwrap();
        // the first use: provisioned again by another process meanwhile (S2); the second: nothing changes
        let mut uses = 0;
        let ((), used) = a
            .with_state_used(|| {
                uses += 1;
                if uses == 1 {
                    std::fs::write(dir.join("state.plist"), state("BAUG")).unwrap();
                }
                async { Ok(()) }
            })
            .await
            .unwrap();
        assert_eq!(uses, 2, "used again");
        assert_ne!(used, s1, "S2's, not S1's");
        assert_eq!(used, a.anisette_identity().unwrap());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The vendored account store writes `accounts.json` whole and private (vendor tests aren't run, so here)
    #[test]
    fn the_account_store_replaces_its_file_whole_and_private() {
        use std::os::unix::fs::PermissionsExt;
        let (_, dir) = acct(false);
        let path = dir.join("accounts.json");
        std::fs::write(&path, br#"{"selected_account":null,"accounts":{}}"#).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mut store = plume_store::AccountStore::load_sync(&Some(path.clone())).unwrap();
        store
            .accounts_add_sync(plume_store::GsaAccount::new(
                "a@b.c".into(),
                "A".into(),
                "id".into(),
                "tok".into(),
                "T".into(),
            ))
            .unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("a@b.c"));
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(left.len(), 1, "no temporary file left: {left:?}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Discarding the anisette state leaves nothing to send: the next use provisions with the configured address
    #[tokio::test]
    async fn a_discarded_state_is_provisioned_fresh() {
        let (a, dir) = acct(true);
        std::fs::write(dir.join("state.plist"), b"not a plist").unwrap();
        assert!(a.anisette_origin().is_err(), "unreadable is not missing");
        omnisette::state_file::discard(&dir).await.unwrap();
        assert_eq!(a.anisette_origin().unwrap(), OriginCheck::Fresh);
        a.check_origin().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
