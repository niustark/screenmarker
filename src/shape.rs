// 图形数据模型：所有可在画布上绘制的图形，以及全局状态管理。

#![allow(dead_code)]

// ---------- 基础类型 ----------
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub fn new(x: f32, y: f32) -> Self {
        Point { x, y }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn from_points(a: Point, b: Point) -> Self {
        Rect {
            x: a.x.min(b.x),
            y: a.y.min(b.y),
            w: (a.x - b.x).abs(),
            h: (a.y - b.y).abs(),
        }
    }

    pub fn right(&self) -> f32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
    pub fn contains(&self, p: Point, tol: f32) -> bool {
        p.x >= self.x - tol
            && p.x <= self.right() + tol
            && p.y >= self.y - tol
            && p.y <= self.bottom() + tol
    }
    pub fn center(&self) -> Point {
        Point::new(self.x + self.w / 2.0, self.y + self.h / 2.0)
    }
}

/// 颜色（ARGB）
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color(pub u32);

impl Color {
    pub fn rgb(r: u8, g: u8, b: u8) -> Self {
        Color(0xFF00_0000 | ((r as u32) << 16) | ((g as u32) << 8) | b as u32)
    }
    pub fn argb(a: u8, r: u8, g: u8, b: u8) -> Self {
        Color(((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | b as u32)
    }
    pub fn with_alpha(self, a: u8) -> Self {
        Color((self.0 & 0x00FF_FFFF) | ((a as u32) << 24))
    }
    pub fn alpha(self) -> u8 {
        (self.0 >> 24) as u8
    }
    pub fn red(self) -> u8 {
        (self.0 >> 16) as u8
    }
    pub fn green(self) -> u8 {
        (self.0 >> 8) as u8
    }
    pub fn blue(self) -> u8 {
        self.0 as u8
    }
}

pub const COLOR_RED: Color = Color(0xFF_FF4D4F);
pub const COLOR_BLUE: Color = Color(0xFF_0A84FF);
pub const COLOR_WHITE: Color = Color(0xFF_FFFFFF);
pub const COLOR_BLACK: Color = Color(0xFF_000000);

// ---------- 工具类型 ----------
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    None,
    Pen,
    Highlighter,
    Rectangle,
    Ellipse,
    Spotlight,
    Arrow,
    Polyline,
    Wavy,
    Magnifier,
    Text,
    Watermark,
    Number,
    Mosaic,
    Eraser,
    Select,
}

// ---------- 图形枚举 ----------
#[derive(Clone, Debug)]
pub enum Shape {
    Freehand {
        points: Vec<Point>,
        color: Color,
        width: f32,
        alpha: u8, // 荧光笔用半透明
    },
    Rect {
        rect: Rect,
        color: Color,
        width: f32,
    },
    Ellipse {
        rect: Rect,
        color: Color,
        width: f32,
    },
    Arrow {
        p1: Point,
        p2: Point,
        color: Color,
        width: f32,
    },
    Polyline {
        points: Vec<Point>,
        color: Color,
        width: f32,
    },
    Wavy {
        p1: Point,
        p2: Point,
        color: Color,
        width: f32,
    },
    Spotlight {
        center: Point,
        radius: f32,
    },
    /// 放大镜：圆形取景框。画面内容来自「进屏幕标注时抓的整屏快照」，
    /// 由 canvas 全局保存一份，这里只记位置、半径和倍率。
    Magnifier {
        center: Point,
        radius: f32,
        /// 放大倍率（滚轮调，1.5~8）
        zoom: f32,
    },
    Text {
        rect: Rect,
        text: String,
        color: Color,
        font_size: f32,
    },
    Watermark {
        text: String,
        color: Color,
        /// 字号（跟随工具条滑块，见 canvas::sync_watermark）
        size: f32,
    },
    Number {
        pos: Point,
        number: i32,
        color: Color,
        radius: f32,
    },
    Mosaic {
        rect: Rect,
        pixmap: Option<Vec<u8>>,
        pixmap_w: i32,
        pixmap_h: i32,
    },
}

impl Shape {
    /// 是否可被橡皮擦删除（聚光灯/水印是整屏效果，不进橡皮擦范围）
    pub fn erasable(&self) -> bool {
        !matches!(self, Shape::Spotlight { .. } | Shape::Watermark { .. })
    }

