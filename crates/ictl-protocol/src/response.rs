//! Control protocol response, outcome, structured data, task metadata, and screen stream frame types.

use serde::{Deserialize, Serialize};

#[cfg(doc)]
use crate::BACKGROUND;
use crate::Request;

/// One event of a task, as the daemon streams it and the control page shows it: `step`, `confirm`, `state`, `done`
/// (and `task`, the first one, with the task's [`TaskInfo`])
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskEvent {
    pub event: String,
    pub data: serde_json::Value,
}

/// A task as [`Request::TaskList`] lists it
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskInfo {
    pub id: String,
    pub title: String,
    pub owner: String,
    pub udid: String,
    /// `running` / `paused` / `takeover` / `waiting`, or how it ended: `completed` / `failed` / `cancelled` /
    /// `rejected` / `abandoned` / `interrupted`
    pub state: String,
    /// Unix milliseconds
    pub started: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<u64>,
    /// Steps so far
    #[serde(default)]
    pub steps: u64,
    /// Bundle IDs the task may act in (empty: no limit), including those the user let it into
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub apps: Vec<String>,
}

impl TaskInfo {
    pub fn active(&self) -> bool {
        matches!(self.state.as_str(), "running" | "paused" | "takeover" | "waiting")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Text {
        text: String,
        /// What the text reports, as fields, so clients don't have to read the text (which depends on the
        /// language). Daemons before this field existed don't send it; their text is always Chinese
        #[serde(default, skip_serializing_if = "Option::is_none")]
        outcome: Option<Outcome>,
        /// The same facts as data, for programs such as the control page ([`Data`]): the text is worded for people
        /// and agents, in their language, and may change. Only some requests have it
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<Data>,
    },
    /// JPEG base64 + 说明文字（尺寸、放大倍数、坐标换算式）
    Image { jpeg_base64: String, caption: String },
    Error {
        message: String,
        /// The touch Down could not open a HID session and sent no contact report.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        retry_as_gesture: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failure: Option<ActionFailure>,
    },
}

/// Delivery of the requested input. Acknowledging the input does not verify its effect on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    NotSent,
    Unknown,
    Sent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureStage {
    Preflight,
    Connect,
    Send,
    Reply,
    Readback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionFailure {
    pub delivery: Delivery,
    pub stage: FailureStage,
}

/// Facts about a [`Response::Text`] that clients act on
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    /// `iphone_tap_element` tapped nothing: no match, index out of range, or the element is covered
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tapped_nothing: bool,
    /// The text already reports a system alert blocking the screen
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub alert_reported: bool,
    /// Non-empty text input: whether a subsequent field read saw the requested text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field_verified: Option<bool>,
    /// A replacement can verify the desired value without sending any input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<Delivery>,
}

/// What [`Request::Status`], [`Request::ListElements`], [`Request::ListApps`] and [`Request::AlertCheck`] report, as
/// data ([`Response::Text`]'s `data`)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Data {
    Status(StatusData),
    Elements {
        elements: Vec<ElementData>,
        /// Matches in the observed tree before the caller's limit. Older daemons do not report this.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        matched_count: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        truncated: Option<bool>,
    },
    ScrollCollection {
        stop_reason: ScrollStop,
        scrolls: u32,
        rows: Vec<ScrollRow>,
        matched: Option<Box<ScrollMatch>>,
    },
    Apps {
        apps: Vec<AppData>,
    },
    /// A system alert in front (its text), or none
    Alert {
        text: Option<String>,
    },
    /// Nothing was read: a [`BACKGROUND`] request gave way to an action
    Busy,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StatusData {
    pub wda_ready: bool,
    /// `None` when WDA couldn't say (it isn't running)
    pub locked: Option<bool>,
    /// Looks like the lock screen though WDA says unlocked (`locked` is then true too)
    pub possibly_locked: bool,
    /// The foreground app's bundle ID
    pub foreground: Option<String>,
    /// The screen in points, as it is turned now
    pub width: Option<f64>,
    pub height: Option<f64>,
    /// Driven natively (iOS 27 and later without WDA: remoteAXService, HID, CoreDevice)
    #[serde(default)]
    pub native: bool,
}

/// An element on screen: its type, the text it shows, and its box in points (top left, size)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ElementData {
    #[serde(rename = "type")]
    pub kind: String,
    pub label: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// The names are stable across daemon and MCP languages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScrollStop {
    Interrupted,
    Found,
    End,
    MaxScrolls,
    Limit,
}

