// 「关于」小窗口：软件名 + 版本号 + GitHub 图标按钮（点击跳转项目页）。
//
// GitHub 图标由官方 Octocat 矢量路径经 GDI+ 直接绘制（见 ghlogo.rs），
// 不依赖任何图片文件。
//
// 所有尺寸都写成 96 DPI 下的「基准值」，实际绘制时乘以 SCALE，
// 这样在 125% / 150% 缩放下窗口和文字不会被系统拉伸变糊。

#![allow(dead_code)]

use std::ptr;

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Graphics::GdiPlus::{
    GdipCreateFromHDC, GdipDeleteGraphics, GdiplusStartup, GdiplusStartupInput, GpGraphics,
};
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::*;

// ---- 96 DPI 下的基准布局（客户区坐标） ----
const BASE_W: i32 = 320;
const BASE_H: i32 = 216;
const BASE_GH_SZ: i32 = 50; // 图标实心圆直径
const BASE_GH_Y: i32 = 108;
const BASE_NAME_Y: i32 = 26;
const BASE_NAME_H: i32 = 30;
const BASE_VER_Y: i32 = 58;
const BASE_VER_H: i32 = 22;
const BASE_DIV_Y: i32 = 94;
const BASE_HINT_H: i32 = 20;

const GITHUB_URL: &str = "https://github.com/niustark/screenmarker";

static mut ABOUT_HWND: usize = 0;
static mut GH_HOVER: bool = false;
static mut GH_DOWN: bool = false;
/// 当前窗口的 DPI 缩放系数（1.0 = 96 DPI）
static mut SCALE: f32 = 1.0;

/// GDI+ 是否已启动（全局一次性）
static mut GDI_TOKEN: usize = 0;

/// 把基准尺寸换算成当前 DPI 下的像素
fn sc(v: i32) -> i32 {
    unsafe { (v as f32 * SCALE).round() as i32 }
}

fn client_w() -> i32 {
    sc(BASE_W)
}

fn client_h() -> i32 {
    sc(BASE_H)
}

/// GitHub 图标按钮的矩形（水平居中）
fn gh_rect() -> RECT {
    let sz = sc(BASE_GH_SZ);
    rect((client_w() - sz) / 2, sc(BASE_GH_Y), sz, sz)
}

