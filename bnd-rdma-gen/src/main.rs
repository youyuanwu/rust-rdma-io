use std::path::PathBuf;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let workspace_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let rdma_io_sys_dir = workspace_dir.join("rdma-io-sys");

    bnd_rdma_gen::generate(&rdma_io_sys_dir);

    println!(
        "Generated rdma-io-sys bindings at {}",
        rdma_io_sys_dir.display()
    );
}
