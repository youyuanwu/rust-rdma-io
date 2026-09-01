fn removed<N: rdma_io::v2::CqNotifier>(
    completions: &mut rdma_io::v2::Completions<N>,
    context: &mut std::task::Context<'_>,
) {
    let mut buffer = [rdma_io::v2::Completion::default(); 1];
    let _ = completions.poll_next(context, &mut buffer);
}
fn main() {}
