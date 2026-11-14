// 工具条图标：全部用 GDI 原语手绘。
//
// 设计风格：极简线性、深灰描边、选中时变红。所有坐标都按 24×24 的逻辑网格书写，
// 再按按钮实际尺寸缩放，这样换 DPI 或改按钮大小都不用重写路径。

#![allow(dead_code)]

use crate::shape::Tool;

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;

pub const NORMAL: (u8, u8, u8) = (0x5B, 0x64, 0x70);
/// 未选中但悬停：深灰图标转成品牌红（边框也同时变红）
pub const ACTIVE: (u8, u8, u8) = (0xFF, 0x4D, 0x4F);
/// 选中态是实心红底，图标必须反白才看得见
pub const ACTIVE_ON: (u8, u8, u8) = (0xFF, 0xFF, 0xFF);

fn cref(rgb: (u8, u8, u8)) -> COLORREF {
    COLORREF(((rgb.2 as u32) << 16) | ((rgb.1 as u32) << 8) | rgb.0 as u32)
}

/// 把 24 网格坐标映射到设备坐标，可选绕中心旋转
struct G {
    hdc: HDC,
    ox: f32,
    oy: f32,
    s: f32,
    rot: f32,
    cx: f32,
    cy: f32,
}

impl G {
    fn new(hdc: HDC, r: RECT) -> Self {
        let w = (r.right - r.left) as f32;
        let h = (r.bottom - r.top) as f32;
        let side = w.min(h);
        let s = side / 24.0;
        G {
            hdc,
            ox: r.left as f32 + (w - side) / 2.0,
            oy: r.top as f32 + (h - side) / 2.0,
            s,
            rot: 0.0,
            cx: 12.0,
            cy: 12.0,
        }
    }

    fn rotated(mut self, deg: f32) -> Self {
        self.rot = deg.to_radians();
        self
    }

    fn p(&self, x: f32, y: f32) -> POINT {
        let (mut dx, mut dy) = (x - self.cx, y - self.cy);
        if self.rot != 0.0 {
            let (sin, cos) = self.rot.sin_cos();
            let (nx, ny) = (dx * cos - dy * sin, dx * sin + dy * cos);
            dx = nx;
            dy = ny;
        }
        POINT {
            x: (self.ox + (self.cx + dx) * self.s) as i32,
            y: (self.oy + (self.cy + dy) * self.s) as i32,
        }
    }

    fn pts(&self, list: &[(f32, f32)]) -> Vec<POINT> {
        list.iter().map(|(x, y)| self.p(*x, *y)).collect()
    }

    fn lw(&self, w: f32) -> i32 {
        ((w * self.s) as i32).max(1)
    }

    /// 折线（w = 线宽，单位是 24 网格里的像素）
    fn poly(&self, list: &[(f32, f32)], w: f32, col: COLORREF) {
        if list.len() < 2 {
            return;
        }
        unsafe {
            let pen = CreatePen(PS_SOLID, self.lw(w), col);
            let old = SelectObject(self.hdc, HGDIOBJ(pen.0));
            let pts = self.pts(list);
            Polyline(self.hdc, &pts);
            SelectObject(self.hdc, old);
            DeleteObject(HGDIOBJ(pen.0));
        }
    }

    fn line(&self, a: (f32, f32), b: (f32, f32), w: f32, col: COLORREF) {
        self.poly(&[a, b], w, col);
    }

    /// 实心多边形
    fn fill(&self, list: &[(f32, f32)], col: COLORREF) {
        if list.len() < 3 {
            return;
        }
        unsafe {
            let b = CreateSolidBrush(col);
            let old = SelectObject(self.hdc, HGDIOBJ(b.0));
            let op = SelectObject(self.hdc, GetStockObject(NULL_PEN));
            let pts = self.pts(list);
            Polygon(self.hdc, &pts);
            SelectObject(self.hdc, op);
            SelectObject(self.hdc, old);
            DeleteObject(HGDIOBJ(b.0));
        }
    }

