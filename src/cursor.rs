// 自定义光标：擦除工具下用「半透明橡皮擦」。
//
// Windows 的 32 位光标走 CreateIconIndirect：颜色位图里的 alpha 通道会被
// 系统拿去做混合，所以这里不用 GDI（GDI 写不进 alpha），直接逐像素做
// 4×4 超采样算覆盖率，边缘是平滑的半透明过渡，不会出现锯齿硬边。

#![allow(dead_code)]

use std::ptr;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::shape::Tool;

static mut ERASER_CUR: HCURSOR = HCURSOR(ptr::null_mut());

/// 取擦除光标（只在第一次调用时创建，之后复用）
pub fn eraser() -> HCURSOR {
    unsafe {
        if ERASER_CUR.is_invalid() {
            ERASER_CUR = build_eraser();
        }
        ERASER_CUR
    }
}

/// 按当前工具设置光标。切换工具后立刻调用，免得要等鼠标移动才变。
pub fn apply(tool: Tool) {
    unsafe {
        if tool == Tool::Eraser {
            SetCursor(eraser());
        } else if let Ok(a) = LoadCursorW(None, IDC_ARROW) {
            SetCursor(a);
        }
    }
}

/// 橡皮擦的几何：局部坐标 (u 沿笔身, v 横跨笔身)
const CX: f32 = 16.0;
const CY: f32 = 15.0;
/// 45° 笔身方向（指向右上），擦头在左下端
const DX: f32 = std::f32::consts::FRAC_1_SQRT_2;
const DY: f32 = -std::f32::consts::FRAC_1_SQRT_2;
const HALF_L: f32 = 11.0;
const HALF_W: f32 = 6.0;
const CORNER: f32 = 2.5;

/// 采样一个点（单位空间 32×32），返回 (r, g, b, a)
fn sample(x: f32, y: f32) -> Option<(f32, f32, f32, f32)> {
    let px = x - CX;
    let py = y - CY;
    // 转到笔身局部坐标：u 沿长度，v 横跨
    let u = px * DX + py * DY;
    let v = -px * DY + py * DX;

    // 圆角矩形的有符号距离
    let qx = u.abs() - (HALF_L - CORNER);
    let qy = v.abs() - (HALF_W - CORNER);
    let d = if qx > 0.0 && qy > 0.0 {
        (qx * qx + qy * qy).sqrt() - CORNER
    } else {
        qx.max(qy) - CORNER
    };
    if d > 0.0 {
        return None;
    }
    // 描边：最外侧 1.3 个单位画深色，轮廓才清楚
    if d > -1.3 {
        return Some((52.0, 58.0, 68.0, 0.82));
    }
    // 中段一条彩色带（笔套），其余是接近白色的橡皮本体
    if u > 0.5 && u < 4.5 {
        return Some((255.0, 122.0, 132.0, 0.72));
    }
    Some((248.0, 249.0, 252.0, 0.60))
}

fn build_eraser() -> HCURSOR {
    unsafe {
        // 光标尺寸跟随系统（高 DPI 下 SM_CXCURSOR 会大于 32）
        let n = GetSystemMetrics(SM_CXCURSOR).clamp(16, 128);
        let k = n as f32 / 32.0;
        let mut rgba = vec![0u8; (n * n * 4) as usize];

        for py in 0..n {
            for px in 0..n {
                let (mut ar, mut ag, mut ab, mut aa, mut cov) = (0f32, 0f32, 0f32, 0f32, 0f32);
                // 4×4 超采样，边缘按覆盖率变成半透明
                for sy in 0..4 {
                    for sx in 0..4 {
                        let fx = (px as f32 + (sx as f32 + 0.5) / 4.0) / k;
                        let fy = (py as f32 + (sy as f32 + 0.5) / 4.0) / k;
                        if let Some((r, g, b, a)) = sample(fx, fy) {
                            ar += r * a;
                            ag += g * a;
                            ab += b * a;
                            aa += a;
                            cov += 1.0;
                        }
                    }
                }
                if aa <= 0.0 || cov <= 0.0 {
                    continue;
                }
                // 覆盖率折进 alpha，颜色取覆盖像素的加权平均
                let a = (aa / cov) * (cov / 16.0);
                let i = ((py * n + px) * 4) as usize;
                rgba[i] = (ab / aa).round() as u8;
                rgba[i + 1] = (ag / aa).round() as u8;
                rgba[i + 2] = (ar / aa).round() as u8;
                rgba[i + 3] = (a * 255.0).round().clamp(0.0, 255.0) as u8;
            }
        }

        // 32 位 top-down DIB 作为颜色位图
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: n,
                biHeight: -n,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            bmiColors: [RGBQUAD::default(); 1],
        };
        let mut bits: *mut core::ffi::c_void = ptr::null_mut();
        let hdc = GetDC(None);
        let color = match CreateDIBSection(hdc, &bi, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(h) => h,
            Err(_) => {
                ReleaseDC(None, hdc);
                return HCURSOR(ptr::null_mut());
            }
        };
        ReleaseDC(None, hdc);
        if bits.is_null() {
            let _ = DeleteObject(HGDIOBJ(color.0));
            return HCURSOR(ptr::null_mut());
        }
        ptr::copy_nonoverlapping(rgba.as_ptr(), bits as *mut u8, rgba.len());

        // 掩码位图给一张全 0 的 1bpp（32 位光标靠 alpha 混合，掩码只是兜底）
        let mask = CreateBitmap(n, n, 1, 1, None);

        // 热点落在擦头（左下端点）
        let hx = (CX - HALF_L * DX).round().max(0.0) as u32;
        let hy = (CY - HALF_L * DY).round().max(0.0) as u32;
        let ii = ICONINFO {
            fIcon: FALSE,
            xHotspot: ((hx as f32) * k) as u32,
            yHotspot: ((hy as f32) * k) as u32,
            hbmMask: mask,
            hbmColor: color,
        };
        // CreateIconIndirect 返回 HICON，类型不同但本质同一个句柄
        let cur = match CreateIconIndirect(&ii) {
            Ok(h) => HCURSOR(h.0),
            Err(_) => HCURSOR(ptr::null_mut()),
        };
        let _ = DeleteObject(HGDIOBJ(mask.0));
        let _ = DeleteObject(HGDIOBJ(color.0));
        cur
    }
}
