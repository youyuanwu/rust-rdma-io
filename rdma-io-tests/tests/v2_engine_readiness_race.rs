use std::collections::HashSet;
use std::time::Duration;

use rdma_io::test_support::engine_driver::{TestAcceptedOperation, TestEngineResources};
use rdma_io::v2::{AccessIntent, CompletionMode, RdmaEngineBuilder};
use rdma_io::wc::WcOpcode;
use rdma_io_tests::engine_test_helpers::{EngineTestEndpoint, setup_engine_pair};
use rdma_io_tests::test_helpers::has_software_rdma;

const RACE_ITERATIONS: usize = 256;

fn software_device_name() -> Option<String> {
    let list = rdma_io::cm::RdmaCmDeviceList::new().ok()?;
    list.device_names()
        .into_iter()
        .find(|name| name.starts_with("rxe") || name.starts_with("siw"))
}

fn take_endpoint_route(
    resources: &TestEngineResources,
    endpoint: &mut EngineTestEndpoint,
    operations: impl IntoIterator<Item = TestAcceptedOperation>,
) -> rdma_io::test_support::engine_driver::TestRouteHandle {
    let route = resources
        .install_route(endpoint.qp.take().expect("endpoint QP"), operations)
        .unwrap();
    route.retain(endpoint.cm.take().expect("endpoint CM owner"));
    route
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn readiness_arm_post_race_observes_each_cqe_exactly_once() {
    if !has_software_rdma() {
        return;
    }
    let device = software_device_name().expect("software RDMA device");
    let (engine, driver) = RdmaEngineBuilder::new(device)
        .completion_mode(CompletionMode::Readiness)
        .maximum_live_connections(4)
        .maximum_inflight_operations(1024)
        .cq_capacity(1024)
        .build()
        .unwrap();
    let resources = engine.test_resources().unwrap();
    let mut pair = setup_engine_pair(&resources).await;

    let recv_route = take_endpoint_route(
        &resources,
        &mut pair.server,
        (0..RACE_ITERATIONS * 2)
            .map(|index| TestAcceptedOperation::new(100_000 + index as u64, WcOpcode::Recv)),
    );
    let send_route = take_endpoint_route(
        &resources,
        &mut pair.client,
        (0..RACE_ITERATIONS * 2)
            .map(|index| TestAcceptedOperation::new(200_000 + index as u64, WcOpcode::Send)),
    );

    let driver_task = tokio::spawn(driver);
    let mut arm_generation = 0;
    for index in 0..RACE_ITERATIONS {
        let arm_window = resources.pause_next_cq_arm_window().unwrap();
        let kick_index = index * 2;
        let kick_recv_id = 100_000 + kick_index as u64;
        let kick_send_id = 200_000 + kick_index as u64;
        let mut kick_recv = resources
            .register_memory(64, AccessIntent::LocalOnly)
            .unwrap();
        recv_route
            .qp()
            .post_recv(&mut kick_recv, kick_recv_id)
            .unwrap();
        recv_route.retain(kick_recv);
        let kick_send = resources
            .register_memory(64, AccessIntent::LocalOnly)
            .unwrap();
        send_route.qp().post_send(&kick_send, kick_send_id).unwrap();
        send_route.retain(kick_send);

        arm_generation = tokio::time::timeout(
            Duration::from_secs(10),
            arm_window.wait_for_pause_after(arm_generation),
        )
        .await
        .expect("driver did not pause in the CQ arm-to-post-poll window")
        .unwrap();

        let race_index = kick_index + 1;
        let recv_id = 100_000 + race_index as u64;
        let send_id = 200_000 + race_index as u64;
        let mut recv = resources
            .register_memory(64, AccessIntent::LocalOnly)
            .unwrap();
        recv_route.qp().post_recv(&mut recv, recv_id).unwrap();
        recv_route.retain(recv);
        let send = resources
            .register_memory(64, AccessIntent::LocalOnly)
            .unwrap();
        send_route.qp().post_send(&send, send_id).unwrap();
        send_route.retain(send);
        arm_window.release(arm_generation).unwrap();

        tokio::time::timeout(Duration::from_secs(10), async {
            let expected = (index + 1) * 2;
            tokio::join!(
                recv_route.wait_for_completion_count(expected),
                send_route.wait_for_completion_count(expected)
            );
        })
        .await
        .expect("arm/post race lost a shared-CQ completion");
    }

    let recv_completions = recv_route.remove().unwrap();
    let send_completions = send_route.remove().unwrap();
    assert_eq!(recv_completions.len(), RACE_ITERATIONS * 2);
    assert_eq!(send_completions.len(), RACE_ITERATIONS * 2);
    assert_eq!(
        recv_completions
            .iter()
            .map(|completion| completion.wr_id())
            .collect::<HashSet<_>>()
            .len(),
        RACE_ITERATIONS * 2
    );
    assert_eq!(
        send_completions
            .iter()
            .map(|completion| completion.wr_id())
            .collect::<HashSet<_>>()
            .len(),
        RACE_ITERATIONS * 2
    );
    assert!(
        recv_completions
            .iter()
            .all(|completion| completion.is_success())
    );
    assert!(
        send_completions
            .iter()
            .all(|completion| completion.is_success())
    );

    drop(pair);
    engine.shutdown().await.unwrap();
    driver_task.await.unwrap().unwrap();
}
