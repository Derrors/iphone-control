//! CoreDevice HID 会话（iOS 27 起，由开发者镜像里的 `dtuhidd` 提供，免签名）。
//!
//! 坐标统一用点，内部换算成 0..=65535。会话持有显示媒体流：没有它，所有 HID 事件都会被系统静默丢弃，
//! 调用也不报错——只能用截图判断是否生效。
//! 按键表和 ASCII→HID 键码表来自 idevice v0.1.68 `tools/src/hid.rs`（MIT，Copyright Jackson Coxson）。

use std::time::Duration;

use idevice::{
    ReadWrite, RsdService,
    core_device::hid::{
        ButtonState, IndigoHidClient, TOUCHSCREEN_STATE_CONTACT, TOUCHSCREEN_STATE_RELEASE, UniversalHidServiceClient,
    },
    rsd::RsdHandshake,
    tcp::handle::AdapterHandle,
};

use crate::{DeviceError, Result, Rotation, ScreenSize, limit, media::ScreenMediaStream};

pub const HID_MAX: f64 = 65535.0;

/// HID 硬件按键
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Home,
    Lock,
    VolumeUp,
    VolumeDown,
}

impl Button {
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "home" => Self::Home,
            "lock" => Self::Lock,
            "volume_up" | "volume-up" => Self::VolumeUp,
            "volume_down" | "volume-down" => Self::VolumeDown,
            _ => return None,
        })
    }

    /// (usage page, usage code, 按住毫秒)
    fn usage(self) -> (u64, u64, u64) {
        match self {
            Self::Home => (0x0C, 0x40, 80),
            Self::Lock => (0x0C, 0x30, 200),
            Self::VolumeUp => (0x0C, 0xE9, 80),
            Self::VolumeDown => (0x0C, 0xEA, 80),
        }
    }
}

/// 手势时长的上限：再长的按住、拖动都是给错了数
const MAX_GESTURE: f64 = 60.0;

/// 手势的一段时长：负数当 0，超过 [`MAX_GESTURE`] 截到上限；不是有限数（`NaN`、无穷）的不做，
/// `Duration::from_secs_f64` 遇到它会 panic
fn gesture_time(seconds: f64) -> Result<Duration> {
    if !seconds.is_finite() {
        return Err(DeviceError::InvalidInput(ictl_l10n::trf!(
            "手势时长不是有效的数：{seconds}",
            "The gesture's duration isn't a valid number: {seconds}"
        )));
    }
    Ok(Duration::from_secs_f64(seconds.clamp(0.0, MAX_GESTURE)))
}

/// 界面上的点 → HID 归一化坐标。不是有限数的坐标、量不出的屏幕尺寸不换算：`NaN` 会悄悄变成 (0,0)，点到左上角
fn map_point(p: (f64, f64), screen: ScreenSize, rotation: Rotation) -> Result<(u16, u16)> {
    if !(p.0.is_finite() && p.1.is_finite()) {
        return Err(DeviceError::InvalidInput(ictl_l10n::trf!(
            "坐标不是有效的数：({}, {})",
            "The point isn't a valid number: ({}, {})",
            p.0,
            p.1
        )));
    }
    if !(screen.width.is_finite() && screen.height.is_finite() && screen.width > 0.0 && screen.height > 0.0) {
        return Err(DeviceError::InvalidInput(
            ictl_l10n::tr!("屏幕尺寸未知，没法换算坐标", "The screen size is unknown, so the point can't be mapped")
                .into(),
        ));
    }
    let (x, y) = rotation.to_portrait(p, screen);
    Ok(to_hid(x, y, screen))
}

/// 点 → HID 归一化坐标
pub fn to_hid(x: f64, y: f64, screen: ScreenSize) -> (u16, u16) {
    let n = |v: f64, max: f64| (v / max * HID_MAX).round().clamp(0.0, HID_MAX) as u16;
    (n(x, screen.width), n(y, screen.height))
}

