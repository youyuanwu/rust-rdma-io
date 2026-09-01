fn removed(cm: &rdma_io::cm::CmId) {
    let _ = rdma_io::v2::Context::from_cm(cm);
}
fn main() {}
