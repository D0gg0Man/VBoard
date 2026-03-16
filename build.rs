fn main() -> Result<(), nlprule_build::Error> {
    println!("cargo:rerun-if-changed=build.rs");
 
    // Downloads and validates the nlprule rule binaries for English and German.
    // Both are baked into the compiled binary via include_bytes! in main.rs,
    // but only the language matching config.model_size is parsed into RAM at
    // runtime — all other languages never touch these bytes.
    nlprule_build::BinaryBuilder::new(
        &["en", "de"],
        std::env::var("OUT_DIR").expect("OUT_DIR is set during build"),
    )
    .build()?
    .validate()
}
 
