// 程序图标：把 app.ico 直接嵌进 exe，运行时用 CreateIconFromResourceEx 造出 HICON。
//
// 为什么不走 .rc + build.rs：那要额外依赖 windres，而本机是 gnu 工具链，
// 编译链上多一个外部依赖就多一个出错点。include_bytes! + 运行时解析更稳，
// 且 exe 仍然是单文件。

use windows::core::*;
use windows::Win32::UI::WindowsAndMessaging::*;

const ICO: &[u8] = include_bytes!("../app.ico");

/// 从内嵌的 ICO 里挑一个最合适的尺寸，返回 HICON（失败返回 null，调用方可用默认图标）
pub fn load() -> HICON {
    load_sized(0, 0)
}

/// 指定期望尺寸（0,0 = 系统默认）。托盘图标要用 SM_CXSMICON 取，否则会被拉伸糊掉
pub fn load_sized(cx: i32, cy: i32) -> HICON {
    unsafe {
        let p = ICO.as_ptr();
        // 返回的是「该图像数据相对文件起始的偏移」
        let off = LookupIconIdFromDirectoryEx(p, true, cx, cy, LR_DEFAULTCOLOR);
        if off <= 0 {
            return HICON::default();
        }
        let off = off as usize;
        if off >= ICO.len() {
            return HICON::default();
        }

        // 在 ICONDIR 里找到对应条目，取出这一帧的字节数
        let count = u16::from_le_bytes([ICO[4], ICO[5]]) as usize;
        let mut size = 0u32;
        for i in 0..count {
            let e = 6 + i * 16;
            if e + 16 > ICO.len() {
                break;
            }
            let bytes = u32::from_le_bytes([ICO[e + 8], ICO[e + 9], ICO[e + 10], ICO[e + 11]]);
            let o = u32::from_le_bytes([ICO[e + 12], ICO[e + 13], ICO[e + 14], ICO[e + 15]]);
            if o as usize == off {
                size = bytes;
                break;
            }
        }
        if size == 0 {
            return HICON::default();
        }

        // windows crate 的封装把「指针 + 长度」收成了一个切片，flags 仍是必填
        let end = (off + size as usize).min(ICO.len());
        CreateIconFromResourceEx(&ICO[off..end], true, 0x0003_0000, cx, cy, LR_DEFAULTCOLOR)
            .unwrap_or_default()
    }
}
