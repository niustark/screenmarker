// 全局键盘 + 鼠标钩子（低级钩子 WH_KEYBOARD_LL / WH_MOUSE_LL）
//
// 注意：低级钩子的回调由系统在「安装钩子的线程」调用，因此必须让该线程
// 持续泵消息。这里钩子在主线程安装，Qt/Win32 主消息循环天然满足这一点，
// 不能再另开后台线程 GetMessageW（那样回调反而收不到）。

#![allow(dead_code)]

use crate::canvas;
use crate::shape::AppState;

use std::cell::RefCell;
use std::ptr;

use windows::Win32::Foundation::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

static mut KEYBOARD_HOOK: HHOOK = HHOOK(ptr::null_mut());
static mut MOUSE_HOOK: HHOOK = HHOOK(ptr::null_mut());

static mut G_STATE_PTR: usize = 0;
static mut G_CANVAS_PTR: usize = 0;
static mut G_TOOLBAR_PTR: usize = 0;

// 长按去重闩锁：按住不放时系统会连发 keydown
static mut SPACE_LATCH: bool = false;
static mut ESC_LATCH: bool = false;
static mut DEL_LATCH: bool = false;
// 上一次「按下」的时间戳（KBDLLHOOKSTRUCT.time，毫秒）。
// 低级钩子在主线程里跑，一旦某次处理超时（比如正好在整屏重绘），
// 系统会跳过那一批事件 —— keyup 被吞掉后闩锁就再也回不到 false，
// 表现就是「按右侧 Ctrl 没反应，只能点主图标」。用时间间隔兜底：
// 距上次按下超过 800ms 必然是新手势（长按的自动重复间隔远小于这个值）。
static mut SPACE_LAST: u32 = 0;
static mut ESC_LAST: u32 = 0;
static mut DEL_LAST: u32 = 0;

// 删除键的「归属」：最近一次鼠标按下是否落在我们自己的窗口上。
// 低级鼠标钩子先于窗口消息派发，所以 mouse_proc 里一律先清成 false；
// 若点击真的落到画布/工具条，它们的窗口过程随后把它置回 true。
// 穿透态下点在空白处会穿透到下面的程序，我们收不到窗口消息 → 保持 false，
// Del/Backspace 就原样留给 PPT / 浏览器，不会误删残留的选中图元。
static mut DEL_OWNED_BY_US: bool = false;

/// 由画布 / 工具条的窗口过程在收到鼠标按下（或右键抬起）时调用。
pub fn note_our_window_clicked() {
    unsafe { DEL_OWNED_BY_US = true; }
}

// ---------- 钩子动作的异步派发 ----------
/// 低级钩子回调必须尽快返回：超时会让系统跳过事件、甚至悄悄摘掉钩子
/// （表现就是「快捷键用着用着失效」）。回调里只做判定 + 闩锁，真正的动作
/// 通过这条消息投给画布窗口，在窗口过程里执行 —— 整屏重绘、SetWindowPos、
/// 跨进程 SendMessageTimeoutW(50ms)、写日志文件全部搬出钩子。
pub const WM_APP_HOOK_ACTION: u32 = WM_APP + 2;

pub const HOOK_TOGGLE: usize = 1;
pub const HOOK_ESC: usize = 2;
pub const HOOK_DELETE: usize = 3;
pub const HOOK_WHEEL: usize = 4;

#[inline]
unsafe fn post_action(hwnd: HWND, action: usize, param: isize) {
    if !hwnd.is_invalid() {
        let _ = PostMessageW(hwnd, WM_APP_HOOK_ACTION, WPARAM(action), LPARAM(param));
    }
}

