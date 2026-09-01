fn removed(completion: &rdma_io::wc::WorkCompletion) {
    let _ = rdma_io::v2::Qp::check_completion(completion);
}
fn main() {}
