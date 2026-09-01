fn removed(completion: &rdma_io::v2::Completion) {
    let _ = completion.as_wc();
}
fn main() {}