/// ASCII 字符 → (HID 键盘 usage, 是否要 Shift)，美式布局
pub fn ascii_to_usage(c: char) -> Option<(u64, bool)> {
    Some(match c {
        'a'..='z' => (0x04 + (c as u64 - 'a' as u64), false),
        'A'..='Z' => (0x04 + (c as u64 - 'A' as u64), true),
        '1'..='9' => (0x1E + (c as u64 - '1' as u64), false),
        '0' => (0x27, false),
        '\n' => (0x28, false),
        '\t' => (0x2B, false),
        ' ' => (0x2C, false),
        '!' => (0x1E, true),
        '@' => (0x1F, true),
        '#' => (0x20, true),
        '$' => (0x21, true),
        '%' => (0x22, true),
        '^' => (0x23, true),
        '&' => (0x24, true),
        '*' => (0x25, true),
        '(' => (0x26, true),
        ')' => (0x27, true),
        '-' => (0x2D, false),
        '_' => (0x2D, true),
        '=' => (0x2E, false),
        '+' => (0x2E, true),
        '[' => (0x2F, false),
        '{' => (0x2F, true),
        ']' => (0x30, false),
        '}' => (0x30, true),
        '\\' => (0x31, false),
        '|' => (0x31, true),
        ';' => (0x33, false),
        ':' => (0x33, true),
        '\'' => (0x34, false),
        '"' => (0x34, true),
        '`' => (0x35, false),
        '~' => (0x35, true),
        ',' => (0x36, false),
        '<' => (0x36, true),
        '.' => (0x37, false),
        '>' => (0x37, true),
        '/' => (0x38, false),
        '?' => (0x38, true),
        _ => return None,
    })
}

const KEY_LEFT_SHIFT: u64 = 0xE1;
/// 左 Command（HID usage Left GUI）
const KEY_LEFT_GUI: u64 = 0xE3;
const KEY_V: u64 = 0x19;
const KEY_A: u64 = 0x04;

pub struct HidSession {
    /// 设备竖放时的尺寸：HID 触摸坐标按竖放归一化
    pub screen: ScreenSize,
    /// 界面现在转了多少；点击、拖动的坐标是界面上的，发出去前按它换算回竖放（调用方每次操作前设）
    pub rotation: Rotation,
    contact: Option<(u16, u16)>,
    stream: Option<ScreenMediaStream>,
    /// The screen video's bitrate cap, set when it started ([`crate::media::FULL_BITRATE`])
    max_bitrate: u32,
    touch: UniversalHidServiceClient<Box<dyn ReadWrite>>,
    indigo: IndigoHidClient<Box<dyn ReadWrite>>,
}

impl HidSession {
    pub(crate) async fn open(
        adapter: &mut AdapterHandle,
        handshake: &mut RsdHandshake,
        screen: ScreenSize,
        max_bitrate: u32,
    ) -> Result<Self> {
        let stream = crate::media::start(adapter, handshake, 1, max_bitrate).await?;
        // 时限加在这里而不是外层：外层取消会直接丢掉媒体流，走不到下面的 stop
        let clients =
            limit(ictl_l10n::tr!("连接 HID 服务", "connect to the HID service"), Duration::from_secs(20), async {
                // backboardd 要一点时间把 HID 设备和刚授权的媒体流对上
                tokio::time::sleep(Duration::from_millis(300)).await;
                let touch = UniversalHidServiceClient::connect_rsd(adapter, handshake).await?;
                let indigo = IndigoHidClient::connect_rsd(adapter, handshake).await?;
                Ok::<_, DeviceError>((touch, indigo))
            })
            .await;
        match clients {
            Ok((touch, indigo)) => Ok(Self {
                screen: screen.portrait(),
                rotation: Rotation::Portrait,
                contact: None,
                stream: Some(stream),
                max_bitrate,
                touch,
                indigo,
            }),
            Err(e) => {
                // 媒体流已经起来了：不停掉的话手机端会一直挂着它
                stream.stop().await;
                Err(e)
            }
        }
    }

    /// 界面上的点 → HID 归一化坐标（先按 [`Self::rotation`] 换算回竖放），见 [`map_point`]
    fn hid_point(&self, p: (f64, f64)) -> Result<(u16, u16)> {
        map_point(p, self.screen, self.rotation)
    }

