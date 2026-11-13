// 极简调试日志：仅在设置了环境变量 SM_LOG 时才写文件。
// GUI 程序没有控制台，出问题时靠它定位。
//
// 两条约束（都是热路径逼出来的）：
//   1) 调用点一律用 logf!(...)——开关没开时连 format! 都不求值；
//      旧的 log(&format!(...)) 会先拼好字符串再进函数判断开关，滚轮每格都白干。
//   2) 文件句柄进程内复用，不再每条日志 open/append/close 一次。

#![allow(dead_code)]

use std::fs::File;
use std::io::Write;
use std::sync::{Mutex, OnceLock};

/// 进程内只打开一次日志文件。None = SM_LOG 没开或打开失败。
static LOGGER: OnceLock<Option<Mutex<File>>> = OnceLock::new();

/// 开关只探测一次环境变量（进程生命周期内不会变）。
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("SM_LOG").is_some())
}

fn file() -> Option<&'static Mutex<File>> {
    LOGGER
        .get_or_init(|| {
            if !enabled() {
                return None;
            }
            let path = std::env::var("SM_LOG").unwrap_or_else(|_| "dbg.log".to_string());
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok()
                .map(Mutex::new)
        })
        .as_ref()
}

/// 写一行已格式化的文本；给纯字符串常量用。
pub fn log(msg: &str) {
    if let Some(f) = file() {
        if let Ok(mut f) = f.lock() {
            let _ = writeln!(f, "{}", msg);
        }
    }
}

/// 惰性格式化日志：参数与 format! 相同，只在 SM_LOG 开启时才求值。
macro_rules! logf {
    ($($arg:tt)*) => {{
        if $crate::dbg::enabled() {
            $crate::dbg::log(&format!($($arg)*));
        }
    }};
}
pub(crate) use logf;
