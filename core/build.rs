fn main() {
    cc::Build::new()
        .file("vendor/kcp/ikcp.c")
        .include("vendor/kcp")
        .warnings(false)
        .compile("xiaomi_kcp");
    println!("cargo:rerun-if-changed=vendor/kcp/ikcp.c");
    println!("cargo:rerun-if-changed=vendor/kcp/ikcp.h");
}
