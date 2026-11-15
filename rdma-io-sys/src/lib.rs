//! Low-level RDMA sys crate — FFI bindings for libibverbs + librdmacm.
//!
//! Generated at build time by bindgen from the installed rdma-core headers.

#![allow(
    non_snake_case,
    non_upper_case_globals,
    non_camel_case_types,
    dead_code,
    clippy::all
)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    #[test]
    fn generated_declarations_link_all_native_libraries() {
        unsafe {
            assert_eq!(
                CStr::from_ptr(ibv_wc_status_str(IBV_WC_SUCCESS)).to_bytes(),
                b"success"
            );
            assert_eq!(
                CStr::from_ptr(rdma_event_str(RDMA_CM_EVENT_ESTABLISHED)).to_bytes(),
                b"RDMA_CM_EVENT_ESTABLISHED"
            );
            assert_eq!(rdma_wrap_ibv_inc_rkey(0x1234_56ff), 0x1234_5600);
        }
    }
}
