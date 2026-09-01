fn removed(context: std::sync::Arc<rdma_io::device::Context>) {
    let _ = rdma_io::v2::Context::from_inner(context);
}
fn main() {}
