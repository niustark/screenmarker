// 浮动工具条：GDI 自绘的双行面板。
//
// 上行：14 个绘制工具（图标自绘，见 icons.rs）
// 下行：铅笔主图标 | 颜色色块×7 | 粗细预览 | 撤回 清空 选择
//
// 线宽不再用 +/- 按钮，改成鼠标滚轮调节（见 hooks.rs 的滚轮分支）。
//
// 交互：
//   点工具 → 切工具并显示画布（关键：之前漏了这一步，导致「点了画不出来」）
//   点主图标 → 收纳 / 展开（收纳后只剩铅笔，半透明）
//   拖空白处 → 移动工具条（靠 WM_NCHITTEST 返回 HTCAPTION）
//   右键 → 展开态：收回主图标；收纳态：直接退出程序
//   右侧 Ctrl → 全局展开 / 收纳（见 hooks.rs）

#![allow(dead_code)]

use crate::canvas;
use crate::icons;
use crate::shape::*;

use std::cell::RefCell;
use std::ptr;
use std::rc::Rc;
use std::sync::{Mutex, OnceLock};

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
};
use windows::Win32::UI::WindowsAndMessaging::*;

pub static mut TOOLBAR_STATE_PTR: usize = 0;
static mut TOOLBAR_CANVAS: usize = 0;
static mut TOOLBAR_HWND: usize = 0;
static mut EXPANDED: bool = false;
static mut SCALE: f32 = 1.0;
static mut WIN_W: i32 = 0;
static mut WIN_H: i32 = 0;

/// 首次启动「亮相」是否正在进行：此时窗口是展开尺寸，但状态仍是收纳态。
static mut PEEK: bool = false;
/// 亮相结束的定时器 id（只在这一处用，随意取个不冲突的小整数）
const HINT_TIMER_ID: usize = 0x51;
const HINT_HOLD_MS: u32 = 2200;
/// 亮相的淡入淡出：定时器逐帧改分层窗口的整体 alpha。
/// 收纳态本来就是 180，展开态是 255，所以淡入就是 180→255、淡出 255→180，
/// 两头都落在「正常值」上，动画结束不会有一下亮度跳变。
const FADE_TIMER_ID: usize = 0x52;
const FADE_STEP_MS: u32 = 16;
const FADE_STEP_ALPHA: i32 = 12;
static mut FADE_CUR: i32 = 180;
static mut FADE_TARGET: i32 = 180;
/// 淡出结束后要不要真的缩回收纳图标（亮相的退场靠这个衔接）
static mut FADE_COLLAPSE_AFTER: bool = false;

/// 亮相期间被点了一下：这一下的「抬起」要吃掉。
/// 因为展开是把主图标钉在原地，光标正好落在铅笔上，
/// 不吞掉的话抬起会被当成「点主图标」→ 刚展开又被收回，等于白点。
static mut PEEK_SWALLOW_UP: bool = false;

// 4 倍超采样位图缓存。WM_PAINT 每次都 CreateCompatibleBitmap(w*4,h*4)
// 会反复分配/释放好几 MB；窗口尺寸不变时（展开/收纳切换时才变）直接复用。
static mut SS_DC: isize = 0;
static mut SS_BMP: isize = 0;
static mut SS_OLD: isize = 0;
static mut SS_W: i32 = 0;
static mut SS_H: i32 = 0;
// 渲染内容签名：尺寸或「状态」变化时才会重画 paint_content，否则直接复用
// 已缓存的 4x 位图（鼠标停留在同一个按钮上时不重复光栅化）。
// 元组 = (展开?, tool 判别, 颜色, 线宽*100, 逻辑宽, 逻辑高, 悬停项索引)
static mut TB_SIG: (u8, u8, u32, i32, i32, i32, i32) = (0, 0, 0, 0, 0, 0, -1);

/// 当前鼠标悬停的按钮在 layout() 里的下标（-1 = 没有）。
/// 官网演示页里悬停会让按钮变红边 + 红图标，这里保持同样的反馈。
static mut HOVER_IDX: i32 = -1;

// ---------- 工具按钮悬停说明 ----------
// 与之前删掉的「收纳态气泡」不是一回事：这里是展开后逐个按钮讲功能，
// 延时 380ms 才出现，扫过按钮不会乱弹；移开/点击/收纳立刻消失。
static mut TIP_HWND: usize = 0;
static mut TIP_TEXT: String = String::new();
const TIP_TIMER_ID: usize = 0x53;
const TIP_DELAY_MS: u32 = 380;

/// Tool 枚举没有 repr(u8)，转一个稳定判别值用于渲染签名
fn tool_id(t: Tool) -> u8 {
    match t {
        Tool::None => 0,
        Tool::Pen => 1,
        Tool::Highlighter => 2,
        Tool::Rectangle => 3,
        Tool::Ellipse => 4,
        Tool::Spotlight => 5,
        Tool::Arrow => 6,
        Tool::Polyline => 7,
        Tool::Wavy => 8,
        Tool::Magnifier => 9,
        Tool::Text => 10,
        Tool::Watermark => 11,
        Tool::Number => 12,
        Tool::Mosaic => 13,
        Tool::Eraser => 14,
        Tool::Select => 15,
    }
}

unsafe fn ss_release() {
    if SS_DC != 0 {
        SelectObject(
            HDC(SS_DC as *mut core::ffi::c_void),
            HGDIOBJ(SS_OLD as *mut core::ffi::c_void),
        );
        DeleteObject(HGDIOBJ(SS_BMP as *mut core::ffi::c_void));
        DeleteDC(HDC(SS_DC as *mut core::ffi::c_void));
        SS_DC = 0;
        SS_BMP = 0;
        SS_OLD = 0;
        SS_W = 0;
        SS_H = 0;
    }
}

/// 取得（必要时重建）超采样用的内存 DC
unsafe fn ss_surface(hdc: HDC, w: i32, h: i32) -> HDC {
    if SS_DC == 0 || SS_W != w || SS_H != h {
        ss_release();
        let mdc = CreateCompatibleDC(hdc);
        let bmp = CreateCompatibleBitmap(hdc, w, h);
        let old = SelectObject(mdc, HGDIOBJ(bmp.0));
        SS_DC = mdc.0 as isize;
        SS_BMP = bmp.0 as isize;
        SS_OLD = old.0 as isize;
        SS_W = w;
        SS_H = h;
    }
    HDC(SS_DC as *mut core::ffi::c_void)
}
/// WM_MOUSELEAVE（0x02A3）：windows 0.58 把它藏在未启用的 UI::Controls feature 里，
/// 这里必须自定义；漏了的话 match 模式会退化为变量绑定（匹配一切），
/// 把 WM_NCCREATE 也吞掉返回 0，导致工具条窗口创建失败（句柄无效）。
const WM_MOUSELEAVE: u32 = 0x02A3;
/// 鼠标移到工具条「非按钮区」时收到的是 WM_NCMOUSEMOVE（0x00A0，
/// 因为 WM_NCHITTEST 把空隙判成了标题栏），此时要撤掉悬停高亮。
const WM_NCMOUSEMOVE_: u32 = 0x00A0;

const TOOLS: &[(Tool, &str)] = &[
    (Tool::Pen, "画笔"),
    (Tool::Highlighter, "荧光"),
    (Tool::Rectangle, "矩形"),
    (Tool::Ellipse, "圆"),
    (Tool::Text, "文字"),
    (Tool::Arrow, "箭头"),
    (Tool::Polyline, "折线"),
    (Tool::Wavy, "波浪"),
    (Tool::Magnifier, "放大"),
    (Tool::Spotlight, "聚光"),
    (Tool::Watermark, "水印"),
    (Tool::Number, "序号"),
    (Tool::Mosaic, "马赛克"),
];

const PALETTE: &[Color] = &[
    Color(0xFF_FF4D4F), // 红
    Color(0xFF_FF9F0A), // 橙
    Color(0xFF_FFD60A), // 黄
    Color(0xFF_34C759), // 绿
    Color(0xFF_0A84FF), // 蓝
    Color(0xFF_FFFFFF), // 白
    Color(0xFF_1C1C1E), // 黑
];

#[derive(Clone, Copy, PartialEq, Debug)]
enum Action {
    Tool(Tool),
    Main,
    ColorIdx(usize),
    /// 线宽预览（只是显示，点它没反应）
    WidthPreview,
    /// 粗细 / 字号滑块，可左右拖动
    Slider,
    /// 竖直分隔线（只用来分组，不响应点击）
    Separator,
    Undo,
    Clear,
    Select,
}

/// 悬停提示文案：<名称>｜<作用>。分隔线和当前已删掉的按钮不给提示。
fn tip_for_action(a: Action) -> Option<&'static str> {
    Some(match a {
        Action::Tool(t) => match t {
            Tool::Pen => "画笔｜自由手绘线条，按住拖动才生成，单击不落点",
            Tool::Highlighter => "荧光笔｜半透明划重点，按住拖动才生成",
            Tool::Rectangle => "矩形｜拖动画方框，按住 Shift 画正方形",
            Tool::Ellipse => "圆｜拖动画椭圆，按住 Shift 画正圆",
            Tool::Text => "文字｜点击输入，回车确认，可接着写下一处",
            Tool::Arrow => "箭头｜拖出带箭头的指向线",
            Tool::Polyline => "折线｜连续点击画多段线，双击或右键结束",
            Tool::Wavy => "波浪线｜拖出波浪下划线，适合圈注",
            Tool::Magnifier => "放大镜｜局部放大，滚轮调范围，Ctrl+滚轮调倍率",
            Tool::Spotlight => "聚光灯｜拖圆改位置，拉边缘调范围",
            Tool::Watermark => "水印｜点一下开 / 关，Ctrl+滚轮调字号",
            Tool::Number => "序号｜依次标 1、2、3…，删除后可回退",
            Tool::Mosaic => "马赛克｜点一下建框，拖边框调范围",
            Tool::Eraser => "橡皮擦｜按住拖动擦掉图元",
            _ => return None,
        },
        Action::Main => "主图标｜收起 / 展开工具条",
        Action::ColorIdx(_) => "颜色｜选画笔、文字、水印的颜色",
        Action::Slider => "线宽 / 字号｜左右拖动调整粗细",
        Action::WidthPreview => "线宽预览｜当前粗细与字号",
        Action::Undo => "撤销｜删掉最后画的一个图元",
        Action::Clear => "清空｜清掉屏幕上所有图元",
        Action::Select => "选择｜点选图元后可拖动、缩放",
        Action::Separator => return None,
    })
}

