fn removed(qp: &rdma_io::v2::Qp) {
    qp.submit(());
}
fn main() {}