/// 由画布窗口过程调用：执行钩子判定出的动作。
pub fn dispatch(action: usize, param: isize) {
    match action {
        HOOK_TOGGLE => crate::toolbar::toggle_collapse(),
        HOOK_ESC => unsafe {
            // 问前台控件 WM_GETDLGCODE 最多 50ms，放在窗口过程里不会拖住钩子。
            if !keyboard_owned_by_edit() {
                crate::canvas::exit_markup();
            }
        },
        HOOK_DELETE => {
            crate::dbg::logf!("Del 动作 vk={param:#x}");
            crate::canvas::delete_selected();
        }
        HOOK_WHEEL => {
            // param bit0 = 向上滚，bit1 = Ctrl 按下
            let up = param & 1 != 0;
            let ctrl = param & 2 != 0;
            crate::dbg::logf!("滚轮动作 up={up} ctrl={ctrl}");
            if !canvas::handle_wheel(up, ctrl) {
                crate::toolbar::adjust_width(if up { 1.0 } else { -1.0 });
            }
        }
        _ => {}
    }
}

// 鼠标按下常量自己定义，避免依赖未开启的 windows crate feature。
// （用 match 分支引用未定义常量会被编译器当成变量绑定，曾导致启动崩溃）
const WM_LBUTTONDOWN_C: u32 = 0x0201;
const WM_RBUTTONDOWN_C: u32 = 0x0204;
const WM_MBUTTONDOWN_C: u32 = 0x0207;

/// 这次按下是否算「新手势」（而不是按住不放产生的自动重复）
fn is_fresh(latch: bool, last: u32, now: u32) -> bool {
    !latch || now.wrapping_sub(last) > 800
}

const VK_SPACE: u32 = 0x20;
const VK_ESCAPE: u32 = 0x1B;
const VK_DELETE: u32 = 0x2E;
const VK_BACK: u32 = 0x08;
// 右侧 Ctrl：全局展开/折叠工具条（替代原来的空格）。
// 注意用 VK_RCONTROL 而非 VK_CONTROL，只认「右侧」那一颗，
// 左侧 Ctrl 仍留给 Ctrl+C/V 等常规组合键，互不干扰。
const VK_RCTRL: u32 = 0xA3;

/// 这个窗口的类名是不是 name（不区分大小写）
unsafe fn class_is(h: HWND, name: &str) -> bool {
    if h.is_invalid() {
        return false;
    }
    let mut buf = [0u16; 96];
    let n = GetClassNameW(h, &mut buf);
    n > 0 && String::from_utf16_lossy(&buf[..n as usize]).eq_ignore_ascii_case(name)
}

/// 取窗口类名（小写），取不到返回空串
unsafe fn class_name(h: HWND) -> String {
    if h.is_invalid() {
        return String::new();
    }
    let mut buf = [0u16; 128];
    let n = GetClassNameW(h, &mut buf);
    if n <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..n as usize]).to_ascii_lowercase()
}

/// 这个类名是不是「能输入文字的控件」。
/// 系统标准 EDIT 只是其中一种：RichEdit / Scintilla / 终端 / 各种自绘输入框
/// 的类名都不一样，只认 "EDIT" 会漏掉绝大多数第三方程序的输入框。
unsafe fn is_edit_class(name: &str) -> bool {
    const KEYS: &[&str] = &[
        "edit",
        "richedit",
        "scintilla",
        "tmemo",
        "textbox",
        "consolewindowclass",
        "_wwg",
        "screenmarkertextedit",
        "screenmarkerinput",
    ];
    KEYS.iter().any(|k| name.contains(k))
}

/// 这个焦点窗口是否「接收字符输入」（是文本输入控件）。
///
/// 用 WM_GETDLGCODE 问控件自己要不要字符。这是 Dialog Manager 判断「哪些键给控件、
/// 哪些键给对话框」的标准协议，所有正规文本输入控件（标准 EDIT/RichEdit、
/// Chromium 的网页输入框、Qt 的 QLineEdit、Office 的编辑区……）都会响应
/// DLGC_WANTCHARS / DLGC_WANTALLKEYS。类名、caret 那些信号对自绘输入框
/// （尤其浏览器）经常是缺的，只有这个协议最通用、也最可靠。
unsafe fn focus_wants_chars(h: HWND) -> bool {
    if h.is_invalid() || !IsWindow(h).as_bool() {
        return false;
    }
    // WM_GETDLGCODE 的 wparam 是「触发查询的虚拟键码」，传 VK_SPACE：
    // 问控件「按空格时你要不要自己处理（要字符）」。
    // 返回 DLGC_WANTCHARS / DLGC_WANTALLKEYS 的控件就是文本输入框。
    // 用 SendMessageTimeout 防止跨进程回调卡死（挂起的进程不至于拖垮钩子线程）。
    let mut res: usize = 0;
    let r = SendMessageTimeoutW(
        h,
        WM_GETDLGCODE,
        WPARAM(VK_SPACE as usize),
        LPARAM(0),
        SMTO_ABORTIFHUNG | SMTO_BLOCK,
        50,
        Some(&mut res),
    );
    if r.0 == 0 {
        // 目标窗口挂了或没响应：保守按「不是输入框」处理，别误吞空格
        return false;
    }
    let code = res as u32;
    (code & DLGC_WANTCHARS) != 0 || (code & DLGC_WANTALLKEYS) != 0
}

