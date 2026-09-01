fn removed(poller: rdma_io::v2::CqPoller) {
    let _ = poller.into_cq();
}
fn main() {}