/// 滑块对应的线宽区间（同时决定字号：font_size = width * 4）
const W_MIN: f32 = 1.0;
const W_MAX: f32 = 40.0;

/// 正在拖动滑块（滑块要跨出按钮区域，必须靠 SetCapture 才能跟住鼠标）
static mut SLIDING: bool = false;

/// 收纳态：按在主图标上、可能是在拖它 / 也可能只是点一下展开
static mut ICON_DOWN: bool = false;
static mut ICON_DRAG: bool = false;
static mut ICON_P0: (i32, i32) = (0, 0);
static mut WIN_P0: (i32, i32) = (0, 0);

/// COLORREF 是 0x00BBGGRR，用这个辅助函数按 RGB 顺序写更不容易出错
fn c(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(((b as u32) << 16) | ((g as u32) << 8) | r as u32)
}

// 官网演示页（screenmarker.9niu.top）工具条 replica 的配色。
// 面板是极浅的竖向渐变，选中/悬停用同一个品牌红 #ff4d4f。
const PANEL_TOP: (u8, u8, u8) = (0xFB, 0xFC, 0xFE);
const PANEL_BOTTOM: (u8, u8, u8) = (0xEE, 0xF1, 0xF6);
const PANEL_LINE: (u8, u8, u8) = (0xE7, 0xEB, 0xF1);
const ACCENT: (u8, u8, u8) = (0xFF, 0x4D, 0x4F);

fn accent() -> COLORREF {
    c(ACCENT.0, ACCENT.1, ACCENT.2)
}

fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    ((a as f32 + (b as f32 - a as f32) * t).round()).clamp(0.0, 255.0) as u8
}

fn rect(x: i32, y: i32, w: i32, h: i32) -> RECT {
    RECT {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    }
}

/// 接近纯白的颜色：白底调参区里的预览线 / 滑块填充 / 文字预览会完全看不见，
/// 需要额外衬一层深灰描边才看得出（同时仍能看出「选的是白色」）。
fn is_pale(color: Color) -> bool {
    color.red() >= 235 && color.green() >= 235 && color.blue() >= 235
}

/// 浅色内容在白底上的衬底色
fn ink_shadow() -> COLORREF {
    c(0x8A, 0x90, 0x98)
}

fn hit(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

/// 整体视觉缩放：1.0 = 按 96dpi 原样，0.8 = 整体缩小 20%。
/// 只乘在 SCALE 上，按钮、内边距、色块、图标、圆角、滑块会一起等比缩，
/// 比一个个改 BTN/PAD/... 更不容易漏。
const UI_TUNE: f32 = 0.8;

/// 尺寸基准（96dpi 逻辑像素），layout() 和 paint_content() 共用，
/// 免得改了一处忘了另一处，分隔线/图标就错位
const BTN: f32 = 40.0;   // 方形按钮边长
const PAD: f32 = 6.0;    // 面板内边距
const ROW_GAP: f32 = 4.0; // 两行间距
const GAP: f32 = 3.0;    // 按钮间距
const SW: f32 = 24.0;    // 色块边长
const ICON_INSET: f32 = 6.0; // 图标距按钮边的留白（越大图标越小）

/// 计算布局：返回 (窗口宽, 窗口高, 动作区域列表)
fn layout(s: f32) -> (i32, i32, Vec<(RECT, Action)>) {
    // 工具按钮做成正方形（图标外圈是方的）
    let b = (BTN * s) as i32;
    let b_w = b;
    let b_h = b;
    let pad = (PAD * s) as i32;
    let gap = (GAP * s) as i32;
    let row_gap = (ROW_GAP * s) as i32;
    let sw = (SW * s) as i32;
    let a_w = b;

    let mut items: Vec<(RECT, Action)> = Vec::new();

    // 上行：工具
    for (i, (t, _)) in TOOLS.iter().enumerate() {
        let x = pad + (i as i32) * (b_w + gap);
        items.push((rect(x, pad, b_w, b_h), Action::Tool(*t)));
    }
    let row1_end = pad + (TOOLS.len() as i32) * b_w + (TOOLS.len() as i32 - 1) * gap;

    // 下行
    let y2 = pad + b_h + row_gap;
    let mut x = pad;

    // 主图标
    items.push((rect(x, y2, b_w, b_h), Action::Main));
    x += b_w + gap * 2;

    // 颜色色块
    let sw_y = y2 + (b_h - sw) / 2;
    for i in 0..PALETTE.len() {
        items.push((rect(x, sw_y, sw, sw), Action::ColorIdx(i)));
        x += sw + gap;
    }
    x += gap * 2;

    // 撤回 / 擦除 / 清空 三个按钮锚定到上排最右侧三个工具按钮（第 11/12/13 个），
    // 与上排垂直对齐；分隔线紧贴撤回按钮左侧。这样先算出分隔线的位置，
    // 再反推滑块与线宽预览的宽度，让「调参区」右端正好贴满分隔线左边，不留空白。
    let undo_x = pad + ((TOOLS.len() - 3) as i32) * (b_w + gap);
    let sep_x = undo_x - gap * 2;

    // 竖直分隔线：把「调参区」和「操作按钮」分开
    let sep_h = (b_h as f32 * 0.62) as i32;
    items.push((
        rect(sep_x, y2 + (b_h - sep_h) / 2, gap.max(1), sep_h),
        Action::Separator,
    ));

    let mut row2_end = x;

    // 滑块：右端贴到分隔线左侧，保持固定宽度
    let sl_w = (96.0 * s) as i32;
    let slider_x = sep_x - gap - sl_w;
    items.push((rect(slider_x, sw_y, sl_w, sw), Action::Slider));

    // 线宽预览：占据「色块之后 → 滑块左端」的剩余空间，把空白填满
    let preview_x = x;
    let preview_w = (slider_x - gap - x).max(a_w);
    items.push((rect(preview_x, sw_y, preview_w, sw), Action::WidthPreview));

    for (k, act) in [Action::Undo, Action::Tool(Tool::Eraser), Action::Clear]
        .iter()
        .enumerate()
    {
        // 上排第 11、12、13 个按钮的索引是 10、11、12
        let col = TOOLS.len() - 3 + k;
        let bx = pad + (col as i32) * (b_w + gap);
        items.push((rect(bx, y2, a_w, b_h), *act));
        row2_end = row2_end.max(bx + a_w);
    }

    let w = row1_end.max(row2_end) + pad;
    let h = pad * 2 + b_h * 2 + row_gap;
    (w, h, items)
}

/// layout() 的按缩放比缓存。
///
/// s 实际只有两个值：命中测试/尺寸用 SCALE，绘制用 SCALE*4（超采样）。
/// 但 WM_NCHITTEST、MOUSEMOVE、PAINT 等 7 处都会调 layout()，每次重建
/// 一份约 27 个矩形的 Vec。这里按 s 缓存进程级条目，命中与绘制共用。
/// 条目用 Box::leak 常驻（最多几个槽，随进程退出回收）。
struct LayoutEntry {
    key: u32,
    w: i32,
    h: i32,
    items: Vec<(RECT, Action)>,
}
static LAYOUT_CACHE: OnceLock<Mutex<Vec<&'static LayoutEntry>>> = OnceLock::new();

fn layout_cached(s: f32) -> &'static LayoutEntry {
    let key = s.to_bits();
    let mut slots = LAYOUT_CACHE
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(e) = slots.iter().find(|e| e.key == key) {
        return *e;
    }
    let (w, h, items) = layout(s);
    let e: &'static LayoutEntry = Box::leak(Box::new(LayoutEntry { key, w, h, items }));
    slots.push(e);
    e
}

unsafe fn expanded_size() -> (i32, i32) {
    let e = layout_cached(SCALE);
    (e.w, e.h)
}

unsafe fn collapsed_size() -> i32 {
    ((BTN + PAD * 2.0) * SCALE) as i32
}

// ---------- 绘制 ----------

/// PixPin 风格的圆角小瓷砖：填充 + 描边，圆角随缩放。
/// 在 4 倍超采样位图上作画，线宽/圆角都乘 s。
unsafe fn round_tile(
    hdc: HDC,
    r: RECT,
    fill: COLORREF,
    border: COLORREF,
    border_w: f32,
    radius: f32,
    s: f32,
) {
    let pen = CreatePen(PS_SOLID, ((border_w * s) as i32).max(1), border);
    let brush = CreateSolidBrush(fill);
    let op = SelectObject(hdc, HGDIOBJ(pen.0));
    let ob = SelectObject(hdc, HGDIOBJ(brush.0));
    let rad = ((radius * s) as i32).max(2);
    RoundRect(hdc, r.left, r.top, r.right + 1, r.bottom + 1, rad, rad);
    SelectObject(hdc, ob);
    SelectObject(hdc, op);
    DeleteObject(HGDIOBJ(pen.0));
    DeleteObject(HGDIOBJ(brush.0));
}

