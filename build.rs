use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-env-changed=RC");
    println!("cargo:rerun-if-changed=assets/app.rc");
    println!("cargo:rerun-if-changed=assets/pausic.ico");
    println!("cargo:rerun-if-changed=app.manifest");

    let resource =
        PathBuf::from(env::var_os("OUT_DIR").expect("Cargo output directory")).join("pausic.res");
    let compiler = env::var_os("RC").unwrap_or_else(|| "rc.exe".into());
    let status = Command::new(compiler)
        .current_dir("assets")
        .arg("/nologo")
        .arg("/fo")
        .arg(&resource)
        .arg("app.rc")
        .status()
        .expect("Windows resource compiler is required; run from a Native Tools command prompt");
    assert!(status.success(), "Windows resource compilation failed");
    println!("cargo:rustc-link-arg-bin=Pausic={}", resource.display());
}
