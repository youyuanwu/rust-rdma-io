//! V2 API integration tests.
//!
//! Tests the ergonomic v2 API with a real RDMA device (RXE/siw).
//! Covers device discovery, CQ builder, MR registration, QP builder,
//! send/recv with poll and async completions, one-sided RDMA read/write,
//! error handling, and resource drop ordering.

use rdma_io::async_cm::AsyncCmId;
use rdma_io::cm::ConnParam;
use rdma_io::mr::AccessFlags;
use rdma_io::v2::*;
use rdma_io::wc::WorkCompletion;
use rdma_io_tests::test_helpers::{connect_addr_for, has_software_rdma};

#[macro_export]
macro_rules! require_software_rdma {
    () => {
        if !has_software_rdma() {
            tracing::warn!("SKIPPED: test requires software RDMA device (rxe/siw)");
            return;
        }
    };
}

// ---- Device / PD ----

#[test]
fn test_v2_context_and_pd() {
    require_software_rdma!();
    let ctx = Context::open_first().expect("open device");
    let pd = ctx.alloc_pd().expect("alloc pd");
    // PD should reference the same context
    assert!(!pd.inner().as_raw().is_null());
}

#[test]
fn test_v2_context_open_by_name() {
    require_software_rdma!();
    // Try rxe0 first, then siw0
    let result = Context::open_by_name("rxe0")
        .or_else(|_| Context::open_by_name("siw0"));
    assert!(result.is_ok(), "should open rxe0 or siw0");
}

#[test]
fn test_v2_context_not_found() {
    let result = Context::open_by_name("nonexistent_device_12345");
    assert!(matches!(result, Err(Error::DeviceNotFound(_))));
}

// ---- CQ Builder ----

#[test]
fn test_v2_cq_poll_only() {
    require_software_rdma!();
    let ctx = Context::open_first().unwrap();
    let cq = CqBuilder::new(&ctx, 16).build().unwrap();

    assert!(!cq.has_channel());
    assert!(cq.fd().is_none());

    // Empty poll returns 0
    let mut wc = [WorkCompletion::default(); 4];
    assert_eq!(cq.poll(&mut wc).unwrap(), 0);
}

#[test]
fn test_v2_cq_with_channel() {
    require_software_rdma!();
    let ctx = Context::open_first().unwrap();
    let cq = CqBuilder::new(&ctx, 16).with_channel().build().unwrap();

    assert!(cq.has_channel());
    let fd = cq.fd().expect("should have fd");
    assert!(fd >= 0, "fd should be non-negative");
}

// ---- MR Registration ----

#[test]
fn test_v2_mr_registration() {
    require_software_rdma!();
    let ctx = Context::open_first().unwrap();
    let pd = ctx.alloc_pd().unwrap();

    // Test each access intent
    let mr_local = pd.reg_mr(64, AccessIntent::LocalOnly).unwrap();
    assert_eq!(mr_local.len(), 64);
    assert!(!mr_local.is_empty());

    let mr_rr = pd.reg_mr(128, AccessIntent::RemoteRead).unwrap();
    assert_eq!(mr_rr.len(), 128);

    let mr_rw = pd.reg_mr(256, AccessIntent::RemoteWrite).unwrap();
    assert_eq!(mr_rw.len(), 256);

    let mr_rrw = pd.reg_mr(512, AccessIntent::RemoteReadWrite).unwrap();
    assert_eq!(mr_rrw.len(), 512);

    // Verify to_remote
    let remote = mr_rrw.to_remote();
    assert_eq!(remote.len, 512);
    assert_ne!(remote.rkey, 0);
}

#[test]
fn test_v2_mr_zero_size_rejected() {
    require_software_rdma!();
    let ctx = Context::open_first().unwrap();
    let pd = ctx.alloc_pd().unwrap();
    let result = pd.reg_mr(0, AccessIntent::LocalOnly);
    assert!(matches!(result, Err(Error::InvalidConfig(_))));
}

// ---- Builder Defaults ----

#[test]
fn test_v2_builder_defaults() {
    require_software_rdma!();
    let ctx = Context::open_first().unwrap();
    let pd = ctx.alloc_pd().unwrap();
    let send_cq = CqBuilder::new(&ctx, 16).build().unwrap();
    let recv_cq = CqBuilder::new(&ctx, 16).build().unwrap();

    let builder = QpBuilder::new(&pd, &send_cq, &recv_cq);
    let attr = builder.attr();
    assert_eq!(attr.max_send_wr, 16);
    assert_eq!(attr.max_recv_wr, 16);
    assert_eq!(attr.max_send_sge, 1);
    assert_eq!(attr.max_recv_sge, 1);
    assert!(attr.sq_sig_all);
}