/// 键盘此刻是否应该让给输入框。
///
/// 光看 textedit::is_typing()（我方状态）不够稳：个别时序下（输入框刚弹出、
/// 或编辑框被销毁前后）标志可能还没翻，空格就会漏到「收纳/展开工具条」。
/// 而且用户经常是在**别的程序**的输入框里打字，那些窗口类名各不相同。
/// 这里用几层系统状态交叉判断：
///   1) 我方文字/水印输入框窗口存在；
///   2) 前台窗口本身就是我们的输入框；
///   3) 焦点控件通过 WM_GETDLGCODE 声明要接收字符（覆盖浏览器/Qt 等自绘输入框）；
///   4) 前台线程存在文本插入符（caret）；
///   5) 焦点控件 / 前台窗口的类名属于已知编辑控件。
unsafe fn keyboard_owned_by_edit() -> bool {
    if crate::textedit::is_typing() {
        return true;
    }
    let fg = GetForegroundWindow();
    if class_is(fg, "ScreenMarkerTextEdit") || class_is(fg, "ScreenMarkerInput") {
        return true;
    }

    let mut gti = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    if GetGUIThreadInfo(0, &mut gti).is_ok() {
        // caret 正在闪烁，或者有窗口持有文本插入符 → 光标就在某个输入框中
        if (gti.flags.0 & GUI_CARETBLINKING.0) != 0 {
            return true;
        }
        if !gti.hwndCaret.is_invalid() && IsWindow(gti.hwndCaret).as_bool() {
            return true;
        }
        if focus_wants_chars(gti.hwndFocus) {
            return true;
        }
        if is_edit_class(&class_name(gti.hwndFocus)) {
            return true;
        }
    }
    // 兜底：连前台窗口本身也问一遍（有些程序的焦点控件藏得很深）
    if focus_wants_chars(fg) {
        return true;
    }
    is_edit_class(&class_name(fg))
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        let state_ptr = G_STATE_PTR as *const RefCell<AppState>;
        let canvas_hwnd = HWND(G_CANVAS_PTR as *mut _);

        // 只响应「按下」。按住不放时系统会连发 WM_KEYDOWN，
        // 用一个闩锁保证长按只触发一次（否则空格会来回抖动）。
        let is_down = wparam.0 as u32 == WM_KEYDOWN || wparam.0 as u32 == WM_SYSKEYDOWN;
        let is_up = wparam.0 as u32 == WM_KEYUP || wparam.0 as u32 == WM_SYSKEYUP;
        if is_up {
            // 只清「抬起的那颗键」对应的闩锁。任意键 keyup 都把三个闩锁全清，
            // 会让按住右侧 Ctrl 时敲别的键提前复位 Ctrl 闩锁，下一次自动重复
            // 又被当成新手势 → 工具条来回抖动。
            match kb.vkCode {
                VK_RCTRL => SPACE_LATCH = false,
                VK_ESCAPE => ESC_LATCH = false,
                VK_DELETE | VK_BACK => DEL_LATCH = false,
                _ => {}
            }
        }

        // 右侧 Ctrl：收纳 / 展开工具条（替代原来的空格）
        // 与空格不同，右侧 Ctrl 是修饰键，本身不会往输入框里输入字符，
        // 所以不需要 keyboard_owned_by_edit() 让路；但要防止它作为 Ctrl+C/V
        // 等组合键的一部分时误触发 —— 只有「单独按下右侧 Ctrl」才算数。
        if is_down && kb.vkCode == VK_RCTRL && !state_ptr.is_null() {
            // 组合键判定：除了右侧 Ctrl 自身，其他修饰键（Shift/Alt/左Ctrl/Win）
            // 或任何主键区按键正在按下时，都视为组合键，不触发工具条。
            let alone = !crate::canvas::is_click_through()
                && (GetAsyncKeyState(VK_SHIFT.0 as i32) as u16 & 0x8000) == 0
                && (GetAsyncKeyState(VK_MENU.0 as i32) as u16 & 0x8000) == 0
                && (GetAsyncKeyState(VK_LCONTROL.0 as i32) as u16 & 0x8000) == 0
                && (GetAsyncKeyState(VK_LWIN.0 as i32) as u16 & 0x8000) == 0
                && (GetAsyncKeyState(VK_RWIN.0 as i32) as u16 & 0x8000) == 0;
            if alone {
                let fresh = is_fresh(SPACE_LATCH, SPACE_LAST, kb.time);
                SPACE_LAST = kb.time;
                if fresh {
                    SPACE_LATCH = true;
                    post_action(canvas_hwnd, HOOK_TOGGLE, 0);
                }
            } else {
                // 组合键中的右侧 Ctrl 不属于我们：不碰工具条，也不更新闩锁。
                SPACE_LATCH = false;
            }
        }

        // Esc：收起画布并退出当前工具（输入文字时交给输入框自己处理）
        if is_down && kb.vkCode == VK_ESCAPE && !state_ptr.is_null() {
            let fresh = is_fresh(ESC_LATCH, ESC_LAST, kb.time);
            ESC_LAST = kb.time;
            if fresh {
                ESC_LATCH = true;
                // Esc = 整体退出标注（画布收起、工具条收回主图标），图形留着。
                // 是否让给输入框在窗口过程里判断（要跨进程查询，不能放进钩子）。
                post_action(canvas_hwnd, HOOK_ESC, 0);
            }
        }

        // Del / Backspace：删掉当前选中的那个图形。
        //
        // 这里不能复用 keyboard_owned_by_edit()：那套判断原本是给「空格别在
        // 打字时误触工具条」设计的，会把「前台线程存在有效 hwndCaret」也算成
        // 输入框。实测 PowerPoint 的 mdiClass 容器在普通浏览状态下就带一个有效
        // hwndCaret（flags=0、WM_GETDLGCODE=0，其实并没有在编辑文字），于是
        // PPT 一打开，选中图形后按 Del/Backspace 完全没反应。
        // 删除是「作用于我们自己选中的图元」的动作，只在我方输入框打字时让路。
        if is_down
            && (kb.vkCode == VK_DELETE || kb.vkCode == VK_BACK)
            && !state_ptr.is_null()
        {
            let fresh = is_fresh(DEL_LATCH, DEL_LAST, kb.time);
            DEL_LAST = kb.time;
            if fresh {
                DEL_LATCH = true;
                let ours = DEL_OWNED_BY_US && !crate::textedit::is_typing();
                // 吞键必须在钩子回调里同步决定，所以只做「本地轻量判定」：
                // 最近一次点击属于我们 + 没在打字 + 确实有选中图元。
                // 真正的删除（会整屏重绘）投给窗口过程异步执行。
                if ours && crate::canvas::has_selection() {
                    post_action(canvas_hwnd, HOOK_DELETE, kb.vkCode as isize);
                    return LRESULT(1);
                }
            }
        }
    }
    CallNextHookEx(KEYBOARD_HOOK, code, wparam, lparam)
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let mouse_msg = wparam.0 as u32;
        // 任何一次鼠标按下，先假定「这次点击是给下面那个程序的」。
        // 钩子回调发生在系统把消息派发给目标窗口之前：若点击真的落到画布 /
        // 工具条，它们的窗口过程随后会调 note_our_window_clicked() 置回 true；
        // 穿透到下面程序的点击（穿透态空白区域）则永远保持 false。
        if mouse_msg == WM_LBUTTONDOWN_C
            || mouse_msg == WM_RBUTTONDOWN_C
            || mouse_msg == WM_MBUTTONDOWN_C
        {
            DEL_OWNED_BY_US = false;
        }
        // 滚轮：调线宽。只在「工具条展开 + 非穿透态 + 已选工具 + 画布可见」时生效，
        // 否则用户在普通窗口里滚动页面也会被我们改掉粗细。
        //
        // 收纳态原先 tool = Select，被这里当成「工具仍激活」，于是每滚一格都在
        // 偷偷调线宽并整屏重绘（UpdateLayeredWindow）；穿透态的画布又靠
        // HTTRANSPARENT 让位，反复重绘会反复触发命中测试，Windows 为此播放
        // 默认提示音 —— 表现就是「收纳状态下滚动鼠标会蜂鸣」。
        // 现在收纳态 tool 已是 None，再加这道闸门防止别的路径把它设回去。
        if mouse_msg == WM_MOUSEWHEEL {
            let state_ptr = G_STATE_PTR as *const RefCell<AppState>;
            if !state_ptr.is_null() {
                let ms = &*(lparam.0 as *const MSLLHOOKSTRUCT);
                // mouseData 的高 16 位是有符号滚轮增量
                let delta = ((ms.mouseData >> 16) & 0xFFFF) as u16 as i16;
                let canvas_hwnd = HWND(G_CANVAS_PTR as *mut _);
                let tool = (*state_ptr)
                    .try_borrow()
                    .map(|st| st.tool)
                    .unwrap_or(crate::shape::Tool::None);
                let active = tool != crate::shape::Tool::None;
                let expanded = crate::toolbar::is_expanded();
                let through = crate::canvas::is_click_through();
                let visible = !canvas_hwnd.is_invalid()
                    && IsWindowVisible(canvas_hwnd).as_bool();
                let handle = expanded && !through && active && visible;
                if handle {
                    // 滚轮分工（在 dispatch 里执行）：
                    //   放大镜/聚光灯  → 调取景范围（半径）
                    //   放大镜 + Ctrl  → 调放大倍率
                    //   马赛克         → 调区域大小（重新抓屏打码）
                    //   选择态         → 看选中的是什么，套用上面同一套
                    //   其它工具       → 调线宽/字号（序号的大小也由线宽派生）
                    let ctrl = (GetKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000) != 0;
                    let up = delta > 0;
                    let param = (up as isize) | ((ctrl as isize) << 1);
                    post_action(canvas_hwnd, HOOK_WHEEL, param);
                }
            }
        }
    }
    let _ = lparam;
    CallNextHookEx(MOUSE_HOOK, code, wparam, lparam)
}

