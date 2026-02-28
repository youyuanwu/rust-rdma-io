#include <infiniband/verbs.h>
#include "wrapper.h"

void rdma_wrap_ibv_wr_atomic_cmp_swp(struct ibv_qp_ex *qp, uint32_t rkey, uint64_t remote_addr, uint64_t compare, uint64_t swap) {
    ibv_wr_atomic_cmp_swp(qp, rkey, remote_addr, compare, swap);
}

void rdma_wrap_ibv_wr_atomic_fetch_add(struct ibv_qp_ex *qp, uint32_t rkey, uint64_t remote_addr, uint64_t add) {
    ibv_wr_atomic_fetch_add(qp, rkey, remote_addr, add);
}

void rdma_wrap_ibv_wr_bind_mw(struct ibv_qp_ex *qp, struct ibv_mw *mw, uint32_t rkey, const struct ibv_mw_bind_info *bind_info) {
    ibv_wr_bind_mw(qp, mw, rkey, bind_info);
}

void rdma_wrap_ibv_wr_local_inv(struct ibv_qp_ex *qp, uint32_t invalidate_rkey) {
    ibv_wr_local_inv(qp, invalidate_rkey);
}

void rdma_wrap_ibv_wr_rdma_read(struct ibv_qp_ex *qp, uint32_t rkey, uint64_t remote_addr) {
    ibv_wr_rdma_read(qp, rkey, remote_addr);
}

void rdma_wrap_ibv_wr_rdma_write(struct ibv_qp_ex *qp, uint32_t rkey, uint64_t remote_addr) {
    ibv_wr_rdma_write(qp, rkey, remote_addr);
}

void rdma_wrap_ibv_wr_flush(struct ibv_qp_ex *qp, uint32_t rkey, uint64_t remote_addr, size_t len, uint8_t type, uint8_t level) {
    ibv_wr_flush(qp, rkey, remote_addr, len, type, level);
}

void rdma_wrap_ibv_wr_rdma_write_imm(struct ibv_qp_ex *qp, uint32_t rkey, uint64_t remote_addr, __be32 imm_data) {
    ibv_wr_rdma_write_imm(qp, rkey, remote_addr, imm_data);
}

void rdma_wrap_ibv_wr_send(struct ibv_qp_ex *qp) {
    ibv_wr_send(qp);
}

void rdma_wrap_ibv_wr_send_imm(struct ibv_qp_ex *qp, __be32 imm_data) {
    ibv_wr_send_imm(qp, imm_data);
}

void rdma_wrap_ibv_wr_send_inv(struct ibv_qp_ex *qp, uint32_t invalidate_rkey) {
    ibv_wr_send_inv(qp, invalidate_rkey);
}

void rdma_wrap_ibv_wr_send_tso(struct ibv_qp_ex *qp, void *hdr, uint16_t hdr_sz, uint16_t mss) {
    ibv_wr_send_tso(qp, hdr, hdr_sz, mss);
}

void rdma_wrap_ibv_wr_set_ud_addr(struct ibv_qp_ex *qp, struct ibv_ah *ah, uint32_t remote_qpn, uint32_t remote_qkey) {
    ibv_wr_set_ud_addr(qp, ah, remote_qpn, remote_qkey);
}

void rdma_wrap_ibv_wr_set_xrc_srqn(struct ibv_qp_ex *qp, uint32_t remote_srqn) {
    ibv_wr_set_xrc_srqn(qp, remote_srqn);
}

void rdma_wrap_ibv_wr_set_inline_data(struct ibv_qp_ex *qp, void *addr, size_t length) {
    ibv_wr_set_inline_data(qp, addr, length);
}

void rdma_wrap_ibv_wr_set_inline_data_list(struct ibv_qp_ex *qp, size_t num_buf, const struct ibv_data_buf *buf_list) {
    ibv_wr_set_inline_data_list(qp, num_buf, buf_list);
}

