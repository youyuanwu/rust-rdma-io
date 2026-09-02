use std::future::{Future, poll_fn};
use std::task::Poll;
use std::time::Duration;

use rdma_io::v2::test_support::{DestructionKind, DestructionRecorder};
use rdma_io::v2::{AccessIntent, CompletionMode, RdmaConnectionConfig, RdmaEngineBuilder};
use rdma_io_tests::engine_test_helpers::setup_engine_pair;
use rdma_io_tests::test_helpers::has_software_rdma;

fn software_device_name() -> Option<String> {
    let list = rdma_io::cm::RdmaCmDeviceList::new().ok()?;
    list.device_names()
        .into_iter()
        .find(|name| name.starts_with("rxe") || name.starts_with("siw"))
}

async fn post_once_then_cancel(operation: &mut std::pin::Pin<Box<rdma_io::v2::RdmaOperation>>) {
    poll_fn(|cx| {
        assert!(operation.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
}

async fn wait_for_no_accepted(engine: &rdma_io::v2::RdmaEngine) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if engine.diagnostics().accepted_operations == 0 {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("accepted operation did not reach its exact CQE");
}

async fn run_owned_operations(mode: CompletionMode) {
    if !has_software_rdma() {
        return;
    }
    let device = software_device_name().expect("software RDMA device");
    let (engine, driver) = RdmaEngineBuilder::new(device)
        .completion_mode(mode)
        .maximum_live_connections(4)
        .maximum_inflight_operations(256)
        .cq_capacity(256)
        .completion_dispatch_budget(1)
        .build()
        .unwrap();
    let resources = engine.test_resources().unwrap();
    let mut pair = setup_engine_pair(&resources).await;
    let server = resources
        .install_connection(
            pair.server.qp.take().unwrap(),
            pair.server.cm.take().unwrap(),
            RdmaConnectionConfig::default(),
        )
        .unwrap();
    let client = resources
        .install_connection(
            pair.client.qp.take().unwrap(),
            pair.client.cm.take().unwrap(),
            RdmaConnectionConfig::default(),
        )
        .unwrap();
    let driver_task = tokio::spawn(driver);
    let recorder = DestructionRecorder::arm(64);

    let recv = server.register_memory(64, AccessIntent::LocalOnly).unwrap();
    let send = client.register_memory(64, AccessIntent::LocalOnly).unwrap();
    let (recv, send) = tokio::join!(server.recv(recv, None), client.send(send, None));
    let (recv_completion, recv_mr) = recv;
    let (send_completion, send_mr) = send;
    assert_eq!(
        recv_completion.unwrap().qp_num(),
        server.identity().qp_num()
    );
    assert_eq!(
        send_completion.unwrap().qp_num(),
        client.identity().qp_num()
    );
    assert!(recv_mr.is_some());
    assert!(send_mr.is_some());

    let cancelled_recv = server.register_memory(64, AccessIntent::LocalOnly).unwrap();
    let mut cancelled = Box::pin(server.recv(cancelled_recv, None));
    post_once_then_cancel(&mut cancelled).await;
    drop(cancelled);
    let cancelled_diagnostics = engine.diagnostics();
    assert_eq!(cancelled_diagnostics.accepted_operations, 1);
    assert_eq!(cancelled_diagnostics.registered_operations, 1);
    assert_eq!(cancelled_diagnostics.available_cq_credits, 255);
    assert_eq!(
        recorder
            .snapshot()
            .iter()
            .filter(|event| event.kind == DestructionKind::MemoryRegion)
            .count(),
        0,
        "cancellation alone must not deregister the posted MR"
    );
    let matching_send = client.register_memory(64, AccessIntent::LocalOnly).unwrap();
    let (send_result, matching_send) = client.send(matching_send, None).await;
    send_result.unwrap();
    let matching_send = matching_send.expect("matching send must return its MR");
    wait_for_no_accepted(&engine).await;
    let after_cancelled_cqe = recorder.snapshot();
    let mr_events = after_cancelled_cqe
        .iter()
        .filter(|event| event.kind == DestructionKind::MemoryRegion)
        .collect::<Vec<_>>();
    assert_eq!(
        mr_events.len(),
        1,
        "with every other MR retained, the cancelled MR must be the sole deregistration after its exact CQE"
    );
    assert_eq!(
        mr_events[0].result,
        Some(0),
        "the sole cancelled-MR deregistration must succeed"
    );
    drop(matching_send);
    assert_eq!(
        recorder
            .snapshot()
            .iter()
            .filter(|event| event.kind == DestructionKind::MemoryRegion)
            .count(),
        2,
        "dropping the returned matching-send MR must produce the next distinct deregistration"
    );
    let after_cancel = engine.diagnostics();
    assert_eq!(after_cancel.registered_operations, 0);
    assert_eq!(after_cancel.available_cq_credits, 256);

    let flushed_recv = server.register_memory(64, AccessIntent::LocalOnly).unwrap();
    let mut flushed = Box::pin(server.recv(flushed_recv, None));
    post_once_then_cancel(&mut flushed).await;
    drop(flushed);
    resources.transition_connection_to_error(&server).unwrap();
    wait_for_no_accepted(&engine).await;
    assert_eq!(engine.diagnostics().retained_cq_credits, 0);

    server.close().await.unwrap();
    client.close().await.unwrap();
    engine.shutdown().await.unwrap();
    driver_task.await.unwrap().unwrap();
    drop(server);
    drop(client);
    drop(pair);
    drop(resources);
    drop(engine);
    drop(recorder);
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn owned_operations_route_by_generation_qp_and_token_in_both_modes() {
    run_owned_operations(CompletionMode::Readiness).await;
    run_owned_operations(CompletionMode::Polling).await;
}
