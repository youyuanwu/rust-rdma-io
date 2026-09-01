fn removed(completions: &mut [rdma_io::wc::WorkCompletion]) {
    let _ = rdma_io::v2::Completion::from_wc_slice_mut(completions);
}
fn main() {}
