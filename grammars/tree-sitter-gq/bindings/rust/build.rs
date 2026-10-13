fn main() {
    let src = std::path::Path::new("src");
    cc::Build::new()
        .std("c11")
        .include(src)
        .flag_if_supported("-Wno-unused-parameter")
        .file(src.join("parser.c"))
        .compile("tree-sitter-gq");
    println!("cargo:rerun-if-changed=src/parser.c");
}
