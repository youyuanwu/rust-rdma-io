use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=wrapper/wrapper.c");
    println!("cargo:rerun-if-changed=wrapper/wrapper.h");

    let bindings = bindgen::Builder::default()
        .header_contents(
            "rdma.h",
            "#include <infiniband/verbs.h>\n#include <rdma/rdma_cma.h>\n#include \"wrapper/wrapper.h\"\n",
        )
        .clang_arg("-I.")
        .allowlist_type("(ibv|ib_uverbs|rdma|verbs)_.*")
        .allowlist_function("(ibv|_ibv|rdma)_.*|(mbps|mult)_to_ibv_rate")
        .allowlist_var("(IBV|IB_UVERBS|RDMA|RAI|IB_PATH)_.*")
        .blocklist_type("(sockaddr|sockaddr_in|sockaddr_in6|sockaddr_storage|in_addr|in6_addr|socklen_t|timespec|pthread_mutex_t|pthread_cond_t)")
        .raw_line("use libc::{sockaddr, sockaddr_in, sockaddr_in6, sockaddr_storage, socklen_t, timespec, pthread_mutex_t, pthread_cond_t};")
        .prepend_enum_name(false)
        .derive_default(true)
        .derive_debug(false)
        .generate_comments(false)
        .wrap_unsafe_ops(true)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("failed to generate RDMA bindings; install libclang-dev, libibverbs-dev and librdmacm-dev");
    bindings
        .write_to_file(
            PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is unset")).join("bindings.rs"),
        )
        .expect("failed to write RDMA bindings");

    cc::Build::new()
        .file("wrapper/wrapper.c")
        .compile("rdma_wrapper");

    println!("cargo:rustc-link-lib=ibverbs");
    println!("cargo:rustc-link-lib=rdmacm");
}