/// 只描一圈圆角边框、不填充内部（面板用了渐变，不能再被实心圆角矩形盖掉）。
unsafe fn round_outline(hdc: HDC, r: RECT, border: COLORREF, border_w: f32, radius: f32, s: f32) {
    let pen = CreatePen(PS_SOLID, ((border_w * s) as i32).max(1), border);
    let op = SelectObject(hdc, HGDIOBJ(pen.0));
    // NULL_BRUSH 让 RoundRect 只画笔、不填色
    let ob = SelectObject(hdc, GetStockObject(NULL_BRUSH));
    let rad = ((radius * s) as i32).max(2);
    RoundRect(hdc, r.left, r.top, r.right + 1, r.bottom + 1, rad, rad);
    SelectObject(hdc, ob);
    SelectObject(hdc, op);
    DeleteObject(HGDIOBJ(pen.0));
}

/// 面板背景：官网同款 #fbfcfe → #eef1f6 竖向渐变。
/// GDI 没有跨设备的渐变画笔，用若干条纯色横带拼接；因为是画在 4x 超采样
/// 位图上再缩回来的，色带边界会被平均掉，看不出台阶。
unsafe fn panel_gradient(hdc: HDC, rc: RECT, s: f32) {
    let total = (rc.bottom - rc.top).max(1);
    let bands = ((total as f32 / (4.0 * s)).ceil() as i32).clamp(12, 64);
    for i in 0..bands {
        let t = (i as f32 + 0.5) / bands as f32;
        let col = c(
            lerp_u8(PANEL_TOP.0, PANEL_BOTTOM.0, t),
            lerp_u8(PANEL_TOP.1, PANEL_BOTTOM.1, t),
            lerp_u8(PANEL_TOP.2, PANEL_BOTTOM.2, t),
        );
        let ya = rc.top + total * i / bands;
        let yb = rc.top + total * (i + 1) / bands;
        let br = CreateSolidBrush(col);
        FillRect(hdc, &rect(rc.left, ya, rc.right - rc.left, (yb - ya).max(1)), br);
        DeleteObject(HGDIOBJ(br.0));
    }
}

/// 按钮瓷砖（对齐官网 replica）：
///   普通  = 白底 + 浅灰细边
///   悬停  = 白底 + 红边（图标也转红）
///   选中  = 实心红底 + 同色红边 + 下方一层淡红投影，图标改白色
unsafe fn button_bg(hdc: HDC, r: RECT, active: bool, hovered: bool, s_: f32) {
    let red = accent();
    if active {
        let off = (2.0 * s_) as i32;
        let sh = rect(r.left, r.top + off, r.right - r.left, r.bottom - r.top);
        round_tile(hdc, sh, c(0xFF, 0xDD, 0xDE), c(0xFF, 0xDD, 0xDE), 1.0, 8.0, s_);
        round_tile(hdc, r, red, red, 1.0, 8.0, s_);
    } else if hovered {
        round_tile(hdc, r, c(0xFF, 0xFF, 0xFF), red, 1.5, 8.0, s_);
    } else {
        round_tile(hdc, r, c(0xFF, 0xFF, 0xFF), c(0xD9, 0xDD, 0xE2), 1.0, 8.0, s_);
    }
}

/// 图标在该按钮里占的正方形区域
fn icon_rect(r: RECT, s: f32) -> RECT {
    let inset = (ICON_INSET * s) as i32;
    rect(
        r.left + inset,
        r.top + inset,
        (r.right - r.left) - inset * 2,
        (r.bottom - r.top) - inset * 2,
    )
}

/// 线宽预览：一条当前粗细、当前颜色的线（滚轮改粗细后立刻能看到效果）。
/// PixPin 风格：不铺灰底，就是一条线，干干净净。
unsafe fn draw_width_preview(hdc: HDC, r: RECT, width: f32, color: Color, s: f32) {
    let cy = (r.top + r.bottom) / 2;
    // 线横跨整个预览格，左右不留空白（用户要求占满）
    let pad = (2.0 * s) as i32;
    // 预览格高度有限，直接按真实线宽画到 40 会撑成"药丸"，
    // 这里压到 0.55 倍：仍然单调地反映粗细，又不会顶满格子
    let max_pw = (r.bottom - r.top - (6.0 * s) as i32).max(1);
    let pw = ((width * s * 0.55) as i32).clamp(1, max_pw);

    // 白色（近白）线在白瓷砖上会完全消失：先画一条稍粗的灰衬底线，
    // 再把真实颜色的线压在上层，白线就有了深色轮廓，仍能看出选的是白色。
    if is_pale(color) {
        let halo = CreatePen(
            PS_SOLID,
            pw + ((2.0 * s) as i32).max(2),
            ink_shadow(),
        );
        let oh = SelectObject(hdc, HGDIOBJ(halo.0));
        MoveToEx(hdc, r.left + pad, cy, None);
        LineTo(hdc, r.right - pad, cy);
        SelectObject(hdc, oh);
        DeleteObject(HGDIOBJ(halo.0));
    }

    let pen = CreatePen(PS_SOLID, pw, c(color.red(), color.green(), color.blue()));
    let old = SelectObject(hdc, HGDIOBJ(pen.0));
    MoveToEx(hdc, r.left + pad, cy, None);
    LineTo(hdc, r.right - pad, cy);
    SelectObject(hdc, old);
    DeleteObject(HGDIOBJ(pen.0));
}

/// 文字工具下的预览格：显示一个「A」，字号跟着滑块走，
/// 这样拖动滑块能直接看出字号变化（不再只看到一条粗细线，看不出跟文字的关系）
unsafe fn draw_text_preview(hdc: HDC, r: RECT, font_size: f32, color: Color, s: f32) {
    let cell = (r.bottom - r.top) as f32;
    let t = ((font_size - 16.0) / (160.0 - 16.0)).clamp(0.0, 1.0);
    let px = (cell * (0.40 + 0.52 * t)).max(9.0 * s);
    let font = CreateFontW(
        -(px as i32).max(8),
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
        w!("Microsoft YaHei"),
    );
    let old_font = SelectObject(hdc, HGDIOBJ(font.0));
    SetBkMode(hdc, TRANSPARENT);
    let mut wide: Vec<u16> = "A".encode_utf16().collect();
    wide.push(0);
    let flags = DT_CENTER | DT_VCENTER | DT_SINGLELINE;
    // 白色「A」在白底上看不见：先偏移一点画一层灰影，再叠上真实颜色
    if is_pale(color) {
        let d = (1.4 * s) as i32;
        let mut shade = rect(r.left + d, r.top + d, r.right - r.left, r.bottom - r.top);
        SetTextColor(hdc, ink_shadow());
        DrawTextW(hdc, &mut wide, &mut shade, flags);
    }
    SetTextColor(hdc, c(color.red(), color.green(), color.blue()));
    let mut rr = r;
    DrawTextW(hdc, &mut wide, &mut rr, flags);
    SelectObject(hdc, old_font);
    DeleteObject(HGDIOBJ(font.0));
}

/// 滑块轨道的左右端点（避开两端留白）。
/// pad 取圆点半径即可：圆点直径约 13*s，在轨道两端时只需让出一半多一点，
/// 不必像以前那样让出半个控件高度（那会在轨道两端留下大段空白）。
fn slider_span(r: RECT, s: f32) -> (i32, i32) {
    let pad = (7.0 * s) as i32;
    (r.left + pad, r.right - pad)
}

/// 粗细 / 字号滑块：左侧填充到当前值（用当前颜色），右端一个白色圆点。
/// PixPin 风格：不铺底，只有一条细轨道。
unsafe fn draw_slider(hdc: HDC, r: RECT, width: f32, color: Color, s: f32) {
    let (x0, x1) = slider_span(r, s);
    let cy = (r.top + r.bottom) / 2;
    let th = ((4.0 * s) as i32).max(2);
    let t = ((width - W_MIN) / (W_MAX - W_MIN)).clamp(0.0, 1.0);
    let kx = x0 + (((x1 - x0) as f32) * t) as i32;
    let col = c(color.red(), color.green(), color.blue());
    let pale = is_pale(color);

    // 白色填充段在白瓷砖上会消失：先给整条轨道铺一层稍厚的灰衬底，
    // 白色段压在浅灰轨道上就能分辨出来。
    if pale {
        let halo = CreateSolidBrush(c(0xA8, 0xAE, 0xB6));
        let d = ((1.0 * s) as i32).max(1);
        FillRect(
            hdc,
            &rect(x0 - d, cy - th / 2 - d, (x1 - x0).max(1) + d * 2, th + d * 2),
            halo,
        );
        DeleteObject(HGDIOBJ(halo.0));
    }

    // 轨道底色
    let track = CreateSolidBrush(c(0xDC, 0xDF, 0xE4));
    FillRect(hdc, &rect(x0, cy - th / 2, (x1 - x0).max(1), th), track);
    DeleteObject(HGDIOBJ(track.0));

    // 已选中部分：当前颜色
    if kx > x0 {
        let fg = CreateSolidBrush(col);
        FillRect(hdc, &rect(x0, cy - th / 2, kx - x0, th), fg);
        DeleteObject(HGDIOBJ(fg.0));
    }

    // 圆点：白底 + 当前颜色描边（白色时改用灰描边，否则圆点会消失）
    let knob = ((13.0 * s) as i32).max(8);
    let white = CreateSolidBrush(c(0xFF, 0xFF, 0xFF));
    let ring = if pale { ink_shadow() } else { col };
    let pen = CreatePen(PS_SOLID, ((2.0 * s) as i32).max(1), ring);
    let ob = SelectObject(hdc, HGDIOBJ(white.0));
    let op = SelectObject(hdc, HGDIOBJ(pen.0));
    Ellipse(hdc, kx - knob / 2, cy - knob / 2, kx + knob / 2, cy + knob / 2);
    SelectObject(hdc, op);
    SelectObject(hdc, ob);
    DeleteObject(HGDIOBJ(white.0));
    DeleteObject(HGDIOBJ(pen.0));
}

