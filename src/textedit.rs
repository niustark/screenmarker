// 文字输入框：一个独立顶层小窗 + 内嵌 EDIT 控件。
//
// 为什么不能把 EDIT 直接放进画布：画布是 UpdateLayeredWindow 的分层窗口，
// 子控件不会参与合成，放进去看不见、也点不到。所以文字编辑必须独立开窗。
//
// 交互约定：
//   Esc / 失焦 / 回车+Ctrl  → 提交并关闭（Esc 是最常用的「完成」手势）
//   输入框内右键            → 提交并关闭，工具切回「选择」，文字处于可选中态
//   文字为空                → 删除该文字框（含输入框本身）

#![allow(dead_code)]

use crate::render::measure_text_box;
use crate::shape::*;

use std::cell::RefCell;
use std::ptr;
use std::rc::Rc;

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_ESCAPE, VK_RETURN};
use windows::Win32::UI::WindowsAndMessaging::*;

static mut TE_STATE_PTR: usize = 0;
static mut TE_EDIT: HWND = HWND(ptr::null_mut());
/// 编辑器外层窗口（承载 EDIT 的那个）
static mut TE_HWND: HWND = HWND(ptr::null_mut());
/// 当前给 EDIT 用的字体（换字号时会替换并释放旧的）
static mut TE_FONT: HFONT = HFONT(ptr::null_mut());
static mut TE_SHAPE_IDX: usize = usize::MAX;
static mut TE_CLOSING: bool = false;
/// 右键提交时置位：提交完把工具切回「选择」（Esc / 失焦提交不切，方便连续写字）
static mut TE_EXIT_TOOL: bool = false;

// 右键消息常量自己定义：windows 0.58 里 WM_CONTEXTMENU 属于未开启的 feature，
// 若写成 match 的消息分支会被编译器当成「变量绑定模式」吞掉其它消息
// （WM_MOUSELEAVE 曾这样把 WM_NCCREATE 吃掉导致启动崩溃）。这里一律用 if 比较。
const WM_RBUTTONDOWN_C: u32 = 0x0204;
const WM_RBUTTONUP_C: u32 = 0x0205;
const WM_CONTEXTMENU_C: u32 = 0x007B;

/// 输入框内右键：DOWN 先吞掉（不让 EDIT 进入系统右键菜单流程），
/// UP / CONTEXTMENU 时再提交关闭。放在 UP 提交是为了避免鼠标抬起
/// 落到已被销毁窗口下面的画布上，触发画布自己的右键逻辑。
fn is_right_click(msg: u32) -> bool {
    msg == WM_RBUTTONDOWN_C || msg == WM_RBUTTONUP_C || msg == WM_CONTEXTMENU_C
}

/// 同一个输入窗口复用于两种场景：
///   Text      —— 画布上的文字标注（内容写回 Shape::Text）
///   Watermark —— 水印文字（内容写回 AppState::watermark_text）
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Text,
    Watermark,
}
static mut TE_MODE: Mode = Mode::Text;
// 打开时刻：刚弹出时窗口还没拿到焦点，容易被随后的鼠标抬起「打到失焦」，
// 所以 300ms 内的失焦一律忽略。
static mut TE_OPEN_AT: Option<std::time::Instant> = None;
// EDIT 控件原来的窗口过程（子类化后要转调它）
static mut EDIT_OLD_PROC: isize = 0;

/// 子类化 EDIT：按键先到控件本身，父窗口收不到，
/// 所以必须在这里拦 Esc（否则「按 Esc 完成」无效）。
unsafe extern "system" fn edit_proc(
    hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM,
) -> LRESULT {
    if is_right_click(msg) {
        // 按下只吞消息，抬起时才提交并关闭输入框
        if msg == WM_RBUTTONDOWN_C {
            return LRESULT(0);
        }
        if let Ok(parent) = GetParent(hwnd) {
            TE_EXIT_TOOL = true;
            commit_and_close(parent);
        }
        return LRESULT(0);
    }
    if msg == WM_KEYDOWN {
        let vk = wparam.0 as u16;
        if vk == VK_ESCAPE.0 {
            if let Ok(parent) = GetParent(hwnd) {
                commit_and_close(parent);
                return LRESULT(0);
            }
        }
        // 水印输入是单行框，回车 = 确定（多行文字框里回车要留给换行）
        if vk == VK_RETURN.0 && TE_MODE == Mode::Watermark {
            if let Ok(parent) = GetParent(hwnd) {
                commit_and_close(parent);
                return LRESULT(0);
            }
        }
    }
    let old: WNDPROC = std::mem::transmute(EDIT_OLD_PROC);
    CallWindowProcW(old, hwnd, msg, wparam, lparam)
}

