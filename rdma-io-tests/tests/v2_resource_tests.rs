use rdma_io::cm::RdmaCmDeviceList;
use rdma_io::test_support::destruction::{self, DestructionKind};
use rdma_io::v2::{
    AccessIntent, CompletionMode, Context, CqBuilder, Error, RdmaEngineBuilder, RdmaEngineLifecycle,
};
use rdma_io_tests::test_helpers::has_software_rdma;

fn software_device_name(list: &RdmaCmDeviceList) -> Option<String> {
    list.device_names()
        .into_iter()
        .find(|name| name.starts_with("rxe") || name.starts_with("siw"))
}

#[test]
fn pinned_context_owns_pd_cq_and_mr_until_last_child_drop() {
    if !has_software_rdma() {
        return;
    }

    destruction::clear();
    let list = RdmaCmDeviceList::new().expect("enumerate librdmacm devices");
    let name = software_device_name(&list).expect("software RDMA context");
    let inner = list.context_by_name(&name).expect("select exact context");
    assert!(list.contains_context(&inner));
    assert_eq!(inner.device_name(), Some(name.as_str()));
    drop(list);

    let context = Context::from_inner(inner);
    let pd = context.alloc_pd().expect("allocate anchored PD");
    let cq = CqBuilder::new(&context, 16)
        .build()
        .expect("allocate anchored CQ");
    let mr = pd
        .reg_mr(64, AccessIntent::LocalOnly)
        .expect("register anchored MR");

    assert!(
        !destruction::snapshot()
            .iter()
            .any(|event| event.kind == DestructionKind::RdmaFreeDevices)
    );
    drop(mr);
    drop(cq);
    drop(pd);
    drop(context);

    let events = destruction::take();
    assert!(
        !events
            .iter()
            .any(|event| event.kind == DestructionKind::IbvCloseDevice)
    );
    assert_eq!(
        events.last().map(|event| event.kind),
        Some(DestructionKind::RdmaFreeDevices)
    );
    assert!(
        events
            .iter()
            .any(|event| event.kind == DestructionKind::ContextFacade)
    );
}

#[test]
fn provider_resources_and_mr_validation_live_in_integration_tests() {
    if !has_software_rdma() {
        return;
    }
    let list = RdmaCmDeviceList::new().expect("enumerate librdmacm devices");
    let name = software_device_name(&list).expect("software RDMA context");
    let context = Context::from_inner(list.context_by_name(&name).unwrap());
    let pd = context.alloc_pd().unwrap();

    let poll_cq = CqBuilder::new(&context, 16).build().unwrap();
    assert!(!poll_cq.has_channel());
    assert!(poll_cq.fd().is_none());

    let readiness_cq = CqBuilder::new(&context, 16).with_channel().build().unwrap();
    assert!(readiness_cq.has_channel());
    assert!(readiness_cq.fd().is_some());

    assert!(matches!(
        pd.reg_mr(0, AccessIntent::LocalOnly),
        Err(Error::InvalidConfig(_))
    ));
    for intent in [
        AccessIntent::LocalOnly,
        AccessIntent::RemoteRead,
        AccessIntent::RemoteWrite,
        AccessIntent::RemoteReadWrite,
    ] {
        assert_eq!(pd.reg_mr(64, intent).unwrap().len(), 64);
    }
}

#[test]
fn polling_engine_builds_outside_a_runtime_without_io_adapters() {
    if !has_software_rdma() {
        return;
    }
    let list = RdmaCmDeviceList::new().unwrap();
    let name = software_device_name(&list).unwrap();
    drop(list);

    let (engine, driver) = RdmaEngineBuilder::new(name)
        .completion_mode(CompletionMode::Polling)
        .build()
        .expect("polling build outside Tokio");
    let diagnostics = engine.diagnostics();
    assert_eq!(diagnostics.lifecycle, RdmaEngineLifecycle::Created);
    assert_eq!(diagnostics.shared_contexts, 1);
    assert_eq!(diagnostics.shared_protection_domains, 1);
    assert_eq!(diagnostics.shared_completion_queues, 1);
    assert_eq!(diagnostics.shared_completion_channels, 0);
    assert_eq!(diagnostics.shared_cm_event_channels, 1);
    drop(engine);
    drop(driver);
}

#[test_log::test(tokio::test(flavor = "current_thread"))]
async fn readiness_engine_builds_with_one_channel_and_direct_driver() {
    if !has_software_rdma() {
        return;
    }
    let list = RdmaCmDeviceList::new().unwrap();
    let name = software_device_name(&list).unwrap();
    drop(list);

    let (engine, driver) = RdmaEngineBuilder::new(name).build().unwrap();
    let diagnostics = engine.diagnostics();
    assert_eq!(diagnostics.shared_completion_channels, 1);
    let task = tokio::spawn(driver);
    engine.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
    assert_eq!(
        engine.diagnostics().lifecycle,
        RdmaEngineLifecycle::Terminated
    );
}

#[test]
fn invalid_engine_configuration_allocates_no_provider_resources() {
    destruction::clear();
    let result = RdmaEngineBuilder::new("rxe0")
        .completion_mode(CompletionMode::Polling)
        .maximum_live_connections(0)
        .build();
    assert!(matches!(result, Err(Error::InvalidConfig(_))));
    assert!(destruction::take().is_empty());
}
