// 极简调试日志：仅在设置了环境变量 SM_LOG 时才写文件。
// GUI 程序没有控制台，出问题时靠它定位。

#![allow(dead_code)]

use std::fs::OpenOptions;
use std::io::Write;

pub fn log(msg: &str) {
    if std::env::var_os("SM_LOG").is_none() {
        return;
    }
    let path = std::env::var("SM_LOG").unwrap_or_else(|_| "dbg.log".to_string());
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{}", msg);
    }
}
