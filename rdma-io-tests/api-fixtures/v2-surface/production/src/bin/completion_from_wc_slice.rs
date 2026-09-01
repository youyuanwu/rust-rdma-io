fn removed(completions: &[rdma_io::wc::WorkCompletion]) {
    let _ = rdma_io::v2::Completion::from_wc_slice(completions);
}
fn main() {}
