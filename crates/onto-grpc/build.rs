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
    Ok(())
}