void rdma_wrap_ibv_wr_set_sge(struct ibv_qp_ex *qp, uint32_t lkey, uint64_t addr, uint32_t length) {
    ibv_wr_set_sge(qp, lkey, addr, length);
}

void rdma_wrap_ibv_wr_set_sge_list(struct ibv_qp_ex *qp, size_t num_sge, const struct ibv_sge *sg_list) {
    ibv_wr_set_sge_list(qp, num_sge, sg_list);
}

void rdma_wrap_ibv_wr_start(struct ibv_qp_ex *qp) {
    ibv_wr_start(qp);
}

int rdma_wrap_ibv_wr_complete(struct ibv_qp_ex *qp) {
    return ibv_wr_complete(qp);
}

void rdma_wrap_ibv_wr_abort(struct ibv_qp_ex *qp) {
    ibv_wr_abort(qp);
}

void rdma_wrap_ibv_wr_atomic_write(struct ibv_qp_ex *qp, uint32_t rkey, uint64_t remote_addr, const void *atomic_wr) {
    ibv_wr_atomic_write(qp, rkey, remote_addr, atomic_wr);
}

struct ibv_cq *rdma_wrap_ibv_cq_ex_to_cq(struct ibv_cq_ex *cq) {
    return ibv_cq_ex_to_cq(cq);
}

int rdma_wrap_ibv_start_poll(struct ibv_cq_ex *cq, struct ibv_poll_cq_attr *attr) {
    return ibv_start_poll(cq, attr);
}

int rdma_wrap_ibv_next_poll(struct ibv_cq_ex *cq) {
    return ibv_next_poll(cq);
}

void rdma_wrap_ibv_end_poll(struct ibv_cq_ex *cq) {
    ibv_end_poll(cq);
}

enum ibv_wc_opcode rdma_wrap_ibv_wc_read_opcode(struct ibv_cq_ex *cq) {
    return ibv_wc_read_opcode(cq);
}

uint32_t rdma_wrap_ibv_wc_read_vendor_err(struct ibv_cq_ex *cq) {
    return ibv_wc_read_vendor_err(cq);
}

uint32_t rdma_wrap_ibv_wc_read_byte_len(struct ibv_cq_ex *cq) {
    return ibv_wc_read_byte_len(cq);
}

__be32 rdma_wrap_ibv_wc_read_imm_data(struct ibv_cq_ex *cq) {
    return ibv_wc_read_imm_data(cq);
}

uint32_t rdma_wrap_ibv_wc_read_invalidated_rkey(struct ibv_cq_ex *cq) {
    return ibv_wc_read_invalidated_rkey(cq);
}

uint32_t rdma_wrap_ibv_wc_read_qp_num(struct ibv_cq_ex *cq) {
    return ibv_wc_read_qp_num(cq);
}

uint32_t rdma_wrap_ibv_wc_read_src_qp(struct ibv_cq_ex *cq) {
    return ibv_wc_read_src_qp(cq);
}

unsigned int rdma_wrap_ibv_wc_read_wc_flags(struct ibv_cq_ex *cq) {
    return ibv_wc_read_wc_flags(cq);
}

uint32_t rdma_wrap_ibv_wc_read_slid(struct ibv_cq_ex *cq) {
    return ibv_wc_read_slid(cq);
}

uint8_t rdma_wrap_ibv_wc_read_sl(struct ibv_cq_ex *cq) {
    return ibv_wc_read_sl(cq);
}

uint8_t rdma_wrap_ibv_wc_read_dlid_path_bits(struct ibv_cq_ex *cq) {
    return ibv_wc_read_dlid_path_bits(cq);
}

uint64_t rdma_wrap_ibv_wc_read_completion_ts(struct ibv_cq_ex *cq) {
    return ibv_wc_read_completion_ts(cq);
}

uint64_t rdma_wrap_ibv_wc_read_completion_wallclock_ns(struct ibv_cq_ex *cq) {
    return ibv_wc_read_completion_wallclock_ns(cq);
}

