fn main() {
    let source = "vendor/tree-sitter-commonlisp/src";
    cc::Build::new()
        .include(source)
        .file(format!("{source}/parser.c"))
        .flag_if_supported("-Wno-unused-parameter")
        .flag_if_supported("-Wno-unused-but-set-variable")
        .flag_if_supported("-Wno-trigraphs")
        .flag_if_supported("-utf-8")
        .compile("tilth_commonlisp");
    println!("cargo:rerun-if-changed={source}");
}
