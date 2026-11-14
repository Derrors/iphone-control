//! What went wrong carrying out a request. The caller (an agent, the CLI, the control page) gets its text, worded
//! for it and in its language; inside the daemon the cause stays what it was (WDA's, the device's, unlocking's),
//! so `?` passes it on without turning it into a string first, and code can still look at which one it was.

use ictl_device::DeviceError;
use ictl_unlock::UnlockError;
use ictl_wda::WdaError;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Wda(#[from] WdaError),
    #[error(transparent)]
    Device(#[from] DeviceError),
    #[error(transparent)]
    Unlock(#[from] UnlockError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The daemon's own message, already worded for the caller
    #[error("{0}")]
    Msg(String),
}

/// The daemon's result: its [`Error`] unless another is named (`Result<(), Response>`)
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl From<String> for Error {
    fn from(m: String) -> Self {
        Self::Msg(m)
    }
}

impl From<&str> for Error {
    fn from(m: &str) -> Self {
        Self::Msg(m.to_owned())
    }
}

/// The text the caller gets ([`crate::protocol::Response::error`] takes it)
impl From<Error> for String {
    fn from(e: Error) -> Self {
        e.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The caller reads the same text as before: a cause's own, or the daemon's message
    #[test]
    fn the_text_is_the_cause_s_own() {
        let e: Error = WdaError::StartFailed("x".into()).into();
        assert_eq!(e.to_string(), WdaError::StartFailed("x".into()).to_string());
        assert!(matches!(e, Error::Wda(WdaError::StartFailed(_))), "and the cause is still there");
        assert_eq!(String::from(Error::from("手机未连接")), "手机未连接");
    }
}