uint16_t rdma_wrap_ibv_wc_read_cvlan(struct ibv_cq_ex *cq) {
    return ibv_wc_read_cvlan(cq);
}

uint32_t rdma_wrap_ibv_wc_read_flow_tag(struct ibv_cq_ex *cq) {
    return ibv_wc_read_flow_tag(cq);
}

void rdma_wrap_ibv_wc_read_tm_info(struct ibv_cq_ex *cq, struct ibv_wc_tm_info *tm_info) {
    ibv_wc_read_tm_info(cq, tm_info);
}

int rdma_wrap_ibv_post_wq_recv(struct ibv_wq *wq, struct ibv_recv_wr *recv_wr, struct ibv_recv_wr **bad_recv_wr) {
    return ibv_post_wq_recv(wq, recv_wr, bad_recv_wr);
}

struct verbs_context *rdma_wrap_verbs_get_ctx(struct ibv_context *ctx) {
    return verbs_get_ctx(ctx);
}

int rdma_wrap____ibv_query_port(struct ibv_context *context, uint8_t port_num, struct ibv_port_attr *port_attr) {
    return ___ibv_query_port(context, port_num, port_attr);
}

int rdma_wrap_ibv_query_gid_ex(struct ibv_context *context, uint32_t port_num, uint32_t gid_index, struct ibv_gid_entry *entry, uint32_t flags) {
    return ibv_query_gid_ex(context, port_num, gid_index, entry, flags);
}

ssize_t rdma_wrap_ibv_query_gid_table(struct ibv_context *context, struct ibv_gid_entry *entries, size_t max_entries, uint32_t flags) {
    return ibv_query_gid_table(context, entries, max_entries, flags);
}

struct ibv_flow *rdma_wrap_ibv_create_flow(struct ibv_qp *qp, struct ibv_flow_attr *flow) {
    return ibv_create_flow(qp, flow);
}

int rdma_wrap_ibv_destroy_flow(struct ibv_flow *flow_id) {
    return ibv_destroy_flow(flow_id);
}

struct ibv_flow_action *rdma_wrap_ibv_create_flow_action_esp(struct ibv_context *ctx, struct ibv_flow_action_esp_attr *esp) {
    return ibv_create_flow_action_esp(ctx, esp);
}

int rdma_wrap_ibv_modify_flow_action_esp(struct ibv_flow_action *action, struct ibv_flow_action_esp_attr *esp) {
    return ibv_modify_flow_action_esp(action, esp);
}

int rdma_wrap_ibv_destroy_flow_action(struct ibv_flow_action *action) {
    return ibv_destroy_flow_action(action);
}

struct ibv_xrcd *rdma_wrap_ibv_open_xrcd(struct ibv_context *context, struct ibv_xrcd_init_attr *xrcd_init_attr) {
    return ibv_open_xrcd(context, xrcd_init_attr);
}

int rdma_wrap_ibv_close_xrcd(struct ibv_xrcd *xrcd) {
    return ibv_close_xrcd(xrcd);
}

struct ibv_mr *rdma_wrap___ibv_reg_mr(struct ibv_pd *pd, void *addr, size_t length, unsigned int access, int is_access_const) {
    return __ibv_reg_mr(pd, addr, length, access, is_access_const);
}

struct ibv_mr *rdma_wrap___ibv_reg_mr_iova(struct ibv_pd *pd, void *addr, size_t length, uint64_t iova, unsigned int access, int is_access_const) {
    return __ibv_reg_mr_iova(pd, addr, length, iova, access, is_access_const);
}

struct ibv_mw *rdma_wrap_ibv_alloc_mw(struct ibv_pd *pd, enum ibv_mw_type type) {
    return ibv_alloc_mw(pd, type);
}

int rdma_wrap_ibv_dealloc_mw(struct ibv_mw *mw) {
    return ibv_dealloc_mw(mw);
}

