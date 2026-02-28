//! Safe API (rdma-io) integration tests against SoftIWarp.

use rdma_io::device;
use rdma_io::mr::AccessFlags;
use rdma_io::qp::QpInitAttr;
use rdma_io::wc::WorkCompletion;
use rdma_io::wr::QpType;
use std::sync::Arc;

fn require_siw() -> Arc<rdma_io::device::Context> {
    let ctx = device::open_device_by_name("siw0")
        .or_else(|_| device::open_first_device())
        .expect("No RDMA device available");
    Arc::new(ctx)
}

#[test]
fn safe_device_enumeration() {
    let devs = device::devices().expect("devices() failed");
    assert!(!devs.is_empty(), "No RDMA devices found");
    for d in &devs {
        println!("Device: {} (guid={:#x})", d.name(), d.guid());
    }
}

#[test]
fn safe_open_device() {
    let ctx = require_siw();
    let attr = ctx.query_device().expect("query_device failed");
    println!(
        "max_qp={}, max_cq={}, max_mr={}",
        attr.max_qp, attr.max_cq, attr.max_mr
    );
    assert!(attr.max_qp > 0);
}

#[test]
fn safe_query_port() {
    let ctx = require_siw();
    let port_attr = ctx.query_port(1).expect("query_port failed");
    println!("port state={}, lid={}", port_attr.state, port_attr.lid);
}

#[test]
fn safe_query_gid() {
    let ctx = require_siw();
    let gid = ctx.query_gid(1, 0).expect("query_gid failed");
    let raw = unsafe { gid.raw };
    println!("GID[0] = {:02x?}", raw);
}

#[test]
fn safe_pd_lifecycle() {
    let ctx = require_siw();
    let pd = ctx.alloc_pd().expect("alloc_pd failed");
    assert!(!pd.as_raw().is_null());
    // PD is dropped here — RAII dealloc
}

#[test]
fn safe_cq_lifecycle() {
    let ctx = require_siw();
    let cq = ctx.create_cq(32).expect("create_cq failed");
    assert!(!cq.as_raw().is_null());

    // Poll empty CQ should return 0.
    let mut wcs = [WorkCompletion::default(); 4];
    let n = cq.poll(&mut wcs).expect("poll failed");
    assert_eq!(n, 0, "Expected 0 completions on empty CQ");
}

#[test]
fn safe_mr_borrowed() {
    let ctx = require_siw();
    let pd = ctx.alloc_pd().unwrap();
    let mut buf = vec![0u8; 4096];
    let mr = pd
        .reg_mr(
            &mut buf,
            AccessFlags::LOCAL_WRITE | AccessFlags::REMOTE_READ,
        )
        .expect("reg_mr failed");
    assert!(mr.lkey() != 0);
    assert!(mr.rkey() != 0);
    assert_eq!(mr.length(), 4096);
    println!("Borrowed MR: lkey={}, rkey={}", mr.lkey(), mr.rkey());
}

#[test]
fn safe_mr_owned() {
    let ctx = require_siw();
    let pd = ctx.alloc_pd().unwrap();
    let buf = vec![42u8; 2048];
    let mr = pd
        .reg_mr_owned(buf, AccessFlags::LOCAL_WRITE)
        .expect("reg_mr_owned failed");
    assert!(mr.lkey() != 0);
    assert_eq!(mr.as_slice().len(), 2048);
    assert_eq!(mr.as_slice()[0], 42);
}

#[test]
fn safe_qp_create_destroy() {
    let ctx = require_siw();
    let pd = ctx.alloc_pd().unwrap();
    let send_cq = ctx.create_cq(16).unwrap();
    let recv_cq = ctx.create_cq(16).unwrap();

    let qp = pd
        .create_qp(
            &send_cq,
            &recv_cq,
            &QpInitAttr {
                qp_type: QpType::Rc,
                max_send_wr: 16,
                max_recv_wr: 16,
                ..Default::default()
            },
        )
        .expect("create_qp failed");
    assert!(qp.qp_num() > 0);
    println!("QP created: qp_num={}", qp.qp_num());
    // QP is dropped — RAII destroy
}

#[test]
fn safe_multiple_resources() {
    let ctx = require_siw();
    let pd1 = ctx.alloc_pd().unwrap();
    let pd2 = ctx.alloc_pd().unwrap();
    let cq1 = ctx.create_cq(32).unwrap();
    let cq2 = ctx.create_cq(64).unwrap();

    let mut buf1 = vec![0u8; 1024];
    let _mr1 = pd1.reg_mr(&mut buf1, AccessFlags::LOCAL_WRITE).unwrap();

    let _mr2 = pd2
        .reg_mr_owned(vec![0u8; 2048], AccessFlags::LOCAL_WRITE)
        .unwrap();

    let _qp1 = pd1.create_qp(&cq1, &cq2, &QpInitAttr::default()).unwrap();

    let _qp2 = pd2.create_qp(&cq2, &cq1, &QpInitAttr::default()).unwrap();

    // Everything dropped in reverse order — RAII handles it.
}
