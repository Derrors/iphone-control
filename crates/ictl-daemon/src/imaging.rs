//! 截图缩放到点坐标、区域放大。与 toolkit 的 `imaging.py` 行为和说明文字一致。

use std::io::Cursor;

use ictl_device::ScreenSize;
use image::{DynamicImage, ImageFormat, codecs::jpeg::JpegEncoder, imageops::FilterType};

pub const ZOOM_MIN_LONG_SIDE: f64 = 600.0;
pub const ZOOM_MAX_LONG_SIDE: f64 = 1200.0;

/// Python 的 `{:g}`：整数不带小数点，其余用最短表示
pub fn fmt_g(v: f64) -> String {
    format!("{v}")
}

/// Python 的 `{:.3g}`：3 位有效数字、去掉末尾的 0
pub fn fmt_3g(v: f64) -> String {
    if v == 0.0 {
        return "0".into();
    }
    let decimals = (2 - v.abs().log10().floor() as i32).max(0) as usize;
    let s = format!("{v:.decimals$}");
    if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_owned() } else { s }
}

/// 截图是横的而屏幕尺寸是竖的（横屏 App），就把宽高互换
pub fn oriented(img_w: u32, img_h: u32, screen: ScreenSize) -> (u32, u32) {
    let (w, h) = (screen.width.round() as u32, screen.height.round() as u32);
    if img_w > img_h && w < h { (h, w) } else { (w, h) }
}

fn decode(png: &[u8]) -> Result<DynamicImage, String> {
    image::load_from_memory(png).map_err(|e| format!("截图解码失败：{e}"))
}

fn jpeg(img: &DynamicImage, quality: u8) -> Result<Vec<u8>, String> {
    let mut out = Cursor::new(Vec::new());
    img.to_rgb8()
        .write_with_encoder(JpegEncoder::new_with_quality(&mut out, quality))
        .map_err(|e| format!("JPEG 编码失败：{e}"))?;
    Ok(out.into_inner())
}

/// 整屏缩放到点尺寸：图像像素即点坐标。返回 (jpeg, 说明文字)
pub fn full_screen(png: &[u8], screen: ScreenSize, via: &str) -> Result<(Vec<u8>, String), String> {
    let img = decode(png)?;
    let (w, h) = oriented(img.width(), img.height(), screen);
    let out = jpeg(&img.resize_exact(w, h, FilterType::Lanczos3), 80)?;
    Ok((out, format!("{w}x{h} 点,图像像素即点坐标(经 {via})")))
}

/// 区域放大的几何计算（纯函数）：裁剪框（原图像素）、输出尺寸、放大倍数、裁剪后的点区域
#[derive(Debug, Clone, PartialEq)]
pub struct ZoomPlan {
    pub crop: (u32, u32, u32, u32),
    pub out: (u32, u32),
    pub zoom: f64,
    pub region: (f64, f64, f64, f64),
}

pub fn plan_zoom(
    img_w: u32,
    img_h: u32,
    screen: ScreenSize,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
) -> Result<ZoomPlan, String> {
    let (w, h) = oriented(img_w, img_h, screen);
    let (w, h) = (w as f64, h as f64);
    let (x1, y1) = (x.max(0.0), y.max(0.0));
    let (x2, y2) = ((x + width).min(w), (y + height).min(h));
    if x2 - x1 < 1.0 || y2 - y1 < 1.0 {
        return Err(format!("放大区域不在屏幕内(屏幕 {}x{} 点)", w, h));
    }
    let px = img_w as f64 / w; // 每点对应的原图像素，一般是 3
    let (cx1, cy1) = ((x1 * px).round() as u32, (y1 * px).round() as u32);
    let (cx2, cy2) = ((x2 * px).round() as u32, (y2 * px).round() as u32);
    let (cw, ch) = (cx2 - cx1, cy2 - cy1);
    let long = cw.max(ch) as f64;
    let k = if long < ZOOM_MIN_LONG_SIDE { ZOOM_MIN_LONG_SIDE / long } else { (ZOOM_MAX_LONG_SIDE / long).min(1.0) };
    let out = ((cw as f64 * k).round() as u32, (ch as f64 * k).round() as u32);
    Ok(ZoomPlan { crop: (cx1, cy1, cw, ch), out, zoom: out.0 as f64 / (x2 - x1), region: (x1, y1, x2 - x1, y2 - y1) })
}

