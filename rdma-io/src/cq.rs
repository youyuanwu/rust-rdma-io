//! Completion Queue.

use std::sync::Arc;

use rdma_io_sys::ibverbs::*;
use rdma_io_sys::wrapper::*;

use crate::Result;
use crate::comp_channel::CompletionChannel;
use crate::device::Context;
use crate::error::{from_ptr, from_ret};
use crate::wc::WorkCompletion;

/// An RDMA Completion Queue (`ibv_cq`).
pub struct CompletionQueue {
    pub(crate) inner: *mut ibv_cq,
    _ctx: Arc<Context>,
}

// Safety: ibv_cq is thread-safe (polling serialized by caller or lock).
unsafe impl Send for CompletionQueue {}
unsafe impl Sync for CompletionQueue {}

impl Drop for CompletionQueue {
    fn drop(&mut self) {
        let ret = unsafe { ibv_destroy_cq(self.inner) };
        if ret != 0 {
            tracing::error!(
                "ibv_destroy_cq failed: {}",
                std::io::Error::from_raw_os_error(-ret)
            );
        }
    }
}

impl CompletionQueue {
    /// Create a new CQ with `cqe` entries.
    pub fn new(ctx: Arc<Context>, cqe: i32) -> Result<Arc<Self>> {
        let cq = from_ptr(unsafe {
            ibv_create_cq(
                ctx.inner,
                cqe,
                std::ptr::null_mut(), // cq_context
                std::ptr::null_mut(), // comp_channel
                0,                    // comp_vector
            )
        })?;
        Ok(Arc::new(Self {
            inner: cq,
            _ctx: ctx,
        }))
    }

    /// Poll up to `wc_buf.len()` completions.
    ///
    /// Returns the number of completions written to `wc_buf`.
    pub fn poll(&self, wc_buf: &mut [WorkCompletion]) -> Result<usize> {
        let n = unsafe {
            rdma_wrap_ibv_poll_cq(self.inner, wc_buf.len() as i32, wc_buf.as_mut_ptr().cast())
        };
        if n < 0 {
            Err(crate::Error::Verbs(std::io::Error::from_raw_os_error(-n)))
        } else {
            Ok(n as usize)
        }
    }

    /// Request notification for the next completion.
    pub fn req_notify(&self, solicited_only: bool) -> Result<()> {
        from_ret(unsafe { rdma_wrap_ibv_req_notify_cq(self.inner, i32::from(solicited_only)) })
    }

    /// Raw pointer (for advanced/FFI use).
    pub fn as_raw(&self) -> *mut ibv_cq {
        self.inner
    }

    /// Create a CQ associated with a completion channel.
    ///
    /// When completions arrive, the channel's fd becomes readable,
    /// enabling async notification via `epoll`/`kqueue`.
    pub fn with_comp_channel(
        ctx: Arc<Context>,
        cqe: i32,
        channel: &CompletionChannel,
    ) -> Result<Arc<Self>> {
        let cq = from_ptr(unsafe {
            ibv_create_cq(
                ctx.inner,
                cqe,
                std::ptr::null_mut(), // cq_context
                channel.as_raw(),     // comp_channel
                0,                    // comp_vector
            )
        })?;
        Ok(Arc::new(Self {
            inner: cq,
            _ctx: ctx,
        }))
    }
}

impl Context {
    /// Create a Completion Queue.
    pub fn create_cq(self: &Arc<Self>, cqe: i32) -> Result<Arc<CompletionQueue>> {
        CompletionQueue::new(Arc::clone(self), cqe)
    }
}
