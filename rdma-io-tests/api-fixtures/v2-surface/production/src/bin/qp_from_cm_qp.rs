fn removed(qp: rdma_io::cm::CmQueuePair) {
    let _ = rdma_io::v2::Qp::from_cm_qp(qp);
}
fn main() {}
