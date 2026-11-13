// 画布窗口：全屏透明覆盖层，负责合成所有图形 + 鼠标绘制交互。
//
// 渲染管线（每次重绘）：
//   1. Frame::clear         —— 全屏填成「alpha=1 的隐形背景」（可接收鼠标）
//   2. 逐个图形用 GDI 画到 DIB（半透明图形先用占位色画）
//   3. Frame::convert_magic —— 占位色换成真色 + 指定 alpha
//   4. Frame::fix_alpha     —— 画过的像素提升为不透明
//   5. Frame::present       —— UpdateLayeredWindow 提交（逐像素 alpha）

#![allow(dead_code)]

use crate::frame::{Frame, MAGIC};
use crate::render::*;
use crate::shape::*;
use crate::textedit;

use std::cell::RefCell;
use std::ptr;
use std::rc::Rc;

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetCapture, GetKeyState, ReleaseCapture, SetCapture, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::*;

pub static mut CANVAS_STATE_PTR: usize = 0;
// 工具条句柄：画布显示时必须插到它「下面」，否则全屏画布会盖住工具条，
/// 导致点了按钮却画在画布上（之前就是这个 bug）
static mut CANVAS_TOOLBAR: usize = 0;
static mut FRAME_PTR: usize = 0;
static mut CANVAS_HWND: usize = 0;
static mut CANVAS_HINSTANCE: usize = 0;

// 绘制中的临时状态
static mut CURRENT_DRAWING: Option<Shape> = None;
static mut CURRENT_POLY: Vec<Point> = Vec::new();
static mut DRAW_START: Option<Point> = None;
static mut SELECTED: Option<usize> = None;
static mut DRAG_LAST: Option<Point> = None;
// 进程级保留一份 state 的 Rc，方便文字编辑框等模块克隆使用
static mut STATE_RC: Option<Rc<RefCell<AppState>>> = None;
// 「穿透态」：退出当前工具、但保留画面的状态下，画布空白区域不再拦截鼠标，
// 点击直接穿透到下面的程序；画了内容的像素仍拦截（还能点选/拖动图元）。
// 进入任何绘制工具、或画布被重新激活绘图时，穿透关掉。
static mut CLICK_THROUGH: bool = false;

// ---------- 整屏快照（放大镜的「底片」） ----------
/// 放大镜要显示「画布下面的真实画面」，而画布本身盖在上面。
/// 每次重绘都藏窗口再截图会闪，所以进放大镜时抓一张整屏快照存着，
/// 之后所有取样都从这张图里取。
static mut SNAP: *const (Vec<u8>, i32, i32) = ptr::null();

/// 供 render.rs 取样：返回 (BGRA 数据, 宽, 高)
pub fn snapshot() -> Option<&'static (Vec<u8>, i32, i32)> {
    unsafe {
        if SNAP.is_null() {
            None
        } else {
            Some(&*SNAP)
        }
    }
}

fn set_snapshot(v: (Vec<u8>, i32, i32)) {
    unsafe {
        if !SNAP.is_null() {
            drop(Box::from_raw(SNAP as *mut (Vec<u8>, i32, i32)));
        }
        SNAP = Box::into_raw(Box::new(v));
    }
}

// ---------- 选择 / 移动 / 缩放 ----------
/// 拖动模式掩码：0 = 没在拖；1/2/4/8 = 左/右/上/下（组合成角点）；0x80 = 整体移动
const EDIT_MOVE: u8 = 0x80;
static mut EDIT_MASK: u8 = 0;
static mut EDIT_IDX: usize = usize::MAX;
/// 按下时的鼠标位置
static mut EDIT_P0: Option<Point> = None;
/// 按下时图元的外接框（缩放的基准）
static mut EDIT_BB0: Option<Rect> = None;
/// 按下时图元的一份副本 —— 每次移动都从它重算，
/// 避免「连续缩放时误差累积」把图形越缩越歪
static mut EDIT_BASE: Option<Shape> = None;

/// 折线橡皮筋：光标位置，用来画「上一个顶点 → 光标」的预览线
static mut POLY_RUBBER: Option<Point> = None;

/// 放大镜是否处于「按下拖动」中
static mut MAG_DRAG: bool = false;
/// 放大镜是否处于「拖圆周改半径」中
static mut MAG_RESIZE: bool = false;
/// 聚光灯是否处于「按下拖动」中
static mut SPOT_DRAG: bool = false;
/// 聚光灯是否处于「拖圆周改半径」中
static mut SPOT_RESIZE: bool = false;

// ---------- 马赛克框（可拖边框调整大小） ----------
/// 当前「激活」的马赛克图元下标（usize::MAX = 无）。只有它显示虚线框。
static mut ACTIVE_MOSAIC: usize = usize::MAX;
/// 缩放/移动状态：位 1=左 2=右 4=上 8=下 16=整体移动
static mut MZ_MASK: u8 = 0;
static mut MZ_IDX: usize = usize::MAX;
static mut MZ_P0: Option<Point> = None;
static mut MZ_RECT0: Option<Rect> = None;
/// 马赛克框最小尺寸
const MZ_MIN: f32 = 24.0;
/// 马赛克框默认尺寸
const MZ_DEFAULT: (f32, f32) = (160.0, 90.0);

/// 聚光灯：单击就给的默认半径（像素）
const SPOT_DEFAULT: f32 = 150.0;
/// 聚光灯半径的上下限
const SPOT_MIN: f32 = 20.0;
const SPOT_MAX: f32 = 4000.0;
/// 每格滚轮缩放的比例（乘性缩放比加减固定值手感更均匀）
const SPOT_ZOOM: f32 = 1.10;

/// 放大镜：默认半径、倍率区间、每格滚轮的乘性倍率
const MAG_DEFAULT: f32 = 120.0;
const MAG_MIN_R: f32 = 30.0;
const MAG_MAX_R: f32 = 600.0;
const MAG_ZOOM_MIN: f32 = 1.2;
const MAG_ZOOM_MAX: f32 = 8.0;
const MAG_ZOOM_STEP: f32 = 1.15;

/// 文字字号由粗细滑块派生：滑块 1~40 → 字号 16~160。
/// 和 create_text / textedit 共用，改一处就全变。
pub fn text_size(width: f32) -> f32 {
    (width * 4.0).clamp(16.0, 160.0)
}

/// 水印字号由粗细滑块派生。
/// 系数取 6.5：默认线宽 4.0 → 26px，正好是原来写死的字号，
/// 不改变观感；同时滑块 1~40 对应 20~260px 的可调区间。
pub fn watermark_size(width: f32) -> f32 {
    (width * 6.5).max(20.0)
}

/// 序号圆圈半径同样由粗细滑块派生：滑块 1~40 → 半径 14~120。
/// 默认线宽 4.0 时是 14px，和工作区里原来写死的 (width*3).max(14) 一致。
pub fn number_radius(width: f32) -> f32 {
    (width * 3.0).clamp(14.0, 120.0)
}

fn st_ptr() -> *const RefCell<AppState> {
    unsafe { CANVAS_STATE_PTR as *const RefCell<AppState> }
}

fn hwnd_of() -> HWND {
    unsafe { HWND(CANVAS_HWND as *mut _) }
}

