#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap____ibv_query_port(context : *mut super::verbs::ibv_context, port_num : u8, port_attr : *mut super::verbs::ibv_port_attr) -> i32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap___ibv_reg_mr(pd : *mut super::verbs::ibv_pd, addr : *mut core::ffi::c_void, length : usize, access : u32, is_access_const : i32) -> *mut super::verbs::ibv_mr);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap___ibv_reg_mr_iova(pd : *mut super::verbs::ibv_pd, addr : *mut core::ffi::c_void, length : usize, iova : u64, access : u32, is_access_const : i32) -> *mut super::verbs::ibv_mr);
#[cfg(all(feature = "ib_user_ioctl_verbs", feature = "verbs"))]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_advise_mr(pd : *mut super::verbs::ibv_pd, advice : super::ib_user_ioctl_verbs::ib_uverbs_advise_mr_advice, flags : u32, sg_list : *mut super::verbs::ibv_sge, num_sge : u32) -> i32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_alloc_dm(context : *mut super::verbs::ibv_context, attr : *mut super::verbs::ibv_alloc_dm_attr) -> *mut super::verbs::ibv_dm);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_alloc_mw(pd : *mut super::verbs::ibv_pd, r#type : super::verbs::ibv_mw_type) -> *mut super::verbs::ibv_mw);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_alloc_null_mr(pd : *mut super::verbs::ibv_pd) -> *mut super::verbs::ibv_mr);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_alloc_parent_domain(context : *mut super::verbs::ibv_context, attr : *mut super::verbs::ibv_parent_domain_init_attr) -> *mut super::verbs::ibv_pd);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_alloc_td(context : *mut super::verbs::ibv_context, init_attr : *mut super::verbs::ibv_td_init_attr) -> *mut super::verbs::ibv_td);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_attach_counters_point_flow(counters : *mut super::verbs::ibv_counters, attr : *mut super::verbs::ibv_counter_attach_attr, flow : *mut super::verbs::ibv_flow) -> i32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_bind_mw(qp : *mut super::verbs::ibv_qp, mw : *mut super::verbs::ibv_mw, mw_bind : *mut super::verbs::ibv_mw_bind) -> i32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_close_xrcd(xrcd : *mut super::verbs::ibv_xrcd) -> i32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_cq_ex_to_cq(cq : *mut super::verbs::ibv_cq_ex) -> *mut super::verbs::ibv_cq);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_create_counters(context : *mut super::verbs::ibv_context, init_attr : *mut super::verbs::ibv_counters_init_attr) -> *mut super::verbs::ibv_counters);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_create_cq_ex(context : *mut super::verbs::ibv_context, cq_attr : *mut super::verbs::ibv_cq_init_attr_ex) -> *mut super::verbs::ibv_cq_ex);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_create_flow(qp : *mut super::verbs::ibv_qp, flow : *mut super::verbs::ibv_flow_attr) -> *mut super::verbs::ibv_flow);
#[cfg(all(feature = "ib_user_ioctl_verbs", feature = "verbs"))]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_create_flow_action_esp(ctx : *mut super::verbs::ibv_context, esp : *mut super::verbs::ibv_flow_action_esp_attr) -> *mut super::verbs::ibv_flow_action);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_create_qp_ex(context : *mut super::verbs::ibv_context, qp_init_attr_ex : *mut super::verbs::ibv_qp_init_attr_ex) -> *mut super::verbs::ibv_qp);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_create_rwq_ind_table(context : *mut super::verbs::ibv_context, init_attr : *mut super::verbs::ibv_rwq_ind_table_init_attr) -> *mut super::verbs::ibv_rwq_ind_table);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_create_srq_ex(context : *mut super::verbs::ibv_context, srq_init_attr_ex : *mut super::verbs::ibv_srq_init_attr_ex) -> *mut super::verbs::ibv_srq);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_create_wq(context : *mut super::verbs::ibv_context, wq_init_attr : *mut super::verbs::ibv_wq_init_attr) -> *mut super::verbs::ibv_wq);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_dealloc_mw(mw : *mut super::verbs::ibv_mw) -> i32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_dealloc_td(td : *mut super::verbs::ibv_td) -> i32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_destroy_counters(counters : *mut super::verbs::ibv_counters) -> i32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_destroy_flow(flow_id : *mut super::verbs::ibv_flow) -> i32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_destroy_flow_action(action : *mut super::verbs::ibv_flow_action) -> i32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_destroy_rwq_ind_table(rwq_ind_table : *mut super::verbs::ibv_rwq_ind_table) -> i32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_destroy_wq(wq : *mut super::verbs::ibv_wq) -> i32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_end_poll(cq : *mut super::verbs::ibv_cq_ex));
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_flow_label_to_udp_sport(fl : u32) -> u16);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_free_dm(dm : *mut super::verbs::ibv_dm) -> i32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_get_srq_num(srq : *mut super::verbs::ibv_srq, srq_num : *mut u32) -> i32);
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_inc_rkey(rkey : u32) -> u32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_is_qpt_supported(caps : u32, qpt : super::verbs::ibv_qp_type) -> i32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_memcpy_from_dm(host_addr : *mut core::ffi::c_void, dm : *mut super::verbs::ibv_dm, dm_offset : u64, length : usize) -> i32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_memcpy_to_dm(dm : *mut super::verbs::ibv_dm, dm_offset : u64, host_addr : *const core::ffi::c_void, length : usize) -> i32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_modify_cq(cq : *mut super::verbs::ibv_cq, attr : *mut super::verbs::ibv_modify_cq_attr) -> i32);
#[cfg(all(feature = "ib_user_ioctl_verbs", feature = "verbs"))]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_modify_flow_action_esp(action : *mut super::verbs::ibv_flow_action, esp : *mut super::verbs::ibv_flow_action_esp_attr) -> i32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_modify_qp_rate_limit(qp : *mut super::verbs::ibv_qp, attr : *mut super::verbs::ibv_qp_rate_limit_attr) -> i32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_modify_wq(wq : *mut super::verbs::ibv_wq, wq_attr : *mut super::verbs::ibv_wq_attr) -> i32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_next_poll(cq : *mut super::verbs::ibv_cq_ex) -> i32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_open_qp(context : *mut super::verbs::ibv_context, qp_open_attr : *mut super::verbs::ibv_qp_open_attr) -> *mut super::verbs::ibv_qp);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_open_xrcd(context : *mut super::verbs::ibv_context, xrcd_init_attr : *mut super::verbs::ibv_xrcd_init_attr) -> *mut super::verbs::ibv_xrcd);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_poll_cq(cq : *mut super::verbs::ibv_cq, num_entries : i32, wc : *mut super::verbs::ibv_wc) -> i32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_post_recv(qp : *mut super::verbs::ibv_qp, wr : *mut super::verbs::ibv_recv_wr, bad_wr : *mut *mut super::verbs::ibv_recv_wr) -> i32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_post_send(qp : *mut super::verbs::ibv_qp, wr : *mut super::verbs::ibv_send_wr, bad_wr : *mut *mut super::verbs::ibv_send_wr) -> i32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_post_srq_ops(srq : *mut super::verbs::ibv_srq, op : *mut super::verbs::ibv_ops_wr, bad_op : *mut *mut super::verbs::ibv_ops_wr) -> i32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_post_srq_recv(srq : *mut super::verbs::ibv_srq, recv_wr : *mut super::verbs::ibv_recv_wr, bad_recv_wr : *mut *mut super::verbs::ibv_recv_wr) -> i32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_post_wq_recv(wq : *mut super::verbs::ibv_wq, recv_wr : *mut super::verbs::ibv_recv_wr, bad_recv_wr : *mut *mut super::verbs::ibv_recv_wr) -> i32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_query_device_ex(context : *mut super::verbs::ibv_context, input : *const super::verbs::ibv_query_device_ex_input, attr : *mut super::verbs::ibv_device_attr_ex) -> i32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_query_gid_ex(context : *mut super::verbs::ibv_context, port_num : u32, gid_index : u32, entry : *mut super::verbs::ibv_gid_entry, flags : u32) -> i32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_query_gid_table(context : *mut super::verbs::ibv_context, entries : *mut super::verbs::ibv_gid_entry, max_entries : usize, flags : u32) -> bnd_linux::libc::types::ssize_t);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_query_rt_values_ex(context : *mut super::verbs::ibv_context, values : *mut super::verbs::ibv_values_ex) -> i32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_read_counters(counters : *mut super::verbs::ibv_counters, counters_value : *mut u64, ncounters : u32, flags : u32) -> i32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_reg_dm_mr(pd : *mut super::verbs::ibv_pd, dm : *mut super::verbs::ibv_dm, dm_offset : u64, length : usize, access : u32) -> *mut super::verbs::ibv_mr);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_req_notify_cq(cq : *mut super::verbs::ibv_cq, solicited_only : i32) -> i32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_start_poll(cq : *mut super::verbs::ibv_cq_ex, attr : *mut super::verbs::ibv_poll_cq_attr) -> i32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_wc_read_byte_len(cq : *mut super::verbs::ibv_cq_ex) -> u32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_wc_read_completion_ts(cq : *mut super::verbs::ibv_cq_ex) -> u64);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_wc_read_completion_wallclock_ns(cq : *mut super::verbs::ibv_cq_ex) -> u64);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_wc_read_cvlan(cq : *mut super::verbs::ibv_cq_ex) -> u16);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_wc_read_dlid_path_bits(cq : *mut super::verbs::ibv_cq_ex) -> u8);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_wc_read_flow_tag(cq : *mut super::verbs::ibv_cq_ex) -> u32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_wc_read_imm_data(cq : *mut super::verbs::ibv_cq_ex) -> bnd_linux::libc::types::__be32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_wc_read_invalidated_rkey(cq : *mut super::verbs::ibv_cq_ex) -> u32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_wc_read_opcode(cq : *mut super::verbs::ibv_cq_ex) -> super::verbs::ibv_wc_opcode);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_wc_read_qp_num(cq : *mut super::verbs::ibv_cq_ex) -> u32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_wc_read_sl(cq : *mut super::verbs::ibv_cq_ex) -> u8);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_wc_read_slid(cq : *mut super::verbs::ibv_cq_ex) -> u32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_wc_read_src_qp(cq : *mut super::verbs::ibv_cq_ex) -> u32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_wc_read_tm_info(cq : *mut super::verbs::ibv_cq_ex, tm_info : *mut super::verbs::ibv_wc_tm_info));
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_wc_read_vendor_err(cq : *mut super::verbs::ibv_cq_ex) -> u32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_wc_read_wc_flags(cq : *mut super::verbs::ibv_cq_ex) -> u32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_wr_abort(qp : *mut super::verbs::ibv_qp_ex));
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_wr_atomic_cmp_swp(qp : *mut super::verbs::ibv_qp_ex, rkey : u32, remote_addr : u64, compare : u64, swap : u64));
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_wr_atomic_fetch_add(qp : *mut super::verbs::ibv_qp_ex, rkey : u32, remote_addr : u64, add : u64));
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_wr_atomic_write(qp : *mut super::verbs::ibv_qp_ex, rkey : u32, remote_addr : u64, atomic_wr : *const core::ffi::c_void));
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_wr_bind_mw(qp : *mut super::verbs::ibv_qp_ex, mw : *mut super::verbs::ibv_mw, rkey : u32, bind_info : *const super::verbs::ibv_mw_bind_info));
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_wr_complete(qp : *mut super::verbs::ibv_qp_ex) -> i32);
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_wr_flush(qp : *mut super::verbs::ibv_qp_ex, rkey : u32, remote_addr : u64, len : usize, r#type : u8, level : u8));
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_wr_local_inv(qp : *mut super::verbs::ibv_qp_ex, invalidate_rkey : u32));
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_wr_rdma_read(qp : *mut super::verbs::ibv_qp_ex, rkey : u32, remote_addr : u64));
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_wr_rdma_write(qp : *mut super::verbs::ibv_qp_ex, rkey : u32, remote_addr : u64));
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_wr_rdma_write_imm(qp : *mut super::verbs::ibv_qp_ex, rkey : u32, remote_addr : u64, imm_data : bnd_linux::libc::types::__be32));
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_wr_send(qp : *mut super::verbs::ibv_qp_ex));
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_wr_send_imm(qp : *mut super::verbs::ibv_qp_ex, imm_data : bnd_linux::libc::types::__be32));
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_wr_send_inv(qp : *mut super::verbs::ibv_qp_ex, invalidate_rkey : u32));
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_wr_send_tso(qp : *mut super::verbs::ibv_qp_ex, hdr : *mut core::ffi::c_void, hdr_sz : u16, mss : u16));
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_wr_set_inline_data(qp : *mut super::verbs::ibv_qp_ex, addr : *mut core::ffi::c_void, length : usize));
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_wr_set_inline_data_list(qp : *mut super::verbs::ibv_qp_ex, num_buf : usize, buf_list : *const super::verbs::ibv_data_buf));
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_wr_set_sge(qp : *mut super::verbs::ibv_qp_ex, lkey : u32, addr : u64, length : u32));
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_wr_set_sge_list(qp : *mut super::verbs::ibv_qp_ex, num_sge : usize, sg_list : *const super::verbs::ibv_sge));
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_wr_set_ud_addr(qp : *mut super::verbs::ibv_qp_ex, ah : *mut super::verbs::ibv_ah, remote_qpn : u32, remote_qkey : u32));
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_wr_set_xrc_srqn(qp : *mut super::verbs::ibv_qp_ex, remote_srqn : u32));
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_ibv_wr_start(qp : *mut super::verbs::ibv_qp_ex));
#[cfg(feature = "verbs")]
windows_link::link!("rdma_wrapper" "C" fn rdma_wrap_verbs_get_ctx(ctx : *mut super::verbs::ibv_context) -> *mut super::verbs::verbs_context);