const PAD: i32 = 4;
/// 文字框默认宽度 = 10 个字
pub const MIN_CHARS: f32 = 10.0;
/// EN_CHANGE：EDIT 内容变化通知
const EN_CHANGE_CODE: usize = 0x0300;

unsafe extern "system" fn te_wndproc(
    hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM,
) -> LRESULT {
    // 右键可能落在外层窗口（虚线框那一圈），同样按「结束输入」处理。
    // 必须放在 match 之前用 if 判断，见上面常量的说明。
    if is_right_click(msg) {
        if msg == WM_RBUTTONDOWN_C {
            return LRESULT(0);
        }
        TE_EXIT_TOOL = true;
        commit_and_close(hwnd);
        return LRESULT(0);
    }
    match msg {
        WM_KEYDOWN => {
            if wparam.0 as u16 == VK_ESCAPE.0 {
                commit_and_close(hwnd);
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_COMMAND => {
            // 每敲一个字就重算一次尺寸，让虚线框「跟着内容长」
            let code = (wparam.0 >> 16) & 0xFFFF;
            let from_edit = HWND(lparam.0 as *mut _) == TE_EDIT;
            if code == EN_CHANGE_CODE && from_edit && TE_MODE == Mode::Text {
                resize_box(hwnd);
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => {
            // 水印输入框浮在屏幕正中，必须有实底，否则透出桌面看不清
            if TE_MODE == Mode::Watermark {
                let hdc = HDC(wparam.0 as *mut _);
                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                let brush = CreateSolidBrush(COLORREF(0x00F5F5F5));
                FillRect(hdc, &rc, brush);
                let _ = DeleteObject(HGDIOBJ(brush.0));
                return LRESULT(1);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_KILLFOCUS => {
            // 刚打开的一小段时间内忽略失焦（否则点击的鼠标抬起就把框关了）
            let mature = match &*(std::ptr::addr_of!(TE_OPEN_AT)) {
                Some(t) => t.elapsed().as_millis() > 300,
                None => true,
            };
            if mature {
                commit_and_close(hwnd);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            commit_and_close(hwnd);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// 取编辑框里的当前文字
unsafe fn current_text() -> String {
    let edit = TE_EDIT;
    if edit.is_invalid() {
        return String::new();
    }
    let mut buf = [0u16; 16384];
    let n = GetWindowTextW(edit, &mut buf);
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// 按当前内容重算虚线框尺寸：宽度跟最长一行走（不低于 10 个字），高度跟行数走。
unsafe fn resize_box(hwnd: HWND) {
    let state_ptr = TE_STATE_PTR as *const RefCell<AppState>;
    let idx = TE_SHAPE_IDX;
    if state_ptr.is_null() || idx == usize::MAX {
        return;
    }
    let text = current_text();

    // 先取字体大小与左上角，借用结束再动窗口，避免 RefCell 双重借用
    let info = {
        let st = (*state_ptr).borrow();
        match st.shapes.get(idx) {
            Some(Shape::Text {
                rect, font_size, ..
            }) => Some((rect.x, rect.y, *font_size)),
            _ => None,
        }
    };
    let Some((x, y, font_size)) = info else {
        return;
    };

    let (w, h) = measure_text_box(&text, font_size as i32, MIN_CHARS);

    {
        let mut st = (*state_ptr).borrow_mut();
        if let Some(Shape::Text { rect, .. }) = st.shapes.get_mut(idx) {
            rect.w = w;
            rect.h = h;
        }
    }

    // 同步编辑器窗口与内嵌 EDIT 的大小
    let iw = (w as i32 - PAD * 2).max(60);
    let ih = (h as i32 - PAD * 2).max(28);
    SetWindowPos(
        hwnd,
        None,
        x as i32 + PAD,
        y as i32 + PAD,
        iw,
        ih,
        SWP_NOZORDER | SWP_NOACTIVATE,
    );
    SetWindowPos(
        TE_EDIT,
        None,
        0,
        0,
        iw,
        ih,
        SWP_NOZORDER | SWP_NOACTIVATE,
    );
    crate::canvas::repaint_global();
}

unsafe fn commit_and_close(hwnd: HWND) {
    if TE_CLOSING {
        return;
    }
    TE_CLOSING = true;

    let edit = TE_EDIT;
    let idx = TE_SHAPE_IDX;
    let mode = TE_MODE;
    let state_ptr = TE_STATE_PTR as *const RefCell<AppState>;

    if !edit.is_invalid() && !state_ptr.is_null() {
        let mut buf = [0u16; 8192];
        let n = GetWindowTextW(edit, &mut buf);
        let text = String::from_utf16_lossy(&buf[..n.max(0) as usize]);

        if mode == Mode::Watermark {
            if !text.trim().is_empty() {
                {
                    let mut st = (*state_ptr).borrow_mut();
                    // 记住内容：下次打开输入框就是上次输入的文字
                    st.watermark_text = text;
                    st.text_editing = false;
                }
                // 水印是整屏效果，确定后立刻铺上去（已存在则原地改文字）
                crate::canvas::apply_watermark();
            } else {
                (*state_ptr).borrow_mut().text_editing = false;
            }
        } else if idx != usize::MAX {
            let mut kept = false;
            {
                let mut st = (*state_ptr).borrow_mut();
                if idx < st.shapes.len() {
                    if text.trim().is_empty() {
                        st.shapes.remove(idx);
                    } else if let Shape::Text {
                        rect,
                        text: t,
                        font_size,
                        ..
                    } = &mut st.shapes[idx]
                    {
                        *t = text;
                        // 提交时再按内容收一次尺寸（宽度跟最长一行，高度跟换行后的行数）
                        let (w, h) = measure_text_box(t, *font_size as i32, MIN_CHARS);
                        rect.w = w;
                        rect.h = h;
                        kept = true;
                    }
                }
                st.text_editing = false;
            }
            TE_SHAPE_IDX = usize::MAX;
            // 提交后直接选中：不用再点一次，立刻就能拖动/缩放手柄；
            // 空内容被删掉时则清掉选中态，别让旧索引指向别的图元。
            if kept {
                crate::canvas::select_index(idx);
            } else {
                crate::canvas::select_none();
            }
            crate::dbg::log(&format!("文字提交 idx={idx} kept={kept}"));
            crate::canvas::repaint_global();
        }
        TE_MODE = Mode::Text;
    }

    // 右键提交额外收尾：工具切回「选择」，让刚写完的文字处于标准可选中态
    // （有选中框，可直接拖动/缩放/滚轮改大小）。Esc、失焦提交不受影响，
    // 仍留在文字工具，方便接着点下一处继续写。
    if TE_EXIT_TOOL {
        TE_EXIT_TOOL = false;
        // 右键提交的按下事件已经被低级鼠标钩子清掉了「点击归属」，
        // 但提交后文字正处于可选中态，紧接着按 Del 应该能删掉它。
        crate::hooks::note_our_window_clicked();
        if mode == Mode::Text && !state_ptr.is_null() {
            (*state_ptr).borrow_mut().tool = crate::shape::Tool::Select;
            // 工具条的按钮高亮是从 state.tool 读的，改完必须重绘
            crate::toolbar::refresh();
        }
    }

    let _ = DestroyWindow(hwnd);
    // 必须在销毁之后再提一次工具条：TOPMOST 带会因窗口销毁而重排，
    // 画布可能被顶到工具条上面（表现就是「工具按钮点不动」）
    crate::canvas::reassert_toolbar();
    TE_EDIT = HWND(ptr::null_mut());
    TE_HWND = HWND(ptr::null_mut());
    if !TE_FONT.is_invalid() {
        let _ = DeleteObject(HGDIOBJ(TE_FONT.0));
        TE_FONT = HFONT(ptr::null_mut());
    }
    TE_CLOSING = false;
}

/// 输入框开着就直接提交并关掉（收纳工具条时用：键盘焦点要还回去）
pub fn commit_open() {
    unsafe {
        if TE_CLOSING || TE_HWND.is_invalid() {
            return;
        }
        if !IsWindow(TE_HWND).as_bool() {
            return;
        }
        commit_and_close(TE_HWND);
    }
}

/// 是否正在打字。
///
/// 工具条的全局空格快捷键要靠它让路：编辑框已经弹出（或正在关闭的瞬间）时，
/// 空格必须原样送给编辑框，不能拿去展开/收纳工具条。
/// 光看 state.text_editing 不够 —— 那个标志只在 open/commit 时翻，窗口状态更真实。
pub fn is_typing() -> bool {
    unsafe {
        if !TE_EDIT.is_invalid() && IsWindow(TE_EDIT).as_bool() {
            return true;
        }
        if !TE_HWND.is_invalid() && IsWindow(TE_HWND).as_bool() && IsWindowVisible(TE_HWND).as_bool()
        {
            return true;
        }
        let state_ptr = TE_STATE_PTR as *const RefCell<AppState>;
        !state_ptr.is_null() && (*state_ptr).borrow().text_editing
    }
}

/// 字号被滑块改了：把编辑框的字体和尺寸同步过去（打字过程中就能看到效果）
pub fn retune() {
    unsafe {
        if TE_EDIT.is_invalid() || TE_CLOSING {
            return;
        }
        let state_ptr = TE_STATE_PTR as *const RefCell<AppState>;
        let idx = TE_SHAPE_IDX;
        if state_ptr.is_null() || idx == usize::MAX {
            return;
        }
        let fs = {
            let st = (*state_ptr).borrow();
            match st.shapes.get(idx) {
                Some(Shape::Text { font_size, .. }) => *font_size,
                _ => return,
            }
        };
        let font = CreateFontW(
            -(fs as i32).max(8),
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
        SendMessageW(TE_EDIT, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
        // 换完再释放旧字体：先删会被 EDIT 沿用成野句柄
        if !TE_FONT.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(TE_FONT.0));
        }
        TE_FONT = font;
        // 尺寸跟着字号走
        if let Ok(parent) = GetParent(TE_EDIT) {
            resize_box(parent);
        }
    }
}

/// 打开水印文字输入框：单行、屏幕居中、回车/Esc 都算确定。
///
/// 水印是整屏平铺的文字，所以不需要「先点位置再输入」，
/// 弹框确定后直接铺满整屏；再次点水印工具就是改内容。
pub fn open_watermark(hinstance: HINSTANCE, initial: &str, state: Rc<RefCell<AppState>>) {
    unsafe {
        if !TE_EDIT.is_invalid() {
            if let Ok(prev) = GetParent(TE_EDIT) {
                commit_and_close(prev);
            }
        }

        TE_MODE = Mode::Watermark;
        TE_STATE_PTR = Rc::as_ptr(&state) as *const RefCell<AppState> as usize;
        TE_SHAPE_IDX = usize::MAX;

        // 跟 DPI 走，否则 200% 缩放下框会小得看不清
        let dpi = GetDpiForSystem().max(96) as f32;
        let s = dpi / 96.0;
        let cw = (440.0 * s) as i32;
        let ch = (46.0 * s) as i32;
        let sw = GetSystemMetrics(SM_CXSCREEN);
        let sh = GetSystemMetrics(SM_CYSCREEN);

        let class = w!("ScreenMarkerInput");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(te_wndproc),
            hInstance: hinstance,
            hbrBackground: HBRUSH(ptr::null_mut()),
            hIcon: crate::appicon::load(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);

        let style = WS_CAPTION | WS_SYSMENU | WS_POPUP;
        let mut rc = RECT {
            left: 0,
            top: 0,
            right: cw,
            bottom: ch,
        };
        let _ = AdjustWindowRect(&mut rc, style, false);
        let ww = rc.right - rc.left;
        let wh = rc.bottom - rc.top;
        let x = (sw - ww) / 2;
        // 放在屏幕偏上，避开画布上正在标注的内容
        let y = (sh - wh) / 2 - (180.0 * s) as i32;

        let hwnd = match CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            class,
            w!("水印文字（回车确定）"),
            style,
            x,
            y,
            ww,
            wh,
            None,
            None,
            hinstance,
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                crate::dbg::log(&format!("创建水印输入框失败: {e:?}"));
                TE_MODE = Mode::Text;
                return;
            }
        };
        TE_HWND = hwnd;

        // ES_AUTOHSCROLL：内容超宽时横向滚动，不要截断
        let edit = match CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("EDIT"),
            w!(""),
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(0x0080),
            6,
            6,
            cw,
            ch,
            hwnd,
            None,
            hinstance,
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                crate::dbg::log(&format!("创建水印 EDIT 失败: {e:?}"));
                let _ = DestroyWindow(hwnd);
                TE_MODE = Mode::Text;
                return;
            }
        };
        TE_EDIT = edit;
        TE_OPEN_AT = Some(std::time::Instant::now());
        EDIT_OLD_PROC =
            SetWindowLongPtrW(edit, GWLP_WNDPROC, edit_proc as *const () as isize);

        let font = CreateFontW(
            -((18.0 * s) as i32).max(12),
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
        SendMessageW(edit, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
        if !TE_FONT.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(TE_FONT.0));
        }
        TE_FONT = font;

        let wide: Vec<u16> = initial.encode_utf16().chain(std::iter::once(0)).collect();
        SetWindowTextW(edit, PCWSTR(wide.as_ptr()));

        ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(edit);
        // 全选：直接输入就是替换，想微调也能用方向键
        SendMessageW(edit, 0x00B1u32, WPARAM(0), LPARAM(-1));
        crate::dbg::log("水印输入框已打开");
    }
}

/// 打开文字输入框，rect 是文字框在屏幕上的位置
pub fn open(
    hinstance: HINSTANCE,
    rect: Rect,
    initial: &str,
    shape_idx: usize,
    state: Rc<RefCell<AppState>>,
) {
    unsafe {
        // 已经开着就先关掉上一个
        if !TE_EDIT.is_invalid() {
            if let Ok(prev) = GetParent(TE_EDIT) {
                commit_and_close(prev);
            }
        }

        TE_STATE_PTR = Rc::as_ptr(&state) as *const RefCell<AppState> as usize;
        TE_SHAPE_IDX = shape_idx;
        TE_MODE = Mode::Text;

        let class = w!("ScreenMarkerTextEdit");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(te_wndproc),
            hInstance: hinstance,
            hbrBackground: HBRUSH(ptr::null_mut()),
            hIcon: crate::appicon::load(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);

        // 内嵌 EDIT 往里缩一圈，好让画布上的虚线框露出来
        let x = rect.x as i32 + PAD;
        let y = rect.y as i32 + PAD;
        let w = (rect.w as i32 - PAD * 2).max(60);
        let h = (rect.h as i32 - PAD * 2).max(28);

        crate::dbg::log(&format!("textedit::open rect=({},{},{},{})", rect.x, rect.y, rect.w, rect.h));
        let hwnd = match CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            class,
            w!("文字标注框"),
            WS_POPUP | WS_BORDER,
            x,
            y,
            w,
            h,
            None,
            None,
            hinstance,
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                crate::dbg::log(&format!("创建文字框窗口失败: {e:?}"));
                return;
            }
        };
        crate::dbg::log(&format!("文字框窗口已创建 {hwnd:?}"));
        TE_HWND = hwnd;

        let edit = match CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("EDIT"),
            w!(""),
            // ES_MULTILINE | ES_AUTOVSCROLL | ES_WANTRETURN：
            // 支持真换行（回车插入 \n），超长自动滚动而不是把内容截掉
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(0x0004 | 0x0040 | 0x1000),
            0,
            0,
            w,
            h,
            hwnd,
            None,
            hinstance,
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                crate::dbg::log(&format!("创建 EDIT 控件失败: {e:?}"));
                let _ = DestroyWindow(hwnd);
                return;
            }
        };
        crate::dbg::log(&format!("EDIT 控件已创建 {edit:?}"));
        TE_EDIT = edit;
        TE_OPEN_AT = Some(std::time::Instant::now());
        // 子类化 EDIT，拦截 Esc
        EDIT_OLD_PROC = SetWindowLongPtrW(
            edit,
            GWLP_WNDPROC,
            edit_proc as *const () as isize,
        );

        // 字号跟当前设置的「粗细」联动（canvas::text_size 是唯一来源，
        // 工具条滑块一动，这里和画布上的文字一起变）
        let font_size = {
            let mut st = state.borrow_mut();
            st.text_editing = true;
            crate::canvas::text_size(st.width) as i32
        };
        let font = CreateFontW(
            -font_size,
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
        SendMessageW(
            edit,
            WM_SETFONT,
            WPARAM(font.0 as usize),
            LPARAM(1),
        );
        // 记下来：之后换字号时要先删旧的
        if !TE_FONT.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(TE_FONT.0));
        }
        TE_FONT = font;

        let wide: Vec<u16> = initial.encode_utf16().chain(std::iter::once(0)).collect();
        SetWindowTextW(edit, PCWSTR(wide.as_ptr()));

        ShowWindow(hwnd, SW_SHOW);
        let ok_fg = SetForegroundWindow(hwnd).as_bool();
        // SetFocus 在 SetForegroundWindow 失败时会静默失败，
        // 那就等于编辑框永远收不到键盘
        let focus_res = SetFocus(edit);
        crate::dbg::log(&format!(
            "SetForegroundWindow={ok_fg} SetFocus={focus_res:?}"
        ));
        // 全选，方便直接覆盖输入
        SendMessageW(edit, 0x00B1u32, WPARAM(0), LPARAM(-1)); // EM_SETSEL
    }
}
