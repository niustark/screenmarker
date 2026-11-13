// 纯 GDI 绘制引擎：把图形画到 HDC 上。
//
// 半透明图形的处理方式：
//   GDI 无法直接输出 alpha，所以「半透明图形」先用 MAGIC_RGB 占位色画一遍，
//   再由 Frame::convert_magic 把占位色整块替换成「真色 + 指定 alpha」。
//   Renderer::with_override 就是用来切换成占位色绘制的。

#![allow(dead_code)]

use crate::frame::MAGIC;
use crate::shape::*;
use std::cell::RefCell;
use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;

pub struct Renderer {
    hdc: HDC,
    /// Some(rgb) 时，所有画笔/画刷都用这个颜色（占位色绘制）
    over: Option<(u8, u8, u8)>,
}

impl Renderer {
    pub fn new(hdc: HDC) -> Self {
        Renderer { hdc, over: None }
    }

    pub fn with_override(hdc: HDC, rgb: (u8, u8, u8)) -> Self {
        Renderer {
            hdc,
            over: Some(rgb),
        }
    }

    /// 用当前工具条统一的占位色构造
    pub fn magic(hdc: HDC) -> Self {
        Renderer {
            hdc,
            over: Some(MAGIC),
        }
    }

    fn col(&self, c: Color) -> Color {
        match self.over {
            Some((r, g, b)) => Color::rgb(r, g, b),
            None => c,
        }
    }

    fn gdi_color(c: Color) -> COLORREF {
        // COLORREF 的字节序是 0x00BBGGRR：低字节是红、高字节是蓝。
        // 之前写成 R 在高位，红蓝通道正好对调（红色画出来是蓝的）。
        COLORREF(((c.blue() as u32) << 16) | ((c.green() as u32) << 8) | (c.red() as u32))
    }

    fn pen(&self, c: Color, width: i32) -> HPEN {
        unsafe { CreatePen(PS_SOLID, width.max(1), Self::gdi_color(self.col(c))) }
    }

    /// 不受 override 影响的画笔（放大镜边框、序号数字等必须保持可见）
    fn pen_raw(&self, c: Color, width: i32) -> HPEN {
        unsafe { CreatePen(PS_SOLID, width.max(1), Self::gdi_color(c)) }
    }

    fn brush(&self, c: Color) -> HBRUSH {
        unsafe { CreateSolidBrush(Self::gdi_color(self.col(c))) }
    }

    fn line(&self, pen: HPEN, p1: Point, p2: Point) {
        unsafe {
            let old = SelectObject(self.hdc, HGDIOBJ(pen.0));
            MoveToEx(self.hdc, p1.x as i32, p1.y as i32, None);
            LineTo(self.hdc, p2.x as i32, p2.y as i32);
            SelectObject(self.hdc, old);
        }
    }

    fn stroke_rect(&self, pen: HPEN, r: Rect) {
        unsafe {
            let old = SelectObject(self.hdc, HGDIOBJ(pen.0));
            let ob = SelectObject(self.hdc, GetStockObject(NULL_BRUSH));
            Rectangle(self.hdc, r.x as i32, r.y as i32, r.right() as i32, r.bottom() as i32);
            SelectObject(self.hdc, ob);
            SelectObject(self.hdc, old);
        }
    }

    fn stroke_ellipse(&self, pen: HPEN, r: Rect) {
        unsafe {
            let old = SelectObject(self.hdc, HGDIOBJ(pen.0));
            let ob = SelectObject(self.hdc, GetStockObject(NULL_BRUSH));
            Ellipse(self.hdc, r.x as i32, r.y as i32, r.right() as i32, r.bottom() as i32);
            SelectObject(self.hdc, ob);
            SelectObject(self.hdc, old);
        }
    }

    fn fill_ellipse(&self, b: HBRUSH, r: Rect) {
        unsafe {
            let old = SelectObject(self.hdc, HGDIOBJ(b.0));
            let op = SelectObject(self.hdc, GetStockObject(NULL_PEN));
            Ellipse(self.hdc, r.x as i32, r.y as i32, r.right() as i32, r.bottom() as i32);
            SelectObject(self.hdc, op);
            SelectObject(self.hdc, old);
        }
    }

    fn fill_rect(&self, b: HBRUSH, r: Rect) {
        unsafe {
            let old = SelectObject(self.hdc, HGDIOBJ(b.0));
            let op = SelectObject(self.hdc, GetStockObject(NULL_PEN));
            Rectangle(self.hdc, r.x as i32, r.y as i32, r.right() as i32, r.bottom() as i32);
            SelectObject(self.hdc, op);
            SelectObject(self.hdc, old);
        }
    }

    fn polyline(&self, pen: HPEN, points: &[Point]) {
        if points.len() < 2 {
            return;
        }
        unsafe {
            let old = SelectObject(self.hdc, HGDIOBJ(pen.0));
            let pts: Vec<POINT> = points
                .iter()
                .map(|p| POINT {
                    x: p.x as i32,
                    y: p.y as i32,
                })
                .collect();
            Polyline(self.hdc, &pts);
            SelectObject(self.hdc, old);
        }
    }

    fn polygon(&self, b: HBRUSH, points: &[Point]) {
        if points.len() < 3 {
            return;
        }
        unsafe {
            let old = SelectObject(self.hdc, HGDIOBJ(b.0));
            let op = SelectObject(self.hdc, GetStockObject(NULL_PEN));
            let pts: Vec<POINT> = points
                .iter()
                .map(|p| POINT {
                    x: p.x as i32,
                    y: p.y as i32,
                })
                .collect();
            Polygon(self.hdc, &pts);
            SelectObject(self.hdc, op);
            SelectObject(self.hdc, old);
        }
    }

    fn dashed_rect(&self, r: Rect, c: Color) {
        unsafe {
            let pen = CreatePen(PS_DOT, 1, Self::gdi_color(self.col(c)));
            let old = SelectObject(self.hdc, HGDIOBJ(pen.0));
            let ob = SelectObject(self.hdc, GetStockObject(NULL_BRUSH));
            Rectangle(self.hdc, r.x as i32, r.y as i32, r.right() as i32, r.bottom() as i32);
            SelectObject(self.hdc, ob);
            SelectObject(self.hdc, old);
            DeleteObject(HGDIOBJ(pen.0));
        }
    }

