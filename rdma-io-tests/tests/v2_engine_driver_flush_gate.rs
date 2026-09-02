use std::time::Duration;

use rdma_io::v2::test_support::{
    DestructionKind, DestructionRecorder, TestAcceptedOperation, TestEngineResources,
    TestRouteHandle,
};
use rdma_io::v2::{AccessIntent, CompletionMode, RdmaEngineBuilder};
use rdma_io::wc::{WcOpcode, WcStatus};
use rdma_io_tests::engine_test_helpers::{EngineTestEndpoint, setup_engine_pair};
use rdma_io_tests::test_helpers::has_software_rdma;

fn software_device_name() -> Option<String> {
    let list = rdma_io::cm::RdmaCmDeviceList::new().ok()?;
    list.device_names()
        .into_iter()
        .find(|name| name.starts_with("rxe") || name.starts_with("siw"))
}

fn install_endpoint_route(
    resources: &TestEngineResources,
    endpoint: &mut EngineTestEndpoint,
    operations: impl IntoIterator<Item = TestAcceptedOperation>,
) -> TestRouteHandle {
    let qp = endpoint.qp.take().expect("endpoint QP");
    let route = resources.install_route(qp, operations).unwrap();
    route.retain(endpoint.cm.take().expect("endpoint CM owner"));
    route
}

async fn wait_drained(label: &str, route: &TestRouteHandle) {
    if tokio::time::timeout(Duration::from_secs(10), route.wait_until_drained())
        .await
        .is_err()
    {
        panic!(
            "timed out draining {label}: remaining={}, completions={:?}",
            route.accepted_outstanding(),
            route.completions()
        );
    }
}

