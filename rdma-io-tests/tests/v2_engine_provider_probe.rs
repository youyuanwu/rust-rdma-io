use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use rdma_io::async_cm::AsyncCmId;
use rdma_io::cm::{ConnParam, RdmaCmDeviceList};
use rdma_io::test_support::destruction::{self, DestructionKind};
use rdma_io::v2::{
    AccessIntent, CompletionMode, Context, Cq, CqBuilder, Error, Pd, Qp, QpBuilder,
    RdmaEngineBuilder,
};
use rdma_io::wc::{WcStatus, WorkCompletion};
use rdma_io_tests::test_helpers::{connect_addr_for, has_software_rdma};

const DIRECT_FLUSH_SOURCE: &str = include_str!("v2_tests.rs");
const OLD_ASYNC_FLUSH_SOURCE: &str = include_str!("v2_shared_qp_tests.rs");

struct Endpoint {
    qp: Qp,
    _cm: AsyncCmId,
}

struct Pair {
    server: Endpoint,
    client: Endpoint,
}

struct SharedProbeResources {
    anchored_context: Arc<rdma_io::device::Context>,
    pd: Pd,
    cq: Arc<Cq>,
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
    let anchored_context = list.context_by_name(&name).expect("select exact context");
    drop(list);
    let context = Context::from_inner(Arc::clone(&anchored_context));
    let pd = context.alloc_pd().expect("allocate shared PD");
    let cq = Arc::new(
        CqBuilder::new(&context, 256)
            .build()
            .expect("allocate shared CQ"),
    );
    Some((
        name,
        SharedProbeResources {
            anchored_context,
            pd,
            cq,
        },
    ))
}

async fn setup_pair(resources: &SharedProbeResources) -> Pair {
    let listener = rdma_io_tests::test_helpers::bind_listener_with_retry().await;
    let connect_addr = connect_addr_for(listener.local_addr());

    let server_pd = resources.pd.clone();
    let server_cq = Arc::clone(&resources.cq);
    let server_context = Arc::clone(&resources.anchored_context);
    let server = tokio::spawn(async move {
        let conn_id = listener.get_request().await.unwrap();
        assert!(
            conn_id.uses_context(&server_context),
            "inbound child must use the exact pinned context"
        );
        let qp = QpBuilder::new(&server_pd, &server_cq, &server_cq)
            .max_send_wr(64)
            .max_recv_wr(64)
            .build_with_cm(&conn_id)
            .unwrap();
        let cm = listener
            .complete_accept(conn_id, &ConnParam::default())
            .await
            .unwrap();
        Endpoint { qp, _cm: cm }
    });

    let client_pd = resources.pd.clone();
    let client_cq = Arc::clone(&resources.cq);
    let client_context = Arc::clone(&resources.anchored_context);
    let client = tokio::spawn(async move {
        let (cm, qp) =
            rdma_io_tests::test_helpers::connect_client_with_retry(&connect_addr, |cm| {
                assert!(
                    cm.cm_id().uses_context(&client_context),
                    "outbound route must use the exact pinned context"
                );
                let cm_qp = cm
                    .create_qp_with_cq(
                        client_pd.inner(),
                        &rdma_io::qp::QpInitAttr {
                            max_send_wr: 64,
                            max_recv_wr: 64,
                            ..Default::default()
                        },
                        Some(client_cq.inner()),
                        Some(client_cq.inner()),
                    )
                    .unwrap();
                Qp::from_cm_qp(cm_qp)
            })
            .await;
        Endpoint { qp, _cm: cm }
    });

    let (server, client) = tokio::join!(server, client);
    Pair {
        server: server.unwrap(),
        client: client.unwrap(),
    }
}

async fn collect_expected(
    cq: &Cq,
    expected: &HashMap<u64, (u32, Option<bool>)>,
) -> HashMap<u64, WorkCompletion> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut observed = HashMap::new();
        let mut completions = [WorkCompletion::default(); 16];
        while observed.len() < expected.len() {
            let count = cq.poll(&mut completions).unwrap();
            if count == 0 {
                tokio::task::yield_now().await;
                continue;
            }
            for completion in &completions[..count] {
                let wr_id = completion.wr_id();
                if let Some((expected_qp, should_succeed)) = expected.get(&wr_id) {
                    assert_eq!(
                        completion.qp_num(),
                        *expected_qp,
                        "CQE must report its exact owning QP"
                    );
                    if let Some(should_succeed) = should_succeed {
                        assert_eq!(completion.is_success(), *should_succeed);
                    }
                    assert!(
                        observed.insert(wr_id, *completion).is_none(),
                        "duplicate completion for WR {wr_id}"
                    );
                }
            }
        }
        observed
    })
    .await
    .expect("timed out draining shared CQ")
}

#[test]
fn baseline_sources_distinguish_provider_capability_from_old_async_skip() {
    assert!(DIRECT_FLUSH_SOURCE.contains("async fn test_v2_completion_error"));
    assert!(DIRECT_FLUSH_SOURCE.contains("WrFlushErr"));
    assert!(OLD_ASYNC_FLUSH_SOURCE.contains("async fn test_shared_qp_completion_error"));
    assert!(OLD_ASYNC_FLUSH_SOURCE.contains("require_no_iwarp!"));
}

