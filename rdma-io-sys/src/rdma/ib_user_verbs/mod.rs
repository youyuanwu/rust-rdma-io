pub const IB_DEVICE_NAME_MAX: i32 = 64;
pub const IB_FLUSH_GLOBAL: ib_placement_type = 1;
pub const IB_FLUSH_MR: ib_selectivity_level = 1;
pub const IB_FLUSH_PERSISTENT: ib_placement_type = 2;
pub const IB_FLUSH_RANGE: ib_selectivity_level = 0;
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
pub const IB_UVERBS_RAW_PACKET_CAP_CVLAN_STRIPPING: ib_uverbs_raw_packet_caps = 1;
pub const IB_UVERBS_RAW_PACKET_CAP_DELAY_DROP: ib_uverbs_raw_packet_caps = 8;
pub const IB_UVERBS_RAW_PACKET_CAP_IP_CSUM: ib_uverbs_raw_packet_caps = 4;
pub const IB_UVERBS_RAW_PACKET_CAP_SCATTER_FCS: ib_uverbs_raw_packet_caps = 2;
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
pub type ib_placement_type = u32;
pub type ib_selectivity_level = u32;
#[repr(C)]
#[cfg(feature = "int_ll64")]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_ah_attr {
    pub grh: ib_uverbs_global_route,
    pub dlid: bnd_linux::libc::int_ll64::__u16,
    pub sl: super::int_ll64::__u8,
    pub src_path_bits: super::int_ll64::__u8,
    pub static_rate: super::int_ll64::__u8,
    pub is_global: super::int_ll64::__u8,
    pub port_num: super::int_ll64::__u8,
    pub reserved: super::int_ll64::__u8,
}
#[repr(C)]
#[cfg(feature = "int_ll64")]
#[derive(Clone, Copy)]
pub struct ib_uverbs_alloc_mw {
    pub response: bnd_linux::libc::int_ll64::__u64,
    pub pd_handle: bnd_linux::libc::int_ll64::__u32,
    pub mw_type: super::int_ll64::__u8,
    pub reserved: [super::int_ll64::__u8; 3],
    pub driver_data: [bnd_linux::libc::int_ll64::__u64; 0],
}
#[cfg(feature = "int_ll64")]
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
#[cfg(feature = "int_ll64")]
#[derive(Clone, Copy)]
pub struct ib_uverbs_attach_mcast {
    pub gid: [super::int_ll64::__u8; 16],
    pub qp_handle: bnd_linux::libc::int_ll64::__u32,
    pub mlid: bnd_linux::libc::int_ll64::__u16,
    pub reserved: bnd_linux::libc::int_ll64::__u16,
    pub driver_data: [bnd_linux::libc::int_ll64::__u64; 0],
}
#[cfg(feature = "int_ll64")]
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
#[cfg(feature = "int_ll64")]
#[derive(Clone, Copy)]
pub struct ib_uverbs_create_ah {
    pub response: bnd_linux::libc::int_ll64::__u64,
    pub user_handle: bnd_linux::libc::int_ll64::__u64,
    pub pd_handle: bnd_linux::libc::int_ll64::__u32,
    pub reserved: bnd_linux::libc::int_ll64::__u32,
    pub attr: ib_uverbs_ah_attr,
    pub driver_data: [bnd_linux::libc::int_ll64::__u64; 0],
}
#[cfg(feature = "int_ll64")]
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
#[cfg(feature = "int_ll64")]
#[derive(Clone, Copy)]
pub struct ib_uverbs_create_cq {
    pub response: bnd_linux::libc::int_ll64::__u64,
    pub user_handle: bnd_linux::libc::int_ll64::__u64,
    pub cqe: bnd_linux::libc::int_ll64::__u32,
    pub comp_vector: bnd_linux::libc::int_ll64::__u32,
    pub comp_channel: super::int_ll64::__s32,
    pub reserved: bnd_linux::libc::int_ll64::__u32,
    pub driver_data: [bnd_linux::libc::int_ll64::__u64; 0],
}
#[cfg(feature = "int_ll64")]
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
#[cfg(feature = "int_ll64")]
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
#[cfg(feature = "int_ll64")]
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
    pub sq_sig_all: super::int_ll64::__u8,
    pub qp_type: super::int_ll64::__u8,
    pub is_srq: super::int_ll64::__u8,
    pub reserved: super::int_ll64::__u8,
    pub driver_data: [bnd_linux::libc::int_ll64::__u64; 0],
}
#[cfg(feature = "int_ll64")]
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
#[cfg(feature = "int_ll64")]
#[derive(Clone, Copy)]
pub struct ib_uverbs_detach_mcast {
    pub gid: [super::int_ll64::__u8; 16],
    pub qp_handle: bnd_linux::libc::int_ll64::__u32,
    pub mlid: bnd_linux::libc::int_ll64::__u16,
    pub reserved: bnd_linux::libc::int_ll64::__u16,
    pub driver_data: [bnd_linux::libc::int_ll64::__u64; 0],
}
#[cfg(feature = "int_ll64")]
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
#[cfg(feature = "int_ll64")]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_ex_create_cq {
    pub user_handle: bnd_linux::libc::int_ll64::__u64,
    pub cqe: bnd_linux::libc::int_ll64::__u32,
    pub comp_vector: bnd_linux::libc::int_ll64::__u32,
    pub comp_channel: super::int_ll64::__s32,
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
#[cfg(feature = "int_ll64")]
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
    pub sq_sig_all: super::int_ll64::__u8,
    pub qp_type: super::int_ll64::__u8,
    pub is_srq: super::int_ll64::__u8,
    pub reserved: super::int_ll64::__u8,
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
#[cfg(feature = "int_ll64")]
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
#[cfg(feature = "int_ll64")]
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
#[cfg(feature = "int_ll64")]
#[derive(Clone, Copy)]
pub struct ib_uverbs_flow_attr {
    pub r#type: bnd_linux::libc::int_ll64::__u32,
    pub size: bnd_linux::libc::int_ll64::__u16,
    pub priority: bnd_linux::libc::int_ll64::__u16,
    pub num_of_specs: super::int_ll64::__u8,
    pub reserved: [super::int_ll64::__u8; 2],
    pub port: super::int_ll64::__u8,
    pub flags: bnd_linux::libc::int_ll64::__u32,
    pub flow_specs: [ib_uverbs_flow_spec_hdr; 0],
}
#[cfg(feature = "int_ll64")]
impl Default for ib_uverbs_flow_attr {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[cfg(feature = "int_ll64")]
#[derive(Clone, Copy)]
pub struct ib_uverbs_flow_eth_filter {
    pub dst_mac: [super::int_ll64::__u8; 6],
    pub src_mac: [super::int_ll64::__u8; 6],
    pub ether_type: bnd_linux::libc::types::__be16,
    pub vlan_tag: bnd_linux::libc::types::__be16,
}
#[cfg(feature = "int_ll64")]
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
#[cfg(feature = "int_ll64")]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_flow_ipv4_filter {
    pub src_ip: bnd_linux::libc::types::__be32,
    pub dst_ip: bnd_linux::libc::types::__be32,
    pub proto: super::int_ll64::__u8,
    pub tos: super::int_ll64::__u8,
    pub ttl: super::int_ll64::__u8,
    pub flags: super::int_ll64::__u8,
}
#[repr(C)]
#[cfg(feature = "int_ll64")]
#[derive(Clone, Copy)]
pub struct ib_uverbs_flow_ipv6_filter {
    pub src_ip: [super::int_ll64::__u8; 16],
    pub dst_ip: [super::int_ll64::__u8; 16],
    pub flow_label: bnd_linux::libc::types::__be32,
    pub next_hdr: super::int_ll64::__u8,
    pub traffic_class: super::int_ll64::__u8,
    pub hop_limit: super::int_ll64::__u8,
    pub reserved: super::int_ll64::__u8,
}
#[cfg(feature = "int_ll64")]
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
#[cfg(feature = "int_ll64")]
#[derive(Clone, Copy)]
pub struct ib_uverbs_flow_spec_eth {
    pub Anonymous: ib_uverbs_flow_spec_eth_0,
    pub val: ib_uverbs_flow_eth_filter,
    pub mask: ib_uverbs_flow_eth_filter,
}
#[cfg(feature = "int_ll64")]
impl Default for ib_uverbs_flow_spec_eth {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[cfg(feature = "int_ll64")]
#[derive(Clone, Copy)]
pub union ib_uverbs_flow_spec_eth_0 {
    pub hdr: ib_uverbs_flow_spec_hdr,
    pub Anonymous: ib_uverbs_flow_spec_eth_0_0,
}
#[cfg(feature = "int_ll64")]
impl Default for ib_uverbs_flow_spec_eth_0 {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[cfg(feature = "int_ll64")]
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
#[cfg(feature = "int_ll64")]
#[derive(Clone, Copy)]
pub struct ib_uverbs_flow_spec_ipv4 {
    pub Anonymous: ib_uverbs_flow_spec_ipv4_0,
    pub val: ib_uverbs_flow_ipv4_filter,
    pub mask: ib_uverbs_flow_ipv4_filter,
}
#[cfg(feature = "int_ll64")]
impl Default for ib_uverbs_flow_spec_ipv4 {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[cfg(feature = "int_ll64")]
#[derive(Clone, Copy)]
pub union ib_uverbs_flow_spec_ipv4_0 {
    pub hdr: ib_uverbs_flow_spec_hdr,
    pub Anonymous: ib_uverbs_flow_spec_ipv4_0_0,
}
#[cfg(feature = "int_ll64")]
impl Default for ib_uverbs_flow_spec_ipv4_0 {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[cfg(feature = "int_ll64")]
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_flow_spec_ipv4_0_0 {
    pub r#type: bnd_linux::libc::int_ll64::__u32,
    pub size: bnd_linux::libc::int_ll64::__u16,
    pub reserved: bnd_linux::libc::int_ll64::__u16,
}
#[repr(C)]
#[cfg(feature = "int_ll64")]
#[derive(Clone, Copy)]
pub struct ib_uverbs_flow_spec_ipv6 {
    pub Anonymous: ib_uverbs_flow_spec_ipv6_0,
    pub val: ib_uverbs_flow_ipv6_filter,
    pub mask: ib_uverbs_flow_ipv6_filter,
}
#[cfg(feature = "int_ll64")]
impl Default for ib_uverbs_flow_spec_ipv6 {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[cfg(feature = "int_ll64")]
#[derive(Clone, Copy)]
pub union ib_uverbs_flow_spec_ipv6_0 {
    pub hdr: ib_uverbs_flow_spec_hdr,
    pub Anonymous: ib_uverbs_flow_spec_ipv6_0_0,
}
#[cfg(feature = "int_ll64")]
impl Default for ib_uverbs_flow_spec_ipv6_0 {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[cfg(feature = "int_ll64")]
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
#[cfg(feature = "int_ll64")]
#[derive(Clone, Copy)]
pub struct ib_uverbs_global_route {
    pub dgid: [super::int_ll64::__u8; 16],
    pub flow_label: bnd_linux::libc::int_ll64::__u32,
    pub sgid_index: super::int_ll64::__u8,
    pub hop_limit: super::int_ll64::__u8,
    pub traffic_class: super::int_ll64::__u8,
    pub reserved: super::int_ll64::__u8,
}
#[cfg(feature = "int_ll64")]
impl Default for ib_uverbs_global_route {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[cfg(feature = "int_ll64")]
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
    pub qp_state: super::int_ll64::__u8,
    pub cur_qp_state: super::int_ll64::__u8,
    pub path_mtu: super::int_ll64::__u8,
    pub path_mig_state: super::int_ll64::__u8,
    pub en_sqd_async_notify: super::int_ll64::__u8,
    pub max_rd_atomic: super::int_ll64::__u8,
    pub max_dest_rd_atomic: super::int_ll64::__u8,
    pub min_rnr_timer: super::int_ll64::__u8,
    pub port_num: super::int_ll64::__u8,
    pub timeout: super::int_ll64::__u8,
    pub retry_cnt: super::int_ll64::__u8,
    pub rnr_retry: super::int_ll64::__u8,
    pub alt_port_num: super::int_ll64::__u8,
    pub alt_timeout: super::int_ll64::__u8,
    pub reserved: [super::int_ll64::__u8; 2],
    pub driver_data: [bnd_linux::libc::int_ll64::__u64; 0],
}
#[cfg(feature = "int_ll64")]
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
#[cfg(feature = "int_ll64")]
#[derive(Clone, Copy)]
pub struct ib_uverbs_open_qp {
    pub response: bnd_linux::libc::int_ll64::__u64,
    pub user_handle: bnd_linux::libc::int_ll64::__u64,
    pub pd_handle: bnd_linux::libc::int_ll64::__u32,
    pub qpn: bnd_linux::libc::int_ll64::__u32,
    pub qp_type: super::int_ll64::__u8,
    pub reserved: [super::int_ll64::__u8; 7],
    pub driver_data: [bnd_linux::libc::int_ll64::__u64; 0],
}
#[cfg(feature = "int_ll64")]
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
#[cfg(feature = "int_ll64")]
#[derive(Clone, Copy)]
pub struct ib_uverbs_poll_cq_resp {
    pub count: bnd_linux::libc::int_ll64::__u32,
    pub reserved: bnd_linux::libc::int_ll64::__u32,
    pub wc: [ib_uverbs_wc; 0],
}
#[cfg(feature = "int_ll64")]
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
#[cfg(feature = "int_ll64")]
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
    pub en_sqd_async_notify: super::int_ll64::__u8,
    pub sq_draining: super::int_ll64::__u8,
    pub max_rd_atomic: super::int_ll64::__u8,
    pub max_dest_rd_atomic: super::int_ll64::__u8,
    pub min_rnr_timer: super::int_ll64::__u8,
    pub port_num: super::int_ll64::__u8,
    pub timeout: super::int_ll64::__u8,
    pub retry_cnt: super::int_ll64::__u8,
    pub rnr_retry: super::int_ll64::__u8,
    pub alt_port_num: super::int_ll64::__u8,
    pub alt_timeout: super::int_ll64::__u8,
    pub reserved: [super::int_ll64::__u8; 5],
}
#[cfg(feature = "int_ll64")]
impl Default for ib_uverbs_qp_attr {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[cfg(feature = "int_ll64")]
#[derive(Clone, Copy)]
pub struct ib_uverbs_qp_dest {
    pub dgid: [super::int_ll64::__u8; 16],
    pub flow_label: bnd_linux::libc::int_ll64::__u32,
    pub dlid: bnd_linux::libc::int_ll64::__u16,
    pub reserved: bnd_linux::libc::int_ll64::__u16,
    pub sgid_index: super::int_ll64::__u8,
    pub hop_limit: super::int_ll64::__u8,
    pub traffic_class: super::int_ll64::__u8,
    pub sl: super::int_ll64::__u8,
    pub src_path_bits: super::int_ll64::__u8,
    pub static_rate: super::int_ll64::__u8,
    pub is_global: super::int_ll64::__u8,
    pub port_num: super::int_ll64::__u8,
}
#[cfg(feature = "int_ll64")]
impl Default for ib_uverbs_qp_dest {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
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
#[cfg(feature = "int_ll64")]
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
    pub local_ca_ack_delay: super::int_ll64::__u8,
    pub phys_port_cnt: super::int_ll64::__u8,
    pub reserved: [super::int_ll64::__u8; 4],
}
#[cfg(feature = "int_ll64")]
impl Default for ib_uverbs_query_device_resp {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[cfg(feature = "int_ll64")]
#[derive(Clone, Copy)]
pub struct ib_uverbs_query_port {
    pub response: bnd_linux::libc::int_ll64::__u64,
    pub port_num: super::int_ll64::__u8,
    pub reserved: [super::int_ll64::__u8; 7],
    pub driver_data: [bnd_linux::libc::int_ll64::__u64; 0],
}
#[cfg(feature = "int_ll64")]
impl Default for ib_uverbs_query_port {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[cfg(feature = "int_ll64")]
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
    pub state: super::int_ll64::__u8,
    pub max_mtu: super::int_ll64::__u8,
    pub active_mtu: super::int_ll64::__u8,
    pub lmc: super::int_ll64::__u8,
    pub max_vl_num: super::int_ll64::__u8,
    pub sm_sl: super::int_ll64::__u8,
    pub subnet_timeout: super::int_ll64::__u8,
    pub init_type_reply: super::int_ll64::__u8,
    pub active_width: super::int_ll64::__u8,
    pub active_speed: super::int_ll64::__u8,
    pub phys_state: super::int_ll64::__u8,
    pub link_layer: super::int_ll64::__u8,
    pub flags: super::int_ll64::__u8,
    pub reserved: super::int_ll64::__u8,
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
#[cfg(feature = "int_ll64")]
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
    pub qp_state: super::int_ll64::__u8,
    pub cur_qp_state: super::int_ll64::__u8,
    pub path_mtu: super::int_ll64::__u8,
    pub path_mig_state: super::int_ll64::__u8,
    pub sq_draining: super::int_ll64::__u8,
    pub max_rd_atomic: super::int_ll64::__u8,
    pub max_dest_rd_atomic: super::int_ll64::__u8,
    pub min_rnr_timer: super::int_ll64::__u8,
    pub port_num: super::int_ll64::__u8,
    pub timeout: super::int_ll64::__u8,
    pub retry_cnt: super::int_ll64::__u8,
    pub rnr_retry: super::int_ll64::__u8,
    pub alt_port_num: super::int_ll64::__u8,
    pub alt_timeout: super::int_ll64::__u8,
    pub sq_sig_all: super::int_ll64::__u8,
    pub reserved: [super::int_ll64::__u8; 5],
    pub driver_data: [bnd_linux::libc::int_ll64::__u64; 0],
}
#[cfg(feature = "int_ll64")]
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
#[cfg(feature = "int_ll64")]
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
    pub sl: super::int_ll64::__u8,
    pub dlid_path_bits: super::int_ll64::__u8,
    pub port_num: super::int_ll64::__u8,
    pub reserved: super::int_ll64::__u8,
}
#[cfg(feature = "int_ll64")]
impl Default for ib_uverbs_wc {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[cfg(feature = "int_ll64")]
#[derive(Clone, Copy)]
pub union ib_uverbs_wc_0 {
    pub imm_data: bnd_linux::libc::types::__be32,
    pub invalidate_rkey: bnd_linux::libc::int_ll64::__u32,
}
#[cfg(feature = "int_ll64")]
impl Default for ib_uverbs_wc_0 {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
pub type ib_uverbs_wc_opcode = u32;
pub type ib_uverbs_wr_opcode = u32;
pub type ib_uverbs_write_cmds = u32;