    /// 是否可被「选择/移动」选中
    pub fn selectable(&self) -> bool {
        !matches!(self, Shape::Watermark { .. })
    }

    /// 外接矩形（用于命中测试、橡皮擦、选中框）
    pub fn bbox(&self) -> Rect {
        match self {
            Shape::Freehand { points, width, .. } => bbox_of_points(points, *width + 6.0),
            Shape::Rect { rect, width, .. } => pad_rect(*rect, *width + 4.0),
            Shape::Ellipse { rect, width, .. } => pad_rect(*rect, *width + 4.0),
            Shape::Arrow { p1, p2, width, .. } => bbox_of_points(&[*p1, *p2], *width + 14.0),
            Shape::Polyline { points, width, .. } => bbox_of_points(points, *width + 6.0),
            Shape::Wavy { p1, p2, width, .. } => bbox_of_points(&[*p1, *p2], *width + 14.0),
            Shape::Spotlight { center, radius } => Rect {
                x: center.x - radius,
                y: center.y - radius,
                w: radius * 2.0,
                h: radius * 2.0,
            },
            Shape::Magnifier { center, radius, .. } => Rect {
                x: center.x - radius,
                y: center.y - radius,
                w: radius * 2.0,
                h: radius * 2.0,
            },
            Shape::Text { rect, .. } => *rect,
            Shape::Watermark { .. } => Rect {
                x: 0.0,
                y: 0.0,
                w: 100000.0,
                h: 100000.0,
            },
            Shape::Number { pos, radius, .. } => Rect {
                x: pos.x - radius,
                y: pos.y - radius,
                w: radius * 2.0,
                h: radius * 2.0,
            },
            Shape::Mosaic { rect, .. } => *rect,
        }
    }

    /// 命中测试（点是否落在这个图形上）
    pub fn hit(&self, p: Point, tol: f32) -> bool {
        match self {
            Shape::Spotlight { center, radius } => {
                let dx = p.x - center.x;
                let dy = p.y - center.y;
                (dx * dx + dy * dy).sqrt() <= radius + tol
            }
            Shape::Magnifier { center, radius, .. } => {
                let dx = p.x - center.x;
                let dy = p.y - center.y;
                (dx * dx + dy * dy).sqrt() <= radius + tol
            }
            Shape::Number { pos, radius, .. } => {
                let dx = p.x - pos.x;
                let dy = p.y - pos.y;
                (dx * dx + dy * dy).sqrt() <= radius + tol
            }
            Shape::Freehand { points, width, .. } => {
                let d = (width / 2.0 + tol).max(tol);
                for w in points.windows(2) {
                    if dist_to_segment(p, w[0], w[1]) <= d {
                        return true;
                    }
                }
                false
            }
            _ => self.bbox().contains(p, tol),
        }
    }

    /// 平移
    pub fn translate(&mut self, dx: f32, dy: f32) {
        match self {
            Shape::Freehand { points, .. } => {
                for pt in points {
                    pt.x += dx;
                    pt.y += dy;
                }
            }
            Shape::Rect { rect, .. }
            | Shape::Ellipse { rect, .. }
            | Shape::Mosaic { rect, .. }
            | Shape::Text { rect, .. } => {
                rect.x += dx;
                rect.y += dy;
            }
            Shape::Arrow { p1, p2, .. } | Shape::Wavy { p1, p2, .. } => {
                p1.x += dx;
                p1.y += dy;
                p2.x += dx;
                p2.y += dy;
            }
            Shape::Polyline { points, .. } => {
                for pt in points {
                    pt.x += dx;
                    pt.y += dy;
                }
            }
            Shape::Spotlight { center, .. } | Shape::Magnifier { center, .. } => {
                center.x += dx;
                center.y += dy;
            }
            Shape::Number { pos, .. } => {
                pos.x += dx;
                pos.y += dy;
            }
            Shape::Watermark { .. } => {}
        }
    }

