// 「关于」小窗口：软件名 + 版本号 + GitHub 图标按钮（点击跳转项目页）。
//
// GitHub 图标用真实 PNG（github.png）嵌入 exe，GDI+ 从内存流加载绘制，
// 保证透明背景和原图质感，同时 exe 仍是单文件。

#![allow(dead_code)]

use std::ptr;

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Graphics::GdiPlus::{
    GdipCreateBitmapFromStream, GdipCreateFromHDC, GdipDeleteGraphics, GdipDisposeImage,
    GdipDrawImageRectI, GdiplusShutdown, GdiplusStartup, GdiplusStartupInput, GpBitmap, GpGraphics,
    GpImage,
};
use windows::Win32::System::Com::IStream;
use windows::Win32::System::Com::StructuredStorage::CreateStreamOnHGlobal;
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GHND};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::*;

const WIN_W: i32 = 280;
const WIN_H: i32 = 210;

// GitHub 图标按钮的客户区矩形（约在窗口下方中间）
const GH_X: i32 = 110;
const GH_Y: i32 = 112;
const GH_SZ: i32 = 44;

const GITHUB_URL: &str = "https://github.com/niustark/screenmarker";

/// 嵌入的 GitHub 图标（透明背景 PNG）
const GITHUB_PNG: &[u8] = include_bytes!("../github.png");

static mut ABOUT_HWND: usize = 0;
static mut GH_HOVER: bool = false;
static mut GH_DOWN: bool = false;

/// GDI+ 是否已启动（全局一次性）
static mut GDI_TOKEN: usize = 0;

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

/// 用 GDI+ 从内嵌 PNG 加载出位图（失败返回 null）。调用方负责 GdipDisposeImage。
unsafe fn load_github_bitmap() -> *mut GpBitmap {
    if !gdi_ready() {
        return ptr::null_mut();
    }
    let len = GITHUB_PNG.len();
    if len == 0 {
        return ptr::null_mut();
    }
    // 分配全局内存，把 PNG 字节拷进去，再包成 IStream 给 GDI+ 解码。
    let Ok(hg) = GlobalAlloc(GHND, len) else {
        return ptr::null_mut();
    };
    let dst = GlobalLock(hg);
    if dst.is_null() {
        let _ = GlobalUnlock(hg);
        return ptr::null_mut();
    }
    ptr::copy_nonoverlapping(GITHUB_PNG.as_ptr(), dst as *mut u8, len);
    let _ = GlobalUnlock(hg);

    let Ok(stream) = CreateStreamOnHGlobal(hg, true) else {
        return ptr::null_mut();
    };

    let mut bmp: *mut GpBitmap = ptr::null_mut();
    if GdipCreateBitmapFromStream(&stream, &mut bmp).0 != 0 || bmp.is_null() {
        return ptr::null_mut();
    }
    bmp
}

/// 在目标矩形内绘制 GitHub 图标（保持原图宽高比，居中）
unsafe fn draw_github(hdc: HDC, r: RECT, _active: bool) {
    let bmp = load_github_bitmap();
    if bmp.is_null() {
        return;
    }
    let mut g: *mut GpGraphics = ptr::null_mut();
    if GdipCreateFromHDC(hdc, &mut g).0 == 0 && !g.is_null() {
        // 目标区域留边，居中按比例缩放
        let tw = (r.right - r.left) as f32;
        let th = (r.bottom - r.top) as f32;
        let scale = tw.min(th) / 64.0; // 原图 64×64
        let dw = (64.0 * scale) as i32;
        let dh = (64.0 * scale) as i32;
        let dx = r.left + ((r.right - r.left) - dw) / 2;
        let dy = r.top + ((r.bottom - r.top) - dh) / 2;
        let _ = GdipDrawImageRectI(
            g,
            bmp as *mut GpImage,
            dx,
            dy,
            dw,
            dh,
        );
        let _ = GdipDeleteGraphics(g);
    }
    let _ = GdipDisposeImage(bmp as *mut GpImage);
}

