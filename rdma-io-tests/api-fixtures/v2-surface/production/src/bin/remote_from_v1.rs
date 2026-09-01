fn removed(remote: rdma_io::mr::RemoteMr) {
    let _ = rdma_io::v2::RemoteMr::from_v1(remote);
}
fn main() {}
