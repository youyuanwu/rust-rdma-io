use std::time::Duration;

use rdma_io::cm::RdmaCmDeviceList;
use rdma_io::v2::test_support::{
    DestructionKind, DestructionRecorder, TestAcceptedOperation, TestEngineResources,
    TestRouteHandle,
};
use rdma_io::v2::{
    AccessIntent, CompletionMode, Context, Error, RdmaEngine, RdmaEngineBuilder, RdmaEngineDriver,
};
use rdma_io::wc::{WcOpcode, WcStatus};
use rdma_io_tests::engine_test_helpers::{EngineTestEndpoint, setup_engine_pair};
use rdma_io_tests::test_helpers::{connect_addr_for, has_software_rdma};

const DIRECT_FLUSH_SOURCE: &str = include_str!("v2_tests.rs");
const ENGINE_ASYNC_FLUSH_SOURCE: &str = include_str!("v2_engine_driver_flush_gate.rs");

struct SharedProbeResources {
    engine: RdmaEngine,
    driver: Option<RdmaEngineDriver>,
    lease: TestEngineResources,
}

fn software_device_name(list: &RdmaCmDeviceList) -> Option<String> {
    list.device_names()
        .into_iter()
        .find(|name| name.starts_with("rxe") || name.starts_with("siw"))
}

fn pinned_resources() -> Option<(String, SharedProbeResources)> {
    if !has_software_rdma() {
        return None;
    }
    let list = RdmaCmDeviceList::new().expect("enumerate librdmacm devices");
    let name = software_device_name(&list)?;
    drop(list);
    let independent = Context::open_by_name(&name).expect("open anchored public context");
    let independent_pd = independent.alloc_pd().expect("allocate independent PD");
    drop(independent_pd);
    drop(independent);
    let (engine, driver) = RdmaEngineBuilder::new(&name)
        .completion_mode(CompletionMode::Polling)
        .maximum_live_connections(8)
        .maximum_inflight_operations(256)
        .cq_capacity(16_384)
        .build()
        .expect("build pinned provider-probe engine");
    let lease = engine.test_resources().expect("engine test resource lease");
    Some((
        name,
        SharedProbeResources {
            engine,
            driver: Some(driver),
            lease,
        },
    ))
}

fn install_endpoint_route(
    resources: &TestEngineResources,
    endpoint: &mut EngineTestEndpoint,
    operations: impl IntoIterator<Item = TestAcceptedOperation>,
) -> TestRouteHandle {
    let route = resources
        .install_route(endpoint.qp.take().expect("endpoint QP"), operations)
        .unwrap();
    route.retain(endpoint.cm.take().expect("endpoint CM owner"));
    route
}

async fn wait_drained(route: &TestRouteHandle) {
    tokio::time::timeout(Duration::from_secs(10), route.wait_until_drained())
        .await
        .expect("timed out draining provider-probe route");
}

#[test]
fn direct_and_engine_async_flush_sources_require_provider_cqes() {
    assert!(DIRECT_FLUSH_SOURCE.contains("async fn test_v2_completion_error"));
    assert!(DIRECT_FLUSH_SOURCE.contains("WrFlushErr"));
    assert!(
        ENGINE_ASYNC_FLUSH_SOURCE
            .contains("explicit_qp_err_flushes_every_accepted_wr_in_readiness_and_polling_modes")
    );
    assert!(ENGINE_ASYNC_FLUSH_SOURCE.contains("WcStatus::WrFlushErr"));
    assert!(!ENGINE_ASYNC_FLUSH_SOURCE.contains("require_no_iwarp!"));
}