#[test]
fn pinned_provider_limits_include_portable_engine_defaults() {
    let Some((name, resources)) = pinned_resources() else {
        return;
    };
    let attr = resources.anchored_context.query_device().unwrap();
    let max_qp = attr.max_qp;
    let max_qp_wr = attr.max_qp_wr;
    let max_sge = attr.max_sge;
    let max_cqe = attr.max_cqe;
    let max_qp_rd_atom = attr.max_qp_rd_atom;
    let max_qp_init_rd_atom = attr.max_qp_init_rd_atom;
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
    let attr = resources.anchored_context.query_device().unwrap();
    let max_qp = attr.max_qp as usize;
    let max_cqe = attr.max_cqe as usize;
    drop(resources);

    destruction::clear();
    let connection_result = RdmaEngineBuilder::new(&name)
        .completion_mode(CompletionMode::Polling)
        .maximum_live_connections(max_qp + 1)
        .build();
    assert!(matches!(connection_result, Err(Error::InvalidConfig(_))));
    let events = destruction::take();
    assert!(!events.iter().any(|event| {
        matches!(
            event.kind,
            DestructionKind::ProtectionDomain | DestructionKind::CompletionQueue
        )
    }));

    destruction::clear();
    let cq_result = RdmaEngineBuilder::new(name)
        .completion_mode(CompletionMode::Polling)
        .cq_capacity(max_cqe + 1)
        .build();
    assert!(matches!(cq_result, Err(Error::InvalidConfig(_))));
    let events = destruction::take();
    assert!(!events.iter().any(|event| {
        matches!(
            event.kind,
            DestructionKind::ProtectionDomain | DestructionKind::CompletionQueue
        )
    }));
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn shared_cq_reports_exact_qp_for_normal_and_explicit_err_completions() {
    let Some((_name, resources)) = pinned_resources() else {
        return;
    };
    let pair_one = setup_pair(&resources).await;
    let pair_two = setup_pair(&resources).await;

    let mut normal_recv = resources.pd.reg_mr(64, AccessIntent::LocalOnly).unwrap();
    pair_one.server.qp.post_recv(&mut normal_recv, 100).unwrap();
    let normal_send = resources.pd.reg_mr(64, AccessIntent::LocalOnly).unwrap();
    pair_one.client.qp.post_send(&normal_send, 101).unwrap();
    let normal_expected = HashMap::from([
        (100, (pair_one.server.qp.qp_num(), Some(true))),
        (101, (pair_one.client.qp.qp_num(), Some(true))),
    ]);
    collect_expected(&resources.cq, &normal_expected).await;

    let mut flush_recv = resources.pd.reg_mr(64, AccessIntent::LocalOnly).unwrap();
    pair_one.server.qp.post_recv(&mut flush_recv, 200).unwrap();
    let mut second_flush_recv = resources.pd.reg_mr(64, AccessIntent::LocalOnly).unwrap();
    pair_one
        .server
        .qp
        .post_recv(&mut second_flush_recv, 202)
        .unwrap();
    let flush_send = resources.pd.reg_mr(64, AccessIntent::LocalOnly).unwrap();
    pair_one.server.qp.post_send(&flush_send, 201).unwrap();

    let mut unrelated_recvs = Vec::new();
    let mut unrelated_sends = Vec::new();
    for index in 0..8u64 {
        let mut recv = resources.pd.reg_mr(64, AccessIntent::LocalOnly).unwrap();
        pair_two
            .server
            .qp
            .post_recv(&mut recv, 300 + index)
            .unwrap();
        let send = resources.pd.reg_mr(64, AccessIntent::LocalOnly).unwrap();
        pair_two.client.qp.post_send(&send, 400 + index).unwrap();
        unrelated_recvs.push(recv);
        unrelated_sends.push(send);
    }

    destruction::clear();
    pair_one.server.qp.to_error().unwrap();
    let mut expected = HashMap::from([
        (200, (pair_one.server.qp.qp_num(), None)),
        (201, (pair_one.server.qp.qp_num(), None)),
        (202, (pair_one.server.qp.qp_num(), None)),
    ]);
    for index in 0..8u64 {
        expected.insert(300 + index, (pair_two.server.qp.qp_num(), Some(true)));
        expected.insert(400 + index, (pair_two.client.qp.qp_num(), Some(true)));
    }
    let observed = collect_expected(&resources.cq, &expected).await;

    assert!(
        [200, 201, 202]
            .into_iter()
            .any(|wr_id| observed[&wr_id].status() == WcStatus::WrFlushErr),
        "explicit local QP ERR must produce at least one flush completion"
    );
    assert!(
        !destruction::snapshot()
            .iter()
            .any(|event| event.kind == DestructionKind::QueuePair),
        "routing/draining completions must not destroy a live QP"
    );

    drop(normal_recv);
    drop(normal_send);
    drop(flush_recv);
    drop(second_flush_recv);
    drop(flush_send);
    drop(unrelated_recvs);
    drop(unrelated_sends);
    drop(pair_one);
    drop(pair_two);

    let qp_destroys = destruction::take()
        .into_iter()
        .filter(|event| event.kind == DestructionKind::QueuePair)
        .count();
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
    let independent = Context::open_by_name(&name).unwrap();

    cm.cm_id()
        .require_context(&resources.anchored_context)
        .unwrap();
    assert!(cm.cm_id().require_context(independent.inner()).is_err());
    assert_eq!(cm.cm_id().device_name(), Some(name.as_str()));
}
