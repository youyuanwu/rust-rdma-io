fn removed(pd: &rdma_io::v2::Pd) {
    let _ = pd.inner();
}
fn main() {}