    fn text(&self, s: &str, color: Color, font_size: i32, r: Rect, center: bool) {
        unsafe {
            let font = CreateFontW(
                -font_size.max(6),
                0,
                0,
                0,
                400,
                0,
                0,
                0,
                DEFAULT_CHARSET.0 as u32,
                0,
                0,
                0,
                0,
                w!("Microsoft YaHei"),
            );
            let old_font = SelectObject(self.hdc, HGDIOBJ(font.0));
            SetBkMode(self.hdc, TRANSPARENT);
            // 文字颜色不受 override 影响，否则半透明图形里的文字也会变成占位色
            SetTextColor(self.hdc, Self::gdi_color(color));
            let mut wide: Vec<u16> = s.encode_utf16().collect();
            wide.push(0);
            let mut rr = RECT {
                left: r.x as i32,
                top: r.y as i32,
                right: r.right() as i32,
                bottom: r.bottom() as i32,
            };
            let flags = if center {
                DT_CENTER | DT_VCENTER | DT_SINGLELINE
            } else {
                DT_LEFT | DT_TOP | DT_WORDBREAK
            };
            DrawTextW(self.hdc, &mut wide, &mut rr, flags);
            SelectObject(self.hdc, old_font);
            DeleteObject(HGDIOBJ(font.0));
        }
    }

    /// 倾斜文字（水印用）。
    ///
    /// 踩过的坑：DrawTextW **不认世界变换**（SetWorldTransform），把 DC 转了也还是横的。
    /// GDI 里真正能给文字上角度的办法是字体本身的 escapement/orientation
    /// （单位 1/10 度，逆时针为正），配合 ExtTextOutW 直接按点输出。
    /// 倾斜字体单独建一次即可（水印一帧要铺十几个锚点）。
    fn angled_font(font_size: i32, deg: f32) -> HFONT {
        unsafe {
            let esc = (deg * 10.0) as i32; // 450 = 逆时针 45°
            CreateFontW(
                -font_size.max(6),
                0,
                esc, // lfEscapement：整行走向
                esc, // lfOrientation：字形自身也转
                400,
                0,
                0,
                0,
                DEFAULT_CHARSET.0 as u32,
                0,
                0,
                0,
                0,
                w!("Microsoft YaHei"),
            )
        }
    }

    /// 只负责把已转好的宽字符串按点输出；字体/DC 状态由调用方设置一次。
    fn draw_angled_wide(&self, wide: &[u16], at: Point) {
        unsafe {
            if !wide.is_empty() {
                ExtTextOutW(
                    self.hdc,
                    at.x as i32,
                    at.y as i32,
                    ETO_OPTIONS(0),
                    None,
                    PCWSTR(wide.as_ptr()),
                    wide.len() as u32,
                    None,
                );
            }
        }
    }

    /// 按「墨迹中心」画一段文字（序号数字用）。
    ///
    /// 为什么不能直接用 DT_CENTER：DrawText 居中的是**字宽前进量盒**（advance），
    /// 而数字 `1` 这类字形在自己盒子里天然偏左，墨迹中心与盒子中心差几个像素，
    /// 放进小圆圈里就成了肉眼可见的「偏左」。这里先量出真实墨迹框，再反推笔位，
    /// 做到像素级的视觉居中。
    fn text_ink_centered(&self, s: &str, color: Color, font_size: i32, center: Point) {
        let fs = font_size.max(6);
        let Some((l, t, w, h)) = ink_box(s, fs) else {
            // 量不出来就退回原来的居中方式，不至于不画
            self.text(
                s,
                color,
                fs,
                Rect {
                    x: center.x - fs as f32,
                    y: center.y - fs as f32,
                    w: fs as f32 * 2.0,
                    h: fs as f32 * 2.0,
                },
                true,
            );
            return;
        };
        unsafe {
            let font = CreateFontW(
                -fs,
                0,
                0,
                0,
                400,
                0,
                0,
                0,
                DEFAULT_CHARSET.0 as u32,
                0,
                0,
                0,
                0,
                w!("Microsoft YaHei"),
            );
            let old_font = SelectObject(self.hdc, HGDIOBJ(font.0));
            SetBkMode(self.hdc, TRANSPARENT);
            SetTextColor(self.hdc, Self::gdi_color(color));
            let wide: Vec<u16> = s.encode_utf16().collect();
            if !wide.is_empty() {
                // 笔位往左上挪「墨迹左边界/上边界」，再挪半个墨迹尺寸，
                // 墨迹框中心就正好落在 center 上
                let x = center.x - (l as f32 + w as f32 / 2.0);
                let y = center.y - (t as f32 + h as f32 / 2.0);
                ExtTextOutW(
                    self.hdc,
                    x as i32,
                    y as i32,
                    ETO_OPTIONS(0),
                    None,
                    PCWSTR(wide.as_ptr()),
                    wide.len() as u32,
                    None,
                );
            }
            SelectObject(self.hdc, old_font);
            DeleteObject(HGDIOBJ(font.0));
        }
    }

