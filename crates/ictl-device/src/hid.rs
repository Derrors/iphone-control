//! CoreDevice HID 会话（iOS 27 起，由开发者镜像里的 `dtuhidd` 提供，免签名）。
//!
//! 坐标统一用点，内部换算成 0..=65535。会话持有显示媒体流：没有它，所有 HID 事件都会被系统静默丢弃，
//! 调用也不报错——只能用截图判断是否生效。
//! 按键表和 ASCII→HID 键码表来自 idevice v0.1.68 `tools/src/hid.rs`（MIT，Copyright Jackson Coxson）。

use std::time::Duration;

use idevice::{
    ReadWrite, RsdService,
    core_device::hid::{ButtonState, IndigoHidClient, UniversalHidServiceClient},
    rsd::RsdHandshake,
    tcp::handle::AdapterHandle,
};

use crate::{DeviceError, Result, ScreenSize, media::ScreenMediaStream};

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

pub struct HidSession {
    pub screen: ScreenSize,
    stream: Option<ScreenMediaStream>,
    touch: UniversalHidServiceClient<Box<dyn ReadWrite>>,
    indigo: IndigoHidClient<Box<dyn ReadWrite>>,
}

impl HidSession {
    pub(crate) async fn open(adapter: &mut AdapterHandle, handshake: &mut RsdHandshake, screen: ScreenSize) -> Result<Self> {
        let stream = crate::media::start(adapter, handshake, 1).await?;
        // backboardd 要一点时间把 HID 设备和刚授权的媒体流对上
        tokio::time::sleep(Duration::from_millis(300)).await;
        let touch = UniversalHidServiceClient::connect_rsd(adapter, handshake).await?;
        let indigo = IndigoHidClient::connect_rsd(adapter, handshake).await?;
        Ok(Self { screen, stream: Some(stream), touch, indigo })
    }

    pub async fn tap(&mut self, x: f64, y: f64) -> Result<()> {
        let (hx, hy) = to_hid(x, y, self.screen);
        Ok(self.touch.tap(hx, hy).await?)
    }

    /// 拖动：按时长换算采样数（每 20ms 一个点，至少 10 个），太快会被识别成点击
    pub async fn drag(&mut self, from: (f64, f64), to: (f64, f64), seconds: f64) -> Result<()> {
        let (x1, y1) = to_hid(from.0, from.1, self.screen);
        let (x2, y2) = to_hid(to.0, to.1, self.screen);
        let steps = ((seconds * 1000.0 / 20.0).round() as u32).max(10);
        Ok(self.touch.drag(x1, y1, x2, y2, steps, 20).await?)
    }

    pub async fn press(&mut self, button: Button) -> Result<()> {
        let (page, code, hold) = button.usage();
        self.indigo.send_button(page, code, ButtonState::Down).await?;
        tokio::time::sleep(Duration::from_millis(hold)).await;
        Ok(self.indigo.send_button(page, code, ButtonState::Up).await?)
    }

    /// 只能输入美式键盘直接按得出的 ASCII；有不支持的字符时一个都不输
    pub async fn type_ascii(&mut self, text: &str) -> Result<()> {
        let keys: Vec<_> = text
            .chars()
            .map(|c| ascii_to_usage(c).ok_or_else(|| DeviceError::Idevice(format!("HID 不能输入字符 {c:?}"))))
            .collect::<Result<_>>()?;
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

    /// 停止媒体流。不调用也行，但设备那边的流要等超时才结束
    pub async fn close(mut self) {
        if let Some(s) = self.stream.take() {
            s.stop().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: ScreenSize = ScreenSize { width: 430.0, height: 932.0 };

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
