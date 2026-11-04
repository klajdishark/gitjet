//! With `--features ai`, the model file named by GITJET_MODEL is compiled into the binary.

fn main() {
    println!("cargo:rerun-if-env-changed=GITJET_MODEL");
    if std::env::var_os("CARGO_FEATURE_AI").is_none() {
        return;
    }
    let Some(path) = std::env::var_os("GITJET_MODEL") else {
        panic!("--features ai needs the model: GITJET_MODEL=/path/to/gitjet-intent-<hash>.gguf");
    };
    let path = std::fs::canonicalize(&path).unwrap_or_else(|e| {
        panic!(
            "GITJET_MODEL={}: {e}",
            std::path::Path::new(&path).display()
        )
    });
    println!("cargo:rerun-if-changed={}", path.display());
    println!("cargo:rustc-env=GITJET_MODEL_PATH={}", path.display());
    let name = path.file_name().unwrap().to_string_lossy();
    println!("cargo:rustc-env=GITJET_MODEL_NAME={name}");
}
