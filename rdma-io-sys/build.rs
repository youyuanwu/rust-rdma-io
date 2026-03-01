fn main() {
    cc::Build::new()
        .file("wrapper/wrapper.c")
        .include("/usr/include")
        .compile("rdma_wrapper");
}