    fn rect(&self, x: f32, y: f32, w: f32, h: f32, pen_w: f32, col: COLORREF) {
        let p1 = self.p(x, y);
        let p2 = self.p(x + w, y + h);
        unsafe {
            let pen = CreatePen(PS_SOLID, self.lw(pen_w), col);
            let old = SelectObject(self.hdc, HGDIOBJ(pen.0));
            let ob = SelectObject(self.hdc, GetStockObject(NULL_BRUSH));
            Rectangle(self.hdc, p1.x, p1.y, p2.x, p2.y);
            SelectObject(self.hdc, ob);
            SelectObject(self.hdc, old);
            DeleteObject(HGDIOBJ(pen.0));
        }
    }

    fn dashed_rect(&self, x: f32, y: f32, w: f32, h: f32, col: COLORREF) {
        let p1 = self.p(x, y);
        let p2 = self.p(x + w, y + h);
        unsafe {
            let pen = CreatePen(PS_DOT, self.lw(1.6), col);
            let old = SelectObject(self.hdc, HGDIOBJ(pen.0));
            let ob = SelectObject(self.hdc, GetStockObject(NULL_BRUSH));
            Rectangle(self.hdc, p1.x, p1.y, p2.x, p2.y);
            SelectObject(self.hdc, ob);
            SelectObject(self.hdc, old);
            DeleteObject(HGDIOBJ(pen.0));
        }
    }

    fn circle(&self, cx: f32, cy: f32, r: f32, pen_w: f32, col: COLORREF) {
        let p1 = self.p(cx - r, cy - r);
        let p2 = self.p(cx + r, cy + r);
        unsafe {
            let pen = CreatePen(PS_SOLID, self.lw(pen_w), col);
            let old = SelectObject(self.hdc, HGDIOBJ(pen.0));
            let ob = SelectObject(self.hdc, GetStockObject(NULL_BRUSH));
            Ellipse(self.hdc, p1.x, p1.y, p2.x, p2.y);
            SelectObject(self.hdc, ob);
            SelectObject(self.hdc, old);
            DeleteObject(HGDIOBJ(pen.0));
        }
    }

    fn disc(&self, cx: f32, cy: f32, r: f32, col: COLORREF) {
        let p1 = self.p(cx - r, cy - r);
        let p2 = self.p(cx + r, cy + r);
        unsafe {
            let b = CreateSolidBrush(col);
            let old = SelectObject(self.hdc, HGDIOBJ(b.0));
            let op = SelectObject(self.hdc, GetStockObject(NULL_PEN));
            Ellipse(self.hdc, p1.x, p1.y, p2.x, p2.y);
            SelectObject(self.hdc, op);
            SelectObject(self.hdc, old);
            DeleteObject(HGDIOBJ(b.0));
        }
    }

