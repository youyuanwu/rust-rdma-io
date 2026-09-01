fn main() {
    let _ = rdma_io::v2::RdmaConnectionConfig::default().max_send_wr(8);
}