    /// 画一个图形。
    ///
    /// 返回值：如果这个图形是用 GDI+ 抗锯齿画的，返回 (外接矩形, 主色)，
    /// 调用方必须紧接着用它做一次 `Frame::unmix` —— 把 AA 边缘的混色还原成
    /// 半透明像素，否则边缘会变成一圈发黑的硬边。
    pub fn draw_shape(
        &self,
        sh: &Shape,
        screen_w: i32,
        screen_h: i32,
    ) -> Option<(Rect, Color)> {
        // 占位色绘制模式（荧光笔/水印/聚光灯的半透明路径）不能用 GDI+：
        // 那条路要求用 MAGIC 单色画，再由 convert_magic 反解 alpha。
        let aa_ok = self.over.is_none();
        match sh {
            Shape::Freehand {
                points,
                color,
                width,
                alpha,
            } => {
                if points.len() < 2 {
                    return None;
                }
                if aa_ok && *alpha >= 255 {
                    if let Some(g) = aa::G::new(self.hdc) {
                        g.stroke(points, *color, *width);
                        return Some((sh.bbox(), *color));
                    }
                }
                let pen = self.pen(color.with_alpha(*alpha), *width as i32);
                self.polyline(pen, points);
                unsafe {
                    DeleteObject(HGDIOBJ(pen.0));
                }
            }
            Shape::Rect { rect, color, width } => {
                if aa_ok {
                    if let Some(g) = aa::G::new(self.hdc) {
                        g.rect(*rect, *color, *width);
                        return Some((sh.bbox(), *color));
                    }
                }
                let pen = self.pen(*color, *width as i32);
                self.stroke_rect(pen, *rect);
                unsafe {
                    DeleteObject(HGDIOBJ(pen.0));
                }
            }
            Shape::Ellipse { rect, color, width } => {
                if aa_ok {
                    if let Some(g) = aa::G::new(self.hdc) {
                        g.ellipse(*rect, *color, *width);
                        return Some((sh.bbox(), *color));
                    }
                }
                let pen = self.pen(*color, *width as i32);
                self.stroke_ellipse(pen, *rect);
                unsafe {
                    DeleteObject(HGDIOBJ(pen.0));
                }
            }
            Shape::Arrow {
                p1,
                p2,
                color,
                width,
            } => {
                if aa_ok {
                    if let Some(g) = aa::G::new(self.hdc) {
                        if self.arrow_aa(&g, *p1, *p2, *color, *width) {
                            return Some((sh.bbox(), *color));
                        }
                    }
                }
                self.arrow(*p1, *p2, *color, *width);
            }
            Shape::Polyline {
                points,
                color,
                width,
            } => {
                if aa_ok {
                    if let Some(g) = aa::G::new(self.hdc) {
                        g.stroke(points, *color, *width);
                        // 顶点小圆点，让折线拐点更清晰
                        let d = (*width * 1.2).max(4.0);
                        for pt in points {
                            g.fill_ellipse(
                                Rect {
                                    x: pt.x - d / 2.0,
                                    y: pt.y - d / 2.0,
                                    w: d,
                                    h: d,
                                },
                                *color,
                            );
                        }
                        return Some((sh.bbox(), *color));
                    }
                }
                let pen = self.pen(*color, *width as i32);
                self.polyline(pen, points);
                unsafe {
                    DeleteObject(HGDIOBJ(pen.0));
                }
                // 顶点小圆点，让折线拐点更清晰
                let b = self.brush(*color);
                let d = (*width * 1.2).max(4.0);
                for pt in points {
                    self.fill_ellipse(
                        b,
                        Rect {
                            x: pt.x - d / 2.0,
                            y: pt.y - d / 2.0,
                            w: d,
                            h: d,
                        },
                    );
                }
                unsafe {
                    DeleteObject(HGDIOBJ(b.0));
                }
            }
            Shape::Wavy {
                p1,
                p2,
                color,
                width,
            } => {
                if aa_ok {
                    if let Some(g) = aa::G::new(self.hdc) {
                        let pts = wavy_points(*p1, *p2, *width);
                        g.stroke(&pts, *color, *width);
                        return Some((sh.bbox(), *color));
                    }
                }
                self.wavy(*p1, *p2, *color, *width);
            }
            Shape::Spotlight { .. } => {
                // 聚光灯由画布分两步绘制（调暗区域 + 高亮边框），这里不做处理
            }
            Shape::Magnifier {
                center,
                radius,
                zoom,
            } => {
                let r = *radius;
                let dst = Rect {
                    x: center.x - r,
                    y: center.y - r,
                    w: r * 2.0,
                    h: r * 2.0,
                };
                // 取景范围 = 圆半径 / 倍率：倍率越大，取样范围越小（看着越"放得大"）
                if let Some(snap) = crate::canvas::snapshot() {
                    let (data, sw, sh) = (snap.0.as_slice(), snap.1, snap.2);
                    let z = (*zoom).max(1.0);
                    let half = r / z;
                    let src = Rect {
                        x: center.x - half,
                        y: center.y - half,
                        w: half * 2.0,
                        h: half * 2.0,
                    };
                    // 圆形裁剪：不裁的话就是方图，边角会露出桌面
                    unsafe {
                        let rgn = CreateEllipticRgn(
                            dst.x as i32,
                            dst.y as i32,
                            dst.right() as i32,
                            dst.bottom() as i32,
                        );
                        let _ = SelectClipRgn(self.hdc, rgn);
                        self.blit_sub(data, sw, sh, src, dst);
                        // 传 NULL 区域 = 取消裁剪，恢复本 DC 原来（无裁剪）的状态
                        let _ = SelectClipRgn(self.hdc, HRGN(std::ptr::null_mut()));
                        let _ = DeleteObject(HGDIOBJ(rgn.0));
                    }
                }
                // 镜圈：外深内白两圈。底色是不透明的截图，AA 边缘直接提成
                // 不透明即可，不需要 unmix，所以这里不返回 unmix 区域。
                if let Some(g) = aa::G::new(self.hdc) {
                    g.ellipse(dst, Color::rgb(0x33, 0x38, 0x40), 5.0);
                    g.ellipse(dst, COLOR_WHITE, 3.0);
                } else {
                    let ring = self.pen_raw(Color::rgb(0x33, 0x38, 0x40), 5);
                    self.stroke_ellipse(ring, dst);
                    unsafe {
                        DeleteObject(HGDIOBJ(ring.0));
                    }
                    let ring = self.pen_raw(COLOR_WHITE, 3);
                    self.stroke_ellipse(ring, dst);
                    unsafe {
                        DeleteObject(HGDIOBJ(ring.0));
                    }
                }
            }
            Shape::Text {
                rect,
                text,
                color,
                font_size,
            } => {
                // 文字框的虚线边和右下角小方块只在「被选中」时由 selection_box 画，
                // 平时写死的框会一直糊在画面上，看着像没画完。
                let inner = Rect {
                    x: rect.x + 6.0,
                    y: rect.y + 6.0,
                    w: rect.w - 12.0,
                    h: rect.h - 12.0,
                };
                self.text(text, *color, *font_size as i32, inner, false);
            }
            Shape::Watermark { text, color, size } => {
                self.watermark(text, *color, *size, screen_w, screen_h);
            }
            Shape::Number {
                pos,
                number,
                color,
                radius,
            } => {
                let disc = Rect {
                    x: pos.x - radius,
                    y: pos.y - radius,
                    w: *radius * 2.0,
                    h: *radius * 2.0,
                };
                if aa_ok {
                    if let Some(g) = aa::G::new(self.hdc) {
                        g.fill_ellipse(disc, *color);
                        // 数字按墨迹居中，不再用 DT_CENTER（那个居中的是字宽盒子，会偏左）
                        self.text_ink_centered(
                            &number.to_string(),
                            COLOR_WHITE,
                            *radius as i32,
                            *pos,
                        );
                        return Some((sh.bbox(), *color));
                    }
                }
                let b = self.brush(*color);
                self.fill_ellipse(b, disc);
                unsafe {
                    DeleteObject(HGDIOBJ(b.0));
                }
                // 数字按墨迹居中，不再用 DT_CENTER（那个居中的是字宽盒子，会偏左）
                self.text_ink_centered(&number.to_string(), COLOR_WHITE, *radius as i32, *pos);
            }
            Shape::Mosaic {
                rect,
                pixmap,
                pixmap_w,
                pixmap_h,
            } => {
                if let Some(px) = pixmap {
                    self.blit(px, *pixmap_w, *pixmap_h, *rect);
                }
            }
        }
        None
    }