/// 把鼠标 x 映射成线宽，并立刻重绘画布与工具条
unsafe fn slider_apply(hwnd: HWND, r: RECT, x: i32, state_ptr: *const RefCell<AppState>) {
    if state_ptr.is_null() {
        return;
    }
    let (x0, x1) = slider_span(r, SCALE);
    let span = (x1 - x0).max(1);
    let t = ((x - x0) as f32 / span as f32).clamp(0.0, 1.0);
    {
        let mut st = (*state_ptr).borrow_mut();
        st.width = (W_MIN + t * (W_MAX - W_MIN)).clamp(W_MIN, W_MAX);
    }
    canvas::sync_watermark();
    canvas::sync_text();
    canvas::sync_number();
    canvas::request_repaint_global();
    InvalidateRect(hwnd, None, true);
}

unsafe fn draw_swatch(hdc: HDC, r: RECT, color: Color, selected: bool, s: f32) {
    // 官网同款：选中时在色块外围套一圈 2px 红环，色块本身留 2px 白边
    // 把红环和填充色隔开（不然「选中红色」会红成一片，看不出来）。
    let fill = COLORREF(
        ((color.blue() as u32) << 16) | ((color.green() as u32) << 8) | color.red() as u32,
    );
    if selected {
        let g = (2.0 * s) as i32;
        let ring = rect(
            r.left - g,
            r.top - g,
            (r.right - r.left) + 2 * g,
            (r.bottom - r.top) + 2 * g,
        );
        let red = accent();
        round_tile(hdc, ring, red, red, 1.0, 8.0, s);
        round_tile(hdc, r, fill, c(0xFF, 0xFF, 0xFF), 2.0, 6.0, s);
    } else {
        round_tile(hdc, r, fill, c(0xB7, 0xBC, 0xC3), 1.0, 6.0, s);
    }
}

/// 画一支斜放的红底铅笔当主图标
unsafe fn draw_pencil(hdc: HDC, r: RECT, s: f32) {
    let cx = (r.left + r.right) as f32 / 2.0;
    let cy = (r.top + r.bottom) as f32 / 2.0;
    let u = ((r.right - r.left) as f32 * 0.62).max(14.0);

    // 45° 方向向量（朝右上方）
    let ang = -std::f32::consts::FRAC_PI_4;
    let (dx, dy) = (ang.cos(), ang.sin());
    let (px, py) = (-dy, dx);
    let len = u;
    let half = u * 0.19;
    let tip = u * 0.42;
    // 笔尖是往 -len/2 方向多伸出去一截的（tip），整个图形的视觉中心因此
    // 沿轴偏了 -tip/2。这里补回 +tip/2，图标才真正落在按钮正中；
    // 否则收纳态（图标更大）和展开态（图标更小）的画法会让它上下"跳"。
    let cx = cx + dx * (tip * 0.5);
    let cy = cy + dy * (tip * 0.5);

    let at = |a: f32, b: f32| POINT {
        x: (cx + dx * a + px * b) as i32,
        y: (cy + dy * a + py * b) as i32,
    };

    // 笔身
    let body = [
        at(-len / 2.0, -half),
        at(len / 2.0 - tip, -half),
        at(len / 2.0 - tip, half),
        at(-len / 2.0, half),
    ];
    // 红色笔身
    let red = CreateSolidBrush(c(0xFF, 0x4D, 0x4F));
    let old = SelectObject(hdc, HGDIOBJ(red.0));
    let op = SelectObject(hdc, GetStockObject(NULL_PEN));
    Polygon(hdc, &body);
    SelectObject(hdc, op);

    // 深色笔尖
    let tip_tri = [
        at(-len / 2.0, -half),
        at(-len / 2.0, half),
        at(-len / 2.0 - tip, 0.0),
    ];
    let dark = CreateSolidBrush(c(0x2B, 0x2B, 0x2B));
    SelectObject(hdc, HGDIOBJ(dark.0));
    Polygon(hdc, &tip_tri);

    // 浅色橡皮头
    let cap = [
        at(len / 2.0 - tip, -half),
        at(len / 2.0, -half),
        at(len / 2.0, half),
        at(len / 2.0 - tip, half),
    ];
    let light = CreateSolidBrush(c(0xF2, 0xC4, 0xC8));
    SelectObject(hdc, HGDIOBJ(light.0));
    Polygon(hdc, &cap);

    SelectObject(hdc, old);
    DeleteObject(HGDIOBJ(red.0));
    DeleteObject(HGDIOBJ(dark.0));
    DeleteObject(HGDIOBJ(light.0));
    let _ = s;
}

/// 按给定缩放比把整个工具条画到 hdc 上（4 倍超采样时 s = SCALE * 4）
unsafe fn paint_content(
    hdc: HDC,
    rc: RECT,
    s: f32,
    state_ptr: *const RefCell<AppState>,
) {
    // 展开态用官网同款浅渐变面板；收纳态仍是原来的白底小方块，
    // 免得半透明铅笔图标背后突然多出一块渐变色。
    if EXPANDED {
        panel_gradient(hdc, rc, s);
    } else {
        let bg = CreateSolidBrush(c(0xFF, 0xFF, 0xFF));
        FillRect(hdc, &rc, bg);
        DeleteObject(HGDIOBJ(bg.0));
    }

    // 面板整体一圈细边框：浅色背景下让工具条有个清晰的外轮廓
    let border_r = rect(
        rc.left + (s as i32),
        rc.top + (s as i32),
        (rc.right - rc.left) - (s as i32) * 2,
        (rc.bottom - rc.top) - (s as i32) * 2,
    );
    if EXPANDED {
        round_outline(hdc, border_r, c(PANEL_LINE.0, PANEL_LINE.1, PANEL_LINE.2), 1.0, 14.0, s);
    } else {
        round_tile(hdc, border_r, c(0xFF, 0xFF, 0xFF), c(0xDD, 0xE0, 0xE5), 1.0, 14.0, s);
    }

    if !EXPANDED {
        // 收纳态：把主图标画在一个「和展开态按钮同样大」的居中方块里。
        // 之前是直接铺满整个收纳窗口，图标比展开态大一圈，切换时看着像往下跳。
        let side = (BTN * s) as i32;
        let w = rc.right - rc.left;
        let h = rc.bottom - rc.top;
        let side = side.min(w).min(h);
        let r = rect((w - side) / 2, (h - side) / 2, side, side);
        draw_pencil(hdc, r, s);
        return;
    }

    let items = &layout_cached(s).items;
    let st = if state_ptr.is_null() {
        None
    } else {
        Some((*state_ptr).borrow())
    };
    let tool = st.as_ref().map(|x| x.tool).unwrap_or(Tool::None);
    let color = st.as_ref().map(|x| x.color).unwrap_or(PALETTE[0]);
    let width = st.as_ref().map(|x| x.width).unwrap_or(4.0);
    drop(st);

    for (idx, &(r, act)) in items.iter().enumerate() {
        let hov = HOVER_IDX == idx as i32 && EXPANDED;
        match act {
            Action::Tool(t) => {
                let on = t == tool;
                button_bg(hdc, r, on, hov, s);
                // 红底上必须换白图标；悬停（未选中）则把深灰图标转成品牌红。
                let ink = if on {
                    icons::ACTIVE_ON
                } else if hov {
                    icons::ACTIVE
                } else {
                    icons::NORMAL
                };
                icons::draw_tool_icon(hdc, icon_rect(r, s), t, ink);
            }
            Action::Main => draw_pencil(hdc, r, s),
            Action::ColorIdx(i) => draw_swatch(hdc, r, PALETTE[i], PALETTE[i] == color, s),
            Action::WidthPreview => {
                if tool == Tool::Text {
                    draw_text_preview(hdc, r, canvas::text_size(width), color, s)
                } else {
                    draw_width_preview(hdc, r, width, color, s)
                }
            }
            Action::Slider => draw_slider(hdc, r, width, color, s),
            Action::Separator => {
                let pen = CreatePen(PS_SOLID, ((1.0 * s) as i32).max(1), c(0xDC, 0xDF, 0xE4));
                let old = SelectObject(hdc, HGDIOBJ(pen.0));
                let cx = (r.left + r.right) / 2;
                MoveToEx(hdc, cx, r.top, None);
                LineTo(hdc, cx, r.bottom);
                SelectObject(hdc, old);
                DeleteObject(HGDIOBJ(pen.0));
            }
            Action::Undo => {
                button_bg(hdc, r, false, hov, s);
                icons::draw_undo(
                    hdc,
                    icon_rect(r, s),
                    if hov { icons::ACTIVE } else { icons::NORMAL },
                );
            }
            Action::Clear => {
                button_bg(hdc, r, false, hov, s);
                icons::draw_clear(
                    hdc,
                    icon_rect(r, s),
                    if hov { icons::ACTIVE } else { icons::NORMAL },
                );
            }
            Action::Select => {
                // 按钮已删除，这里不再会有实例（枚举保留以兼容旧状态）
            }
        }
    }
}

