fn removed(cq: &rdma_io::v2::Cq) {
    let mut completions = [rdma_io::wc::WorkCompletion::default(); 1];
    let _ = cq.poll(&mut completions);
}
fn main() {}
