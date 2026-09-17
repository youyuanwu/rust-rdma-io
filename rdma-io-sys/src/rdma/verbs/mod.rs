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
pub const IBV_ACCESS_RELAXED_ORDERING: ibv_access_flags = 1048576;
pub const IBV_ACCESS_REMOTE_ATOMIC: ibv_access_flags = 8;
pub const IBV_ACCESS_REMOTE_READ: ibv_access_flags = 4;
pub const IBV_ACCESS_REMOTE_WRITE: ibv_access_flags = 2;
pub const IBV_ACCESS_ZERO_BASED: ibv_access_flags = 32;
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
pub const IBV_FLOW_ACTION_ESP_MASK_ESN: ibv_flow_action_esp_mask = 1;
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
pub const IB_GRH_FLOWLABEL_MASK: i32 = 1048575;
pub const IB_ROCE_UDP_ENCAP_VALID_PORT_MAX: i32 = 65535;
pub const IB_ROCE_UDP_ENCAP_VALID_PORT_MIN: i32 = 49152;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct _compat_ibv_port_attr(pub u8);
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct _ibv_device_ops {
    pub _dummy1: *mut u8,
    pub _dummy2: *mut u8,
}
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
#[cfg(feature = "ib_user_ioctl_verbs")]
#[derive(Clone, Copy, Default)]
pub struct ibv_flow_action_esp_attr {
    pub esp_attr: *mut super::ib_user_ioctl_verbs::ib_uverbs_flow_action_esp,
    pub keymat_proto: super::ib_user_ioctl_verbs::ib_uverbs_flow_action_esp_keymat,
    pub keymat_len: u16,
    pub keymat_ptr: *mut core::ffi::c_void,
    pub replay_proto: super::ib_user_ioctl_verbs::ib_uverbs_flow_action_esp_replay,
    pub replay_len: u16,
    pub replay_ptr: *mut core::ffi::c_void,
    pub esp_encap: *mut super::ib_user_ioctl_verbs::ib_uverbs_flow_action_esp_encap,
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