// ---- Endpoint helper ----

/// Holds v2 resources for a connected endpoint.
/// Drop order: qp, then pd/cqs, then cm.
struct V2Endpoint {
    qp: Qp,
    pd: Pd,
    send_cq: Cq,
    recv_cq: Cq,
    _cm: AsyncCmId,
}

/// Setup a connected server+client pair using the v2 API.
async fn setup_v2_connection() -> (V2Endpoint, V2Endpoint) {
    let listener = rdma_io_tests::test_helpers::bind_listener_with_retry().await;
    let connect_addr = connect_addr_for(listener.local_addr());

    let server_handle = tokio::spawn(async move {
        let conn_id = listener.get_request().await.unwrap();

        let ctx = Context::from_cm(&conn_id).unwrap();
        let pd = ctx.alloc_pd().unwrap();
        let send_cq = CqBuilder::new(&ctx, 32).with_channel().build().unwrap();
        let recv_cq = CqBuilder::new(&ctx, 32).with_channel().build().unwrap();

        let qp = QpBuilder::new(&pd, &send_cq, &recv_cq)
            .build_with_cm(&conn_id)
            .unwrap();

        let async_cm = listener
            .complete_accept(conn_id, &ConnParam::default())
            .await
            .unwrap();

        V2Endpoint {
            qp,
            pd,
            send_cq,
            recv_cq,
            _cm: async_cm,
        }
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let client_handle = tokio::spawn(async move {
        let (async_cm, endpoint) =
            rdma_io_tests::test_helpers::connect_client_with_retry(&connect_addr, |cm| {
                // cm is &AsyncCmId — get verbs context and build v2 resources
                let verbs_ctx = cm.verbs_context().unwrap();
                let ctx = Context::from_inner(verbs_ctx);
                let pd = ctx.alloc_pd().unwrap();
                let send_cq = CqBuilder::new(&ctx, 32).with_channel().build().unwrap();
                let recv_cq = CqBuilder::new(&ctx, 32).with_channel().build().unwrap();

                let cmqp = cm
                    .create_qp_with_cq(
                        pd.inner(),
                        &rdma_io::qp::QpInitAttr::default(),
                        Some(send_cq.inner()),
                        Some(recv_cq.inner()),
                    )
                    .unwrap();
                let qp = Qp::from_cm_qp(cmqp);

                (pd, send_cq, recv_cq, qp)
            })
            .await;

        let (pd, send_cq, recv_cq, qp) = endpoint;

        V2Endpoint {
            qp,
            pd,
            send_cq,
            recv_cq,
            _cm: async_cm,
        }
    });

    let (server_res, client_res) = tokio::join!(server_handle, client_handle);
    (server_res.unwrap(), client_res.unwrap())
}

// ---- Send/Recv with poll-based completion ----

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_v2_send_recv_poll() {
    require_software_rdma!();

    let (server, client) = setup_v2_connection().await;

    let msg = b"hello v2 poll!";
    let mut recv_mr = server.pd.reg_mr(64, AccessIntent::LocalOnly).unwrap();
    server.qp.post_recv(&mut recv_mr, 1).unwrap();

    let mut send_mr = client.pd.reg_mr(64, AccessIntent::LocalOnly).unwrap();
    send_mr.as_mut_slice()[..msg.len()].copy_from_slice(msg);
    client.qp.post_send(&send_mr, 2).unwrap();

    // Poll for send completion on client
    let mut wc = [WorkCompletion::default(); 4];
    loop {
        let n = client.send_cq.poll(&mut wc).unwrap();
        if n > 0 {
            assert!(wc[0].is_success(), "send should succeed");
            assert_eq!(wc[0].wr_id(), 2);
            break;
        }
        tokio::task::yield_now().await;
    }

    // Poll for recv completion on server
    loop {
        let n = server.recv_cq.poll(&mut wc).unwrap();
        if n > 0 {
            assert!(wc[0].is_success(), "recv should succeed");
            assert_eq!(wc[0].wr_id(), 1);
            // byte_len is the full MR size since we sent the entire buffer
            assert_eq!(wc[0].byte_len(), 64);
            break;
        }
        tokio::task::yield_now().await;
    }

    // Verify data in the first msg.len() bytes
    assert_eq!(&recv_mr.as_slice()[..msg.len()], msg);
}

// ---- Send/Recv with async completion ----

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_v2_send_recv_async() {
    require_software_rdma!();

    // Build connection with separate endpoint structures
    let listener = rdma_io_tests::test_helpers::bind_listener_with_retry().await;
    let connect_addr = connect_addr_for(listener.local_addr());

    let server_handle = tokio::spawn(async move {
        let conn_id = listener.get_request().await.unwrap();
        let ctx = Context::from_cm(&conn_id).unwrap();
        let pd = ctx.alloc_pd().unwrap();
        let send_cq = CqBuilder::new(&ctx, 32).with_channel().build().unwrap();
        let recv_cq = CqBuilder::new(&ctx, 32).with_channel().build().unwrap();
        let qp = QpBuilder::new(&pd, &send_cq, &recv_cq)
            .build_with_cm(&conn_id)
            .unwrap();
        let async_cm = listener
            .complete_accept(conn_id, &ConnParam::default())
            .await
            .unwrap();
        (qp, pd, send_cq, recv_cq, async_cm)
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let client_handle = tokio::spawn(async move {
        let (async_cm, (pd, send_cq, recv_cq, qp)) =
            rdma_io_tests::test_helpers::connect_client_with_retry(&connect_addr, |cm| {
                let verbs_ctx = cm.verbs_context().unwrap();
                let ctx = Context::from_inner(verbs_ctx);
                let pd = ctx.alloc_pd().unwrap();
                let send_cq = CqBuilder::new(&ctx, 32).with_channel().build().unwrap();
                let recv_cq = CqBuilder::new(&ctx, 32).with_channel().build().unwrap();
                let cmqp = cm
                    .create_qp_with_cq(
                        pd.inner(),
                        &rdma_io::qp::QpInitAttr::default(),
                        Some(send_cq.inner()),
                        Some(recv_cq.inner()),
                    )
                    .unwrap();
                let qp = Qp::from_cm_qp(cmqp);
                (pd, send_cq, recv_cq, qp)
            })
            .await;
        (qp, pd, send_cq, recv_cq, async_cm)
    });

    let (server_res, client_res) = tokio::join!(server_handle, client_handle);
    let (s_qp, s_pd, _s_send_cq, s_recv_cq, _s_cm) = server_res.unwrap();
    let (c_qp, c_pd, c_send_cq, _c_recv_cq, _c_cm) = client_res.unwrap();

    // Convert CQs to async completions (takes ownership)
    let mut server_recv_completions = s_recv_cq.completions_tokio().unwrap();
    let mut client_send_completions = c_send_cq.completions_tokio().unwrap();

    let msg = b"hello v2 async!";

    // Server: register and post recv
    let mut recv_mr = s_pd.reg_mr(64, AccessIntent::LocalOnly).unwrap();
    s_qp.post_recv(&mut recv_mr, 10).unwrap();

    // Client: register, fill, and post send
    let mut send_mr = c_pd.reg_mr(64, AccessIntent::LocalOnly).unwrap();
    send_mr.as_mut_slice()[..msg.len()].copy_from_slice(msg);
    c_qp.post_send(&send_mr, 20).unwrap();

    // Await completions concurrently
    let mut wc = [WorkCompletion::default(); 4];

    let (_, _) = tokio::join!(
        async {
            let n = client_send_completions.next(&mut wc).await.unwrap();
            assert!(n > 0);
            assert!(wc[0].is_success());
            assert_eq!(wc[0].wr_id(), 20);
        },
        async {
            let mut wc2 = [WorkCompletion::default(); 4];
            let n = server_recv_completions.next(&mut wc2).await.unwrap();
            assert!(n > 0);
            assert!(wc2[0].is_success());
            assert_eq!(wc2[0].wr_id(), 10);
            // byte_len is the full MR size since we sent the entire buffer
            assert_eq!(wc2[0].byte_len(), 64);
        }
    );

    // Verify data arrived
    assert_eq!(&recv_mr.as_slice()[..msg.len()], msg);
}

// ---- RDMA Write + Read ----

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_v2_rdma_write_read() {
    require_software_rdma!();
    rdma_io_tests::require_no_iwarp!();

    // Build connections inline for access to individual CQs
    let listener = rdma_io_tests::test_helpers::bind_listener_with_retry().await;
    let connect_addr = connect_addr_for(listener.local_addr());

    let server_handle = tokio::spawn(async move {
        let conn_id = listener.get_request().await.unwrap();
        let ctx = Context::from_cm(&conn_id).unwrap();
        let pd = ctx.alloc_pd().unwrap();
        let send_cq = CqBuilder::new(&ctx, 32).with_channel().build().unwrap();
        let recv_cq = CqBuilder::new(&ctx, 32).with_channel().build().unwrap();
        let qp = QpBuilder::new(&pd, &send_cq, &recv_cq)
            .build_with_cm(&conn_id)
            .unwrap();
        let async_cm = listener
            .complete_accept(conn_id, &ConnParam::default())
            .await
            .unwrap();
        (qp, pd, send_cq, recv_cq, async_cm)
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let client_handle = tokio::spawn(async move {
        let (async_cm, (pd, send_cq, recv_cq, qp)) =
            rdma_io_tests::test_helpers::connect_client_with_retry(&connect_addr, |cm| {
                let verbs_ctx = cm.verbs_context().unwrap();
                let ctx = Context::from_inner(verbs_ctx);
                let pd = ctx.alloc_pd().unwrap();
                let send_cq = CqBuilder::new(&ctx, 32).with_channel().build().unwrap();
                let recv_cq = CqBuilder::new(&ctx, 32).with_channel().build().unwrap();
                let cmqp = cm
                    .create_qp_with_cq(
                        pd.inner(),
                        &rdma_io::qp::QpInitAttr::default(),
                        Some(send_cq.inner()),
                        Some(recv_cq.inner()),
                    )
                    .unwrap();
                let qp = Qp::from_cm_qp(cmqp);
                (pd, send_cq, recv_cq, qp)
            })
            .await;
        (qp, pd, send_cq, recv_cq, async_cm)
    });

    let (server_res, client_res) = tokio::join!(server_handle, client_handle);
    let (s_qp, s_pd, _s_send_cq, s_recv_cq, _s_cm) = server_res.unwrap();
    let (c_qp, c_pd, c_send_cq, _c_recv_cq, _c_cm) = client_res.unwrap();

    // Register MRs with remote access on both sides
    let server_mr = s_pd.reg_mr(64, AccessIntent::RemoteReadWrite).unwrap();
    let client_mr = c_pd.reg_mr(64, AccessIntent::RemoteReadWrite).unwrap();

    // Build async completions for exchange
    let mut s_send_comp = _s_send_cq.completions_tokio().unwrap();
    let mut s_recv_comp = s_recv_cq.completions_tokio().unwrap();
    let mut c_send_comp = c_send_cq.completions_tokio().unwrap();
    let mut c_recv_comp = _c_recv_cq.completions_tokio().unwrap();

    // Exchange remote MR descriptors via send/recv
    let mut s_desc_recv = s_pd.reg_mr(16, AccessIntent::LocalOnly).unwrap();
    s_qp.post_recv(&mut s_desc_recv, 100).unwrap();

    let mut c_desc_recv = c_pd.reg_mr(16, AccessIntent::LocalOnly).unwrap();
    c_qp.post_recv(&mut c_desc_recv, 101).unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    // Server sends its descriptor
    let s_remote = server_mr.to_remote();
    let mut s_desc_send = s_pd.reg_mr(16, AccessIntent::LocalOnly).unwrap();
    s_desc_send.as_mut_slice()[..8].copy_from_slice(&s_remote.addr.to_le_bytes());
    s_desc_send.as_mut_slice()[8..12].copy_from_slice(&s_remote.rkey.to_le_bytes());
    s_desc_send.as_mut_slice()[12..16].copy_from_slice(&s_remote.len.to_le_bytes());
    s_qp.post_send(&s_desc_send, 102).unwrap();

    // Client sends its descriptor
    let c_remote = client_mr.to_remote();
    let mut c_desc_send = c_pd.reg_mr(16, AccessIntent::LocalOnly).unwrap();
    c_desc_send.as_mut_slice()[..8].copy_from_slice(&c_remote.addr.to_le_bytes());
    c_desc_send.as_mut_slice()[8..12].copy_from_slice(&c_remote.rkey.to_le_bytes());
    c_desc_send.as_mut_slice()[12..16].copy_from_slice(&c_remote.len.to_le_bytes());
    c_qp.post_send(&c_desc_send, 103).unwrap();

    // Wait for all 4 exchange completions
    let mut wc = [WorkCompletion::default(); 4];
    let (_, _, _, _) = tokio::join!(
        async {
            let n = s_send_comp.next(&mut wc).await.unwrap();
            assert!(n > 0 && wc[0].is_success());
        },
        async {
            let mut w = [WorkCompletion::default(); 4];
            let n = s_recv_comp.next(&mut w).await.unwrap();
            assert!(n > 0 && w[0].is_success());
        },
        async {
            let mut w = [WorkCompletion::default(); 4];
            let n = c_send_comp.next(&mut w).await.unwrap();
            assert!(n > 0 && w[0].is_success());
        },
        async {
            let mut w = [WorkCompletion::default(); 4];
            let n = c_recv_comp.next(&mut w).await.unwrap();
            assert!(n > 0 && w[0].is_success());
        }
    );

    // Parse server's remote MR from client's received descriptor
    let parse_remote = |buf: &[u8]| -> RemoteMr {
        let addr = u64::from_le_bytes(buf[..8].try_into().unwrap());
        let rkey = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        let len = u32::from_le_bytes(buf[12..16].try_into().unwrap());
        RemoteMr { addr, rkey, len }
    };
    let server_remote_for_client = parse_remote(&c_desc_recv.as_slice()[..16]);

    // Client writes data to server's remote memory
    let write_data = b"v2 rdma write!";
    let mut write_mr = c_pd.reg_mr(write_data.len(), AccessIntent::LocalOnly).unwrap();
    write_mr.as_mut_slice().copy_from_slice(write_data);
    c_qp.post_write(&write_mr, &server_remote_for_client, 200).unwrap();

    // Wait for write completion
    let mut wc2 = [WorkCompletion::default(); 4];
    let n = c_send_comp.next(&mut wc2).await.unwrap();
    assert!(n > 0);
    assert!(wc2[0].is_success(), "write should succeed: {:?}", wc2[0].status());

    // Brief delay for data to land
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    // Client reads server's memory to verify
    let mut read_mr = c_pd.reg_mr(write_data.len(), AccessIntent::LocalOnly).unwrap();
    c_qp.post_read(&mut read_mr, &server_remote_for_client, 201).unwrap();

    let n = c_send_comp.next(&mut wc2).await.unwrap();
    assert!(n > 0);
    assert!(wc2[0].is_success(), "read should succeed: {:?}", wc2[0].status());

    // Verify data matches
    assert_eq!(read_mr.as_slice(), write_data);
}

// ---- Drop Order Test ----

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_v2_drop_order() {
    require_software_rdma!();

    // Establish connection
    let (server, client) = setup_v2_connection().await;

    // Explicitly drop in correct order: qp first, then cqs, then cm
    drop(server.qp);
    drop(server.send_cq);
    drop(server.recv_cq);
    drop(server.pd);
    drop(server._cm);

    drop(client.qp);
    drop(client.send_cq);
    drop(client.recv_cq);
    drop(client.pd);
    drop(client._cm);
    // No panics = success
}

// ---- Access intent flags test ----

#[test]
fn test_v2_access_intent_flags() {
    assert_eq!(
        AccessIntent::LocalOnly.to_flags(),
        AccessFlags::LOCAL_WRITE
    );
    assert_eq!(
        AccessIntent::RemoteRead.to_flags(),
        AccessFlags::LOCAL_WRITE | AccessFlags::REMOTE_READ
    );
    assert_eq!(
        AccessIntent::RemoteWrite.to_flags(),
        AccessFlags::LOCAL_WRITE | AccessFlags::REMOTE_WRITE
    );
    assert_eq!(
        AccessIntent::RemoteReadWrite.to_flags(),
        AccessFlags::LOCAL_WRITE | AccessFlags::REMOTE_READ | AccessFlags::REMOTE_WRITE
    );
}

// ---- Completion Error Test ----

/// Test that failed completions surface error status through v2 APIs.
/// Transitions QP to error state, which flushes outstanding WRs with
/// WrFlushErr status — verifying FR-008 error surfacing.
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_v2_completion_error() {
    require_software_rdma!();

    let (server, _client) = setup_v2_connection().await;

    // Post a recv WR that will never be fulfilled
    let mut recv_mr = server.pd.reg_mr(64, AccessIntent::LocalOnly).unwrap();
    server.qp.post_recv(&mut recv_mr, 42).unwrap();

    // Transition QP to error state — flushes all outstanding WRs
    server.qp.to_error().unwrap();

    // Poll for the flushed completion — should show error status
    let mut wc = [WorkCompletion::default(); 4];
    let mut found_error = false;
    for _ in 0..100 {
        let n = server.recv_cq.poll(&mut wc).unwrap();
        if n > 0 {
            assert!(!wc[0].is_success(), "flushed WR should have error status");
            assert_eq!(wc[0].status(), rdma_io::wc::WcStatus::WrFlushErr);
            assert_eq!(wc[0].wr_id(), 42);
            found_error = true;
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(found_error, "should have received flushed completion with error status");
}