    pub async fn tap(&mut self, x: f64, y: f64) -> Result<()> {
        let (hx, hy) = self.hid_point((x, y))?;
        limit(ictl_l10n::tr!("HID 点击", "HID tap"), Duration::from_secs(10), async {
            Ok(self.touch.tap(hx, hy).await?)
        })
        .await
    }

    /// 拖动：按时长换算采样数（每 20ms 一个点，至少 10 个），太快会被识别成点击
    pub async fn drag(&mut self, from: (f64, f64), to: (f64, f64), seconds: f64) -> Result<()> {
        let (x1, y1) = self.hid_point(from)?;
        let (x2, y2) = self.hid_point(to)?;
        let moving = gesture_time(seconds)?;
        let steps = ((moving.as_secs_f64() * 1000.0 / 20.0).round() as u32).max(10);
        let d = moving + Duration::from_secs(10);
        limit(ictl_l10n::tr!("HID 拖动", "HID drag"), d, async {
            Ok(self.touch.drag(x1, y1, x2, y2, steps, 20).await?)
        })
        .await
    }

    /// 按住再拖：按下后在起点按住 `hold` 秒（和 [`Self::long_press`] 一样每 100 毫秒重报一次位置），不抬起，接着
    /// 像 [`Self::drag`] 一样移到终点抬起。拖动主屏幕图标、列表排序这类要先长按才能拖的
    pub async fn held_drag(&mut self, from: (f64, f64), to: (f64, f64), hold: f64, seconds: f64) -> Result<()> {
        let (x1, y1) = self.hid_point(from)?;
        let (x2, y2) = self.hid_point(to)?;
        let held = gesture_time(hold)?;
        let moving = gesture_time(seconds)?;
        let steps = ((moving.as_secs_f64() * 1000.0 / 20.0).round() as u32).max(10);
        let d = held + moving + Duration::from_secs(10);
        limit(ictl_l10n::tr!("HID 按住拖动", "HID held drag"), d, async {
            let end = tokio::time::Instant::now() + held;
            self.touch.send_touchscreen(TOUCHSCREEN_STATE_CONTACT, x1, y1, None).await?;
            // the finger is down: if this fails or times out from here on, closing the session lifts it
            self.contact = Some((x1, y1));
            while tokio::time::Instant::now() < end {
                tokio::time::sleep(Duration::from_millis(100).min(end - tokio::time::Instant::now())).await;
                self.touch.send_touchscreen(TOUCHSCREEN_STATE_CONTACT, x1, y1, None).await?;
            }
            // drag() starts with a contact at (x1, y1): the finger stays down from the hold into the move
            self.touch.drag(x1, y1, x2, y2, steps, 20).await?;
            self.contact = None;
            Ok(())
        })
        .await
    }

    /// One sample of a finger that follows the user's pointer (the web viewer): `contact` true puts the finger at the
    /// point (the first one touches down, later ones move it), false lifts it there. iOS works out taps, long presses
    /// and the speed of a swipe from these samples and their timing, the same way as from a real finger
    pub async fn touch(&mut self, contact: bool, x: f64, y: f64) -> Result<()> {
        let (hx, hy) = self.hid_point((x, y))?;
        let state = if contact { TOUCHSCREEN_STATE_CONTACT } else { TOUCHSCREEN_STATE_RELEASE };
        limit(ictl_l10n::tr!("HID 触摸", "HID touch"), Duration::from_secs(5), async {
            self.touch.send_touchscreen(state, hx, hy, None).await?;
            self.contact = contact.then_some((hx, hy));
            Ok(())
        })
        .await
    }

    /// 长按：按下，按住 `seconds` 秒，抬起。按住时每 100 毫秒重报一次位置：只报一次的话，有的 App（微信）
    /// 会当成轻点（iCTRL 的做法）
    pub async fn long_press(&mut self, x: f64, y: f64, seconds: f64) -> Result<()> {
        let (hx, hy) = self.hid_point((x, y))?;
        let held = gesture_time(seconds)?;
        limit(ictl_l10n::tr!("HID 长按", "HID long press"), held + Duration::from_secs(10), async {
            let end = tokio::time::Instant::now() + held;
            self.touch.send_touchscreen(TOUCHSCREEN_STATE_CONTACT, hx, hy, None).await?;
            self.contact = Some((hx, hy));
            while tokio::time::Instant::now() < end {
                tokio::time::sleep(Duration::from_millis(100).min(end - tokio::time::Instant::now())).await;
                self.touch.send_touchscreen(TOUCHSCREEN_STATE_CONTACT, hx, hy, None).await?;
            }
            self.touch.send_touchscreen(TOUCHSCREEN_STATE_RELEASE, hx, hy, None).await?;
            self.contact = None;
            Ok(())
        })
        .await
    }

