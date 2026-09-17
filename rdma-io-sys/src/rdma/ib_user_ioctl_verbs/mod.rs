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
pub const IB_UVERBS_READ_COUNTERS_PREFER_CACHED: ib_uverbs_read_counters_flags = 1;
pub const IB_UVERBS_SRQT_BASIC: ib_uverbs_srq_type = 0;
pub const IB_UVERBS_SRQT_TM: ib_uverbs_srq_type = 2;
pub const IB_UVERBS_SRQT_XRC: ib_uverbs_srq_type = 1;
pub const IB_UVERBS_WQT_RQ: ib_uverbs_wq_type = 0;
pub const IB_UVERBS_WQ_FLAGS_CVLAN_STRIPPING: ib_uverbs_wq_flags = 1;
pub const IB_UVERBS_WQ_FLAGS_DELAY_DROP: ib_uverbs_wq_flags = 4;
pub const IB_UVERBS_WQ_FLAGS_PCI_WRITE_END_PADDING: ib_uverbs_wq_flags = 8;
pub const IB_UVERBS_WQ_FLAGS_SCATTER_FCS: ib_uverbs_wq_flags = 2;
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
pub type ib_uverbs_access_flags = u32;
pub type ib_uverbs_advise_mr_advice = u32;
pub type ib_uverbs_advise_mr_flag = u32;
pub type ib_uverbs_core_support = u32;
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
#[derive(Clone, Copy, Default)]
pub struct ib_uverbs_qp_cap {
    pub max_send_wr: bnd_linux::libc::int_ll64::__u32,
    pub max_recv_wr: bnd_linux::libc::int_ll64::__u32,
    pub max_send_sge: bnd_linux::libc::int_ll64::__u32,
    pub max_recv_sge: bnd_linux::libc::int_ll64::__u32,
    pub max_inline_data: bnd_linux::libc::int_ll64::__u32,
}
pub type ib_uverbs_qp_create_flags = u32;
pub type ib_uverbs_qp_type = u32;
pub type ib_uverbs_query_port_cap_flags = u32;
pub type ib_uverbs_query_port_flags = u32;
#[repr(C)]
#[cfg(all(feature = "ib_user_verbs", feature = "int_ll64"))]
#[derive(Clone, Copy)]
pub struct ib_uverbs_query_port_resp_ex {
    pub legacy_resp: super::ib_user_verbs::ib_uverbs_query_port_resp,
    pub port_cap_flags2: bnd_linux::libc::int_ll64::__u16,
    pub reserved: [super::int_ll64::__u8; 2],
    pub active_speed_ex: bnd_linux::libc::int_ll64::__u32,
}
#[cfg(all(feature = "ib_user_verbs", feature = "int_ll64"))]
impl Default for ib_uverbs_query_port_resp_ex {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
pub type ib_uverbs_read_counters_flags = u32;
pub type ib_uverbs_srq_type = u32;
pub type ib_uverbs_wq_flags = u32;
pub type ib_uverbs_wq_type = u32;
pub type rdma_driver_id = u32;
