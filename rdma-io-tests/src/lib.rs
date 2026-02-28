//! Integration tests for rdma-io-sys against SoftIWarp (siw).
//!
//! These tests require a siw device to be loaded:
//! ```bash
//! sudo modprobe siw
//! sudo rdma link add siw0 type siw netdev eth0
//! ```
//!
//! All tests exercise the control-path only (device, PD, CQ, MR).
//! Data-path tests (QP, send/recv) require rdma_cm for siw and
//! are not included here.

#[cfg(test)]
mod tests {
    use rdma_io_sys::ibverbs::*;
    use std::ffi::CStr;

    /// Find a siw device from the device list, returning its name.
    fn find_siw_device() -> Option<String> {
        unsafe {
            let mut num_devices: i32 = 0;
            let device_list = ibv_get_device_list(&mut num_devices);
            if device_list.is_null() || num_devices == 0 {
                return None;
            }

            let mut result = None;
            for i in 0..num_devices as isize {
                let device = *device_list.offset(i);
                if device.is_null() {
                    continue;
                }
                let name_ptr = ibv_get_device_name(device);
                if !name_ptr.is_null() {
                    let name = CStr::from_ptr(name_ptr).to_string_lossy().into_owned();
                    if name.starts_with("siw") {
                        result = Some(name);
                        break;
                    }
                }
            }

            ibv_free_device_list(device_list);
            result
        }
    }

    /// Open a siw device, returning (context, device_name).
    /// Panics if no siw device is available.
    fn open_siw_device() -> (*mut ibv_context, String) {
        unsafe {
            let mut num_devices: i32 = 0;
            let device_list = ibv_get_device_list(&mut num_devices);
            assert!(!device_list.is_null(), "ibv_get_device_list returned null");
            assert!(num_devices > 0, "no RDMA devices found");

            let mut ctx = std::ptr::null_mut();
            let mut name = String::new();

            for i in 0..num_devices as isize {
                let device = *device_list.offset(i);
                if device.is_null() {
                    continue;
                }
                let name_ptr = ibv_get_device_name(device);
                if !name_ptr.is_null() {
                    let n = CStr::from_ptr(name_ptr).to_string_lossy().into_owned();
                    if n.starts_with("siw") {
                        ctx = ibv_open_device(device);
                        name = n;
                        break;
                    }
                }
            }

            ibv_free_device_list(device_list);
            assert!(
                !ctx.is_null(),
                "no siw device found or ibv_open_device failed"
            );
            (ctx, name)
        }
    }

    #[test]
    fn test_siw_device_available() {
        let name = find_siw_device();
        assert!(
            name.is_some(),
            "No siw device found. Run: sudo modprobe siw && sudo rdma link add siw0 type siw netdev eth0"
        );
        println!("Found siw device: {}", name.unwrap());
    }

    #[test]
    fn test_open_close_device() {
        let (ctx, name) = open_siw_device();
        println!("Opened device: {}", name);
        let ret = unsafe { ibv_close_device(ctx) };
        assert_eq!(ret, 0, "ibv_close_device failed");
    }

    #[test]
    fn test_query_device() {
        let (ctx, name) = open_siw_device();
        unsafe {
            let mut attr: ibv_device_attr = std::mem::zeroed();
            let ret = ibv_query_device(ctx, &mut attr);
            assert_eq!(ret, 0, "ibv_query_device failed");

            println!("Device: {name}");
            println!("  max_qp: {}", attr.max_qp);
            println!("  max_cq: {}", attr.max_cq);
            println!("  max_mr: {}", attr.max_mr);
            println!("  max_pd: {}", attr.max_pd);
            println!("  max_qp_wr: {}", attr.max_qp_wr);
            println!("  max_cqe: {}", attr.max_cqe);

            assert!(attr.max_qp > 0, "max_qp should be > 0");
            assert!(attr.max_cq > 0, "max_cq should be > 0");
            assert!(attr.max_mr > 0, "max_mr should be > 0");

            ibv_close_device(ctx);
        }
    }

    #[test]
    fn test_alloc_dealloc_pd() {
        let (ctx, _) = open_siw_device();
        unsafe {
            let pd = ibv_alloc_pd(ctx);
            assert!(!pd.is_null(), "ibv_alloc_pd failed");

            let ret = ibv_dealloc_pd(pd);
            assert_eq!(ret, 0, "ibv_dealloc_pd failed");

            ibv_close_device(ctx);
        }
    }

    #[test]
    fn test_create_destroy_cq() {
        let (ctx, _) = open_siw_device();
        unsafe {
            let cq = ibv_create_cq(ctx, 16, std::ptr::null_mut(), std::ptr::null_mut(), 0);
            assert!(!cq.is_null(), "ibv_create_cq failed");

            let ret = ibv_destroy_cq(cq);
            assert_eq!(ret, 0, "ibv_destroy_cq failed");

            ibv_close_device(ctx);
        }
    }