    pub async fn press(&mut self, button: Button) -> Result<()> {
        let (page, code, hold) = button.usage();
        limit(ictl_l10n::tr!("HID 按键", "HID key press"), Duration::from_secs(10), async {
            self.indigo.send_button(page, code, ButtonState::Down).await?;
            tokio::time::sleep(Duration::from_millis(hold)).await;
            Ok(self.indigo.send_button(page, code, ButtonState::Up).await?)
        })
        .await
    }

    /// 只能输入美式键盘直接按得出的 ASCII；有不支持的字符时一个都不输
    pub async fn type_ascii(&mut self, text: &str) -> Result<()> {
        let keys: Vec<_> = text
            .chars()
            .map(|c| {
                ascii_to_usage(c).ok_or_else(|| {
                    DeviceError::InvalidInput(ictl_l10n::trf!(
                        "HID 不能输入字符 {c:?}",
                        "HID can't type the character {c:?}"
                    ))
                })
            })
            .collect::<Result<_>>()?;
        let d = Duration::from_secs(10) + Duration::from_millis(50 * keys.len() as u64);
        limit(ictl_l10n::tr!("HID 输入", "HID typing"), d, self.send_keys(keys)).await
    }

    /// 按 Cmd-V（硬件键盘的粘贴）：按住 Cmd，按一下 V，松开 Cmd。手机不确认收到，粘没粘上只能看屏幕
    /// （iPhone16,2 / iOS 27.2.1 实测会触发粘贴，见 [`crate::Tunnel::paste_text`]）
    pub async fn paste_shortcut(&mut self) -> Result<()> {
        self.command_shortcut(KEY_V, ictl_l10n::tr!("HID 粘贴", "HID paste")).await
    }

    pub async fn select_all(&mut self) -> Result<()> {
        self.command_shortcut(KEY_A, ictl_l10n::tr!("HID 全选", "HID select all")).await
    }

    /// Delete (Backspace): with text selected, deletes it
    pub async fn delete_backward(&mut self) -> Result<()> {
        const DELETE: u64 = 0x2a;
        limit("HID Delete", Duration::from_secs(10), self.send_keys(vec![(DELETE, false)])).await
    }

    pub async fn collapse_selection(&mut self) -> Result<()> {
        const RIGHT_ARROW: u64 = 0x4f;
        limit("HID Right Arrow", Duration::from_secs(10), self.send_keys(vec![(RIGHT_ARROW, false)])).await
    }

    async fn command_shortcut(&mut self, key: u64, name: &str) -> Result<()> {
        limit(name, Duration::from_secs(10), async {
            self.indigo.send_keyboard(KEY_LEFT_GUI, ButtonState::Down).await?;
            let r = async {
                self.indigo.send_keyboard(key, ButtonState::Down).await?;
                self.indigo.send_keyboard(key, ButtonState::Up).await
            }
            .await;
            // Release Cmd even when the key event failed.
            let up = self.indigo.send_keyboard(KEY_LEFT_GUI, ButtonState::Up).await;
            r?;
            Ok(up?)
        })
        .await
    }

    async fn send_keys(&mut self, keys: Vec<(u64, bool)>) -> Result<()> {
        for (usage, shift) in keys {
            if shift {
                self.indigo.send_keyboard(KEY_LEFT_SHIFT, ButtonState::Down).await?;
            }
            self.indigo.send_keyboard(usage, ButtonState::Down).await?;
            self.indigo.send_keyboard(usage, ButtonState::Up).await?;
            if shift {
                self.indigo.send_keyboard(KEY_LEFT_SHIFT, ButtonState::Up).await?;
            }
            tokio::time::sleep(Duration::from_millis(12)).await;
        }
        Ok(())
    }