unsafe extern "system" fn canvas_wndproc(
    hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM,
) -> LRESULT {
    let state_ptr = st_ptr();
    match msg {
        WM_PAINT => {
            // 我们用 UpdateLayeredWindow 直接提交画面，这里只需要把无效区域「消化掉」
            let mut ps = PAINTSTRUCT::default();
            let _ = BeginPaint(hwnd, &mut ps);
            let _ = EndPaint(hwnd, &ps);
            repaint(hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_NCHITTEST => {
            // 穿透态：空白区域（alpha=0 的像素）返回 HTTRANSPARENT，
            // 让鼠标事件落到画布下面的程序；画了内容的像素返回 HTCLIENT，
            // 仍能点选/拖动图元。非穿透态一律 HTCLIENT（正常绘图）。
            if !CLICK_THROUGH {
                return LRESULT(HTCLIENT as isize);
            }
            // lparam 是屏幕坐标（低 16 位 x、高 16 位 y，有符号）。
            // 画布是全屏分层窗口、位于 (0,0)，但为稳妥仍转成窗口坐标。
            let sx = (lparam.0 as i16) as i32;
            let sy = ((lparam.0 >> 16) as i16) as i32;
            let mut pt = POINT { x: sx, y: sy };
            ScreenToClient(hwnd, &mut pt);
            let alpha = if FRAME_PTR == 0 {
                0
            } else {
                (*(FRAME_PTR as *const Frame)).alpha_at(pt.x, pt.y)
            };
            if alpha == 0 {
                LRESULT(HTTRANSPARENT as isize)
            } else {
                LRESULT(HTCLIENT as isize)
            }
        }
        WM_SETCURSOR => {
            // 擦除工具下把光标换成自绘的半透明橡皮擦；
            // 其它工具交回系统默认（否则从擦除切走时箭头不会变回来）
            let tool = if state_ptr.is_null() {
                Tool::None
            } else {
                (*state_ptr).borrow().tool
            };
            if tool == Tool::Eraser {
                SetCursor(crate::cursor::eraser());
                return LRESULT(1);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_LBUTTONDOWN => {
            crate::hooks::note_our_window_clicked();
            if state_ptr.is_null() {
                return LRESULT(0);
            }
            let p = point_of(lparam);
            crate::dbg::log(&format!(
                "canvas 收到按下 p=({:.0},{:.0}) tool={:?}",
                p.x,
                p.y,
                (*state_ptr).borrow().tool
            ));
            SetCapture(hwnd);
            let tool = (*state_ptr).borrow().tool;
            match tool {
                Tool::Eraser => {
                    let mut st = (*state_ptr).borrow_mut();
                    erase_at(&mut st, p);
                }
                // 收纳态的 tool 是 None（不是 Select），两者都按「选择 / 编辑」处理，
                // 这样收起工具条后仍可点选、拖动已画好的图元。
                Tool::None | Tool::Select => {
                    // 按下：优先抓已选中图元的手柄 → 缩放；框内 → 移动；否则重新选。
                    // 点在空白处 = 「我不画了」：取消选中、退出当前功能、工具条收进主图标、
                    // 焦点还给下面窗口。但画布和已经画好的图案必须继续显示 ——
                    // 之前这里直接调 exit_markup()，展开态会把整层画布 SW_HIDE，
                    // 用户看到的就是「点一下空白，刚才画的图全没了」。
                    let hit = {
                        let mut st = (*state_ptr).borrow_mut();
                        edit_begin(&mut st, p)
                    };
                    if !hit {
                        ReleaseCapture();
                        keep_canvas_exit();
                        return LRESULT(0);
                    }
                }
                Tool::Polyline => {
                    CURRENT_POLY.push(p);
                    POLY_RUBBER = Some(p);
                }
                Tool::Spotlight => {
                    // 聚光灯全屏只要一个圆：已经有就把它挪到点击处，不再生成第二个。
                    // 按在圆周上则是「拉边改半径」，圆心不动。
                    let (idx, resize) = {
                        let mut st = (*state_ptr).borrow_mut();
                        let exist = st
                            .shapes
                            .iter()
                            .rposition(|s| matches!(s, Shape::Spotlight { .. }));
                        match exist {
                            Some(i) => {
                                let mut rs = false;
                                if let Some(Shape::Spotlight { center, radius }) = st.shapes.get(i) {
                                    let d = ((p.x - center.x).powi(2)
                                        + (p.y - center.y).powi(2))
                                    .sqrt();
                                    rs = (d - *radius).abs() <= 12.0;
                                }
                                if !rs {
                                    if let Some(Shape::Spotlight { center, .. }) =
                                        st.shapes.get_mut(i)
                                    {
                                        center.x = p.x;
                                        center.y = p.y;
                                    }
                                }
                                (i, rs)
                            }
                            None => {
                                st.shapes.push(Shape::Spotlight {
                                    center: p,
                                    radius: SPOT_DEFAULT,
                                });
                                (st.shapes.len() - 1, false)
                            }
                        }
                    };
                    SELECTED = Some(idx);
                    SPOT_RESIZE = resize;
                    SPOT_DRAG = !resize;
                }
                Tool::Magnifier => {
                    // 第一次用放大镜才抓底片；之后整屏快照一直复用
                    if snapshot().is_none() {
                        capture_snapshot();
                    }
                    // 同理：只保留一个圆，按圆周 = 调半径，按别处 = 挪圆心
                    let (idx, resize) = {
                        let mut st = (*state_ptr).borrow_mut();
                        let exist = st
                            .shapes
                            .iter()
                            .rposition(|s| matches!(s, Shape::Magnifier { .. }));
                        match exist {
                            Some(i) => {
                                let mut rs = false;
                                if let Some(Shape::Magnifier { center, radius, .. }) =
                                    st.shapes.get(i)
                                {
                                    let d = ((p.x - center.x).powi(2)
                                        + (p.y - center.y).powi(2))
                                    .sqrt();
                                    rs = (d - *radius).abs() <= 12.0;
                                }
                                if !rs {
                                    if let Some(Shape::Magnifier { center, .. }) =
                                        st.shapes.get_mut(i)
                                    {
                                        center.x = p.x;
                                        center.y = p.y;
                                    }
                                }
                                (i, rs)
                            }
                            None => {
                                st.shapes.push(Shape::Magnifier {
                                    center: p,
                                    radius: MAG_DEFAULT,
                                    zoom: 2.0,
                                });
                                (st.shapes.len() - 1, false)
                            }
                        }
                    };
                    // 立刻选中它：滚轮调倍率、拖动改位置都作用在它身上
                    SELECTED = Some(idx);
                    MAG_RESIZE = resize;
                    MAG_DRAG = !resize;
                }
                Tool::Text => {
                    // 先看是不是点在已有文字上：命中就进通用编辑（移动 + 手柄缩放），
                    // 不再新建一个空框压在它上面 —— 文字画完也能直接选中拖动
                    let on_text = {
                        let st = (*state_ptr).borrow();
                        st.shapes
                            .iter()
                            .rposition(|s| matches!(s, Shape::Text { .. }) && s.hit(p, 8.0))
                    };
                    if let Some(i) = on_text {
                        SELECTED = Some(i);
                        let mut st = (*state_ptr).borrow_mut();
                        edit_begin(&mut st, p);
                    } else {
                        // 先释放鼠标捕获，避免随后的抬起把刚弹出的输入框打到失焦
                        ReleaseCapture();
                        // 注意：这里必须先结束 state 的借用，再打开输入框。
                        // 输入框内部还要 borrow_mut，否则 RefCell 双重借用会直接 panic。
                        let info = {
                            let mut st = (*state_ptr).borrow_mut();
                            create_text(&mut st, p)
                        };
                        if let Some((idx, rect)) = info {
                            open_text_editor(idx, rect, "");
                        }
                    }
                }
                Tool::Watermark => {
                    // 水印是整屏平铺文字，没有位置概念：点它 = 改文字内容
                    ReleaseCapture();
                    edit_watermark();
                }
                Tool::Mosaic => {
                    // 已有激活的马赛克：拖边框 = 缩放，拖内部 = 整体移动。
                    // 其它位置单击 = 新建一个 160×90 的框。
                    let mut mask = 0u8;
                    let mut inside = false;
                    let mut r0 = None;
                    {
                        let st = (*state_ptr).borrow();
                        if ACTIVE_MOSAIC != usize::MAX && ACTIVE_MOSAIC < st.shapes.len() {
                            if let Shape::Mosaic { rect, .. } = &st.shapes[ACTIVE_MOSAIC] {
                                mask = mosaic_handle(*rect, p, 10.0);
                                inside = rect.contains(p, 0.0);
                                r0 = Some(*rect);
                            }
                        }
                    }
                    let mut started = false;
                    if let Some(rr) = r0 {
                        if mask != 0 || inside {
                            MZ_IDX = ACTIVE_MOSAIC;
                            MZ_MASK = if mask != 0 { mask } else { 16 };
                            MZ_P0 = Some(p);
                            MZ_RECT0 = Some(rr);
                            started = true;
                        }
                    }
                    if !started {
                        let idx = {
                            let mut st = (*state_ptr).borrow_mut();
                            mosaic_create(&mut st, p)
                        };
                        mosaic_refresh(idx);
                    }
                }
                _ => {
                    let mut st = (*state_ptr).borrow_mut();
                    begin_draw(&mut st, p);
                }
            }
            raise_toolbar();
            // 自动化验证用：只在 SM_LOG 打开时写，平时不产生任何文件
            if std::env::var_os("SM_LOG").is_some() {
                let st = (*state_ptr).borrow();
                let mag = st
                    .shapes
                    .iter()
                    .filter(|s| matches!(s, Shape::Magnifier { .. }))
                    .count();
                let spot = st
                    .shapes
                    .iter()
                    .filter(|s| matches!(s, Shape::Spotlight { .. }))
                    .count();
                crate::dbg::log(&format!(
                    "按下后 放大镜={mag} 聚光灯={spot} 图元总数={}",
                    st.shapes.len()
                ));
            }
            repaint(hwnd);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if state_ptr.is_null() {
                return LRESULT(0);
            }
            let p = point_of(lparam);
            let tool = (*state_ptr).borrow().tool;

            // 擦除中每次移动都重设一次：拖得越快，系统越可能来不及派 WM_SETCURSOR
            if tool == Tool::Eraser {
                SetCursor(crate::cursor::eraser());
            }

            // 折线：没按着鼠标时也要跟光标，画出「上一顶点 → 光标」的橡皮筋。
            // 之前两次点击之间画面完全不动，手感就是"点了没反应/很卡"。
            if tool == Tool::Polyline && !CURRENT_POLY.is_empty() {
                let moved = match POLY_RUBBER {
                    Some(q) => (q.x - p.x).abs() + (q.y - p.y).abs() > 1.0,
                    None => true,
                };
                if moved {
                    POLY_RUBBER = Some(p);
                    repaint(hwnd);
                }
                return LRESULT(0);
            }

            if GetCapture() != hwnd {
                return LRESULT(0);
            }
            match tool {
                Tool::Eraser => {
                    let mut st = (*state_ptr).borrow_mut();
                    erase_at(&mut st, p);
                    drop(st);
                    repaint(hwnd);
                }
                Tool::None | Tool::Select => {
                    let changed = {
                        let mut st = (*state_ptr).borrow_mut();
                        edit_drag(&mut st, p)
                    };
                    if changed {
                        repaint(hwnd);
                    }
                }
                Tool::Magnifier => {
                    // 拖着圆走 / 拉圆边改半径
                    if MAG_DRAG || MAG_RESIZE {
                        let mut changed = false;
                        {
                            let mut st = (*state_ptr).borrow_mut();
                            if let Some(i) = SELECTED {
                                if let Some(Shape::Magnifier { center, radius, .. }) =
                                    st.shapes.get_mut(i)
                                {
                                    if MAG_RESIZE {
                                        let d = ((p.x - center.x).powi(2)
                                            + (p.y - center.y).powi(2))
                                        .sqrt();
                                        *radius = d.clamp(MAG_MIN_R, MAG_MAX_R);
                                    } else {
                                        center.x = p.x;
                                        center.y = p.y;
                                    }
                                    changed = true;
                                }
                            }
                        }
                        if changed {
                            repaint(hwnd);
                        }
                    }
                }
                Tool::Spotlight => {
                    // 拖着圆走 / 拉圆边改半径：始终只有这一个圆
                    if SPOT_DRAG || SPOT_RESIZE {
                        let mut changed = false;
                        {
                            let mut st = (*state_ptr).borrow_mut();
                            if let Some(i) = SELECTED {
                                if let Some(Shape::Spotlight { center, radius }) =
                                    st.shapes.get_mut(i)
                                {
                                    if SPOT_RESIZE {
                                        let d = ((p.x - center.x).powi(2)
                                            + (p.y - center.y).powi(2))
                                        .sqrt();
                                        *radius = d.clamp(SPOT_MIN, SPOT_MAX);
                                    } else {
                                        center.x = p.x;
                                        center.y = p.y;
                                    }
                                    changed = true;
                                }
                            }
                        }
                        if changed {
                            repaint(hwnd);
                        }
                    }
                }
                Tool::Text => {
                    // 文字工具下点中已有文字 = 普通编辑：整体移动、拖手柄缩放
                    let changed = {
                        let mut st = (*state_ptr).borrow_mut();
                        edit_drag(&mut st, p)
                    };
                    if changed {
                        repaint(hwnd);
                    }
                }
                Tool::Mosaic => {
                    if MZ_MASK != 0 && MZ_IDX != usize::MAX {
                        if let (Some(p0), Some(r0)) = (MZ_P0, MZ_RECT0) {
                            let dx = p.x - p0.x;
                            let dy = p.y - p0.y;
                            let mut r = r0;
                            if MZ_MASK & 16 != 0 {
                                r.x = r0.x + dx;
                                r.y = r0.y + dy;
                            } else {
                                if MZ_MASK & 1 != 0 {
                                    let nx = (r0.x + dx).min(r0.right() - MZ_MIN);
                                    r.x = nx;
                                    r.w = r0.right() - nx;
                                }
                                if MZ_MASK & 2 != 0 {
                                    r.w = (r0.w + dx).max(MZ_MIN);
                                }
                                if MZ_MASK & 4 != 0 {
                                    let ny = (r0.y + dy).min(r0.bottom() - MZ_MIN);
                                    r.y = ny;
                                    r.h = r0.bottom() - ny;
                                }
                                if MZ_MASK & 8 != 0 {
                                    r.h = (r0.h + dy).max(MZ_MIN);
                                }
                            }
                            {
                                let mut st = (*state_ptr).borrow_mut();
                                if let Some(Shape::Mosaic { rect, .. }) = st.shapes.get_mut(MZ_IDX) {
                                    *rect = r;
                                }
                            }
                            repaint(hwnd);
                        }
                    }
                }
                Tool::Polyline => {}
                _ => {
                    let mut st = (*state_ptr).borrow_mut();
                    update_draw(&mut st, p);
                    drop(st);
                    repaint(hwnd);
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            // 注意：必须先释放捕获，否则后续所有鼠标消息都会被画布截走，
            // 工具条按钮就永远点不动了
            ReleaseCapture();
            if state_ptr.is_null() {
                return LRESULT(0);
            }
            let p = point_of(lparam);
            let tool = (*state_ptr).borrow().tool;
            match tool {
                Tool::Mosaic => {
                    // 松手才真正重抓：拖动过程中只是把旧位图拉伸预览
                    if MZ_MASK != 0 {
                        let idx = MZ_IDX;
                        MZ_MASK = 0;
                        MZ_P0 = None;
                        MZ_RECT0 = None;
                        if idx != usize::MAX {
                            mosaic_refresh(idx);
                        }
                    }
                }
                Tool::None | Tool::Select => {
                    // 松手：结束拖动；马赛克被拉过大小要重抓一次，否则糊着
                    let refresh = {
                        let mut st = (*state_ptr).borrow_mut();
                        edit_end(&mut st)
                    };
                    if let Some(i) = refresh {
                        mosaic_refresh(i);
                    }
                }
                Tool::Magnifier => {
                    MAG_DRAG = false;
                    MAG_RESIZE = false;
                }
                Tool::Spotlight => {
                    SPOT_DRAG = false;
                    SPOT_RESIZE = false;
                }
                Tool::Text => {
                    // 文字框拖动/缩放结束
                    let mut st = (*state_ptr).borrow_mut();
                    edit_end(&mut st);
                }
                Tool::Polyline => {
                    POLY_RUBBER = Some(p);
                }
                Tool::Eraser => {}
                _ => {
                    let mut st = (*state_ptr).borrow_mut();
                    end_draw(&mut st, p);
                }
            }
            DRAG_LAST = None;
            // 自动化验证用：松手时把圆形图元的几何打出来
            if std::env::var_os("SM_LOG").is_some() {
                let st = (*state_ptr).borrow();
                for s in st.shapes.iter() {
                    match s {
                        Shape::Spotlight { center, radius } => crate::dbg::log(&format!(
                            "几何 聚光 center=({:.0},{:.0}) r={:.0}",
                            center.x, center.y, radius
                        )),
                        Shape::Magnifier {
                            center,
                            radius,
                            zoom,
                        } => crate::dbg::log(&format!(
                            "几何 放大 center=({:.0},{:.0}) r={:.0} zoom={:.2}",
                            center.x, center.y, radius, zoom
                        )),
                        _ => {}
                    }
                }
            }
            raise_toolbar();
            repaint(hwnd);
            LRESULT(0)
        }
        WM_LBUTTONDBLCLK => {
            crate::hooks::note_our_window_clicked();
            if state_ptr.is_null() {
                return LRESULT(0);
            }
            let tool = (*state_ptr).borrow().tool;
            if tool == Tool::Polyline {
                // 双击收尾：双击会先来一个 LBUTTONDOWN 把同一个点又压进去一次，
                // 这里去掉重复点，避免多出一段零长度线段
                if CURRENT_POLY.len() >= 2 {
                    let last = CURRENT_POLY[CURRENT_POLY.len() - 1];
                    let prev = CURRENT_POLY[CURRENT_POLY.len() - 2];
                    if (last.x - prev.x).abs() < 1.0 && (last.y - prev.y).abs() < 1.0 {
                        CURRENT_POLY.pop();
                    }
                }
                let idx = {
                    let mut st = (*state_ptr).borrow_mut();
                    finalize_poly(&mut st);
                    st.shapes.len().checked_sub(1)
                };
                POLY_RUBBER = None;
                // 收尾后顺手选中它，接着就能拖
                SELECTED = idx;
                {
                    let mut st = (*state_ptr).borrow_mut();
                    st.tool = Tool::Select;
                }
                repaint(hwnd);
            }
            if tool == Tool::Text {
                // 双击已有文字 = 重新编辑内容（文字画完之后还能改）
                let p = point_of(lparam);
                let info = {
                    let st = (*state_ptr).borrow();
                    st.shapes
                        .iter()
                        .rposition(|s| matches!(s, Shape::Text { .. }) && s.hit(p, 8.0))
                        .and_then(|i| match st.shapes.get(i) {
                            Some(Shape::Text { rect, text, .. }) => Some((i, *rect, text.clone())),
                            _ => None,
                        })
                };
                if let Some((i, rect, text)) = info {
                    ReleaseCapture();
                    SELECTED = Some(i);
                    open_text_editor(i, rect, &text);
                    repaint(hwnd);
                }
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            crate::hooks::note_our_window_clicked();
            if state_ptr.is_null() {
                return LRESULT(0);
            }
            // 1) 折线正在画：右键 = 收尾，并选中刚画的那条。
            //    注意：折线最后一段是「最后一个顶点 → 光标」的橡皮筋，右键收尾时
            //    若光标位置还没落成顶点（POLY_RUBBER 有值），要把那个点也收进去，
            //    否则用户看到的最后一段会凭空消失。
            if CURRENT_POLY.len() >= 1 {
                if let Some(r) = POLY_RUBBER {
                    let last = CURRENT_POLY.last().copied();
                    let dup = last.map(|l| (l.x - r.x).abs() < 1.0 && (l.y - r.y).abs() < 1.0)
                        .unwrap_or(false);
                    if !dup {
                        CURRENT_POLY.push(r);
                    }
                }
                let idx = {
                    let mut st = (*state_ptr).borrow_mut();
                    finalize_poly(&mut st);
                    st.tool = Tool::Select;
                    st.shapes.len().checked_sub(1)
                };
                POLY_RUBBER = None;
                SELECTED = idx;
                repaint(hwnd);
                return LRESULT(0);
            }
            let tool = (*state_ptr).borrow().tool;
            // 文字 / 水印：输入框还开着时，右键 = 提交并关闭输入框、退出工具，
            // 不要留着输入框、也不要再弹出一个新的。
            // 水印额外：退出时把水印 shape 一起删掉（水印铺满全屏，右键即视为
            // 「在水印区域上点右键」→ 退出并删除）。
            if tool == Tool::Text || tool == Tool::Watermark {
                if crate::textedit::is_typing() {
                    crate::textedit::commit_open();
                }
                {
                    let mut st = (*state_ptr).borrow_mut();
                    st.tool = Tool::Select;
                    CURRENT_DRAWING = None;
                    DRAW_START = None;
                    if tool == Tool::Watermark {
                        st.shapes
                            .retain(|s| !matches!(s, Shape::Watermark { .. }));
                        crate::dbg::log("右键退出水印 已删除水印");
                    }
                }
                SELECTED = last_selectable(state_ptr);
                repaint(hwnd);
                return LRESULT(0);
            }
            match tool {
                // 2) 已经在「选择/编辑」态：只清掉选中框，画布和画好的图案全部留着。
                Tool::None | Tool::Select => {
                    if SELECTED.is_some() {
                        SELECTED = None;
                        repaint(hwnd);
                    } else {
                        keep_canvas_exit();
                    }
                }
                // 3) 聚光灯 / 放大镜：右键退出工具，并把那个圆删掉（不再保留）。
                Tool::Spotlight | Tool::Magnifier => {
                    {
                        let mut st = (*state_ptr).borrow_mut();
                        st.tool = Tool::Select;
                        CURRENT_DRAWING = None;
                        DRAW_START = None;
                        let removed = st
                            .shapes
                            .iter()
                            .rposition(|s| match tool {
                                Tool::Spotlight => matches!(s, Shape::Spotlight { .. }),
                                _ => matches!(s, Shape::Magnifier { .. }),
                            })
                            .map(|i| {
                                st.shapes.remove(i);
                                true
                            })
                            .unwrap_or(false);
                        crate::dbg::log(&format!("右键退出{:?} 删除圆={removed}", tool));
                    }
                    select_none();
                    crate::cursor::apply(Tool::Select);
                    repaint(hwnd);
                }
                // 4) 正在用其它工具：退出该工具 → 进入选择/编辑态，
                //    并自动选中刚画好的图元，右键之后立刻就能拖动它。
                _ => {
                    {
                        let mut st = (*state_ptr).borrow_mut();
                        st.tool = Tool::Select;
                        CURRENT_DRAWING = None;
                        DRAW_START = None;
                    }
                    SELECTED = last_selectable(state_ptr);
                    repaint(hwnd);
                }
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn point_of(lparam: LPARAM) -> Point {
    let x = (lparam.0 & 0xFFFF) as i16 as f32;
    let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;
    Point::new(x, y)
}

/// Shift 是否按着（画矩形/圆时用来锁成正方形/正圆）
fn shift_down() -> bool {
    unsafe { (GetKeyState(VK_SHIFT.0 as i32) as u16 & 0x8000) != 0 }
}

// ---------- 重绘 ----------

/// 把单个图形合成到帧缓冲（处理半透明 + 抗锯齿边缘）
fn render_one(f: &Frame, sh: &Shape) {
    match sh {
        Shape::Freehand { color, alpha, .. } if *alpha < 255 => {
            let _ = Renderer::with_override(f.dc(), MAGIC).draw_shape(sh, f.w, f.h);
            let bb = sh.bbox();
            f.convert_magic(
                (bb.x, bb.y, bb.w, bb.h),
                (color.red(), color.green(), color.blue()),
                *alpha,
            );
        }
        Shape::Watermark { color, .. } => {
            // 水印原来给 110 的半透明，铺在白底或深色画面上都发灰看不清，
            // 现在按不透明画（alpha 255）。
            let _ = Renderer::with_override(f.dc(), MAGIC).draw_shape(sh, f.w, f.h);
            f.convert_magic(
                (0.0, 0.0, f.w as f32, f.h as f32),
                (color.red(), color.green(), color.blue()),
                255,
            );
        }
        Shape::Spotlight { center, radius } => {
            Renderer::with_override(f.dc(), MAGIC)
                .spotlight_dim(*center, *radius, f.w, f.h);
            f.convert_magic((0.0, 0.0, f.w as f32, f.h as f32), (0, 0, 0), 150);
            Renderer::new(f.dc()).spotlight_border(*center, *radius);
        }
        _ => {
            // 用 GDI+ 抗锯齿画出来的图形，需要把边缘的混色还原成半透明，
            // 否则那些像素会被 fix_alpha 一律提成不透明，看着就是一圈黑边
            if let Some((bb, c)) = Renderer::new(f.dc()).draw_shape(sh, f.w, f.h) {
                f.unmix(
                    (bb.x, bb.y, bb.w, bb.h),
                    (c.red(), c.green(), c.blue()),
                );
            }
        }
    }
}

pub fn repaint(hwnd: HWND) {
    unsafe {
        if FRAME_PTR == 0 || hwnd.is_invalid() {
            crate::dbg::log("repaint 跳过：FRAME_PTR=0 或窗口无效");
            return;
        }
        let f = &*(FRAME_PTR as *const Frame);
        // 穿透态：背景像素 alpha 清成 0（真透明），配合 WS_EX_TRANSPARENT 让
        // 空白区域点击穿透；正常绘图态：背景 alpha=1，接住所有鼠标事件。
        if CLICK_THROUGH {
            f.clear_transparent();
        } else {
            f.clear();
        }

        let state_ptr = st_ptr();
        if !state_ptr.is_null() {
            let st = (*state_ptr).borrow();
            for sh in &st.shapes {
                render_one(f, sh);
            }
            // 选中框
            if let Some(idx) = SELECTED {
                if idx < st.shapes.len() {
                    match &st.shapes[idx] {
                        // 圆形图元只画圆周上的 4 个圆点，不套外接方框
                        Shape::Spotlight { center, radius }
                        | Shape::Magnifier { center, radius, .. } => {
                            Renderer::new(f.dc())
                                .selection_circle(center.x, center.y, *radius);
                        }
                        other => Renderer::new(f.dc()).selection_box(other.bbox()),
                    }
                }
            }
            // 马赛克虚线框：只在「马赛克工具 + 激活状态」下显示，
            // 它就是拖边框缩放的抓手
            if st.tool == Tool::Mosaic
                && ACTIVE_MOSAIC != usize::MAX
                && ACTIVE_MOSAIC < st.shapes.len()
            {
                if let Shape::Mosaic { rect, .. } = &st.shapes[ACTIVE_MOSAIC] {
                    Renderer::new(f.dc()).selection_box(*rect);
                }
            }
        }

        // 正在绘制的图形（实时预览）
        if let Some(sh) = &CURRENT_DRAWING {
            render_one(f, sh);
        }
        if CURRENT_POLY.len() >= 1 {
            // 预览把「光标当前位置」也接上去，形成橡皮筋；
            // 颜色/线宽取当前设置，所见即所得
            let mut pts = CURRENT_POLY.clone();
            if let Some(r) = POLY_RUBBER {
                pts.push(r);
            } else if pts.len() == 1 {
                pts.push(pts[0]);
            }
            let (c, w) = if state_ptr.is_null() {
                (Color::rgb(255, 77, 79), 4.0)
            } else {
                let st = (*state_ptr).borrow();
                (st.color, st.width)
            };
            let preview = Shape::Polyline {
                points: pts,
                color: c,
                width: w,
            };
            render_one(f, &preview);
            // 顶点小标记，方便看"下一点从哪出发"
            if let Some(last) = CURRENT_POLY.last() {
                let b = CreateSolidBrush(COLORREF(
                    ((c.blue() as u32) << 16) | ((c.green() as u32) << 8) | c.red() as u32,
                ));
                let d = 8.0;
                let old = SelectObject(f.dc(), HGDIOBJ(b.0));
                let op = SelectObject(f.dc(), GetStockObject(NULL_PEN));
                Ellipse(
                    f.dc(),
                    (last.x - d / 2.0) as i32,
                    (last.y - d / 2.0) as i32,
                    (last.x + d / 2.0) as i32,
                    (last.y + d / 2.0) as i32,
                );
                SelectObject(f.dc(), op);
                SelectObject(f.dc(), old);
                let _ = DeleteObject(HGDIOBJ(b.0));
            }
        }

        f.fix_alpha();
        f.present(hwnd);
    }
    // 画布和工具条都在 TOPMOST 带里，任何一次重排（比如文字框被销毁）
    // 都可能把画布顶到工具条上面，导致工具条按钮点不动。
    // 每次重绘都把工具条提回去：位置尺寸都没变，代价很小。
    raise_toolbar();
}

/// 滚轮缩放聚光灯半径（由全局滚轮钩子在 Spotlight 工具下调用）。
/// 优先缩放选中的那个，没选中就缩放最后画的那个。
pub fn zoom_spotlight(up: bool) {
    let sp = st_ptr();
    if sp.is_null() {
        return;
    }
    let mut changed = false;
    {
        // 借用必须在 repaint 之前结束，否则 repaint 里的 borrow 会撞车
        let mut st = unsafe { (*sp).borrow_mut() };
        let sel = unsafe { SELECTED }.filter(|i| {
            matches!(st.shapes.get(*i), Some(Shape::Spotlight { .. }))
        });
        let idx = sel.or_else(|| {
            st.shapes
                .iter()
                .rposition(|s| matches!(s, Shape::Spotlight { .. }))
        });
        if let Some(i) = idx {
            if let Some(Shape::Spotlight { radius, .. }) = st.shapes.get_mut(i) {
                let k = if up { SPOT_ZOOM } else { 1.0 / SPOT_ZOOM };
                *radius = (*radius * k).clamp(SPOT_MIN, SPOT_MAX);
                crate::dbg::log(&format!("滚轮 聚光半径 r={:.0}", radius));
                changed = true;
            }
        }
    }
    if changed {
        repaint_global();
    }
}

/// 让已铺的水印跟随「当前颜色 + 粗细滑块」。
///
/// 只在水印工具下生效：其它工具改颜色时不应该把之前画好的水印也改掉。
/// 这里不重绘，调用方（工具条）本来就会紧接着重绘一次。
pub fn sync_watermark() {
    let sp = st_ptr();
    if sp.is_null() {
        return;
    }
    let mut st = unsafe { (*sp).borrow_mut() };
    if st.tool != Tool::Watermark {
        return;
    }
    let color = st.color;
    let size = watermark_size(st.width);
    let mut n = 0;
    for sh in st.shapes.iter_mut() {
        if let Shape::Watermark {
            color: c, size: sz, ..
        } = sh
        {
            *c = color;
            *sz = size;
            n += 1;
        }
    }
    // 只在开了 SM_LOG 时写文件，方便自动化验证「颜色/字号确实传到了水印上」
    crate::dbg::log(&format!(
        "水印同步 个数={n} color=({},{},{}) size={size:.1}",
        color.red(),
        color.green(),
        color.blue()
    ));
}

/// 供其它模块调用：重新把工具条提回画布之上。
/// 典型场景：文字框（TOPMOST）被销毁时，Windows 会重排 TOPMOST 带，
/// 画布有可能被顶到工具条上面，必须在这之后再提一次。
pub fn reassert_toolbar() {
    raise_toolbar();
}

/// 把工具条重新提到画布之上。
/// 画布被点一下会被「激活」，激活会在 TOPMOST 组里把画布顶到最前面，
/// 于是工具条被盖住、按钮点不动。除了给画布加 WS_EX_NOACTIVATE，
/// 这里再补一道保险。
fn raise_toolbar() {
    unsafe {
        let tb = HWND(CANVAS_TOOLBAR as *mut _);
        let cv = hwnd_of();
        if tb.is_invalid() || cv.is_invalid() || !IsWindowVisible(cv).as_bool() {
            return;
        }
        // 之前是「把工具条提到 HWND_TOPMOST」，但对已经 topmost 的窗口
        // 这个调用不一定真的重排（画布仍可能压在工具条上）。
        // 改成把画布主动插到工具条后面 —— 相对顺序是确定的。
        SetWindowPos(
            cv,
            tb,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

pub fn repaint_global() {
    let h = hwnd_of();
    if !h.is_invalid() && unsafe { IsWindowVisible(h).as_bool() } {
        repaint(h);
    }
}

/// 由 main 在工具条创建后调用，建立「画布在工具条之下」的 Z 序
pub fn set_toolbar(hwnd: HWND) {
    unsafe {
        CANVAS_TOOLBAR = hwnd.0 as usize;
    }
}

pub fn show_canvas(hwnd: HWND) {
    unsafe {
        ShowWindow(hwnd, SW_SHOW);
        let toolbar = HWND(CANVAS_TOOLBAR as *mut _);
        if !toolbar.is_invalid() {
            // 把画布插到工具条后面（即 Z 序更低），保证工具条始终可点
            SetWindowPos(
                hwnd,
                toolbar,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
        } else {
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
    }
    repaint(hwnd);
}

pub fn hide_canvas(hwnd: HWND) {
    unsafe {
        ShowWindow(hwnd, SW_HIDE);
    }
}

// ---------- 绘制逻辑 ----------

fn begin_draw(st: &mut AppState, p: Point) {
    unsafe {
        DRAW_START = Some(p);
        CURRENT_DRAWING = match st.tool {
            Tool::Pen => Some(Shape::Freehand {
                points: vec![p],
                color: st.color,
                width: st.width,
                alpha: 255,
            }),
            Tool::Highlighter => Some(Shape::Freehand {
                points: vec![p],
                color: st.color,
                width: st.width * 2.5,
                alpha: 110,
            }),
            Tool::Rectangle => Some(Shape::Rect {
                rect: Rect {
                    x: p.x,
                    y: p.y,
                    w: 0.0,
                    h: 0.0,
                },
                color: st.color,
                width: st.width,
            }),
            Tool::Ellipse => Some(Shape::Ellipse {
                rect: Rect {
                    x: p.x,
                    y: p.y,
                    w: 0.0,
                    h: 0.0,
                },
                color: st.color,
                width: st.width,
            }),
            Tool::Arrow => Some(Shape::Arrow {
                p1: p,
                p2: p,
                color: st.color,
                width: st.width,
            }),
            Tool::Wavy => Some(Shape::Wavy {
                p1: p,
                p2: p,
                color: st.color,
                width: st.width,
            }),
            Tool::Spotlight => Some(Shape::Spotlight {
                center: p,
                // 一点下去就是个完整的圆，不用拖；之后滚轮调大小
                radius: SPOT_DEFAULT,
            }),
            Tool::Magnifier => None, // 放大镜在 LBUTTONDOWN 里直接落图形
            Tool::Mosaic => Some(Shape::Mosaic {
                rect: Rect {
                    x: p.x,
                    y: p.y,
                    w: 0.0,
                    h: 0.0,
                },
                pixmap: None,
                pixmap_w: 0,
                pixmap_h: 0,
            }),
            Tool::Number => {
                st.number_counter += 1;
                let num = st.number_counter;
                st.shapes.push(Shape::Number {
                    pos: p,
                    number: num,
                    color: st.color,
                    radius: number_radius(st.width),
                });
                None
            }
            Tool::Watermark => {
                st.shapes.push(Shape::Watermark {
                    text: st.watermark_text.clone(),
                    color: st.color,
                    size: watermark_size(st.width),
                });
                None
            }
            _ => None,
        };
    }
}

fn update_draw(_st: &mut AppState, p: Point) {
    unsafe {
        if let Some(sh) = &mut CURRENT_DRAWING {
            match sh {
                Shape::Freehand { points, .. } => points.push(p),
                Shape::Rect { rect, .. } | Shape::Ellipse { rect, .. } => {
                    if let Some(start) = DRAW_START {
                        // 按住 Shift = 正方形 / 正圆：长短边取同一个长度，
                        // 方向跟着鼠标所在象限走，拖起来仍然顺手
                        *rect = if shift_down() {
                            let dx = p.x - start.x;
                            let dy = p.y - start.y;
                            let s = dx.abs().max(dy.abs());
                            let sx = if dx < 0.0 { -1.0 } else { 1.0 };
                            let sy = if dy < 0.0 { -1.0 } else { 1.0 };
                            Rect::from_points(
                                start,
                                Point::new(start.x + s * sx, start.y + s * sy),
                            )
                        } else {
                            Rect::from_points(start, p)
                        };
                    }
                }
                Shape::Mosaic { rect, .. } => {
                    if let Some(start) = DRAW_START {
                        *rect = Rect::from_points(start, p);
                    }
                }
                Shape::Arrow { p2, .. } | Shape::Wavy { p2, .. } => {
                    *p2 = p;
                }
                Shape::Spotlight { center, radius } => {
                    // 聚光灯一点下去就是 SPOT_DEFAULT 的圆，主要靠滚轮缩放。
                    // 这里保留「拖出去改半径」，但要求拖够距离才生效，
                    // 否则手抖几像素就把圆缩成一个小点，很难用。
                    let dx = p.x - center.x;
                    let dy = p.y - center.y;
                    let d = (dx * dx + dy * dy).sqrt();
                    if d > 30.0 {
                        *radius = d.clamp(SPOT_MIN, SPOT_MAX);
                    }
                }
                Shape::Magnifier { center, radius, .. } => {
                    // 建好后拖出去可以改半径；拖动改变位置在 wndproc 里处理
                    let dx = p.x - center.x;
                    let dy = p.y - center.y;
                    let d = (dx * dx + dy * dy).sqrt();
                    if d > 30.0 {
                        *radius = d.clamp(MAG_MIN_R, MAG_MAX_R);
                    }
                }
                _ => {}
            }
        }
    }
}

fn end_draw(st: &mut AppState, p: Point) {
    unsafe {
        let Some(mut sh) = CURRENT_DRAWING.take() else {
            DRAW_START = None;
            return;
        };
        DRAW_START = None;

        // 太小的图形视为误触，丢弃
        let keep = match &sh {
            Shape::Freehand { points, .. } => points.len() > 1,
            Shape::Rect { rect, .. } | Shape::Ellipse { rect, .. } => {
                rect.w > 3.0 && rect.h > 3.0
            }
            Shape::Mosaic { rect, .. } => rect.w > 6.0 && rect.h > 6.0,
            Shape::Arrow { p1, p2, .. } | Shape::Wavy { p1, p2, .. } => {
                let dx = p2.x - p1.x;
                let dy = p2.y - p1.y;
                (dx * dx + dy * dy).sqrt() > 5.0
            }
            Shape::Spotlight { radius, .. } | Shape::Magnifier { radius, .. } => *radius > 10.0,
            _ => true,
        };
        if !keep {
            return;
        }

        // 需要抓屏的图形：先把画布藏起来，避免把自己截进去
        match sh {
            Shape::Mosaic { rect, .. } => {
                hide_canvas(hwnd_of());
                let cap = capture_screen(
                    rect.x as i32,
                    rect.y as i32,
                    rect.w as i32,
                    rect.h as i32,
                );
                show_canvas(hwnd_of());
                if let Some((data, w, h)) = cap {
                    let block = (st.width * 2.0).max(8.0) as i32;
                    let px = pixelate(&data, w, h, block);
                    sh = Shape::Mosaic {
                        rect,
                        pixmap: Some(px),
                        pixmap_w: w,
                        pixmap_h: h,
                    };
                } else {
                    return;
                }
            }
            Shape::Magnifier { .. } => {
                // 放大镜不再抓局部图：内容是渲染时从整屏快照里实时取的，
                // 拖动圆或调倍率都能立刻反映，不会像以前那样"松手才出图"。
            }
            _ => {}
        }
        let _ = p;
        // 自动化验证用：把刚落定图形的几何写进日志（只在 SM_LOG 打开时）
        if std::env::var_os("SM_LOG").is_some() {
            match &sh {
                Shape::Rect { rect, .. } | Shape::Ellipse { rect, .. } => crate::dbg::log(&format!(
                    "几何 方框 {:.0}x{:.0} at ({:.0},{:.0})",
                    rect.w, rect.h, rect.x, rect.y
                )),
                Shape::Number { radius, .. } => {
                    crate::dbg::log(&format!("几何 序号 r={:.0}", radius))
                }
                _ => {}
            }
        }
        st.shapes.push(sh);
    }
}

fn finalize_poly(st: &mut AppState) {
    unsafe {
        if CURRENT_POLY.len() >= 2 {
            let points = std::mem::take(&mut CURRENT_POLY);
            st.shapes.push(Shape::Polyline {
                points,
                color: st.color,
                width: st.width,
            });
        } else {
            CURRENT_POLY.clear();
        }
        DRAW_START = None;
    }
}

// ---------- 选择 / 移动 / 缩放（右键退出工具后进入这个模式） ----------

/// 最上面一个可选中的图元下标（右键退出工具后自动选中它）
fn last_selectable(state_ptr: *const RefCell<AppState>) -> Option<usize> {
    if state_ptr.is_null() {
        return None;
    }
    let st = unsafe { (*state_ptr).borrow() };
    st.shapes.iter().rposition(|s| s.selectable())
}

/// 按下：判断这次是「缩放某个图元」「移动某个图元」还是「重新选中」。
/// 返回 true 表示点到了图形（开始一次拖动），false 表示点在空白处。
fn edit_begin(st: &mut AppState, p: Point) -> bool {
    // 1) 已经有选中的：先看是不是按在手柄上（手柄优先，因为手柄可能压到别的图元上）
    if let Some(idx) = unsafe { SELECTED } {
        if idx < st.shapes.len() {
            let bb = st.shapes[idx].bbox();
            // 圆形图元用圆周上的 4 个控制点，其它图形用外接框的 8 个手柄。
            // 手柄优先，且容差给得宽一点（10px 的方块 + 8px 余量）：
            // 实测 4px 时用户只要点偏 10px 就会变成「整体移动」，很难抓到角点。
            let mut mask = match &st.shapes[idx] {
                Shape::Spotlight { center, radius }
                | Shape::Magnifier { center, radius, .. } => {
                    crate::render::circle_handle_hit(center.x, center.y, *radius, p, 10.0)
                }
                _ => crate::render::handle_hit(bb, p, 8.0),
            };
            if mask == 0 && bb.contains(p, 0.0) {
                mask = EDIT_MOVE;
            }
            if mask != 0 {
                begin_edit_drag(st, idx, mask, p, bb);
                return true;
            }
        }
    }
    // 2) 没按在手柄/已选图元上 → 重新做一次命中测试
    unsafe { SELECTED = hit_shape(&st.shapes, p) };
    unsafe {
        EDIT_MASK = 0;
        EDIT_BASE = None;
        EDIT_IDX = usize::MAX;
    }
    match unsafe { SELECTED } {
        Some(idx) => {
            let bb = st.shapes[idx].bbox();
            begin_edit_drag(st, idx, EDIT_MOVE, p, bb);
            true
        }
        None => false,
    }
}

fn begin_edit_drag(st: &mut AppState, idx: usize, mask: u8, p: Point, bb: Rect) {
    unsafe {
        EDIT_IDX = idx;
        EDIT_MASK = mask;
        EDIT_P0 = Some(p);
        EDIT_BB0 = Some(bb);
        EDIT_BASE = Some(st.shapes[idx].clone());
        DRAG_LAST = Some(p);
    }
}

/// 拖动中：返回 true 表示图形变了（需要重绘）
fn edit_drag(st: &mut AppState, p: Point) -> bool {
    let mask = unsafe { EDIT_MASK };
    let idx = unsafe { EDIT_IDX };
    if mask == 0 || idx == usize::MAX || idx >= st.shapes.len() {
        return false;
    }
    // let-else 的初始化表达式不能以 `}` 结尾（语法歧义），所以整段用括号包起来
    let (Some(base), Some(bb0), Some(p0)) = (unsafe {
        (
            (*std::ptr::addr_of!(EDIT_BASE)).clone(),
            *std::ptr::addr_of!(EDIT_BB0),
            *std::ptr::addr_of!(EDIT_P0),
        )
    }) else {
        return false;
    };
    let mut sh = base;
    if mask == EDIT_MOVE {
        sh.translate(p.x - p0.x, p.y - p0.y);
    } else if let Shape::Spotlight { center, radius } = &mut sh {
        // 拖圆周上的控制点：圆心不动，半径跟着鼠标到圆心的距离走
        let d = ((p.x - center.x).powi(2) + (p.y - center.y).powi(2)).sqrt();
        *radius = d.clamp(SPOT_MIN, SPOT_MAX);
    } else if let Shape::Magnifier { center, radius, .. } = &mut sh {
        let d = ((p.x - center.x).powi(2) + (p.y - center.y).powi(2)).sqrt();
        *radius = d.clamp(MAG_MIN_R, MAG_MAX_R);
    } else {
        // 缩放：拖动哪条边就改哪条边，对角（或对边）保持不动当锚点
        let min = 8.0;
        let mut l = bb0.x;
        let mut r = bb0.right();
        let mut t = bb0.y;
        let mut b = bb0.bottom();
        if mask & 1 != 0 {
            l = p.x.min(r - min);
        }
        if mask & 2 != 0 {
            r = p.x.max(l + min);
        }
        if mask & 4 != 0 {
            t = p.y.min(b - min);
        }
        if mask & 8 != 0 {
            b = p.y.max(t + min);
        }
        let sx = ((r - l) / bb0.w.max(1.0)).max(0.01);
        let sy = ((b - t) / bb0.h.max(1.0)).max(0.01);
        let ax = if mask & 1 != 0 { bb0.right() } else { bb0.x };
        let ay = if mask & 4 != 0 { bb0.bottom() } else { bb0.y };
        sh.scale_about(Point::new(ax, ay), sx, sy);
    }
    st.shapes[idx] = sh;
    true
}

/// 松手：清掉拖动状态。若刚缩放的是马赛克，返回它的下标让调用方重新抓屏。
fn edit_end(st: &mut AppState) -> Option<usize> {
    let mask = unsafe { EDIT_MASK };
    let idx = unsafe { EDIT_IDX };
    unsafe {
        EDIT_MASK = 0;
        EDIT_IDX = usize::MAX;
        EDIT_P0 = None;
        EDIT_BB0 = None;
        EDIT_BASE = None;
    }
    if mask != 0 && mask != EDIT_MOVE && idx != usize::MAX {
        if matches!(st.shapes.get(idx), Some(Shape::Mosaic { .. })) {
            return Some(idx);
        }
    }
    None
}

// ---------- 放大镜 ----------

/// 抓一张整屏快照当放大镜的底片（藏画布 → 截图 → 显画布）
fn capture_snapshot() {
    let h = hwnd_of();
    if h.is_invalid() {
        return;
    }
    let sw = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let sh = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    hide_canvas(h);
    let cap = capture_screen(0, 0, sw, sh);
    if let Some((data, w, hh)) = cap {
        set_snapshot((data, w, hh));
        crate::dbg::log(&format!("放大镜快照 {w}x{hh}"));
    }
    // 快照先存好再显示画布，否则 show_canvas 里的重绘取不到样
    show_canvas(h);
}

/// 取消选中（清空/撤销后调用）
pub fn select_none() {
    unsafe {
        SELECTED = None;
        EDIT_MASK = 0;
        EDIT_IDX = usize::MAX;
        EDIT_BASE = None;
    }
}

/// 删除当前选中的图元（Del 键）。返回是否真的删了。
pub fn delete_selected() -> bool {
    let sp = st_ptr();
    if sp.is_null() {
        return false;
    }
    let removed = {
        let mut st = unsafe { (*sp).borrow_mut() };
        match unsafe { SELECTED } {
            Some(i) if i < st.shapes.len() => {
                st.shapes.remove(i);
                true
            }
            _ => false,
        }
    };
    if removed {
        select_none();
        repaint_global();
        crate::dbg::log("Del 删除选中图元");
    }
    removed
}

/// 「软退出」：退出当前功能、工具条收进主图标、焦点还给下面窗口，
/// 但**画布和已经画好的图案必须继续显示**。
///
/// 用在「右键退出工具后，左键点了空白处」以及「选择态下再点右键」。
/// 以前这些地方直接调 exit_markup()，它在工具条展开时会 SW_HIDE 整层画布，
/// 图形虽然还在 state 里，屏幕上却全没了 —— 用户看到的就是「图案消失了」。
/// 现在统一走这里：只收工具条、切回空工具态（None，画布里与 Select 一样
/// 支持点选/拖动），画面原样留在屏幕上，鼠标随后还能点选/拖动任意已画好的图元。
/// 用 None 而不是 Select：全局滚轮钩子靠 tool != None 判断「工具是否激活」，
/// 收纳态留着 Select 会让每格滚轮都偷偷调线宽并重绘整屏。
///
/// 同时进入「穿透态」：空白区域点击穿透到下面的程序，画了内容的像素仍拦截。
pub fn keep_canvas_exit() {
    let sp = st_ptr();
    if !sp.is_null() {
        let mut st = unsafe { (*sp).borrow_mut() };
        st.tool = Tool::None;
    }
    reset_transient();
    select_none();

    // 进入穿透态：空白区域点击直接穿透到下面的程序，
    // 画了内容的像素仍拦截鼠标（可点选/拖动图元）。
    unsafe {
        CLICK_THROUGH = true;
    }

    let h = hwnd_of();
    // 画布保持可见：这是「图案还保留显示」的关键，绝不 hide_canvas。
    // 若之前被某个手势藏过，这里也把它重新亮出来。
    if !h.is_invalid() && !unsafe { IsWindowVisible(h).as_bool() } {
        show_canvas(h);
    }

    crate::cursor::apply(Tool::Select);
    // 展开态：collapse_to_icon() 内部已经顺带 release_focus()，不要重复调
    // （release_focus 会临时 hide/show 画布，重复一次会明显闪一下）。
    // 已经是收纳态时它什么也不做，这里补一次焦点释放。
    let was_expanded = crate::toolbar::is_expanded();
    crate::toolbar::collapse_to_icon();
    if !was_expanded {
        release_focus();
    }
    repaint_global();
}

/// 工具条切到「选择/绘制工具」时调用：退出穿透态，让画布重新接收鼠标。
/// on=true 恢复穿透，on=false 关闭穿透（重新开始标注）。
pub fn set_click_through(on: bool) {
    unsafe {
        if CLICK_THROUGH != on {
            CLICK_THROUGH = on;
            repaint_global();
        }
    }
}

/// 当前是否处于穿透态（退出工具、保留画面、空白区域点击穿透）。
/// 全局空格快捷键据此让路：穿透态下用户已经回到正常使用电脑，
/// 空格不该再触发工具条。
pub fn is_click_through() -> bool {
    unsafe { CLICK_THROUGH }
}

/// 退出当前功能。
///
/// 两种情形：
///   * 工具条是展开的 —— 整体退出标注：清工具 → 隐藏画布 → 工具条收进主图标 → 焦点还给下面的窗口。
///   * 工具条已经收纳 —— 只退出「当前工具」，画布和已经画好的内容全部留着，
///     工具切到「选择」，鼠标可以直接点选/拖动屏幕上的其它图元。
///
/// 图形本身永远保留在 state 里，下次选工具会原样显示；这不是"清空"。
/// 注意：Esc / 整体退出走这里；左键点空白等「不画了但还要看画面」的手势
/// 走 keep_canvas_exit()。
pub fn exit_markup() {
    let sp = st_ptr();
    let collapsed = !crate::toolbar::is_expanded();
    if collapsed {
        // 收纳态：保留画面，工具切成 None（画布里 None 与 Select 一样可点选/拖动，
        // 但不会让全局滚轮钩子误以为「还有工具在激活」）。
        if !sp.is_null() {
            let mut st = unsafe { (*sp).borrow_mut() };
            st.tool = Tool::None;
        }
        reset_transient();
        select_none();
        // 收纳态退出也进入穿透态：空白区域穿透，画面保留
        unsafe {
            CLICK_THROUGH = true;
        }
        repaint_global();
        return;
    }
    if !sp.is_null() {
        let mut st = unsafe { (*sp).borrow_mut() };
        st.tool = Tool::None;
    }
    reset_transient();
    let h = hwnd_of();
    if !h.is_invalid() {
        hide_canvas(h);
    }
    // 整体退出标注后，空格快捷键也休眠：用户已经回到正常使用，
    // 去别的程序打字时空格不该再触发工具条。重新点主图标展开工具条
    // 选工具时会自动关掉穿透态、恢复空格。
    unsafe {
        CLICK_THROUGH = true;
    }
    // 焦点还给光标底下的窗口。落点如果还是我们自己的窗口（工具条/画布）就别抢。
    unsafe {
        let mut pt = POINT::default();
        GetCursorPos(&mut pt);
        let under = WindowFromPoint(pt);
        let tb = HWND(CANVAS_TOOLBAR as *mut _);
        if !under.is_invalid() && under != h && under != tb {
            let _ = SetForegroundWindow(under);
        }
    }
    // 工具条收缩成主图标（顺便把光标换回箭头）
    crate::cursor::apply(Tool::None);
    crate::toolbar::collapse_to_icon();
    repaint_global();
}

/// 清掉「画到一半」的各种临时状态（不动已落定的图元）
fn reset_transient() {
    unsafe {
        CURRENT_DRAWING = None;
        CURRENT_POLY.clear();
        POLY_RUBBER = None;
        DRAW_START = None;
        DRAG_LAST = None;
        EDIT_MASK = 0;
        EDIT_BASE = None;
        EDIT_IDX = usize::MAX;
        MAG_DRAG = false;
        MAG_RESIZE = false;
        SPOT_DRAG = false;
        SPOT_RESIZE = false;
    }
}

/// 把键盘焦点还给「光标下面那个真正属于别人的窗口」。
///
/// 工具条/画布都是 WS_EX_NOACTIVATE，正常情况下不会抢前台；但文字输入框
/// 是普通窗口，它关掉之后前台有可能还留在我方。空格收纳工具条时调一次，
/// 保证接下来的输入落到用户正在用的程序里（而不是被我们的窗口吃掉）。
pub fn release_focus() {
    unsafe {
        let h = hwnd_of();
        let fg = GetForegroundWindow();
        if fg.is_invalid() {
            return;
        }
        let mut fg_pid = 0u32;
        GetWindowThreadProcessId(fg, Some(&mut fg_pid));
        let mut my_pid = 0u32;
        if !h.is_invalid() {
            GetWindowThreadProcessId(h, Some(&mut my_pid));
        }
        if my_pid == 0 || fg_pid != my_pid {
            return; // 前台本来就不是我们，别乱动
        }
        // 画布铺满全屏，WindowFromPoint 只会返回它自己：
        // 先收起画布问一次，问完立刻恢复（分层窗口的隐藏/显示都很快）
        let vis = IsWindowVisible(h).as_bool();
        if vis {
            ShowWindow(h, SW_HIDE);
        }
        let mut pt = POINT::default();
        GetCursorPos(&mut pt);
        let under = WindowFromPoint(pt);
        let tb = HWND(CANVAS_TOOLBAR as *mut _);
        if !under.is_invalid() && under != h && under != tb {
            let _ = SetForegroundWindow(under);
        }
        if vis {
            show_canvas(h);
        }
    }
}

/// 滚轮调放大倍率：优先选中的那个，没选中就最后一个
/// 滚轮调放大镜的取景范围（半径）。倍率另由 Ctrl+滚轮调（zoom_magnifier）。
pub fn resize_magnifier(up: bool) {
    let sp = st_ptr();
    if sp.is_null() {
        return;
    }
    let mut changed = false;
    {
        let mut st = unsafe { (*sp).borrow_mut() };
        let sel = unsafe { SELECTED }
            .filter(|i| matches!(st.shapes.get(*i), Some(Shape::Magnifier { .. })));
        let idx = sel.or_else(|| {
            st.shapes
                .iter()
                .rposition(|s| matches!(s, Shape::Magnifier { .. }))
        });
        if let Some(i) = idx {
            if let Some(Shape::Magnifier { radius, .. }) = st.shapes.get_mut(i) {
                let k = if up { 1.10 } else { 1.0 / 1.10 };
                *radius = (*radius * k).clamp(MAG_MIN_R, MAG_MAX_R);
                crate::dbg::log(&format!("滚轮 放大半径 r={:.0}", radius));
                changed = true;
            }
        }
    }
    if changed {
        repaint_global();
    }
}

/// 当前选中的圆形图元类型：0 = 没有，1 = 放大镜，2 = 聚光灯。
/// 滚轮要靠它决定「调范围还是调倍率」。
pub fn selected_circle() -> u8 {
    unsafe {
        let Some(i) = SELECTED else {
            return 0;
        };
        let sp = st_ptr();
        if sp.is_null() {
            return 0;
        }
        match (*sp).borrow().shapes.get(i) {
            Some(Shape::Magnifier { .. }) => 1,
            Some(Shape::Spotlight { .. }) => 2,
            _ => 0,
        }
    }
}

/// 滚轮调放大镜的放大倍率（Ctrl + 滚轮）
pub fn zoom_magnifier(up: bool) {
    let sp = st_ptr();
    if sp.is_null() {
        return;
    }
    let mut changed = false;
    {
        let mut st = unsafe { (*sp).borrow_mut() };
        let sel = unsafe { SELECTED }
            .filter(|i| matches!(st.shapes.get(*i), Some(Shape::Magnifier { .. })));
        let idx = sel.or_else(|| {
            st.shapes
                .iter()
                .rposition(|s| matches!(s, Shape::Magnifier { .. }))
        });
        if let Some(i) = idx {
            if let Some(Shape::Magnifier { zoom, .. }) = st.shapes.get_mut(i) {
                let k = if up { MAG_ZOOM_STEP } else { 1.0 / MAG_ZOOM_STEP };
                *zoom = (*zoom * k).clamp(MAG_ZOOM_MIN, MAG_ZOOM_MAX);
                crate::dbg::log(&format!("滚轮 放大倍率 z={:.2}", zoom));
                changed = true;
            }
        }
    }
    if changed {
        repaint_global();
    }
}

/// 选中的马赛克用滚轮改大小（马赛克工具下改的是当前激活的那个框）。
///
/// 缩放后必须重抓一次像素：马赛克是「截图 + 打码」的静态位图，
/// 只改矩形不重抓的话，图会被拉伸得一片糊。
pub fn resize_mosaic(up: bool) -> bool {
    let sp = st_ptr();
    if sp.is_null() {
        return false;
    }
    let idx = {
        let st = unsafe { (*sp).borrow() };
        let sel = (unsafe { SELECTED }).filter(|i| matches!(st.shapes.get(*i), Some(Shape::Mosaic { .. })));
        let am = unsafe { ACTIVE_MOSAIC };
        let active = if am != usize::MAX && matches!(st.shapes.get(am), Some(Shape::Mosaic { .. }))
        {
            Some(am)
        } else {
            None
        };
        match sel {
            Some(i) => Some(i),
            None if st.tool == Tool::Mosaic => active,
            None => None,
        }
    };
    let Some(idx) = idx else {
        return false;
    };

    let k = if up { 1.12 } else { 1.0 / 1.12 };
    let (sw, sh) = unsafe {
        (
            GetSystemMetrics(SM_CXSCREEN) as f32,
            GetSystemMetrics(SM_CYSCREEN) as f32,
        )
    };
    {
        let mut st = unsafe { (*sp).borrow_mut() };
        if let Some(Shape::Mosaic { rect, .. }) = st.shapes.get_mut(idx) {
            let cx = rect.center().x;
            let cy = rect.center().y;
            let w = (rect.w * k).clamp(MZ_MIN, sw);
            let h = (rect.h * k).clamp(MZ_MIN, sh);
            rect.w = w;
            rect.h = h;
            rect.x = (cx - w / 2.0).clamp(0.0, (sw - w).max(0.0));
            rect.y = (cy - h / 2.0).clamp(0.0, (sh - h).max(0.0));
            crate::dbg::log(&format!(
                "滚轮 马赛克尺寸 {:.0}x{:.0}",
                rect.w, rect.h
            ));
        }
    }
    // 重新抓屏打码（内部会 hide/show 画布并重绘）
    mosaic_refresh(idx);
    true
}

/// 当前选中图元的类型：0 普通/无，1 放大镜，2 聚光灯，3 序号，4 马赛克。
/// 滚轮要根据它决定「调范围 / 调倍率 / 调大小」。
pub fn selected_kind() -> u8 {
    unsafe {
        let Some(i) = SELECTED else {
            return 0;
        };
        let sp = st_ptr();
        if sp.is_null() {
            return 0;
        }
        match (*sp).borrow().shapes.get(i) {
            Some(Shape::Magnifier { .. }) => 1,
            Some(Shape::Spotlight { .. }) => 2,
            Some(Shape::Number { .. }) => 3,
            Some(Shape::Mosaic { .. }) => 4,
            _ => 0,
        }
    }
}

/// 选中的普通图形用滚轮等比缩放（矩形/圆/箭头/折线/波浪/自由手绘/文字）。
/// 以图形中心为锚点，乘性缩放，改完立即重绘。
pub fn zoom_selected(up: bool) -> bool {
    let sp = st_ptr();
    if sp.is_null() {
        return false;
    }
    let Some(i) = (unsafe { SELECTED }) else {
        return false;
    };
    let mut done = false;
    {
        let mut st = unsafe { (*sp).borrow_mut() };
        if i < st.shapes.len() {
            // 圆形图元（放大镜/聚光灯/序号）有各自专门的滚轮语义，
            // 这里只处理「普通几何图形」；水印不可选中，自然到不了这。
            let is_circle = matches!(
                st.shapes[i],
                Shape::Spotlight { .. }
                    | Shape::Magnifier { .. }
                    | Shape::Number { .. }
                    | Shape::Mosaic { .. }
            );
            if !is_circle {
                let bb = st.shapes[i].bbox();
                let c = bb.center();
                let k = if up { 1.10 } else { 1.0 / 1.10 };
                st.shapes[i].scale_about(c, k, k);
                crate::dbg::log(&format!("滚轮 选中图元缩放 k={k:.3}"));
                done = true;
            }
        }
    }
    if done {
        repaint_global();
    }
    done
}

/// 滚轮总入口（由全局滚轮钩子调用）。返回 true 表示已经被图元消费掉，
/// 不再去调线宽；返回 false 则由调用方走「调线宽/字号」。
///
/// 分工：
///   放大镜        —— 滚轮 = 取景范围，Ctrl+滚轮 = 放大倍率
///   聚光灯        —— 滚轮 = 半径
///   马赛克        —— 滚轮 = 区域大小（重抓打码）
///   序号          —— 滚轮 = 字号/圈的大小（走线宽，再由 sync_number 派生）
///   选择态        —— 看选中的是什么：圆形图元套用上面同一套，
///                   普通图形（矩形/圆/箭头/折线/波浪/手绘/文字）等比缩放
pub fn handle_wheel(up: bool, ctrl: bool) -> bool {
    let sp = st_ptr();
    if sp.is_null() {
        return false;
    }
    let tool = unsafe { (*sp).borrow().tool };
    let kind = match tool {
        Tool::Magnifier => 1,
        Tool::Spotlight => 2,
        Tool::Mosaic => 4,
        Tool::Select => selected_kind(),
        _ => 0,
    };
    match kind {
        1 => {
            if ctrl {
                zoom_magnifier(up);
            } else {
                resize_magnifier(up);
            }
            true
        }
        2 => {
            zoom_spotlight(up);
            true
        }
        3 => resize_number(up),
        4 => resize_mosaic(up),
        // 选择态下选中的普通几何图形 → 等比缩放
        0 => {
            if tool == Tool::Select {
                zoom_selected(up)
            } else {
                false
            }
        }
        _ => false,
    }
}

/// 让已画的文字跟随「当前粗细滑块」的字号。
///
/// 只跟水印一样在「文字工具」下生效：别的工具改滑块不该把之前写好的字全变大小。
pub fn sync_text() {    let sp = st_ptr();
    if sp.is_null() {
        return;
    }
    let (fs, n) = {
        let mut st = unsafe { (*sp).borrow_mut() };
        if st.tool != Tool::Text {
            return;
        }
        let fs = text_size(st.width);
        let mut n = 0;
        for sh in st.shapes.iter_mut() {
            if let Shape::Text {
                rect,
                text,
                font_size,
                ..
            } = sh
            {
                *font_size = fs;
                // 字号变了外框也要跟着重算，否则字会溢出虚线框
                let (w, h) = measure_text_box(text, fs as i32, textedit::MIN_CHARS);
                rect.w = w;
                rect.h = h;
                n += 1;
            }
        }
        (fs, n)
    };
    crate::dbg::log(&format!("文字同步 个数={n} size={fs:.1}"));
    crate::textedit::retune();
}

/// 让已画的序号跟随「当前颜色 + 粗细滑块」。
///
/// 序号工具下改颜色/滚轮调粗细，屏幕上的序号要立刻一起变 —— 这样它就是
/// 所见即所得的「当前样式」；离开序号工具后只同步被选中的那一个的颜色，
/// 不会把之前画好的一批序号一起改掉。
pub fn sync_number() {
    let sp = st_ptr();
    if sp.is_null() {
        return;
    }
    let (tool, color, radius) = {
        let st = unsafe { (*sp).borrow() };
        (st.tool, st.color, number_radius(st.width))
    };
    let sel = unsafe { SELECTED };
    let mut st = unsafe { (*sp).borrow_mut() };
    if tool == Tool::Number {
        // 只同步「刚画的那一个」：颜色/滚轮是给手上这个序号用的。
        // 全部一起改的话，之前画的一批序号会被一次性刷成同一个颜色，
        // 想给不同位置标不同颜色的序号就做不到了。
        let last = st
            .shapes
            .iter()
            .rposition(|s| matches!(s, Shape::Number { .. }));
        if let Some(i) = last {
            if let Some(Shape::Number {
                color: c,
                radius: r,
                ..
            }) = st.shapes.get_mut(i)
            {
                *c = color;
                *r = radius;
            }
        }
    } else if let Some(i) = sel {
        if let Some(Shape::Number { color: c, .. }) = st.shapes.get_mut(i) {
            *c = color;
        }
    }
    crate::dbg::log(&format!(
        "序号同步 color=({},{},{}) r={radius:.0}",
        color.red(),
        color.green(),
        color.blue()
    ));
}

/// 选中的序号用滚轮调大小（选择态下生效）
pub fn resize_number(up: bool) -> bool {
    let sp = st_ptr();
    if sp.is_null() {
        return false;
    }
    let Some(i) = (unsafe { SELECTED }) else {
        return false;
    };
    let mut st = unsafe { (*sp).borrow_mut() };
    let mut done = false;
    if let Some(Shape::Number { radius, .. }) = st.shapes.get_mut(i) {
        let k = if up { 1.12 } else { 1.0 / 1.12 };
        *radius = (*radius * k).clamp(10.0, 400.0);
        crate::dbg::log(&format!("滚轮 序号半径 r={:.0}", radius));
        done = true;
    }
    drop(st);
    if done {
        repaint_global();
    }
    done
}

/// 收掉还没落定的折线（切工具时调用，免得半截折线挂在画面上）
pub fn finalize_pending() {
    let sp = st_ptr();
    if sp.is_null() {
        return;
    }
    let mut st = unsafe { (*sp).borrow_mut() };
    unsafe {
        POLY_RUBBER = None;
    }
    finalize_poly(&mut st);
}

fn erase_at(st: &mut AppState, p: Point) {
    let r = (st.width * 2.0).max(12.0);
    let mut i = 0;
    while i < st.shapes.len() {
        if st.shapes[i].erasable() && st.shapes[i].hit(p, r) {
            st.shapes.remove(i);
        } else {
            i += 1;
        }
    }
}

fn hit_shape(shapes: &[Shape], p: Point) -> Option<usize> {
    for i in (0..shapes.len()).rev() {
        if shapes[i].selectable() && shapes[i].hit(p, 6.0) {
            return Some(i);
        }
    }
    None
}

/// 新建文字框（默认宽 = 10 个字），返回 (索引, 位置) 供随后打开输入框
fn create_text(st: &mut AppState, p: Point) -> Option<(usize, Rect)> {
    unsafe {
        let font_size = text_size(st.width);
        let (w, h) = measure_text_box("", font_size as i32, textedit::MIN_CHARS);
        // 别让框跑出屏幕
        let sw = GetSystemMetrics(SM_CXSCREEN) as f32;
        let sh = GetSystemMetrics(SM_CYSCREEN) as f32;
        let x = p.x.min((sw - w).max(0.0)).max(0.0);
        let y = p.y.min((sh - h).max(0.0)).max(0.0);
        let rect = Rect { x, y, w, h };
        st.shapes.push(Shape::Text {
            rect,
            text: String::new(),
            color: st.color,
            font_size,
        });
        let idx = st.shapes.len() - 1;
        Some((idx, rect))
    }
}

// ---------- 马赛克框 ----------

/// 判断点落在矩形哪几条边上（0 = 不在边框上）
fn mosaic_handle(r: Rect, p: Point, tol: f32) -> u8 {
    let mut m = 0u8;
    let in_v = p.y >= r.y - tol && p.y <= r.bottom() + tol;
    let in_h = p.x >= r.x - tol && p.x <= r.right() + tol;
    if in_v && (p.x - r.x).abs() <= tol {
        m |= 1;
    }
    if in_v && (p.x - r.right()).abs() <= tol {
        m |= 2;
    }
    if in_h && (p.y - r.y).abs() <= tol {
        m |= 4;
    }
    if in_h && (p.y - r.bottom()).abs() <= tol {
        m |= 8;
    }
    m
}

/// 新建马赛克框：点一下就给一个 160×90 的虚线框，并立刻抓取该区域打码
fn mosaic_create(st: &mut AppState, p: Point) -> usize {
    unsafe {
        let sw = GetSystemMetrics(SM_CXSCREEN) as f32;
        let sh = GetSystemMetrics(SM_CYSCREEN) as f32;
        let w = MZ_DEFAULT.0.min(sw);
        let h = MZ_DEFAULT.1.min(sh);
        let x = p.x.min((sw - w).max(0.0)).max(0.0);
        let y = p.y.min((sh - h).max(0.0)).max(0.0);
        st.shapes.push(Shape::Mosaic {
            rect: Rect { x, y, w, h },
            pixmap: None,
            pixmap_w: 0,
            pixmap_h: 0,
        });
        let idx = st.shapes.len() - 1;
        ACTIVE_MOSAIC = idx;
        idx
    }
}

/// 按当前矩形重新抓屏打码。
///
/// 关键点：抓屏前必须把画布藏起来 —— 马赛克只存在于我们这个分层窗口里，
/// 窗口一藏，露出来的就是「没被马赛克污染」的真实画面，可以反复重抓。
fn mosaic_refresh(idx: usize) {
    let sp = st_ptr();
    if sp.is_null() {
        return;
    }
    let (rect, block) = {
        let st = unsafe { (*sp).borrow() };
        if idx >= st.shapes.len() {
            return;
        }
        match st.shapes[idx] {
            Shape::Mosaic { rect, .. } => (rect, (st.width * 2.0).max(8.0) as i32),
            _ => return,
        }
    };
    let (rx, ry) = (rect.x as i32, rect.y as i32);
    let (rw, rh) = (rect.w as i32, rect.h as i32);
    if rw < 2 || rh < 2 {
        return;
    }
    let h = hwnd_of();
    hide_canvas(h);
    let cap = capture_screen(rx, ry, rw, rh);
    // show_canvas 内部会 repaint（会 borrow state），所以必须等上面的借用结束
    show_canvas(h);
    if let Some((data, cw, ch)) = cap {
        let px = pixelate(&data, cw, ch, block);
        let mut st = unsafe { (*sp).borrow_mut() };
        if let Some(Shape::Mosaic {
            pixmap,
            pixmap_w,
            pixmap_h,
            ..
        }) = st.shapes.get_mut(idx)
        {
            *pixmap = Some(px);
            *pixmap_w = cw;
            *pixmap_h = ch;
        }
    }
}

/// 打开文字输入框（此时不能再持有 state 的借用）
fn open_text_editor(idx: usize, rect: Rect, initial: &str) {
    unsafe {
        let hinstance = HINSTANCE(CANVAS_HINSTANCE as *mut _);
        let state_rc = match (&*(std::ptr::addr_of!(STATE_RC))).as_ref() {
            Some(rc) => rc.clone(),
            None => return,
        };
        textedit::open(hinstance, rect, initial, idx, state_rc);
    }
}

/// 令第 idx 个图元处于选中态（文字提交后直接可拖动/缩放）
pub fn select_index(idx: usize) {
    let sp = st_ptr();
    if sp.is_null() {
        return;
    }
    let n = unsafe { (*sp).borrow().shapes.len() };
    unsafe {
        SELECTED = if idx < n { Some(idx) } else { None };
    }
}

/// 按当前 watermark_text / 颜色 / 字号，生成或更新「那一个」水印。
///
/// 水印是整屏平铺的，重复添加只会让画面越来越浓，所以始终只保留一份。
pub fn apply_watermark() {
    let sp = st_ptr();
    if sp.is_null() {
        return;
    }
    {
        let mut st = unsafe { (*sp).borrow_mut() };
        let text = st.watermark_text.clone();
        let color = st.color;
        let size = watermark_size(st.width);
        let exist = st
            .shapes
            .iter()
            .rposition(|s| matches!(s, Shape::Watermark { .. }));
        match exist {
            Some(i) => {
                if let Shape::Watermark {
                    text: t,
                    color: c,
                    size: s,
                } = &mut st.shapes[i]
                {
                    *t = text;
                    *c = color;
                    *s = size;
                }
            }
            None => st.shapes.push(Shape::Watermark { text, color, size }),
        }
        crate::dbg::log(&format!("水印应用 text={:?}", st.watermark_text));
    }
    repaint_global();
}

/// 弹出水印文字输入框（水印内容可自定义）。
///
/// 水印是整屏平铺的全局效果，一个屏幕上只需要一份，所以这里统一把关：
///   * 屏幕上已经有水印 → 不再弹输入框。点工具/点画布只是回到水印工具，
///     颜色和字号仍可通过工具条同步到已有水印（sync_watermark）；
///   * 想换文字 → 右键删掉水印，再点一次水印工具就会重新问；
///   * 输入框已经开着 → 也不重复弹，避免叠出第二个框。
pub fn edit_watermark() {
    unsafe {
        let sp = st_ptr();
        if !sp.is_null() {
            let has = (*sp)
                .borrow()
                .shapes
                .iter()
                .any(|s| matches!(s, Shape::Watermark { .. }));
            if has {
                crate::dbg::log("已有水印，跳过输入框");
                sync_watermark();
                repaint_global();
                return;
            }
        }
        if textedit::is_typing() {
            crate::dbg::log("已有输入框，跳过水印输入框");
            return;
        }
        crate::dbg::log("edit_watermark 进入");
        let hinstance = HINSTANCE(CANVAS_HINSTANCE as *mut _);
        let state_rc = match (&*(std::ptr::addr_of!(STATE_RC))).as_ref() {
            Some(rc) => rc.clone(),
            None => return,
        };
        let cur = {
            let sp = st_ptr();
            if sp.is_null() {
                "WATERMARK".to_string()
            } else {
                (*sp).borrow().watermark_text.clone()
            }
        };
        textedit::open_watermark(hinstance, &cur, state_rc);
    }
}

/// 画布窗口（句柄随进程存活，不做显式销毁）
pub struct Canvas {
    hwnd: HWND,
}

impl Canvas {
    pub fn new(hinstance: HINSTANCE, state: Rc<RefCell<AppState>>) -> Result<Self> {
        unsafe {
            let class_name = w!("ScreenMarkerCanvas");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(canvas_wndproc),
                hInstance: hinstance,
                hbrBackground: HBRUSH(ptr::null_mut()),
                hIcon: crate::appicon::load(),

                lpszClassName: class_name,
                ..Default::default()
            };
            RegisterClassW(&wc);

            CANVAS_STATE_PTR = Rc::as_ptr(&state) as *const RefCell<AppState> as usize;
            STATE_RC = Some(state.clone());
            CANVAS_HINSTANCE = hinstance.0 as usize;

            let sw = GetSystemMetrics(SM_CXSCREEN);
            let sh = GetSystemMetrics(SM_CYSCREEN);

            // 整屏一块 32 位 DIB 当帧缓冲，repaint 时靠 UpdateLayeredWindow 贴出去
            let f = match Frame::new(sw, sh) {
                Some(f) => f,
                None => return Err(Error::from_hresult(E_FAIL)),
            };
            FRAME_PTR = Box::into_raw(Box::new(f)) as usize;

            // NOACTIVATE：点了画布也不抢焦点，否则工具条与快捷键都会失灵。
            // 注意：不能加 WS_EX_TRANSPARENT —— 它会让整窗忽略形状、把**所有**
            // 鼠标事件穿透到下面（连画了内容的区域也无法点选）。逐像素穿透
            // 改由 WM_NCHITTEST 里按「命中像素 alpha」返回 HTTRANSPARENT 实现。
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                class_name,
                w!("ScreenMarkerCanvas"),
                WS_POPUP,
                0,
                0,
                sw,
                sh,
                None,
                None,
                hinstance,
                None,
            )?;
            CANVAS_HWND = hwnd.0 as usize;

            // 初始不显示：没选工具时屏幕上只留一条工具条
            Ok(Canvas { hwnd })
        }
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }
}