/// 从原始高清截图裁出一块并放大。返回 (jpeg, 说明文字，含坐标换算式)
pub fn region(
    png: &[u8],
    screen: ScreenSize,
    rect: (f64, f64, f64, f64),
    via: &str,
) -> Result<(Vec<u8>, String), String> {
    let img = decode(png)?;
    let p = plan_zoom(img.width(), img.height(), screen, rect.0, rect.1, rect.2, rect.3)?;
    let (cx, cy, cw, ch) = p.crop;
    let mut crop = img.crop_imm(cx, cy, cw, ch);
    if p.out != (cw, ch) {
        crop = crop.resize_exact(p.out.0, p.out.1, FilterType::Lanczos3);
    }
    let (x1, y1, rw, rh) = p.region;
    let (x, y, z) = (fmt_g(x1), fmt_g(y1), fmt_3g(p.zoom));
    let caption = format!(
        "区域 ({x},{y}) 起 {}x{} 点,放大 {z} 倍;图中像素 (u,v) 对应屏幕点 ({x}+u/{z}, {y}+v/{z})(经 {via})",
        fmt_g(rw),
        fmt_g(rh)
    );
    Ok((jpeg(&crop, 90)?, caption))
}

/// 读取 PNG 尺寸（不解码像素）
pub fn png_size(png: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::with_format(Cursor::new(png), ImageFormat::Png).into_dimensions().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: ScreenSize = ScreenSize { width: 430.0, height: 932.0 };

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        image::RgbImage::from_pixel(w, h, image::Rgb([30, 60, 90])).write_to(&mut out, ImageFormat::Png).unwrap();
        out.into_inner()
    }

    fn jpeg_size(j: &[u8]) -> (u32, u32) {
        image::load_from_memory(j).unwrap().to_rgb8().dimensions()
    }

    #[test]
    fn full_screen_is_scaled_to_points() {
        let (j, cap) = full_screen(&png(1290, 2796), SCREEN, "WDA").unwrap();
        assert_eq!(jpeg_size(&j), (430, 932));
        assert_eq!(cap, "430x932 点,图像像素即点坐标(经 WDA)");
    }

    #[test]
    fn landscape_screenshot_swaps_dimensions() {
        let (j, _) = full_screen(&png(2796, 1290), SCREEN, "HID").unwrap();
        assert_eq!(jpeg_size(&j), (932, 430));
    }

    #[test]
    fn wide_region_is_capped_at_1200() {
        let p = plan_zoom(1290, 2796, SCREEN, 0.0, 55.0, 430.0, 40.0).unwrap();
        assert_eq!(p.region, (0.0, 55.0, 430.0, 40.0));
        assert_eq!(p.out.0, 1200);
        assert!((p.zoom - 1200.0 / 430.0).abs() < 1e-9);
    }

    #[test]
    fn small_region_is_upscaled_to_600() {
        let p = plan_zoom(1290, 2796, SCREEN, 115.0, 108.0, 50.0, 20.0).unwrap();
        assert_eq!(p.out.0, 600);
        assert!((p.zoom - 12.0).abs() < 1e-9);
    }

    #[test]
    fn region_is_clamped_to_screen() {
        assert_eq!(plan_zoom(1290, 2796, SCREEN, -10.0, 900.0, 100.0, 100.0).unwrap().region, (0.0, 900.0, 90.0, 32.0));
    }

    #[test]
    fn region_outside_screen_errors() {
        assert_eq!(
            plan_zoom(1290, 2796, SCREEN, 500.0, 10.0, 50.0, 50.0).unwrap_err(),
            "放大区域不在屏幕内(屏幕 430x932 点)"
        );
    }

    /// 回归用例 #16：倍数保留 3 位有效数字（2 位会让右边缘差约 1.4 点）
    #[test]
    fn zoom_caption_uses_three_significant_digits() {
        let (j, cap) = region(&png(1290, 2796), SCREEN, (0.0, 55.0, 430.0, 40.0), "WDA").unwrap();
        assert_eq!(jpeg_size(&j).0, 1200);
        assert_eq!(
            cap,
            "区域 (0,55) 起 430x40 点,放大 2.79 倍;图中像素 (u,v) 对应屏幕点 (0+u/2.79, 55+v/2.79)(经 WDA)"
        );
    }

    #[test]
    fn python_like_number_formatting() {
        assert_eq!((fmt_g(215.0), fmt_g(0.3), fmt_g(10.5)), ("215".into(), "0.3".into(), "10.5".into()));
        assert_eq!(
            (fmt_3g(2.790697), fmt_3g(12.0), fmt_3g(3.0), fmt_3g(0.5)),
            ("2.79".into(), "12".into(), "3".into(), "0.5".into())
        );
    }

    #[test]
    fn png_size_reads_header() {
        assert_eq!(png_size(&png(12, 34)), Some((12, 34)));
    }
}
