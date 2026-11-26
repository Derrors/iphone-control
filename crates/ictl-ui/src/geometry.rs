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

impl ScreenSize {
    /// 设备竖放时的尺寸（短边是宽）
    pub fn portrait(self) -> Self {
        Self { width: self.width.min(self.height), height: self.width.max(self.height) }
    }
}

impl Rotation {
    pub fn from_degrees(z: f64) -> Option<Self> {
        if !z.is_finite() || z.fract() != 0.0 {
            return None;
        }
        Some(match z.rem_euclid(360.0) as u16 {
            0 => Self::Portrait,
            90 => Self::TopToRight,
            180 => Self::UpsideDown,
            270 => Self::TopToLeft,
            _ => return None,
        })
    }

    pub fn degrees(self) -> u16 {
        match self {
            Self::Portrait => 0,
            Self::TopToRight => 90,
            Self::UpsideDown => 180,
            Self::TopToLeft => 270,
        }
    }

    pub fn is_landscape(self) -> bool {
        matches!(self, Self::TopToRight | Self::TopToLeft)
    }

    /// 界面上的点 → 设备竖放时的点。`portrait`：竖放时的屏幕尺寸
    pub fn to_portrait(self, (x, y): (f64, f64), portrait: ScreenSize) -> (f64, f64) {
        let (w, h) = (portrait.width, portrait.height);
        match self {
            Self::Portrait => (x, y),
            Self::UpsideDown => (w - x, h - y),
            Self::TopToLeft => (w - y, x),
            Self::TopToRight => (y, h - x),
        }
    }

    /// WDA 的 `GET /orientation`。两个横屏方向它都可能报 `LANDSCAPE`（iPad mini 5 主屏上），按顶边在左算
    pub fn from_wda_orientation(s: &str) -> Option<Self> {
        Some(match s {
            "PORTRAIT" => Self::Portrait,
            "UIA_DEVICE_ORIENTATION_PORTRAIT_UPSIDEDOWN" => Self::UpsideDown,
            "LANDSCAPE" => Self::TopToLeft,
            "UIA_DEVICE_ORIENTATION_LANDSCAPERIGHT" => Self::TopToRight,
            _ => return None,
        })
    }

    /// 把任一方向下量到的屏幕尺寸换成这个方向下的（横屏宽大于高，竖屏反过来）
    pub fn orient(self, s: ScreenSize) -> ScreenSize {
        let p = s.portrait();
        if self.is_landscape() { ScreenSize { width: p.height, height: p.width } } else { p }
    }
}

impl From<ScreenSize> for View {
    fn from(screen: ScreenSize) -> Self {
        Self { screen, rotation: None }
    }
}

impl Rotation {
    /// 元素树坐标到屏幕坐标的换算。iPad 横屏时 SpringBoard（主屏、长按菜单、锁屏）给的是竖屏坐标：
    /// 窗口 768x1024，屏幕 1024x768；App 里给的是屏幕坐标（iPadOS 26.6.1 真机）。点击用的是屏幕坐标。
    /// 往哪边转看状态栏：它在竖屏坐标里是一条贴边的竖条，贴哪边，哪边就是屏幕顶边。
    /// WDA 报的方向靠不住：iPad mini 5 两个横屏方向在主屏上都报 `LANDSCAPE`（rotation z=90）
    ///
    /// 第一个窗口的宽高和横屏的屏幕正好相反：树是竖屏坐标，按状态栏（`status_bar`，树里第一个 StatusBar）贴的边转。
    /// 树里没有贴边的状态栏时退回看 WDA 报的方向；方向也不知道就不转（[`Rotation::Portrait`]）
    pub(super) fn of_tree(window: Rect, view: View, status_bar: Option<Rect>) -> Self {
        let s = view.screen;
        let swapped = s.width > s.height && (window.w - s.height).abs() < 1.0 && (window.h - s.width).abs() < 1.0;
        if !swapped {
            return Self::Portrait;
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
        match view.rotation {
            Some(r) if r.is_landscape() => r,
            o => {
                tracing::warn!(
                    "{}",
                    ictl_l10n::log_trf!(
                        "元素树是竖屏坐标而屏幕是横的，但没有状态栏、方向是 {o:?}，坐标不换算",
                        "The element tree uses portrait coordinates while the screen is landscape, but there is no status bar and the orientation is {o:?}; not converting coordinates"
                    )
                );
                Self::Portrait
            }
        }
    }

    /// 树里第一个 StatusBar 的原始坐标（不管嵌在哪层，状态栏都是整屏坐标）
    pub(super) fn tree_status_bar(xml: &str) -> Option<Rect> {
        static TAG: LazyLock<Regex> = LazyLock::new(|| regex(r"<XCUIElementTypeStatusBar\b[^>]*>"));
        static ATTR: LazyLock<Regex> = LazyLock::new(|| regex(r#"\b(x|y|width|height)="([^"]*)""#));
        let tag = TAG.find(xml)?.as_str();
        let attr = |k: &str| -> Option<f64> { ATTR.captures_iter(tag).find(|c| &c[1] == k)?[2].parse().ok() };
        Some(Rect { x: attr("x")?, y: attr("y")?, w: attr("width")?, h: attr("height")? })
    }

    /// 竖屏坐标的 `r` 在屏幕上的位置（`s`：屏幕，横的）。只换算两个横屏方向，其余原样：
    /// 顶边在左 (x, y) → (y, 屏幕高 - x)，顶边在右 (x, y) → (屏幕宽 - y, x)
    pub(super) fn tree_rect(self, r: Rect, s: ScreenSize) -> Rect {
        match self {
            Self::Portrait | Self::UpsideDown => r,
            Self::TopToLeft => Rect { x: r.y, y: s.height - r.x - r.w, w: r.h, h: r.w },
            Self::TopToRight => Rect { x: s.width - r.y - r.h, y: r.x, w: r.h, h: r.w },
        }
    }
}
