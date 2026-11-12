// 把 app.ico 作为 RT_GROUP_ICON 直接链接进 exe。
//
// 以前只在 build.bat 里用 _seticon.py 事后改 PE，一旦直接 `cargo build`
// 就会重新链接、把图标资源丢掉（资源段变空，资源管理器显示默认图标）。
// 这里在链接期就把 .res 交给链接器，任何构建方式都不会再丢。
// 找不到 windres 时静默跳过，仍由 _seticon.py 兜底，不影响编译。

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=app.ico");

    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let ico = manifest.join("app.ico");
    let rc_path = out.join("app_icon.rc");
    let res_path = out.join("app_icon.res");

    // windres 对反斜杠路径会当转义符，统一换成正斜杠
    let ico_arg = ico.to_string_lossy().replace('\\', "/");
    let _ = std::fs::write(&rc_path, format!("1 ICON \"{}\"\n", ico_arg));

    let candidates = [
        "windres",
        "D:/Program Files/w64devkit/bin/windres.exe",
        "D:/Program Files/mingw64/bin/windres.exe",
    ];
    for exe in candidates {
        let ok = Command::new(exe)
            .arg("-i")
            .arg(&rc_path)
            .arg("-o")
            .arg(&res_path)
            .arg("-O")
            .arg("coff")
            .arg("--target=pe-x86-64")
            .status()
            .map(|s| s.success() && res_path.exists())
            .unwrap_or(false);
        if ok {
            println!("cargo:rustc-link-arg={}", res_path.to_string_lossy().replace('\\', "/"));
            return;
        }
    }
    println!("cargo:warning=windres 不可用：图标将由 build.bat 的 _seticon.py 在构建后写入");
}
