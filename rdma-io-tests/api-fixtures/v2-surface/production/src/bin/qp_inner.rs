fn removed(qp: &rdma_io::v2::Qp) {
    let _ = qp.inner();
}
fn main() {}
