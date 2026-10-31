//! MCP stdio 服务。工具名、参数、返回文本与 iphone-control-toolkit 一致（docs/decisions.md D10），
//! 切换时 agent 侧不用改。本 crate 不直接碰设备，全部经守护进程控制接口。
//!
//! 工具：iphone_status、iphone_screenshot（可选 x/y/width/height 区域放大）、iphone_list_elements、
//! iphone_tap、iphone_tap_element、iphone_long_press、iphone_swipe、iphone_type_text、
//! iphone_press_button、iphone_list_apps、iphone_launch_app、iphone_terminate_app、iphone_open_url。
//!
//! 基于 rmcp 3.8（`server` + `transport-io`），图片用 `ContentBlock::image(base64, "image/jpeg")` 返回。

use std::path::PathBuf;

/// 服务说明（发给客户端的 instructions），与 toolkit 一致
pub const INSTRUCTIONS: &str = "控制一台通过 USB 连接的真实 iPhone。坐标单位是点(pt)，与 iphone_screenshot 图像像素一致。\
操作前先用 iphone_screenshot 或 iphone_list_elements 看清屏幕；能按名字点就优先用 iphone_tap_element；\
手机平时会自动锁屏，各操作工具会先自动解锁。每次操作后再截图确认结果。\
涉及付款、删除、发送消息、修改账号或隐私设置的操作，先向用户确认。";

/// 区域放大：长边不足这个就放大到它
pub const ZOOM_MIN_LONG_SIDE: u32 = 600;
/// 区域放大：长边超过这个就缩到它
pub const ZOOM_MAX_LONG_SIDE: u32 = 1200;

pub struct McpConfig {
    pub socket: PathBuf,
}

/// 在 stdin/stdout 上运行 MCP 服务直到客户端断开
pub async fn serve_stdio(config: McpConfig) -> anyhow::Result<()> {
    let _ = config;
    todo!("rmcp::ServiceExt::serve(rmcp::transport::stdio())")
}

/// 计算区域放大后的尺寸和倍数；返回 (宽, 高, 倍数)
pub fn zoom_target(region_w_px: u32, region_h_px: u32, px_per_pt: f64) -> (u32, u32, f64) {
    let long = region_w_px.max(region_h_px) as f64;
    let k = if long < ZOOM_MIN_LONG_SIDE as f64 {
        ZOOM_MIN_LONG_SIDE as f64 / long
    } else {
        (ZOOM_MAX_LONG_SIDE as f64 / long).min(1.0)
    };
    let w = (region_w_px as f64 * k).round() as u32;
    let h = (region_h_px as f64 * k).round() as u32;
    (w, h, w as f64 / (region_w_px as f64 / px_per_pt))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoom_small_region_upscales() {
        // 200x60 点、3x 像素 → 600x180 像素 → 已经是 600，不再放大，倍数 3
        let (w, h, z) = zoom_target(600, 180, 3.0);
        assert_eq!((w, h), (600, 180));
        assert!((z - 3.0).abs() < 1e-9);
    }

    #[test]
    fn zoom_wide_region_downscales() {
        // 430x40 点、3x → 1290x120 → 缩到 1200 宽，倍数约 2.79
        let (w, _, z) = zoom_target(1290, 120, 3.0);
        assert_eq!(w, 1200);
        assert_eq!(format!("{z:.3}"), "2.791");
    }
}
