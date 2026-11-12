// 屏幕标注程序 —— Rust 完整版
// 全屏透明画布 + 浮动工具条 + 系统托盘 + 全局快捷键

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

// Win32 GUI 程序里绝大多数 GDI / 窗口 API 的返回值（BOOL、Result）只是成功标志，
// 逐个 `let _ =` 会把代码淹没在噪声里。关键调用（托盘图标、屏幕快照、
// 图层提交）在各自位置显式检查，其余统一允许忽略。
#![allow(unused_must_use)]
// 本程序是单线程消息循环，模块级 static mut 承载 UI 状态，不存在并发访问。
// 将来升级到 edition 2024 前需要改成 thread_local! / UnsafeCell。
#![allow(static_mut_refs)]

mod shape;
mod render;
mod frame;
mod dbg;
mod hooks;
mod icons;
mod appicon;
mod toolbar;
mod canvas;
mod textedit;
mod cursor;
mod about;

use shape::*;
use toolbar::Toolbar;
use canvas::Canvas;

use std::cell::RefCell;
use std::ptr;
use std::rc::Rc;

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

const WM_TRAYICON: u32 = WM_APP + 1;
const ID_TRAY: u32 = 1;
const ID_EXIT: usize = 1;
const ID_ABOUT: usize = 2;

fn main() -> Result<()> {
    unsafe {
        // 高分屏下必须声明 DPI 感知，否则系统会「虚拟化」坐标，
        // 鼠标位置和绘制位置对不上（4K 屏上尤其明显）
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        let hinst = GetModuleHandleW(None)?;
        let hinstance: HINSTANCE = hinst.into();

        // 全局共享状态
        let state = Rc::new(RefCell::new(AppState::new()));

        // 注册托盘窗口类
        let tray_class = w!("ScreenMarkerTray");
        register_class(tray_class, Some(tray_wndproc), hinstance, None);

        // 创建画布
        let canvas = Canvas::new(hinstance, state.clone())?;

        // 创建工具条
        let toolbar = Toolbar::new(hinstance, state.clone(), canvas.hwnd())?;
        // 让画布知道工具条位置，显示时插到它下面（避免画布盖住工具条）
        canvas::set_toolbar(toolbar.hwnd());

        // 注册全局钩子
        hooks::install(hinstance, state.clone(), canvas.hwnd(), toolbar.hwnd())?;

        // 创建托盘图标（用一个隐藏窗口承载）
        let tray_hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            tray_class,
            w!("ScreenMarker"),
            WS_OVERLAPPED,
            0, 0, 0, 0,
            None, None, hinstance, None,
        )?;
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: tray_hwnd,
            uID: ID_TRAY,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: WM_TRAYICON,
            ..Default::default()
        };
        let ico = appicon::load_sized(
            GetSystemMetrics(SM_CXSMICON),
            GetSystemMetrics(SM_CYSMICON),
        );
        nid.hIcon = if ico.is_invalid() {
            LoadIconW(None, IDI_APPLICATION)?
        } else {
            ico
        };
        let tip: Vec<u16> = "屏幕标注\0".encode_utf16().collect();
        nid.szTip[..tip.len()].copy_from_slice(&tip);
        if !Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
            dbg::log("托盘图标添加失败（不影响画布，右键菜单入口会缺失）");
        }

        // 消息循环。GetMessageW 返回 BOOL：>0 正常、0 = WM_QUIT、-1 = 出错。
        // 旧的 `.as_bool()` 会把 -1 也当成 true，出错时拿着脏 MSG 空转。
        let mut msg = MSG::default();
        loop {
            let r = GetMessageW(&mut msg, None, 0, 0);
            if r.0 <= 0 {
                break;
            }
            // 文字编辑框用自绘窗口，直接走默认处理
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // 清理
        Shell_NotifyIconW(NIM_DELETE, &nid);
        hooks::uninstall();
        Ok(())
    }
}

unsafe fn register_class(
    name: PCWSTR,
    proc: WNDPROC,
    hinstance: HINSTANCE,
    background: Option<HBRUSH>,
) {
    let wc = WNDCLASSW {
        lpfnWndProc: proc,
        hInstance: hinstance,
        hbrBackground: background.unwrap_or(HBRUSH(ptr::null_mut())),
        hIcon: appicon::load(),
        lpszClassName: name,
        ..Default::default()
    };
    RegisterClassW(&wc);
}

// 托盘窗口过程
unsafe extern "system" fn tray_wndproc(
    hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_TRAYICON => {
            if lparam.0 as u32 == WM_RBUTTONUP {
                let mut pt = POINT::default();
                GetCursorPos(&mut pt);
                SetForegroundWindow(hwnd);
                let menu = CreatePopupMenu().expect("create menu");
                AppendMenuW(menu, MF_STRING, ID_ABOUT, w!("关于"));
                AppendMenuW(menu, MF_SEPARATOR, 0, None);
                AppendMenuW(menu, MF_STRING, ID_EXIT, w!("退出"));
                let cmd = TrackPopupMenu(
                    menu,
                    TPM_RETURNCMD | TPM_RIGHTBUTTON,
                    pt.x,
                    pt.y,
                    0,
                    hwnd,
                    None,
                );
                DestroyMenu(menu);
                match cmd.0 as usize {
                    ID_EXIT => PostQuitMessage(0),
                    ID_ABOUT => {
                        if let Ok(m) = GetModuleHandleW(None) {
                            about::show(m.into());
                        }
                    }
                    _ => {}
                }
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            if wparam.0 as usize == ID_EXIT {
                PostQuitMessage(0);
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
