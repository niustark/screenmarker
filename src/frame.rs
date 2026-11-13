// 分层窗口帧缓冲：32 位 DIB + UpdateLayeredWindow（真正的逐像素透明）
//
// 为什么需要它：
//   GDI 绘制不写 alpha 通道。若直接用 SetLayeredWindowAttributes(LWA_ALPHA)，
//   只能整窗统一透明度，做不到「画的地方不透明、没画的地方全透明」。
//   正确做法是渲染到 32 位 DIB（自带 alpha），再用 UpdateLayeredWindow 提交。
//
// alpha 约定：
//   背景像素 alpha = 1 —— 肉眼不可见，但 alpha>0 让窗口仍能接收鼠标（否则点击会穿透）；
//   绘制出的像素 alpha = 255 —— 完全不透明；
//   半透明图形（荧光笔/水印/聚光灯）alpha 取 100~160，由 convert_magic 写入。

#![allow(dead_code)]

use std::ptr;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

/// 背景 RGB（R=3,G=2,B=1）。取一个用户几乎不可能选到的颜色，
/// 这样「像素 != 背景色」就能可靠地判定为「画上去的内容」。
pub const BG_RGB: u32 = 0x0003_0201;
/// 背景像素完整值（alpha=1）
pub const BG_PIXEL: u32 = 0x0100_0000 | BG_RGB;

/// 半透明图形的「占位色」(R,G,B)。
/// 先用它画一遍，再把占位色整块换成「真色 + alpha」。
pub const MAGIC: (u8, u8, u8) = (0x13, 0x11, 0x0D);

/// 把 (R,G,B) 转成 DIB 里实际的像素值。
///
/// 32 位 BI_RGB 的内存字节序是 B,G,R,A；在小端机上按 u32 读出来正好是
/// `0xAARRGGBB` —— 也就是标准和 `0x00RRGGBB` 一样，R 在高位。
/// （这里曾经写成 `B<<16|G<<8|R`，导致占位色永远匹配不上、半透明全失效。）
#[inline]
pub fn px_of(rgb: (u8, u8, u8)) -> u32 {
    ((rgb.0 as u32) << 16) | ((rgb.1 as u32) << 8) | (rgb.2 as u32)
}

pub struct Frame {
    pub w: i32,
    pub h: i32,
    hdc_screen: HDC,
    mem_dc: HDC,
    dib: HBITMAP,
    old_bmp: HGDIOBJ,
    bits: *mut u32,
}

