//! 矩形、界面方向、元素树坐标到屏幕坐标的换算

use super::*;

impl Rect {
    pub(super) fn contains(&self, (px, py): (f64, f64)) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }

    /// 整个盖住 `o`（各边容差 1 点）
    pub(super) fn covers(&self, o: &Rect) -> bool {
        self.x <= o.x + 1.0
            && self.y <= o.y + 1.0
            && self.x + self.w >= o.x + o.w - 1.0
            && self.y + self.h >= o.y + o.h - 1.0
    }
}

impl Orientation {
    pub fn from_wda(s: &str) -> Option<Self> {
        Some(match s {
            "PORTRAIT" => Self::Portrait,
            "UIA_DEVICE_ORIENTATION_PORTRAIT_UPSIDEDOWN" => Self::PortraitUpsideDown,
            "LANDSCAPE" => Self::LandscapeLeft,
            "UIA_DEVICE_ORIENTATION_LANDSCAPERIGHT" => Self::LandscapeRight,
            _ => return None,
        })
    }

    pub fn is_landscape(self) -> bool {
        matches!(self, Self::LandscapeLeft | Self::LandscapeRight)
    }

    /// 把任一方向下量到的屏幕尺寸换成这个方向下的（横屏宽大于高，竖屏反过来）
    pub fn orient(self, s: ScreenSize) -> ScreenSize {
        let (short, long) = (s.width.min(s.height), s.width.max(s.height));
        if self.is_landscape() {
            ScreenSize { width: long, height: short }
        } else {
            ScreenSize { width: short, height: long }
        }
    }
}

impl From<ScreenSize> for View {
    fn from(screen: ScreenSize) -> Self {
        Self { screen, orientation: None }
    }
}

impl Rotate {
    /// 第一个窗口的宽高和横屏的屏幕正好相反：树是竖屏坐标，按状态栏（`status_bar`，树里第一个 StatusBar）贴的边转。
    /// 树里没有贴边的状态栏时退回看 WDA 报的方向；方向也不知道就不转
    pub(super) fn detect(window: Rect, view: View, status_bar: Option<Rect>) -> Self {
        let s = view.screen;
        let swapped = s.width > s.height && (window.w - s.height).abs() < 1.0 && (window.h - s.width).abs() < 1.0;
        if !swapped {
            return Self::None;
        }
        // 竖屏坐标里贴边的竖条：贴左边（x=0）是 Home 键在左，贴右边（x+w=屏幕高）是 Home 键在右
        if let Some(b) = status_bar.filter(|b| (b.h - s.width).abs() < 1.0 && b.w > 0.0 && b.w < s.height / 4.0) {
            if b.x.abs() < 1.0 {
                return Self::TopToRight;
            }
            if (b.x + b.w - s.height).abs() < 1.0 {
                return Self::TopToLeft;
            }
        }
        match view.orientation {
            Some(Orientation::LandscapeLeft) => Self::TopToLeft,
            Some(Orientation::LandscapeRight) => Self::TopToRight,
            o => {
                tracing::warn!(
                    "{}",
                    ictl_l10n::log_trf!(
                        "元素树是竖屏坐标而屏幕是横的，但没有状态栏、方向是 {o:?}，坐标不换算",
                        "The element tree uses portrait coordinates while the screen is landscape, but there is no status bar and the orientation is {o:?}; not converting coordinates"
                    )
                );
                Self::None
            }
        }
    }

    /// 树里第一个 StatusBar 的原始坐标（不管嵌在哪层，状态栏都是整屏坐标）
    pub(super) fn status_bar(xml: &str) -> Option<Rect> {
        static TAG: LazyLock<Regex> = LazyLock::new(|| regex(r"<XCUIElementTypeStatusBar\b[^>]*>"));
        static ATTR: LazyLock<Regex> = LazyLock::new(|| regex(r#"\b(x|y|width|height)="([^"]*)""#));
        let tag = TAG.find(xml)?.as_str();
        let attr = |k: &str| -> Option<f64> { ATTR.captures_iter(tag).find(|c| &c[1] == k)?[2].parse().ok() };
        Some(Rect { x: attr("x")?, y: attr("y")?, w: attr("width")?, h: attr("height")? })
    }

    pub(super) fn apply(self, r: Rect, s: ScreenSize) -> Rect {
        match self {
            Self::None => r,
            Self::TopToLeft => Rect { x: r.y, y: s.height - r.x - r.w, w: r.h, h: r.w },
            Self::TopToRight => Rect { x: s.width - r.y - r.h, y: r.x, w: r.h, h: r.w },
        }
    }
}
