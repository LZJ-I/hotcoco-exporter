use std::{env, fs, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() != "windows" {
        return;
    }
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let mut icon = vec![0, 0, 1, 0, 1, 0, 32, 32, 0, 0, 1, 0, 32, 0];
    icon.extend_from_slice(&(40u32 + 32 * 32 * 4 + 128).to_le_bytes());
    icon.extend_from_slice(&22u32.to_le_bytes());
    icon.extend_from_slice(&40u32.to_le_bytes());
    icon.extend_from_slice(&32i32.to_le_bytes());
    icon.extend_from_slice(&64i32.to_le_bytes());
    icon.extend_from_slice(&1u16.to_le_bytes());
    icon.extend_from_slice(&32u16.to_le_bytes());
    icon.extend_from_slice(&[0; 24]);
    for y in (0..32i32).rev() {
        for x in 0..32i32 {
            let inside = (4..28).contains(&x)
                || (4..28).contains(&y)
                || (x - if x < 16 { 4 } else { 27 }).pow(2)
                    + (y - if y < 16 { 4 } else { 27 }).pow(2)
                    <= 16;
            let h = (8..11).contains(&x) && (8..24).contains(&y)
                || (14..17).contains(&x) && (8..24).contains(&y)
                || (8..17).contains(&x) && (14..17).contains(&y);
            let c = (21..26).contains(&x) && ((8..11).contains(&y) || (21..24).contains(&y))
                || (19..22).contains(&x) && (10..22).contains(&y);
            icon.extend_from_slice(if !inside {
                &[0, 0, 0, 0]
            } else if h || c {
                &[255, 255, 255, 255]
            } else {
                &[74, 116, 242, 255]
            });
        }
    }
    icon.extend_from_slice(&[0; 128]);
    fs::write(out.join("app.ico"), icon).unwrap();
    fs::write(out.join("app.manifest"), r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
<assemblyIdentity version="0.2.0.0" processorArchitecture="*" name="HotCoco.Exporter" type="win32"/>
<description>HotCoco Resource Exporter</description>
<trustInfo xmlns="urn:schemas-microsoft-com:asm.v3"><security><requestedPrivileges><requestedExecutionLevel level="asInvoker" uiAccess="false"/></requestedPrivileges></security></trustInfo>
<dependency><dependentAssembly><assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0" processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/></dependentAssembly></dependency>
<application xmlns="urn:schemas-microsoft-com:asm.v3"><windowsSettings><dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness><longPathAware xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">true</longPathAware></windowsSettings></application>
</assembly>"#).unwrap();
    fs::write(
        out.join("app.rc"),
        r#"1 ICON "app.ico"
1 24 "app.manifest"
1 VERSIONINFO
FILEVERSION 0,2,0,0
PRODUCTVERSION 0,2,0,0
FILEOS 0x40004
FILETYPE 0x1
BEGIN
 BLOCK "StringFileInfo"
 BEGIN
  BLOCK "040904B0"
  BEGIN
   VALUE "FileDescription", "HotCoco Resource Exporter\0"
   VALUE "FileVersion", "0.2.0\0"
   VALUE "ProductName", "HotCoco Exporter\0"
   VALUE "ProductVersion", "0.2.0\0"
   VALUE "OriginalFilename", "HotCoco-Exporter.exe\0"
  END
 END
 BLOCK "VarFileInfo"
 BEGIN
  VALUE "Translation", 0x0409, 1200
 END
END
"#,
    )
    .unwrap();
    if env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default() == "gnu" {
        assert!(Command::new("x86_64-w64-mingw32-windres")
            .current_dir(&out)
            .args(["app.rc", "-O", "coff", "-o", "app.o"])
            .status()
            .unwrap()
            .success());
        println!("cargo:rustc-link-arg={}", out.join("app.o").display());
        println!("cargo:rustc-link-arg=-static");
    } else {
        assert!(Command::new("rc.exe")
            .current_dir(&out)
            .args(["/nologo", "/fo", "app.res", "app.rc"])
            .status()
            .unwrap()
            .success());
        println!("cargo:rustc-link-arg={}", out.join("app.res").display());
    }
}
