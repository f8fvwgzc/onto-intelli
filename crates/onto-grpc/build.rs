//! Compiles the built-in proto with protox (pure Rust: no `protoc` needed on
//! build machines), embeds its descriptor set for the dynamic server and
//! reflection, and generates a typed Rust client.

use std::path::PathBuf;

use prost::Message;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("proto");
    let file = "onto_intelli/v1/onto_intelli.proto";
    println!("cargo:rerun-if-changed={}", root.join(file).display());

    let fds = protox::compile([file], [&root])?;
    let out = PathBuf::from(std::env::var("OUT_DIR")?);
    std::fs::write(out.join("onto_intelli_descriptor.bin"), fds.encode_to_vec())?;

    tonic_prost_build::configure().build_server(false).build_client(true).compile_fds(fds)?;
    embed_ui(&out)?;
    Ok(())
}

/// Embed the built web UI (`ui-dist/`, produced by `npm run build` in `ui/`)
/// so the binary serves it with no files on disk. A checkout without a built
/// UI still compiles; `/ui` then explains how to build it.
fn embed_ui(out: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let dist = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("ui-dist");
    println!("cargo:rerun-if-changed={}", dist.display());
    let mut files = Vec::new();
    if dist.is_dir() {
        let mut stack = vec![dist.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir)? {
                let path = entry?.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    let rel = path.strip_prefix(&dist)?.to_string_lossy().replace('\\', "/");
                    files.push((rel, path));
                }
            }
        }
    }
    files.sort();
    let mut code = String::from("pub static FILES: &[(&str, &[u8])] = &[\n");
    for (rel, path) in files {
        code.push_str(&format!("    ({rel:?}, include_bytes!({:?})),\n", path.display().to_string()));
    }
    code.push_str("];\n");
    std::fs::write(out.join("ui_assets.rs"), code)?;
    Ok(())
}
