fn main() {
    cc::Build::new()
        .file("wrapper/wrapper.c")
        .include("/usr/include")
        .compile("rdma_wrapper");

    println!("cargo:rustc-link-lib=ibverbs");
    println!("cargo:rustc-link-lib=rdmacm");
}