    /// 选中提示框：虚线框 + 8 个缩放手柄（白底蓝边，四角 + 四边中点）
    /// 圆形图元的选中提示：只在圆周的上下左右画 4 个小圆点，
    /// 不画外接方框 —— 放大镜/聚光灯本来就是圆的，外面套个方框很别扭。
    pub fn selection_circle(&self, cx: f32, cy: f32, r: f32) {
        let c = Color::rgb(10, 132, 255);
        let d = HANDLE_SIDE * 1.2;
        let white = unsafe { CreateSolidBrush(Self::gdi_color(COLOR_WHITE)) };
        let pen = unsafe { CreatePen(PS_SOLID, 2, Self::gdi_color(c)) };
        for (p, _) in circle_handle_points(cx, cy, r) {
            unsafe {
                let ob = SelectObject(self.hdc, HGDIOBJ(white.0));
                let op = SelectObject(self.hdc, HGDIOBJ(pen.0));
                Ellipse(
                    self.hdc,
                    (p.x - d / 2.0) as i32,
                    (p.y - d / 2.0) as i32,
                    (p.x + d / 2.0) as i32,
                    (p.y + d / 2.0) as i32,
                );
                SelectObject(self.hdc, op);
                SelectObject(self.hdc, ob);
            }
        }
        unsafe {
            DeleteObject(HGDIOBJ(white.0));
            DeleteObject(HGDIOBJ(pen.0));
        }
    }

    pub fn selection_box(&self, r: Rect) {
        let c = Color::rgb(10, 132, 255);
        unsafe {
            let pen = CreatePen(PS_DOT, 2, Self::gdi_color(c));
            let old = SelectObject(self.hdc, HGDIOBJ(pen.0));
            let ob = SelectObject(self.hdc, GetStockObject(NULL_BRUSH));
            Rectangle(
                self.hdc,
                r.x as i32,
                r.y as i32,
                r.right() as i32,
                r.bottom() as i32,
            );
            SelectObject(self.hdc, ob);
            SelectObject(self.hdc, old);
            DeleteObject(HGDIOBJ(pen.0));
        }
        let s = HANDLE_SIDE;
        let half = s / 2.0;
        let white = unsafe { CreateSolidBrush(Self::gdi_color(COLOR_WHITE)) };
        let pen = unsafe { CreatePen(PS_SOLID, 2, Self::gdi_color(c)) };
        for (p, _) in handle_points(r) {
            let hr = Rect {
                x: p.x - half,
                y: p.y - half,
                w: s,
                h: s,
            };
            unsafe {
                let ob = SelectObject(self.hdc, HGDIOBJ(white.0));
                let op = SelectObject(self.hdc, HGDIOBJ(pen.0));
                Rectangle(
                    self.hdc,
                    hr.x as i32,
                    hr.y as i32,
                    hr.right() as i32,
                    hr.bottom() as i32,
                );
                SelectObject(self.hdc, op);
                SelectObject(self.hdc, ob);
            }
        }
        unsafe {
            DeleteObject(HGDIOBJ(white.0));
            DeleteObject(HGDIOBJ(pen.0));
        }
    }

    /// 聚光灯「调暗」区域（半透明，需配合 override 使用）
    ///
    /// 之前用「上下左右四个矩形」拼，圆外接框的四个角会漏掉不压暗，
    /// 看起来就是一块亮方块套着一个圆。改成区域运算：整屏减去圆，一次填满。
    pub fn spotlight_dim(&self, center: Point, radius: f32, sw: i32, sh: i32) {
        unsafe {
            let full = CreateRectRgn(0, 0, sw, sh);
            let hole = CreateEllipticRgn(
                (center.x - radius) as i32,
                (center.y - radius) as i32,
                (center.x + radius) as i32,
                (center.y + radius) as i32,
            );
            if full.is_invalid() || hole.is_invalid() {
                // 只成功创建了其中一个时也要还回去
                if !full.is_invalid() {
                    DeleteObject(HGDIOBJ(full.0));
                }
                if !hole.is_invalid() {
                    DeleteObject(HGDIOBJ(hole.0));
                }
                return;
            }
            // RGN_DIFF：full 减去 hole，剩下的就是圆外面那圈
            let _ = CombineRgn(full, full, hole, RGN_DIFF);
            let b = self.brush(Color::argb(150, 0, 0, 0));
            let old = SelectObject(self.hdc, HGDIOBJ(b.0));
            let _ = FillRgn(self.hdc, full, b);
            SelectObject(self.hdc, old);
            DeleteObject(HGDIOBJ(b.0));
            let _ = DeleteObject(HGDIOBJ(full.0));
            let _ = DeleteObject(HGDIOBJ(hole.0));
        }
    }

    /// 聚光灯高亮圈
    pub fn spotlight_border(&self, center: Point, radius: f32) {
        let pen = self.pen_raw(COLOR_WHITE, 2);
        self.stroke_ellipse(
            pen,
            Rect {
                x: center.x - radius,
                y: center.y - radius,
                w: radius * 2.0,
                h: radius * 2.0,
            },
        );
        unsafe {
            DeleteObject(HGDIOBJ(pen.0));
        }
    }

    fn arrow(&self, p1: Point, p2: Point, color: Color, width: f32) {
        let Some((bx, by, tri)) = arrow_geom(p1, p2, width) else {
            return;
        };
        let pen = self.pen(color, width as i32);
        self.line(pen, p1, Point::new(bx, by));
        unsafe {
            DeleteObject(HGDIOBJ(pen.0));
        }
        let b = self.brush(color);
        self.polygon(b, &tri);
        unsafe {
            DeleteObject(HGDIOBJ(b.0));
        }
    }

    fn wavy(&self, p1: Point, p2: Point, color: Color, width: f32) {
        let pts = wavy_points(p1, p2, width);
        let pen = self.pen(color, width as i32);
        for w in pts.windows(2) {
            self.line(pen, w[0], w[1]);
        }
        unsafe {
            DeleteObject(HGDIOBJ(pen.0));
        }
    }

