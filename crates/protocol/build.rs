//! Generates Rust bindings from the canonical protobuf schemas in `proto/`
//! (docs/30). Generated code lands in the cargo OUT_DIR; the checked-in TS
//! bindings in packages/surface-protocol/src/generated are regenerated from
//! the same schema source and CI asserts they match (docs/70).
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?).join("../..");
    let proto_root = root.join("proto");
    let files = [
        "modbit/protocol/v1/common.proto",
        "modbit/protocol/v1/domain.proto",
        "modbit/protocol/v1/commands.proto",
        "modbit/protocol/v1/events.proto",
        "modbit/protocol/v1/transport.proto",
    ]
    .map(|f| proto_root.join(f));

    prost_build::Config::new().compile_protos(&files, &[proto_root])?;
    Ok(())
}