pub fn install(
    hinstance: HINSTANCE,
    state: std::rc::Rc<RefCell<AppState>>,
    canvas_hwnd: HWND,
    toolbar_hwnd: HWND,
) -> windows::core::Result<()> {
    unsafe {
        G_STATE_PTR = std::rc::Rc::as_ptr(&state) as *const RefCell<AppState> as usize;
        G_CANVAS_PTR = canvas_hwnd.0 as usize;
        G_TOOLBAR_PTR = toolbar_hwnd.0 as usize;

        KEYBOARD_HOOK = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), hinstance, 0)?;
        // 第二个钩子安装失败时要把第一个卸掉，否则启动失败也会留下一个全局钩子。
        MOUSE_HOOK = match SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), hinstance, 0) {
            Ok(h) => h,
            Err(e) => {
                let _ = UnhookWindowsHookEx(KEYBOARD_HOOK);
                KEYBOARD_HOOK = HHOOK(ptr::null_mut());
                return Err(e);
            }
        };
        Ok(())
    }
}

pub fn uninstall() {
    unsafe {
        if !KEYBOARD_HOOK.is_invalid() {
            let _ = UnhookWindowsHookEx(KEYBOARD_HOOK);
        }
        if !MOUSE_HOOK.is_invalid() {
            let _ = UnhookWindowsHookEx(MOUSE_HOOK);
        }
    }
}
