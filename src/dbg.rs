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

/// 把一张 32 位 BGRA 图存成 BMP（仅调试期用，方便确认截屏到底抓到了什么）
pub fn dump_bmp(name: &str, data: &[u8], w: i32, h: i32) {
    if std::env::var_os("SM_LOG").is_none() || w <= 0 || h <= 0 {
        return;
    }
    let stride = (w * 4) as usize;
    let mut out = Vec::with_capacity(54 + stride * h as usize);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&((54 + stride * h as usize) as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&w.to_le_bytes());
    out.extend_from_slice(&(-h).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(stride * h as usize).to_le_bytes());
    out.extend_from_slice(&[0u8; 16]);
    out.extend_from_slice(&data[..(stride * h as usize).min(data.len())]);
    let _ = std::fs::write(name, out);
}
