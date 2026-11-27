//! Native control of iOS 27 and later, without WebDriverAgent (`ICTL_IOS_BACKEND`, docs/dev/ios27-native.md):
//! the screen through remoteAXService (`ictl_ax`, made into the element tree WDA's `/source` gives), touches,
//! keys and Home through HID, text through the pasteboard, apps and links through CoreDevice AppService, the lock
//! state through lockdown.
//!
//! The tools keep their WDA code; where WDA isn't there they come here when [`Daemon::native`] says so.

mod app;
mod ax;
mod hid;
mod unlock;

#[cfg(test)]
mod tests;

use ictl_device::DeviceInfo;

use super::observe;
use super::*;
use crate::config::IosBackend;

pub(crate) use ax::AX_WAIT;

/// See [`IosBackend::drives_natively`]
fn native_for(backend: IosBackend, info: Option<&DeviceInfo>) -> bool {
    backend.drives_natively(info)
}

impl Daemon {
    /// The current phone is driven natively (no WDA): see [`native_for`]
    pub(crate) fn native(&self, g: &Inner) -> bool {
        native_for(self.cfg.ios_backend, g.info.as_ref())
    }
}
