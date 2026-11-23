//! What went wrong carrying out a request. The caller (an agent, the CLI, the control page) gets its text, worded
//! for it and in its language; inside the daemon the cause stays what it was (WDA's, the device's, unlocking's),
//! so `?` passes it on without turning it into a string first, and code can still look at which one it was.

use crate::protocol::{Delivery, FailureStage, Response};
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
    #[error("{message}")]
    Action { delivery: Delivery, stage: FailureStage, message: String },
}

impl Error {
    pub fn action(delivery: Delivery, stage: FailureStage, message: impl std::fmt::Display) -> Self {
        Self::Action { delivery, stage, message: message.to_string() }
    }

    pub fn preflight(message: impl std::fmt::Display) -> Self {
        Self::action(Delivery::NotSent, FailureStage::Preflight, message)
    }

    pub fn readback(message: impl std::fmt::Display) -> Self {
        Self::action(Delivery::Sent, FailureStage::Readback, message)
    }

    pub fn wda_input(e: WdaError) -> Self {
        let (delivery, stage) = match e {
            WdaError::BeforeInput(_) => (Delivery::NotSent, FailureStage::Preflight),
            WdaError::Unavailable(_) => (Delivery::NotSent, FailureStage::Connect),
            WdaError::Write(_) => (Delivery::Unknown, FailureStage::Send),
            _ => (Delivery::Unknown, FailureStage::Reply),
        };
        Self::action(delivery, stage, e)
    }

    /// Add caller-facing context without losing the original delivery classification.
    pub fn with_message(self, message: String) -> Self {
        match self {
            Self::Action { delivery, stage, .. } => Self::Action { delivery, stage, message },
            _ => Self::Msg(message),
        }
    }

    pub fn response(self) -> Response {
        match self {
            Self::Action { delivery, stage, message } => Response::error(message).with_failure(delivery, stage),
            e => Response::error(e),
        }
    }
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

    #[test]
    fn input_stage_survives_conversion_to_a_daemon_reply() {
        let tap = crate::protocol::Request::Tap { x: 0., y: 0., count: 1 };
        for (e, delivery, stage) in [
            (
                WdaError::BeforeInput(Box::new(WdaError::Timeout("session".into()))),
                Delivery::NotSent,
                FailureStage::Preflight,
            ),
            (WdaError::Unavailable("offline".into()), Delivery::NotSent, FailureStage::Connect),
            (WdaError::Write("partial".into()), Delivery::Unknown, FailureStage::Send),
            (WdaError::Timeout("no reply".into()), Delivery::Unknown, FailureStage::Reply),
        ] {
            let r = Error::wda_input(e).response();
            assert_eq!(r.delivery(&tap), delivery);
            assert_eq!(r.failure_stage(), Some(stage));
        }
        assert_eq!(Response::error("older daemon").delivery(&tap), Delivery::Unknown);
    }

    /// The caller reads the same text as before: a cause's own, or the daemon's message
    #[test]
    fn the_text_is_the_cause_s_own() {
        let e: Error = WdaError::StartFailed("x".into()).into();
        assert_eq!(e.to_string(), WdaError::StartFailed("x".into()).to_string());
        assert!(matches!(e, Error::Wda(WdaError::StartFailed(_))), "and the cause is still there");
        assert_eq!(String::from(Error::from("手机未连接")), "手机未连接");
    }
}