    /// 缩放（以锚点 o 为中心，sx/sy 为缩放比）
    pub fn scale_about(&mut self, o: Point, sx: f32, sy: f32) {
        let m = |v: Point| Point::new(o.x + (v.x - o.x) * sx, o.y + (v.y - o.y) * sy);
        match self {
            Shape::Freehand { points, width, .. } => {
                for pt in points {
                    *pt = m(*pt);
                }
                *width = (*width * ((sx + sy) / 2.0)).max(1.0);
            }
            Shape::Rect { rect, width, .. }
            | Shape::Ellipse { rect, width, .. } => {
                let tl = m(Point::new(rect.x, rect.y));
                let br = m(Point::new(rect.right(), rect.bottom()));
                *rect = Rect::from_points(tl, br);
                *width = (*width * ((sx + sy) / 2.0)).max(1.0);
            }
            Shape::Text {
                rect, font_size, ..
            } => {
                let tl = m(Point::new(rect.x, rect.y));
                let br = m(Point::new(rect.right(), rect.bottom()));
                *rect = Rect::from_points(tl, br);
                // 文字框被拉伸时字号也要跟着变，否则框变大了字还是那么小
                *font_size = (*font_size * ((sx + sy) / 2.0)).clamp(8.0, 400.0);
            }
            Shape::Arrow { p1, p2, width, .. } | Shape::Wavy { p1, p2, width, .. } => {
                *p1 = m(*p1);
                *p2 = m(*p2);
                *width = (*width * ((sx + sy) / 2.0)).max(1.0);
            }
            Shape::Polyline { points, width, .. } => {
                for pt in points {
                    *pt = m(*pt);
                }
                *width = (*width * ((sx + sy) / 2.0)).max(1.0);
            }
            Shape::Spotlight { center, radius } | Shape::Magnifier { center, radius, .. } => {
                *center = m(*center);
                *radius = (*radius * ((sx + sy) / 2.0)).max(1.0);
            }
            Shape::Number { pos, radius, .. } => {
                *pos = m(*pos);
                *radius = (*radius * ((sx + sy) / 2.0)).max(4.0);
            }
            Shape::Mosaic { rect, .. } => {
                let tl = m(Point::new(rect.x, rect.y));
                let br = m(Point::new(rect.right(), rect.bottom()));
                *rect = Rect::from_points(tl, br);
            }
            Shape::Watermark { .. } => {}
        }
    }
}

fn bbox_of_points(points: &[Point], pad: f32) -> Rect {
    if points.is_empty() {
        return Rect {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
        };
    }
    let mut min_x = f32::MAX;
    let mut min_y = f32::MAX;
    let mut max_x = f32::MIN;
    let mut max_y = f32::MIN;
    for p in points {
        min_x = min_x.min(p.x);
        min_y = min_y.min(p.y);
        max_x = max_x.max(p.x);
        max_y = max_y.max(p.y);
    }
    Rect {
        x: min_x - pad,
        y: min_y - pad,
        w: (max_x - min_x) + pad * 2.0,
        h: (max_y - min_y) + pad * 2.0,
    }
}

fn pad_rect(r: Rect, pad: f32) -> Rect {
    Rect {
        x: r.x - pad,
        y: r.y - pad,
        w: r.w + pad * 2.0,
        h: r.h + pad * 2.0,
    }
}

fn dist_to_segment(p: Point, a: Point, b: Point) -> f32 {
    let abx = b.x - a.x;
    let aby = b.y - a.y;
    let ab2 = abx * abx + aby * aby;
    if ab2 < 1e-6 {
        return ((p.x - a.x).powi(2) + (p.y - a.y).powi(2)).sqrt();
    }
    let t = (((p.x - a.x) * abx + (p.y - a.y) * aby) / ab2).clamp(0.0, 1.0);
    let px = a.x + abx * t;
    let py = a.y + aby * t;
    ((p.x - px).powi(2) + (p.y - py).powi(2)).sqrt()
}

// ---------- 全局状态 ----------
#[derive(Clone, Debug)]
pub struct AppState {
    pub tool: Tool,
    pub color: Color,
    pub width: f32,
    pub shapes: Vec<Shape>,
    pub number_counter: i32,
    pub watermark_text: String,
    pub text_editing: bool,
}

impl AppState {
    pub fn new() -> Self {
        AppState {
            tool: Tool::None,
            color: COLOR_RED,
            width: 4.0,
            shapes: Vec::new(),
            number_counter: 0,
            watermark_text: "WATERMARK".to_string(),
            text_editing: false,
        }
    }
}
