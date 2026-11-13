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

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
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

static mut TIP_HWND: usize = 0;

// 4 倍超采样位图缓存。WM_PAINT 每次都 CreateCompatibleBitmap(w*4,h*4)
// 会反复分配/释放好几 MB；窗口尺寸不变时（展开/收纳切换时才变）直接复用。
static mut SS_DC: isize = 0;
static mut SS_BMP: isize = 0;
static mut SS_OLD: isize = 0;
static mut SS_W: i32 = 0;
static mut SS_H: i32 = 0;

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
const TIP_TEXT: &str = "点击或按右侧 Ctrl 键展开工具条";
/// WM_MOUSELEAVE（0x02A3）：windows 0.58 把它藏在未启用的 UI::Controls feature 里，
/// 这里必须自定义；漏了的话 match 模式会退化为变量绑定（匹配一切），
/// 把 WM_NCCREATE 也吞掉返回 0，导致工具条窗口创建失败（句柄无效）。
const WM_MOUSELEAVE: u32 = 0x02A3;

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

unsafe fn expanded_size() -> (i32, i32) {
    let (w, h, _) = layout(SCALE);
    (w, h)
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

/// 按钮瓷砖：普通态白底 + 浅灰描边；选中态白底 + 蓝色描边（PixPin 同款）。
/// 白底瓷砖在任何背景（包括纯白桌面）上都有清晰的轮廓。
unsafe fn button_bg(hdc: HDC, r: RECT, active: bool, s_: f32) {
    if active {
        round_tile(
            hdc,
            r,
            c(0xF2, 0xF7, 0xFF),
            c(0x0A, 0x84, 0xFF),
            2.0,
            8.0,
            s_,
        );
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
    canvas::repaint_global();
    InvalidateRect(hwnd, None, true);
}

unsafe fn draw_swatch(hdc: HDC, r: RECT, color: Color, selected: bool, s: f32) {
    // 圆角色块：填充本色；描边选中时蓝色加粗，未选中浅灰细边
    // （白色块靠灰边和白色瓷砖区分开来）
    let fill = COLORREF(
        ((color.blue() as u32) << 16) | ((color.green() as u32) << 8) | color.red() as u32,
    );
    if selected {
        round_tile(hdc, r, fill, c(0x0A, 0x84, 0xFF), 3.0, 6.0, s);
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
    let bg = CreateSolidBrush(c(0xFF, 0xFF, 0xFF));
    FillRect(hdc, &rc, bg);
    DeleteObject(HGDIOBJ(bg.0));

    // 面板整体一圈细边框：白色背景下让工具条有个清晰的外轮廓（PixPin 同款）
    let border_r = rect(
        rc.left + (s as i32),
        rc.top + (s as i32),
        (rc.right - rc.left) - (s as i32) * 2,
        (rc.bottom - rc.top) - (s as i32) * 2,
    );
    round_tile(hdc, border_r, c(0xFF, 0xFF, 0xFF), c(0xDD, 0xE0, 0xE5), 1.0, 14.0, s);

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

    let (_, _, items) = layout(s);
    let st = if state_ptr.is_null() {
        None
    } else {
        Some((*state_ptr).borrow())
    };
    let tool = st.as_ref().map(|x| x.tool).unwrap_or(Tool::None);
    let color = st.as_ref().map(|x| x.color).unwrap_or(PALETTE[0]);
    let width = st.as_ref().map(|x| x.width).unwrap_or(4.0);
    drop(st);

    for (r, act) in items {
        match act {
            Action::Tool(t) => {
                let on = t == tool;
                button_bg(hdc, r, on, s);
                icons::draw_tool_icon(
                    hdc,
                    icon_rect(r, s),
                    t,
                    if on { icons::ACTIVE } else { icons::NORMAL },
                );
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
                button_bg(hdc, r, false, s);
                icons::draw_undo(hdc, icon_rect(r, s), icons::NORMAL);
            }
            Action::Clear => {
                button_bg(hdc, r, false, s);
                icons::draw_clear(hdc, icon_rect(r, s), icons::NORMAL);
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

// ---------- 折叠态悬浮提示 ----------

/// tooltip 窗口过程：只负责把提示文字画成深色圆角气泡。
unsafe extern "system" fn tip_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut rc = RECT::default();
            GetClientRect(hwnd, &mut rc);

            // 深色半透明气泡底
            let bg = CreateSolidBrush(c(0x24, 0x24, 0x27));
            FillRect(hdc, &rc, bg);
            DeleteObject(HGDIOBJ(bg.0));

            // 白字提示，微软雅黑
            let fs = (13.0 * SCALE) as i32;
            let font = CreateFontW(
                -fs.max(12),
                0, 0, 0, 400, 0, 0, 0,
                DEFAULT_CHARSET.0 as u32, 0, 0, 0, 0,
                w!("Microsoft YaHei"),
            );
            let old_font = SelectObject(hdc, HGDIOBJ(font.0));
            SetBkMode(hdc, TRANSPARENT);
            SetTextColor(hdc, c(0xFF, 0xFF, 0xFF));
            let mut wide: Vec<u16> = TIP_TEXT.encode_utf16().collect();
            wide.push(0);
            let mut rr = rc;
            let pad = (10.0 * SCALE) as i32;
            rr.left += pad;
            rr.right -= pad;
            DrawTextW(hdc, &mut wide, &mut rr, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
            SelectObject(hdc, old_font);
            DeleteObject(HGDIOBJ(font.0));

            EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// 折叠态下鼠标悬停主图标时显示提示；鼠标移出/展开后隐藏。
unsafe fn show_tip() {
    if TIP_HWND != 0 {
        return; // 已显示
    }
    let hinst: HINSTANCE = GetModuleHandleW(None).unwrap_or_default().into();

    // 按文字长度估算气泡宽度
    let fs = (13.0 * SCALE) as i32;
    let text_w = (TIP_TEXT.chars().count() as i32) * fs + (24.0 * SCALE) as i32;
    let w = text_w.max(120);
    let h = (30.0 * SCALE) as i32;

    // 定位：折叠图标正上方居中，留一点间距
    let tb = HWND(TOOLBAR_HWND as *mut _);
    let mut r = RECT::default();
    GetWindowRect(tb, &mut r);
    let x = (r.left + r.right) / 2 - w / 2;
    let y = r.top - h - (6.0 * SCALE) as i32;

    let class_name = w!("ScreenMarkerTip");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(tip_wndproc),
        hInstance: hinst,
        hbrBackground: HBRUSH(ptr::null_mut()),
        lpszClassName: class_name,
        ..Default::default()
    };
    RegisterClassW(&wc);

    let hwnd = match CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        class_name,
        w!("ScreenMarkerTip"),
        WS_POPUP,
        x,
        y,
        w,
        h,
        None,
        None,
        hinst,
        None,
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

/// 隐藏并销毁折叠态提示。
unsafe fn hide_tip() {
    if TIP_HWND == 0 {
        return;
    }
    let hwnd = HWND(TIP_HWND as *mut _);
    let _ = DestroyWindow(hwnd);
    TIP_HWND = 0;
}


// ---------- 窗口过程 ----------

unsafe extern "system" fn toolbar_wndproc(
    hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM,
) -> LRESULT {
    let state_ptr = TOOLBAR_STATE_PTR as *const RefCell<AppState>;
    match msg {
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
            paint_content(mdc, rect(0, 0, w * SS, h * SS), SCALE * SS as f32, state_ptr);
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
                let (_, _, items) = layout(SCALE);
                for (r, act) in items {
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
        WM_MOUSELEAVE => {
            // 折叠态鼠标移出 → 隐藏提示
            hide_tip();
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            crate::hooks::note_our_window_clicked();
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
            let (_, _, items) = layout(SCALE);
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
            // 折叠态：鼠标悬停时弹出提示，并追踪「移出」以便及时隐藏。
            if !EXPANDED {
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                let _ = TrackMouseEvent(&mut tme);
                show_tip();
            }
            if ICON_DOWN {
                let mut pt = POINT::default();
                GetCursorPos(&mut pt);
                let dx = pt.x - ICON_P0.0;
                let dy = pt.y - ICON_P0.1;
                // 4px 以内当误触，避免手抖把「点开」判成「拖动」
                if !ICON_DRAG && dx.abs() + dy.abs() > 4 {
                    ICON_DRAG = true;
                    // 开始拖动：提示还悬在旧位置，先隐藏，松手后再悬停会重新弹出
                    hide_tip();
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
                let (_, _, items) = layout(SCALE);
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
                let (_, _, items) = layout(SCALE);
                items
                    .iter()
                    .find(|(r, _)| hit(r, x, y))
                    .map(|(_, a)| *a)
            };

            if let Some(a) = act {
                crate::dbg::log(&format!("工具条点击 x={x} y={y} act={a:?}"));
                apply_action(hwnd, a, state_ptr);
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            crate::hooks::note_our_window_clicked();
            // 右键不再弹菜单：
            //   展开态 → 收回主图标（和点主图标一个效果）
            //   收纳态 → 直接退出程序
            if EXPANDED {
                hide_tip();
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
            let tool = {
                let mut st = (*state_ptr).borrow_mut();
                // 再点一次同一个工具 = 退出该工具 → 回到「选择/编辑」，
                // 画好的内容留在屏幕上，可以接着拖动/改大小（不再整体隐藏）
                st.tool = if st.tool == t { Tool::Select } else { t };
                st.tool
            };
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
            st.shapes.pop();
            drop(st);
            canvas::select_none();
        }
        Action::Clear => {
            let mut st = (*state_ptr).borrow_mut();
            st.shapes.clear();
            st.number_counter = 0;
            drop(st);
            canvas::select_none();
        }
        Action::Select => {
            // 只进不退：点它一定是"进入选择模式并显示画布"，
            // 再点一次就退出的话，屏幕上的图案会突然消失（之前就是这个体验）
            canvas::finalize_pending();
            (*state_ptr).borrow_mut().tool = Tool::Select;
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
    let mut wr = RECT::default();
    GetWindowRect(hwnd, &mut wr);
    // 主图标中心（收纳前后保持不动，避免「跳」）
    let (_, _, items) = layout(SCALE);
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
    unsafe { EXPANDED }
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
                hIcon: crate::appicon::load(),

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

            Ok(Toolbar { hwnd })
        }
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }
}
