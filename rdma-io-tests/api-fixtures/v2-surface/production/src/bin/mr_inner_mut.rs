fn removed(mr: &mut rdma_io::v2::Mr) {
    let _ = mr.inner_mut();
}
fn main() {}