    #[test]
    fn test_register_deregister_mr() {
        let (ctx, _) = open_siw_device();
        unsafe {
            let pd = ibv_alloc_pd(ctx);
            assert!(!pd.is_null(), "ibv_alloc_pd failed");

            let mut buf = vec![0u8; 4096];
            let mr = rdma_io_sys::wrapper::rdma_wrap___ibv_reg_mr(
                pd,
                buf.as_mut_ptr() as *mut core::ffi::c_void,
                buf.len() as u64,
                IBV_ACCESS_LOCAL_WRITE | IBV_ACCESS_REMOTE_READ | IBV_ACCESS_REMOTE_WRITE,
                1,
            );
            assert!(!mr.is_null(), "ibv_reg_mr failed");
            assert!((*mr).lkey != 0, "lkey should be non-zero");
            assert!((*mr).rkey != 0, "rkey should be non-zero");
            println!("MR registered: lkey={}, rkey={}", (*mr).lkey, (*mr).rkey);

            let ret = ibv_dereg_mr(mr);
            assert_eq!(ret, 0, "ibv_dereg_mr failed");

            ibv_dealloc_pd(pd);
            ibv_close_device(ctx);
        }
    }

    #[test]
    fn test_create_qp_with_siw() {
        // QP creation works on siw, but manual state transitions
        // (INIT→RTR→RTS) fail because iWARP needs rdma_cm.
        let (ctx, _) = open_siw_device();
        unsafe {
            let pd = ibv_alloc_pd(ctx);
            assert!(!pd.is_null());

            let send_cq = ibv_create_cq(ctx, 16, std::ptr::null_mut(), std::ptr::null_mut(), 0);
            let recv_cq = ibv_create_cq(ctx, 16, std::ptr::null_mut(), std::ptr::null_mut(), 0);
            assert!(!send_cq.is_null());
            assert!(!recv_cq.is_null());

            let mut qp_init_attr: ibv_qp_init_attr = std::mem::zeroed();
            qp_init_attr.send_cq = send_cq;
            qp_init_attr.recv_cq = recv_cq;
            qp_init_attr.cap.max_send_wr = 16;
            qp_init_attr.cap.max_recv_wr = 16;
            qp_init_attr.cap.max_send_sge = 1;
            qp_init_attr.cap.max_recv_sge = 1;
            qp_init_attr.qp_type = IBV_QPT_RC;

            let qp = ibv_create_qp(pd, &mut qp_init_attr);
            assert!(!qp.is_null(), "ibv_create_qp failed on siw");
            println!("QP created: qp_num={}", (*qp).qp_num);

            let ret = ibv_destroy_qp(qp);
            assert_eq!(ret, 0, "ibv_destroy_qp failed");

            ibv_destroy_cq(send_cq);
            ibv_destroy_cq(recv_cq);
            ibv_dealloc_pd(pd);
            ibv_close_device(ctx);
        }
    }

    #[test]
    fn test_multiple_resources() {
        let (ctx, _) = open_siw_device();
        unsafe {
            let pd1 = ibv_alloc_pd(ctx);
            let pd2 = ibv_alloc_pd(ctx);
            assert!(!pd1.is_null());
            assert!(!pd2.is_null());

            let cq1 = ibv_create_cq(ctx, 32, std::ptr::null_mut(), std::ptr::null_mut(), 0);
            let cq2 = ibv_create_cq(ctx, 64, std::ptr::null_mut(), std::ptr::null_mut(), 0);
            assert!(!cq1.is_null());
            assert!(!cq2.is_null());

            let mut buf1 = vec![0u8; 1024];
            let mut buf2 = vec![0u8; 2048];
            let mr1 = rdma_io_sys::wrapper::rdma_wrap___ibv_reg_mr(
                pd1,
                buf1.as_mut_ptr() as *mut core::ffi::c_void,
                buf1.len() as u64,
                IBV_ACCESS_LOCAL_WRITE,
                1,
            );
            let mr2 = rdma_io_sys::wrapper::rdma_wrap___ibv_reg_mr(
                pd2,
                buf2.as_mut_ptr() as *mut core::ffi::c_void,
                buf2.len() as u64,
                IBV_ACCESS_LOCAL_WRITE,
                1,
            );
            assert!(!mr1.is_null());
            assert!(!mr2.is_null());

            ibv_dereg_mr(mr2);
            ibv_dereg_mr(mr1);
            ibv_destroy_cq(cq2);
            ibv_destroy_cq(cq1);
            ibv_dealloc_pd(pd2);
            ibv_dealloc_pd(pd1);
            ibv_close_device(ctx);
        }
    }
}
