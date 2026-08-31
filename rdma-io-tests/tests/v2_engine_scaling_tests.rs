use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::Context;
use std::time::Duration;

use futures_util::task::{ArcWake, waker};
use rdma_io::test_support::engine_driver::probe_connection_registry;
use rdma_io::v2::{CompletionMode, RdmaEngineBuilder};
use rdma_io_tests::test_helpers::has_software_rdma;

struct CountingWaker(AtomicUsize);

impl ArcWake for CountingWaker {
    fn wake_by_ref(value: &Arc<Self>) {
        value.0.fetch_add(1, Ordering::AcqRel);
    }
}

fn software_device_name() -> Option<String> {
    let list = rdma_io::cm::RdmaCmDeviceList::new().ok()?;
    list.device_names()
        .into_iter()
        .find(|name| name.starts_with("rxe") || name.starts_with("siw"))
}

#[test]
fn million_slot_registry_uses_paged_constant_probe_lookup() {
    for capacity in [1, 1_024, 1_048_576] {
        let probe = probe_connection_registry(capacity).unwrap();
        assert_eq!(probe.configured_capacity, capacity);
        assert_eq!(probe.page_directory_entries, capacity.div_ceil(256));
        assert_eq!(probe.direct_lookup_probes, probe.touched_slots.len() as u64);
        assert!(probe.touched_pages <= 3);
        assert!(probe.touched_pages <= probe.touched_slots.len());
        assert_eq!(probe.touched_slots.first(), Some(&0));
        assert_eq!(probe.touched_slots.last(), Some(&(capacity - 1)));
    }
}

async fn idle_probe(mode: CompletionMode, connection_count: usize) {
    let device = software_device_name().expect("software RDMA device");
    let (engine, mut driver) = RdmaEngineBuilder::new(device)
        .completion_mode(mode)
        .maximum_live_connections(connection_count)
        .maximum_inflight_operations(2_048)
        .cq_capacity(2_048)
        .shutdown_deadline(Duration::from_secs(5))
        .build()
        .unwrap();
    let resources = engine.test_resources().unwrap();
    let fixtures = resources
        .install_idle_connections(connection_count)
        .unwrap();
    assert_eq!(
        engine.diagnostics().live_connection_reservations,
        connection_count
    );

    let counter = Arc::new(CountingWaker(AtomicUsize::new(0)));
    let task_waker = waker(Arc::clone(&counter));
    let mut context = Context::from_waker(&task_waker);
    assert!(Pin::new(&mut driver).poll(&mut context).is_pending());

    let instrumentation = resources.instrumentation().unwrap();
    assert_eq!(instrumentation.connection_selections, 0);
    assert_eq!(instrumentation.connection_quantum_work, 0);
    assert_eq!(instrumentation.maximum_connection_quantum_work, 0);
    assert_eq!(instrumentation.idle_connection_visits, 0);
    assert_eq!(instrumentation.connection_registry_probes, 0);
    assert_eq!(instrumentation.operation_registry_probes, 0);
    match mode {
        CompletionMode::Readiness => {
            assert_eq!(counter.0.load(Ordering::Acquire), 0);
            assert_eq!(instrumentation.driver_yields, 0);
        }
        CompletionMode::Polling => {
            assert_eq!(counter.0.load(Ordering::Acquire), 1);
            assert_eq!(instrumentation.driver_yields, 1);
        }
    }

    drop(fixtures);
    let driver = tokio::spawn(driver);
    engine.shutdown().await.unwrap();
    driver.await.unwrap().unwrap();
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn one_and_1024_idle_connections_have_identical_zero_visit_cost() {
    if !has_software_rdma() {
        return;
    }
    for mode in [CompletionMode::Readiness, CompletionMode::Polling] {
        idle_probe(mode, 1).await;
        idle_probe(mode, 1_024).await;
    }
}