/// 确保 GDI+ 已初始化（幂等）
unsafe fn gdi_ready() -> bool {
    if GDI_TOKEN != 0 {
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
    GDI_TOKEN = token;
    true
}

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

fn font(px: i32, weight: i32) -> HFONT {
    unsafe {
        CreateFontW(
            -sc(px),
            0,
            0,
            0,
            weight,
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

/// 在目标矩形内绘制 GitHub 图标：官方矢量路径，GDI+ 直接填充。
unsafe fn draw_github(hdc: HDC, r: RECT) {
    if !gdi_ready() {
        return;
    }
    let mut g: *mut GpGraphics = std::ptr::null_mut();
    if GdipCreateFromHDC(hdc, &mut g).0 == 0 && !g.is_null() {
        crate::ghlogo::draw(g, r);
        let _ = GdipDeleteGraphics(g);
    }
}

/// 画 GitHub 图标按钮：常态就是深色实心圆本身（矢量绘制），
/// 悬浮/按下时在外面加一圈浅色光晕，不再另铺灰底，避免「圈套圈」。
unsafe fn draw_gh_button(hdc: HDC) {
    let r = gh_rect();
    if GH_HOVER || GH_DOWN {
        let grow = sc(if GH_DOWN { 9 } else { 7 });
        let halo = rect(
            r.left - grow,
            r.top - grow,
            (r.right - r.left) + grow * 2,
            (r.bottom - r.top) + grow * 2,
        );
        let bg = if GH_DOWN {
            c(0xD5, 0xDC, 0xE4)
        } else {
            c(0xE6, 0xEB, 0xF1)
        };
        let b = CreateSolidBrush(bg);
        let old = SelectObject(hdc, HGDIOBJ(b.0));
        let op = SelectObject(hdc, GetStockObject(NULL_PEN));
        Ellipse(hdc, halo.left, halo.top, halo.right, halo.bottom);
        SelectObject(hdc, old);
        SelectObject(hdc, op);
        DeleteObject(HGDIOBJ(b.0));
    }
    draw_github(hdc, r);
}

/// 命中测试用的图标区域（含少量外扩，手感更宽松）
fn gh_hit(x: i32, y: i32) -> bool {
    let r = gh_rect();
    let pad = sc(4);
    x >= r.left - pad && x < r.right + pad && y >= r.top - pad && y < r.bottom + pad
}

unsafe extern "system" fn about_wndproc(
    hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);

            // 背景
            let bg = CreateSolidBrush(c(0xFF, 0xFF, 0xFF));
            FillRect(hdc, &rect(0, 0, client_w(), client_h()), bg);
            DeleteObject(HGDIOBJ(bg.0));

            SetBkMode(hdc, TRANSPARENT);

            // 软件名
            let name_font = font(21, 700);
            let old_font = SelectObject(hdc, HGDIOBJ(name_font.0));
            SetTextColor(hdc, c(0x1C, 0x1C, 0x1E));
            let mut name: Vec<u16> = "画屏点睛".encode_utf16().collect();
            name.push(0);
            let mut r1 = rect(
                sc(16),
                sc(BASE_NAME_Y),
                client_w() - sc(32),
                sc(BASE_NAME_H),
            );
            DrawTextW(hdc, &mut name, &mut r1, DT_CENTER | DT_SINGLELINE | DT_VCENTER);

            // 版本号
            let ver_font = font(13, 400);
            SelectObject(hdc, HGDIOBJ(ver_font.0));
            SetTextColor(hdc, c(0x77, 0x7E, 0x87));
            let mut ver: Vec<u16> = format!("版本 {}", env!("CARGO_PKG_VERSION"))
                .encode_utf16()
                .collect();
            ver.push(0);
            let mut r2 = rect(sc(16), sc(BASE_VER_Y), client_w() - sc(32), sc(BASE_VER_H));
            DrawTextW(hdc, &mut ver, &mut r2, DT_CENTER | DT_SINGLELINE | DT_VCENTER);

            // 分隔线
            let pen = CreatePen(PS_SOLID, sc(1).max(1), c(0xE7, 0xE9, 0xEC));
            let op = SelectObject(hdc, HGDIOBJ(pen.0));
            MoveToEx(hdc, sc(44), sc(BASE_DIV_Y), None);
            LineTo(hdc, client_w() - sc(44), sc(BASE_DIV_Y));
            SelectObject(hdc, op);
            DeleteObject(HGDIOBJ(pen.0));

            // GitHub 图标按钮
            draw_gh_button(hdc);

            // 按钮下方的提示文字
            let hint_font = font(12, 400);
            SelectObject(hdc, HGDIOBJ(hint_font.0));
            SetTextColor(hdc, c(0x8D, 0x94, 0x9C));
            let mut hint: Vec<u16> = "点击图标访问 GitHub 项目主页".encode_utf16().collect();
            hint.push(0);
            let mut r3 = rect(
                sc(16),
                sc(BASE_GH_Y + BASE_GH_SZ + 10),
                client_w() - sc(32),
                sc(BASE_HINT_H),
            );
            DrawTextW(hdc, &mut hint, &mut r3, DT_CENTER | DT_SINGLELINE | DT_VCENTER);

            SelectObject(hdc, old_font);
            DeleteObject(HGDIOBJ(name_font.0));
            DeleteObject(HGDIOBJ(ver_font.0));
            DeleteObject(HGDIOBJ(hint_font.0));
            EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_MOUSEMOVE => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let inside = gh_hit(x, y);
            if inside != GH_HOVER {
                GH_HOVER = inside;
                InvalidateRect(hwnd, None, true);
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            if gh_hit(x, y) {
                GH_DOWN = true;
                InvalidateRect(hwnd, None, true);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let was_down = GH_DOWN;
            GH_DOWN = false;
            InvalidateRect(hwnd, None, true);
            if was_down && gh_hit(x, y) {
                // 用系统默认浏览器打开 GitHub 项目页
                let _ = ShellExecuteW(
                    hwnd,
                    w!("open"),
                    w!("https://github.com/niustark/screenmarker"),
                    None,
                    None,
                    SW_SHOWNORMAL,
                );
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            ShowWindow(hwnd, SW_HIDE);
            LRESULT(0)
        }
        WM_DESTROY => LRESULT(0),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// 显示「关于」窗口（若已存在则置前，否则新建）
pub fn show(hinstance: HINSTANCE) {
    unsafe {
        if ABOUT_HWND != 0 {
            let h = HWND(ABOUT_HWND as *mut _);
            if IsWindow(h).as_bool() {
                ShowWindow(h, SW_SHOW);
                SetForegroundWindow(h);
                return;
            }
            ABOUT_HWND = 0;
        }

        let class_name = w!("ScreenMarkerAbout");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(about_wndproc),
            hInstance: hinstance,
            hbrBackground: HBRUSH(ptr::null_mut()),
            hIcon: crate::appicon::shared(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: class_name,
            ..Default::default()
        };
        RegisterClassW(&wc);

        // 主程序是 per-monitor DPI aware，系统不会帮忙缩放，自己算比例
        let dpi = GetDpiForSystem();
        SCALE = if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 };

        let style = WS_POPUP | WS_CAPTION | WS_SYSMENU;
        let exstyle = WS_EX_TOPMOST | WS_EX_TOOLWINDOW;
        // CreateWindowExW 的宽高是「含边框」的外框尺寸，先按客户区尺寸反推，
        // 否则底部文字会被标题栏吃掉。
        let mut frame = RECT {
            left: 0,
            top: 0,
            right: client_w(),
            bottom: client_h(),
        };
        let _ = AdjustWindowRectEx(&mut frame, style, false, exstyle);
        let win_w = frame.right - frame.left;
        let win_h = frame.bottom - frame.top;

        let sw = GetSystemMetrics(SM_CXSCREEN);
        let sh = GetSystemMetrics(SM_CYSCREEN);
        let x = (sw - win_w) / 2;
        let y = (sh - win_h) / 2;

        let hwnd = CreateWindowExW(
            exstyle,
            class_name,
            w!("关于"),
            style,
            x,
            y,
            win_w,
            win_h,
            None,
            None,
            hinstance,
            None,
        );

        if let Ok(h) = hwnd {
            ABOUT_HWND = h.0 as usize;
            ShowWindow(h, SW_SHOW);
            UpdateWindow(h);
        }
    }
}
