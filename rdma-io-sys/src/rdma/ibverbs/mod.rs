windows_link::link!("ibverbs" "C" fn _ibv_query_gid_ex(context : *mut ibv_context, port_num : u32, gid_index : u32, entry : *mut ibv_gid_entry, flags : u32, entry_size : usize) -> i32);
windows_link::link!("ibverbs" "C" fn _ibv_query_gid_table(context : *mut ibv_context, entries : *mut ibv_gid_entry, max_entries : usize, flags : u32, entry_size : usize) -> bnd_linux::libc::types::ssize_t);
windows_link::link!("ibverbs" "C" fn ibv_ack_async_event(event : *mut ibv_async_event));
windows_link::link!("ibverbs" "C" fn ibv_ack_cq_events(cq : *mut ibv_cq, nevents : u32));
windows_link::link!("ibverbs" "C" fn ibv_alloc_dmah(context : *mut ibv_context, attr : *mut ibv_dmah_init_attr) -> *mut ibv_dmah);
windows_link::link!("ibverbs" "C" fn ibv_alloc_pd(context : *mut ibv_context) -> *mut ibv_pd);
windows_link::link!("ibverbs" "C" fn ibv_attach_mcast(qp : *mut ibv_qp, gid : *const ibv_gid, lid : u16) -> i32);
windows_link::link!("ibverbs" "C" fn ibv_close_device(context : *mut ibv_context) -> i32);
windows_link::link!("ibverbs" "C" fn ibv_create_ah(pd : *mut ibv_pd, attr : *mut ibv_ah_attr) -> *mut ibv_ah);
windows_link::link!("ibverbs" "C" fn ibv_create_ah_from_wc(pd : *mut ibv_pd, wc : *mut ibv_wc, grh : *mut ibv_grh, port_num : u8) -> *mut ibv_ah);
windows_link::link!("ibverbs" "C" fn ibv_create_comp_channel(context : *mut ibv_context) -> *mut ibv_comp_channel);
windows_link::link!("ibverbs" "C" fn ibv_create_cq(context : *mut ibv_context, cqe : i32, cq_context : *mut core::ffi::c_void, channel : *mut ibv_comp_channel, comp_vector : i32) -> *mut ibv_cq);
windows_link::link!("ibverbs" "C" fn ibv_create_qp(pd : *mut ibv_pd, qp_init_attr : *mut ibv_qp_init_attr) -> *mut ibv_qp);
windows_link::link!("ibverbs" "C" fn ibv_create_srq(pd : *mut ibv_pd, srq_init_attr : *mut ibv_srq_init_attr) -> *mut ibv_srq);
windows_link::link!("ibverbs" "C" fn ibv_dealloc_dmah(dmah : *mut ibv_dmah) -> i32);
windows_link::link!("ibverbs" "C" fn ibv_dealloc_pd(pd : *mut ibv_pd) -> i32);
windows_link::link!("ibverbs" "C" fn ibv_dereg_mr(mr : *mut ibv_mr) -> i32);
windows_link::link!("ibverbs" "C" fn ibv_destroy_ah(ah : *mut ibv_ah) -> i32);
windows_link::link!("ibverbs" "C" fn ibv_destroy_comp_channel(channel : *mut ibv_comp_channel) -> i32);
windows_link::link!("ibverbs" "C" fn ibv_destroy_cq(cq : *mut ibv_cq) -> i32);
windows_link::link!("ibverbs" "C" fn ibv_destroy_qp(qp : *mut ibv_qp) -> i32);
windows_link::link!("ibverbs" "C" fn ibv_destroy_srq(srq : *mut ibv_srq) -> i32);
windows_link::link!("ibverbs" "C" fn ibv_detach_mcast(qp : *mut ibv_qp, gid : *const ibv_gid, lid : u16) -> i32);
windows_link::link!("ibverbs" "C" fn ibv_event_type_str(event : ibv_event_type) -> *const i8);
windows_link::link!("ibverbs" "C" fn ibv_fork_init() -> i32);
windows_link::link!("ibverbs" "C" fn ibv_free_device_list(list : *mut *mut ibv_device));
windows_link::link!("ibverbs" "C" fn ibv_get_async_event(context : *mut ibv_context, event : *mut ibv_async_event) -> i32);
windows_link::link!("ibverbs" "C" fn ibv_get_cq_event(channel : *mut ibv_comp_channel, cq : *mut *mut ibv_cq, cq_context : *mut *mut core::ffi::c_void) -> i32);
windows_link::link!("ibverbs" "C" fn ibv_get_device_guid(device : *mut ibv_device) -> bnd_linux::libc::types::__be64);
windows_link::link!("ibverbs" "C" fn ibv_get_device_index(device : *mut ibv_device) -> i32);
windows_link::link!("ibverbs" "C" fn ibv_get_device_list(num_devices : *mut i32) -> *mut *mut ibv_device);
windows_link::link!("ibverbs" "C" fn ibv_get_device_name(device : *mut ibv_device) -> *const i8);
windows_link::link!("ibverbs" "C" fn ibv_get_pkey_index(context : *mut ibv_context, port_num : u8, pkey : bnd_linux::libc::types::__be16) -> i32);
windows_link::link!("ibverbs" "C" fn ibv_import_device(cmd_fd : i32) -> *mut ibv_context);
windows_link::link!("ibverbs" "C" fn ibv_import_dm(context : *mut ibv_context, dm_handle : u32) -> *mut ibv_dm);
windows_link::link!("ibverbs" "C" fn ibv_import_mr(pd : *mut ibv_pd, mr_handle : u32) -> *mut ibv_mr);
windows_link::link!("ibverbs" "C" fn ibv_import_pd(context : *mut ibv_context, pd_handle : u32) -> *mut ibv_pd);
windows_link::link!("ibverbs" "C" fn ibv_init_ah_from_wc(context : *mut ibv_context, port_num : u8, wc : *mut ibv_wc, grh : *mut ibv_grh, ah_attr : *mut ibv_ah_attr) -> i32);
windows_link::link!("ibverbs" "C" fn ibv_is_fork_initialized() -> ibv_fork_status);
windows_link::link!("ibverbs" "C" fn ibv_modify_qp(qp : *mut ibv_qp, attr : *mut ibv_qp_attr, attr_mask : i32) -> i32);
windows_link::link!("ibverbs" "C" fn ibv_modify_srq(srq : *mut ibv_srq, srq_attr : *mut ibv_srq_attr, srq_attr_mask : i32) -> i32);
windows_link::link!("ibverbs" "C" fn ibv_node_type_str(node_type : ibv_node_type) -> *const i8);
windows_link::link!("ibverbs" "C" fn ibv_open_device(device : *mut ibv_device) -> *mut ibv_context);
windows_link::link!("ibverbs" "C" fn ibv_port_state_str(port_state : ibv_port_state) -> *const i8);
windows_link::link!("ibverbs" "C" fn ibv_qp_to_qp_ex(qp : *mut ibv_qp) -> *mut ibv_qp_ex);
windows_link::link!("ibverbs" "C" fn ibv_query_device(context : *mut ibv_context, device_attr : *mut ibv_device_attr) -> i32);
windows_link::link!("ibverbs" "C" fn ibv_query_ece(qp : *mut ibv_qp, ece : *mut ibv_ece) -> i32);
windows_link::link!("ibverbs" "C" fn ibv_query_gid(context : *mut ibv_context, port_num : u8, index : i32, gid : *mut ibv_gid) -> i32);
windows_link::link!("ibverbs" "C" fn ibv_query_pkey(context : *mut ibv_context, port_num : u8, index : i32, pkey : *mut bnd_linux::libc::types::__be16) -> i32);
windows_link::link!("ibverbs" "C" fn ibv_query_port(context : *mut ibv_context, port_num : u8, port_attr : *mut _compat_ibv_port_attr) -> i32);
windows_link::link!("ibverbs" "C" fn ibv_query_qp(qp : *mut ibv_qp, attr : *mut ibv_qp_attr, attr_mask : i32, init_attr : *mut ibv_qp_init_attr) -> i32);
windows_link::link!("ibverbs" "C" fn ibv_query_qp_data_in_order(qp : *mut ibv_qp, op : ibv_wr_opcode, flags : u32) -> i32);
windows_link::link!("ibverbs" "C" fn ibv_query_srq(srq : *mut ibv_srq, srq_attr : *mut ibv_srq_attr) -> i32);
windows_link::link!("ibverbs" "C" fn ibv_rate_to_mbps(rate : ibv_rate) -> i32);
windows_link::link!("ibverbs" "C" fn ibv_rate_to_mult(rate : ibv_rate) -> i32);
windows_link::link!("ibverbs" "C" fn ibv_reg_dmabuf_mr(pd : *mut ibv_pd, offset : u64, length : usize, iova : u64, fd : i32, access : i32) -> *mut ibv_mr);
windows_link::link!("ibverbs" "C" fn ibv_reg_mr(pd : *mut ibv_pd, addr : *mut core::ffi::c_void, length : usize, access : i32) -> *mut ibv_mr);
windows_link::link!("ibverbs" "C" fn ibv_reg_mr_ex(pd : *mut ibv_pd, mr_init_attr : *mut ibv_mr_init_attr) -> *mut ibv_mr);
windows_link::link!("ibverbs" "C" fn ibv_reg_mr_iova(pd : *mut ibv_pd, addr : *mut core::ffi::c_void, length : usize, iova : u64, access : i32) -> *mut ibv_mr);
windows_link::link!("ibverbs" "C" fn ibv_reg_mr_iova2(pd : *mut ibv_pd, addr : *mut core::ffi::c_void, length : usize, iova : u64, access : u32) -> *mut ibv_mr);
windows_link::link!("ibverbs" "C" fn ibv_rereg_mr(mr : *mut ibv_mr, flags : i32, pd : *mut ibv_pd, addr : *mut core::ffi::c_void, length : usize, access : i32) -> i32);
windows_link::link!("ibverbs" "C" fn ibv_resize_cq(cq : *mut ibv_cq, cqe : i32) -> i32);
windows_link::link!("ibverbs" "C" fn ibv_resolve_eth_l2_from_gid(context : *mut ibv_context, attr : *mut ibv_ah_attr, eth_mac : *mut u8, vid : *mut u16) -> i32);
windows_link::link!("ibverbs" "C" fn ibv_set_ece(qp : *mut ibv_qp, ece : *mut ibv_ece) -> i32);
windows_link::link!("ibverbs" "C" fn ibv_unimport_dm(dm : *mut ibv_dm));
windows_link::link!("ibverbs" "C" fn ibv_unimport_mr(mr : *mut ibv_mr));
windows_link::link!("ibverbs" "C" fn ibv_unimport_pd(pd : *mut ibv_pd));
windows_link::link!("ibverbs" "C" fn ibv_wc_status_str(status : ibv_wc_status) -> *const i8);
windows_link::link!("ibverbs" "C" fn ibv_wr_opcode_str(opcode : ibv_wr_opcode) -> *const i8);
windows_link::link!("ibverbs" "C" fn mbps_to_ibv_rate(mbps : i32) -> ibv_rate);
windows_link::link!("ibverbs" "C" fn mult_to_ibv_rate(mult : i32) -> ibv_rate);
pub const ETHERNET_LL_SIZE: i32 = 6;
pub const IBV_ACCESS_FLUSH_GLOBAL: ibv_access_flags = 256;
pub const IBV_ACCESS_FLUSH_PERSISTENT: ibv_access_flags = 512;
pub const IBV_ACCESS_HUGETLB: ibv_access_flags = 128;
pub const IBV_ACCESS_LOCAL_WRITE: ibv_access_flags = 1;
pub const IBV_ACCESS_MW_BIND: ibv_access_flags = 16;
pub const IBV_ACCESS_ON_DEMAND: ibv_access_flags = 64;
pub const IBV_ACCESS_OPTIONAL_FIRST: i32 = 1048576;
pub const IBV_ACCESS_OPTIONAL_RANGE: i32 = 1072693248;
pub const IBV_ACCESS_RELAXED_ORDERING: ibv_access_flags = 1048576;
pub const IBV_ACCESS_REMOTE_ATOMIC: ibv_access_flags = 8;
pub const IBV_ACCESS_REMOTE_READ: ibv_access_flags = 4;
pub const IBV_ACCESS_REMOTE_WRITE: ibv_access_flags = 2;
pub const IBV_ACCESS_ZERO_BASED: ibv_access_flags = 32;
pub const IBV_ADVISE_MR_ADVICE_PREFETCH: i32 = 0;
pub const IBV_ADVISE_MR_ADVICE_PREFETCH_NO_FAULT: i32 = 2;
pub const IBV_ADVISE_MR_ADVICE_PREFETCH_WRITE: i32 = 1;
pub const IBV_ADVISE_MR_FLAG_FLUSH: i32 = 1;
pub const IBV_ATOMIC_GLOB: ibv_atomic_cap = 2;
pub const IBV_ATOMIC_HCA: ibv_atomic_cap = 1;
pub const IBV_ATOMIC_NONE: ibv_atomic_cap = 0;
pub const IBV_COUNTER_BYTES: ibv_counter_description = 1;
pub const IBV_COUNTER_PACKETS: ibv_counter_description = 0;
pub const IBV_CQ_ATTR_MODERATE: ibv_cq_attr_mask = 1;
pub const IBV_CQ_ATTR_RESERVED: ibv_cq_attr_mask = 2;
pub const IBV_CQ_INIT_ATTR_MASK_FLAGS: ibv_cq_init_attr_mask = 1;
pub const IBV_CQ_INIT_ATTR_MASK_PD: ibv_cq_init_attr_mask = 2;
pub const IBV_CREATE_CQ_ATTR_IGNORE_OVERRUN: ibv_create_cq_attr_flags = 2;
pub const IBV_CREATE_CQ_ATTR_SINGLE_THREADED: ibv_create_cq_attr_flags = 1;
pub const IBV_CREATE_CQ_SUP_WC_FLAGS: u32 = 4095;
pub const IBV_CREATE_IND_TABLE_RESERVED: ibv_ind_table_init_attr_mask = 1;
pub const IBV_DEVICE_AUTO_PATH_MIG: ibv_device_cap_flags = 16;
pub const IBV_DEVICE_BAD_PKEY_CNTR: ibv_device_cap_flags = 2;
pub const IBV_DEVICE_BAD_QKEY_CNTR: ibv_device_cap_flags = 4;
pub const IBV_DEVICE_CHANGE_PHY_PORT: ibv_device_cap_flags = 32;
pub const IBV_DEVICE_CURR_QP_STATE_MOD: ibv_device_cap_flags = 128;
pub const IBV_DEVICE_INIT_TYPE: ibv_device_cap_flags = 512;
pub const IBV_DEVICE_MANAGED_FLOW_STEERING: ibv_device_cap_flags = 536870912;
pub const IBV_DEVICE_MEM_MGT_EXTENSIONS: ibv_device_cap_flags = 2097152;
pub const IBV_DEVICE_MEM_WINDOW: ibv_device_cap_flags = 131072;
pub const IBV_DEVICE_MEM_WINDOW_TYPE_2A: ibv_device_cap_flags = 8388608;
pub const IBV_DEVICE_MEM_WINDOW_TYPE_2B: ibv_device_cap_flags = 16777216;
pub const IBV_DEVICE_N_NOTIFY_CQ: ibv_device_cap_flags = 16384;
pub const IBV_DEVICE_PCI_WRITE_END_PADDING: u64 = 68719476736;
pub const IBV_DEVICE_PORT_ACTIVE_EVENT: ibv_device_cap_flags = 1024;
pub const IBV_DEVICE_RAW_IP_CSUM: ibv_device_cap_flags = 67108864;
pub const IBV_DEVICE_RAW_MULTI: ibv_device_cap_flags = 8;
pub const IBV_DEVICE_RAW_SCATTER_FCS: u64 = 17179869184;
pub const IBV_DEVICE_RC_IP_CSUM: ibv_device_cap_flags = 33554432;
pub const IBV_DEVICE_RC_RNR_NAK_GEN: ibv_device_cap_flags = 4096;
pub const IBV_DEVICE_RESIZE_MAX_WR: ibv_device_cap_flags = 1;
pub const IBV_DEVICE_SHUTDOWN_PORT: ibv_device_cap_flags = 256;
pub const IBV_DEVICE_SRQ_RESIZE: ibv_device_cap_flags = 8192;
pub const IBV_DEVICE_SYS_IMAGE_GUID: ibv_device_cap_flags = 2048;
pub const IBV_DEVICE_UD_AV_PORT_ENFORCE: ibv_device_cap_flags = 64;
pub const IBV_DEVICE_UD_IP_CSUM: ibv_device_cap_flags = 262144;
pub const IBV_DEVICE_XRC: ibv_device_cap_flags = 1048576;
pub const IBV_DMAH_INIT_ATTR_MASK_CPU_ID: ibv_dmah_init_attr_mask = 1;
pub const IBV_DMAH_INIT_ATTR_MASK_PH: ibv_dmah_init_attr_mask = 2;
pub const IBV_DMAH_INIT_ATTR_MASK_TPH_MEM_TYPE: ibv_dmah_init_attr_mask = 4;
pub const IBV_DM_MASK_HANDLE: ibv_dm_mask = 1;
pub const IBV_EVENT_CLIENT_REREGISTER: ibv_event_type = 17;
pub const IBV_EVENT_COMM_EST: ibv_event_type = 4;
pub const IBV_EVENT_CQ_ERR: ibv_event_type = 0;
pub const IBV_EVENT_DEVICE_FATAL: ibv_event_type = 8;
pub const IBV_EVENT_GID_CHANGE: ibv_event_type = 18;
pub const IBV_EVENT_LID_CHANGE: ibv_event_type = 11;
pub const IBV_EVENT_PATH_MIG: ibv_event_type = 6;
pub const IBV_EVENT_PATH_MIG_ERR: ibv_event_type = 7;
pub const IBV_EVENT_PKEY_CHANGE: ibv_event_type = 12;
pub const IBV_EVENT_PORT_ACTIVE: ibv_event_type = 9;
pub const IBV_EVENT_PORT_ERR: ibv_event_type = 10;
pub const IBV_EVENT_QP_ACCESS_ERR: ibv_event_type = 3;
pub const IBV_EVENT_QP_FATAL: ibv_event_type = 1;
pub const IBV_EVENT_QP_LAST_WQE_REACHED: ibv_event_type = 16;
pub const IBV_EVENT_QP_REQ_ERR: ibv_event_type = 2;
pub const IBV_EVENT_SM_CHANGE: ibv_event_type = 13;
pub const IBV_EVENT_SQ_DRAINED: ibv_event_type = 5;
pub const IBV_EVENT_SRQ_ERR: ibv_event_type = 14;
pub const IBV_EVENT_SRQ_LIMIT_REACHED: ibv_event_type = 15;
pub const IBV_EVENT_WQ_FATAL: ibv_event_type = 19;
pub const IBV_FLOW_ACTION_ESP_FLAGS_DECRYPT: i32 = 0;
pub const IBV_FLOW_ACTION_ESP_FLAGS_ENCRYPT: i32 = 4;
pub const IBV_FLOW_ACTION_ESP_FLAGS_ESN_NEW_WINDOW: i32 = 8;
pub const IBV_FLOW_ACTION_ESP_FLAGS_FULL_OFFLOAD: i32 = 1;
pub const IBV_FLOW_ACTION_ESP_FLAGS_INLINE_CRYPTO: i32 = 0;
pub const IBV_FLOW_ACTION_ESP_FLAGS_TRANSPORT: i32 = 2;
pub const IBV_FLOW_ACTION_ESP_FLAGS_TUNNEL: i32 = 0;
pub const IBV_FLOW_ACTION_ESP_KEYMAT_AES_GCM: i32 = 0;
pub const IBV_FLOW_ACTION_ESP_MASK_ESN: ibv_flow_action_esp_mask = 1;
pub const IBV_FLOW_ACTION_ESP_REPLAY_BMP: i32 = 1;
pub const IBV_FLOW_ACTION_ESP_REPLAY_NONE: i32 = 0;
pub const IBV_FLOW_ACTION_IV_ALGO_SEQ: i32 = 0;
pub const IBV_FLOW_ATTR_ALL_DEFAULT: ibv_flow_attr_type = 1;
pub const IBV_FLOW_ATTR_FLAGS_DONT_TRAP: ibv_flow_flags = 2;
pub const IBV_FLOW_ATTR_FLAGS_EGRESS: ibv_flow_flags = 4;
pub const IBV_FLOW_ATTR_MC_DEFAULT: ibv_flow_attr_type = 2;
pub const IBV_FLOW_ATTR_NORMAL: ibv_flow_attr_type = 0;
pub const IBV_FLOW_ATTR_SNIFFER: ibv_flow_attr_type = 3;
pub const IBV_FLOW_SPEC_ACTION_COUNT: ibv_flow_spec_type = 4099;
pub const IBV_FLOW_SPEC_ACTION_DROP: ibv_flow_spec_type = 4097;
pub const IBV_FLOW_SPEC_ACTION_HANDLE: ibv_flow_spec_type = 4098;
pub const IBV_FLOW_SPEC_ACTION_TAG: ibv_flow_spec_type = 4096;
pub const IBV_FLOW_SPEC_ESP: ibv_flow_spec_type = 52;
pub const IBV_FLOW_SPEC_ETH: ibv_flow_spec_type = 32;
pub const IBV_FLOW_SPEC_GRE: ibv_flow_spec_type = 81;
pub const IBV_FLOW_SPEC_INNER: ibv_flow_spec_type = 256;
pub const IBV_FLOW_SPEC_IPV4: ibv_flow_spec_type = 48;
pub const IBV_FLOW_SPEC_IPV4_EXT: ibv_flow_spec_type = 50;
pub const IBV_FLOW_SPEC_IPV6: ibv_flow_spec_type = 49;
pub const IBV_FLOW_SPEC_MPLS: ibv_flow_spec_type = 96;
pub const IBV_FLOW_SPEC_TCP: ibv_flow_spec_type = 64;
pub const IBV_FLOW_SPEC_UDP: ibv_flow_spec_type = 65;
pub const IBV_FLOW_SPEC_VXLAN_TUNNEL: ibv_flow_spec_type = 80;
pub const IBV_FLUSH_GLOBAL: ibv_placement_type = 1;
pub const IBV_FLUSH_MR: ibv_selectivity_level = 1;
pub const IBV_FLUSH_PERSISTENT: ibv_placement_type = 2;
pub const IBV_FLUSH_RANGE: ibv_selectivity_level = 0;
pub const IBV_FORK_DISABLED: ibv_fork_status = 0;
pub const IBV_FORK_ENABLED: ibv_fork_status = 1;
pub const IBV_FORK_UNNEEDED: ibv_fork_status = 2;
pub const IBV_GID_TYPE_IB: ibv_gid_type = 0;
pub const IBV_GID_TYPE_ROCE_V1: ibv_gid_type = 1;
pub const IBV_GID_TYPE_ROCE_V2: ibv_gid_type = 2;
pub const IBV_LINK_LAYER_ETHERNET: u32 = 2;
pub const IBV_LINK_LAYER_INFINIBAND: u32 = 1;
pub const IBV_LINK_LAYER_UNSPECIFIED: u32 = 0;
pub const IBV_MIG_ARMED: ibv_mig_state = 2;
pub const IBV_MIG_MIGRATED: ibv_mig_state = 0;
pub const IBV_MIG_REARM: ibv_mig_state = 1;
pub const IBV_MTU_1024: ibv_mtu = 3;
pub const IBV_MTU_2048: ibv_mtu = 4;
pub const IBV_MTU_256: ibv_mtu = 1;
pub const IBV_MTU_4096: ibv_mtu = 5;
pub const IBV_MTU_512: ibv_mtu = 2;
pub const IBV_MW_TYPE_1: ibv_mw_type = 1;
pub const IBV_MW_TYPE_2: ibv_mw_type = 2;
pub const IBV_NODE_CA: ibv_node_type = 1;
pub const IBV_NODE_RNIC: ibv_node_type = 4;
pub const IBV_NODE_ROUTER: ibv_node_type = 3;
pub const IBV_NODE_SWITCH: ibv_node_type = 2;
pub const IBV_NODE_UNKNOWN: ibv_node_type = -1;
pub const IBV_NODE_UNSPECIFIED: ibv_node_type = 7;
pub const IBV_NODE_USNIC: ibv_node_type = 5;
pub const IBV_NODE_USNIC_UDP: ibv_node_type = 6;
pub const IBV_ODP_SUPPORT: ibv_odp_general_caps = 1;
pub const IBV_ODP_SUPPORT_ATOMIC: ibv_odp_transport_cap_bits = 16;
pub const IBV_ODP_SUPPORT_ATOMIC_WRITE: ibv_odp_transport_cap_bits = 128;
pub const IBV_ODP_SUPPORT_FLUSH: ibv_odp_transport_cap_bits = 64;
pub const IBV_ODP_SUPPORT_IMPLICIT: ibv_odp_general_caps = 2;
pub const IBV_ODP_SUPPORT_READ: ibv_odp_transport_cap_bits = 8;
pub const IBV_ODP_SUPPORT_RECV: ibv_odp_transport_cap_bits = 2;
pub const IBV_ODP_SUPPORT_SEND: ibv_odp_transport_cap_bits = 1;
pub const IBV_ODP_SUPPORT_SRQ_RECV: ibv_odp_transport_cap_bits = 32;
pub const IBV_ODP_SUPPORT_WRITE: ibv_odp_transport_cap_bits = 4;
pub const IBV_OPS_SIGNALED: ibv_ops_flags = 1;
pub const IBV_OPS_TM_SYNC: ibv_ops_flags = 2;
pub const IBV_PARENT_DOMAIN_INIT_ATTR_ALLOCATORS: ibv_parent_domain_init_attr_mask = 1;
pub const IBV_PARENT_DOMAIN_INIT_ATTR_PD_CONTEXT: ibv_parent_domain_init_attr_mask = 2;
pub const IBV_PCI_ATOMIC_OPERATION_16_BYTE_SIZE_SUP: ibv_pci_atomic_op_size = 4;
pub const IBV_PCI_ATOMIC_OPERATION_4_BYTE_SIZE_SUP: ibv_pci_atomic_op_size = 1;
pub const IBV_PCI_ATOMIC_OPERATION_8_BYTE_SIZE_SUP: ibv_pci_atomic_op_size = 2;
pub const IBV_PORT_ACTIVE: ibv_port_state = 4;
pub const IBV_PORT_ACTIVE_DEFER: ibv_port_state = 5;
pub const IBV_PORT_ARMED: ibv_port_state = 3;
pub const IBV_PORT_AUTO_MIGR_SUP: ibv_port_cap_flags = 32;
pub const IBV_PORT_BOOT_MGMT_SUP: ibv_port_cap_flags = 8388608;
pub const IBV_PORT_CAP_MASK2_SUP: ibv_port_cap_flags = 32768;
pub const IBV_PORT_CAP_MASK_NOTICE_SUP: ibv_port_cap_flags = 4194304;
pub const IBV_PORT_CLIENT_REG_SUP: ibv_port_cap_flags = 33554432;
pub const IBV_PORT_CM_SUP: ibv_port_cap_flags = 65536;
pub const IBV_PORT_DEVICE_MGMT_SUP: ibv_port_cap_flags = 524288;
pub const IBV_PORT_DOWN: ibv_port_state = 1;
pub const IBV_PORT_DR_NOTICE_SUP: ibv_port_cap_flags = 2097152;
pub const IBV_PORT_EXTENDED_SPEEDS_SUP: ibv_port_cap_flags = 16384;
pub const IBV_PORT_INFO_EXT_SUP: ibv_port_cap_flags2 = 2;
pub const IBV_PORT_INIT: ibv_port_state = 2;
pub const IBV_PORT_IP_BASED_GIDS: ibv_port_cap_flags = 67108864;
pub const IBV_PORT_LED_INFO_SUP: ibv_port_cap_flags = 512;
pub const IBV_PORT_LINK_LATENCY_SUP: ibv_port_cap_flags = 16777216;
pub const IBV_PORT_LINK_SPEED_HDR_SUP: ibv_port_cap_flags2 = 32;
pub const IBV_PORT_LINK_SPEED_NDR_SUP: ibv_port_cap_flags2 = 1024;
pub const IBV_PORT_LINK_SPEED_XDR_SUP: ibv_port_cap_flags2 = 4096;
pub const IBV_PORT_LINK_WIDTH_2X_SUP: ibv_port_cap_flags2 = 16;
pub const IBV_PORT_MKEY_NVRAM: ibv_port_cap_flags = 128;
pub const IBV_PORT_NOP: ibv_port_state = 0;
pub const IBV_PORT_NOTICE_SUP: ibv_port_cap_flags = 4;
pub const IBV_PORT_OPT_IPD_SUP: ibv_port_cap_flags = 16;
pub const IBV_PORT_PKEY_NVRAM: ibv_port_cap_flags = 256;
pub const IBV_PORT_PKEY_SW_EXT_PORT_TRAP_SUP: ibv_port_cap_flags = 4096;
pub const IBV_PORT_REINIT_SUP: ibv_port_cap_flags = 262144;
pub const IBV_PORT_SET_NODE_DESC_SUP: ibv_port_cap_flags2 = 1;
pub const IBV_PORT_SL_MAP_SUP: ibv_port_cap_flags = 64;
pub const IBV_PORT_SM: ibv_port_cap_flags = 2;
pub const IBV_PORT_SNMP_TUNNEL_SUP: ibv_port_cap_flags = 131072;
pub const IBV_PORT_SWITCH_PORT_STATE_TABLE_SUP: ibv_port_cap_flags2 = 8;
pub const IBV_PORT_SYS_IMAGE_GUID_SUP: ibv_port_cap_flags = 2048;
pub const IBV_PORT_TRAP_SUP: ibv_port_cap_flags = 8;
pub const IBV_PORT_VENDOR_CLASS_SUP: ibv_port_cap_flags = 1048576;
pub const IBV_PORT_VIRT_SUP: ibv_port_cap_flags2 = 4;
pub const IBV_QPF_GRH_REQUIRED: i32 = 1;
pub const IBV_QPS_ERR: ibv_qp_state = 6;
pub const IBV_QPS_INIT: ibv_qp_state = 1;
pub const IBV_QPS_RESET: ibv_qp_state = 0;
pub const IBV_QPS_RTR: ibv_qp_state = 2;
pub const IBV_QPS_RTS: ibv_qp_state = 3;
pub const IBV_QPS_SQD: ibv_qp_state = 4;
pub const IBV_QPS_SQE: ibv_qp_state = 5;
pub const IBV_QPS_UNKNOWN: ibv_qp_state = 7;
pub const IBV_QPT_DRIVER: ibv_qp_type = 255;
pub const IBV_QPT_RAW_PACKET: ibv_qp_type = 8;
pub const IBV_QPT_RC: ibv_qp_type = 2;
pub const IBV_QPT_UC: ibv_qp_type = 3;
pub const IBV_QPT_UD: ibv_qp_type = 4;
pub const IBV_QPT_XRC_RECV: ibv_qp_type = 10;
pub const IBV_QPT_XRC_SEND: ibv_qp_type = 9;
pub const IBV_QP_ACCESS_FLAGS: ibv_qp_attr_mask = 8;
pub const IBV_QP_ALT_PATH: ibv_qp_attr_mask = 16384;
pub const IBV_QP_AV: ibv_qp_attr_mask = 128;
pub const IBV_QP_CAP: ibv_qp_attr_mask = 524288;
pub const IBV_QP_CREATE_BLOCK_SELF_MCAST_LB: ibv_qp_create_flags = 2;
pub const IBV_QP_CREATE_CVLAN_STRIPPING: ibv_qp_create_flags = 512;
pub const IBV_QP_CREATE_PCI_WRITE_END_PADDING: ibv_qp_create_flags = 2048;
pub const IBV_QP_CREATE_SCATTER_FCS: ibv_qp_create_flags = 256;
pub const IBV_QP_CREATE_SOURCE_QPN: ibv_qp_create_flags = 1024;
pub const IBV_QP_CUR_STATE: ibv_qp_attr_mask = 2;
pub const IBV_QP_DEST_QPN: ibv_qp_attr_mask = 1048576;
pub const IBV_QP_EN_SQD_ASYNC_NOTIFY: ibv_qp_attr_mask = 4;
pub const IBV_QP_EX_WITH_ATOMIC_CMP_AND_SWP: ibv_qp_create_send_ops_flags = 32;
pub const IBV_QP_EX_WITH_ATOMIC_FETCH_AND_ADD: ibv_qp_create_send_ops_flags = 64;
pub const IBV_QP_EX_WITH_ATOMIC_WRITE: ibv_qp_create_send_ops_flags = 4096;
pub const IBV_QP_EX_WITH_BIND_MW: ibv_qp_create_send_ops_flags = 256;
pub const IBV_QP_EX_WITH_FLUSH: ibv_qp_create_send_ops_flags = 2048;
pub const IBV_QP_EX_WITH_LOCAL_INV: ibv_qp_create_send_ops_flags = 128;
pub const IBV_QP_EX_WITH_RDMA_READ: ibv_qp_create_send_ops_flags = 16;
pub const IBV_QP_EX_WITH_RDMA_WRITE: ibv_qp_create_send_ops_flags = 1;
pub const IBV_QP_EX_WITH_RDMA_WRITE_WITH_IMM: ibv_qp_create_send_ops_flags = 2;
pub const IBV_QP_EX_WITH_SEND: ibv_qp_create_send_ops_flags = 4;
pub const IBV_QP_EX_WITH_SEND_WITH_IMM: ibv_qp_create_send_ops_flags = 8;
pub const IBV_QP_EX_WITH_SEND_WITH_INV: ibv_qp_create_send_ops_flags = 512;
pub const IBV_QP_EX_WITH_TSO: ibv_qp_create_send_ops_flags = 1024;
pub const IBV_QP_INIT_ATTR_CREATE_FLAGS: ibv_qp_init_attr_mask = 4;
pub const IBV_QP_INIT_ATTR_IND_TABLE: ibv_qp_init_attr_mask = 16;
pub const IBV_QP_INIT_ATTR_MAX_TSO_HEADER: ibv_qp_init_attr_mask = 8;
pub const IBV_QP_INIT_ATTR_PD: ibv_qp_init_attr_mask = 1;
pub const IBV_QP_INIT_ATTR_RX_HASH: ibv_qp_init_attr_mask = 32;
pub const IBV_QP_INIT_ATTR_SEND_OPS_FLAGS: ibv_qp_init_attr_mask = 64;
pub const IBV_QP_INIT_ATTR_XRCD: ibv_qp_init_attr_mask = 2;
pub const IBV_QP_MAX_DEST_RD_ATOMIC: ibv_qp_attr_mask = 131072;
pub const IBV_QP_MAX_QP_RD_ATOMIC: ibv_qp_attr_mask = 8192;
pub const IBV_QP_MIN_RNR_TIMER: ibv_qp_attr_mask = 32768;
pub const IBV_QP_OPEN_ATTR_CONTEXT: ibv_qp_open_attr_mask = 4;
pub const IBV_QP_OPEN_ATTR_NUM: ibv_qp_open_attr_mask = 1;
pub const IBV_QP_OPEN_ATTR_RESERVED: ibv_qp_open_attr_mask = 16;
pub const IBV_QP_OPEN_ATTR_TYPE: ibv_qp_open_attr_mask = 8;
pub const IBV_QP_OPEN_ATTR_XRCD: ibv_qp_open_attr_mask = 2;
pub const IBV_QP_PATH_MIG_STATE: ibv_qp_attr_mask = 262144;
pub const IBV_QP_PATH_MTU: ibv_qp_attr_mask = 256;
pub const IBV_QP_PKEY_INDEX: ibv_qp_attr_mask = 16;
pub const IBV_QP_PORT: ibv_qp_attr_mask = 32;
pub const IBV_QP_QKEY: ibv_qp_attr_mask = 64;
pub const IBV_QP_RATE_LIMIT: ibv_qp_attr_mask = 33554432;
pub const IBV_QP_RETRY_CNT: ibv_qp_attr_mask = 1024;
pub const IBV_QP_RNR_RETRY: ibv_qp_attr_mask = 2048;
pub const IBV_QP_RQ_PSN: ibv_qp_attr_mask = 4096;
pub const IBV_QP_SQ_PSN: ibv_qp_attr_mask = 65536;
pub const IBV_QP_STATE: ibv_qp_attr_mask = 1;
pub const IBV_QP_TIMEOUT: ibv_qp_attr_mask = 512;
pub const IBV_QUERY_QP_DATA_IN_ORDER_ALIGNED_128_BYTES: ibv_query_qp_data_in_order_caps = 2;
pub const IBV_QUERY_QP_DATA_IN_ORDER_DEVICE_ONLY: ibv_query_qp_data_in_order_flags = 2;
pub const IBV_QUERY_QP_DATA_IN_ORDER_RETURN_CAPS: ibv_query_qp_data_in_order_flags = 1;
pub const IBV_QUERY_QP_DATA_IN_ORDER_WHOLE_MSG: ibv_query_qp_data_in_order_caps = 1;
pub const IBV_RATE_100_GBPS: ibv_rate = 16;
pub const IBV_RATE_10_GBPS: ibv_rate = 3;
pub const IBV_RATE_112_GBPS: ibv_rate = 13;
pub const IBV_RATE_1200_GBPS: ibv_rate = 24;
pub const IBV_RATE_120_GBPS: ibv_rate = 10;
pub const IBV_RATE_14_GBPS: ibv_rate = 11;
pub const IBV_RATE_168_GBPS: ibv_rate = 14;
pub const IBV_RATE_200_GBPS: ibv_rate = 17;
pub const IBV_RATE_20_GBPS: ibv_rate = 6;
pub const IBV_RATE_25_GBPS: ibv_rate = 15;
pub const IBV_RATE_28_GBPS: ibv_rate = 19;
pub const IBV_RATE_2_5_GBPS: ibv_rate = 2;
pub const IBV_RATE_300_GBPS: ibv_rate = 18;
pub const IBV_RATE_30_GBPS: ibv_rate = 4;
pub const IBV_RATE_400_GBPS: ibv_rate = 21;
pub const IBV_RATE_40_GBPS: ibv_rate = 7;
pub const IBV_RATE_50_GBPS: ibv_rate = 20;
pub const IBV_RATE_56_GBPS: ibv_rate = 12;
pub const IBV_RATE_5_GBPS: ibv_rate = 5;
pub const IBV_RATE_600_GBPS: ibv_rate = 22;
pub const IBV_RATE_60_GBPS: ibv_rate = 8;
pub const IBV_RATE_800_GBPS: ibv_rate = 23;
pub const IBV_RATE_80_GBPS: ibv_rate = 9;
pub const IBV_RATE_MAX: ibv_rate = 0;
pub const IBV_RAW_PACKET_CAP_CVLAN_STRIPPING: ibv_raw_packet_caps = 1;
pub const IBV_RAW_PACKET_CAP_DELAY_DROP: ibv_raw_packet_caps = 8;
pub const IBV_RAW_PACKET_CAP_IP_CSUM: ibv_raw_packet_caps = 4;
pub const IBV_RAW_PACKET_CAP_SCATTER_FCS: ibv_raw_packet_caps = 2;
pub const IBV_READ_COUNTERS_ATTR_PREFER_CACHED: ibv_read_counters_flags = 1;
pub const IBV_REG_MR_MASK_ADDR: ibv_mr_init_attr_mask = 2;
pub const IBV_REG_MR_MASK_DMAH: ibv_mr_init_attr_mask = 16;
pub const IBV_REG_MR_MASK_FD: ibv_mr_init_attr_mask = 4;
pub const IBV_REG_MR_MASK_FD_OFFSET: ibv_mr_init_attr_mask = 8;
pub const IBV_REG_MR_MASK_IOVA: ibv_mr_init_attr_mask = 1;
pub const IBV_REREG_MR_CHANGE_ACCESS: ibv_rereg_mr_flags = 4;
pub const IBV_REREG_MR_CHANGE_PD: ibv_rereg_mr_flags = 2;
pub const IBV_REREG_MR_CHANGE_TRANSLATION: ibv_rereg_mr_flags = 1;
pub const IBV_REREG_MR_ERR_CMD: ibv_rereg_mr_err_code = -4;
pub const IBV_REREG_MR_ERR_CMD_AND_DO_FORK_NEW: ibv_rereg_mr_err_code = -5;
pub const IBV_REREG_MR_ERR_DONT_FORK_NEW: ibv_rereg_mr_err_code = -2;
pub const IBV_REREG_MR_ERR_DO_FORK_OLD: ibv_rereg_mr_err_code = -3;
pub const IBV_REREG_MR_ERR_INPUT: ibv_rereg_mr_err_code = -1;
pub const IBV_REREG_MR_FLAGS_SUPPORTED: ibv_rereg_mr_flags = 7;
pub const IBV_RX_HASH_DST_IPV4: ibv_rx_hash_fields = 2;
pub const IBV_RX_HASH_DST_IPV6: ibv_rx_hash_fields = 8;
pub const IBV_RX_HASH_DST_PORT_TCP: ibv_rx_hash_fields = 32;
pub const IBV_RX_HASH_DST_PORT_UDP: ibv_rx_hash_fields = 128;
pub const IBV_RX_HASH_FUNC_TOEPLITZ: ibv_rx_hash_function_flags = 1;
pub const IBV_RX_HASH_INNER: ibv_rx_hash_fields = 2147483648;
pub const IBV_RX_HASH_IPSEC_SPI: ibv_rx_hash_fields = 256;
pub const IBV_RX_HASH_SRC_IPV4: ibv_rx_hash_fields = 1;
pub const IBV_RX_HASH_SRC_IPV6: ibv_rx_hash_fields = 4;
pub const IBV_RX_HASH_SRC_PORT_TCP: ibv_rx_hash_fields = 16;
pub const IBV_RX_HASH_SRC_PORT_UDP: ibv_rx_hash_fields = 64;
pub const IBV_SEND_FENCE: ibv_send_flags = 1;
pub const IBV_SEND_INLINE: ibv_send_flags = 8;
pub const IBV_SEND_IP_CSUM: ibv_send_flags = 16;
pub const IBV_SEND_SIGNALED: ibv_send_flags = 2;
pub const IBV_SEND_SOLICITED: ibv_send_flags = 4;
pub const IBV_SRQT_BASIC: ibv_srq_type = 0;
pub const IBV_SRQT_TM: ibv_srq_type = 2;
pub const IBV_SRQT_XRC: ibv_srq_type = 1;
pub const IBV_SRQ_INIT_ATTR_CQ: ibv_srq_init_attr_mask = 8;
pub const IBV_SRQ_INIT_ATTR_PD: ibv_srq_init_attr_mask = 2;
pub const IBV_SRQ_INIT_ATTR_RESERVED: ibv_srq_init_attr_mask = 32;
pub const IBV_SRQ_INIT_ATTR_TM: ibv_srq_init_attr_mask = 16;
pub const IBV_SRQ_INIT_ATTR_TYPE: ibv_srq_init_attr_mask = 1;
pub const IBV_SRQ_INIT_ATTR_XRCD: ibv_srq_init_attr_mask = 4;
pub const IBV_SRQ_LIMIT: ibv_srq_attr_mask = 2;
pub const IBV_SRQ_MAX_WR: ibv_srq_attr_mask = 1;
pub const IBV_SYSFS_NAME_MAX: u32 = 64;
pub const IBV_SYSFS_PATH_MAX: u32 = 256;
pub const IBV_TM_CAP_RC: ibv_tm_cap_flags = 1;
pub const IBV_TPH_MEM_TYPE_PM: ibv_tph_mem_type = 1;
pub const IBV_TPH_MEM_TYPE_VM: ibv_tph_mem_type = 0;
pub const IBV_TRANSPORT_IB: ibv_transport_type = 0;
pub const IBV_TRANSPORT_IWARP: ibv_transport_type = 1;
pub const IBV_TRANSPORT_UNKNOWN: ibv_transport_type = -1;
pub const IBV_TRANSPORT_UNSPECIFIED: ibv_transport_type = 4;
pub const IBV_TRANSPORT_USNIC: ibv_transport_type = 2;
pub const IBV_TRANSPORT_USNIC_UDP: ibv_transport_type = 3;
pub const IBV_VALUES_MASK_RAW_CLOCK: ibv_values_mask = 1;
pub const IBV_VALUES_MASK_RESERVED: ibv_values_mask = 2;
pub const IBV_WC_ATOMIC_WRITE: ibv_wc_opcode = 9;
pub const IBV_WC_BAD_RESP_ERR: ibv_wc_status = 7;
pub const IBV_WC_BIND_MW: ibv_wc_opcode = 5;
pub const IBV_WC_COMP_SWAP: ibv_wc_opcode = 3;
pub const IBV_WC_DRIVER1: ibv_wc_opcode = 135;
pub const IBV_WC_DRIVER2: ibv_wc_opcode = 136;
pub const IBV_WC_DRIVER3: ibv_wc_opcode = 137;
pub const IBV_WC_EX_WITH_BYTE_LEN: ibv_create_cq_wc_flags = 1;
pub const IBV_WC_EX_WITH_COMPLETION_TIMESTAMP: ibv_create_cq_wc_flags = 128;
pub const IBV_WC_EX_WITH_COMPLETION_TIMESTAMP_WALLCLOCK: ibv_create_cq_wc_flags = 2048;
pub const IBV_WC_EX_WITH_CVLAN: ibv_create_cq_wc_flags = 256;
pub const IBV_WC_EX_WITH_DLID_PATH_BITS: ibv_create_cq_wc_flags = 64;
pub const IBV_WC_EX_WITH_FLOW_TAG: ibv_create_cq_wc_flags = 512;
pub const IBV_WC_EX_WITH_IMM: ibv_create_cq_wc_flags = 2;
pub const IBV_WC_EX_WITH_QP_NUM: ibv_create_cq_wc_flags = 4;
pub const IBV_WC_EX_WITH_SL: ibv_create_cq_wc_flags = 32;
pub const IBV_WC_EX_WITH_SLID: ibv_create_cq_wc_flags = 16;
pub const IBV_WC_EX_WITH_SRC_QP: ibv_create_cq_wc_flags = 8;
pub const IBV_WC_EX_WITH_TM_INFO: ibv_create_cq_wc_flags = 1024;
pub const IBV_WC_FATAL_ERR: ibv_wc_status = 19;
pub const IBV_WC_FETCH_ADD: ibv_wc_opcode = 4;
pub const IBV_WC_FLUSH: ibv_wc_opcode = 8;
pub const IBV_WC_GENERAL_ERR: ibv_wc_status = 21;
pub const IBV_WC_GRH: ibv_wc_flags = 1;
pub const IBV_WC_INV_EECN_ERR: ibv_wc_status = 17;
pub const IBV_WC_INV_EEC_STATE_ERR: ibv_wc_status = 18;
pub const IBV_WC_IP_CSUM_OK: ibv_wc_flags = 4;
pub const IBV_WC_IP_CSUM_OK_SHIFT: u32 = 2;
pub const IBV_WC_LOCAL_INV: ibv_wc_opcode = 6;
pub const IBV_WC_LOC_ACCESS_ERR: ibv_wc_status = 8;
pub const IBV_WC_LOC_EEC_OP_ERR: ibv_wc_status = 3;
pub const IBV_WC_LOC_LEN_ERR: ibv_wc_status = 1;
pub const IBV_WC_LOC_PROT_ERR: ibv_wc_status = 4;
pub const IBV_WC_LOC_QP_OP_ERR: ibv_wc_status = 2;
pub const IBV_WC_LOC_RDD_VIOL_ERR: ibv_wc_status = 14;
pub const IBV_WC_MW_BIND_ERR: ibv_wc_status = 6;
pub const IBV_WC_RDMA_READ: ibv_wc_opcode = 2;
pub const IBV_WC_RDMA_WRITE: ibv_wc_opcode = 1;
pub const IBV_WC_RECV: ibv_wc_opcode = 128;
pub const IBV_WC_RECV_RDMA_WITH_IMM: ibv_wc_opcode = 129;
pub const IBV_WC_REM_ABORT_ERR: ibv_wc_status = 16;
pub const IBV_WC_REM_ACCESS_ERR: ibv_wc_status = 10;
pub const IBV_WC_REM_INV_RD_REQ_ERR: ibv_wc_status = 15;
pub const IBV_WC_REM_INV_REQ_ERR: ibv_wc_status = 9;
pub const IBV_WC_REM_OP_ERR: ibv_wc_status = 11;
pub const IBV_WC_RESP_TIMEOUT_ERR: ibv_wc_status = 20;
pub const IBV_WC_RETRY_EXC_ERR: ibv_wc_status = 12;
pub const IBV_WC_RNR_RETRY_EXC_ERR: ibv_wc_status = 13;
pub const IBV_WC_SEND: ibv_wc_opcode = 0;
pub const IBV_WC_STANDARD_FLAGS: u32 = 127;
pub const IBV_WC_SUCCESS: ibv_wc_status = 0;
pub const IBV_WC_TM_ADD: ibv_wc_opcode = 130;
pub const IBV_WC_TM_DATA_VALID: ibv_wc_flags = 64;
pub const IBV_WC_TM_DEL: ibv_wc_opcode = 131;
pub const IBV_WC_TM_ERR: ibv_wc_status = 22;
pub const IBV_WC_TM_MATCH: ibv_wc_flags = 32;
pub const IBV_WC_TM_NO_TAG: ibv_wc_opcode = 134;
pub const IBV_WC_TM_RECV: ibv_wc_opcode = 133;
pub const IBV_WC_TM_RNDV_INCOMPLETE: ibv_wc_status = 23;
pub const IBV_WC_TM_SYNC: ibv_wc_opcode = 132;
pub const IBV_WC_TM_SYNC_REQ: ibv_wc_flags = 16;
pub const IBV_WC_TSO: ibv_wc_opcode = 7;
pub const IBV_WC_WITH_IMM: ibv_wc_flags = 2;
pub const IBV_WC_WITH_INV: ibv_wc_flags = 8;
pub const IBV_WC_WR_FLUSH_ERR: ibv_wc_status = 5;
pub const IBV_WQS_ERR: ibv_wq_state = 2;
pub const IBV_WQS_RDY: ibv_wq_state = 1;
pub const IBV_WQS_RESET: ibv_wq_state = 0;
pub const IBV_WQS_UNKNOWN: ibv_wq_state = 3;
pub const IBV_WQT_RQ: ibv_wq_type = 0;
pub const IBV_WQ_ATTR_CURR_STATE: ibv_wq_attr_mask = 2;
pub const IBV_WQ_ATTR_FLAGS: ibv_wq_attr_mask = 4;
pub const IBV_WQ_ATTR_RESERVED: ibv_wq_attr_mask = 8;
pub const IBV_WQ_ATTR_STATE: ibv_wq_attr_mask = 1;
pub const IBV_WQ_FLAGS_CVLAN_STRIPPING: ibv_wq_flags = 1;
pub const IBV_WQ_FLAGS_DELAY_DROP: ibv_wq_flags = 4;
pub const IBV_WQ_FLAGS_PCI_WRITE_END_PADDING: ibv_wq_flags = 8;
pub const IBV_WQ_FLAGS_RESERVED: ibv_wq_flags = 16;
pub const IBV_WQ_FLAGS_SCATTER_FCS: ibv_wq_flags = 2;
pub const IBV_WQ_INIT_ATTR_FLAGS: ibv_wq_init_attr_mask = 1;
pub const IBV_WQ_INIT_ATTR_RESERVED: ibv_wq_init_attr_mask = 2;
pub const IBV_WR_ATOMIC_CMP_AND_SWP: ibv_wr_opcode = 5;
pub const IBV_WR_ATOMIC_FETCH_AND_ADD: ibv_wr_opcode = 6;
pub const IBV_WR_ATOMIC_WRITE: ibv_wr_opcode = 15;
pub const IBV_WR_BIND_MW: ibv_wr_opcode = 8;
pub const IBV_WR_DRIVER1: ibv_wr_opcode = 11;
pub const IBV_WR_FLUSH: ibv_wr_opcode = 14;
pub const IBV_WR_LOCAL_INV: ibv_wr_opcode = 7;
pub const IBV_WR_RDMA_READ: ibv_wr_opcode = 4;
pub const IBV_WR_RDMA_WRITE: ibv_wr_opcode = 0;
pub const IBV_WR_RDMA_WRITE_WITH_IMM: ibv_wr_opcode = 1;
pub const IBV_WR_SEND: ibv_wr_opcode = 2;
pub const IBV_WR_SEND_WITH_IMM: ibv_wr_opcode = 3;
pub const IBV_WR_SEND_WITH_INV: ibv_wr_opcode = 9;
pub const IBV_WR_TAG_ADD: ibv_ops_wr_opcode = 0;
pub const IBV_WR_TAG_DEL: ibv_ops_wr_opcode = 1;
pub const IBV_WR_TAG_SYNC: ibv_ops_wr_opcode = 2;
pub const IBV_WR_TSO: ibv_wr_opcode = 10;
pub const IBV_XRCD_INIT_ATTR_FD: ibv_xrcd_init_attr_mask = 1;
pub const IBV_XRCD_INIT_ATTR_OFLAGS: ibv_xrcd_init_attr_mask = 2;
pub const IBV_XRCD_INIT_ATTR_RESERVED: ibv_xrcd_init_attr_mask = 4;
pub const IB_DEVICE_NAME_MAX: i32 = 64;
pub const IB_FLUSH_GLOBAL: ib_placement_type = 1;
pub const IB_FLUSH_MR: ib_selectivity_level = 1;
pub const IB_FLUSH_PERSISTENT: ib_placement_type = 2;
pub const IB_FLUSH_RANGE: ib_selectivity_level = 0;
pub const IB_GRH_FLOWLABEL_MASK: i32 = 1048575;
pub const IB_ROCE_UDP_ENCAP_VALID_PORT_MAX: i32 = 65535;
pub const IB_ROCE_UDP_ENCAP_VALID_PORT_MIN: i32 = 49152;
pub const IB_USER_VERBS_ABI_VERSION: i32 = 6;
pub const IB_USER_VERBS_CMD_ALLOC_MW: ib_uverbs_write_cmds = 14;
pub const IB_USER_VERBS_CMD_ALLOC_PD: ib_uverbs_write_cmds = 3;
pub const IB_USER_VERBS_CMD_ATTACH_MCAST: ib_uverbs_write_cmds = 30;
pub const IB_USER_VERBS_CMD_BIND_MW: ib_uverbs_write_cmds = 15;
pub const IB_USER_VERBS_CMD_CLOSE_XRCD: ib_uverbs_write_cmds = 38;
pub const IB_USER_VERBS_CMD_COMMAND_MASK: i32 = 255;
pub const IB_USER_VERBS_CMD_CREATE_AH: ib_uverbs_write_cmds = 5;
pub const IB_USER_VERBS_CMD_CREATE_COMP_CHANNEL: ib_uverbs_write_cmds = 17;
pub const IB_USER_VERBS_CMD_CREATE_CQ: ib_uverbs_write_cmds = 18;
pub const IB_USER_VERBS_CMD_CREATE_QP: ib_uverbs_write_cmds = 24;
pub const IB_USER_VERBS_CMD_CREATE_SRQ: ib_uverbs_write_cmds = 32;
pub const IB_USER_VERBS_CMD_CREATE_XSRQ: ib_uverbs_write_cmds = 39;
pub const IB_USER_VERBS_CMD_DEALLOC_MW: ib_uverbs_write_cmds = 16;
pub const IB_USER_VERBS_CMD_DEALLOC_PD: ib_uverbs_write_cmds = 4;
pub const IB_USER_VERBS_CMD_DEREG_MR: ib_uverbs_write_cmds = 13;
pub const IB_USER_VERBS_CMD_DESTROY_AH: ib_uverbs_write_cmds = 8;
pub const IB_USER_VERBS_CMD_DESTROY_CQ: ib_uverbs_write_cmds = 20;
pub const IB_USER_VERBS_CMD_DESTROY_QP: ib_uverbs_write_cmds = 27;
pub const IB_USER_VERBS_CMD_DESTROY_SRQ: ib_uverbs_write_cmds = 35;
pub const IB_USER_VERBS_CMD_DETACH_MCAST: ib_uverbs_write_cmds = 31;
pub const IB_USER_VERBS_CMD_FLAG_EXTENDED: u32 = 2147483648;
pub const IB_USER_VERBS_CMD_GET_CONTEXT: ib_uverbs_write_cmds = 0;
pub const IB_USER_VERBS_CMD_MODIFY_AH: ib_uverbs_write_cmds = 6;
pub const IB_USER_VERBS_CMD_MODIFY_QP: ib_uverbs_write_cmds = 26;
pub const IB_USER_VERBS_CMD_MODIFY_SRQ: ib_uverbs_write_cmds = 33;
pub const IB_USER_VERBS_CMD_OPEN_QP: ib_uverbs_write_cmds = 40;
pub const IB_USER_VERBS_CMD_OPEN_XRCD: ib_uverbs_write_cmds = 37;
pub const IB_USER_VERBS_CMD_PEEK_CQ: ib_uverbs_write_cmds = 22;
pub const IB_USER_VERBS_CMD_POLL_CQ: ib_uverbs_write_cmds = 21;
pub const IB_USER_VERBS_CMD_POST_RECV: ib_uverbs_write_cmds = 29;
pub const IB_USER_VERBS_CMD_POST_SEND: ib_uverbs_write_cmds = 28;
pub const IB_USER_VERBS_CMD_POST_SRQ_RECV: ib_uverbs_write_cmds = 36;
pub const IB_USER_VERBS_CMD_QUERY_AH: ib_uverbs_write_cmds = 7;
pub const IB_USER_VERBS_CMD_QUERY_DEVICE: ib_uverbs_write_cmds = 1;
pub const IB_USER_VERBS_CMD_QUERY_MR: ib_uverbs_write_cmds = 12;
pub const IB_USER_VERBS_CMD_QUERY_PORT: ib_uverbs_write_cmds = 2;
pub const IB_USER_VERBS_CMD_QUERY_QP: ib_uverbs_write_cmds = 25;
pub const IB_USER_VERBS_CMD_QUERY_SRQ: ib_uverbs_write_cmds = 34;
pub const IB_USER_VERBS_CMD_REG_MR: ib_uverbs_write_cmds = 9;
pub const IB_USER_VERBS_CMD_REG_SMR: ib_uverbs_write_cmds = 10;
pub const IB_USER_VERBS_CMD_REQ_NOTIFY_CQ: ib_uverbs_write_cmds = 23;
pub const IB_USER_VERBS_CMD_REREG_MR: ib_uverbs_write_cmds = 11;
pub const IB_USER_VERBS_CMD_RESIZE_CQ: ib_uverbs_write_cmds = 19;
pub const IB_USER_VERBS_CMD_THRESHOLD: i32 = 50;
pub const IB_USER_VERBS_EX_CMD_CREATE_CQ: u32 = 18;
pub const IB_USER_VERBS_EX_CMD_CREATE_FLOW: u32 = 50;
pub const IB_USER_VERBS_EX_CMD_CREATE_QP: u32 = 24;
pub const IB_USER_VERBS_EX_CMD_CREATE_RWQ_IND_TBL: u32 = 55;
pub const IB_USER_VERBS_EX_CMD_CREATE_WQ: u32 = 52;
pub const IB_USER_VERBS_EX_CMD_DESTROY_FLOW: u32 = 51;
pub const IB_USER_VERBS_EX_CMD_DESTROY_RWQ_IND_TBL: u32 = 56;
pub const IB_USER_VERBS_EX_CMD_DESTROY_WQ: u32 = 54;
pub const IB_USER_VERBS_EX_CMD_MODIFY_CQ: u32 = 57;
pub const IB_USER_VERBS_EX_CMD_MODIFY_QP: u32 = 26;
pub const IB_USER_VERBS_EX_CMD_MODIFY_WQ: u32 = 53;
pub const IB_USER_VERBS_EX_CMD_QUERY_DEVICE: u32 = 1;
pub const IB_USER_VERBS_MAX_LOG_IND_TBL_SIZE: i32 = 13;
pub const IB_UVERBS_ACCESS_FLUSH_GLOBAL: ib_uverbs_access_flags = 256;
pub const IB_UVERBS_ACCESS_FLUSH_PERSISTENT: ib_uverbs_access_flags = 512;
pub const IB_UVERBS_ACCESS_HUGETLB: ib_uverbs_access_flags = 128;
pub const IB_UVERBS_ACCESS_LOCAL_WRITE: ib_uverbs_access_flags = 1;
pub const IB_UVERBS_ACCESS_MW_BIND: ib_uverbs_access_flags = 16;
pub const IB_UVERBS_ACCESS_ON_DEMAND: ib_uverbs_access_flags = 64;
pub const IB_UVERBS_ACCESS_OPTIONAL_FIRST: i32 = 1048576;
pub const IB_UVERBS_ACCESS_OPTIONAL_LAST: i32 = 536870912;
pub const IB_UVERBS_ACCESS_OPTIONAL_RANGE: ib_uverbs_access_flags = 1072693248;
pub const IB_UVERBS_ACCESS_RELAXED_ORDERING: ib_uverbs_access_flags = 1048576;
pub const IB_UVERBS_ACCESS_REMOTE_ATOMIC: ib_uverbs_access_flags = 8;
pub const IB_UVERBS_ACCESS_REMOTE_READ: ib_uverbs_access_flags = 4;
pub const IB_UVERBS_ACCESS_REMOTE_WRITE: ib_uverbs_access_flags = 2;
pub const IB_UVERBS_ACCESS_ZERO_BASED: ib_uverbs_access_flags = 32;
pub const IB_UVERBS_ADVISE_MR_ADVICE_PREFETCH: ib_uverbs_advise_mr_advice = 0;
pub const IB_UVERBS_ADVISE_MR_ADVICE_PREFETCH_NO_FAULT: ib_uverbs_advise_mr_advice = 2;
pub const IB_UVERBS_ADVISE_MR_ADVICE_PREFETCH_WRITE: ib_uverbs_advise_mr_advice = 1;
pub const IB_UVERBS_ADVISE_MR_FLAG_FLUSH: ib_uverbs_advise_mr_flag = 1;
pub const IB_UVERBS_CORE_SUPPORT_OPTIONAL_MR_ACCESS: ib_uverbs_core_support = 1;
pub const IB_UVERBS_CQ_FLAGS_IGNORE_OVERRUN: ib_uverbs_ex_create_cq_flags = 2;
pub const IB_UVERBS_CQ_FLAGS_TIMESTAMP_COMPLETION: ib_uverbs_ex_create_cq_flags = 1;
pub const IB_UVERBS_CREATE_QP_MASK_IND_TABLE: ib_uverbs_create_qp_mask = 1;
pub const IB_UVERBS_CREATE_QP_SUP_COMP_MASK: u32 = 1;
pub const IB_UVERBS_DEVICE_ATOMIC_WRITE: ib_uverbs_device_cap_flags = 1099511627776;
pub const IB_UVERBS_DEVICE_AUTO_PATH_MIG: ib_uverbs_device_cap_flags = 16;
pub const IB_UVERBS_DEVICE_BAD_PKEY_CNTR: ib_uverbs_device_cap_flags = 2;
pub const IB_UVERBS_DEVICE_BAD_QKEY_CNTR: ib_uverbs_device_cap_flags = 4;
pub const IB_UVERBS_DEVICE_CHANGE_PHY_PORT: ib_uverbs_device_cap_flags = 32;
pub const IB_UVERBS_DEVICE_CURR_QP_STATE_MOD: ib_uverbs_device_cap_flags = 128;
pub const IB_UVERBS_DEVICE_FLUSH_GLOBAL: ib_uverbs_device_cap_flags = 274877906944;
pub const IB_UVERBS_DEVICE_FLUSH_PERSISTENT: ib_uverbs_device_cap_flags = 549755813888;
pub const IB_UVERBS_DEVICE_MANAGED_FLOW_STEERING: ib_uverbs_device_cap_flags = 536870912;
pub const IB_UVERBS_DEVICE_MEM_MGT_EXTENSIONS: ib_uverbs_device_cap_flags = 2097152;
pub const IB_UVERBS_DEVICE_MEM_WINDOW: ib_uverbs_device_cap_flags = 131072;
pub const IB_UVERBS_DEVICE_MEM_WINDOW_TYPE_2A: ib_uverbs_device_cap_flags = 8388608;
pub const IB_UVERBS_DEVICE_MEM_WINDOW_TYPE_2B: ib_uverbs_device_cap_flags = 16777216;
pub const IB_UVERBS_DEVICE_N_NOTIFY_CQ: ib_uverbs_device_cap_flags = 16384;
pub const IB_UVERBS_DEVICE_PCI_WRITE_END_PADDING: ib_uverbs_device_cap_flags = 68719476736;
pub const IB_UVERBS_DEVICE_PORT_ACTIVE_EVENT: ib_uverbs_device_cap_flags = 1024;
pub const IB_UVERBS_DEVICE_RAW_IP_CSUM: ib_uverbs_device_cap_flags = 67108864;
pub const IB_UVERBS_DEVICE_RAW_MULTI: ib_uverbs_device_cap_flags = 8;
pub const IB_UVERBS_DEVICE_RAW_SCATTER_FCS: ib_uverbs_device_cap_flags = 17179869184;
pub const IB_UVERBS_DEVICE_RC_IP_CSUM: ib_uverbs_device_cap_flags = 33554432;
pub const IB_UVERBS_DEVICE_RC_RNR_NAK_GEN: ib_uverbs_device_cap_flags = 4096;
pub const IB_UVERBS_DEVICE_RESIZE_MAX_WR: ib_uverbs_device_cap_flags = 1;
pub const IB_UVERBS_DEVICE_SHUTDOWN_PORT: ib_uverbs_device_cap_flags = 256;
pub const IB_UVERBS_DEVICE_SRQ_RESIZE: ib_uverbs_device_cap_flags = 8192;
pub const IB_UVERBS_DEVICE_SYS_IMAGE_GUID: ib_uverbs_device_cap_flags = 2048;
pub const IB_UVERBS_DEVICE_UD_AV_PORT_ENFORCE: ib_uverbs_device_cap_flags = 64;
pub const IB_UVERBS_DEVICE_UD_IP_CSUM: ib_uverbs_device_cap_flags = 262144;
pub const IB_UVERBS_DEVICE_XRC: ib_uverbs_device_cap_flags = 1048576;
pub const IB_UVERBS_FLOW_ACTION_ESP_FLAGS_DECRYPT: ib_uverbs_flow_action_esp_flags = 0;
pub const IB_UVERBS_FLOW_ACTION_ESP_FLAGS_ENCRYPT: ib_uverbs_flow_action_esp_flags = 4;
pub const IB_UVERBS_FLOW_ACTION_ESP_FLAGS_ESN_NEW_WINDOW: ib_uverbs_flow_action_esp_flags = 8;
pub const IB_UVERBS_FLOW_ACTION_ESP_FLAGS_FULL_OFFLOAD: ib_uverbs_flow_action_esp_flags = 1;
pub const IB_UVERBS_FLOW_ACTION_ESP_FLAGS_INLINE_CRYPTO: ib_uverbs_flow_action_esp_flags = 0;
pub const IB_UVERBS_FLOW_ACTION_ESP_FLAGS_TRANSPORT: ib_uverbs_flow_action_esp_flags = 2;
pub const IB_UVERBS_FLOW_ACTION_ESP_FLAGS_TUNNEL: ib_uverbs_flow_action_esp_flags = 0;
pub const IB_UVERBS_FLOW_ACTION_ESP_KEYMAT_AES_GCM: ib_uverbs_flow_action_esp_keymat = 0;
pub const IB_UVERBS_FLOW_ACTION_ESP_REPLAY_BMP: ib_uverbs_flow_action_esp_replay = 1;
pub const IB_UVERBS_FLOW_ACTION_ESP_REPLAY_NONE: ib_uverbs_flow_action_esp_replay = 0;
pub const IB_UVERBS_FLOW_ACTION_IV_ALGO_SEQ: ib_uverbs_flow_action_esp_keymat_aes_gcm_iv_algo = 0;
pub const IB_UVERBS_GID_TYPE_IB: ib_uverbs_gid_type = 0;
pub const IB_UVERBS_GID_TYPE_ROCE_V1: ib_uverbs_gid_type = 1;
pub const IB_UVERBS_GID_TYPE_ROCE_V2: ib_uverbs_gid_type = 2;
pub const IB_UVERBS_ODP_SUPPORT: ib_uverbs_odp_general_cap_bits = 1;
pub const IB_UVERBS_ODP_SUPPORT_ATOMIC: ib_uverbs_odp_transport_cap_bits = 16;
pub const IB_UVERBS_ODP_SUPPORT_ATOMIC_WRITE: ib_uverbs_odp_transport_cap_bits = 128;
pub const IB_UVERBS_ODP_SUPPORT_FLUSH: ib_uverbs_odp_transport_cap_bits = 64;
pub const IB_UVERBS_ODP_SUPPORT_IMPLICIT: ib_uverbs_odp_general_cap_bits = 2;
pub const IB_UVERBS_ODP_SUPPORT_READ: ib_uverbs_odp_transport_cap_bits = 8;
pub const IB_UVERBS_ODP_SUPPORT_RECV: ib_uverbs_odp_transport_cap_bits = 2;
pub const IB_UVERBS_ODP_SUPPORT_SEND: ib_uverbs_odp_transport_cap_bits = 1;
pub const IB_UVERBS_ODP_SUPPORT_SRQ_RECV: ib_uverbs_odp_transport_cap_bits = 32;
pub const IB_UVERBS_ODP_SUPPORT_WRITE: ib_uverbs_odp_transport_cap_bits = 4;
pub const IB_UVERBS_PCF_AUTO_MIGR_SUP: ib_uverbs_query_port_cap_flags = 32;
pub const IB_UVERBS_PCF_BOOT_MGMT_SUP: ib_uverbs_query_port_cap_flags = 8388608;
pub const IB_UVERBS_PCF_CAP_MASK_NOTICE_SUP: ib_uverbs_query_port_cap_flags = 4194304;
pub const IB_UVERBS_PCF_CLIENT_REG_SUP: ib_uverbs_query_port_cap_flags = 33554432;
pub const IB_UVERBS_PCF_CM_SUP: ib_uverbs_query_port_cap_flags = 65536;
pub const IB_UVERBS_PCF_DEVICE_MGMT_SUP: ib_uverbs_query_port_cap_flags = 524288;
pub const IB_UVERBS_PCF_DR_NOTICE_SUP: ib_uverbs_query_port_cap_flags = 2097152;
pub const IB_UVERBS_PCF_EXTENDED_SPEEDS_SUP: ib_uverbs_query_port_cap_flags = 16384;
pub const IB_UVERBS_PCF_HIERARCHY_INFO_SUP: ib_uverbs_query_port_cap_flags = 2147483648;
pub const IB_UVERBS_PCF_IP_BASED_GIDS: ib_uverbs_query_port_cap_flags = 67108864;
pub const IB_UVERBS_PCF_LED_INFO_SUP: ib_uverbs_query_port_cap_flags = 512;
pub const IB_UVERBS_PCF_LINK_LATENCY_SUP: ib_uverbs_query_port_cap_flags = 16777216;
pub const IB_UVERBS_PCF_LINK_SPEED_WIDTH_TABLE_SUP: ib_uverbs_query_port_cap_flags = 134217728;
pub const IB_UVERBS_PCF_MCAST_FDB_TOP_SUP: ib_uverbs_query_port_cap_flags = 1073741824;
pub const IB_UVERBS_PCF_MCAST_PKEY_TRAP_SUPPRESSION_SUP: ib_uverbs_query_port_cap_flags = 536870912;
pub const IB_UVERBS_PCF_MKEY_NVRAM: ib_uverbs_query_port_cap_flags = 128;
pub const IB_UVERBS_PCF_NOTICE_SUP: ib_uverbs_query_port_cap_flags = 4;
pub const IB_UVERBS_PCF_OPT_IPD_SUP: ib_uverbs_query_port_cap_flags = 16;
pub const IB_UVERBS_PCF_PKEY_NVRAM: ib_uverbs_query_port_cap_flags = 256;
pub const IB_UVERBS_PCF_PKEY_SW_EXT_PORT_TRAP_SUP: ib_uverbs_query_port_cap_flags = 4096;
pub const IB_UVERBS_PCF_REINIT_SUP: ib_uverbs_query_port_cap_flags = 262144;
pub const IB_UVERBS_PCF_SL_MAP_SUP: ib_uverbs_query_port_cap_flags = 64;
pub const IB_UVERBS_PCF_SM: ib_uverbs_query_port_cap_flags = 2;
pub const IB_UVERBS_PCF_SM_DISABLED: ib_uverbs_query_port_cap_flags = 1024;
pub const IB_UVERBS_PCF_SNMP_TUNNEL_SUP: ib_uverbs_query_port_cap_flags = 131072;
pub const IB_UVERBS_PCF_SYS_IMAGE_GUID_SUP: ib_uverbs_query_port_cap_flags = 2048;
pub const IB_UVERBS_PCF_TRAP_SUP: ib_uverbs_query_port_cap_flags = 8;
pub const IB_UVERBS_PCF_VENDOR_CLASS_SUP: ib_uverbs_query_port_cap_flags = 1048576;
pub const IB_UVERBS_PCF_VENDOR_SPECIFIC_MADS_TABLE_SUP: ib_uverbs_query_port_cap_flags = 268435456;
pub const IB_UVERBS_QPF_GRH_REQUIRED: ib_uverbs_query_port_flags = 1;
pub const IB_UVERBS_QPT_DRIVER: ib_uverbs_qp_type = 255;
pub const IB_UVERBS_QPT_RAW_PACKET: ib_uverbs_qp_type = 8;
pub const IB_UVERBS_QPT_RC: ib_uverbs_qp_type = 2;
pub const IB_UVERBS_QPT_UC: ib_uverbs_qp_type = 3;
pub const IB_UVERBS_QPT_UD: ib_uverbs_qp_type = 4;
pub const IB_UVERBS_QPT_XRC_INI: ib_uverbs_qp_type = 9;
pub const IB_UVERBS_QPT_XRC_TGT: ib_uverbs_qp_type = 10;
pub const IB_UVERBS_QP_CREATE_BLOCK_MULTICAST_LOOPBACK: ib_uverbs_qp_create_flags = 2;
pub const IB_UVERBS_QP_CREATE_CVLAN_STRIPPING: ib_uverbs_qp_create_flags = 512;
pub const IB_UVERBS_QP_CREATE_PCI_WRITE_END_PADDING: ib_uverbs_qp_create_flags = 2048;
pub const IB_UVERBS_QP_CREATE_SCATTER_FCS: ib_uverbs_qp_create_flags = 256;
pub const IB_UVERBS_QP_CREATE_SQ_SIG_ALL: ib_uverbs_qp_create_flags = 4096;
pub const IB_UVERBS_RAW_PACKET_CAP_CVLAN_STRIPPING: ib_uverbs_raw_packet_caps = 1;
pub const IB_UVERBS_RAW_PACKET_CAP_DELAY_DROP: ib_uverbs_raw_packet_caps = 8;
pub const IB_UVERBS_RAW_PACKET_CAP_IP_CSUM: ib_uverbs_raw_packet_caps = 4;
pub const IB_UVERBS_RAW_PACKET_CAP_SCATTER_FCS: ib_uverbs_raw_packet_caps = 2;
pub const IB_UVERBS_READ_COUNTERS_PREFER_CACHED: ib_uverbs_read_counters_flags = 1;
pub const IB_UVERBS_SRQT_BASIC: ib_uverbs_srq_type = 0;
pub const IB_UVERBS_SRQT_TM: ib_uverbs_srq_type = 2;
pub const IB_UVERBS_SRQT_XRC: ib_uverbs_srq_type = 1;
pub const IB_UVERBS_WC_ATOMIC_WRITE: ib_uverbs_wc_opcode = 9;
pub const IB_UVERBS_WC_BIND_MW: ib_uverbs_wc_opcode = 5;
pub const IB_UVERBS_WC_COMP_SWAP: ib_uverbs_wc_opcode = 3;
pub const IB_UVERBS_WC_FETCH_ADD: ib_uverbs_wc_opcode = 4;
pub const IB_UVERBS_WC_FLUSH: ib_uverbs_wc_opcode = 8;
pub const IB_UVERBS_WC_LOCAL_INV: ib_uverbs_wc_opcode = 6;
pub const IB_UVERBS_WC_RDMA_READ: ib_uverbs_wc_opcode = 2;
pub const IB_UVERBS_WC_RDMA_WRITE: ib_uverbs_wc_opcode = 1;
pub const IB_UVERBS_WC_SEND: ib_uverbs_wc_opcode = 0;
pub const IB_UVERBS_WC_TSO: ib_uverbs_wc_opcode = 7;
pub const IB_UVERBS_WQT_RQ: ib_uverbs_wq_type = 0;
pub const IB_UVERBS_WQ_FLAGS_CVLAN_STRIPPING: ib_uverbs_wq_flags = 1;
pub const IB_UVERBS_WQ_FLAGS_DELAY_DROP: ib_uverbs_wq_flags = 4;
pub const IB_UVERBS_WQ_FLAGS_PCI_WRITE_END_PADDING: ib_uverbs_wq_flags = 8;
pub const IB_UVERBS_WQ_FLAGS_SCATTER_FCS: ib_uverbs_wq_flags = 2;
pub const IB_UVERBS_WR_ATOMIC_CMP_AND_SWP: ib_uverbs_wr_opcode = 5;
pub const IB_UVERBS_WR_ATOMIC_FETCH_AND_ADD: ib_uverbs_wr_opcode = 6;
pub const IB_UVERBS_WR_ATOMIC_WRITE: ib_uverbs_wr_opcode = 15;
pub const IB_UVERBS_WR_BIND_MW: ib_uverbs_wr_opcode = 8;
pub const IB_UVERBS_WR_FLUSH: ib_uverbs_wr_opcode = 14;
pub const IB_UVERBS_WR_LOCAL_INV: ib_uverbs_wr_opcode = 7;
pub const IB_UVERBS_WR_MASKED_ATOMIC_CMP_AND_SWP: ib_uverbs_wr_opcode = 12;
pub const IB_UVERBS_WR_MASKED_ATOMIC_FETCH_AND_ADD: ib_uverbs_wr_opcode = 13;
pub const IB_UVERBS_WR_RDMA_READ: ib_uverbs_wr_opcode = 4;
pub const IB_UVERBS_WR_RDMA_READ_WITH_INV: ib_uverbs_wr_opcode = 11;
pub const IB_UVERBS_WR_RDMA_WRITE: ib_uverbs_wr_opcode = 0;
pub const IB_UVERBS_WR_RDMA_WRITE_WITH_IMM: ib_uverbs_wr_opcode = 1;
pub const IB_UVERBS_WR_SEND: ib_uverbs_wr_opcode = 2;
pub const IB_UVERBS_WR_SEND_WITH_IMM: ib_uverbs_wr_opcode = 3;
pub const IB_UVERBS_WR_SEND_WITH_INV: ib_uverbs_wr_opcode = 9;
pub const IB_UVERBS_WR_TSO: ib_uverbs_wr_opcode = 10;
pub const RDMA_DRIVER_BNXT_RE: rdma_driver_id = 6;
pub const RDMA_DRIVER_CXGB3: rdma_driver_id = 3;
pub const RDMA_DRIVER_CXGB4: rdma_driver_id = 4;
pub const RDMA_DRIVER_EFA: rdma_driver_id = 17;
pub const RDMA_DRIVER_ERDMA: rdma_driver_id = 19;
pub const RDMA_DRIVER_HFI1: rdma_driver_id = 15;
pub const RDMA_DRIVER_HNS: rdma_driver_id = 12;
pub const RDMA_DRIVER_I40IW: rdma_driver_id = 9;
pub const RDMA_DRIVER_IONIC: rdma_driver_id = 21;
pub const RDMA_DRIVER_IRDMA: rdma_driver_id = 9;
pub const RDMA_DRIVER_MANA: rdma_driver_id = 20;
pub const RDMA_DRIVER_MLX4: rdma_driver_id = 2;
pub const RDMA_DRIVER_MLX5: rdma_driver_id = 1;
pub const RDMA_DRIVER_MTHCA: rdma_driver_id = 5;
pub const RDMA_DRIVER_NES: rdma_driver_id = 8;
pub const RDMA_DRIVER_OCRDMA: rdma_driver_id = 7;
pub const RDMA_DRIVER_QEDR: rdma_driver_id = 11;
pub const RDMA_DRIVER_QIB: rdma_driver_id = 16;
pub const RDMA_DRIVER_RXE: rdma_driver_id = 14;
pub const RDMA_DRIVER_SIW: rdma_driver_id = 18;
pub const RDMA_DRIVER_UNKNOWN: rdma_driver_id = 0;
pub const RDMA_DRIVER_USNIC: rdma_driver_id = 13;
pub const RDMA_DRIVER_VMW_PVRDMA: rdma_driver_id = 10;
pub type __s32 = i32;
pub type __u8 = u8;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct _compat_ibv_port_attr(pub u8);
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct _ibv_device_ops {
    pub _dummy1: *mut u8,
    pub _dummy2: *mut u8,
}
pub type ib_placement_type = u32;
pub type ib_selectivity_level = u32;
pub type ib_uverbs_access_flags = u32;
pub type ib_uverbs_advise_mr_advice = u32;
pub type ib_uverbs_advise_mr_flag = u32;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_ah_attr {
    pub grh: ib_uverbs_global_route,
    pub dlid: bnd_linux::libc::int_ll64::__u16,
    pub sl: __u8,
    pub src_path_bits: __u8,
    pub static_rate: __u8,
    pub is_global: __u8,
    pub port_num: __u8,
    pub reserved: __u8,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_alloc_mw {
    pub response: bnd_linux::libc::int_ll64::__u64,
    pub pd_handle: bnd_linux::libc::int_ll64::__u32,
    pub mw_type: __u8,
    pub reserved: [__u8; 3],
    pub driver_data: [bnd_linux::libc::int_ll64::__u64; 0],
}
impl Default for ib_uverbs_alloc_mw {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_alloc_mw_resp {
    pub mw_handle: bnd_linux::libc::int_ll64::__u32,
    pub rkey: bnd_linux::libc::int_ll64::__u32,
    pub driver_data: [bnd_linux::libc::int_ll64::__u64; 0],
}
impl Default for ib_uverbs_alloc_mw_resp {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_alloc_pd {
    pub response: bnd_linux::libc::int_ll64::__u64,
    pub driver_data: [bnd_linux::libc::int_ll64::__u64; 0],
}
impl Default for ib_uverbs_alloc_pd {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_alloc_pd_resp {
    pub pd_handle: bnd_linux::libc::int_ll64::__u32,
    pub driver_data: [bnd_linux::libc::int_ll64::__u32; 0],
}
impl Default for ib_uverbs_alloc_pd_resp {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_async_event_desc {
    pub element: bnd_linux::libc::int_ll64::__u64,
    pub event_type: bnd_linux::libc::int_ll64::__u32,
    pub reserved: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_attach_mcast {
    pub gid: [__u8; 16],
    pub qp_handle: bnd_linux::libc::int_ll64::__u32,
    pub mlid: bnd_linux::libc::int_ll64::__u16,
    pub reserved: bnd_linux::libc::int_ll64::__u16,
    pub driver_data: [bnd_linux::libc::int_ll64::__u64; 0],
}
impl Default for ib_uverbs_attach_mcast {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_close_xrcd {
    pub xrcd_handle: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_cmd_hdr {
    pub command: bnd_linux::libc::int_ll64::__u32,
    pub in_words: bnd_linux::libc::int_ll64::__u16,
    pub out_words: bnd_linux::libc::int_ll64::__u16,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_comp_event_desc {
    pub cq_handle: bnd_linux::libc::int_ll64::__u64,
}
pub type ib_uverbs_core_support = u32;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_cq_moderation {
    pub cq_count: bnd_linux::libc::int_ll64::__u16,
    pub cq_period: bnd_linux::libc::int_ll64::__u16,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_cq_moderation_caps {
    pub max_cq_moderation_count: bnd_linux::libc::int_ll64::__u16,
    pub max_cq_moderation_period: bnd_linux::libc::int_ll64::__u16,
    pub reserved: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_create_ah {
    pub response: bnd_linux::libc::int_ll64::__u64,
    pub user_handle: bnd_linux::libc::int_ll64::__u64,
    pub pd_handle: bnd_linux::libc::int_ll64::__u32,
    pub reserved: bnd_linux::libc::int_ll64::__u32,
    pub attr: ib_uverbs_ah_attr,
    pub driver_data: [bnd_linux::libc::int_ll64::__u64; 0],
}
impl Default for ib_uverbs_create_ah {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_create_ah_resp {
    pub ah_handle: bnd_linux::libc::int_ll64::__u32,
    pub driver_data: [bnd_linux::libc::int_ll64::__u32; 0],
}
impl Default for ib_uverbs_create_ah_resp {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_create_comp_channel {
    pub response: bnd_linux::libc::int_ll64::__u64,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_create_comp_channel_resp {
    pub fd: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_create_cq {
    pub response: bnd_linux::libc::int_ll64::__u64,
    pub user_handle: bnd_linux::libc::int_ll64::__u64,
    pub cqe: bnd_linux::libc::int_ll64::__u32,
    pub comp_vector: bnd_linux::libc::int_ll64::__u32,
    pub comp_channel: __s32,
    pub reserved: bnd_linux::libc::int_ll64::__u32,
    pub driver_data: [bnd_linux::libc::int_ll64::__u64; 0],
}
impl Default for ib_uverbs_create_cq {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_create_cq_resp {
    pub cq_handle: bnd_linux::libc::int_ll64::__u32,
    pub cqe: bnd_linux::libc::int_ll64::__u32,
    pub driver_data: [bnd_linux::libc::int_ll64::__u64; 0],
}
impl Default for ib_uverbs_create_cq_resp {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_create_flow {
    pub comp_mask: bnd_linux::libc::int_ll64::__u32,
    pub qp_handle: bnd_linux::libc::int_ll64::__u32,
    pub flow_attr: ib_uverbs_flow_attr,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_create_flow_resp {
    pub comp_mask: bnd_linux::libc::int_ll64::__u32,
    pub flow_handle: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_create_qp {
    pub response: bnd_linux::libc::int_ll64::__u64,
    pub user_handle: bnd_linux::libc::int_ll64::__u64,
    pub pd_handle: bnd_linux::libc::int_ll64::__u32,
    pub send_cq_handle: bnd_linux::libc::int_ll64::__u32,
    pub recv_cq_handle: bnd_linux::libc::int_ll64::__u32,
    pub srq_handle: bnd_linux::libc::int_ll64::__u32,
    pub max_send_wr: bnd_linux::libc::int_ll64::__u32,
    pub max_recv_wr: bnd_linux::libc::int_ll64::__u32,
    pub max_send_sge: bnd_linux::libc::int_ll64::__u32,
    pub max_recv_sge: bnd_linux::libc::int_ll64::__u32,
    pub max_inline_data: bnd_linux::libc::int_ll64::__u32,
    pub sq_sig_all: __u8,
    pub qp_type: __u8,
    pub is_srq: __u8,
    pub reserved: __u8,
    pub driver_data: [bnd_linux::libc::int_ll64::__u64; 0],
}
impl Default for ib_uverbs_create_qp {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
pub type ib_uverbs_create_qp_mask = u32;
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_create_qp_resp {
    pub qp_handle: bnd_linux::libc::int_ll64::__u32,
    pub qpn: bnd_linux::libc::int_ll64::__u32,
    pub max_send_wr: bnd_linux::libc::int_ll64::__u32,
    pub max_recv_wr: bnd_linux::libc::int_ll64::__u32,
    pub max_send_sge: bnd_linux::libc::int_ll64::__u32,
    pub max_recv_sge: bnd_linux::libc::int_ll64::__u32,
    pub max_inline_data: bnd_linux::libc::int_ll64::__u32,
    pub reserved: bnd_linux::libc::int_ll64::__u32,
    pub driver_data: [bnd_linux::libc::int_ll64::__u32; 0],
}
impl Default for ib_uverbs_create_qp_resp {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_create_srq {
    pub response: bnd_linux::libc::int_ll64::__u64,
    pub user_handle: bnd_linux::libc::int_ll64::__u64,
    pub pd_handle: bnd_linux::libc::int_ll64::__u32,
    pub max_wr: bnd_linux::libc::int_ll64::__u32,
    pub max_sge: bnd_linux::libc::int_ll64::__u32,
    pub srq_limit: bnd_linux::libc::int_ll64::__u32,
    pub driver_data: [bnd_linux::libc::int_ll64::__u64; 0],
}
impl Default for ib_uverbs_create_srq {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_create_srq_resp {
    pub srq_handle: bnd_linux::libc::int_ll64::__u32,
    pub max_wr: bnd_linux::libc::int_ll64::__u32,
    pub max_sge: bnd_linux::libc::int_ll64::__u32,
    pub srqn: bnd_linux::libc::int_ll64::__u32,
    pub driver_data: [bnd_linux::libc::int_ll64::__u32; 0],
}
impl Default for ib_uverbs_create_srq_resp {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_create_xsrq {
    pub response: bnd_linux::libc::int_ll64::__u64,
    pub user_handle: bnd_linux::libc::int_ll64::__u64,
    pub srq_type: bnd_linux::libc::int_ll64::__u32,
    pub pd_handle: bnd_linux::libc::int_ll64::__u32,
    pub max_wr: bnd_linux::libc::int_ll64::__u32,
    pub max_sge: bnd_linux::libc::int_ll64::__u32,
    pub srq_limit: bnd_linux::libc::int_ll64::__u32,
    pub max_num_tags: bnd_linux::libc::int_ll64::__u32,
    pub xrcd_handle: bnd_linux::libc::int_ll64::__u32,
    pub cq_handle: bnd_linux::libc::int_ll64::__u32,
    pub driver_data: [bnd_linux::libc::int_ll64::__u64; 0],
}
impl Default for ib_uverbs_create_xsrq {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_dealloc_mw {
    pub mw_handle: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_dealloc_pd {
    pub pd_handle: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_dereg_mr {
    pub mr_handle: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_destroy_ah {
    pub ah_handle: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_destroy_cq {
    pub response: bnd_linux::libc::int_ll64::__u64,
    pub cq_handle: bnd_linux::libc::int_ll64::__u32,
    pub reserved: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_destroy_cq_resp {
    pub comp_events_reported: bnd_linux::libc::int_ll64::__u32,
    pub async_events_reported: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_destroy_flow {
    pub comp_mask: bnd_linux::libc::int_ll64::__u32,
    pub flow_handle: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_destroy_qp {
    pub response: bnd_linux::libc::int_ll64::__u64,
    pub qp_handle: bnd_linux::libc::int_ll64::__u32,
    pub reserved: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_destroy_qp_resp {
    pub events_reported: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_destroy_srq {
    pub response: bnd_linux::libc::int_ll64::__u64,
    pub srq_handle: bnd_linux::libc::int_ll64::__u32,
    pub reserved: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_destroy_srq_resp {
    pub events_reported: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_detach_mcast {
    pub gid: [__u8; 16],
    pub qp_handle: bnd_linux::libc::int_ll64::__u32,
    pub mlid: bnd_linux::libc::int_ll64::__u16,
    pub reserved: bnd_linux::libc::int_ll64::__u16,
    pub driver_data: [bnd_linux::libc::int_ll64::__u64; 0],
}
impl Default for ib_uverbs_detach_mcast {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
pub type ib_uverbs_device_cap_flags = u64;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_ex_cmd_hdr {
    pub response: bnd_linux::libc::int_ll64::__u64,
    pub provider_in_words: bnd_linux::libc::int_ll64::__u16,
    pub provider_out_words: bnd_linux::libc::int_ll64::__u16,
    pub cmd_hdr_reserved: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_ex_create_cq {
    pub user_handle: bnd_linux::libc::int_ll64::__u64,
    pub cqe: bnd_linux::libc::int_ll64::__u32,
    pub comp_vector: bnd_linux::libc::int_ll64::__u32,
    pub comp_channel: __s32,
    pub comp_mask: bnd_linux::libc::int_ll64::__u32,
    pub flags: bnd_linux::libc::int_ll64::__u32,
    pub reserved: bnd_linux::libc::int_ll64::__u32,
}
pub type ib_uverbs_ex_create_cq_flags = u32;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_ex_create_cq_resp {
    pub base: ib_uverbs_create_cq_resp,
    pub comp_mask: bnd_linux::libc::int_ll64::__u32,
    pub response_length: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_ex_create_qp {
    pub user_handle: bnd_linux::libc::int_ll64::__u64,
    pub pd_handle: bnd_linux::libc::int_ll64::__u32,
    pub send_cq_handle: bnd_linux::libc::int_ll64::__u32,
    pub recv_cq_handle: bnd_linux::libc::int_ll64::__u32,
    pub srq_handle: bnd_linux::libc::int_ll64::__u32,
    pub max_send_wr: bnd_linux::libc::int_ll64::__u32,
    pub max_recv_wr: bnd_linux::libc::int_ll64::__u32,
    pub max_send_sge: bnd_linux::libc::int_ll64::__u32,
    pub max_recv_sge: bnd_linux::libc::int_ll64::__u32,
    pub max_inline_data: bnd_linux::libc::int_ll64::__u32,
    pub sq_sig_all: __u8,
    pub qp_type: __u8,
    pub is_srq: __u8,
    pub reserved: __u8,
    pub comp_mask: bnd_linux::libc::int_ll64::__u32,
    pub create_flags: bnd_linux::libc::int_ll64::__u32,
    pub rwq_ind_tbl_handle: bnd_linux::libc::int_ll64::__u32,
    pub source_qpn: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_ex_create_qp_resp {
    pub base: ib_uverbs_create_qp_resp,
    pub comp_mask: bnd_linux::libc::int_ll64::__u32,
    pub response_length: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_ex_create_rwq_ind_table {
    pub comp_mask: bnd_linux::libc::int_ll64::__u32,
    pub log_ind_tbl_size: bnd_linux::libc::int_ll64::__u32,
    pub wq_handles: [bnd_linux::libc::int_ll64::__u32; 0],
}
impl Default for ib_uverbs_ex_create_rwq_ind_table {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_ex_create_rwq_ind_table_resp {
    pub comp_mask: bnd_linux::libc::int_ll64::__u32,
    pub response_length: bnd_linux::libc::int_ll64::__u32,
    pub ind_tbl_handle: bnd_linux::libc::int_ll64::__u32,
    pub ind_tbl_num: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_ex_create_wq {
    pub comp_mask: bnd_linux::libc::int_ll64::__u32,
    pub wq_type: bnd_linux::libc::int_ll64::__u32,
    pub user_handle: bnd_linux::libc::int_ll64::__u64,
    pub pd_handle: bnd_linux::libc::int_ll64::__u32,
    pub cq_handle: bnd_linux::libc::int_ll64::__u32,
    pub max_wr: bnd_linux::libc::int_ll64::__u32,
    pub max_sge: bnd_linux::libc::int_ll64::__u32,
    pub create_flags: bnd_linux::libc::int_ll64::__u32,
    pub reserved: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_ex_create_wq_resp {
    pub comp_mask: bnd_linux::libc::int_ll64::__u32,
    pub response_length: bnd_linux::libc::int_ll64::__u32,
    pub wq_handle: bnd_linux::libc::int_ll64::__u32,
    pub max_wr: bnd_linux::libc::int_ll64::__u32,
    pub max_sge: bnd_linux::libc::int_ll64::__u32,
    pub wqn: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_ex_destroy_rwq_ind_table {
    pub comp_mask: bnd_linux::libc::int_ll64::__u32,
    pub ind_tbl_handle: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_ex_destroy_wq {
    pub comp_mask: bnd_linux::libc::int_ll64::__u32,
    pub wq_handle: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_ex_destroy_wq_resp {
    pub comp_mask: bnd_linux::libc::int_ll64::__u32,
    pub response_length: bnd_linux::libc::int_ll64::__u32,
    pub events_reported: bnd_linux::libc::int_ll64::__u32,
    pub reserved: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_ex_modify_cq {
    pub cq_handle: bnd_linux::libc::int_ll64::__u32,
    pub attr_mask: bnd_linux::libc::int_ll64::__u32,
    pub attr: ib_uverbs_cq_moderation,
    pub reserved: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_ex_modify_qp {
    pub base: ib_uverbs_modify_qp,
    pub rate_limit: bnd_linux::libc::int_ll64::__u32,
    pub reserved: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_ex_modify_qp_resp {
    pub comp_mask: bnd_linux::libc::int_ll64::__u32,
    pub response_length: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_ex_modify_wq {
    pub attr_mask: bnd_linux::libc::int_ll64::__u32,
    pub wq_handle: bnd_linux::libc::int_ll64::__u32,
    pub wq_state: bnd_linux::libc::int_ll64::__u32,
    pub curr_wq_state: bnd_linux::libc::int_ll64::__u32,
    pub flags: bnd_linux::libc::int_ll64::__u32,
    pub flags_mask: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_ex_query_device {
    pub comp_mask: bnd_linux::libc::int_ll64::__u32,
    pub reserved: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_ex_query_device_resp {
    pub base: ib_uverbs_query_device_resp,
    pub comp_mask: bnd_linux::libc::int_ll64::__u32,
    pub response_length: bnd_linux::libc::int_ll64::__u32,
    pub odp_caps: ib_uverbs_odp_caps,
    pub timestamp_mask: bnd_linux::libc::int_ll64::__u64,
    pub hca_core_clock: bnd_linux::libc::int_ll64::__u64,
    pub device_cap_flags_ex: bnd_linux::libc::int_ll64::__u64,
    pub rss_caps: ib_uverbs_rss_caps,
    pub max_wq_type_rq: bnd_linux::libc::int_ll64::__u32,
    pub raw_packet_caps: bnd_linux::libc::int_ll64::__u32,
    pub tm_caps: ib_uverbs_tm_caps,
    pub cq_moderation_caps: ib_uverbs_cq_moderation_caps,
    pub max_dm_size: bnd_linux::libc::int_ll64::__u64,
    pub xrc_odp_caps: bnd_linux::libc::int_ll64::__u32,
    pub reserved: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_flow_action_esp {
    pub spi: bnd_linux::libc::int_ll64::__u32,
    pub seq: bnd_linux::libc::int_ll64::__u32,
    pub tfc_pad: bnd_linux::libc::int_ll64::__u32,
    pub flags: bnd_linux::libc::int_ll64::__u32,
    pub hard_limit_pkts: bnd_linux::libc::int_ll64::__u64,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_flow_action_esp_encap {
    pub Anonymous: ib_uverbs_flow_action_esp_encap_0,
    pub Anonymous2: ib_uverbs_flow_action_esp_encap_1,
    pub len: bnd_linux::libc::int_ll64::__u16,
    pub r#type: bnd_linux::libc::int_ll64::__u16,
}
impl Default for ib_uverbs_flow_action_esp_encap {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub union ib_uverbs_flow_action_esp_encap_0 {
    pub val_ptr: *mut core::ffi::c_void,
    pub val_ptr_data_u64: bnd_linux::libc::int_ll64::__u64,
}
impl Default for ib_uverbs_flow_action_esp_encap_0 {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub union ib_uverbs_flow_action_esp_encap_1 {
    pub next_ptr: *mut ib_uverbs_flow_action_esp_encap,
    pub next_ptr_data_u64: bnd_linux::libc::int_ll64::__u64,
}
impl Default for ib_uverbs_flow_action_esp_encap_1 {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
pub type ib_uverbs_flow_action_esp_flags = u32;
pub type ib_uverbs_flow_action_esp_keymat = u32;
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_flow_action_esp_keymat_aes_gcm {
    pub iv: bnd_linux::libc::int_ll64::__u64,
    pub iv_algo: bnd_linux::libc::int_ll64::__u32,
    pub salt: bnd_linux::libc::int_ll64::__u32,
    pub icv_len: bnd_linux::libc::int_ll64::__u32,
    pub key_len: bnd_linux::libc::int_ll64::__u32,
    pub aes_key: [bnd_linux::libc::int_ll64::__u32; 8],
}
impl Default for ib_uverbs_flow_action_esp_keymat_aes_gcm {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
pub type ib_uverbs_flow_action_esp_keymat_aes_gcm_iv_algo = u32;
pub type ib_uverbs_flow_action_esp_replay = u32;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_flow_action_esp_replay_bmp {
    pub size: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_flow_attr {
    pub r#type: bnd_linux::libc::int_ll64::__u32,
    pub size: bnd_linux::libc::int_ll64::__u16,
    pub priority: bnd_linux::libc::int_ll64::__u16,
    pub num_of_specs: __u8,
    pub reserved: [__u8; 2],
    pub port: __u8,
    pub flags: bnd_linux::libc::int_ll64::__u32,
    pub flow_specs: [ib_uverbs_flow_spec_hdr; 0],
}
impl Default for ib_uverbs_flow_attr {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_flow_eth_filter {
    pub dst_mac: [__u8; 6],
    pub src_mac: [__u8; 6],
    pub ether_type: bnd_linux::libc::types::__be16,
    pub vlan_tag: bnd_linux::libc::types::__be16,
}
impl Default for ib_uverbs_flow_eth_filter {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_flow_gre_filter {
    pub c_ks_res0_ver: bnd_linux::libc::types::__be16,
    pub protocol: bnd_linux::libc::types::__be16,
    pub key: bnd_linux::libc::types::__be32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_flow_ipv4_filter {
    pub src_ip: bnd_linux::libc::types::__be32,
    pub dst_ip: bnd_linux::libc::types::__be32,
    pub proto: __u8,
    pub tos: __u8,
    pub ttl: __u8,
    pub flags: __u8,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_flow_ipv6_filter {
    pub src_ip: [__u8; 16],
    pub dst_ip: [__u8; 16],
    pub flow_label: bnd_linux::libc::types::__be32,
    pub next_hdr: __u8,
    pub traffic_class: __u8,
    pub hop_limit: __u8,
    pub reserved: __u8,
}
impl Default for ib_uverbs_flow_ipv6_filter {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_flow_mpls_filter {
    pub label: bnd_linux::libc::types::__be32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_flow_spec_action_count {
    pub Anonymous: ib_uverbs_flow_spec_action_count_0,
    pub handle: bnd_linux::libc::int_ll64::__u32,
    pub reserved1: bnd_linux::libc::int_ll64::__u32,
}
impl Default for ib_uverbs_flow_spec_action_count {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub union ib_uverbs_flow_spec_action_count_0 {
    pub hdr: ib_uverbs_flow_spec_hdr,
    pub Anonymous: ib_uverbs_flow_spec_action_count_0_0,
}
impl Default for ib_uverbs_flow_spec_action_count_0 {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_flow_spec_action_count_0_0 {
    pub r#type: bnd_linux::libc::int_ll64::__u32,
    pub size: bnd_linux::libc::int_ll64::__u16,
    pub reserved: bnd_linux::libc::int_ll64::__u16,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_flow_spec_action_drop {
    pub Anonymous: ib_uverbs_flow_spec_action_drop_0,
}
impl Default for ib_uverbs_flow_spec_action_drop {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub union ib_uverbs_flow_spec_action_drop_0 {
    pub hdr: ib_uverbs_flow_spec_hdr,
    pub Anonymous: ib_uverbs_flow_spec_action_drop_0_0,
}
impl Default for ib_uverbs_flow_spec_action_drop_0 {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_flow_spec_action_drop_0_0 {
    pub r#type: bnd_linux::libc::int_ll64::__u32,
    pub size: bnd_linux::libc::int_ll64::__u16,
    pub reserved: bnd_linux::libc::int_ll64::__u16,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_flow_spec_action_handle {
    pub Anonymous: ib_uverbs_flow_spec_action_handle_0,
    pub handle: bnd_linux::libc::int_ll64::__u32,
    pub reserved1: bnd_linux::libc::int_ll64::__u32,
}
impl Default for ib_uverbs_flow_spec_action_handle {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub union ib_uverbs_flow_spec_action_handle_0 {
    pub hdr: ib_uverbs_flow_spec_hdr,
    pub Anonymous: ib_uverbs_flow_spec_action_handle_0_0,
}
impl Default for ib_uverbs_flow_spec_action_handle_0 {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_flow_spec_action_handle_0_0 {
    pub r#type: bnd_linux::libc::int_ll64::__u32,
    pub size: bnd_linux::libc::int_ll64::__u16,
    pub reserved: bnd_linux::libc::int_ll64::__u16,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_flow_spec_action_tag {
    pub Anonymous: ib_uverbs_flow_spec_action_tag_0,
    pub tag_id: bnd_linux::libc::int_ll64::__u32,
    pub reserved1: bnd_linux::libc::int_ll64::__u32,
}
impl Default for ib_uverbs_flow_spec_action_tag {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub union ib_uverbs_flow_spec_action_tag_0 {
    pub hdr: ib_uverbs_flow_spec_hdr,
    pub Anonymous: ib_uverbs_flow_spec_action_tag_0_0,
}
impl Default for ib_uverbs_flow_spec_action_tag_0 {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_flow_spec_action_tag_0_0 {
    pub r#type: bnd_linux::libc::int_ll64::__u32,
    pub size: bnd_linux::libc::int_ll64::__u16,
    pub reserved: bnd_linux::libc::int_ll64::__u16,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_flow_spec_esp {
    pub Anonymous: ib_uverbs_flow_spec_esp_0,
    pub val: ib_uverbs_flow_spec_esp_filter,
    pub mask: ib_uverbs_flow_spec_esp_filter,
}
impl Default for ib_uverbs_flow_spec_esp {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub union ib_uverbs_flow_spec_esp_0 {
    pub hdr: ib_uverbs_flow_spec_hdr,
    pub Anonymous: ib_uverbs_flow_spec_esp_0_0,
}
impl Default for ib_uverbs_flow_spec_esp_0 {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_flow_spec_esp_0_0 {
    pub r#type: bnd_linux::libc::int_ll64::__u32,
    pub size: bnd_linux::libc::int_ll64::__u16,
    pub reserved: bnd_linux::libc::int_ll64::__u16,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_flow_spec_esp_filter {
    pub spi: bnd_linux::libc::int_ll64::__u32,
    pub seq: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_flow_spec_eth {
    pub Anonymous: ib_uverbs_flow_spec_eth_0,
    pub val: ib_uverbs_flow_eth_filter,
    pub mask: ib_uverbs_flow_eth_filter,
}
impl Default for ib_uverbs_flow_spec_eth {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub union ib_uverbs_flow_spec_eth_0 {
    pub hdr: ib_uverbs_flow_spec_hdr,
    pub Anonymous: ib_uverbs_flow_spec_eth_0_0,
}
impl Default for ib_uverbs_flow_spec_eth_0 {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_flow_spec_eth_0_0 {
    pub r#type: bnd_linux::libc::int_ll64::__u32,
    pub size: bnd_linux::libc::int_ll64::__u16,
    pub reserved: bnd_linux::libc::int_ll64::__u16,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_flow_spec_gre {
    pub Anonymous: ib_uverbs_flow_spec_gre_0,
    pub val: ib_uverbs_flow_gre_filter,
    pub mask: ib_uverbs_flow_gre_filter,
}
impl Default for ib_uverbs_flow_spec_gre {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub union ib_uverbs_flow_spec_gre_0 {
    pub hdr: ib_uverbs_flow_spec_hdr,
    pub Anonymous: ib_uverbs_flow_spec_gre_0_0,
}
impl Default for ib_uverbs_flow_spec_gre_0 {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_flow_spec_gre_0_0 {
    pub r#type: bnd_linux::libc::int_ll64::__u32,
    pub size: bnd_linux::libc::int_ll64::__u16,
    pub reserved: bnd_linux::libc::int_ll64::__u16,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_flow_spec_hdr {
    pub r#type: bnd_linux::libc::int_ll64::__u32,
    pub size: bnd_linux::libc::int_ll64::__u16,
    pub reserved: bnd_linux::libc::int_ll64::__u16,
    pub flow_spec_data: [bnd_linux::libc::int_ll64::__u64; 0],
}
impl Default for ib_uverbs_flow_spec_hdr {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_flow_spec_ipv4 {
    pub Anonymous: ib_uverbs_flow_spec_ipv4_0,
    pub val: ib_uverbs_flow_ipv4_filter,
    pub mask: ib_uverbs_flow_ipv4_filter,
}
impl Default for ib_uverbs_flow_spec_ipv4 {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub union ib_uverbs_flow_spec_ipv4_0 {
    pub hdr: ib_uverbs_flow_spec_hdr,
    pub Anonymous: ib_uverbs_flow_spec_ipv4_0_0,
}
impl Default for ib_uverbs_flow_spec_ipv4_0 {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_flow_spec_ipv4_0_0 {
    pub r#type: bnd_linux::libc::int_ll64::__u32,
    pub size: bnd_linux::libc::int_ll64::__u16,
    pub reserved: bnd_linux::libc::int_ll64::__u16,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_flow_spec_ipv6 {
    pub Anonymous: ib_uverbs_flow_spec_ipv6_0,
    pub val: ib_uverbs_flow_ipv6_filter,
    pub mask: ib_uverbs_flow_ipv6_filter,
}
impl Default for ib_uverbs_flow_spec_ipv6 {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub union ib_uverbs_flow_spec_ipv6_0 {
    pub hdr: ib_uverbs_flow_spec_hdr,
    pub Anonymous: ib_uverbs_flow_spec_ipv6_0_0,
}
impl Default for ib_uverbs_flow_spec_ipv6_0 {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_flow_spec_ipv6_0_0 {
    pub r#type: bnd_linux::libc::int_ll64::__u32,
    pub size: bnd_linux::libc::int_ll64::__u16,
    pub reserved: bnd_linux::libc::int_ll64::__u16,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_flow_spec_mpls {
    pub Anonymous: ib_uverbs_flow_spec_mpls_0,
    pub val: ib_uverbs_flow_mpls_filter,
    pub mask: ib_uverbs_flow_mpls_filter,
}
impl Default for ib_uverbs_flow_spec_mpls {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub union ib_uverbs_flow_spec_mpls_0 {
    pub hdr: ib_uverbs_flow_spec_hdr,
    pub Anonymous: ib_uverbs_flow_spec_mpls_0_0,
}
impl Default for ib_uverbs_flow_spec_mpls_0 {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_flow_spec_mpls_0_0 {
    pub r#type: bnd_linux::libc::int_ll64::__u32,
    pub size: bnd_linux::libc::int_ll64::__u16,
    pub reserved: bnd_linux::libc::int_ll64::__u16,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_flow_spec_tcp_udp {
    pub Anonymous: ib_uverbs_flow_spec_tcp_udp_0,
    pub val: ib_uverbs_flow_tcp_udp_filter,
    pub mask: ib_uverbs_flow_tcp_udp_filter,
}
impl Default for ib_uverbs_flow_spec_tcp_udp {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub union ib_uverbs_flow_spec_tcp_udp_0 {
    pub hdr: ib_uverbs_flow_spec_hdr,
    pub Anonymous: ib_uverbs_flow_spec_tcp_udp_0_0,
}
impl Default for ib_uverbs_flow_spec_tcp_udp_0 {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_flow_spec_tcp_udp_0_0 {
    pub r#type: bnd_linux::libc::int_ll64::__u32,
    pub size: bnd_linux::libc::int_ll64::__u16,
    pub reserved: bnd_linux::libc::int_ll64::__u16,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_flow_spec_tunnel {
    pub Anonymous: ib_uverbs_flow_spec_tunnel_0,
    pub val: ib_uverbs_flow_tunnel_filter,
    pub mask: ib_uverbs_flow_tunnel_filter,
}
impl Default for ib_uverbs_flow_spec_tunnel {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub union ib_uverbs_flow_spec_tunnel_0 {
    pub hdr: ib_uverbs_flow_spec_hdr,
    pub Anonymous: ib_uverbs_flow_spec_tunnel_0_0,
}
impl Default for ib_uverbs_flow_spec_tunnel_0 {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_flow_spec_tunnel_0_0 {
    pub r#type: bnd_linux::libc::int_ll64::__u32,
    pub size: bnd_linux::libc::int_ll64::__u16,
    pub reserved: bnd_linux::libc::int_ll64::__u16,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_flow_tcp_udp_filter {
    pub dst_port: bnd_linux::libc::types::__be16,
    pub src_port: bnd_linux::libc::types::__be16,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_flow_tunnel_filter {
    pub tunnel_id: bnd_linux::libc::types::__be32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_get_context {
    pub response: bnd_linux::libc::int_ll64::__u64,
    pub driver_data: [bnd_linux::libc::int_ll64::__u64; 0],
}
impl Default for ib_uverbs_get_context {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_get_context_resp {
    pub async_fd: bnd_linux::libc::int_ll64::__u32,
    pub num_comp_vectors: bnd_linux::libc::int_ll64::__u32,
    pub driver_data: [bnd_linux::libc::int_ll64::__u64; 0],
}
impl Default for ib_uverbs_get_context_resp {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_gid_entry {
    pub gid: [bnd_linux::libc::int_ll64::__u64; 2],
    pub gid_index: bnd_linux::libc::int_ll64::__u32,
    pub port_num: bnd_linux::libc::int_ll64::__u32,
    pub gid_type: bnd_linux::libc::int_ll64::__u32,
    pub netdev_ifindex: bnd_linux::libc::int_ll64::__u32,
}
impl Default for ib_uverbs_gid_entry {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
pub type ib_uverbs_gid_type = u32;
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_global_route {
    pub dgid: [__u8; 16],
    pub flow_label: bnd_linux::libc::int_ll64::__u32,
    pub sgid_index: __u8,
    pub hop_limit: __u8,
    pub traffic_class: __u8,
    pub reserved: __u8,
}
impl Default for ib_uverbs_global_route {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_modify_qp {
    pub dest: ib_uverbs_qp_dest,
    pub alt_dest: ib_uverbs_qp_dest,
    pub qp_handle: bnd_linux::libc::int_ll64::__u32,
    pub attr_mask: bnd_linux::libc::int_ll64::__u32,
    pub qkey: bnd_linux::libc::int_ll64::__u32,
    pub rq_psn: bnd_linux::libc::int_ll64::__u32,
    pub sq_psn: bnd_linux::libc::int_ll64::__u32,
    pub dest_qp_num: bnd_linux::libc::int_ll64::__u32,
    pub qp_access_flags: bnd_linux::libc::int_ll64::__u32,
    pub pkey_index: bnd_linux::libc::int_ll64::__u16,
    pub alt_pkey_index: bnd_linux::libc::int_ll64::__u16,
    pub qp_state: __u8,
    pub cur_qp_state: __u8,
    pub path_mtu: __u8,
    pub path_mig_state: __u8,
    pub en_sqd_async_notify: __u8,
    pub max_rd_atomic: __u8,
    pub max_dest_rd_atomic: __u8,
    pub min_rnr_timer: __u8,
    pub port_num: __u8,
    pub timeout: __u8,
    pub retry_cnt: __u8,
    pub rnr_retry: __u8,
    pub alt_port_num: __u8,
    pub alt_timeout: __u8,
    pub reserved: [__u8; 2],
    pub driver_data: [bnd_linux::libc::int_ll64::__u64; 0],
}
impl Default for ib_uverbs_modify_qp {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_modify_srq {
    pub srq_handle: bnd_linux::libc::int_ll64::__u32,
    pub attr_mask: bnd_linux::libc::int_ll64::__u32,
    pub max_wr: bnd_linux::libc::int_ll64::__u32,
    pub srq_limit: bnd_linux::libc::int_ll64::__u32,
    pub driver_data: [bnd_linux::libc::int_ll64::__u64; 0],
}
impl Default for ib_uverbs_modify_srq {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_odp_caps {
    pub general_caps: bnd_linux::libc::int_ll64::__u64,
    pub per_transport_caps: ib_uverbs_odp_caps_0,
    pub reserved: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_odp_caps_0 {
    pub rc_odp_caps: bnd_linux::libc::int_ll64::__u32,
    pub uc_odp_caps: bnd_linux::libc::int_ll64::__u32,
    pub ud_odp_caps: bnd_linux::libc::int_ll64::__u32,
}
pub type ib_uverbs_odp_general_cap_bits = u32;
pub type ib_uverbs_odp_transport_cap_bits = u32;
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_open_qp {
    pub response: bnd_linux::libc::int_ll64::__u64,
    pub user_handle: bnd_linux::libc::int_ll64::__u64,
    pub pd_handle: bnd_linux::libc::int_ll64::__u32,
    pub qpn: bnd_linux::libc::int_ll64::__u32,
    pub qp_type: __u8,
    pub reserved: [__u8; 7],
    pub driver_data: [bnd_linux::libc::int_ll64::__u64; 0],
}
impl Default for ib_uverbs_open_qp {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_open_xrcd {
    pub response: bnd_linux::libc::int_ll64::__u64,
    pub fd: bnd_linux::libc::int_ll64::__u32,
    pub oflags: bnd_linux::libc::int_ll64::__u32,
    pub driver_data: [bnd_linux::libc::int_ll64::__u64; 0],
}
impl Default for ib_uverbs_open_xrcd {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_open_xrcd_resp {
    pub xrcd_handle: bnd_linux::libc::int_ll64::__u32,
    pub driver_data: [bnd_linux::libc::int_ll64::__u32; 0],
}
impl Default for ib_uverbs_open_xrcd_resp {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_poll_cq {
    pub response: bnd_linux::libc::int_ll64::__u64,
    pub cq_handle: bnd_linux::libc::int_ll64::__u32,
    pub ne: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_poll_cq_resp {
    pub count: bnd_linux::libc::int_ll64::__u32,
    pub reserved: bnd_linux::libc::int_ll64::__u32,
    pub wc: [ib_uverbs_wc; 0],
}
impl Default for ib_uverbs_poll_cq_resp {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_post_recv {
    pub response: bnd_linux::libc::int_ll64::__u64,
    pub qp_handle: bnd_linux::libc::int_ll64::__u32,
    pub wr_count: bnd_linux::libc::int_ll64::__u32,
    pub sge_count: bnd_linux::libc::int_ll64::__u32,
    pub wqe_size: bnd_linux::libc::int_ll64::__u32,
    pub recv_wr: [ib_uverbs_recv_wr; 0],
}
impl Default for ib_uverbs_post_recv {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_post_recv_resp {
    pub bad_wr: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_post_send {
    pub response: bnd_linux::libc::int_ll64::__u64,
    pub qp_handle: bnd_linux::libc::int_ll64::__u32,
    pub wr_count: bnd_linux::libc::int_ll64::__u32,
    pub sge_count: bnd_linux::libc::int_ll64::__u32,
    pub wqe_size: bnd_linux::libc::int_ll64::__u32,
    pub send_wr: [ib_uverbs_send_wr; 0],
}
impl Default for ib_uverbs_post_send {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_post_send_resp {
    pub bad_wr: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_post_srq_recv {
    pub response: bnd_linux::libc::int_ll64::__u64,
    pub srq_handle: bnd_linux::libc::int_ll64::__u32,
    pub wr_count: bnd_linux::libc::int_ll64::__u32,
    pub sge_count: bnd_linux::libc::int_ll64::__u32,
    pub wqe_size: bnd_linux::libc::int_ll64::__u32,
    pub recv: [ib_uverbs_recv_wr; 0],
}
impl Default for ib_uverbs_post_srq_recv {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_post_srq_recv_resp {
    pub bad_wr: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_qp_attr {
    pub qp_attr_mask: bnd_linux::libc::int_ll64::__u32,
    pub qp_state: bnd_linux::libc::int_ll64::__u32,
    pub cur_qp_state: bnd_linux::libc::int_ll64::__u32,
    pub path_mtu: bnd_linux::libc::int_ll64::__u32,
    pub path_mig_state: bnd_linux::libc::int_ll64::__u32,
    pub qkey: bnd_linux::libc::int_ll64::__u32,
    pub rq_psn: bnd_linux::libc::int_ll64::__u32,
    pub sq_psn: bnd_linux::libc::int_ll64::__u32,
    pub dest_qp_num: bnd_linux::libc::int_ll64::__u32,
    pub qp_access_flags: bnd_linux::libc::int_ll64::__u32,
    pub ah_attr: ib_uverbs_ah_attr,
    pub alt_ah_attr: ib_uverbs_ah_attr,
    pub max_send_wr: bnd_linux::libc::int_ll64::__u32,
    pub max_recv_wr: bnd_linux::libc::int_ll64::__u32,
    pub max_send_sge: bnd_linux::libc::int_ll64::__u32,
    pub max_recv_sge: bnd_linux::libc::int_ll64::__u32,
    pub max_inline_data: bnd_linux::libc::int_ll64::__u32,
    pub pkey_index: bnd_linux::libc::int_ll64::__u16,
    pub alt_pkey_index: bnd_linux::libc::int_ll64::__u16,
    pub en_sqd_async_notify: __u8,
    pub sq_draining: __u8,
    pub max_rd_atomic: __u8,
    pub max_dest_rd_atomic: __u8,
    pub min_rnr_timer: __u8,
    pub port_num: __u8,
    pub timeout: __u8,
    pub retry_cnt: __u8,
    pub rnr_retry: __u8,
    pub alt_port_num: __u8,
    pub alt_timeout: __u8,
    pub reserved: [__u8; 5],
}
impl Default for ib_uverbs_qp_attr {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_qp_cap {
    pub max_send_wr: bnd_linux::libc::int_ll64::__u32,
    pub max_recv_wr: bnd_linux::libc::int_ll64::__u32,
    pub max_send_sge: bnd_linux::libc::int_ll64::__u32,
    pub max_recv_sge: bnd_linux::libc::int_ll64::__u32,
    pub max_inline_data: bnd_linux::libc::int_ll64::__u32,
}
pub type ib_uverbs_qp_create_flags = u32;
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_qp_dest {
    pub dgid: [__u8; 16],
    pub flow_label: bnd_linux::libc::int_ll64::__u32,
    pub dlid: bnd_linux::libc::int_ll64::__u16,
    pub reserved: bnd_linux::libc::int_ll64::__u16,
    pub sgid_index: __u8,
    pub hop_limit: __u8,
    pub traffic_class: __u8,
    pub sl: __u8,
    pub src_path_bits: __u8,
    pub static_rate: __u8,
    pub is_global: __u8,
    pub port_num: __u8,
}
impl Default for ib_uverbs_qp_dest {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
pub type ib_uverbs_qp_type = u32;
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_query_device {
    pub response: bnd_linux::libc::int_ll64::__u64,
    pub driver_data: [bnd_linux::libc::int_ll64::__u64; 0],
}
impl Default for ib_uverbs_query_device {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_query_device_resp {
    pub fw_ver: bnd_linux::libc::int_ll64::__u64,
    pub node_guid: bnd_linux::libc::types::__be64,
    pub sys_image_guid: bnd_linux::libc::types::__be64,
    pub max_mr_size: bnd_linux::libc::int_ll64::__u64,
    pub page_size_cap: bnd_linux::libc::int_ll64::__u64,
    pub vendor_id: bnd_linux::libc::int_ll64::__u32,
    pub vendor_part_id: bnd_linux::libc::int_ll64::__u32,
    pub hw_ver: bnd_linux::libc::int_ll64::__u32,
    pub max_qp: bnd_linux::libc::int_ll64::__u32,
    pub max_qp_wr: bnd_linux::libc::int_ll64::__u32,
    pub device_cap_flags: bnd_linux::libc::int_ll64::__u32,
    pub max_sge: bnd_linux::libc::int_ll64::__u32,
    pub max_sge_rd: bnd_linux::libc::int_ll64::__u32,
    pub max_cq: bnd_linux::libc::int_ll64::__u32,
    pub max_cqe: bnd_linux::libc::int_ll64::__u32,
    pub max_mr: bnd_linux::libc::int_ll64::__u32,
    pub max_pd: bnd_linux::libc::int_ll64::__u32,
    pub max_qp_rd_atom: bnd_linux::libc::int_ll64::__u32,
    pub max_ee_rd_atom: bnd_linux::libc::int_ll64::__u32,
    pub max_res_rd_atom: bnd_linux::libc::int_ll64::__u32,
    pub max_qp_init_rd_atom: bnd_linux::libc::int_ll64::__u32,
    pub max_ee_init_rd_atom: bnd_linux::libc::int_ll64::__u32,
    pub atomic_cap: bnd_linux::libc::int_ll64::__u32,
    pub max_ee: bnd_linux::libc::int_ll64::__u32,
    pub max_rdd: bnd_linux::libc::int_ll64::__u32,
    pub max_mw: bnd_linux::libc::int_ll64::__u32,
    pub max_raw_ipv6_qp: bnd_linux::libc::int_ll64::__u32,
    pub max_raw_ethy_qp: bnd_linux::libc::int_ll64::__u32,
    pub max_mcast_grp: bnd_linux::libc::int_ll64::__u32,
    pub max_mcast_qp_attach: bnd_linux::libc::int_ll64::__u32,
    pub max_total_mcast_qp_attach: bnd_linux::libc::int_ll64::__u32,
    pub max_ah: bnd_linux::libc::int_ll64::__u32,
    pub max_fmr: bnd_linux::libc::int_ll64::__u32,
    pub max_map_per_fmr: bnd_linux::libc::int_ll64::__u32,
    pub max_srq: bnd_linux::libc::int_ll64::__u32,
    pub max_srq_wr: bnd_linux::libc::int_ll64::__u32,
    pub max_srq_sge: bnd_linux::libc::int_ll64::__u32,
    pub max_pkeys: bnd_linux::libc::int_ll64::__u16,
    pub local_ca_ack_delay: __u8,
    pub phys_port_cnt: __u8,
    pub reserved: [__u8; 4],
}
impl Default for ib_uverbs_query_device_resp {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_query_port {
    pub response: bnd_linux::libc::int_ll64::__u64,
    pub port_num: __u8,
    pub reserved: [__u8; 7],
    pub driver_data: [bnd_linux::libc::int_ll64::__u64; 0],
}
impl Default for ib_uverbs_query_port {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
pub type ib_uverbs_query_port_cap_flags = u32;
pub type ib_uverbs_query_port_flags = u32;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_query_port_resp {
    pub port_cap_flags: bnd_linux::libc::int_ll64::__u32,
    pub max_msg_sz: bnd_linux::libc::int_ll64::__u32,
    pub bad_pkey_cntr: bnd_linux::libc::int_ll64::__u32,
    pub qkey_viol_cntr: bnd_linux::libc::int_ll64::__u32,
    pub gid_tbl_len: bnd_linux::libc::int_ll64::__u32,
    pub pkey_tbl_len: bnd_linux::libc::int_ll64::__u16,
    pub lid: bnd_linux::libc::int_ll64::__u16,
    pub sm_lid: bnd_linux::libc::int_ll64::__u16,
    pub state: __u8,
    pub max_mtu: __u8,
    pub active_mtu: __u8,
    pub lmc: __u8,
    pub max_vl_num: __u8,
    pub sm_sl: __u8,
    pub subnet_timeout: __u8,
    pub init_type_reply: __u8,
    pub active_width: __u8,
    pub active_speed: __u8,
    pub phys_state: __u8,
    pub link_layer: __u8,
    pub flags: __u8,
    pub reserved: __u8,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_query_port_resp_ex {
    pub legacy_resp: ib_uverbs_query_port_resp,
    pub port_cap_flags2: bnd_linux::libc::int_ll64::__u16,
    pub reserved: [__u8; 2],
    pub active_speed_ex: bnd_linux::libc::int_ll64::__u32,
}
impl Default for ib_uverbs_query_port_resp_ex {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_query_qp {
    pub response: bnd_linux::libc::int_ll64::__u64,
    pub qp_handle: bnd_linux::libc::int_ll64::__u32,
    pub attr_mask: bnd_linux::libc::int_ll64::__u32,
    pub driver_data: [bnd_linux::libc::int_ll64::__u64; 0],
}
impl Default for ib_uverbs_query_qp {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_query_qp_resp {
    pub dest: ib_uverbs_qp_dest,
    pub alt_dest: ib_uverbs_qp_dest,
    pub max_send_wr: bnd_linux::libc::int_ll64::__u32,
    pub max_recv_wr: bnd_linux::libc::int_ll64::__u32,
    pub max_send_sge: bnd_linux::libc::int_ll64::__u32,
    pub max_recv_sge: bnd_linux::libc::int_ll64::__u32,
    pub max_inline_data: bnd_linux::libc::int_ll64::__u32,
    pub qkey: bnd_linux::libc::int_ll64::__u32,
    pub rq_psn: bnd_linux::libc::int_ll64::__u32,
    pub sq_psn: bnd_linux::libc::int_ll64::__u32,
    pub dest_qp_num: bnd_linux::libc::int_ll64::__u32,
    pub qp_access_flags: bnd_linux::libc::int_ll64::__u32,
    pub pkey_index: bnd_linux::libc::int_ll64::__u16,
    pub alt_pkey_index: bnd_linux::libc::int_ll64::__u16,
    pub qp_state: __u8,
    pub cur_qp_state: __u8,
    pub path_mtu: __u8,
    pub path_mig_state: __u8,
    pub sq_draining: __u8,
    pub max_rd_atomic: __u8,
    pub max_dest_rd_atomic: __u8,
    pub min_rnr_timer: __u8,
    pub port_num: __u8,
    pub timeout: __u8,
    pub retry_cnt: __u8,
    pub rnr_retry: __u8,
    pub alt_port_num: __u8,
    pub alt_timeout: __u8,
    pub sq_sig_all: __u8,
    pub reserved: [__u8; 5],
    pub driver_data: [bnd_linux::libc::int_ll64::__u64; 0],
}
impl Default for ib_uverbs_query_qp_resp {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_query_srq {
    pub response: bnd_linux::libc::int_ll64::__u64,
    pub srq_handle: bnd_linux::libc::int_ll64::__u32,
    pub reserved: bnd_linux::libc::int_ll64::__u32,
    pub driver_data: [bnd_linux::libc::int_ll64::__u64; 0],
}
impl Default for ib_uverbs_query_srq {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_query_srq_resp {
    pub max_wr: bnd_linux::libc::int_ll64::__u32,
    pub max_sge: bnd_linux::libc::int_ll64::__u32,
    pub srq_limit: bnd_linux::libc::int_ll64::__u32,
    pub reserved: bnd_linux::libc::int_ll64::__u32,
}
pub type ib_uverbs_raw_packet_caps = u32;
pub type ib_uverbs_read_counters_flags = u32;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_recv_wr {
    pub wr_id: bnd_linux::libc::int_ll64::__u64,
    pub num_sge: bnd_linux::libc::int_ll64::__u32,
    pub reserved: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_reg_mr {
    pub response: bnd_linux::libc::int_ll64::__u64,
    pub start: bnd_linux::libc::int_ll64::__u64,
    pub length: bnd_linux::libc::int_ll64::__u64,
    pub hca_va: bnd_linux::libc::int_ll64::__u64,
    pub pd_handle: bnd_linux::libc::int_ll64::__u32,
    pub access_flags: bnd_linux::libc::int_ll64::__u32,
    pub driver_data: [bnd_linux::libc::int_ll64::__u64; 0],
}
impl Default for ib_uverbs_reg_mr {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_reg_mr_resp {
    pub mr_handle: bnd_linux::libc::int_ll64::__u32,
    pub lkey: bnd_linux::libc::int_ll64::__u32,
    pub rkey: bnd_linux::libc::int_ll64::__u32,
    pub driver_data: [bnd_linux::libc::int_ll64::__u32; 0],
}
impl Default for ib_uverbs_reg_mr_resp {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_req_notify_cq {
    pub cq_handle: bnd_linux::libc::int_ll64::__u32,
    pub solicited_only: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_rereg_mr {
    pub response: bnd_linux::libc::int_ll64::__u64,
    pub mr_handle: bnd_linux::libc::int_ll64::__u32,
    pub flags: bnd_linux::libc::int_ll64::__u32,
    pub start: bnd_linux::libc::int_ll64::__u64,
    pub length: bnd_linux::libc::int_ll64::__u64,
    pub hca_va: bnd_linux::libc::int_ll64::__u64,
    pub pd_handle: bnd_linux::libc::int_ll64::__u32,
    pub access_flags: bnd_linux::libc::int_ll64::__u32,
    pub driver_data: [bnd_linux::libc::int_ll64::__u64; 0],
}
impl Default for ib_uverbs_rereg_mr {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_rereg_mr_resp {
    pub lkey: bnd_linux::libc::int_ll64::__u32,
    pub rkey: bnd_linux::libc::int_ll64::__u32,
    pub driver_data: [bnd_linux::libc::int_ll64::__u64; 0],
}
impl Default for ib_uverbs_rereg_mr_resp {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_resize_cq {
    pub response: bnd_linux::libc::int_ll64::__u64,
    pub cq_handle: bnd_linux::libc::int_ll64::__u32,
    pub cqe: bnd_linux::libc::int_ll64::__u32,
    pub driver_data: [bnd_linux::libc::int_ll64::__u64; 0],
}
impl Default for ib_uverbs_resize_cq {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_resize_cq_resp {
    pub cqe: bnd_linux::libc::int_ll64::__u32,
    pub reserved: bnd_linux::libc::int_ll64::__u32,
    pub driver_data: [bnd_linux::libc::int_ll64::__u64; 0],
}
impl Default for ib_uverbs_resize_cq_resp {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_rss_caps {
    pub supported_qpts: bnd_linux::libc::int_ll64::__u32,
    pub max_rwq_indirection_tables: bnd_linux::libc::int_ll64::__u32,
    pub max_rwq_indirection_table_size: bnd_linux::libc::int_ll64::__u32,
    pub reserved: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_send_wr {
    pub wr_id: bnd_linux::libc::int_ll64::__u64,
    pub num_sge: bnd_linux::libc::int_ll64::__u32,
    pub opcode: bnd_linux::libc::int_ll64::__u32,
    pub send_flags: bnd_linux::libc::int_ll64::__u32,
    pub ex: ib_uverbs_send_wr_0,
    pub wr: ib_uverbs_send_wr_1,
}
impl Default for ib_uverbs_send_wr {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub union ib_uverbs_send_wr_0 {
    pub imm_data: bnd_linux::libc::types::__be32,
    pub invalidate_rkey: bnd_linux::libc::int_ll64::__u32,
}
impl Default for ib_uverbs_send_wr_0 {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub union ib_uverbs_send_wr_1 {
    pub rdma: ib_uverbs_send_wr_1_0,
    pub atomic: ib_uverbs_send_wr_1_1,
    pub ud: ib_uverbs_send_wr_1_2,
}
impl Default for ib_uverbs_send_wr_1 {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_send_wr_1_0 {
    pub remote_addr: bnd_linux::libc::int_ll64::__u64,
    pub rkey: bnd_linux::libc::int_ll64::__u32,
    pub reserved: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_send_wr_1_1 {
    pub remote_addr: bnd_linux::libc::int_ll64::__u64,
    pub compare_add: bnd_linux::libc::int_ll64::__u64,
    pub swap: bnd_linux::libc::int_ll64::__u64,
    pub rkey: bnd_linux::libc::int_ll64::__u32,
    pub reserved: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_send_wr_1_2 {
    pub ah: bnd_linux::libc::int_ll64::__u32,
    pub remote_qpn: bnd_linux::libc::int_ll64::__u32,
    pub remote_qkey: bnd_linux::libc::int_ll64::__u32,
    pub reserved: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_sge {
    pub addr: bnd_linux::libc::int_ll64::__u64,
    pub length: bnd_linux::libc::int_ll64::__u32,
    pub lkey: bnd_linux::libc::int_ll64::__u32,
}
pub type ib_uverbs_srq_type = u32;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_tm_caps {
    pub max_rndv_hdr_size: bnd_linux::libc::int_ll64::__u32,
    pub max_num_tags: bnd_linux::libc::int_ll64::__u32,
    pub flags: bnd_linux::libc::int_ll64::__u32,
    pub max_ops: bnd_linux::libc::int_ll64::__u32,
    pub max_sge: bnd_linux::libc::int_ll64::__u32,
    pub reserved: bnd_linux::libc::int_ll64::__u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ib_uverbs_wc {
    pub wr_id: bnd_linux::libc::int_ll64::__u64,
    pub status: bnd_linux::libc::int_ll64::__u32,
    pub opcode: bnd_linux::libc::int_ll64::__u32,
    pub vendor_err: bnd_linux::libc::int_ll64::__u32,
    pub byte_len: bnd_linux::libc::int_ll64::__u32,
    pub ex: ib_uverbs_wc_0,
    pub qp_num: bnd_linux::libc::int_ll64::__u32,
    pub src_qp: bnd_linux::libc::int_ll64::__u32,
    pub wc_flags: bnd_linux::libc::int_ll64::__u32,
    pub pkey_index: bnd_linux::libc::int_ll64::__u16,
    pub slid: bnd_linux::libc::int_ll64::__u16,
    pub sl: __u8,
    pub dlid_path_bits: __u8,
    pub port_num: __u8,
    pub reserved: __u8,
}
impl Default for ib_uverbs_wc {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub union ib_uverbs_wc_0 {
    pub imm_data: bnd_linux::libc::types::__be32,
    pub invalidate_rkey: bnd_linux::libc::int_ll64::__u32,
}
impl Default for ib_uverbs_wc_0 {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
pub type ib_uverbs_wc_opcode = u32;
pub type ib_uverbs_wq_flags = u32;
pub type ib_uverbs_wq_type = u32;
pub type ib_uverbs_wr_opcode = u32;
pub type ib_uverbs_write_cmds = u32;
pub type ibv_access_flags = u32;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_ah {
    pub context: *mut ibv_context,
    pub pd: *mut ibv_pd,
    pub handle: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ibv_ah_attr {
    pub grh: ibv_global_route,
    pub dlid: u16,
    pub sl: u8,
    pub src_path_bits: u8,
    pub static_rate: u8,
    pub is_global: u8,
    pub port_num: u8,
}
impl Default for ibv_ah_attr {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_alloc_dm_attr {
    pub length: usize,
    pub log_align_req: u32,
    pub comp_mask: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ibv_async_event {
    pub element: ibv_async_event_0,
    pub event_type: ibv_event_type,
}
impl Default for ibv_async_event {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub union ibv_async_event_0 {
    pub cq: *mut ibv_cq,
    pub qp: *mut ibv_qp,
    pub srq: *mut ibv_srq,
    pub wq: *mut ibv_wq,
    pub port_num: i32,
}
impl Default for ibv_async_event_0 {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
pub type ibv_atomic_cap = u32;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_comp_channel {
    pub context: *mut ibv_context,
    pub fd: i32,
    pub refcnt: i32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ibv_context {
    pub device: *mut ibv_device,
    pub ops: ibv_context_ops,
    pub cmd_fd: i32,
    pub async_fd: i32,
    pub num_comp_vectors: i32,
    pub mutex: bnd_linux::libc::pthreadtypes::pthread_mutex_t,
    pub abi_compat: *mut core::ffi::c_void,
}
impl Default for ibv_context {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_context_ops {
    pub _compat_query_device: *mut u8,
    pub _compat_query_port: *mut u8,
    pub _compat_alloc_pd: *mut u8,
    pub _compat_dealloc_pd: *mut u8,
    pub _compat_reg_mr: *mut u8,
    pub _compat_rereg_mr: *mut u8,
    pub _compat_dereg_mr: *mut u8,
    pub alloc_mw: *mut u8,
    pub bind_mw: *mut u8,
    pub dealloc_mw: *mut u8,
    pub _compat_create_cq: *mut u8,
    pub poll_cq: *mut u8,
    pub req_notify_cq: *mut u8,
    pub _compat_cq_event: *mut u8,
    pub _compat_resize_cq: *mut u8,
    pub _compat_destroy_cq: *mut u8,
    pub _compat_create_srq: *mut u8,
    pub _compat_modify_srq: *mut u8,
    pub _compat_query_srq: *mut u8,
    pub _compat_destroy_srq: *mut u8,
    pub post_srq_recv: *mut u8,
    pub _compat_create_qp: *mut u8,
    pub _compat_query_qp: *mut u8,
    pub _compat_modify_qp: *mut u8,
    pub _compat_destroy_qp: *mut u8,
    pub post_send: *mut u8,
    pub post_recv: *mut u8,
    pub _compat_create_ah: *mut u8,
    pub _compat_destroy_ah: *mut u8,
    pub _compat_attach_mcast: *mut u8,
    pub _compat_detach_mcast: *mut u8,
    pub _compat_async_event: *mut u8,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_counter_attach_attr {
    pub counter_desc: ibv_counter_description,
    pub index: u32,
    pub comp_mask: u32,
}
pub type ibv_counter_description = u32;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_counters {
    pub context: *mut ibv_context,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_counters_init_attr {
    pub comp_mask: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ibv_cq {
    pub context: *mut ibv_context,
    pub channel: *mut ibv_comp_channel,
    pub cq_context: *mut core::ffi::c_void,
    pub handle: u32,
    pub cqe: i32,
    pub mutex: bnd_linux::libc::pthreadtypes::pthread_mutex_t,
    pub cond: bnd_linux::libc::pthreadtypes::pthread_cond_t,
    pub comp_events_completed: u32,
    pub async_events_completed: u32,
}
impl Default for ibv_cq {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
pub type ibv_cq_attr_mask = u32;
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ibv_cq_ex {
    pub context: *mut ibv_context,
    pub channel: *mut ibv_comp_channel,
    pub cq_context: *mut core::ffi::c_void,
    pub handle: u32,
    pub cqe: i32,
    pub mutex: bnd_linux::libc::pthreadtypes::pthread_mutex_t,
    pub cond: bnd_linux::libc::pthreadtypes::pthread_cond_t,
    pub comp_events_completed: u32,
    pub async_events_completed: u32,
    pub comp_mask: u32,
    pub status: ibv_wc_status,
    pub wr_id: u64,
    pub start_poll: *mut u8,
    pub next_poll: *mut u8,
    pub end_poll: *mut u8,
    pub read_opcode: *mut u8,
    pub read_vendor_err: *mut u8,
    pub read_byte_len: *mut u8,
    pub read_imm_data: *mut u8,
    pub read_qp_num: *mut u8,
    pub read_src_qp: *mut u8,
    pub read_wc_flags: *mut u8,
    pub read_slid: *mut u8,
    pub read_sl: *mut u8,
    pub read_dlid_path_bits: *mut u8,
    pub read_completion_ts: *mut u8,
    pub read_cvlan: *mut u8,
    pub read_flow_tag: *mut u8,
    pub read_tm_info: *mut u8,
    pub read_completion_wallclock_ns: *mut u8,
}
impl Default for ibv_cq_ex {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_cq_init_attr_ex {
    pub cqe: u32,
    pub cq_context: *mut core::ffi::c_void,
    pub channel: *mut ibv_comp_channel,
    pub comp_vector: u32,
    pub wc_flags: u64,
    pub comp_mask: u32,
    pub flags: u32,
    pub parent_domain: *mut ibv_pd,
}
pub type ibv_cq_init_attr_mask = u32;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_cq_moderation_caps {
    pub max_cq_count: u16,
    pub max_cq_period: u16,
}
pub type ibv_create_cq_attr_flags = u32;
pub type ibv_create_cq_wc_flags = u32;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_data_buf {
    pub addr: *mut core::ffi::c_void,
    pub length: usize,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ibv_device {
    pub _ops: _ibv_device_ops,
    pub node_type: ibv_node_type,
    pub transport_type: ibv_transport_type,
    pub name: [i8; 64],
    pub dev_name: [i8; 64],
    pub dev_path: [i8; 256],
    pub ibdev_path: [i8; 256],
}
impl Default for ibv_device {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ibv_device_attr {
    pub fw_ver: [i8; 64],
    pub node_guid: bnd_linux::libc::types::__be64,
    pub sys_image_guid: bnd_linux::libc::types::__be64,
    pub max_mr_size: u64,
    pub page_size_cap: u64,
    pub vendor_id: u32,
    pub vendor_part_id: u32,
    pub hw_ver: u32,
    pub max_qp: i32,
    pub max_qp_wr: i32,
    pub device_cap_flags: u32,
    pub max_sge: i32,
    pub max_sge_rd: i32,
    pub max_cq: i32,
    pub max_cqe: i32,
    pub max_mr: i32,
    pub max_pd: i32,
    pub max_qp_rd_atom: i32,
    pub max_ee_rd_atom: i32,
    pub max_res_rd_atom: i32,
    pub max_qp_init_rd_atom: i32,
    pub max_ee_init_rd_atom: i32,
    pub atomic_cap: ibv_atomic_cap,
    pub max_ee: i32,
    pub max_rdd: i32,
    pub max_mw: i32,
    pub max_raw_ipv6_qp: i32,
    pub max_raw_ethy_qp: i32,
    pub max_mcast_grp: i32,
    pub max_mcast_qp_attach: i32,
    pub max_total_mcast_qp_attach: i32,
    pub max_ah: i32,
    pub max_fmr: i32,
    pub max_map_per_fmr: i32,
    pub max_srq: i32,
    pub max_srq_wr: i32,
    pub max_srq_sge: i32,
    pub max_pkeys: u16,
    pub local_ca_ack_delay: u8,
    pub phys_port_cnt: u8,
}
impl Default for ibv_device_attr {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_device_attr_ex {
    pub orig_attr: ibv_device_attr,
    pub comp_mask: u32,
    pub odp_caps: ibv_odp_caps,
    pub completion_timestamp_mask: u64,
    pub hca_core_clock: u64,
    pub device_cap_flags_ex: u64,
    pub tso_caps: ibv_tso_caps,
    pub rss_caps: ibv_rss_caps,
    pub max_wq_type_rq: u32,
    pub packet_pacing_caps: ibv_packet_pacing_caps,
    pub raw_packet_caps: u32,
    pub tm_caps: ibv_tm_caps,
    pub cq_mod_caps: ibv_cq_moderation_caps,
    pub max_dm_size: u64,
    pub pci_atomic_caps: ibv_pci_atomic_caps,
    pub xrc_odp_caps: u32,
    pub phys_port_cnt_ex: u32,
}
pub type ibv_device_cap_flags = u32;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_dm {
    pub context: *mut ibv_context,
    pub memcpy_to_dm: *mut u8,
    pub memcpy_from_dm: *mut u8,
    pub comp_mask: u32,
    pub handle: u32,
}
pub type ibv_dm_mask = u32;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_dmah {
    pub context: *mut ibv_context,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_dmah_init_attr {
    pub comp_mask: u32,
    pub cpu_id: u32,
    pub ph: u8,
    pub tph_mem_type: u8,
}
pub type ibv_dmah_init_attr_mask = u32;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_ece {
    pub vendor_id: u32,
    pub options: u32,
    pub comp_mask: u32,
}
pub type ibv_event_type = u32;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_fd_arr {
    pub arr: *mut i32,
    pub count: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_flow {
    pub comp_mask: u32,
    pub context: *mut ibv_context,
    pub handle: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_flow_action {
    pub context: *mut ibv_context,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_flow_action_esp_attr {
    pub esp_attr: *mut ib_uverbs_flow_action_esp,
    pub keymat_proto: ib_uverbs_flow_action_esp_keymat,
    pub keymat_len: u16,
    pub keymat_ptr: *mut core::ffi::c_void,
    pub replay_proto: ib_uverbs_flow_action_esp_replay,
    pub replay_len: u16,
    pub replay_ptr: *mut core::ffi::c_void,
    pub esp_encap: *mut ib_uverbs_flow_action_esp_encap,
    pub comp_mask: u32,
    pub esn: u32,
}
pub type ibv_flow_action_esp_mask = u32;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_flow_attr {
    pub comp_mask: u32,
    pub r#type: ibv_flow_attr_type,
    pub size: u16,
    pub priority: u16,
    pub num_of_specs: u8,
    pub port: u8,
    pub flags: u32,
}
pub type ibv_flow_attr_type = u32;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_flow_esp_filter {
    pub spi: u32,
    pub seq: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ibv_flow_eth_filter {
    pub dst_mac: [u8; 6],
    pub src_mac: [u8; 6],
    pub ether_type: u16,
    pub vlan_tag: u16,
}
impl Default for ibv_flow_eth_filter {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
pub type ibv_flow_flags = u32;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_flow_gre_filter {
    pub c_ks_res0_ver: u16,
    pub protocol: u16,
    pub key: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_flow_ipv4_ext_filter {
    pub src_ip: u32,
    pub dst_ip: u32,
    pub proto: u8,
    pub tos: u8,
    pub ttl: u8,
    pub flags: u8,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_flow_ipv4_filter {
    pub src_ip: u32,
    pub dst_ip: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ibv_flow_ipv6_filter {
    pub src_ip: [u8; 16],
    pub dst_ip: [u8; 16],
    pub flow_label: u32,
    pub next_hdr: u8,
    pub traffic_class: u8,
    pub hop_limit: u8,
}
impl Default for ibv_flow_ipv6_filter {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_flow_mpls_filter {
    pub label: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ibv_flow_spec {
    pub Anonymous: ibv_flow_spec_0,
}
impl Default for ibv_flow_spec {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub union ibv_flow_spec_0 {
    pub hdr: ibv_flow_spec_0_0,
    pub eth: ibv_flow_spec_eth,
    pub ipv4: ibv_flow_spec_ipv4,
    pub tcp_udp: ibv_flow_spec_tcp_udp,
    pub ipv4_ext: ibv_flow_spec_ipv4_ext,
    pub ipv6: ibv_flow_spec_ipv6,
    pub esp: ibv_flow_spec_esp,
    pub tunnel: ibv_flow_spec_tunnel,
    pub gre: ibv_flow_spec_gre,
    pub mpls: ibv_flow_spec_mpls,
    pub flow_tag: ibv_flow_spec_action_tag,
    pub drop: ibv_flow_spec_action_drop,
    pub handle: ibv_flow_spec_action_handle,
    pub flow_count: ibv_flow_spec_counter_action,
}
impl Default for ibv_flow_spec_0 {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_flow_spec_0_0 {
    pub r#type: ibv_flow_spec_type,
    pub size: u16,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_flow_spec_action_drop {
    pub r#type: ibv_flow_spec_type,
    pub size: u16,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_flow_spec_action_handle {
    pub r#type: ibv_flow_spec_type,
    pub size: u16,
    pub action: *const ibv_flow_action,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_flow_spec_action_tag {
    pub r#type: ibv_flow_spec_type,
    pub size: u16,
    pub tag_id: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_flow_spec_counter_action {
    pub r#type: ibv_flow_spec_type,
    pub size: u16,
    pub counters: *mut ibv_counters,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_flow_spec_esp {
    pub r#type: ibv_flow_spec_type,
    pub size: u16,
    pub val: ibv_flow_esp_filter,
    pub mask: ibv_flow_esp_filter,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_flow_spec_eth {
    pub r#type: ibv_flow_spec_type,
    pub size: u16,
    pub val: ibv_flow_eth_filter,
    pub mask: ibv_flow_eth_filter,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_flow_spec_gre {
    pub r#type: ibv_flow_spec_type,
    pub size: u16,
    pub val: ibv_flow_gre_filter,
    pub mask: ibv_flow_gre_filter,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_flow_spec_ipv4 {
    pub r#type: ibv_flow_spec_type,
    pub size: u16,
    pub val: ibv_flow_ipv4_filter,
    pub mask: ibv_flow_ipv4_filter,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_flow_spec_ipv4_ext {
    pub r#type: ibv_flow_spec_type,
    pub size: u16,
    pub val: ibv_flow_ipv4_ext_filter,
    pub mask: ibv_flow_ipv4_ext_filter,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_flow_spec_ipv6 {
    pub r#type: ibv_flow_spec_type,
    pub size: u16,
    pub val: ibv_flow_ipv6_filter,
    pub mask: ibv_flow_ipv6_filter,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_flow_spec_mpls {
    pub r#type: ibv_flow_spec_type,
    pub size: u16,
    pub val: ibv_flow_mpls_filter,
    pub mask: ibv_flow_mpls_filter,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_flow_spec_tcp_udp {
    pub r#type: ibv_flow_spec_type,
    pub size: u16,
    pub val: ibv_flow_tcp_udp_filter,
    pub mask: ibv_flow_tcp_udp_filter,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_flow_spec_tunnel {
    pub r#type: ibv_flow_spec_type,
    pub size: u16,
    pub val: ibv_flow_tunnel_filter,
    pub mask: ibv_flow_tunnel_filter,
}
pub type ibv_flow_spec_type = u32;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_flow_tcp_udp_filter {
    pub dst_port: u16,
    pub src_port: u16,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_flow_tunnel_filter {
    pub tunnel_id: u32,
}
pub type ibv_fork_status = u32;
#[repr(C)]
#[derive(Clone, Copy)]
pub union ibv_gid {
    pub raw: [u8; 16],
    pub global: ibv_gid_0,
}
impl Default for ibv_gid {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_gid_0 {
    pub subnet_prefix: bnd_linux::libc::types::__be64,
    pub interface_id: bnd_linux::libc::types::__be64,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ibv_gid_entry {
    pub gid: ibv_gid,
    pub gid_index: u32,
    pub port_num: u32,
    pub gid_type: u32,
    pub ndev_ifindex: u32,
}
impl Default for ibv_gid_entry {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
pub type ibv_gid_type = u32;
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ibv_global_route {
    pub dgid: ibv_gid,
    pub flow_label: u32,
    pub sgid_index: u8,
    pub hop_limit: u8,
    pub traffic_class: u8,
}
impl Default for ibv_global_route {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ibv_grh {
    pub version_tclass_flow: bnd_linux::libc::types::__be32,
    pub paylen: bnd_linux::libc::types::__be16,
    pub next_hdr: u8,
    pub hop_limit: u8,
    pub sgid: ibv_gid,
    pub dgid: ibv_gid,
}
impl Default for ibv_grh {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
pub type ibv_ind_table_init_attr_mask = u32;
pub type ibv_mig_state = u32;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_moderate_cq {
    pub cq_count: u16,
    pub cq_period: u16,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_modify_cq_attr {
    pub attr_mask: u32,
    pub moderate: ibv_moderate_cq,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_mr {
    pub context: *mut ibv_context,
    pub pd: *mut ibv_pd,
    pub addr: *mut core::ffi::c_void,
    pub length: usize,
    pub handle: u32,
    pub lkey: u32,
    pub rkey: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_mr_init_attr {
    pub length: usize,
    pub access: i32,
    pub comp_mask: u64,
    pub iova: u64,
    pub addr: *mut core::ffi::c_void,
    pub fd: i32,
    pub fd_offset: u64,
    pub dmah: *mut ibv_dmah,
}
pub type ibv_mr_init_attr_mask = u32;
pub type ibv_mtu = u32;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_mw {
    pub context: *mut ibv_context,
    pub pd: *mut ibv_pd,
    pub rkey: u32,
    pub handle: u32,
    pub r#type: ibv_mw_type,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_mw_bind {
    pub wr_id: u64,
    pub send_flags: u32,
    pub bind_info: ibv_mw_bind_info,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_mw_bind_info {
    pub mr: *mut ibv_mr,
    pub addr: u64,
    pub length: u64,
    pub mw_access_flags: u32,
}
pub type ibv_mw_type = u32;
pub type ibv_node_type = i32;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_odp_caps {
    pub general_caps: u64,
    pub per_transport_caps: ibv_odp_caps_0,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_odp_caps_0 {
    pub rc_odp_caps: u32,
    pub uc_odp_caps: u32,
    pub ud_odp_caps: u32,
}
pub type ibv_odp_general_caps = u32;
pub type ibv_odp_transport_cap_bits = u32;
pub type ibv_ops_flags = u32;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_ops_wr {
    pub wr_id: u64,
    pub next: *mut Self,
    pub opcode: ibv_ops_wr_opcode,
    pub flags: i32,
    pub tm: ibv_ops_wr_0,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_ops_wr_0 {
    pub unexpected_cnt: u32,
    pub handle: u32,
    pub add: ibv_ops_wr_0_0,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_ops_wr_0_0 {
    pub recv_wr_id: u64,
    pub sg_list: *mut ibv_sge,
    pub num_sge: i32,
    pub tag: u64,
    pub mask: u64,
}
pub type ibv_ops_wr_opcode = u32;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_packet_pacing_caps {
    pub qp_rate_limit_min: u32,
    pub qp_rate_limit_max: u32,
    pub supported_qpts: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_parent_domain_init_attr {
    pub pd: *mut ibv_pd,
    pub td: *mut ibv_td,
    pub comp_mask: u32,
    pub alloc: *mut u8,
    pub free: *mut u8,
    pub pd_context: *mut core::ffi::c_void,
}
pub type ibv_parent_domain_init_attr_mask = u32;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_pci_atomic_caps {
    pub fetch_add: u16,
    pub swap: u16,
    pub compare_swap: u16,
}
pub type ibv_pci_atomic_op_size = u32;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_pd {
    pub context: *mut ibv_context,
    pub handle: u32,
}
pub type ibv_placement_type = u32;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_poll_cq_attr {
    pub comp_mask: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_port_attr {
    pub state: ibv_port_state,
    pub max_mtu: ibv_mtu,
    pub active_mtu: ibv_mtu,
    pub gid_tbl_len: i32,
    pub port_cap_flags: u32,
    pub max_msg_sz: u32,
    pub bad_pkey_cntr: u32,
    pub qkey_viol_cntr: u32,
    pub pkey_tbl_len: u16,
    pub lid: u16,
    pub sm_lid: u16,
    pub lmc: u8,
    pub max_vl_num: u8,
    pub sm_sl: u8,
    pub subnet_timeout: u8,
    pub init_type_reply: u8,
    pub active_width: u8,
    pub active_speed: u8,
    pub phys_state: u8,
    pub link_layer: u8,
    pub flags: u8,
    pub port_cap_flags2: u16,
    pub active_speed_ex: u32,
}
pub type ibv_port_cap_flags = u32;
pub type ibv_port_cap_flags2 = u32;
pub type ibv_port_state = u32;
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ibv_qp {
    pub context: *mut ibv_context,
    pub qp_context: *mut core::ffi::c_void,
    pub pd: *mut ibv_pd,
    pub send_cq: *mut ibv_cq,
    pub recv_cq: *mut ibv_cq,
    pub srq: *mut ibv_srq,
    pub handle: u32,
    pub qp_num: u32,
    pub state: ibv_qp_state,
    pub qp_type: ibv_qp_type,
    pub mutex: bnd_linux::libc::pthreadtypes::pthread_mutex_t,
    pub cond: bnd_linux::libc::pthreadtypes::pthread_cond_t,
    pub events_completed: u32,
}
impl Default for ibv_qp {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ibv_qp_attr {
    pub qp_state: ibv_qp_state,
    pub cur_qp_state: ibv_qp_state,
    pub path_mtu: ibv_mtu,
    pub path_mig_state: ibv_mig_state,
    pub qkey: u32,
    pub rq_psn: u32,
    pub sq_psn: u32,
    pub dest_qp_num: u32,
    pub qp_access_flags: u32,
    pub cap: ibv_qp_cap,
    pub ah_attr: ibv_ah_attr,
    pub alt_ah_attr: ibv_ah_attr,
    pub pkey_index: u16,
    pub alt_pkey_index: u16,
    pub en_sqd_async_notify: u8,
    pub sq_draining: u8,
    pub max_rd_atomic: u8,
    pub max_dest_rd_atomic: u8,
    pub min_rnr_timer: u8,
    pub port_num: u8,
    pub timeout: u8,
    pub retry_cnt: u8,
    pub rnr_retry: u8,
    pub alt_port_num: u8,
    pub alt_timeout: u8,
    pub rate_limit: u32,
}
impl Default for ibv_qp_attr {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
pub type ibv_qp_attr_mask = u32;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_qp_cap {
    pub max_send_wr: u32,
    pub max_recv_wr: u32,
    pub max_send_sge: u32,
    pub max_recv_sge: u32,
    pub max_inline_data: u32,
}
pub type ibv_qp_create_flags = u32;
pub type ibv_qp_create_send_ops_flags = u32;
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ibv_qp_ex {
    pub qp_base: ibv_qp,
    pub comp_mask: u64,
    pub wr_id: u64,
    pub wr_flags: u32,
    pub wr_atomic_cmp_swp: *mut u8,
    pub wr_atomic_fetch_add: *mut u8,
    pub wr_bind_mw: *mut u8,
    pub wr_local_inv: *mut u8,
    pub wr_rdma_read: *mut u8,
    pub wr_rdma_write: *mut u8,
    pub wr_rdma_write_imm: *mut u8,
    pub wr_send: *mut u8,
    pub wr_send_imm: *mut u8,
    pub wr_send_inv: *mut u8,
    pub wr_send_tso: *mut u8,
    pub wr_set_ud_addr: *mut u8,
    pub wr_set_xrc_srqn: *mut u8,
    pub wr_set_inline_data: *mut u8,
    pub wr_set_inline_data_list: *mut u8,
    pub wr_set_sge: *mut u8,
    pub wr_set_sge_list: *mut u8,
    pub wr_start: *mut u8,
    pub wr_complete: *mut u8,
    pub wr_abort: *mut u8,
    pub wr_atomic_write: *mut u8,
    pub wr_flush: *mut u8,
}
impl Default for ibv_qp_ex {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_qp_init_attr {
    pub qp_context: *mut core::ffi::c_void,
    pub send_cq: *mut ibv_cq,
    pub recv_cq: *mut ibv_cq,
    pub srq: *mut ibv_srq,
    pub cap: ibv_qp_cap,
    pub qp_type: ibv_qp_type,
    pub sq_sig_all: i32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_qp_init_attr_ex {
    pub qp_context: *mut core::ffi::c_void,
    pub send_cq: *mut ibv_cq,
    pub recv_cq: *mut ibv_cq,
    pub srq: *mut ibv_srq,
    pub cap: ibv_qp_cap,
    pub qp_type: ibv_qp_type,
    pub sq_sig_all: i32,
    pub comp_mask: u32,
    pub pd: *mut ibv_pd,
    pub xrcd: *mut ibv_xrcd,
    pub create_flags: u32,
    pub max_tso_header: u16,
    pub rwq_ind_tbl: *mut ibv_rwq_ind_table,
    pub rx_hash_conf: ibv_rx_hash_conf,
    pub source_qpn: u32,
    pub send_ops_flags: u64,
}
pub type ibv_qp_init_attr_mask = u32;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_qp_open_attr {
    pub comp_mask: u32,
    pub qp_num: u32,
    pub xrcd: *mut ibv_xrcd,
    pub qp_context: *mut core::ffi::c_void,
    pub qp_type: ibv_qp_type,
}
pub type ibv_qp_open_attr_mask = u32;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_qp_rate_limit_attr {
    pub rate_limit: u32,
    pub max_burst_sz: u32,
    pub typical_pkt_sz: u16,
    pub comp_mask: u32,
}
pub type ibv_qp_state = u32;
pub type ibv_qp_type = u32;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_query_device_ex_input {
    pub comp_mask: u32,
}
pub type ibv_query_qp_data_in_order_caps = u32;
pub type ibv_query_qp_data_in_order_flags = u32;
pub type ibv_rate = u32;
pub type ibv_raw_packet_caps = u32;
pub type ibv_read_counters_flags = u32;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_recv_wr {
    pub wr_id: u64,
    pub next: *mut Self,
    pub sg_list: *mut ibv_sge,
    pub num_sge: i32,
}
pub type ibv_rereg_mr_err_code = i32;
pub type ibv_rereg_mr_flags = u32;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_rss_caps {
    pub supported_qpts: u32,
    pub max_rwq_indirection_tables: u32,
    pub max_rwq_indirection_table_size: u32,
    pub rx_hash_fields_mask: u64,
    pub rx_hash_function: u8,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_rwq_ind_table {
    pub context: *mut ibv_context,
    pub ind_tbl_handle: i32,
    pub ind_tbl_num: i32,
    pub comp_mask: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_rwq_ind_table_init_attr {
    pub log_ind_tbl_size: u32,
    pub ind_tbl: *mut *mut ibv_wq,
    pub comp_mask: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_rx_hash_conf {
    pub rx_hash_function: u8,
    pub rx_hash_key_len: u8,
    pub rx_hash_key: *mut u8,
    pub rx_hash_fields_mask: u64,
}
pub type ibv_rx_hash_fields = u32;
pub type ibv_rx_hash_function_flags = u32;
pub type ibv_selectivity_level = u32;
pub type ibv_send_flags = u32;
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ibv_send_wr {
    pub wr_id: u64,
    pub next: *mut Self,
    pub sg_list: *mut ibv_sge,
    pub num_sge: i32,
    pub opcode: ibv_wr_opcode,
    pub send_flags: u32,
    pub Anonymous: ibv_send_wr_0,
    pub wr: ibv_send_wr_1,
    pub qp_type: ibv_send_wr_2,
    pub Anonymous2: ibv_send_wr_3,
}
impl Default for ibv_send_wr {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub union ibv_send_wr_0 {
    pub imm_data: bnd_linux::libc::types::__be32,
    pub invalidate_rkey: u32,
}
impl Default for ibv_send_wr_0 {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub union ibv_send_wr_1 {
    pub rdma: ibv_send_wr_1_0,
    pub atomic: ibv_send_wr_1_1,
    pub ud: ibv_send_wr_1_2,
}
impl Default for ibv_send_wr_1 {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_send_wr_1_0 {
    pub remote_addr: u64,
    pub rkey: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_send_wr_1_1 {
    pub remote_addr: u64,
    pub compare_add: u64,
    pub swap: u64,
    pub rkey: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_send_wr_1_2 {
    pub ah: *mut ibv_ah,
    pub remote_qpn: u32,
    pub remote_qkey: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub union ibv_send_wr_2 {
    pub xrc: ibv_send_wr_2_0,
}
impl Default for ibv_send_wr_2 {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_send_wr_2_0 {
    pub remote_srqn: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub union ibv_send_wr_3 {
    pub bind_mw: ibv_send_wr_3_0,
    pub tso: ibv_send_wr_3_1,
}
impl Default for ibv_send_wr_3 {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_send_wr_3_0 {
    pub mw: *mut ibv_mw,
    pub rkey: u32,
    pub bind_info: ibv_mw_bind_info,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_send_wr_3_1 {
    pub hdr: *mut core::ffi::c_void,
    pub hdr_sz: u16,
    pub mss: u16,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_sge {
    pub addr: u64,
    pub length: u32,
    pub lkey: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ibv_srq {
    pub context: *mut ibv_context,
    pub srq_context: *mut core::ffi::c_void,
    pub pd: *mut ibv_pd,
    pub handle: u32,
    pub mutex: bnd_linux::libc::pthreadtypes::pthread_mutex_t,
    pub cond: bnd_linux::libc::pthreadtypes::pthread_cond_t,
    pub events_completed: u32,
}
impl Default for ibv_srq {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_srq_attr {
    pub max_wr: u32,
    pub max_sge: u32,
    pub srq_limit: u32,
}
pub type ibv_srq_attr_mask = u32;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_srq_init_attr {
    pub srq_context: *mut core::ffi::c_void,
    pub attr: ibv_srq_attr,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_srq_init_attr_ex {
    pub srq_context: *mut core::ffi::c_void,
    pub attr: ibv_srq_attr,
    pub comp_mask: u32,
    pub srq_type: ibv_srq_type,
    pub pd: *mut ibv_pd,
    pub xrcd: *mut ibv_xrcd,
    pub cq: *mut ibv_cq,
    pub tm_cap: ibv_tm_cap,
}
pub type ibv_srq_init_attr_mask = u32;
pub type ibv_srq_type = u32;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_td {
    pub context: *mut ibv_context,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_td_init_attr {
    pub comp_mask: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_tm_cap {
    pub max_num_tags: u32,
    pub max_ops: u32,
}
pub type ibv_tm_cap_flags = u32;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_tm_caps {
    pub max_rndv_hdr_size: u32,
    pub max_num_tags: u32,
    pub flags: u32,
    pub max_ops: u32,
    pub max_sge: u32,
}
pub type ibv_tph_mem_type = u32;
pub type ibv_transport_type = i32;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_tso_caps {
    pub max_tso: u32,
    pub supported_qpts: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_values_ex {
    pub comp_mask: u32,
    pub raw_clock: bnd_linux::libc::struct_timespec::timespec,
}
pub type ibv_values_mask = u32;
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ibv_wc {
    pub wr_id: u64,
    pub status: ibv_wc_status,
    pub opcode: ibv_wc_opcode,
    pub vendor_err: u32,
    pub byte_len: u32,
    pub Anonymous: ibv_wc_0,
    pub qp_num: u32,
    pub src_qp: u32,
    pub wc_flags: u32,
    pub pkey_index: u16,
    pub slid: u16,
    pub sl: u8,
    pub dlid_path_bits: u8,
}
impl Default for ibv_wc {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub union ibv_wc_0 {
    pub imm_data: bnd_linux::libc::types::__be32,
    pub invalidated_rkey: u32,
}
impl Default for ibv_wc_0 {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
pub type ibv_wc_flags = u32;
pub type ibv_wc_opcode = u32;
pub type ibv_wc_status = u32;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_wc_tm_info {
    pub tag: u64,
    pub r#priv: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ibv_wq {
    pub context: *mut ibv_context,
    pub wq_context: *mut core::ffi::c_void,
    pub pd: *mut ibv_pd,
    pub cq: *mut ibv_cq,
    pub wq_num: u32,
    pub handle: u32,
    pub state: ibv_wq_state,
    pub wq_type: ibv_wq_type,
    pub post_recv: *mut u8,
    pub mutex: bnd_linux::libc::pthreadtypes::pthread_mutex_t,
    pub cond: bnd_linux::libc::pthreadtypes::pthread_cond_t,
    pub events_completed: u32,
    pub comp_mask: u32,
}
impl Default for ibv_wq {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_wq_attr {
    pub attr_mask: u32,
    pub wq_state: ibv_wq_state,
    pub curr_wq_state: ibv_wq_state,
    pub flags: u32,
    pub flags_mask: u32,
}
pub type ibv_wq_attr_mask = u32;
pub type ibv_wq_flags = u32;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_wq_init_attr {
    pub wq_context: *mut core::ffi::c_void,
    pub wq_type: ibv_wq_type,
    pub max_wr: u32,
    pub max_sge: u32,
    pub pd: *mut ibv_pd,
    pub cq: *mut ibv_cq,
    pub comp_mask: u32,
    pub create_flags: u32,
}
pub type ibv_wq_init_attr_mask = u32;
pub type ibv_wq_state = u32;
pub type ibv_wq_type = u32;
pub type ibv_wr_opcode = u32;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_xrcd {
    pub context: *mut ibv_context,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ibv_xrcd_init_attr {
    pub comp_mask: u32,
    pub fd: i32,
    pub oflags: i32,
}
pub type ibv_xrcd_init_attr_mask = u32;
pub type rdma_driver_id = u32;
#[repr(C)]
#[derive(Clone, Copy)]
pub struct verbs_context {
    pub reg_mr_ex: *mut u8,
    pub dealloc_dmah: *mut u8,
    pub alloc_dmah: *mut u8,
    pub query_port: *mut u8,
    pub advise_mr: *mut u8,
    pub alloc_null_mr: *mut u8,
    pub read_counters: *mut u8,
    pub attach_counters_point_flow: *mut u8,
    pub create_counters: *mut u8,
    pub destroy_counters: *mut u8,
    pub reg_dm_mr: *mut u8,
    pub alloc_dm: *mut u8,
    pub free_dm: *mut u8,
    pub modify_flow_action_esp: *mut u8,
    pub destroy_flow_action: *mut u8,
    pub create_flow_action_esp: *mut u8,
    pub modify_qp_rate_limit: *mut u8,
    pub alloc_parent_domain: *mut u8,
    pub dealloc_td: *mut u8,
    pub alloc_td: *mut u8,
    pub modify_cq: *mut u8,
    pub post_srq_ops: *mut u8,
    pub destroy_rwq_ind_table: *mut u8,
    pub create_rwq_ind_table: *mut u8,
    pub destroy_wq: *mut u8,
    pub modify_wq: *mut u8,
    pub create_wq: *mut u8,
    pub query_rt_values: *mut u8,
    pub create_cq_ex: *mut u8,
    pub r#priv: *mut verbs_ex_private,
    pub query_device_ex: *mut u8,
    pub ibv_destroy_flow: *mut u8,
    pub ABI_placeholder2: *mut u8,
    pub ibv_create_flow: *mut u8,
    pub ABI_placeholder1: *mut u8,
    pub open_qp: *mut u8,
    pub create_qp_ex: *mut u8,
    pub get_srq_num: *mut u8,
    pub create_srq_ex: *mut u8,
    pub open_xrcd: *mut u8,
    pub close_xrcd: *mut u8,
    pub _ABI_placeholder3: u64,
    pub sz: usize,
    pub context: ibv_context,
}
impl Default for verbs_context {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct verbs_ex_private(pub u8);
