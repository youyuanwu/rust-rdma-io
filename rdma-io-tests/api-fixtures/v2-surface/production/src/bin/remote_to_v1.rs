fn removed(remote: &rdma_io::v2::RemoteMr) {
    let _ = remote.to_v1();
}
fn main() {}