/// 给工具条套一个圆角窗口区域（不套的话四角是老式的直角）
unsafe fn apply_round_region(hwnd: HWND, w: i32, h: i32) {
    // 弧度对齐 WorkBuddy 那种面板圆角：视觉半径约 14px(96dpi)，
    // 但不能超过短边的一半，否则 Rgn 会退化成椭圆
    let mut r = ((14.0 * SCALE) as i32).max(6);
    r = r.min(w.min(h) / 2);
    let rgn = CreateRoundRectRgn(0, 0, w + 1, h + 1, r, r);
    if rgn.is_invalid() {
        return;
    }
    // 设置成功后该 region 归系统所有，不要再 DeleteObject
    let _ = SetWindowRgn(hwnd, rgn, true);
}

// ---------- 工具按钮悬停说明 ----------
//
// 与收纳态那个已删除的气泡不同：这里是展开后逐个按钮讲功能。
// 提示窗口带 WS_EX_TRANSPARENT，鼠标事件全部穿透给下面的工具条，
// 不会挡住点击、也不会抢焦点；380ms 延时避免光标扫过时乱弹。

unsafe extern "system" fn tip_wndproc(
    hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut rc = RECT::default();
            GetClientRect(hwnd, &mut rc);

            let bg = CreateSolidBrush(c(0x24, 0x24, 0x27));
            FillRect(hdc, &rc, bg);
            DeleteObject(HGDIOBJ(bg.0));

            let fs = ((12.0 * SCALE) as i32).max(11);
            let font = CreateFontW(
                -fs, 0, 0, 0, 400, 0, 0, 0,
                DEFAULT_CHARSET.0 as u32, 0, 0, 0, 0,
                w!("Microsoft YaHei"),
            );
            let old_font = SelectObject(hdc, HGDIOBJ(font.0));
            SetBkMode(hdc, TRANSPARENT);
            SetTextColor(hdc, c(0xFF, 0xFF, 0xFF));
            let mut wide: Vec<u16> = TIP_TEXT.encode_utf16().collect();
            wide.push(0);
            let pad = (10.0 * SCALE) as i32;
            let mut rr = rc;
            rr.left += pad;
            rr.right -= pad;
            rr.top += (5.0 * SCALE) as i32;
            rr.bottom -= (5.0 * SCALE) as i32;
            DrawTextW(hdc, &mut wide, &mut rr, DT_CENTER | DT_VCENTER | DT_WORDBREAK);
            SelectObject(hdc, old_font);
            DeleteObject(HGDIOBJ(font.0));

            EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// 显示（或更新）提示。位置跟着光标走，右侧 / 下方空间不够就翻到另一侧。
unsafe fn show_tool_tip(text: &str, cursor: POINT) {
    if TIP_TEXT != text {
        TIP_TEXT.clear();
        TIP_TEXT.push_str(text);
    }
    let fs = (12.0 * SCALE).max(11.0);
    // 中文按一字一宽、ASCII 按半宽估算，够算出气泡尺寸；DrawTextW 会再做精确折行
    let units: f32 = TIP_TEXT.chars().map(|ch| if ch.is_ascii() { 0.58 } else { 1.0 }).sum();
    let text_w = units * fs;
    let w = (text_w + 26.0 * SCALE)
        .clamp(140.0 * SCALE, 360.0 * SCALE) as i32;
    let two_lines = text_w > 300.0 * SCALE;
    let h = ((if two_lines { 46.0 } else { 30.0 }) * SCALE) as i32;

    let sx = GetSystemMetrics(SM_CXSCREEN);
    let sy = GetSystemMetrics(SM_CYSCREEN);
    let mut x = cursor.x + (18.0 * SCALE) as i32;
    let mut y = cursor.y + (22.0 * SCALE) as i32;
    if x + w > sx {
        x = cursor.x - w - (12.0 * SCALE) as i32;
    }
    if y + h > sy {
        y = cursor.y - h - (12.0 * SCALE) as i32;
    }
    x = x.max(0);
    y = y.max(0);

    if TIP_HWND != 0 {
        let hwnd = HWND(TIP_HWND as *mut _);
        SetWindowPos(hwnd, HWND_TOPMOST, x, y, w, h, SWP_NOACTIVATE);
        apply_round_region(hwnd, w, h);
        InvalidateRect(hwnd, None, true);
        return;
    }

    let hinst: HINSTANCE = GetModuleHandleW(None).unwrap_or_default().into();
    let class_name = w!("ScreenMarkerToolTip");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(tip_wndproc),
        hInstance: hinst,
        hbrBackground: HBRUSH(ptr::null_mut()),
        lpszClassName: class_name,
        ..Default::default()
    };
    RegisterClassW(&wc);
    let hwnd = match CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
        class_name,
        w!("ScreenMarkerToolTip"),
        WS_POPUP,
        x, y, w, h,
        None, None, hinst, None,
    ) {
        Ok(h) => h,
        Err(_) => return,
    };
    TIP_HWND = hwnd.0 as usize;
    SetLayeredWindowAttributes(hwnd, COLORREF(0), 235, LWA_ALPHA);
    apply_round_region(hwnd, w, h);
    ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    UpdateWindow(hwnd);
}

/// 隐藏提示；连定时器一起撤掉，否则光标移开后还会补弹一次。
unsafe fn hide_tool_tip(hwnd: HWND) {
    let _ = KillTimer(hwnd, TIP_TIMER_ID);
    if TIP_HWND == 0 {
        return;
    }
    let tip = HWND(TIP_HWND as *mut _);
    TIP_HWND = 0;
    let _ = DestroyWindow(tip);
}

// ---------- 首次启动的一次性「亮相」 ----------
//
// 收纳态的悬停气泡删掉之后，新用户第一次只看到一个小图标，未必知道能点。
// 与其加文字提示，不如让它自己展示一次：首次启动时展开成完整面板停两秒再缩回去。
// 相比气泡的好处是「用形态本身说话」——不用读字就知道这是个工具条、在哪儿、长什么样。

/// 首次运行的标记文件。故意放在 `%LOCALAPPDATA%\ScreenMarker\` 而不是注册表：
///   - exe 是免安装的，注册表里多一个键就多一处残留；
///   - 用户想再看一次时，直接删掉这个文件就行，不用教他改注册表。
fn hint_marker_path() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(
        std::path::PathBuf::from(base)
            .join("ScreenMarker")
            .join("first-run.seen"),
    )
}

fn hint_already_shown() -> bool {
    matches!(hint_marker_path(), Some(p) if p.exists())
}

fn mark_hint_shown() {
    if let Some(p) = hint_marker_path() {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&p, b"1");
    }
}

/// 展开态里主图标（铅笔）的那一格矩形，用来对齐锚点、避免亮相时图标乱跳。
unsafe fn main_icon_rect() -> RECT {
    layout_cached(SCALE)
        .items
        .iter()
        .find(|(_, a)| *a == Action::Main)
        .map(|(r, _)| *r)
        .unwrap_or(rect(0, 0, (BTN * SCALE) as i32, (BTN * SCALE) as i32))
}

/// 开始亮相：只改窗口尺寸与绘制，**不碰画布、工具、穿透态**。
///
/// 为什么不去复用 toggle_expanded？因为那条路是真展开 —— 会顺手铺开全屏画布、
/// 关掉穿透态、把工具切成荧光笔、光标变荧光笔。为了「让人看一眼」而制造这些
/// 副作用，用户屏幕上会闪一层透明画布，反而是打扰。这里只做个视觉上的展开。
unsafe fn start_first_run_peek(hwnd: HWND) {
    if hint_already_shown() {
        return;
    }
    mark_hint_shown(); // 先记上，免得中途崩溃导致每次启动都来一遍

    PEEK = true;
    EXPANDED = true;
    HOVER_IDX = -1;
    hide_tool_tip(hwnd);

    let (w, h) = expanded_size();
    let main_r = main_icon_rect();
    // 收纳态窗口只有 52×52、图标画在正中，所以窗口中心就是图标中心
    let mut wr = RECT::default();
    GetWindowRect(hwnd, &mut wr);
    let cx = (wr.left + wr.right) / 2;
    let cy = (wr.top + wr.bottom) / 2;
    let x = cx - main_r.left - (main_r.right - main_r.left) / 2;
    let y = cy - main_r.top - (main_r.bottom - main_r.top) / 2;

    SetWindowPos(hwnd, HWND_TOPMOST, x, y, w, h, SWP_SHOWWINDOW);
    apply_round_region(hwnd, w, h);
    WIN_W = w;
    WIN_H = h;
    InvalidateRect(hwnd, None, true);

    // 先按收纳态的半透明亮相，再淡入到全不透明 —— 直接「啪」地铺开太生硬
    FADE_CUR = 180;
    SetLayeredWindowAttributes(hwnd, COLORREF(0), 180, LWA_ALPHA);
    start_fade(hwnd, 255);
    SetTimer(hwnd, HINT_TIMER_ID, HINT_HOLD_MS, None);
}

/// 结束亮相：先淡出到收纳态的透明度，动画走完再缩回小图标。
unsafe fn end_first_run_peek(hwnd: HWND) {
    if !PEEK {
        return;
    }
    let _ = KillTimer(hwnd, HINT_TIMER_ID);
    FADE_COLLAPSE_AFTER = true;
    start_fade(hwnd, 180);
}

