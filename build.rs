//! 构建脚本：生成多尺寸图标并嵌入 exe（含 Per-Monitor V2 DPI 清单——egui 清晰度的前提）。

#[path = "src/icon.rs"]
mod icon;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/icon.rs");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("缺少 OUT_DIR"));
    let ico = out_dir.join("app.ico");
    if let Err(e) = icon::write_ico(&ico, &[16, 20, 24, 32, 40, 48, 64, 128, 256]) {
        println!("cargo:warning=生成图标失败：{e}");
        return;
    }

    // Per-Monitor V2 + asInvoker：不声明 DPI 感知会被系统位图拉伸，界面发虚
    const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity version="1.0.0.0" processorArchitecture="*" name="Pek.RWindows.Launcher" type="win32"/>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">permonitorv2,permonitor</dpiAwareness>
    </windowsSettings>
  </application>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
    </application>
  </compatibility>
</assembly>
"#;

    let mut res = winresource::WindowsResource::new();
    res.set_manifest(MANIFEST);
    res.set_icon(ico.to_str().expect("图标路径含非法字符"));
    res.set("ProductName", "Pek Launcher");
    res.set("FileDescription", "Pek Launcher - app launcher for Windows");
    if let Err(e) = res.compile() {
        println!("cargo:warning=嵌入 Windows 资源失败（程序仍可运行，但无图标/清单）：{e}");
    }
}