    /// 箭头（抗锯齿版）：杆用 AA 折线，箭头用 AA 填充多边形。
    /// 返回 false 表示没画（长度太短），调用方会退回 GDI。
    fn arrow_aa(&self, g: &aa::G, p1: Point, p2: Point, color: Color, width: f32) -> bool {
        let (bx, by, tri) = match arrow_geom(p1, p2, width) {
            Some(v) => v,
            None => return false,
        };
        g.stroke(&[p1, Point::new(bx, by)], color, width);
        g.fill_poly(&tri, color);
        true
    }

    fn watermark(&self, text: &str, color: Color, size: f32, sw: i32, sh: i32) {
        // 用 self.col 让「占位色绘制」时水印文字也变成占位色，
        // 之后统一替换成「真色 + 半透明 alpha」
        let c = self.col(color).with_alpha(110);
        // -28° 斜排（顺时针 28°，基线向右下方延伸）：铺的间距按字号等比放大，
        // 字号一改密度才不会挤成一团或稀稀拉拉。
        // 但要给间距封顶：字号拉到最大时等比算出来的间距是三四千像素，
        // 整屏只剩一两处水印，看着像没铺。封到 1/3 屏就始终有 3×3 覆盖。
        let fs = size.max(8.0);
        let step_x = (fs * 12.3).min(sw as f32 / 3.0).max(fs);
        let step_y = (fs * 9.2).min(sh as f32 / 3.0).max(fs);
        // 字体、宽字符串、DC 状态只准备一次；原来每个锚点都调 text_angled()，
        // 一帧要 CreateFontW + encode_utf16 十几次。
        unsafe {
            let font = Self::angled_font(fs as i32, -28.0);
            let old_font = SelectObject(self.hdc, HGDIOBJ(font.0));
            SetBkMode(self.hdc, TRANSPARENT);
            SetTextColor(self.hdc, Self::gdi_color(c));
            let wide: Vec<u16> = text.encode_utf16().collect();
            let mut y = -step_y;
            while y < sh as f32 + step_y {
                let mut x = -step_x;
                while x < sw as f32 + step_x {
                    self.draw_angled_wide(&wide, Point::new(x, y));
                    x += step_x;
                }
                y += step_y;
            }
            SelectObject(self.hdc, old_font);
            DeleteObject(HGDIOBJ(font.0));
        }
    }

    fn blit(&self, data: &[u8], w: i32, h: i32, dst: Rect) {
        if w <= 0 || h <= 0 || data.is_empty() {
            return;
        }
        self.blit_sub(
            data,
            w,
            h,
            Rect {
                x: 0.0,
                y: 0.0,
                w: w as f32,
                h: h as f32,
            },
            dst,
        );
    }

    /// 从源图里「取一块」缩放到目标矩形（放大镜要用：只取圆内的那一片再放大）。
    fn blit_sub(&self, data: &[u8], sw: i32, sh: i32, src: Rect, dst: Rect) {
        if sw <= 0 || sh <= 0 || data.is_empty() {
            return;
        }
        // 取样框必须落在图内，否则 StretchDIBits 会读到边界外
        let x0 = src.x.floor().max(0.0) as i32;
        let y0 = src.y.floor().max(0.0) as i32;
        let x1 = src.right().ceil().min(sw as f32) as i32;
        let y1 = src.bottom().ceil().min(sh as f32) as i32;
        let (rw, rh) = (x1 - x0, y1 - y0);
        if rw <= 0 || rh <= 0 {
            return;
        }
        // 取样框被夹到图内时，目标矩形要按同样比例内缩，才能保持"放大镜中心对准鼠标"
        let kx = rw as f32 / src.w.max(1.0);
        let ky = rh as f32 / src.h.max(1.0);
        let d = Rect {
            x: dst.x + (x0 as f32 - src.x) * (dst.w / src.w.max(1.0)),
            y: dst.y + (y0 as f32 - src.y) * (dst.h / src.h.max(1.0)),
            w: dst.w * kx,
            h: dst.h * ky,
        };
        unsafe {
            // 坑：即便 biHeight 给的是负值（top-down），StretchDIBits 的 ySrc
            // 仍然是「从底边往上算」的 —— 实测从 y=1440 取 120 行，拿到的却是
            // [600,720)，正好是上下镜像。所以这里把 ySrc 换算成从底边量：
            // 想取 [y0, y0+rh) 就传 sh-y0-rh，行序本身不会翻。
            let bi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: sw,
                    biHeight: -sh,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                bmiColors: [RGBQUAD::default(); 1],
            };
            // 截图数据是 32 位 BGRA，但 GDI 的 StretchDIBits 会忽略 alpha，
            // 这里直接把 RGB 铺上去，再交给 fix_alpha 统一置为不透明。
            StretchDIBits(
                self.hdc,
                d.x as i32,
                d.y as i32,
                d.w as i32,
                d.h as i32,
                x0,
                sh - y0 - rh,
                rw,
                rh,
                Some(data.as_ptr() as *const core::ffi::c_void),
                &bi,
                DIB_RGB_COLORS,
                SRCCOPY,
            );
        }
    }
}

/// 按内容计算文字框尺寸（含内边距）。
///
/// 宽度 = max(最长一行的自然宽度, min_chars 个字) —— 所以短文本保持默认宽度，
/// 单行写超了会自己横向长出去；显式换行（\n）则只增加高度。
pub fn measure_text_box(text: &str, font_size: i32, min_chars: f32) -> (f32, f32) {
    let fsz = font_size.max(6);
    let natural_w = unsafe {
        let hdc = GetDC(None);
        let font = CreateFontW(
            -fsz,
            0,
            0,
            0,
            400,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            0,
            0,
            0,
            0,
            w!("Microsoft YaHei"),
        );
        let old = SelectObject(hdc, HGDIOBJ(font.0));
        let mut max_w = 0i32;
        // 逐行量：DT_SINGLELINE 关闭自动折行，量到的才是「这一行本来有多宽」
        for line in text.split('\n') {
            let mut wide: Vec<u16> = line.encode_utf16().collect();
            wide.push(0);
            let mut rr = RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: fsz + 16,
            };
            DrawTextW(
                hdc,
                &mut wide,
                &mut rr,
                DT_LEFT | DT_TOP | DT_SINGLELINE | DT_CALCRECT,
            );
            max_w = max_w.max(rr.right - rr.left);
        }
        SelectObject(hdc, old);
        DeleteObject(HGDIOBJ(font.0));
        ReleaseDC(None, hdc);
        max_w
    };

    let min_w = min_chars * fsz as f32 + 16.0;
    // 上限：单行太长会一路长到屏幕外，超过屏幕 80% 就让它自己折行
    let cap_w = unsafe {
        let sw = windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
            windows::Win32::UI::WindowsAndMessaging::SM_CXSCREEN,
        ) as f32;
        (sw * 0.8).max(min_w)
    };
    let w = (natural_w as f32 + 16.0).max(min_w).min(cap_w);
    // 高度按这个宽度折行后的实际行数算（显式 \n 也算行）
    let inner = (w - 12.0).max(20.0) as i32;
    let h = measure_text_height(text, inner, fsz) as f32 + 14.0;
    (w, h)
}