/// 立刻收干净、不带动画（亮相中途被点击/按键打断时用：
/// 那时候要马上算准窗口尺寸去做真展开，等不了淡出）。
unsafe fn peek_finish_now(hwnd: HWND) {
    if !PEEK {
        return;
    }
    PEEK = false;
    let _ = KillTimer(hwnd, HINT_TIMER_ID);
    let _ = KillTimer(hwnd, FADE_TIMER_ID);
    FADE_COLLAPSE_AFTER = false;
    FADE_CUR = 180;
    EXPANDED = false;
    HOVER_IDX = -1;
    hide_tool_tip(hwnd);

    let s = collapsed_size();
    let main_r = main_icon_rect();
    let mut wr = RECT::default();
    GetWindowRect(hwnd, &mut wr);
    let cx = wr.left + (main_r.left + main_r.right) / 2;
    let cy = wr.top + (main_r.top + main_r.bottom) / 2;

    SetWindowPos(hwnd, HWND_TOPMOST, cx - s / 2, cy - s / 2, s, s, SWP_SHOWWINDOW);
    SetLayeredWindowAttributes(hwnd, COLORREF(0), 180, LWA_ALPHA);
    apply_round_region(hwnd, s, s);
    WIN_W = s;
    WIN_H = s;
    InvalidateRect(hwnd, None, true);
}

/// 开始一段淡入淡出：记下终点，交给定时器逐帧推进。
unsafe fn start_fade(hwnd: HWND, target: i32) {
    FADE_TARGET = target.clamp(0, 255);
    if FADE_CUR == FADE_TARGET {
        // 已经在终点了，直接收尾，别留一个空转的定时器
        finish_fade(hwnd);
        return;
    }
    SetTimer(hwnd, FADE_TIMER_ID, FADE_STEP_MS, None);
}

/// 推进一帧（由 WM_TIMER 调）。到头了就收尾。
unsafe fn step_fade(hwnd: HWND) {
    let diff = FADE_TARGET - FADE_CUR;
    if diff.abs() <= FADE_STEP_ALPHA {
        FADE_CUR = FADE_TARGET;
    } else if diff > 0 {
        FADE_CUR += FADE_STEP_ALPHA;
    } else {
        FADE_CUR -= FADE_STEP_ALPHA;
    }
    SetLayeredWindowAttributes(hwnd, COLORREF(0), FADE_CUR.clamp(0, 255) as u8, LWA_ALPHA);
    if FADE_CUR == FADE_TARGET {
        finish_fade(hwnd);
    }
}

/// 一段淡入淡出结束：停表，并按需把亮相真正缩回收纳图标。
unsafe fn finish_fade(hwnd: HWND) {
    let _ = KillTimer(hwnd, FADE_TIMER_ID);
    if FADE_COLLAPSE_AFTER {
        FADE_COLLAPSE_AFTER = false;
        peek_finish_now(hwnd);
    }
}

// ---------- 窗口过程 ----------

