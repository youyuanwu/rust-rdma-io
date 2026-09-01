use std::future::Future;

use rdma_io::v2::{
    Completion, Context, Cq, CqPoller, Mr, RdmaConnectionConfig, RdmaEngine, RdmaEngineDriver,
    RdmaOperation, Result,
};

fn engine_traits<T: Clone + Send + Sync + 'static>() {}
fn driver_traits<T: Future<Output = Result<()>> + Send + 'static>() {}
fn operation_traits<T: Future<Output = (Result<Completion>, Option<Mr>)> + Send + 'static>() {}

fn retained_signatures(cq: &Cq, poller: &CqPoller) -> Result<()> {
    let mut completions = [Completion::default(); 2];
    let _ = cq.poll(&mut completions)?;
    let mut task = std::task::Context::from_waker(std::task::Waker::noop());
    let _ = poller.poll_completions(&mut task, &mut completions);
    Ok(())
}

fn main() {
    engine_traits::<RdmaEngine>();
    driver_traits::<RdmaEngineDriver>();
    operation_traits::<RdmaOperation>();
    let _ = RdmaConnectionConfig::default()
        .max_send_wr(8)
        .max_recv_wr(8)
        .retry_count(3);
    let _: Option<Context> = None;
    let _ = retained_signatures;
}