#[test]
fn pinned_provider_limits_include_portable_engine_defaults() {
    let Some((name, resources)) = pinned_resources() else {
        return;
    };
    let limits = resources.lease.provider_limits().unwrap();
    let max_qp = limits.max_qp();
    let max_qp_wr = limits.max_qp_wr();
    let max_sge = limits.max_sge();
    let max_cqe = limits.max_cqe();
    let max_qp_rd_atom = limits.max_qp_rd_atom();
    let max_qp_init_rd_atom = limits.max_qp_init_rd_atom();
    println!(
        "provider={name} max_qp={max_qp} max_qp_wr={max_qp_wr} max_sge={max_sge} max_cqe={max_cqe} max_qp_rd_atom={max_qp_rd_atom} max_qp_init_rd_atom={max_qp_init_rd_atom}"
    );
    assert!(max_qp >= 256);
    assert!(max_qp_wr >= 34);
    assert!(max_sge >= 1);
    assert!(max_cqe >= 16_384);
    assert!(max_qp_rd_atom >= 1);
    assert!(max_qp_init_rd_atom >= 1);
    if name.starts_with("rxe") {
        assert_eq!(max_cqe, 32_767);
    }
}

#[test]
fn provider_limits_reject_unreachable_engine_requests_before_pd_or_cq_creation() {
    let Some((name, resources)) = pinned_resources() else {
        return;
    };
    let limits = resources.lease.provider_limits().unwrap();
    let max_qp = limits.max_qp();
    let max_cqe = limits.max_cqe();
    drop(resources);

    let recorder = DestructionRecorder::arm(16);
    let connection_result = RdmaEngineBuilder::new(&name)
        .completion_mode(CompletionMode::Polling)
        .maximum_live_connections(max_qp + 1)
        .build();
    assert!(matches!(connection_result, Err(Error::InvalidConfig(_))));
    let events = recorder.take();
    assert!(!events.iter().any(|event| {
        matches!(
            event.kind,
            DestructionKind::ProtectionDomain | DestructionKind::CompletionQueue
        )
    }));
    assert!(!recorder.overflowed());

    let cq_result = RdmaEngineBuilder::new(name)
        .completion_mode(CompletionMode::Polling)
        .cq_capacity(max_cqe + 1)
        .build();
    assert!(matches!(cq_result, Err(Error::InvalidConfig(_))));
    let events = recorder.take();
    assert!(!events.iter().any(|event| {
        matches!(
            event.kind,
            DestructionKind::ProtectionDomain | DestructionKind::CompletionQueue
        )
    }));
    assert!(!recorder.overflowed());
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn shared_cq_reports_exact_qp_for_normal_and_explicit_err_completions() {
    let Some((_name, mut resources)) = pinned_resources() else {
        return;
    };
    let driver_task = tokio::spawn(resources.driver.take().expect("provider-probe driver"));
    let mut pair_one = setup_engine_pair(&resources.lease).await;
    let mut pair_two = setup_engine_pair(&resources.lease).await;
    let primary_route = install_endpoint_route(
        &resources.lease,
        &mut pair_one.server,
        [
            TestAcceptedOperation::new(100, WcOpcode::Recv),
            TestAcceptedOperation::new(200, WcOpcode::Recv),
            TestAcceptedOperation::new(202, WcOpcode::Recv),
            TestAcceptedOperation::new(201, WcOpcode::Send),
        ],
    );
    let normal_send_route = install_endpoint_route(
        &resources.lease,
        &mut pair_one.client,
        [TestAcceptedOperation::new(101, WcOpcode::Send)],
    );
    let traffic_recv_route = install_endpoint_route(
        &resources.lease,
        &mut pair_two.server,
        (0..8).map(|index| TestAcceptedOperation::new(300 + index, WcOpcode::Recv)),
    );
    let traffic_send_route = install_endpoint_route(
        &resources.lease,
        &mut pair_two.client,
        (0..8).map(|index| TestAcceptedOperation::new(400 + index, WcOpcode::Send)),
    );

    let mut normal_recv = resources
        .lease
        .register_memory(64, AccessIntent::LocalOnly)
        .unwrap();
    primary_route.qp().post_recv(&mut normal_recv, 100).unwrap();
    primary_route.retain(normal_recv);
    let normal_send = resources
        .lease
        .register_memory(64, AccessIntent::LocalOnly)
        .unwrap();
    normal_send_route.qp().post_send(&normal_send, 101).unwrap();
    normal_send_route.retain(normal_send);
    primary_route.wait_for_completion_count(1).await;
    normal_send_route.wait_for_completion_count(1).await;
    assert!(primary_route.completions()[0].is_success());
    assert!(normal_send_route.completions()[0].is_success());

    let mut flush_recv = resources
        .lease
        .register_memory(64, AccessIntent::LocalOnly)
        .unwrap();
    primary_route.qp().post_recv(&mut flush_recv, 200).unwrap();
    primary_route.retain(flush_recv);
    let mut second_flush_recv = resources
        .lease
        .register_memory(64, AccessIntent::LocalOnly)
        .unwrap();
    primary_route
        .qp()
        .post_recv(&mut second_flush_recv, 202)
        .unwrap();
    primary_route.retain(second_flush_recv);
    let flush_send = resources
        .lease
        .register_memory(64, AccessIntent::LocalOnly)
        .unwrap();
    primary_route.qp().post_send(&flush_send, 201).unwrap();
    primary_route.retain(flush_send);

    for index in 0..8u64 {
        let mut recv = resources
            .lease
            .register_memory(64, AccessIntent::LocalOnly)
            .unwrap();
        traffic_recv_route
            .qp()
            .post_recv(&mut recv, 300 + index)
            .unwrap();
        traffic_recv_route.retain(recv);
        let send = resources
            .lease
            .register_memory(64, AccessIntent::LocalOnly)
            .unwrap();
        traffic_send_route
            .qp()
            .post_send(&send, 400 + index)
            .unwrap();
        traffic_send_route.retain(send);
    }

    let recorder = DestructionRecorder::arm(128);
    primary_route.qp().to_error().unwrap();
    let ((), (), ()) = tokio::join!(
        wait_drained(&primary_route),
        wait_drained(&traffic_recv_route),
        wait_drained(&traffic_send_route)
    );
    wait_drained(&normal_send_route).await;
    let primary_qp = primary_route.qp_num();
    let observed = primary_route.completions();

    assert!(
        observed
            .iter()
            .filter(|completion| matches!(completion.wr_id(), 200..=202))
            .any(|completion| completion.status() == WcStatus::WrFlushErr),
        "explicit local QP ERR must produce at least one flush completion"
    );
    assert!(
        observed
            .iter()
            .all(|completion| completion.qp_num() == primary_qp)
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
    assert!(
        !recorder
            .snapshot()
            .iter()
            .any(|event| event.kind == DestructionKind::QueuePair),
        "routing/draining completions must not destroy a live QP"
    );

    primary_route.remove().unwrap();
    normal_send_route.remove().unwrap();
    traffic_recv_route.remove().unwrap();
    traffic_send_route.remove().unwrap();
    drop(pair_one);
    drop(pair_two);
    resources.engine.shutdown().await.unwrap();
    driver_task.await.unwrap().unwrap();
    drop(resources.lease);
    drop(resources.engine);

    let qp_destroys = recorder
        .take()
        .into_iter()
        .filter(|event| event.kind == DestructionKind::QueuePair)
        .count();
    assert!(!recorder.overflowed());
    assert_eq!(qp_destroys, 4);
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn independently_opened_same_device_context_is_rejected_by_pointer_gate() {
    let Some((name, resources)) = pinned_resources() else {
        return;
    };
    let listener = rdma_io_tests::test_helpers::bind_listener_with_retry().await;
    let connect_addr = connect_addr_for(listener.local_addr());
    let cm = rdma_io_tests::test_helpers::connect_client_cm_with_retry(&connect_addr).await;
    resources.lease.require_context(cm.cm_id()).unwrap();
    assert!(
        !resources
            .lease
            .context_identity()
            .unwrap()
            .matches_independently_opened(&name)
            .unwrap()
    );
    assert_eq!(cm.cm_id().device_name(), Some(name.as_str()));
}
