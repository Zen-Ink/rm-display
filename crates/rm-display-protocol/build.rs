fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto = "../../protocol/rm_display/v2/rm_display.proto";
    let include = "../../protocol";

    println!("cargo:rerun-if-changed={proto}");

    let mut config = prost_build::Config::new();
    config.bytes([".rm_display.v2"]);
    config.compile_protos(&[proto], &[include])?;
    Ok(())
}