/// 测量一段文字在指定宽度内需要多高（用于文字框自动调整高度）
pub fn measure_text_height(text: &str, width: i32, font_size: i32) -> i32 {
    unsafe {
        let hdc = GetDC(None);
        let font = CreateFontW(
            -font_size.max(6),
            0,
            0,
            0,
            400,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            0,
            0,
            0,
            0,
            w!("Microsoft YaHei"),
        );
        let old = SelectObject(hdc, HGDIOBJ(font.0));
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        wide.push(0);
        let mut rr = RECT {
            left: 0,
            top: 0,
            right: width.max(20),
            bottom: 0,
        };
        DrawTextW(hdc, &mut wide, &mut rr, DT_LEFT | DT_TOP | DT_WORDBREAK | DT_CALCRECT);
        let h = rr.bottom - rr.top;
        SelectObject(hdc, old);
        DeleteObject(HGDIOBJ(font.0));
        ReleaseDC(None, hdc);
        h.max(font_size + 4)
    }
}

/// 抓取屏幕矩形区域（BGRA）。调用前需先隐藏画布，否则会把自己截进去。
pub fn capture_screen(x: i32, y: i32, w: i32, h: i32) -> Option<(Vec<u8>, i32, i32)> {
    if w <= 0 || h <= 0 {
        return None;
    }
    unsafe {
        let hdc_screen = GetDC(None);
        if hdc_screen.is_invalid() {
            return None;
        }
        let hdc_mem = CreateCompatibleDC(hdc_screen);
        let bmp = CreateCompatibleBitmap(hdc_screen, w, h);
        let old = SelectObject(hdc_mem, HGDIOBJ(bmp.0));
        let blt_ok = BitBlt(hdc_mem, 0, 0, w, h, hdc_screen, x, y, SRCCOPY).is_ok();
        let mut bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            bmiColors: [RGBQUAD::default(); 1],
        };
        let mut data = vec![0u8; (w * h * 4) as usize];
        let lines = GetDIBits(
            hdc_mem,
            bmp,
            0,
            h as u32,
            Some(data.as_mut_ptr() as *mut core::ffi::c_void),
            &mut bi,
            DIB_RGB_COLORS,
        );
        SelectObject(hdc_mem, old);
        DeleteObject(HGDIOBJ(bmp.0));
        DeleteDC(hdc_mem);
        ReleaseDC(None, hdc_screen);
        // 失败时返回 None，别把一张全黑的位图当快照交给马赛克/放大镜
        if !blt_ok || lines == 0 {
            return None;
        }
        Some((data, w, h))
    }
}

/// 马赛克：按 block 大小取平均色填充
pub fn pixelate(data: &[u8], w: i32, h: i32, block: i32) -> Vec<u8> {
    let block = block.max(2);
    let mut out = data.to_vec();
    if w <= 0 || h <= 0 {
        return out;
    }
    let mut by = 0;
    while by < h {
        let mut bx = 0;
        while bx < w {
            let (mut sr, mut sg, mut sb, mut cnt) = (0u32, 0u32, 0u32, 0u32);
            let y_end = (by + block).min(h);
            let x_end = (bx + block).min(w);
            for yy in by..y_end {
                for xx in bx..x_end {
                    let idx = ((yy * w + xx) * 4) as usize;
                    if idx + 3 < data.len() {
                        sb += data[idx] as u32;
                        sg += data[idx + 1] as u32;
                        sr += data[idx + 2] as u32;
                        cnt += 1;
                    }
                }
            }
            if cnt > 0 {
                sr /= cnt;
                sg /= cnt;
                sb /= cnt;
            }
            for yy in by..y_end {
                for xx in bx..x_end {
                    let idx = ((yy * w + xx) * 4) as usize;
                    if idx + 3 < out.len() {
                        out[idx] = sb as u8;
                        out[idx + 1] = sg as u8;
                        out[idx + 2] = sr as u8;
                        out[idx + 3] = 255;
                    }
                }
            }
            bx += block;
        }
        by += block;
    }
    out
}

/// 给画布用的占位色（与 frame 模块保持一致）
pub fn magic_rgb() -> (u8, u8, u8) {
    MAGIC
}

// ---------- 几何辅助 ----------

/// 箭头几何：返回 (杆末端, 箭头三角)。长度太短返回 None。
fn arrow_geom(p1: Point, p2: Point, width: f32) -> Option<(f32, f32, [Point; 3])> {
    let dx = p2.x - p1.x;
    let dy = p2.y - p1.y;
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1.0 {
        return None;
    }
    let ux = dx / len;
    let uy = dy / len;
    let nx = -uy;
    let ny = ux;
    let head = 12.0 + width * 2.0;
    let half = 5.0 + width;
    let bx = p2.x - head * ux;
    let by = p2.y - head * uy;
    Some((
        bx,
        by,
        [
            p2,
            Point::new(bx + half * nx, by + half * ny),
            Point::new(bx - half * nx, by - half * ny),
        ],
    ))
}

/// 波浪线的采样点
fn wavy_points(p1: Point, p2: Point, width: f32) -> Vec<Point> {
    let dx = p2.x - p1.x;
    let dy = p2.y - p1.y;
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1.0 {
        return vec![p1, p2];
    }
    let nx = -dy / len;
    let ny = dx / len;
    let amp = (width * 1.5).max(6.0);
    let waves = (len / 24.0).max(1.0);
    let steps = ((waves * 16.0) as i32).max(2);
    let mut pts = Vec::with_capacity(steps as usize + 1);
    for i in 0..=steps {
        let t = i as f32 / steps as f32;
        let off = amp * (t * std::f32::consts::PI * waves * 2.0).sin();
        pts.push(Point::new(p1.x + dx * t + nx * off, p1.y + dy * t + ny * off));
    }
    pts
}

