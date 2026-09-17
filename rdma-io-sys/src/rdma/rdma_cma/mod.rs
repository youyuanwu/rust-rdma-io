#[cfg(all(feature = "sa", feature = "verbs"))]
windows_link::link!("rdmacm" "C" fn rdma_accept(id : *mut rdma_cm_id, conn_param : *mut rdma_conn_param) -> i32);
#[cfg(all(feature = "sa", feature = "verbs"))]
windows_link::link!("rdmacm" "C" fn rdma_ack_cm_event(event : *mut rdma_cm_event) -> i32);
#[cfg(all(feature = "sa", feature = "verbs"))]
windows_link::link!("rdmacm" "C" fn rdma_bind_addr(id : *mut rdma_cm_id, addr : *mut bnd_linux::libc::socket::sockaddr) -> i32);
#[cfg(all(feature = "sa", feature = "verbs"))]
windows_link::link!("rdmacm" "C" fn rdma_connect(id : *mut rdma_cm_id, conn_param : *mut rdma_conn_param) -> i32);
#[cfg(all(feature = "sa", feature = "verbs"))]
windows_link::link!("rdmacm" "C" fn rdma_create_ep(id : *mut *mut rdma_cm_id, res : *mut rdma_addrinfo, pd : *mut super::verbs::ibv_pd, qp_init_attr : *mut super::verbs::ibv_qp_init_attr) -> i32);
windows_link::link!("rdmacm" "C" fn rdma_create_event_channel() -> *mut rdma_event_channel);
#[cfg(all(feature = "sa", feature = "verbs"))]
windows_link::link!("rdmacm" "C" fn rdma_create_id(channel : *mut rdma_event_channel, id : *mut *mut rdma_cm_id, context : *mut core::ffi::c_void, ps : rdma_port_space) -> i32);
#[cfg(all(feature = "sa", feature = "verbs"))]
windows_link::link!("rdmacm" "C" fn rdma_create_qp(id : *mut rdma_cm_id, pd : *mut super::verbs::ibv_pd, qp_init_attr : *mut super::verbs::ibv_qp_init_attr) -> i32);
#[cfg(all(feature = "sa", feature = "verbs"))]
windows_link::link!("rdmacm" "C" fn rdma_create_qp_ex(id : *mut rdma_cm_id, qp_init_attr : *mut super::verbs::ibv_qp_init_attr_ex) -> i32);
#[cfg(all(feature = "sa", feature = "verbs"))]
windows_link::link!("rdmacm" "C" fn rdma_destroy_ep(id : *mut rdma_cm_id));
windows_link::link!("rdmacm" "C" fn rdma_destroy_event_channel(channel : *mut rdma_event_channel));
#[cfg(all(feature = "sa", feature = "verbs"))]
windows_link::link!("rdmacm" "C" fn rdma_destroy_id(id : *mut rdma_cm_id) -> i32);
#[cfg(all(feature = "sa", feature = "verbs"))]
windows_link::link!("rdmacm" "C" fn rdma_destroy_qp(id : *mut rdma_cm_id));
#[cfg(all(feature = "sa", feature = "verbs"))]
windows_link::link!("rdmacm" "C" fn rdma_disconnect(id : *mut rdma_cm_id) -> i32);
#[cfg(all(feature = "sa", feature = "verbs"))]
windows_link::link!("rdmacm" "C" fn rdma_establish(id : *mut rdma_cm_id) -> i32);
windows_link::link!("rdmacm" "C" fn rdma_event_str(event : rdma_cm_event_type) -> *const i8);
#[cfg(feature = "verbs")]
windows_link::link!("rdmacm" "C" fn rdma_free_devices(list : *mut *mut super::verbs::ibv_context));
windows_link::link!("rdmacm" "C" fn rdma_freeaddrinfo(res : *mut rdma_addrinfo));
#[cfg(all(feature = "sa", feature = "verbs"))]
windows_link::link!("rdmacm" "C" fn rdma_get_cm_event(channel : *mut rdma_event_channel, event : *mut *mut rdma_cm_event) -> i32);
#[cfg(feature = "verbs")]
windows_link::link!("rdmacm" "C" fn rdma_get_devices(num_devices : *mut i32) -> *mut *mut super::verbs::ibv_context);
#[cfg(all(feature = "sa", feature = "verbs"))]
windows_link::link!("rdmacm" "C" fn rdma_get_dst_port(id : *mut rdma_cm_id) -> bnd_linux::libc::types::__be16);
#[cfg(all(feature = "sa", feature = "verbs"))]
windows_link::link!("rdmacm" "C" fn rdma_get_remote_ece(id : *mut rdma_cm_id, ece : *mut super::verbs::ibv_ece) -> i32);
#[cfg(all(feature = "sa", feature = "verbs"))]
windows_link::link!("rdmacm" "C" fn rdma_get_request(listen : *mut rdma_cm_id, id : *mut *mut rdma_cm_id) -> i32);
#[cfg(all(feature = "sa", feature = "verbs"))]
windows_link::link!("rdmacm" "C" fn rdma_get_src_port(id : *mut rdma_cm_id) -> bnd_linux::libc::types::__be16);
windows_link::link!("rdmacm" "C" fn rdma_getaddrinfo(node : *const i8, service : *const i8, hints : *const rdma_addrinfo, res : *mut *mut rdma_addrinfo) -> i32);
#[cfg(all(feature = "sa", feature = "verbs"))]
windows_link::link!("rdmacm" "C" fn rdma_init_qp_attr(id : *mut rdma_cm_id, qp_attr : *mut super::verbs::ibv_qp_attr, qp_attr_mask : *mut i32) -> i32);
#[cfg(all(feature = "sa", feature = "verbs"))]
windows_link::link!("rdmacm" "C" fn rdma_join_multicast(id : *mut rdma_cm_id, addr : *mut bnd_linux::libc::socket::sockaddr, context : *mut core::ffi::c_void) -> i32);
#[cfg(all(feature = "sa", feature = "verbs"))]
windows_link::link!("rdmacm" "C" fn rdma_join_multicast_ex(id : *mut rdma_cm_id, mc_join_attr : *mut rdma_cm_join_mc_attr_ex, context : *mut core::ffi::c_void) -> i32);
#[cfg(all(feature = "sa", feature = "verbs"))]
windows_link::link!("rdmacm" "C" fn rdma_leave_multicast(id : *mut rdma_cm_id, addr : *mut bnd_linux::libc::socket::sockaddr) -> i32);
#[cfg(all(feature = "sa", feature = "verbs"))]
windows_link::link!("rdmacm" "C" fn rdma_listen(id : *mut rdma_cm_id, backlog : i32) -> i32);
#[cfg(all(feature = "sa", feature = "verbs"))]
windows_link::link!("rdmacm" "C" fn rdma_migrate_id(id : *mut rdma_cm_id, channel : *mut rdma_event_channel) -> i32);
#[cfg(all(feature = "sa", feature = "verbs"))]
windows_link::link!("rdmacm" "C" fn rdma_notify(id : *mut rdma_cm_id, event : super::verbs::ibv_event_type) -> i32);
#[cfg(all(feature = "sa", feature = "verbs"))]
windows_link::link!("rdmacm" "C" fn rdma_query_addrinfo(id : *mut rdma_cm_id, info : *mut *mut rdma_addrinfo) -> i32);
#[cfg(all(feature = "sa", feature = "verbs"))]
windows_link::link!("rdmacm" "C" fn rdma_reject(id : *mut rdma_cm_id, private_data : *const core::ffi::c_void, private_data_len : u8) -> i32);
#[cfg(all(feature = "sa", feature = "verbs"))]
windows_link::link!("rdmacm" "C" fn rdma_reject_ece(id : *mut rdma_cm_id, private_data : *const core::ffi::c_void, private_data_len : u8) -> i32);
#[cfg(all(feature = "sa", feature = "verbs"))]
windows_link::link!("rdmacm" "C" fn rdma_resolve_addr(id : *mut rdma_cm_id, src_addr : *mut bnd_linux::libc::socket::sockaddr, dst_addr : *mut bnd_linux::libc::socket::sockaddr, timeout_ms : i32) -> i32);
#[cfg(all(feature = "sa", feature = "verbs"))]
windows_link::link!("rdmacm" "C" fn rdma_resolve_addrinfo(id : *mut rdma_cm_id, node : *const i8, service : *const i8, hints : *const rdma_addrinfo) -> i32);
#[cfg(all(feature = "sa", feature = "verbs"))]
windows_link::link!("rdmacm" "C" fn rdma_resolve_route(id : *mut rdma_cm_id, timeout_ms : i32) -> i32);
#[cfg(all(feature = "sa", feature = "verbs"))]
windows_link::link!("rdmacm" "C" fn rdma_set_local_ece(id : *mut rdma_cm_id, ece : *mut super::verbs::ibv_ece) -> i32);
#[cfg(all(feature = "sa", feature = "verbs"))]
windows_link::link!("rdmacm" "C" fn rdma_set_option(id : *mut rdma_cm_id, level : i32, optname : i32, optval : *mut core::ffi::c_void, optlen : usize) -> i32);
#[cfg(all(feature = "sa", feature = "verbs"))]
windows_link::link!("rdmacm" "C" fn rdma_write_cm_event(id : *mut rdma_cm_id, event : rdma_cm_event_type, status : i32, arg : u64) -> i32);
pub const RAI_DNS: i32 = 32;
pub const RAI_FAMILY: i32 = 8;
pub const RAI_NOROUTE: i32 = 4;
pub const RAI_NUMERICHOST: i32 = 2;
pub const RAI_PASSIVE: i32 = 1;
pub const RAI_SA: i32 = 16;
pub const RDMA_CM_EVENT_ADDRINFO_ERROR: rdma_cm_event_type = 17;
pub const RDMA_CM_EVENT_ADDRINFO_RESOLVED: rdma_cm_event_type = 16;
pub const RDMA_CM_EVENT_ADDR_CHANGE: rdma_cm_event_type = 14;
pub const RDMA_CM_EVENT_ADDR_ERROR: rdma_cm_event_type = 1;
pub const RDMA_CM_EVENT_ADDR_RESOLVED: rdma_cm_event_type = 0;
pub const RDMA_CM_EVENT_CONNECT_ERROR: rdma_cm_event_type = 6;
pub const RDMA_CM_EVENT_CONNECT_REQUEST: rdma_cm_event_type = 4;
pub const RDMA_CM_EVENT_CONNECT_RESPONSE: rdma_cm_event_type = 5;
pub const RDMA_CM_EVENT_DEVICE_REMOVAL: rdma_cm_event_type = 11;
pub const RDMA_CM_EVENT_DISCONNECTED: rdma_cm_event_type = 10;
pub const RDMA_CM_EVENT_ESTABLISHED: rdma_cm_event_type = 9;
pub const RDMA_CM_EVENT_INTERNAL: rdma_cm_event_type = 19;
pub const RDMA_CM_EVENT_MULTICAST_ERROR: rdma_cm_event_type = 13;
pub const RDMA_CM_EVENT_MULTICAST_JOIN: rdma_cm_event_type = 12;
pub const RDMA_CM_EVENT_REJECTED: rdma_cm_event_type = 8;
pub const RDMA_CM_EVENT_ROUTE_ERROR: rdma_cm_event_type = 3;
pub const RDMA_CM_EVENT_ROUTE_RESOLVED: rdma_cm_event_type = 2;
pub const RDMA_CM_EVENT_TIMEWAIT_EXIT: rdma_cm_event_type = 15;
pub const RDMA_CM_EVENT_UNREACHABLE: rdma_cm_event_type = 7;
pub const RDMA_CM_EVENT_USER: rdma_cm_event_type = 18;
pub const RDMA_CM_JOIN_MC_ATTR_ADDRESS: rdma_cm_join_mc_attr_mask = 1;
pub const RDMA_CM_JOIN_MC_ATTR_JOIN_FLAGS: rdma_cm_join_mc_attr_mask = 2;
pub const RDMA_CM_JOIN_MC_ATTR_RESERVED: rdma_cm_join_mc_attr_mask = 4;
pub const RDMA_IB_IP_PORT_MASK: u64 = 65535;
pub const RDMA_IB_IP_PS_MASK: u64 = 18446744073709486080;
pub const RDMA_IB_IP_PS_TCP: u64 = 17170432;
pub const RDMA_IB_IP_PS_UDP: u64 = 17891328;
pub const RDMA_IB_PS_IB: u64 = 20905984;
pub const RDMA_MAX_INIT_DEPTH: u32 = 255;
pub const RDMA_MAX_RESP_RES: u32 = 255;
pub const RDMA_MC_JOIN_FLAG_FULLMEMBER: rdma_cm_mc_join_flags = 0;
pub const RDMA_MC_JOIN_FLAG_RESERVED: rdma_cm_mc_join_flags = 2;
pub const RDMA_MC_JOIN_FLAG_SENDONLY_FULLMEMBER: rdma_cm_mc_join_flags = 1;
pub const RDMA_OPTION_IB: u32 = 1;
pub const RDMA_OPTION_IB_PATH: u32 = 1;
pub const RDMA_OPTION_ID: u32 = 0;
pub const RDMA_OPTION_ID_ACK_TIMEOUT: u32 = 3;
pub const RDMA_OPTION_ID_AFONLY: u32 = 2;
pub const RDMA_OPTION_ID_REUSEADDR: u32 = 1;
pub const RDMA_OPTION_ID_TOS: u32 = 0;
pub const RDMA_PS_IB: rdma_port_space = 319;
pub const RDMA_PS_IPOIB: rdma_port_space = 2;
pub const RDMA_PS_TCP: rdma_port_space = 262;
pub const RDMA_PS_UDP: rdma_port_space = 273;
pub const RDMA_UDP_QKEY: i32 = 19088743;
#[repr(C)]
#[cfg(feature = "verbs")]
#[derive(Clone, Copy)]
pub struct rdma_addr {
    pub Anonymous: rdma_addr_0,
    pub Anonymous2: rdma_addr_1,
    pub addr: rdma_addr_2,
}
#[cfg(feature = "verbs")]
impl Default for rdma_addr {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[cfg(feature = "verbs")]
#[derive(Clone, Copy)]
pub union rdma_addr_0 {
    pub src_addr: bnd_linux::libc::socket::sockaddr,
    pub src_sin: bnd_linux::libc::in_::sockaddr_in,
    pub src_sin6: bnd_linux::libc::in_::sockaddr_in6,
    pub src_storage: bnd_linux::libc::socket::sockaddr_storage,
}
#[cfg(feature = "verbs")]
impl Default for rdma_addr_0 {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[cfg(feature = "verbs")]
#[derive(Clone, Copy)]
pub union rdma_addr_1 {
    pub dst_addr: bnd_linux::libc::socket::sockaddr,
    pub dst_sin: bnd_linux::libc::in_::sockaddr_in,
    pub dst_sin6: bnd_linux::libc::in_::sockaddr_in6,
    pub dst_storage: bnd_linux::libc::socket::sockaddr_storage,
}
#[cfg(feature = "verbs")]
impl Default for rdma_addr_1 {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[cfg(feature = "verbs")]
#[derive(Clone, Copy)]
pub union rdma_addr_2 {
    pub ibaddr: rdma_ib_addr,
}
#[cfg(feature = "verbs")]
impl Default for rdma_addr_2 {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct rdma_addrinfo {
    pub ai_flags: i32,
    pub ai_family: i32,
    pub ai_qp_type: i32,
    pub ai_port_space: i32,
    pub ai_src_len: bnd_linux::libc::unistd::socklen_t,
    pub ai_dst_len: bnd_linux::libc::unistd::socklen_t,
    pub ai_src_addr: *mut bnd_linux::libc::socket::sockaddr,
    pub ai_dst_addr: *mut bnd_linux::libc::socket::sockaddr,
    pub ai_src_canonname: *mut i8,
    pub ai_dst_canonname: *mut i8,
    pub ai_route_len: usize,
    pub ai_route: *mut core::ffi::c_void,
    pub ai_connect_len: usize,
    pub ai_connect: *mut core::ffi::c_void,
    pub ai_next: *mut Self,
}
#[repr(C)]
#[cfg(all(feature = "sa", feature = "verbs"))]
#[derive(Clone, Copy)]
pub struct rdma_cm_event {
    pub id: *mut rdma_cm_id,
    pub listen_id: *mut rdma_cm_id,
    pub event: rdma_cm_event_type,
    pub status: i32,
    pub param: rdma_cm_event_0,
}
#[cfg(all(feature = "sa", feature = "verbs"))]
impl Default for rdma_cm_event {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[cfg(all(feature = "sa", feature = "verbs"))]
#[derive(Clone, Copy)]
pub union rdma_cm_event_0 {
    pub conn: rdma_conn_param,
    pub ud: rdma_ud_param,
    pub arg: u64,
}
#[cfg(all(feature = "sa", feature = "verbs"))]
impl Default for rdma_cm_event_0 {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
pub type rdma_cm_event_type = u32;
#[repr(C)]
#[cfg(all(feature = "sa", feature = "verbs"))]
#[derive(Clone, Copy)]
pub struct rdma_cm_id {
    pub verbs: *mut super::verbs::ibv_context,
    pub channel: *mut rdma_event_channel,
    pub context: *mut core::ffi::c_void,
    pub qp: *mut super::verbs::ibv_qp,
    pub route: rdma_route,
    pub ps: rdma_port_space,
    pub port_num: u8,
    pub event: *mut rdma_cm_event,
    pub send_cq_channel: *mut super::verbs::ibv_comp_channel,
    pub send_cq: *mut super::verbs::ibv_cq,
    pub recv_cq_channel: *mut super::verbs::ibv_comp_channel,
    pub recv_cq: *mut super::verbs::ibv_cq,
    pub srq: *mut super::verbs::ibv_srq,
    pub pd: *mut super::verbs::ibv_pd,
    pub qp_type: super::verbs::ibv_qp_type,
}
#[cfg(all(feature = "sa", feature = "verbs"))]
impl Default for rdma_cm_id {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct rdma_cm_join_mc_attr_ex {
    pub comp_mask: u32,
    pub join_flags: u32,
    pub addr: *mut bnd_linux::libc::socket::sockaddr,
}
pub type rdma_cm_join_mc_attr_mask = u32;
pub type rdma_cm_mc_join_flags = u32;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct rdma_conn_param {
    pub private_data: *const core::ffi::c_void,
    pub private_data_len: u8,
    pub responder_resources: u8,
    pub initiator_depth: u8,
    pub flow_control: u8,
    pub retry_count: u8,
    pub rnr_retry_count: u8,
    pub srq: u8,
    pub qp_num: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct rdma_event_channel {
    pub fd: i32,
}
#[repr(C)]
#[cfg(feature = "verbs")]
#[derive(Clone, Copy)]
pub struct rdma_ib_addr {
    pub sgid: super::verbs::ibv_gid,
    pub dgid: super::verbs::ibv_gid,
    pub pkey: bnd_linux::libc::types::__be16,
}
#[cfg(feature = "verbs")]
impl Default for rdma_ib_addr {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
pub type rdma_port_space = u32;
#[repr(C)]
#[cfg(all(feature = "sa", feature = "verbs"))]
#[derive(Clone, Copy)]
pub struct rdma_route {
    pub addr: rdma_addr,
    pub path_rec: *mut super::sa::ibv_sa_path_rec,
    pub num_paths: i32,
}
#[cfg(all(feature = "sa", feature = "verbs"))]
impl Default for rdma_route {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[cfg(feature = "verbs")]
#[derive(Clone, Copy)]
pub struct rdma_ud_param {
    pub private_data: *const core::ffi::c_void,
    pub private_data_len: u8,
    pub ah_attr: super::verbs::ibv_ah_attr,
    pub qp_num: u32,
    pub qkey: u32,
}
#[cfg(feature = "verbs")]
impl Default for rdma_ud_param {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