    /// 屏幕媒体流的画面（控制台用）；流已经停了时为 None
    /// The screen video's bitrate cap (bit/s): to change it, close the session and open another
    pub fn max_bitrate(&self) -> u32 {
        self.max_bitrate
    }

    pub fn screen_feed(&self) -> Option<crate::ScreenFeed> {
        self.stream.as_ref().map(|s| s.feed())
    }

    /// 停止屏幕媒体流。必须调用：直接丢掉的话手机端一直挂着这个会话（见 [`crate::Tunnel`]）
    pub async fn close(mut self) {
        if let Some((x, y)) = self.contact.take() {
            let _ = tokio::time::timeout(
                Duration::from_millis(500),
                self.touch.send_touchscreen(TOUCHSCREEN_STATE_RELEASE, x, y, None),
            )
            .await;
        }
        if let Some(s) = self.stream.take() {
            s.stop().await;
        }
    }
}

impl Drop for HidSession {
    fn drop(&mut self) {
        if self.stream.is_some() {
            tracing::warn!(
                "{}",
                ictl_l10n::log_tr!(
                    "HID 会话没有 close() 就被丢掉，手机端的屏幕媒体流会一直挂着（不自动锁屏），需拔插或重开一次 HID 清掉",
                    "HID session dropped without close(): the phone keeps its screen media stream open (no auto-lock); replug or open HID once more to clear it"
                )
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: ScreenSize = ScreenSize { width: 430.0, height: 932.0 };

    /// `NaN` or an infinity used to panic in `Duration::from_secs_f64`, or tap the top left corner (`NaN as u16` is 0)
    #[test]
    fn only_finite_points_and_durations_reach_the_phone() {
        let p = |x, y| map_point((x, y), SCREEN, Rotation::Portrait);
        assert_eq!(p(0.0, 0.0).unwrap(), (0, 0));
        assert_eq!(p(430.0, 932.0).unwrap(), (65535, 65535));
        for (x, y) in [(f64::NAN, 1.0), (1.0, f64::INFINITY), (f64::NEG_INFINITY, 1.0)] {
            assert!(matches!(p(x, y), Err(DeviceError::InvalidInput(_))), "({x}, {y})");
        }
        for screen in [ScreenSize { width: 0.0, height: 932.0 }, ScreenSize { width: f64::NAN, height: 932.0 }] {
            assert!(matches!(map_point((1.0, 1.0), screen, Rotation::Portrait), Err(DeviceError::InvalidInput(_))));
        }
        assert_eq!(gesture_time(0.5).unwrap(), Duration::from_millis(500));
        assert_eq!(gesture_time(-3.0).unwrap(), Duration::ZERO);
        assert_eq!(gesture_time(1e9).unwrap(), Duration::from_secs(60), "capped");
        for s in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(matches!(gesture_time(s), Err(DeviceError::InvalidInput(_))), "{s}");
        }
    }

    #[test]
    fn hid_norm_corners_center_and_clamp() {
        assert_eq!(to_hid(0.0, 0.0, SCREEN), (0, 0));
        assert_eq!(to_hid(430.0, 932.0, SCREEN), (65535, 65535));
        assert_eq!(to_hid(215.0, 466.0, SCREEN), (32768, 32768));
        assert_eq!(to_hid(-5.0, 2000.0, SCREEN), (0, 65535));
    }

    #[test]
    fn ascii_usage_covers_letters_digits_and_shifted_symbols() {
        assert_eq!(ascii_to_usage('a'), Some((0x04, false)));
        assert_eq!(ascii_to_usage('Z'), Some((0x1D, true)));
        assert_eq!(ascii_to_usage('0'), Some((0x27, false)));
        assert_eq!(ascii_to_usage('?'), Some((0x38, true)));
        assert_eq!(ascii_to_usage('设'), None);
    }

    #[test]
    fn button_names_match_toolkit() {
        assert_eq!(Button::parse("volume_up"), Some(Button::VolumeUp));
        assert_eq!(Button::parse("home"), Some(Button::Home));
        assert_eq!(Button::parse("foo"), None);
    }
}
