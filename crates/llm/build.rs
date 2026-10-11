//! On macOS, link clang's compiler-rt builtins into whatever links this crate.
//!
//! ggml-metal checks `@available(macOS 15, *)`. With a deployment target below
//! that (Tauri exports `MACOSX_DEPLOYMENT_TARGET=14.4` from `minimumSystemVersion`
//! during `tauri build`) clang lowers the check to a call to
//! `__isPlatformVersionAtLeast`, which lives in `libclang_rt.osx.a`. rustc links
//! with `-nodefaultlibs`, so the archive has to be named explicitly or the final
//! link fails with an undefined symbol. Only the needed members are pulled in.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CC");
    #[cfg(target_os = "macos")]
    link_compiler_rt();
}

#[cfg(target_os = "macos")]
fn link_compiler_rt() {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
    let dir: Option<PathBuf> = [cc.as_str(), "clang"].iter().find_map(|compiler| {
        let out = Command::new(compiler).arg("--print-file-name=libclang_rt.osx.a").output().ok()?;
        let printed = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let path = Path::new(&printed);
        if path.is_file() { path.parent().map(Path::to_path_buf) } else { None }
    });
    match dir {
        Some(dir) => {
            println!("cargo:rustc-link-search=native={}", dir.display());
            println!("cargo:rustc-link-lib=static=clang_rt.osx");
        }
        None => println!(
            "cargo:warning=libclang_rt.osx.a not found; linking may fail with MACOSX_DEPLOYMENT_TARGET below 15 (undefined ___isPlatformVersionAtLeast)"
        ),
    }
}