/// The matched element's visible bounds and an uncovered point in the final viewport.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScrollMatch {
    #[serde(flatten)]
    pub element: ElementData,
    pub point: (f64, f64),
}

/// A row observed along the way. Its original screen coordinates can already be stale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScrollRow {
    #[serde(rename = "type")]
    pub kind: String,
    pub label: String,
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppData {
    pub name: String,
    pub bundle_id: String,
}

impl Response {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text { text: s.into(), outcome: None, data: None }
    }
    /// Text with its [`Outcome`] (the daemon answers every text request this way)
    pub fn text_with(s: impl Into<String>, outcome: Outcome) -> Self {
        Self::Text { text: s.into(), outcome: Some(outcome), data: None }
    }
    /// Text with its [`Outcome`] and the same facts as [`Data`]
    pub fn text_data(s: impl Into<String>, outcome: Outcome, data: Option<Data>) -> Self {
        Self::Text { text: s.into(), outcome: Some(outcome), data }
    }
    pub fn error(s: impl Into<String>) -> Self {
        Self::Error { message: s.into(), retry_as_gesture: false, failure: None }
    }
    pub fn with_failure(mut self, delivery: Delivery, stage: FailureStage) -> Self {
        if let Self::Error { failure, .. } = &mut self {
            *failure = Some(ActionFailure { delivery, stage });
        }
        self
    }

    pub fn failure_stage(&self) -> Option<FailureStage> {
        match self {
            Self::Error { failure, .. } => failure.map(|f| f.stage),
            _ => None,
        }
    }

    pub fn field_readback(&self, req: &Request) -> Option<&'static str> {
        if !matches!(req, Request::ReplaceText { .. })
            && !matches!(req, Request::TypeText { text, .. } if !text.is_empty())
        {
            return None;
        }
        match self {
            Self::Text { outcome: Some(o), .. } => o.field_verified.map(|v| if v { "verified" } else { "unverified" }),
            Self::Error { failure: Some(f), .. } if f.stage == FailureStage::Readback => Some("failed"),
            _ => None,
        }
    }

    /// For callers reporting a device action. Older daemons have no error classification.
    pub fn delivery(&self, req: &Request) -> Delivery {
        match self {
            Self::Text { outcome: Some(o), .. } if matches!(req, Request::TapElement { .. }) && o.tapped_nothing => {
                Delivery::NotSent
            }
            Self::Text { .. } if matches!(req, Request::TypeText { text, submit: false } if text.is_empty()) => {
                Delivery::NotSent
            }
            Self::Text { outcome, .. } if matches!(req, Request::ReplaceText { .. }) => {
                outcome.and_then(|o| o.delivery).unwrap_or(Delivery::Unknown)
            }
            Self::Text { .. } if req.reports_delivery() => Delivery::Sent,
            Self::Error { failure: Some(f), .. } => f.delivery,
            _ => Delivery::Unknown,
        }
    }
}

/// Messages after a [`Request::ScreenStream`] answer: one type byte, a 4-byte big-endian length, the payload
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ScreenMsg {
    /// JSON: `codec` (WebCodecs string), before the first frame and whenever it changes
    Config = 0,
    /// A keyframe, Annex-B, with its parameter sets
    Key = 1,
    /// Any other frame, Annex-B
    Delta = 2,
    /// JSON: `rotation` in degrees (0, 90, 180, 270: how far the portrait top edge has turned clockwise), when it
    /// becomes known or changes. The video itself is always portrait
    Info = 3,
}

impl ScreenMsg {
    pub fn from_byte(b: u8) -> Option<Self> {
        Some(match b {
            0 => Self::Config,
            1 => Self::Key,
            2 => Self::Delta,
            3 => Self::Info,
            _ => return None,
        })
    }

    /// The message with its header
    pub fn frame(self, payload: &[u8]) -> Vec<u8> {
        let mut m = Vec::with_capacity(5 + payload.len());
        m.push(self as u8);
        m.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        m.extend_from_slice(payload);
        m
    }
}

/// The byte a [`Request::ScreenStream`] client writes to ask for a keyframe
pub const SCREEN_KEYFRAME: u8 = b'K';
