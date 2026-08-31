use crate::cm::RdmaCmDeviceList;
use crate::test_support::destruction::{DestructionKind, DestructionRecorder};
use crate::v2::{CompletionMode, RdmaEngineBuilder};

fn software_device_name() -> Option<String> {
    let devices = RdmaCmDeviceList::new().ok()?;
    devices
        .device_names()
        .into_iter()
        .find(|name| name.starts_with("rxe") || name.starts_with("siw"))
}

fn position(
    events: &[crate::test_support::destruction::DestructionEvent],
    kind: DestructionKind,
) -> usize {
    events
        .iter()
        .position(|event| event.kind == kind)
        .unwrap_or_else(|| panic!("missing destruction event {kind:?}"))
}

async fn assert_canonical_drop_order(mode: CompletionMode) {
    let Some(device) = software_device_name() else {
        return;
    };
    let recorder = DestructionRecorder::arm(64);
    let (engine, driver) = RdmaEngineBuilder::new(device)
        .completion_mode(mode)
        .maximum_live_connections(1)
        .maximum_inflight_operations(64)
        .cq_capacity(64)
        .build()
        .unwrap();

    let (shutdown, driven) = tokio::join!(engine.shutdown(), driver);
    shutdown.unwrap();
    driven.unwrap();
    drop(engine);

    let events = recorder.take();
    assert!(!recorder.overflowed());
    let final_cm_drain = position(&events, DestructionKind::CmFinalDrainToWouldBlock);
    let cq = position(&events, DestructionKind::CompletionQueue);
    let pd = position(&events, DestructionKind::ProtectionDomain);
    let cm_channel = position(&events, DestructionKind::CmEventChannel);
    let context = position(&events, DestructionKind::ContextFacade);
    let anchor = position(&events, DestructionKind::RdmaFreeDevices);
    assert!(final_cm_drain < cq);
    assert!(cq < pd);
    assert!(pd < cm_channel);
    assert!(cm_channel < context);
    assert!(context < anchor);

    if mode == CompletionMode::Readiness {
        let cq_adapter = position(&events, DestructionKind::CqReadinessAdapter);
        let cm_adapter = position(&events, DestructionKind::CmReadinessAdapter);
        let completion_channel = position(&events, DestructionKind::CompletionChannel);
        assert!(final_cm_drain < cq_adapter);
        assert!(final_cm_drain < cm_adapter);
        assert!(cq_adapter < cq);
        assert!(cm_adapter < cm_channel);
        assert!(cq < completion_channel);
        assert!(completion_channel < pd);
    } else {
        assert!(!events.iter().any(|event| matches!(
            event.kind,
            DestructionKind::CqReadinessAdapter | DestructionKind::CmReadinessAdapter
        )));
    }
    assert_eq!(events.last().map(|event| event.kind), Some(anchor_kind()));
}

const fn anchor_kind() -> DestructionKind {
    DestructionKind::RdmaFreeDevices
}

#[tokio::test(flavor = "current_thread")]
async fn actual_wrappers_drop_in_canonical_order_in_both_modes() {
    assert_canonical_drop_order(CompletionMode::Readiness).await;
    assert_canonical_drop_order(CompletionMode::Polling).await;
}
