fn main() -> Result<(), Box<dyn std::error::Error>> {
    configure_me_codegen::build_script_auto().unwrap();
    tonic_build::compile_protos("proto/service.proto")?;
    Ok(())
}
