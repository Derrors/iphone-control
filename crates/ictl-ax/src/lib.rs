//! The phone's accessibility tree through `com.apple.accessibility.axAuditDaemon.remoteAXService`, the service
//! Xcode 27's Device Hub uses, without WebDriverAgent (iOS 27 and later).
//!
//! - [`archive`]: NSKeyedArchiver archives (the protocol's payloads)
//! - [`dtx`]: the DTX messages the service speaks
//! - [`session`]: the handshake, the trees the phone pushes after every change, requests
//! - [`tree`]: elements and their attributes
//! - [`node`]: a read as the element tree WebDriverAgent's `/source` gives (`ictl_ui::Source`), so `ictl_ui::Tree::of`
//!   reads it as it reads WDA's
//!
//! No device I/O of its own: `ictl-device` opens the connection through its tunnel and hands it to
//! [`session::Session::connect`]. Protocol details: `docs/dev/ios27-native.md`.

pub mod archive;
pub mod dtx;
pub mod node;
pub mod session;
pub mod tree;

pub use archive::{Obj, ObjectRef};
pub use session::{PushedAt, Response, Session};
pub use tree::{Element, Pushed};

/// The service's name in the phone's RSD list
pub const SERVICE: &str = "com.apple.accessibility.axAuditDaemon.remoteAXService";

#[derive(Debug, thiserror::Error)]
pub enum AxError {
    #[error("remoteAXService: {0}")]
    Io(#[from] std::io::Error),
    #[error("remoteAXService: archive: {0}")]
    Archive(String),
    #[error("remoteAXService: {0}")]
    Protocol(String),
    #[error("remoteAXService: no answer to {0} in time")]
    Timeout(&'static str),
    #[error("remoteAXService: connection closed")]
    Closed,
}

pub type Result<T> = std::result::Result<T, AxError>;
