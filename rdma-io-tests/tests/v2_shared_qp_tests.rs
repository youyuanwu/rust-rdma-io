//! V2 per-operation future (SharedQp) integration tests.
//!
//! Tests the compio/tokio-uring-style per-operation future API with
//! real RDMA operations on RXE.

use rdma_io::cm::ConnParam;
use rdma_io::v2::*;
use rdma_io::wc::WcOpcode;
use rdma_io_tests::test_helpers::{connect_addr_for, has_software_rdma};

#[macro_export]
macro_rules! require_software_rdma {
    () => {
        if !has_software_rdma() {
            tracing::warn!("SKIPPED: requires software RDMA device");
            return;
        }
    };
}

fn assert_operation_posted(mut future: std::pin::Pin<&mut OpFuture>) {
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(
        std::future::Future::poll(future.as_mut(), &mut cx).is_pending(),
        "operation completed before its peer operation was submitted"
    );
}

// ---- Send/Recv future test with inline setup ----

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_shared_qp_send_recv_fd() {
    require_software_rdma!();

    let listener = rdma_io_tests::test_helpers::bind_listener_with_retry().await;
    let connect_addr = connect_addr_for(listener.local_addr());

    // Server setup
    let server_handle = tokio::spawn(async move {
        let conn_id = listener.get_request().await.unwrap();
        let ctx = Context::from_cm(&conn_id).unwrap();
        let pd = ctx.alloc_pd().unwrap();
        let send_cq = CqBuilder::new(&ctx, 64).with_channel().build().unwrap();
        let recv_cq = CqBuilder::new(&ctx, 64).with_channel().build().unwrap();
        let qp = QpBuilder::new(&pd, &send_cq, &recv_cq)
            .build_with_cm(&conn_id)
            .unwrap();
        let async_cm = listener
            .complete_accept(conn_id, &ConnParam::default())
            .await
            .unwrap();

        let (send_driver, send_handle) = FdCqDriver::new(send_cq, 64);
        let (recv_driver, recv_handle) = FdCqDriver::new(recv_cq, 64);
        let _send_task = tokio::spawn(send_driver.run_tokio());
        let _recv_task = tokio::spawn(recv_driver.run_tokio());

        // Use recv handle for recv ops
        let sqp = SharedQp::new(qp, recv_handle, pd);
        (sqp, send_handle, async_cm)
    });

    // Client setup
    let client_handle = tokio::spawn(async move {
        let (async_cm, (pd, send_cq, recv_cq, qp)) =
            rdma_io_tests::test_helpers::connect_client_with_retry(&connect_addr, |cm| {
                let verbs_ctx = cm.verbs_context().unwrap();
                let ctx = Context::from_inner(verbs_ctx);
                let pd = ctx.alloc_pd().unwrap();
                let send_cq = CqBuilder::new(&ctx, 64).with_channel().build().unwrap();
                let recv_cq = CqBuilder::new(&ctx, 64).with_channel().build().unwrap();
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

        let (send_driver, send_handle) = FdCqDriver::new(send_cq, 64);
        let (recv_driver, recv_handle) = FdCqDriver::new(recv_cq, 64);
        let _send_task = tokio::spawn(send_driver.run_tokio());
        let _recv_task = tokio::spawn(recv_driver.run_tokio());

        // Use send handle for send ops
        let sqp = SharedQp::new(qp, send_handle, pd);
        (sqp, recv_handle, async_cm)
    });

    let (server_res, client_res) = tokio::join!(server_handle, client_handle);
    let (server_sqp, _s_send_handle, _s_cm) = server_res.unwrap();
    let (client_sqp, _c_recv_handle, _c_cm) = client_res.unwrap();

    let msg = b"hello shared qp!";

    // Server: post recv (await completion)
    let mut recv_mr = server_sqp.pd().reg_mr(64, AccessIntent::LocalOnly).unwrap();
    recv_mr.as_mut_slice()[..64].fill(0);

    // Client: prepare send
    let mut send_mr = client_sqp.pd().reg_mr(64, AccessIntent::LocalOnly).unwrap();
    send_mr.as_mut_slice()[..msg.len()].copy_from_slice(msg);

    // Polling the receive future once posts the receive WR deterministically.
    let mut recv_future = std::pin::pin!(server_sqp.recv(recv_mr, None));
    assert_operation_posted(recv_future.as_mut());

    let send_result = client_sqp.send(send_mr, None).await;
    let recv_result = recv_future.await;

    let (recv_res, recv_mr) = recv_result;
    let recv_completion = recv_res.expect("recv should succeed");
    assert_eq!(recv_completion.opcode(), WcOpcode::Recv);

    let (send_res, _send_mr) = send_result;
    let send_completion = send_res.expect("send should succeed");
    assert_eq!(send_completion.opcode(), WcOpcode::Send);

    // Verify data
    assert_eq!(&recv_mr.as_slice()[..msg.len()], msg);

    // Cleanup
    server_sqp.shutdown().ok();
    client_sqp.shutdown().ok();
}

// ---- RDMA Write/Read future test ----

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_shared_qp_write_read_fd() {
    require_software_rdma!();
    rdma_io_tests::require_no_iwarp!();

    let listener = rdma_io_tests::test_helpers::bind_listener_with_retry().await;
    let connect_addr = connect_addr_for(listener.local_addr());

    let server_handle = tokio::spawn(async move {
        let conn_id = listener.get_request().await.unwrap();
        let ctx = Context::from_cm(&conn_id).unwrap();
        let pd = ctx.alloc_pd().unwrap();
        let send_cq = CqBuilder::new(&ctx, 64).with_channel().build().unwrap();
        let recv_cq = CqBuilder::new(&ctx, 64).with_channel().build().unwrap();
        let qp = QpBuilder::new(&pd, &send_cq, &recv_cq)
            .build_with_cm(&conn_id)
            .unwrap();
        let async_cm = listener
            .complete_accept(conn_id, &ConnParam::default())
            .await
            .unwrap();
        let (send_driver, send_handle) = FdCqDriver::new(send_cq, 64);
        let (recv_driver, recv_handle) = FdCqDriver::new(recv_cq, 64);
        let _t1 = tokio::spawn(send_driver.run_tokio());
        let _t2 = tokio::spawn(recv_driver.run_tokio());
        let sqp = SharedQp::new(qp, send_handle, pd.clone());
        (sqp, recv_handle, pd, async_cm)
    });

    let client_handle = tokio::spawn(async move {
        let (async_cm, (pd, send_cq, recv_cq, qp)) =
            rdma_io_tests::test_helpers::connect_client_with_retry(&connect_addr, |cm| {
                let verbs_ctx = cm.verbs_context().unwrap();
                let ctx = Context::from_inner(verbs_ctx);
                let pd = ctx.alloc_pd().unwrap();
                let send_cq = CqBuilder::new(&ctx, 64).with_channel().build().unwrap();
                let recv_cq = CqBuilder::new(&ctx, 64).with_channel().build().unwrap();
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
        let (send_driver, send_handle) = FdCqDriver::new(send_cq, 64);
        let (recv_driver, recv_handle) = FdCqDriver::new(recv_cq, 64);
        let _t1 = tokio::spawn(send_driver.run_tokio());
        let _t2 = tokio::spawn(recv_driver.run_tokio());
        let sqp = SharedQp::new(qp, send_handle, pd.clone());
        (sqp, recv_handle, pd, async_cm)
    });

    let (server_res, client_res) = tokio::join!(server_handle, client_handle);
    let (s_sqp, _s_recv_h, s_pd, _s_cm) = server_res.unwrap();
    let (c_sqp, _c_recv_h, c_pd, _c_cm) = client_res.unwrap();

    // Register remote-accessible MR on server
    let server_data_mr = s_pd.reg_mr(64, AccessIntent::RemoteReadWrite).unwrap();
    let server_remote = server_data_mr.to_remote();

    // Client: write data to server's memory
    let write_data = b"hello write future!";
    let mut write_mr = c_pd
        .reg_mr(write_data.len(), AccessIntent::LocalOnly)
        .unwrap();
    write_mr.as_mut_slice().copy_from_slice(write_data);

    let (write_result, _write_mr) = c_sqp.write(write_mr, server_remote, None).await;
    write_result.expect("write should succeed");

    // Client: read server's memory back
    let read_mr = c_pd
        .reg_mr(write_data.len(), AccessIntent::LocalOnly)
        .unwrap();
    let (read_result, read_mr) = c_sqp.read(read_mr, server_remote, None).await;
    read_result.expect("read should succeed");

    assert_eq!(read_mr.as_slice(), write_data);

    s_sqp.shutdown().ok();
    c_sqp.shutdown().ok();
}

// ---- Polling driver test ----

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_shared_qp_send_recv_polling() {
    require_software_rdma!();

    let listener = rdma_io_tests::test_helpers::bind_listener_with_retry().await;
    let connect_addr = connect_addr_for(listener.local_addr());

    let server_handle = tokio::spawn(async move {
        let conn_id = listener.get_request().await.unwrap();
        let ctx = Context::from_cm(&conn_id).unwrap();
        let pd = ctx.alloc_pd().unwrap();
        // Poll-only CQs (no channel)
        let send_cq = CqBuilder::new(&ctx, 64).build().unwrap();
        let recv_cq = CqBuilder::new(&ctx, 64).build().unwrap();
        let qp = QpBuilder::new(&pd, &send_cq, &recv_cq)
            .build_with_cm(&conn_id)
            .unwrap();
        let async_cm = listener
            .complete_accept(conn_id, &ConnParam::default())
            .await
            .unwrap();
        let (recv_driver, recv_handle) = PollingCqDriver::new(recv_cq, 64);
        let _t = tokio::spawn(recv_driver.run());
        let sqp = SharedQp::new(qp, recv_handle, pd);
        (sqp, async_cm)
    });

    let client_handle = tokio::spawn(async move {
        let (async_cm, (pd, send_cq, _recv_cq, qp)) =
            rdma_io_tests::test_helpers::connect_client_with_retry(&connect_addr, |cm| {
                let verbs_ctx = cm.verbs_context().unwrap();
                let ctx = Context::from_inner(verbs_ctx);
                let pd = ctx.alloc_pd().unwrap();
                let send_cq = CqBuilder::new(&ctx, 64).build().unwrap();
                let recv_cq = CqBuilder::new(&ctx, 64).build().unwrap();
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
        let (send_driver, send_handle) = PollingCqDriver::new(send_cq, 64);
        let _t = tokio::spawn(send_driver.run());
        let sqp = SharedQp::new(qp, send_handle, pd);
        (sqp, async_cm)
    });

    let (server_res, client_res) = tokio::join!(server_handle, client_handle);
    let (server_sqp, _s_cm) = server_res.unwrap();
    let (client_sqp, _c_cm) = client_res.unwrap();

    let msg = b"polling driver!";
    let recv_mr = server_sqp.pd().reg_mr(64, AccessIntent::LocalOnly).unwrap();
    let mut send_mr = client_sqp.pd().reg_mr(64, AccessIntent::LocalOnly).unwrap();
    send_mr.as_mut_slice()[..msg.len()].copy_from_slice(msg);

    let mut recv_future = std::pin::pin!(server_sqp.recv(recv_mr, None));
    assert_operation_posted(recv_future.as_mut());

    let (send_res, _send_mr) = client_sqp.send(send_mr, None).await;
    let (recv_res, recv_mr) = recv_future.await;

    recv_res.expect("recv should succeed");
    assert_eq!(&recv_mr.as_slice()[..msg.len()], msg);

    send_res.expect("send should succeed");

    server_sqp.shutdown().ok();
    client_sqp.shutdown().ok();
}

// ---- Completion error propagation test ----

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn test_shared_qp_completion_error() {
    require_software_rdma!();

    let listener = rdma_io_tests::test_helpers::bind_listener_with_retry().await;
    let connect_addr = connect_addr_for(listener.local_addr());

    let server_handle = tokio::spawn(async move {
        let conn_id = listener.get_request().await.unwrap();
        let ctx = Context::from_cm(&conn_id).unwrap();
        let pd = ctx.alloc_pd().unwrap();
        let send_cq = CqBuilder::new(&ctx, 64).with_channel().build().unwrap();
        let recv_cq = CqBuilder::new(&ctx, 64).with_channel().build().unwrap();
        let qp = QpBuilder::new(&pd, &send_cq, &recv_cq)
            .build_with_cm(&conn_id)
            .unwrap();
        let async_cm = listener
            .complete_accept(conn_id, &ConnParam::default())
            .await
            .unwrap();
        let (recv_driver, recv_handle) = FdCqDriver::new(recv_cq, 64);
        let _t = tokio::spawn(recv_driver.run_tokio());
        let sqp = SharedQp::new(qp, recv_handle, pd);
        (sqp, async_cm)
    });

    // Client connects and immediately disconnects so server's recv fails
    let client_handle = tokio::spawn(async move {
        let (async_cm, _) =
            rdma_io_tests::test_helpers::connect_client_with_retry(&connect_addr, |cm| {
                let pd = cm.alloc_pd().unwrap();
                let _ctx = cm.verbs_context().unwrap();
                let cmqp = cm
                    .create_qp_with_cq(&pd, &rdma_io::qp::QpInitAttr::default(), None, None)
                    .unwrap();
                (pd, cmqp)
            })
            .await;
        async_cm
    });

    let (server_res, client_res) = tokio::join!(server_handle, client_handle);
    let (server_sqp, _s_cm) = server_res.unwrap();
    let _c_cm = client_res.unwrap();

    // Server posts recv
    let recv_mr = server_sqp.pd().reg_mr(64, AccessIntent::LocalOnly).unwrap();

    // Force QP to error state — the recv will be flushed
    server_sqp.qp().to_error().unwrap();

    let (recv_result, _recv_mr) = server_sqp.recv(recv_mr, None).await;

    // Should get CompletionError with WrFlushErr
    match recv_result {
        Err(Error::CompletionError { status, .. }) => {
            assert_eq!(status, rdma_io::wc::WcStatus::WrFlushErr);
        }
        Err(e) => panic!("expected CompletionError, got: {e}"),
        Ok(_) => panic!("expected error for flushed recv"),
    }

    server_sqp.shutdown().ok();
}

// ---- Inflight map unit tests (embedded for fast iteration) ----

#[test]
fn test_inflight_concurrent_registrations() {
    use rdma_io::v2::inflight::InflightMap;

    let map = InflightMap::new(8);

    // Register several operations
    let tokens: Vec<_> = (0..8).map(|_| map.register().unwrap().token).collect();
    assert_eq!(map.inflight_count(), 8);
    assert!(map.register().is_none()); // full

    // Complete them all
    for &token in &tokens {
        assert!(map.complete(token, rdma_io::wc::WorkCompletion::default()));
    }

    // Release them all
    for &token in &tokens {
        map.release(token);
    }
    assert_eq!(map.inflight_count(), 0);
}