impl Frame {
    pub fn new(w: i32, h: i32) -> Option<Self> {
        unsafe {
            let hdc_screen = GetDC(None);
            if hdc_screen.is_invalid() {
                return None;
            }
            let bi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h, // 负值 = 自上而下，行序与屏幕一致
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                bmiColors: [RGBQUAD::default(); 1],
            };
            let mut bits: *mut core::ffi::c_void = ptr::null_mut();
            // 早退路径也必须把已经拿到的 HDC / HBITMAP 还回去，
            // 否则「创建帧缓冲失败」会一次漏掉两个 GDI 句柄。
            let dib = match CreateDIBSection(hdc_screen, &bi, DIB_RGB_COLORS, &mut bits, None, 0) {
                Ok(d) => d,
                Err(_) => {
                    ReleaseDC(None, hdc_screen);
                    return None;
                }
            };
            let mem_dc = CreateCompatibleDC(hdc_screen);
            if mem_dc.is_invalid() {
                DeleteObject(HGDIOBJ(dib.0));
                ReleaseDC(None, hdc_screen);
                return None;
            }
            let old_bmp = SelectObject(mem_dc, HGDIOBJ(dib.0));
            if old_bmp.is_invalid() {
                DeleteDC(mem_dc);
                DeleteObject(HGDIOBJ(dib.0));
                ReleaseDC(None, hdc_screen);
                return None;
            }
            Some(Frame {
                w,
                h,
                hdc_screen,
                mem_dc,
                dib,
                old_bmp,
                bits: bits as *mut u32,
            })
        }
    }

    /// 供 GDI 绘制用的内存 DC
    pub fn dc(&self) -> HDC {
        self.mem_dc
    }

    pub fn pixels(&self) -> &mut [u32] {
        unsafe { std::slice::from_raw_parts_mut(self.bits, (self.w * self.h) as usize) }
    }

    /// 读某个像素的 alpha（0~255）。越界返回 0。
    /// 用于 WM_NCHITTEST 判断「命中位置是不是透明空白」——alpha=0 就穿透。
    pub fn alpha_at(&self, x: i32, y: i32) -> u8 {
        if x < 0 || y < 0 || x >= self.w || y >= self.h {
            return 0;
        }
        unsafe {
            let px = *self.bits.add((y as usize) * (self.w as usize) + (x as usize));
            (px >> 24) as u8
        }
    }

    /// 清空为「透明但可点击」的背景。
    /// alpha=1：肉眼不可见，但 alpha>0 让窗口仍能接收鼠标（画图时要接住所有点击）。
    pub fn clear(&self) {
        self.pixels().fill(BG_PIXEL);
    }

    /// 清空为「真透明、鼠标可穿透」的背景。
    /// alpha=0：配合窗口的 WS_EX_TRANSPARENT，这些像素区域不再接收鼠标，
    /// 点击直接落到画布下面的程序（浏览器/桌面等）；已画内容的像素仍是
    /// alpha=255，会继续拦截鼠标，所以还能点选/拖动图元。
    pub fn clear_transparent(&self) {
        // alpha=0 + 保留背景 RGB，让 fix_alpha() 仍能靠 RGB==BG_RGB 识别「这是背景」
        self.pixels().fill(BG_RGB);
    }

    /// GDI 的绘制是异步批处理的：直接读 DIB 内存前必须 GdiFlush，
    /// 否则读到的是还没落盘的旧像素（这正是「画了却看不见」的元凶）。
    pub fn flush(&self) {
        unsafe {
            GdiFlush();
        }
    }

    /// 把「画过的像素」（RGB 不是背景色、且 alpha 还停留在 0/1）提升为完全不透明。
    /// 已经带 alpha 的半透明像素（>1）保持不变。
    pub fn fix_alpha(&self) {
        self.flush();
        for px in self.pixels().iter_mut() {
            if (*px >> 24) <= 1 && (*px & 0x00FF_FFFF) != BG_RGB {
                *px |= 0xFF00_0000;
            }
        }
    }

    /// 把区域内「占位色」像素还原成：真色 + 指定 alpha。
    ///
    /// 不能只做「等于占位色」的精确相等判断 —— 文字的抗锯齿边缘是占位色与背景的
    /// 混合色，精确比较会漏掉这些像素，留下硬邦邦的锯齿边。
    ///
    /// 这里的做法：把像素看作 `t*占位色 + (1-t)*背景色`，逐通道解出 t，
    /// 只有当三个通道解出的 t 互相一致（说明这个像素确实落在「背景→占位色」这条
    /// 混合线上）时才采信，最后用 `alpha * t` 作为该像素的真实透明度。
    /// 这样抗锯齿边缘就自动拿到了中间 alpha，一次成型。
    pub fn convert_magic(&self, area: (f32, f32, f32, f32), rgb: (u8, u8, u8), alpha: u8) {
        self.resolve(area, MAGIC, rgb, alpha);
    }

    /// 抗锯齿图形专用：GDI+ 画出来的边缘像素是「真色与透明背景的混合」，
    /// 这里同理解出覆盖率 t，把它还原成「真色 + t*255 的 alpha」。
    ///
    /// 不做这一步的话，AA 边缘会被 fix_alpha 一律提成不透明，
    /// 而它的 RGB 已经和近乎全黑的背景色混过 —— 看着就是一圈发黑的毛边。
    pub fn unmix(&self, area: (f32, f32, f32, f32), rgb: (u8, u8, u8)) {
        self.resolve(area, rgb, rgb, 255);
    }

    /// 公共实现：把 area 内落在「背景色 → from 色」混合线上的像素，
    /// 换算成「rgb + alpha*t」。
    fn resolve(
        &self,
        area: (f32, f32, f32, f32),
        from: (u8, u8, u8),
        rgb: (u8, u8, u8),
        alpha: u8,
    ) {
        self.flush();
        let x0 = area.0.floor().max(0.0) as i32;
        let y0 = area.1.floor().max(0.0) as i32;
        let x1 = (area.0 + area.2).ceil().min(self.w as f32) as i32;
        let y1 = (area.1 + area.3).ceil().min(self.h as f32) as i32;
        if x1 <= x0 || y1 <= y0 {
            return;
        }

        // 背景色与「绘制时用的颜色」，逐通道比较
        let bg = [
            ((BG_RGB >> 16) & 0xFF) as f32,
            ((BG_RGB >> 8) & 0xFF) as f32,
            (BG_RGB & 0xFF) as f32,
        ];
        let mg = [from.0 as f32, from.1 as f32, from.2 as f32];
        let new_rgb = px_of(rgb);
        let from_px = px_of(from);
        // 同上：预乘 alpha。alpha=255 时等价于原来的写法。
        let pm = |c: u32| ((c * alpha as u32 + 127) / 255) & 0xFF;
        let full = ((alpha as u32) << 24)
            | (pm((new_rgb >> 16) & 0xFF) << 16)
            | (pm((new_rgb >> 8) & 0xFF) << 8)
            | pm(new_rgb & 0xFF);

        let px = self.pixels();
        for y in y0..y1 {
            let base = (y as usize) * (self.w as usize);
            for x in x0..x1 {
                let p = &mut px[base + x as usize];
                // 快路径 1：正好是实心像素（矩形填充、粗线内部，
                // 占绝大多数）。聚光灯铺满全屏时全靠这条，省掉浮点运算。
                if (*p & 0x00FF_FFFF) == from_px {
                    *p = full;
                    continue;
                }
                // 快路径 2：本来就是透明背景，没什么可做的。
                // BG_PIXEL = alpha 1（绘图态的「可点击但不可见」背景）；
                // BG_RGB   = alpha 0（穿透态的真透明背景）。
                // 后者若不跳过，下面会算出 t≈0 并把它写成 BG_PIXEL(alpha 1)，
                // 于是整片空白变成「可点击」，点击穿透就失效了。
                if *p == BG_PIXEL || *p == BG_RGB {
                    continue;
                }
                let ch = [
                    ((*p >> 16) & 0xFF) as f32,
                    ((*p >> 8) & 0xFF) as f32,
                    (*p & 0xFF) as f32,
                ];
                let mut tmin = f32::INFINITY;
                let mut tmax = f32::NEG_INFINITY;
                let mut sum = 0.0f32;
                let mut n = 0u32;
                for i in 0..3 {
                    let d = mg[i] - bg[i];
                    // 差值太小的通道没有分辨力，跳过（否则除零或放大噪声）
                    if d.abs() < 4.0 {
                        continue;
                    }
                    let t = (ch[i] - bg[i]) / d;
                    tmin = tmin.min(t);
                    tmax = tmax.max(t);
                    sum += t;
                    n += 1;
                }
                if n == 0 {
                    // 绘制色与背景色太接近（例如近黑色），三个通道都没有分辨力，
                    // 解不出覆盖率。留着不动会被 fix_alpha 提成不透明、留下一圈
                    // 发黑的毛边，所以按「覆盖度为零」还原成透明背景。
                    *p = if (*p >> 24) == 0 { BG_RGB } else { BG_PIXEL };
                    continue;
                }
                // 三通道解出的 t 不一致 → 这个像素不是我们画的（比如底下原有的图形）
                if tmax - tmin > 0.35 {
                    continue;
                }
                // 比绘制色还亮 → 也不是我们的像素，别去动它
                if tmax > 1.4 {
                    continue;
                }
                let t = (sum / n as f32).clamp(0.0, 1.0);
                if t < 0.04 {
                    // 覆盖度几乎为零：还原成透明背景，免得被 fix_alpha 提成不透明。
                    // 保留原像素的 alpha（0=穿透态 / 1=绘图态），别一律写成 1，
                    // 否则穿透态下这些空白又会重新拦住鼠标。
                    // 只保留「原本就是真透明」的 alpha=0，其余一律恢复成绘图态的
                    // alpha=1（和旧行为一致，避免影响 GDI+ 抗锯齿像素）。
                    *p = if (*p >> 24) == 0 { BG_RGB } else { BG_PIXEL };
                } else {
                    let a = (alpha as f32 * t).round().clamp(1.0, 255.0) as u32;
                    // UpdateLayeredWindow 用的是 AC_SRC_ALPHA（预乘 alpha）：
                    // 半透明像素必须把 RGB 也乘上 alpha，否则合成时会当成
                    // 「亮度不衰减的颜色」叠上去，抗锯齿边缘就会发亮（白边）。
                    let pr = |c: u32| ((c * a + 127) / 255) & 0xFF;
                    let rgb_pm = (pr((new_rgb >> 16) & 0xFF) << 16)
                        | (pr((new_rgb >> 8) & 0xFF) << 8)
                        | pr(new_rgb & 0xFF);
                    *p = (a << 24) | rgb_pm;
                }
            }
        }
    }

    /// 提交到分层窗口
    pub fn present(&self, hwnd: HWND) {
        unsafe {
            // 提交前把 GDI 的绘制全部落盘到 DIB 内存
            GdiFlush();
            let dst = POINT { x: 0, y: 0 };
            let size = SIZE {
                cx: self.w,
                cy: self.h,
            };
            let src = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let _ = UpdateLayeredWindow(
                hwnd,
                self.hdc_screen,
                Some(&dst),
                Some(&size),
                self.mem_dc,
                Some(&src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            )
            .map_err(|e| {
                crate::dbg::logf!("UpdateLayeredWindow 失败: {e:?}");
            });
        }
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.mem_dc, self.old_bmp);
            DeleteObject(HGDIOBJ(self.dib.0));
            DeleteDC(self.mem_dc);
            ReleaseDC(None, self.hdc_screen);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回归：穿透态背景是 alpha=0 的 BG_RGB。resolve() 早期曾把这类像素
    /// 当成「t≈0 的混合像素」写成 BG_PIXEL(alpha=1)，导致整片空白重新
    /// 拦住鼠标、点击穿透失效。有荧光笔/水印/AA 图形时必现。
    #[test]
    fn transparent_bg_survives_magic_resolve() {
        let f = Frame::new(64, 64).expect("Frame::new 失败");
        f.clear_transparent();
        // 模拟 GDI 画上去的占位色像素（GDI 不写 alpha，所以仍是 0）
        f.pixels()[32 * 64 + 32] = px_of(MAGIC);
        f.convert_magic((0.0, 0.0, 64.0, 64.0), (255, 0, 0), 128);
        assert_eq!(f.alpha_at(10, 10), 0, "穿透态空白必须保持 alpha=0");
        assert!(f.alpha_at(32, 32) >= 100, "占位色应转成带色半透明像素");
    }

    /// 绘图态背景是 alpha=1 的 BG_PIXEL，要继续保持可点击。
    #[test]
    fn drawing_bg_stays_clickable() {
        let f = Frame::new(64, 64).expect("Frame::new 失败");
        f.clear();
        f.convert_magic((0.0, 0.0, 64.0, 64.0), (255, 0, 0), 128);
        assert_eq!(f.alpha_at(10, 10), 1, "绘图态空白保持 alpha=1");
    }

    /// 回归：绘制色与背景色几乎一样时（各通道差值 < 4），resolve() 解不出
    /// 覆盖率。旧代码直接 continue，留下一个「非背景 RGB、alpha=0」的像素，
    /// 随后 fix_alpha 会把它提成不透明，表现就是一圈发黑的毛边。
    #[test]
    fn indistinguishable_color_has_no_dark_fringe() {
        let f = Frame::new(64, 64).expect("Frame::new 失败");
        f.clear_transparent();
        // 介于 BG_RGB(3,2,1) 与绘制色(5,4,3) 之间的 AA 边缘像素
        f.pixels()[20 * 64 + 20] = px_of((4, 3, 2));
        f.unmix((0.0, 0.0, 64.0, 64.0), (5, 4, 3));
        assert_eq!(f.alpha_at(30, 30), 0, "穿透态空白保持 alpha=0");
        f.fix_alpha();
        assert_eq!(f.alpha_at(20, 20), 0, "解不出覆盖率的像素不应被提成不透明");
    }
}