unsafe extern "system" fn toolbar_wndproc(
    hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM,
) -> LRESULT {
    let state_ptr = TOOLBAR_STATE_PTR as *const RefCell<AppState>;
    match msg {
        WM_TIMER if wparam.0 == HINT_TIMER_ID => {
            // 亮相时间到：淡出后缩回收纳图标
            end_first_run_peek(hwnd);
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == FADE_TIMER_ID => {
            step_fade(hwnd);
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIP_TIMER_ID => {
            // 延时到点：光标仍停在原按钮上才弹，快速划过时 HOVER_IDX 变化会撤掉定时器
            let _ = KillTimer(hwnd, TIP_TIMER_ID);
            let idx = HOVER_IDX;
            if idx >= 0 && EXPANDED && !PEEK {
                let items = &layout_cached(SCALE).items;
                if let Some((_, act)) = items.get(idx as usize) {
                    if let Some(text) = tip_for_action(*act) {
                        let mut p = POINT::default();
                        GetCursorPos(&mut p);
                        show_tool_tip(text, p);
                    }
                }
            }
            LRESULT(0)
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut rc = RECT::default();
            GetClientRect(hwnd, &mut rc);
            let w = (rc.right - rc.left).max(1);
            let h = (rc.bottom - rc.top).max(1);

            // GDI 画线画圆都是硬边的。先把整个工具条按 4 倍尺寸画到内存位图，
            // 再用 HALFTONE 平均采样缩回原尺寸 —— 等于手工做了抗锯齿，
            // 图标边缘和小数字都不会再有锯齿。
            const SS: i32 = 4;
            let mdc = ss_surface(hdc, w * SS, h * SS);
            // 状态签名：仅当尺寸或 tool/color/width 变化才重画内容，
            // 否则直接复用已缓存的 4x 位图，避免每次 WM_PAINT 重光栅化所有按钮。
            let sig = if state_ptr.is_null() {
                (0u8, 0u8, 0u32, 0i32, w, h, HOVER_IDX)
            } else {
                let st = (*state_ptr).borrow();
                (
                    EXPANDED as u8,
                    tool_id(st.tool),
                    st.color.0,
                    (st.width * 100.0) as i32,
                    w,
                    h,
                    HOVER_IDX,
                )
            };
            if unsafe { TB_SIG } != sig {
                paint_content(mdc, rect(0, 0, w * SS, h * SS), SCALE * SS as f32, state_ptr);
                TB_SIG = sig;
            }
            let old_mode = SetStretchBltMode(hdc, HALFTONE);
            let _ = SetBrushOrgEx(hdc, 0, 0, None);
            let _ = StretchBlt(hdc, 0, 0, w, h, mdc, 0, 0, w * SS, h * SS, SRCCOPY);
            SetStretchBltMode(hdc, STRETCH_BLT_MODE(old_mode));

            EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_SETCURSOR => {
            // 擦除光标只在画布上用：指针移到工具条上要立刻恢复箭头，
            // 否则会残留一把橡皮擦悬在按钮上
            if let Ok(a) = LoadCursorW(None, IDC_ARROW) {
                SetCursor(a);
            }
            LRESULT(1)
        }
        WM_NCHITTEST => {
            // 按钮区域算客户区（可点击），其余当标题栏（可拖动窗口）
            let sx = (lparam.0 & 0xFFFF) as i16 as i32;
            let sy = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let mut pt = POINT { x: sx, y: sy };
            ScreenToClient(hwnd, &mut pt);
            if EXPANDED {
                let items = &layout_cached(SCALE).items;
                for &(r, act) in items.iter() {
                    // 分隔线不算可点区域，否则这一小条会拖不动工具条
                    if act != Action::Separator && hit(&r, pt.x, pt.y) {
                        return LRESULT(HTCLIENT as isize);
                    }
                }
            } else if pt.x >= 0 && pt.y >= 0 && pt.x < collapsed_size() && pt.y < collapsed_size()
            {
                // 收纳态整块都能点（点它 = 展开）
                return LRESULT(HTCLIENT as isize);
            }
            LRESULT(HTCAPTION as isize)
        }
        WM_NCMOUSEMOVE_ => {
            hide_tool_tip(hwnd);
            if HOVER_IDX != -1 {
                HOVER_IDX = -1;
                InvalidateRect(hwnd, None, false);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_MOUSELEAVE => {
            // 鼠标移出工具条 → 撤掉悬停高亮和说明（收纳态没有高亮，走空转）
            hide_tool_tip(hwnd);
            if HOVER_IDX != -1 {
                HOVER_IDX = -1;
                InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            crate::hooks::note_our_window_clicked();
            hide_tool_tip(hwnd);
            if PEEK {
                // 亮相这两秒里被点到，说明用户就是要打开它：先缩回再走真展开，
                // 这一下点击整体吃掉（不然抬起会落到光标底下的按钮上）。
                PEEK_SWALLOW_UP = true;
                peek_finish_now(hwnd);
                if !state_ptr.is_null() {
                    toggle_expanded(hwnd);
                }
                return LRESULT(0);
            }
            if !EXPANDED {
                // 收纳态的主图标既能拖走、也能点开：先记下按下位置，
                // 松手时再看是「拖过」还是「只是点了一下」
                ICON_DOWN = true;
                ICON_DRAG = false;
                let mut pt = POINT::default();
                GetCursorPos(&mut pt);
                ICON_P0 = (pt.x, pt.y);
                let mut wr = RECT::default();
                GetWindowRect(hwnd, &mut wr);
                WIN_P0 = (wr.left, wr.top);
                SetCapture(hwnd);
                return LRESULT(0);
            }
            if state_ptr.is_null() {
                return LRESULT(0);
            }
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let items = &layout_cached(SCALE).items;
            let hit_sl = items
                .iter()
                .find(|(r, a)| *a == Action::Slider && hit(r, x, y))
                .map(|(r, _)| *r);
            if let Some(r) = hit_sl {
                SLIDING = true;
                SetCapture(hwnd);
                // 点哪跳哪：一按下就直接对齐到鼠标位置
                slider_apply(hwnd, r, x, state_ptr);
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            // 追踪「移出」，展开态用来撤掉悬停高亮。
            let mut tme = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            let _ = TrackMouseEvent(&mut tme);
            if EXPANDED && !PEEK && !ICON_DOWN && !SLIDING {
                // 和官网一致：鼠标压在哪个按钮上，哪个按钮就变红边 + 红图标。
                // 只在「索引变化」时失效重绘，签名缓存会挡掉同一按钮内的重复刷新。
                let x = (lparam.0 & 0xFFFF) as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                let items = &layout_cached(SCALE).items;
                let idx = items
                    .iter()
                    .position(|(r, a)| *a != Action::Separator && hit(r, x, y))
                    .map(|i| i as i32)
                    .unwrap_or(-1);
                if idx != HOVER_IDX {
                    HOVER_IDX = idx;
                    InvalidateRect(hwnd, None, false);
                    // 悬停说明延时弹出；换按钮 / 移到空白处都先撤掉旧的
                    let _ = KillTimer(hwnd, TIP_TIMER_ID);
                    if idx >= 0 && tip_for_action(
                        layout_cached(SCALE).items[idx as usize].1,
                    ).is_some() {
                        SetTimer(hwnd, TIP_TIMER_ID, TIP_DELAY_MS, None);
                    } else {
                        hide_tool_tip(hwnd);
                    }
                }
            }
            if ICON_DOWN {
                let mut pt = POINT::default();
                GetCursorPos(&mut pt);
                let dx = pt.x - ICON_P0.0;
                let dy = pt.y - ICON_P0.1;
                // 4px 以内当误触，避免手抖把「点开」判成「拖动」
                if !ICON_DRAG && dx.abs() + dy.abs() > 4 {
                    ICON_DRAG = true;
                }
                if ICON_DRAG {
                    SetWindowPos(
                        hwnd,
                        HWND_TOPMOST,
                        WIN_P0.0 + dx,
                        WIN_P0.1 + dy,
                        0,
                        0,
                        SWP_NOSIZE | SWP_NOACTIVATE,
                    );
                }
                return LRESULT(0);
            }
            if SLIDING && !state_ptr.is_null() {
                let x = (lparam.0 & 0xFFFF) as i16 as i32;
                let items = &layout_cached(SCALE).items;
                if let Some((r, _)) = items.iter().find(|(_, a)| *a == Action::Slider) {
                    let r = *r;
                    slider_apply(hwnd, r, x, state_ptr);
                }
            }
            LRESULT(0)
        }
        WM_CAPTURECHANGED => {
            SLIDING = false;
            ICON_DOWN = false;
            ICON_DRAG = false;
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if PEEK_SWALLOW_UP {
                PEEK_SWALLOW_UP = false;
                return LRESULT(0);
            }
            if ICON_DOWN {
                let dragged = ICON_DRAG;
                ICON_DOWN = false;
                ICON_DRAG = false;
                let _ = ReleaseCapture();
                // 只是点了一下（没拖动）= 展开工具条
                if !dragged && !state_ptr.is_null() {
                    apply_action(hwnd, Action::Main, state_ptr);
                }
                return LRESULT(0);
            }
            crate::dbg::log("toolbar 收到抬起");
            // 拖滑块松手：只结束拖动，不要再当成点击去触发别的按钮
            if SLIDING {
                SLIDING = false;
                let _ = ReleaseCapture();
                InvalidateRect(hwnd, None, true);
                return LRESULT(0);
            }
            if state_ptr.is_null() {
                return LRESULT(0);
            }
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;

            let act = if !EXPANDED {
                if x >= 0 && y >= 0 && x < collapsed_size() && y < collapsed_size() {
                    Some(Action::Main)
                } else {
                    None
                }
            } else {
                let items = &layout_cached(SCALE).items;
                items
                    .iter()
                    .find(|(r, _)| hit(r, x, y))
                    .map(|(_, a)| *a)
            };

            if let Some(a) = act {
                crate::dbg::logf!("工具条点击 x={x} y={y} act={a:?}");
                apply_action(hwnd, a, state_ptr);
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            crate::hooks::note_our_window_clicked();
            if PEEK {
                // 亮相期间右键＝「我不想看」，收回去完事，不要顺手退出程序
                end_first_run_peek(hwnd);
                return LRESULT(0);
            }
            // 右键不再弹菜单：
            //   展开态 → 收回主图标（和点主图标一个效果）
            //   收纳态 → 直接退出程序
            if EXPANDED {
                collapse_to_icon();
            } else {
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            ss_release();
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn apply_action(hwnd: HWND, act: Action, state_ptr: *const RefCell<AppState>) {
    let canvas_hwnd = HWND(TOOLBAR_CANVAS as *mut _);
    match act {
        Action::Tool(t) => {
            // 切工具前先把半截折线收掉，免得它一直挂在画面上
            canvas::finalize_pending();
            let prev_tool = (*state_ptr).borrow().tool;
            // 水印按钮是「开关」，不是普通工具：
            //   没开 → 打开并弹框（预填上次文字）
            //   已开 → 一键关掉：删水印、回选择、按钮熄灭
            // 以前「再点一次」被当成「重新弹框」，退出水印只能靠清空文字回车，
            // 太绕；现在点一下就退出，想改文字关掉后再点开即可。
            if t == Tool::Watermark && prev_tool == Tool::Watermark {
                canvas::close_watermark();
                return;
            }
            let tool = {
                let mut st = (*state_ptr).borrow_mut();
                // 再点一次同一个工具 = 退出该工具 → 回到「选择/编辑」，
                // 画好的内容留在屏幕上，可以接着拖动/改大小（不再整体隐藏）。
                st.tool = if st.tool == t { Tool::Select } else { t };
                st.tool
            };
            // 从水印切走：文字输入框要立刻关掉，且不能把没确认的内容铺成水印。
            // 工具条是 NOACTIVATE 窗口，点它不会让输入框失焦，所以必须显式关。
            if prev_tool == Tool::Watermark && tool != Tool::Watermark {
                crate::textedit::cancel_watermark_open();
            }
            // 切到另一个绘制工具 = 开始画新东西，上一张图的「生成即选中」
            // 编辑态到此结束。否则在新工具里点中旧图元会被当成「拖走旧图元」，
            // 连画一个压在它上面的新图形都做不到。
            // 回到 Select / None 要保留选中：那正是「再点一次当前工具」进入
            // 编辑态的路径，用户就是冲着刚画好的那个图元去的。
            if tool != Tool::Select && tool != Tool::None {
                canvas::select_none();
            }
            if !canvas_hwnd.is_invalid() && tool != Tool::None {
                canvas::show_canvas(canvas_hwnd);
                raise_above_canvas(hwnd);
            }
            // 只要不是回到「选择/空」，就说明用户要开始新一轮标注了：
            // 关闭穿透态，画布重新接收鼠标。回到 Select/None 时保持穿透，
            // 让画完的图案可点选、空白区继续穿透。
            if tool != Tool::Select && tool != Tool::None {
                canvas::set_click_through(false);
            }
            // 光标跟着工具走：擦除工具要用半透明橡皮擦的样子
            crate::cursor::apply(tool);
            // 水印没有位置概念，点工具就直接问「写什么字」
            if tool == Tool::Watermark {
                canvas::edit_watermark();
            }
        }
        Action::Main => {
            toggle_expanded(hwnd);
        }
        Action::ColorIdx(i) => {
            (*state_ptr).borrow_mut().color = PALETTE[i];
            // 水印铺满全屏、序号是「当前样式」：改颜色要立刻作用到它们身上
            canvas::sync_watermark();
            canvas::sync_number();
        }
        Action::WidthPreview => {} // 只显示，不响应点击
        Action::Slider => {}       // 拖动逻辑在 WM_LBUTTONDOWN/MOUSEMOVE 里
        Action::Separator => {}    // 分隔线只用来分组
        Action::Undo => {
            let mut st = (*state_ptr).borrow_mut();
            let n = st.shapes.len();
            if n > 0 {
                canvas::remove_shape(&mut *st, n - 1);
            }
            drop(st);
            canvas::select_none();
        }
        Action::Clear => {
            let mut st = (*state_ptr).borrow_mut();
            st.shapes.clear();
            st.number_counter = 0;
            drop(st);
            canvas::reset_shape_indices();
        }
        Action::Select => {
            // 只进不退：点它一定是"进入选择模式并显示画布"，
            // 再点一次就退出的话，屏幕上的图案会突然消失（之前就是这个体验）
            canvas::finalize_pending();
            let prev_tool = (*state_ptr).borrow().tool;
            (*state_ptr).borrow_mut().tool = Tool::Select;
            if prev_tool == Tool::Watermark {
                crate::textedit::cancel_watermark_open();
            }
            if !canvas_hwnd.is_invalid() {
                canvas::show_canvas(canvas_hwnd);
                raise_above_canvas(hwnd);
            }
        }
    }
    if !canvas_hwnd.is_invalid() {
        canvas::repaint_global();
    }
    InvalidateRect(hwnd, None, true);
}

/// 让工具条重新浮到画布上面（两者都是 TOPMOST，后设置的在上）
unsafe fn raise_above_canvas(hwnd: HWND) {
    SetWindowPos(
        hwnd,
        HWND_TOPMOST,
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
    );
}

unsafe fn toggle_expanded(hwnd: HWND) {
    // 亮相途中被要求切换（按右 Ctrl、点图标、外部唤醒）：
    // 先把亮相收干净 —— 它会把自己算成收纳态缩回去，
    // 于是下面这句「取反」正好变成一次货真价实的展开，锚点也算得准。
    if PEEK {
        peek_finish_now(hwnd);
    }
    // 切换状态后鼠标位置的含义全变了，旧的悬停索引和说明都不能带过去
    HOVER_IDX = -1;
    hide_tool_tip(hwnd);
    let mut wr = RECT::default();
    GetWindowRect(hwnd, &mut wr);
    // 主图标中心（收纳前后保持不动，避免「跳」）
    let items = &layout_cached(SCALE).items;
    let main_r = items
        .iter()
        .find(|(_, a)| *a == Action::Main)
        .map(|(r, _)| *r)
        .unwrap_or(rect(0, 0, (BTN * SCALE) as i32, (BTN * SCALE) as i32));

    // 关键：两种状态算锚点的方式不同。
    //   展开态 —— 主图标在第二行第一格，中心 = 窗口原点 + main_r 中心
    //   收纳态 —— 窗口只有 52×52，图标画在正中，中心 = 窗口中心
    // 之前两种状态都用 main_r 算，收纳态窗口矮了 44px，
    // anchor 就多算了 44，于是每次展开都往下跳一行。
    let was_expanded = EXPANDED;
    let (anchor_x, anchor_y) = if was_expanded {
        (
            wr.left + (main_r.left + main_r.right) / 2,
            wr.top + (main_r.top + main_r.bottom) / 2,
        )
    } else {
        ((wr.left + wr.right) / 2, (wr.top + wr.bottom) / 2)
    };

    EXPANDED = !EXPANDED;
    if EXPANDED {
        let (w, h) = expanded_size();
        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            anchor_x - main_r.left - (main_r.right - main_r.left) / 2,
            anchor_y - main_r.top - (main_r.bottom - main_r.top) / 2,
            w,
            h,
            SWP_SHOWWINDOW,
        );
        SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA);
        apply_round_region(hwnd, w, h);
        WIN_W = w;
        WIN_H = h;

        // 展开 = 用户要重新开始标注：关闭穿透态，画布重新接收鼠标。
        // 穿透态下空格已不再能展开工具条，唯一入口就是点主图标，
        // 所以这里必须把穿透态关掉，否则展开后画布仍穿透、点下去画不出来。
        canvas::set_click_through(false);

        // 展开即默认选中「荧光笔」：展开工具条就是为了标注，
        // 荧光笔是最高频的讲解工具（划重点），省掉每次再点一下的步骤。
        let canvas_hwnd = HWND(TOOLBAR_CANVAS as *mut _);
        if !canvas_hwnd.is_invalid() {
            let state_ptr = TOOLBAR_STATE_PTR as *const RefCell<AppState>;
            if !state_ptr.is_null() {
                canvas::finalize_pending();
                let cur = {
                    let mut st = (*state_ptr).borrow_mut();
                    st.tool = Tool::Highlighter;
                    st.tool
                };
                canvas::show_canvas(canvas_hwnd);
                raise_above_canvas(hwnd);
                crate::cursor::apply(cur);
            }
        }
    } else {
        let s = collapsed_size();
        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            anchor_x - s / 2,
            anchor_y - s / 2,
            s,
            s,
            SWP_SHOWWINDOW,
        );
        SetLayeredWindowAttributes(hwnd, COLORREF(0), 180, LWA_ALPHA);
        apply_round_region(hwnd, s, s);
        WIN_W = s;
        WIN_H = s;

        // 收起前把半截折线落定，别让用户白画
        canvas::finalize_pending();
        // 收纳会把 tool 切到 None（相当于离开水印工具）：水印框按「取消」关掉，
        // 不能让预填的文字被当成确认过的水印铺满全屏。普通文字框照旧提交。
        crate::textedit::cancel_watermark_open();
        // 正在打字就先提交（输入框是普通窗口，关掉它键盘焦点才真的还回去）
        crate::textedit::commit_open();

        // 收纳 = 不再「正在画某个图形」。
        // 画面留着（用户要能看到自己画的东西），工具切到 None：
        // 画布里 None 与 Select 走同一套点选/拖动逻辑，功能不变；
        // 但全局滚轮钩子用 tool != None 判断「工具是否激活」，若留 Select，
        // 收纳后每滚一格都会偷偷调线宽并整屏重绘，还会触发系统提示音。
        // 注意：整体退出标注时画布已经被隐藏了，那种情况不要动状态。
        let canvas = HWND(TOOLBAR_CANVAS as *mut _);
        if !canvas.is_invalid() && IsWindowVisible(canvas).as_bool() {
            let state_ptr = TOOLBAR_STATE_PTR as *const RefCell<AppState>;
            if !state_ptr.is_null() {
                let changed = {
                    let mut st = (*state_ptr).borrow_mut();
                    if st.tool != Tool::None {
                        st.tool = Tool::None;
                        true
                    } else {
                        false
                    }
                };
                if changed {
                    canvas::repaint_global();
                }
            }
        }
        // 收纳后不该再是橡皮擦，换回普通箭头（Select 与 None 都是箭头）
        crate::cursor::apply(Tool::None);
        // 收纳完把键盘焦点还给光标底下那个程序（我们不该占着前台）
        canvas::release_focus();
    }
    InvalidateRect(hwnd, None, true);
}

/// 把工具条收进主图标（不切换：已经是收纳态就什么都不做）
pub fn collapse_to_icon() {
    unsafe {
        if TOOLBAR_HWND == 0 || !EXPANDED {
            return;
        }
        toggle_expanded(HWND(TOOLBAR_HWND as *mut _));
    }
}

/// 单实例唤醒：第二个实例启动时广播过来，把「已经跑着的那个」亮出来。
/// 收纳态 → 展开工具条（用户双击后立刻有反馈）；
/// 已展开 → 提到 TOPMOST 带最前并重绘（避免它被别的窗口盖住而看不见）。
pub fn reveal() {
    unsafe {
        if TOOLBAR_HWND == 0 {
            return;
        }
        let hwnd = HWND(TOOLBAR_HWND as *mut _);
        if !EXPANDED {
            toggle_expanded(hwnd);
        } else {
            SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
            InvalidateRect(hwnd, None, true);
        }
    }
}

/// 供全局钩子调用：空格键 → 收纳 / 展开工具条
pub fn toggle_collapse() {
    unsafe {
        if TOOLBAR_HWND == 0 {
            return;
        }
        let hwnd = HWND(TOOLBAR_HWND as *mut _);
        // 只动工具条本身：画布上已经画好的内容不该因为"收一下工具条"就消失
        toggle_expanded(hwnd);
    }
}

/// 鼠标滚轮改线宽（1~40）。之所以放在这里由全局钩子调用，
/// 是因为本程序的窗口都带 WS_EX_NOACTIVATE，拿不到焦点、收不到滚轮消息。
pub fn adjust_width(delta: f32) {
    unsafe {
        let state_ptr = TOOLBAR_STATE_PTR as *const RefCell<AppState>;
        if state_ptr.is_null() {
            return;
        }
        {
            let mut st = (*state_ptr).borrow_mut();
            st.width = (st.width + delta).clamp(1.0, 40.0);
        }
        canvas::sync_watermark();
        canvas::sync_text();
        canvas::sync_number();
        canvas::repaint_global();
        if TOOLBAR_HWND != 0 {
            InvalidateRect(HWND(TOOLBAR_HWND as *mut _), None, true);
        }
    }
}

pub fn is_expanded() -> bool {
    // 亮相期间窗口虽然铺开了，但对用户来说还没「进入标注」，
    // 所以滚轮等外部判断要当成收纳态，别去拦用户的滚轮。
    unsafe { EXPANDED && !PEEK }
}

/// 外部改了 state.tool 后调用：刷新按钮高亮。
/// 文字输入框右键提交会把工具切回「选择」，此时工具条自己收不到任何消息，
/// 不主动重绘的话「文字」按钮还会一直亮着。
pub fn refresh() {
    unsafe {
        if TOOLBAR_HWND != 0 {
            InvalidateRect(HWND(TOOLBAR_HWND as *mut _), None, true);
        }
    }
}

pub struct Toolbar {
    hwnd: HWND,
}

impl Toolbar {
    pub fn new(
        hinstance: HINSTANCE,
        state: Rc<RefCell<AppState>>,
        canvas_hwnd: HWND,
    ) -> Result<Self> {
        unsafe {
            let dpi = GetDpiForSystem();
            SCALE = (dpi as f32 / 96.0).clamp(1.0, 2.5) * UI_TUNE;

            let class_name = w!("ScreenMarkerToolbar");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(toolbar_wndproc),
                hInstance: hinstance,
                hbrBackground: HBRUSH(ptr::null_mut()),
                hIcon: crate::appicon::shared(),

                lpszClassName: class_name,
                ..Default::default()
            };
            RegisterClassW(&wc);

            TOOLBAR_STATE_PTR = Rc::as_ptr(&state) as *const RefCell<AppState> as usize;
            TOOLBAR_CANVAS = canvas_hwnd.0 as usize;

            // 启动时处于收纳态：只显示一个半透明主图标，用户点它才展开工具条。
            let s = collapsed_size();
            WIN_W = s;
            WIN_H = s;

            // 初始位置：屏幕下方中间（收纳态是个小图标，放底部中间不碍事）
            let sw = GetSystemMetrics(SM_CXSCREEN);
            let sh = GetSystemMetrics(SM_CYSCREEN);
            let x = (sw - s) / 2;
            let y = sh - s - (60.0 * SCALE) as i32;

            let hwnd = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                class_name,
                w!("ScreenMarkerToolbar"),
                WS_POPUP,
                x,
                y,
                s,
                s,
                None,
                None,
                hinstance,
                None,
            )?;
            TOOLBAR_HWND = hwnd.0 as usize;
            // 收纳态半透明（与 toggle_expanded 收起时一致）
            SetLayeredWindowAttributes(hwnd, COLORREF(0), 180, LWA_ALPHA);
            apply_round_region(hwnd, s, s);

            ShowWindow(hwnd, SW_SHOW);
            SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
            );
            InvalidateRect(hwnd, None, true);

            // 首次运行：展开亮相两秒再缩回去（之后永不再来）
            start_first_run_peek(hwnd);

            Ok(Toolbar { hwnd })
        }
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }
}
