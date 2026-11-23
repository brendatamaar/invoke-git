use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=assets/invoke-git.rc");
    println!("cargo:rerun-if-changed=assets/invoke-git.ico");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("invoke-git.res");
    let status = Command::new(resource_compiler())
        .current_dir("assets")
        .arg("/nologo")
        .arg("/fo")
        .arg(&output)
        .arg("invoke-git.rc")
        .status()
        .expect("run rc.exe from the Windows SDK");
    assert!(status.success(), "compile the Invoke Git icon resource");
    println!("cargo:rustc-link-arg-bin=invoke-git={}", output.display());
}

fn resource_compiler() -> PathBuf {
    // A developer command prompt already has rc.exe on PATH. Plain Cargo
    // invocations can find it in the Visual Studio Windows SDK installation.
    let arch = match env::var("HOST").unwrap().split('-').next() {
        Some("x86_64") => "x64",
        Some("aarch64") => "arm64",
        _ => "x86",
    };
    let sdk_compiler = env::var_os("ProgramFiles(x86)")
        .map(PathBuf::from)
        .and_then(|root| root.join("Windows Kits/10/bin").read_dir().ok())
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let version = entry
                .file_name()
                .to_str()?
                .split('.')
                .map(str::parse::<u32>)
                .collect::<Result<Vec<_>, _>>()
                .ok()?;
            let compiler = entry.path().join(arch).join("rc.exe");
            compiler.is_file().then_some((version, compiler))
        })
        .max_by(|(left, _), (right, _)| left.cmp(right))
        .map(|(_, compiler)| compiler);
    sdk_compiler.unwrap_or_else(|| PathBuf::from("rc.exe"))
}