// ---------- GDI+ 抗锯齿 ----------
//
// 纯 GDI 画线是硬边的：斜线、圆、字符边缘都是一格一格的锯齿。
// 工具条那套「4 倍超采样再缩回」对整屏画布不现实（3840×2160 的 4 倍要 500MB），
// 所以画布改走 GDI+：SmoothingModeAntiAlias 出来的边缘就是半透明覆盖率，
// 再配合 Frame::unmix 把它还原成真正的 alpha，边缘就能和底下的桌面正确混合。
pub mod aa {
    use super::*;
    // 注意：不能用 glob 导入 GdiPlus，它自带 Point/Rect/Color 之类的同名类型，
    // 会和 shape 模块里的模型撞名（E0659），所以逐个列出来。
    use windows::Win32::Graphics::GdiPlus::{
        FillModeAlternate, GdipCreateFromHDC, GdipCreatePen1, GdipCreateSolidFill,
        GdipDeleteBrush, GdipDeleteGraphics, GdipDeletePen, GdipDrawEllipse, GdipDrawLines,
        GdipDrawRectangle, GdipFillEllipse, GdipFillPolygon, GdipSetPenEndCap,
        GdipSetPenLineJoin, GdipSetPenStartCap, GdipSetSmoothingMode, GdiplusStartup,
        GdiplusStartupInput, GpBrush, GpGraphics, GpPen, GpSolidFill, LineCapRound,
        LineJoinRound, PointF, SmoothingModeAntiAlias, UnitPixel,
    };

    static mut TOKEN: usize = 0;

    fn ready() -> bool {
        unsafe {
            if TOKEN != 0 {
                return true;
            }
            let mut token: usize = 0;
            let input = GdiplusStartupInput {
                GdiplusVersion: 1,
                DebugEventCallback: 0,
                SuppressBackgroundThread: BOOL(0),
                SuppressExternalCodecs: BOOL(0),
            };
            if GdiplusStartup(&mut token, &input, std::ptr::null_mut()).0 != 0 {
                return false;
            }
            TOKEN = token;
            true
        }
    }

    /// 一次绘制用的 Graphics（Drop 时释放）
    pub struct G(*mut GpGraphics);

    impl G {
        pub fn new(hdc: HDC) -> Option<G> {
            if !ready() {
                return None;
            }
            unsafe {
                let mut g: *mut GpGraphics = std::ptr::null_mut();
                if GdipCreateFromHDC(hdc, &mut g).0 != 0 || g.is_null() {
                    return None;
                }
                let _ = GdipSetSmoothingMode(g, SmoothingModeAntiAlias);
                Some(G(g))
            }
        }

        pub fn stroke(&self, pts: &[Point], color: Color, width: f32) {
            if pts.len() < 2 {
                return;
            }
            let Some(p) = Pen::new(color, width) else {
                return;
            };
            let v: Vec<PointF> = pts.iter().map(|q| PointF { X: q.x, Y: q.y }).collect();
            unsafe {
                let _ = GdipDrawLines(self.0, p.0, v.as_ptr(), v.len() as i32);
            }
        }

        pub fn rect(&self, r: Rect, color: Color, width: f32) {
            let Some(p) = Pen::new(color, width) else {
                return;
            };
            unsafe {
                let _ = GdipDrawRectangle(self.0, p.0, r.x, r.y, r.w, r.h);
            }
        }

        pub fn ellipse(&self, r: Rect, color: Color, width: f32) {
            let Some(p) = Pen::new(color, width) else {
                return;
            };
            unsafe {
                let _ = GdipDrawEllipse(self.0, p.0, r.x, r.y, r.w, r.h);
            }
        }

        pub fn fill_ellipse(&self, r: Rect, color: Color) {
            let Some(b) = Brush::new(color) else {
                return;
            };
            unsafe {
                let _ = GdipFillEllipse(self.0, b.0, r.x, r.y, r.w, r.h);
            }
        }

        pub fn fill_poly(&self, pts: &[Point], color: Color) {
            if pts.len() < 3 {
                return;
            }
            let Some(b) = Brush::new(color) else {
                return;
            };
            let v: Vec<PointF> = pts.iter().map(|q| PointF { X: q.x, Y: q.y }).collect();
            unsafe {
                let _ = GdipFillPolygon(self.0, b.0, v.as_ptr(), v.len() as i32, FillModeAlternate);
            }
        }
    }

    impl Drop for G {
        fn drop(&mut self) {
            unsafe {
                let _ = GdipDeleteGraphics(self.0);
            }
        }
    }

    struct Pen(*mut GpPen);

    impl Pen {
        fn new(color: Color, width: f32) -> Option<Pen> {
            unsafe {
                let mut p: *mut GpPen = std::ptr::null_mut();
                if GdipCreatePen1(color.0, width.max(1.0), UnitPixel, &mut p).0 != 0 || p.is_null() {
                    return None;
                }
                // 圆头圆角：折线拐点不会出现尖刺，笔迹端点也不会是方的
                let _ = GdipSetPenLineJoin(p, LineJoinRound);
                let _ = GdipSetPenStartCap(p, LineCapRound);
                let _ = GdipSetPenEndCap(p, LineCapRound);
                Some(Pen(p))
            }
        }
    }

    impl Drop for Pen {
        fn drop(&mut self) {
            unsafe {
                let _ = GdipDeletePen(self.0);
            }
        }
    }

    struct Brush(*mut GpBrush);

    impl Brush {
        fn new(color: Color) -> Option<Brush> {
            unsafe {
                let mut b: *mut GpSolidFill = std::ptr::null_mut();
                if GdipCreateSolidFill(color.0, &mut b).0 != 0 || b.is_null() {
                    return None;
                }
                Some(Brush(b as *mut GpBrush))
            }
        }
    }

    impl Drop for Brush {
        fn drop(&mut self) {
            unsafe {
                let _ = GdipDeleteBrush(self.0);
            }
        }
    }
}

// ---------- 选中框手柄 ----------

/// 手柄边长（96dpi 逻辑像素）
pub const HANDLE_SIDE: f32 = 10.0;

