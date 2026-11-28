//! Acting at a point or on an element: taps, touches, long presses, swipes, typing

mod gesture;
mod search;
mod text;

#[cfg(test)]
mod tests;

use super::scroll;

pub(crate) use gesture::Stroke;
pub(crate) use search::TapTarget;
#[cfg(test)]
pub(super) use search::{reachable, reachable_exact, search_stroke};
#[cfg(test)]
pub(super) use text::missing_suffix;
pub(super) use text::{bare, newly_typed, replaced, typed_but_whitespace};