/// 画一个悬浮/按下态的小圆按钮底
unsafe fn draw_gh_button(hdc: HDC) {
    let r = rect(GH_X, GH_Y, GH_SZ, GH_SZ);
    // 底：浅灰圆（hover 加深，按下更深）
    let bg = if GH_DOWN {
        c(0xD8, 0xDB, 0xDF)
    } else if GH_HOVER {
        c(0xE8, 0xEA, 0xEC)
    } else {
        c(0xF0, 0xF1, 0xF3)
    };
    let b = CreateSolidBrush(bg);
    let old = SelectObject(hdc, HGDIOBJ(b.0));
    let op = SelectObject(hdc, GetStockObject(NULL_PEN));
    Ellipse(hdc, r.left, r.top, r.right, r.bottom);
    SelectObject(hdc, old);
    SelectObject(hdc, op);
    DeleteObject(HGDIOBJ(b.0));

    // 里面画猫头，四周留一点边
    let pad = 9;
    draw_github(
        hdc,
        rect(GH_X + pad, GH_Y + pad, GH_SZ - pad * 2, GH_SZ - pad * 2),
        GH_HOVER || GH_DOWN,
    );
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
            FillRect(hdc, &rect(0, 0, WIN_W, WIN_H), bg);
            DeleteObject(HGDIOBJ(bg.0));

            // 软件名
            let name_font = CreateFontW(
                -20, 0, 0, 0, 700, 0, 0, 0,
                DEFAULT_CHARSET.0 as u32, 0, 0, 0, 0,
                w!("Microsoft YaHei"),
            );
            let old_font = SelectObject(hdc, HGDIOBJ(name_font.0));
            SetBkMode(hdc, TRANSPARENT);
            SetTextColor(hdc, c(0x1C, 0x1C, 0x1E));
            let mut name: Vec<u16> = "屏幕标注 ScreenMarker".encode_utf16().collect();
            name.push(0);
            let mut r1 = rect(20, 26, WIN_W - 40, 30);
            DrawTextW(hdc, &mut name, &mut r1, DT_CENTER | DT_SINGLELINE | DT_VCENTER);

            // 版本号
            let ver_font = CreateFontW(
                -14, 0, 0, 0, 400, 0, 0, 0,
                DEFAULT_CHARSET.0 as u32, 0, 0, 0, 0,
                w!("Microsoft YaHei"),
            );
            SelectObject(hdc, HGDIOBJ(ver_font.0));
            SetTextColor(hdc, c(0x6B, 0x72, 0x7A));
            let mut ver: Vec<u16> = format!("版本 {}", env!("CARGO_PKG_VERSION")).encode_utf16().collect();
            ver.push(0);
            let mut r2 = rect(20, 58, WIN_W - 40, 24);
            DrawTextW(hdc, &mut ver, &mut r2, DT_CENTER | DT_SINGLELINE | DT_VCENTER);

            // 分隔线
            let pen = CreatePen(PS_SOLID, 1, c(0xE0, 0xE2, 0xE5));
            let op = SelectObject(hdc, HGDIOBJ(pen.0));
            MoveToEx(hdc, 30, 92, None);
            LineTo(hdc, WIN_W - 30, 92);
            SelectObject(hdc, op);
            DeleteObject(HGDIOBJ(pen.0));

            // GitHub 图标按钮
            draw_gh_button(hdc);

            // 按钮下方的提示文字
            let hint_font = CreateFontW(
                -12, 0, 0, 0, 400, 0, 0, 0,
                DEFAULT_CHARSET.0 as u32, 0, 0, 0, 0,
                w!("Microsoft YaHei"),
            );
            SelectObject(hdc, HGDIOBJ(hint_font.0));
            SetTextColor(hdc, c(0x8A, 0x90, 0x98));
            let mut hint: Vec<u16> = "GitHub 项目主页".encode_utf16().collect();
            hint.push(0);
            let mut r3 = rect(20, GH_Y + GH_SZ + 4, WIN_W - 40, 20);
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
            let in_gh = x >= GH_X && x < GH_X + GH_SZ && y >= GH_Y && y < GH_Y + GH_SZ;
            if in_gh != GH_HOVER {
                GH_HOVER = in_gh;
                InvalidateRect(hwnd, None, true);
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            if x >= GH_X && x < GH_X + GH_SZ && y >= GH_Y && y < GH_Y + GH_SZ {
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
            if was_down && x >= GH_X && x < GH_X + GH_SZ && y >= GH_Y && y < GH_Y + GH_SZ {
                // 用系统默认浏览器打开 GitHub 项目页
                ShellExecuteW(
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
            hIcon: crate::appicon::load(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: class_name,
            ..Default::default()
        };
        RegisterClassW(&wc);

        let sw = GetSystemMetrics(SM_CXSCREEN);
        let sh = GetSystemMetrics(SM_CYSCREEN);
        let x = (sw - WIN_W) / 2;
        let y = (sh - WIN_H) / 2;

        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            class_name,
            w!("关于 屏幕标注"),
            WS_POPUP | WS_CAPTION | WS_SYSMENU,
            x,
            y,
            WIN_W,
            WIN_H,
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
