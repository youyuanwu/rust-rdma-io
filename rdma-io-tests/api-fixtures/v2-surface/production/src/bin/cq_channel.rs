fn removed(cq: &rdma_io::v2::Cq) {
    let _ = cq.channel();
}
fn main() {}
