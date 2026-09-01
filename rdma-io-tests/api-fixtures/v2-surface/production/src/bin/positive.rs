use std::future::Future;

use rdma_io::cm::CmId;
use rdma_io::v2::{
    AccessIntent, Completion, Context, Cq, CqNotifier, CqPoller, Error, MessageTransport,
    MessageTransportBuilder, Mr, Pd, Qp, QpBuilder, RdmaConnectionConfig, RdmaConnectionIdentity,
    RdmaEngine, RdmaEngineDriver, RdmaOperation, ReceivedMessage, RemoteMr, Result,
    TokioCompletions,
};

fn engine_traits<T: Clone + Send + Sync + 'static>() {}
fn future_traits<T: Future<Output = Result<()>> + Send + 'static>() {}
fn operation_traits<T: Future<Output = (Result<Completion>, Option<Mr>)> + Send + 'static>() {}
fn identity_traits<T: Clone + Copy + std::fmt::Debug + Eq + std::hash::Hash>() {}
fn message_traits<T: AsRef<[u8]> + std::ops::Deref<Target = [u8]>>() {}
fn notifier_trait<T: CqNotifier>() {}

fn signatures(
    cq: &Cq,
    poller: &CqPoller,
    qp_builder: QpBuilder<'_>,
    cm_id: &CmId,
    qp: &Qp,
    mr: &mut Mr,
    remote: &RemoteMr,
    identity: &RdmaConnectionIdentity,
) -> Result<()> {
    let mut completions = [Completion::default(); 4];
    let _ = cq.poll(&mut completions)?;
    let mut task = std::task::Context::from_waker(std::task::Waker::noop());
    let _ = poller.poll_completions(&mut task, &mut completions);
    let _ = qp_builder.build_with_cm(cm_id)?;
    qp.post_recv(mr, 1)?;
    qp.post_send(mr, 2)?;
    qp.post_write(mr, remote, 3)?;
    qp.post_read(mr, remote, 4)?;
    let _ = identity.qp_num();
    Ok(())
}

fn main() {
    engine_traits::<RdmaEngine>();
    future_traits::<RdmaEngineDriver>();
    operation_traits::<RdmaOperation>();
    identity_traits::<RdmaConnectionIdentity>();
    message_traits::<ReceivedMessage>();
    let _: Error = std::io::Error::other("retained conversion").into();
    let _: Option<TokioCompletions> = None;
    notifier_trait::<rdma_io::tokio_notifier::TokioCqNotifier>();
    let _: Option<Context> = None;
    let _: Option<Pd> = None;
    let _: Option<AccessIntent> = None;
    let _: Option<RdmaConnectionConfig> = None;
    let _: Option<MessageTransportBuilder> = None;
    let _: Option<MessageTransport> = None;
    let _ = signatures;
}
