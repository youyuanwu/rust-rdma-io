pub const IBV_PATH_FLAG_ALTERNATE: i32 = 4;
pub const IBV_PATH_FLAG_BIDIRECTIONAL: i32 = 40;
pub const IBV_PATH_FLAG_GMP: i32 = 1;
pub const IBV_PATH_FLAG_INBOUND: i32 = 16;
pub const IBV_PATH_FLAG_INBOUND_REVERSE: i32 = 32;
pub const IBV_PATH_FLAG_OUTBOUND: i32 = 8;
pub const IBV_PATH_FLAG_PRIMARY: i32 = 2;
pub const IBV_PATH_RECORD_REVERSIBLE: i32 = 128;
#[repr(C)]
#[cfg(feature = "verbs")]
#[derive(Clone, Copy)]
pub struct ibv_path_data {
    pub flags: u32,
    pub reserved: u32,
    pub path: ibv_path_record,
}
#[cfg(feature = "verbs")]
impl Default for ibv_path_data {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[cfg(feature = "verbs")]
#[derive(Clone, Copy)]
pub struct ibv_path_record {
    pub service_id: bnd_linux::libc::types::__be64,
    pub dgid: super::verbs::ibv_gid,
    pub sgid: super::verbs::ibv_gid,
    pub dlid: bnd_linux::libc::types::__be16,
    pub slid: bnd_linux::libc::types::__be16,
    pub flowlabel_hoplimit: bnd_linux::libc::types::__be32,
    pub tclass: u8,
    pub reversible_numpath: u8,
    pub pkey: bnd_linux::libc::types::__be16,
    pub qosclass_sl: bnd_linux::libc::types::__be16,
    pub mtu: u8,
    pub rate: u8,
    pub packetlifetime: u8,
    pub preference: u8,
    pub reserved: [u8; 6],
}
#[cfg(feature = "verbs")]
impl Default for ibv_path_record {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[cfg(feature = "verbs")]
#[derive(Clone, Copy)]
pub struct ibv_sa_mcmember_rec {
    pub mgid: super::verbs::ibv_gid,
    pub port_gid: super::verbs::ibv_gid,
    pub qkey: u32,
    pub mlid: u16,
    pub mtu_selector: u8,
    pub mtu: u8,
    pub traffic_class: u8,
    pub pkey: u16,
    pub rate_selector: u8,
    pub rate: u8,
    pub packet_life_time_selector: u8,
    pub packet_life_time: u8,
    pub sl: u8,
    pub flow_label: u32,
    pub hop_limit: u8,
    pub scope: u8,
    pub join_state: u8,
    pub proxy_join: i32,
}
#[cfg(feature = "verbs")]
impl Default for ibv_sa_mcmember_rec {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[cfg(feature = "verbs")]
#[derive(Clone, Copy)]
pub struct ibv_sa_path_rec {
    pub dgid: super::verbs::ibv_gid,
    pub sgid: super::verbs::ibv_gid,
    pub dlid: bnd_linux::libc::types::__be16,
    pub slid: bnd_linux::libc::types::__be16,
    pub raw_traffic: i32,
    pub flow_label: bnd_linux::libc::types::__be32,
    pub hop_limit: u8,
    pub traffic_class: u8,
    pub reversible: i32,
    pub numb_path: u8,
    pub pkey: bnd_linux::libc::types::__be16,
    pub sl: u8,
    pub mtu_selector: u8,
    pub mtu: u8,
    pub rate_selector: u8,
    pub rate: u8,
    pub packet_life_time_selector: u8,
    pub packet_life_time: u8,
    pub preference: u8,
}
#[cfg(feature = "verbs")]
impl Default for ibv_sa_path_rec {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
#[repr(C)]
#[cfg(feature = "verbs")]
#[derive(Clone, Copy)]
pub struct ibv_sa_service_rec {
    pub id: u64,
    pub gid: super::verbs::ibv_gid,
    pub pkey: u16,
    pub lease: u32,
    pub key: [u8; 16],
    pub name: [u8; 64],
    pub data8: [u8; 16],
    pub data16: [u16; 8],
    pub data32: [u32; 4],
    pub data64: [u64; 2],
}
#[cfg(feature = "verbs")]
impl Default for ibv_sa_service_rec {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