uint32_t rdma_wrap_ibv_inc_rkey(uint32_t rkey) {
    return ibv_inc_rkey(rkey);
}

int rdma_wrap_ibv_bind_mw(struct ibv_qp *qp, struct ibv_mw *mw, struct ibv_mw_bind *mw_bind) {
    return ibv_bind_mw(qp, mw, mw_bind);
}

int rdma_wrap_ibv_advise_mr(struct ibv_pd *pd, enum ibv_advise_mr_advice advice, uint32_t flags, struct ibv_sge *sg_list, uint32_t num_sge) {
    return ibv_advise_mr(pd, advice, flags, sg_list, num_sge);
}

struct ibv_dm *rdma_wrap_ibv_alloc_dm(struct ibv_context *context, struct ibv_alloc_dm_attr *attr) {
    return ibv_alloc_dm(context, attr);
}

int rdma_wrap_ibv_free_dm(struct ibv_dm *dm) {
    return ibv_free_dm(dm);
}

int rdma_wrap_ibv_memcpy_to_dm(struct ibv_dm *dm, uint64_t dm_offset, const void *host_addr, size_t length) {
    return ibv_memcpy_to_dm(dm, dm_offset, host_addr, length);
}

int rdma_wrap_ibv_memcpy_from_dm(void *host_addr, struct ibv_dm *dm, uint64_t dm_offset, size_t length) {
    return ibv_memcpy_from_dm(host_addr, dm, dm_offset, length);
}

struct ibv_mr *rdma_wrap_ibv_alloc_null_mr(struct ibv_pd *pd) {
    return ibv_alloc_null_mr(pd);
}

struct ibv_mr *rdma_wrap_ibv_reg_dm_mr(struct ibv_pd *pd, struct ibv_dm *dm, uint64_t dm_offset, size_t length, unsigned int access) {
    return ibv_reg_dm_mr(pd, dm, dm_offset, length, access);
}

struct ibv_cq_ex *rdma_wrap_ibv_create_cq_ex(struct ibv_context *context, struct ibv_cq_init_attr_ex *cq_attr) {
    return ibv_create_cq_ex(context, cq_attr);
}

int rdma_wrap_ibv_poll_cq(struct ibv_cq *cq, int num_entries, struct ibv_wc *wc) {
    return ibv_poll_cq(cq, num_entries, wc);
}

int rdma_wrap_ibv_req_notify_cq(struct ibv_cq *cq, int solicited_only) {
    return ibv_req_notify_cq(cq, solicited_only);
}

int rdma_wrap_ibv_modify_cq(struct ibv_cq *cq, struct ibv_modify_cq_attr *attr) {
    return ibv_modify_cq(cq, attr);
}

struct ibv_srq *rdma_wrap_ibv_create_srq_ex(struct ibv_context *context, struct ibv_srq_init_attr_ex *srq_init_attr_ex) {
    return ibv_create_srq_ex(context, srq_init_attr_ex);
}

int rdma_wrap_ibv_get_srq_num(struct ibv_srq *srq, uint32_t *srq_num) {
    return ibv_get_srq_num(srq, srq_num);
}

int rdma_wrap_ibv_post_srq_recv(struct ibv_srq *srq, struct ibv_recv_wr *recv_wr, struct ibv_recv_wr **bad_recv_wr) {
    return ibv_post_srq_recv(srq, recv_wr, bad_recv_wr);
}

int rdma_wrap_ibv_post_srq_ops(struct ibv_srq *srq, struct ibv_ops_wr *op, struct ibv_ops_wr **bad_op) {
    return ibv_post_srq_ops(srq, op, bad_op);
}

struct ibv_qp *rdma_wrap_ibv_create_qp_ex(struct ibv_context *context, struct ibv_qp_init_attr_ex *qp_init_attr_ex) {
    return ibv_create_qp_ex(context, qp_init_attr_ex);
}

