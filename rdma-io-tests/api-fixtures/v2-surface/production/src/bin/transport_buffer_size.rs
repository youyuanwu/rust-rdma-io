fn removed(transport: &rdma_io::v2::MessageTransport) {
    let _ = transport.buffer_size();
}
fn main() {}