async fn run_flush_gate(mode: CompletionMode) {
    if !has_software_rdma() {
        return;
    }
    let device = software_device_name().expect("software RDMA device");
    let recorder = DestructionRecorder::arm(128);
    let (engine, driver) = RdmaEngineBuilder::new(device)
        .completion_mode(mode)
        .maximum_live_connections(8)
        .maximum_inflight_operations(256)
        .cq_capacity(256)
        .build()
        .unwrap();
    let resources = engine.test_resources().unwrap();
    let driver_task = tokio::spawn(driver);

    let mut flush_pair = setup_engine_pair(&resources).await;
    let mut traffic_pair = setup_engine_pair(&resources).await;

    let flush_route = install_endpoint_route(
        &resources,
        &mut flush_pair.server,
        [
            TestAcceptedOperation::new(10_000, WcOpcode::Recv),
            TestAcceptedOperation::new(10_001, WcOpcode::Recv),
            TestAcceptedOperation::new(10_002, WcOpcode::Send),
        ],
    );
    let traffic_recv_route = install_endpoint_route(
        &resources,
        &mut traffic_pair.server,
        (0..8).map(|index| TestAcceptedOperation::new(20_000 + index, WcOpcode::Recv)),
    );
    let traffic_send_route = install_endpoint_route(
        &resources,
        &mut traffic_pair.client,
        (0..8).map(|index| TestAcceptedOperation::new(30_000 + index, WcOpcode::Send)),
    );

    let mut flush_recv_one = resources
        .register_memory(64, AccessIntent::LocalOnly)
        .unwrap();
    flush_route
        .qp()
        .post_recv(&mut flush_recv_one, 10_000)
        .unwrap();
    flush_route.retain(flush_recv_one);

    let mut flush_recv_two = resources
        .register_memory(64, AccessIntent::LocalOnly)
        .unwrap();
    flush_route
        .qp()
        .post_recv(&mut flush_recv_two, 10_001)
        .unwrap();
    flush_route.retain(flush_recv_two);

    let flush_send = resources
        .register_memory(64, AccessIntent::LocalOnly)
        .unwrap();
    flush_route.qp().post_send(&flush_send, 10_002).unwrap();
    flush_route.retain(flush_send);

    for index in 0..8u64 {
        let mut recv = resources
            .register_memory(64, AccessIntent::LocalOnly)
            .unwrap();
        traffic_recv_route
            .qp()
            .post_recv(&mut recv, 20_000 + index)
            .unwrap();
        traffic_recv_route.retain(recv);

        let send = resources
            .register_memory(64, AccessIntent::LocalOnly)
            .unwrap();
        traffic_send_route
            .qp()
            .post_send(&send, 30_000 + index)
            .unwrap();
        traffic_send_route.retain(send);
    }

    flush_route.qp().to_error().unwrap();

    let ((), (), ()) = tokio::join!(
        wait_drained("flush route", &flush_route),
        wait_drained("traffic receive route", &traffic_recv_route),
        wait_drained("traffic send route", &traffic_send_route),
    );

    let flush_qp_num = flush_route.qp_num();
    let flush_completions = flush_route.completions();
    assert_eq!(flush_completions.len(), 3);
    assert!(
        flush_completions
            .iter()
            .all(|completion| completion.qp_num() == flush_qp_num)
    );
    assert!(
        flush_completions
            .iter()
            .any(|completion| completion.status() == WcStatus::WrFlushErr),
        "explicit local QP ERR must produce a flush completion in {mode:?} mode"
    );
    assert!(
        traffic_recv_route
            .completions()
            .iter()
            .all(|completion| completion.is_success())
    );
    assert!(
        traffic_send_route
            .completions()
            .iter()
            .all(|completion| completion.is_success())
    );

    flush_route.remove().unwrap();
    traffic_recv_route.remove().unwrap();
    traffic_send_route.remove().unwrap();
    drop(flush_pair);
    drop(traffic_pair);

    engine.shutdown().await.unwrap();
    driver_task.await.unwrap().unwrap();
    drop(resources);
    drop(engine);

    let events = recorder.take();
    assert!(!recorder.overflowed());
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == DestructionKind::QueuePair)
            .count(),
        4,
        "all QPs must be destroyed only after routed accepted sets reach zero"
    );
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn explicit_qp_err_flushes_every_accepted_wr_in_readiness_and_polling_modes() {
    run_flush_gate(CompletionMode::Readiness).await;
    run_flush_gate(CompletionMode::Polling).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn deterministic_cqe_suppression_retains_the_accepted_set_until_released() {
    if !has_software_rdma() {
        return;
    }
    let device = software_device_name().expect("software RDMA device");
    let (engine, driver) = RdmaEngineBuilder::new(device)
        .completion_mode(CompletionMode::Polling)
        .maximum_live_connections(2)
        .maximum_inflight_operations(64)
        .cq_capacity(64)
        .build()
        .unwrap();
    let resources = engine.test_resources().unwrap();
    let driver_task = tokio::spawn(driver);
    let mut pair = setup_engine_pair(&resources).await;

    let recv_route = install_endpoint_route(
        &resources,
        &mut pair.server,
        [TestAcceptedOperation::new(40_000, WcOpcode::Recv)],
    );
    let send_route = install_endpoint_route(
        &resources,
        &mut pair.client,
        [TestAcceptedOperation::new(50_000, WcOpcode::Send)],
    );
    let suppression = send_route.suppress_next(50_000).unwrap();

    let mut recv = resources
        .register_memory(64, AccessIntent::LocalOnly)
        .unwrap();
    recv_route.qp().post_recv(&mut recv, 40_000).unwrap();
    recv_route.retain(recv);
    let send = resources
        .register_memory(64, AccessIntent::LocalOnly)
        .unwrap();
    send_route.qp().post_send(&send, 50_000).unwrap();
    send_route.retain(send);

    tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(recv_route.wait_until_drained(), suppression.wait_observed());
    })
    .await
    .expect("suppressed completion was not observed");
    assert_eq!(send_route.accepted_outstanding(), 1);

    suppression.release().unwrap();
    wait_drained("released suppression route", &send_route).await;
    recv_route.remove().unwrap();
    send_route.remove().unwrap();
    drop(pair);

    engine.shutdown().await.unwrap();
    driver_task.await.unwrap().unwrap();
}

#[test]
fn route_handle_and_qp_are_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    fn assert_send<T: Send>() {}
    assert_send_sync::<rdma_io::v2::test_support::TestEngineQp>();
    assert_send::<TestRouteHandle>();
}