struct ibv_td *rdma_wrap_ibv_alloc_td(struct ibv_context *context, struct ibv_td_init_attr *init_attr) {
    return ibv_alloc_td(context, init_attr);
}

int rdma_wrap_ibv_dealloc_td(struct ibv_td *td) {
    return ibv_dealloc_td(td);
}

struct ibv_pd *rdma_wrap_ibv_alloc_parent_domain(struct ibv_context *context, struct ibv_parent_domain_init_attr *attr) {
    return ibv_alloc_parent_domain(context, attr);
}

int rdma_wrap_ibv_query_rt_values_ex(struct ibv_context *context, struct ibv_values_ex *values) {
    return ibv_query_rt_values_ex(context, values);
}

int rdma_wrap_ibv_query_device_ex(struct ibv_context *context, const struct ibv_query_device_ex_input *input, struct ibv_device_attr_ex *attr) {
    return ibv_query_device_ex(context, input, attr);
}

struct ibv_qp *rdma_wrap_ibv_open_qp(struct ibv_context *context, struct ibv_qp_open_attr *qp_open_attr) {
    return ibv_open_qp(context, qp_open_attr);
}

int rdma_wrap_ibv_modify_qp_rate_limit(struct ibv_qp *qp, struct ibv_qp_rate_limit_attr *attr) {
    return ibv_modify_qp_rate_limit(qp, attr);
}

struct ibv_wq *rdma_wrap_ibv_create_wq(struct ibv_context *context, struct ibv_wq_init_attr *wq_init_attr) {
    return ibv_create_wq(context, wq_init_attr);
}

int rdma_wrap_ibv_modify_wq(struct ibv_wq *wq, struct ibv_wq_attr *wq_attr) {
    return ibv_modify_wq(wq, wq_attr);
}

int rdma_wrap_ibv_destroy_wq(struct ibv_wq *wq) {
    return ibv_destroy_wq(wq);
}

struct ibv_rwq_ind_table *rdma_wrap_ibv_create_rwq_ind_table(struct ibv_context *context, struct ibv_rwq_ind_table_init_attr *init_attr) {
    return ibv_create_rwq_ind_table(context, init_attr);
}

int rdma_wrap_ibv_destroy_rwq_ind_table(struct ibv_rwq_ind_table *rwq_ind_table) {
    return ibv_destroy_rwq_ind_table(rwq_ind_table);
}

int rdma_wrap_ibv_post_send(struct ibv_qp *qp, struct ibv_send_wr *wr, struct ibv_send_wr **bad_wr) {
    return ibv_post_send(qp, wr, bad_wr);
}

int rdma_wrap_ibv_post_recv(struct ibv_qp *qp, struct ibv_recv_wr *wr, struct ibv_recv_wr **bad_wr) {
    return ibv_post_recv(qp, wr, bad_wr);
}

int rdma_wrap_ibv_is_qpt_supported(uint32_t caps, enum ibv_qp_type qpt) {
    return ibv_is_qpt_supported(caps, qpt);
}

struct ibv_counters *rdma_wrap_ibv_create_counters(struct ibv_context *context, struct ibv_counters_init_attr *init_attr) {
    return ibv_create_counters(context, init_attr);
}

int rdma_wrap_ibv_destroy_counters(struct ibv_counters *counters) {
    return ibv_destroy_counters(counters);
}

int rdma_wrap_ibv_attach_counters_point_flow(struct ibv_counters *counters, struct ibv_counter_attach_attr *attr, struct ibv_flow *flow) {
    return ibv_attach_counters_point_flow(counters, attr, flow);
}

int rdma_wrap_ibv_read_counters(struct ibv_counters *counters, uint64_t *counters_value, uint32_t ncounters, uint32_t flags) {
    return ibv_read_counters(counters, counters_value, ncounters, flags);
}

uint16_t rdma_wrap_ibv_flow_label_to_udp_sport(uint32_t fl) {
    return ibv_flow_label_to_udp_sport(fl);
}