    /// 用文字画图标（序号里的「1」）
    fn glyph(&self, s: &str, cx: f32, cy: f32, size: f32, col: COLORREF) {
        unsafe {
            let font = CreateFontW(
                -self.lw(size),
                0,
                0,
                0,
                700,
                0,
                0,
                0,
                DEFAULT_CHARSET.0 as u32,
                0,
                0,
                0,
                0,
                w!("Arial"),
            );
            let old = SelectObject(self.hdc, HGDIOBJ(font.0));
            SetBkMode(self.hdc, TRANSPARENT);
            SetTextColor(self.hdc, col);
            let mut wide: Vec<u16> = s.encode_utf16().collect();
            wide.push(0);
            let p1 = self.p(cx - 6.0, cy - 6.0);
            let p2 = self.p(cx + 6.0, cy + 6.0);
            let mut rr = RECT {
                left: p1.x,
                top: p1.y,
                right: p2.x,
                bottom: p2.y,
            };
            DrawTextW(self.hdc, &mut wide, &mut rr, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
            SelectObject(self.hdc, old);
            DeleteObject(HGDIOBJ(font.0));
        }
    }
}

/// 一个小圆点（折线顶点用）
fn dot(g: &G, x: f32, y: f32, r: f32, col: COLORREF) {
    g.disc(x, y, r, col);
}

/// 画一个工具图标。r 是按钮矩形，color 指定描边颜色。
pub fn draw_tool_icon(hdc: HDC, r: RECT, tool: Tool, color: (u8, u8, u8)) {
    let col = cref(color);
    let g = G::new(hdc, r);
    match tool {
        Tool::Pen => {
            // 铅笔：斜置笔身 + 实心笔尖三角 + 金属箍分隔线
            // （之前只有一圈细轮廓，缩到 30px 时笔尖看不出来，像根棍子）
            g.poly(
                &[
                    (16.2, 2.8),
                    (21.2, 7.8),
                    (9.4, 19.6),
                    (4.2, 19.6),
                    (4.2, 14.4),
                    (16.2, 2.8),
                ],
                1.8,
                col,
            );
            // 笔尖：左下角的实心三角，这是「有没有笔尖」的关键
            g.fill(&[(4.2, 19.6), (4.2, 14.4), (9.4, 19.6)], col);
            // 金属箍
            g.line((14.2, 4.8), (19.2, 9.8), 1.6, col);
            // 笔尾（橡皮头）短线
            g.line((17.6, 4.0), (20.0, 6.4), 1.4, col);
        }
        Tool::Highlighter => {
            // 荧光笔：45° 斜置马克笔，粗笔身 + 实心斜切笔尖
            let g = g.rotated(-45.0);
            // 旋转后的矩形不能用 Rectangle（它只画轴对齐），必须自己连多边形
            g.poly(
                &[
                    (8.5, 2.5),
                    (15.5, 2.5),
                    (15.5, 13.5),
                    (8.5, 13.5),
                    (8.5, 2.5),
                ],
                1.8,
                col,
            );
            g.fill(&[(8.5, 13.5), (15.5, 13.5), (13.4, 21.0), (10.6, 21.0)], col);
            g.line((9.0, 9.5), (15.0, 9.5), 1.6, col);
        }
        Tool::Rectangle => g.rect(3.5, 6.0, 17.0, 12.0, 1.9, col),
        Tool::Ellipse => g.circle(12.0, 12.0, 8.2, 1.9, col),
        Tool::Spotlight => {
            g.disc(12.0, 12.0, 4.0, col);
            for (a, b) in [
                ((12.0, 2.5), (12.0, 5.0)),
                ((12.0, 19.0), (12.0, 21.5)),
                ((2.5, 12.0), (5.0, 12.0)),
                ((19.0, 12.0), (21.5, 12.0)),
                ((5.4, 5.4), (7.2, 7.2)),
                ((16.8, 16.8), (18.6, 18.6)),
                ((18.6, 5.4), (16.8, 7.2)),
                ((5.4, 18.6), (7.2, 16.8)),
            ] {
                g.line(a, b, 1.8, col);
            }
        }
        Tool::Arrow => {
            g.line((4.0, 20.0), (14.5, 9.5), 1.9, col);
            g.fill(&[(20.0, 4.0), (11.5, 5.4), (18.6, 12.5)], col);
        }
        Tool::Polyline => {
            let path = [(4.0, 18.0), (9.0, 8.0), (15.0, 15.0), (20.0, 6.0)];
            g.poly(&path, 1.8, col);
            for (x, y) in path {
                dot(&g, x, y, 1.7, col);
            }
        }
        Tool::Wavy => {
            let mut pts = Vec::new();
            let mut i = 0;
            while i <= 48 {
                let t = i as f32 / 48.0;
                let x = 2.5 + t * 19.0;
                let y = 12.0 - 5.0 * (t * std::f32::consts::PI * 4.0).sin();
                pts.push((x, y));
                i += 1;
            }
            g.poly(&pts, 1.8, col);
        }
        Tool::Magnifier => {
            g.circle(10.0, 10.0, 6.2, 1.9, col);
            g.line((14.6, 14.6), (20.0, 20.0), 2.2, col);
        }
        Tool::Text => {
            g.line((5.0, 5.5), (19.0, 5.5), 2.0, col);
            g.line((12.0, 5.5), (12.0, 19.5), 2.0, col);
        }
        Tool::Watermark => {
            // 水滴：上半三角 + 下半圆，中间留白
            g.poly(&[(12.0, 3.0), (17.5, 12.5), (6.5, 12.5), (12.0, 3.0)], 1.8, col);
            g.circle(12.0, 15.0, 5.6, 1.8, col);
        }
        Tool::Number => {
            g.circle(12.0, 12.0, 8.4, 1.9, col);
            // 数字 1 用笔画出来：不依赖字体，永远不会变成豆腐块
            g.line((10.2, 8.6), (12.2, 6.9), 1.6, col);
            g.line((12.2, 6.9), (12.2, 17.2), 1.7, col);
        }
        Tool::Mosaic => {
            // 打码：外框 + 2×2 棋盘格（深浅交替）。
            // 原来的 3×3 全填色缩到 30px 像九宫格键盘，看不出「马赛克」的意思。
            g.rect(3.4, 3.4, 17.2, 17.2, 1.8, col);
            // 选中态是红底 + 白图标：棋盘格的「暗格」不能用原来的浅灰，
            // 否则红底上会糊成一块，这里换成半透明的浅红（用实色近似）。
            let dim = if color == ACTIVE_ON {
                cref((0xFF, 0xB8, 0xB9))
            } else {
                cref((175, 183, 193))
            };
            let cell = 7.0;
            for (i, x) in [5.0f32, 12.0].iter().enumerate() {
                for (j, y) in [5.0f32, 12.0].iter().enumerate() {
                    let c = if (i + j) % 2 == 0 { col } else { dim };
                    g.fill(&[(*x, *y), (*x + cell, *y), (*x + cell, *y + cell), (*x, *y + cell)], c);
                }
            }
        }
        Tool::Eraser => {
            g.poly(
                &[
                    (3.5, 15.5),
                    (10.5, 8.0),
                    (16.5, 14.0),
                    (12.5, 18.0),
                    (7.0, 18.0),
                    (3.5, 15.5),
                ],
                1.9,
                col,
            );
            g.line((8.5, 21.0), (20.5, 21.0), 1.9, col);
        }
        Tool::Select => {
            g.dashed_rect(3.0, 3.0, 18.0, 18.0, col);
            g.line((12.0, 8.0), (12.0, 16.0), 1.6, col);
            g.line((8.0, 12.0), (16.0, 12.0), 1.6, col);
            g.fill(&[(12.0, 6.0), (10.0, 9.0), (14.0, 9.0)], col);
            g.fill(&[(12.0, 18.0), (10.0, 15.0), (14.0, 15.0)], col);
            g.fill(&[(6.0, 12.0), (9.0, 10.0), (9.0, 14.0)], col);
            g.fill(&[(18.0, 12.0), (15.0, 10.0), (15.0, 14.0)], col);
        }
        _ => {}
    }
}

/// 撤销：左侧实心箭头（指左）+ 向右伸出再向下勾回的圆弧，即标准的「↩」
///
/// 之前画的是「半个圆 + 左端一坨三角」，缩到 20 多像素后既不像箭头也不像圆，
/// 重画时把形状拆成三部分：箭头(实心)、横杆、右下的钩。
pub fn draw_undo(hdc: HDC, r: RECT, color: (u8, u8, u8)) {
    let col = cref(color);
    let g = G::new(hdc, r);

    let y = 8.0;          // 横杆所在高度
    let cx = 14.2;        // 钩的圆心
    let cy = 13.6;
    let rad = 5.6;

    // 横杆：从箭头根部一直连到钩的起点
    g.line((8.0, y), (cx, y), 1.9, col);

    // 钩：从正上方顺时针绕到左下（90° → -112°），
    // 角度必须是递减的，递增会绕到左边去，跟箭头糊在一起
    let mut pts = Vec::new();
    let mut i = 0;
    while i <= 20 {
        let deg = 90.0 - (90.0 + 112.0) * (i as f32 / 20.0);
        let t = deg * std::f32::consts::PI / 180.0;
        pts.push((cx + rad * t.cos(), cy - rad * t.sin()));
        i += 1;
    }
    g.poly(&pts, 1.9, col);

    // 箭头：实心三角，尖端朝左
    g.fill(&[(2.6, y), (8.7, y - 4.3), (8.7, y + 4.3)], col);
}

/// 清空（垃圾桶）
pub fn draw_clear(hdc: HDC, r: RECT, color: (u8, u8, u8)) {
    let col = cref(color);
    let g = G::new(hdc, r);
    g.line((4.0, 7.0), (20.0, 7.0), 1.8, col);
    g.poly(&[(9.0, 7.0), (9.0, 4.0), (15.0, 4.0), (15.0, 7.0)], 1.8, col);
    g.poly(&[(6.0, 7.5), (7.2, 20.5), (16.8, 20.5), (18.0, 7.5)], 1.8, col);
    g.line((10.5, 10.5), (11.0, 17.5), 1.5, col);
    g.line((13.5, 10.5), (13.0, 17.5), 1.5, col);
}
