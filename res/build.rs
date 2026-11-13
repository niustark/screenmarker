// 把 res/app.ico 作为 RT_GROUP_ICON 在链接期直接嵌进 exe。
//
// 由 GNU 资源编译器 windres 生成 .res，再交给链接器合并进 .rsrc。
// 因此一条 `cargo build --release` 就能产出自带图标的程序，
// 不需要额外的构建脚本，也不需要在编译后改写 PE。
//
// 注意：下面的路径都相对「包根」（CARGO_MANIFEST_DIR），不是相对本文件所在的 res/。

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=res/app.ico");

    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let ico = manifest.join("res").join("app.ico");
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
    println!("cargo:warning=windres 不可用：本次构建不会嵌入程序图标（不影响功能）");
}
