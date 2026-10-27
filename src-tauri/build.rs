fn main() {
    // tauri-build 只对 tauri.conf.json / capabilities 声明 rerun-if-changed，不含图标；
    // 不补这条的话，换了 icons/ 下的图标构建脚本不会重跑，exe 里永远嵌旧图标。
    let manifest_dir =
        std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    for icon in ["icon.ico", "icon.png"] {
        println!(
            "cargo:rerun-if-changed={}",
            manifest_dir.join("icons").join(icon).display()
        );
    }

    // 默认 manifest 只会被 tauri-build 链接进 bin；测试 target 没有它时会加载 comctl32 v5，
    // 进程在解析 TaskDialogIndirect 导入时直接退出。改为对所有最终产物显式链接同一份 manifest。
    let attributes = tauri_build::Attributes::new()
        .windows_attributes(tauri_build::WindowsAttributes::new_without_app_manifest());
    tauri_build::try_build(attributes).expect("tauri-build");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let manifest =
            std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"))
                .join("windows-app.manifest");
        println!("cargo:rerun-if-changed={}", manifest.display());
        println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg=/MANIFESTINPUT:{}", manifest.display());
    }
}
