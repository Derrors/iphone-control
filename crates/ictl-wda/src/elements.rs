//! 元素树解析（纯函数，单测用脱敏 XML 样本）。
//!
//! 规则（来自 toolkit）：
//! - 只保留有 label/name/value 的元素
//! - 按坐标判断是否在屏幕内；铺满全屏的 Application/Window/Toolbar 之类去掉
//! - 父子重复（同文字同位置）去重
//! - 点击中心取元素在屏幕内的部分
//! - 锁屏密码键盘的 `Key` 元素不显示时也在树里，**不能**据此判断键盘已出现

use ictl_device::ScreenSize;

#[derive(Debug, Clone, PartialEq)]
pub struct Element {
    pub kind: String,
    pub label: String,
    pub name: String,
    pub value: String,
    pub center: (i64, i64),
    pub size: (i64, i64),
}

pub fn parse(xml: &str, screen: ScreenSize) -> Vec<Element> {
    let _ = (xml, screen);
    todo!("quick-xml 遍历")
}

/// 按文字找元素：先完全相同，再包含（不区分大小写）
pub fn find<'a>(elements: &'a [Element], text: &str) -> Vec<&'a Element> {
    let _ = (elements, text);
    todo!()
}

/// 锁屏键盘 0～9 的中心点；没找齐 10 个返回 None
pub fn keypad(elements: &[Element]) -> Option<[(i64, i64); 10]> {
    let _ = elements;
    todo!()
}

/// 解析密码栏的值「共6位数，已输入2位」→ (6, 2)。只认恰好两个整数。
pub fn parse_passcode_field(value: &str) -> Option<(u32, u32)> {
    let nums: Vec<u32> = value
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect();
    match nums[..] {
        [total, entered] => Some((total, entered)),
        _ => None,
    }
}

/// 元素一行描述，格式与 toolkit 一致：`Button "秒表" @(261,880) 106x54`
pub fn describe(e: &Element) -> String {
    let text = [&e.label, &e.name, &e.value].into_iter().find(|s| !s.is_empty()).cloned().unwrap_or_default();
    format!("{} \"{}\" @({},{}) {}x{}", e.kind, text, e.center.0, e.center.1, e.size.0, e.size.1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passcode_field() {
        assert_eq!(parse_passcode_field("共6位数，已输入0位"), Some((6, 0)));
        assert_eq!(parse_passcode_field("共6位数，已输入5位"), Some((6, 5)));
        assert_eq!(parse_passcode_field("密码栏"), None);
    }
}
