fn removed(mr: &rdma_io::v2::Mr) {
    let _ = mr.inner();
}
fn main() {}
