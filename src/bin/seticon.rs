//! 把 app.ico 的所有尺寸写进已编译 exe 的 PE 资源段（RT_ICON + RT_GROUP_ICON）。
//!
//! 用系统官方支持的 UpdateResource 系列 API 直接改现成 PE。
//! `build.rs` 会在链接期用 windres 嵌入图标，本工具只作兜底；重复写入按 ID
//! 覆盖，是幂等的，所以两条路径都走也没问题。
//!
//! 用法: seticon.exe [exe路径] [ico路径]

use std::ffi::c_void;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{BOOL, HANDLE};
use windows::Win32::System::LibraryLoader::{
    BeginUpdateResourceW, EndUpdateResourceW, UpdateResourceW,
};

const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
const LANG: u16 = 0x0409; // en-US

fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

// Win32 的 MAKEINTRESOURCE：资源 ID 为小整数时，直接把整数当指针值传。
fn int_res(id: u16) -> PCWSTR {
    PCWSTR(id as usize as *const u16)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn main() {
    if let Err(e) = run() {
        eprintln!("seticon: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    let exe = args
        .get(1)
        .map(String::as_str)
        .unwrap_or("target/release/screenmarker.exe");
    let ico = args.get(2).map(String::as_str).unwrap_or("app.ico");

    let data = std::fs::read(ico).map_err(|e| format!("read {ico}: {e}"))?;
    if data.len() < 6 || u16le(&data, 0) != 0 || u16le(&data, 2) != 1 {
        return Err(format!("{ico} is not a valid ICO"));
    }
    let count = u16le(&data, 4) as usize;
    if data.len() < 6 + count * 16 {
        return Err(format!("{ico}: truncated ICONDIR"));
    }

    // 组图标目录：ICONDIR 头 + 每个成员的 GRPICONDIRENTRY（末位是资源 ID）
    let mut grp = Vec::with_capacity(6 + count * 14);
    grp.extend_from_slice(&0u16.to_le_bytes());
    grp.extend_from_slice(&1u16.to_le_bytes());
    grp.extend_from_slice(&(count as u16).to_le_bytes());
    for i in 0..count {
        let off = 6 + i * 16;
        let bytes_in_res = u32le(&data, off + 8);
        let img_off = u32le(&data, off + 12);
        grp.extend_from_slice(&data[off..off + 8]); // 宽/高/色数/保留/平面/位深
        grp.extend_from_slice(&bytes_in_res.to_le_bytes());
        grp.extend_from_slice(&((i + 1) as u16).to_le_bytes());
        if img_off as usize + bytes_in_res as usize > data.len() {
            return Err(format!("{ico}: icon #{} out of bounds", i + 1));
        }
    }

    let exe_w = wide(exe);
    let h: HANDLE = unsafe { BeginUpdateResourceW(PCWSTR(exe_w.as_ptr()), BOOL(0)) }
        .map_err(|e| format!("BeginUpdateResource failed: {e}"))?;
    if h.0.is_null() {
        return Err("BeginUpdateResource returned a null handle".into());
    }

    let result = (|| -> Result<(), String> {
        for i in 0..count {
            let off = 6 + i * 16;
            let bytes_in_res = u32le(&data, off + 8) as usize;
            let img_off = u32le(&data, off + 12) as usize;
            let blob = &data[img_off..img_off + bytes_in_res];
            unsafe {
                UpdateResourceW(
                    h,
                    int_res(RT_ICON),
                    int_res((i + 1) as u16),
                    LANG,
                    Some(blob.as_ptr() as *const c_void),
                    blob.len() as u32,
                )
            }
            .map_err(|e| format!("UpdateResource RT_ICON #{} failed: {e}", i + 1))?;
        }
        unsafe {
            UpdateResourceW(
                h,
                int_res(RT_GROUP_ICON),
                int_res(1),
                LANG,
                Some(grp.as_ptr() as *const c_void),
                grp.len() as u32,
            )
        }
        .map_err(|e| format!("UpdateResource RT_GROUP_ICON failed: {e}"))?;
        Ok(())
    })();

    // 失败时丢弃本次更新，避免留下半截资源；成功才提交。
    let discard = result.is_err();
    unsafe { EndUpdateResourceW(h, BOOL(if discard { 1 } else { 0 })) }
        .map_err(|e| format!("EndUpdateResource failed: {e}"))?;
    result?;

    println!("seticon: wrote {} icon sizes -> {}", count, exe);
    Ok(())
}