/// 选中框的 8 个手柄：返回 (中心点, 方向掩码)。
/// 掩码位：1=左边 2=右边 4=上边 8=下边（两个位同时置位 = 角点）
pub fn handle_points(r: Rect) -> [(Point, u8); 8] {
    let cx = r.x + r.w / 2.0;
    let cy = r.y + r.h / 2.0;
    [
        (Point::new(r.x, r.y), 1 | 4),          // 左上
        (Point::new(cx, r.y), 4),               // 上
        (Point::new(r.right(), r.y), 2 | 4),    // 右上
        (Point::new(r.right(), cy), 2),         // 右
        (Point::new(r.right(), r.bottom()), 2 | 8), // 右下
        (Point::new(cx, r.bottom()), 8),        // 下
        (Point::new(r.x, r.bottom()), 1 | 8),   // 左下
        (Point::new(r.x, cy), 1),               // 左
    ]
}

/// 圆形图元（放大镜 / 聚光灯）的控制点：上下左右四个，直接贴在圆周上。
/// 「拉伸圆形的边」比拖外接方框的 8 个角点更符合直觉，也不会在圆外多出一个方框。
pub fn circle_handle_points(cx: f32, cy: f32, r: f32) -> [(Point, u8); 4] {
    [
        (Point::new(cx, cy - r), 4), // 上
        (Point::new(cx + r, cy), 2), // 右
        (Point::new(cx, cy + r), 8), // 下
        (Point::new(cx - r, cy), 1), // 左
    ]
}

/// 点落在圆形图元的哪个控制点上（0 = 没命中）
pub fn circle_handle_hit(cx: f32, cy: f32, r: f32, p: Point, tol: f32) -> u8 {
    let lim = (HANDLE_SIDE / 2.0 + tol).max(8.0);
    for (hp, mask) in circle_handle_points(cx, cy, r) {
        if (p.x - hp.x).abs() <= lim && (p.y - hp.y).abs() <= lim {
            return mask;
        }
    }
    0
}

/// 点落在哪个手柄上（返回方向掩码，0 = 没命中）
pub fn handle_hit(r: Rect, p: Point, tol: f32) -> u8 {
    let lim = (HANDLE_SIDE / 2.0 + tol).max(6.0);
    for (hp, mask) in handle_points(r) {
        if (p.x - hp.x).abs() <= lim && (p.y - hp.y).abs() <= lim {
            return mask;
        }
    }
    0
}

// ---------- 文字墨迹测量 ----------

thread_local! {
    /// (文本, 字号) → 墨迹框 (left, top, w, h)，相对笔位原点。
    /// 不缓存的话每帧重绘都要建一个 DIB 再扫一遍像素，滚轮连续缩放时会掉帧。
    static INK_CACHE: RefCell<Vec<(String, i32, (i32, i32, i32, i32))>> = const { RefCell::new(Vec::new()) };
}

fn ink_box(s: &str, font_size: i32) -> Option<(i32, i32, i32, i32)> {
    let cached = INK_CACHE.with(|c| {
        c.borrow()
            .iter()
            .find(|(t, f, _)| t == s && *f == font_size)
            .map(|(_, _, b)| *b)
    });
    if let Some(b) = cached {
        return Some(b);
    }
    let b = measure_ink(s, font_size)?;
    INK_CACHE.with(|c| {
        let mut v = c.borrow_mut();
        if v.len() >= 128 {
            v.clear();
        }
        v.push((s.to_string(), font_size, b));
    });
    Some(b)
}

/// 把文字画到一张离屏位图上，扫描非黑像素得到真实墨迹包围盒。
///
/// 比 GetGlyphOutline/ABCFLOAT 那套更好：完全不依赖字体度量语义，
/// 连「1 的字形在盒子里偏左」这种字体自身的不对称也一并量准了。
fn measure_ink(s: &str, font_size: i32) -> Option<(i32, i32, i32, i32)> {
    let fs = font_size.max(6);
    unsafe {
        let screen = GetDC(None);
        if screen.is_invalid() {
            return None;
        }
        let mem = CreateCompatibleDC(screen);
        if mem.is_invalid() {
            ReleaseDC(None, screen);
            return None;
        }
        // 留够余量：字号两倍边距，字符数 + 2 的横向预算，斜体/连笔也不会被切
        let pad = fs + 6;
        let bw = ((s.chars().count().max(1) as i32 + 2) * fs).max(32) + pad * 2;
        let bh = fs * 3 + pad * 2;
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: bw,
                biHeight: -bh,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            bmiColors: [RGBQUAD::default(); 1],
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        // 原来的 `.ok()?` 会把 screen/mem 两个 DC 一起漏掉。
        let dib = match CreateDIBSection(screen, &bi, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(d) => d,
            Err(_) => {
                DeleteDC(mem);
                ReleaseDC(None, screen);
                return None;
            }
        };
        let old = SelectObject(mem, HGDIOBJ(dib.0));

        // 黑底
        let blk = CreateSolidBrush(COLORREF(0));
        FillRect(
            mem,
            &RECT {
                left: 0,
                top: 0,
                right: bw,
                bottom: bh,
            },
            blk,
        );
        DeleteObject(HGDIOBJ(blk.0));

        // 白字，笔位固定在 (pad, pad)，跟最终绘制用的是同一个 API，参考点语义一致
        let font = CreateFontW(
            -fs,
            0,
            0,
            0,
            400,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            0,
            0,
            0,
            0,
            w!("Microsoft YaHei"),
        );
        let of = SelectObject(mem, HGDIOBJ(font.0));
        SetBkMode(mem, TRANSPARENT);
        SetTextColor(mem, COLORREF(0x00FF_FFFF));
        let wide: Vec<u16> = s.encode_utf16().collect();
        if !wide.is_empty() {
            ExtTextOutW(
                mem,
                pad,
                pad,
                ETO_OPTIONS(0),
                None,
                PCWSTR(wide.as_ptr()),
                wide.len() as u32,
                None,
            );
        }
        SelectObject(mem, of);
        DeleteObject(HGDIOBJ(font.0));
        GdiFlush();

        let px = std::slice::from_raw_parts(bits as *const u32, (bw * bh) as usize);
        let (mut minx, mut miny, mut maxx, mut maxy) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
        for y in 0..bh {
            for x in 0..bw {
                if px[(y * bw + x) as usize] & 0x00FF_FFFF != 0 {
                    minx = minx.min(x);
                    maxx = maxx.max(x);
                    miny = miny.min(y);
                    maxy = maxy.max(y);
                }
            }
        }

        SelectObject(mem, old);
        DeleteObject(HGDIOBJ(dib.0));
        DeleteDC(mem);
        ReleaseDC(None, screen);

        if maxx < minx {
            return None; // 全黑 = 没画出任何墨迹（空白串）
        }
        Some((
            minx - pad,
            miny - pad,
            maxx - minx + 1,
            maxy - miny + 1,
        ))
    }
}
