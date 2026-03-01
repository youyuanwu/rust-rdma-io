# RDMA Testing Approaches — Research & Strategy

> Surveyed Feb 2026. Based on analysis of existing Rust RDMA libraries and the broader RDMA testing ecosystem.

---

## 1. How Existing Rust RDMA Libraries Test

### Summary Matrix

| Project | Unit Tests | Integration Tests | CI Tests Run? | Hardware Required? | Simulation Used | Mocking |
|---|---|---|---|---|---|---|
| **rust-ibverbs** | 4 (serde/layout) | Example only | Compile + unit only | No (SoftRoCE suggested) | SoftRoCE recommended | None |
| **async-rdma** | 0 in-src | 10 test files | ✅ Full functional | No | SoftRoCE (rxe) + SoftIWarp (siw) | None |
| **rdma (Nugine)** | 2-3 (FFI layout) | Pingpong examples | Compile + lint only | Needed for examples | rdma-core built from source | None |
| **sideway** | In-src + 6 test files | Pingpong examples | ✅ Full functional | No | SoftRoCE (rxe) | None |
| **rrddmma** | 0 | 8 examples | ❌ No CI | Yes (Mellanox) | None | None |

**Key finding**: No project uses mocking. All functional tests require a real or emulated RDMA device.

---

### 1.1 rust-ibverbs

**Test approach**: Minimal unit tests (4 total: serde encode/decode, GID conversion, memory layout). The `loopback.rs` example serves as an informal integration test but is not automated.

**CI**: GitHub Actions runs `cargo test` but this only exercises the 4 unit tests. Actual RDMA operations are not tested in CI. `configure-ci.sh` installs `libibverbs-dev` for compilation but does not set up an rxe device.

**Lesson**: Compilation checks are necessary but not sufficient. The library has minimal test confidence for actual RDMA operations.

### 1.2 async-rdma

**Test approach**: The most comprehensive test suite among Rust RDMA libraries.

- **10 integration tests**: atomics, cancel safety, device detection, immediate data, MR slicing, remote MR access/timeout, event loops, timing
- **Shared test utilities** (`test_utilities.rs`): Server/client helpers using `RdmaBuilder`, port picker for localhost testing, tokio runtime
- **CI setup**: Installs `rdma-core` packages, creates **both** SoftRoCE (rxe) and SoftIWarp (siw) devices, runs full test suite with `ulimit -l unlimited`
- **Example-as-test**: Examples (rpc, server/client) are also executed in CI
- **Verification**: Uses `ibv_rc_pingpong`, `ibv_uc_pingpong`, `ibv_ud_pingpong`, `ibv_srq_pingpong` standard tools as validation

**CI commands** (from `scripts/run.sh`):
```bash
# Setup rxe device
sudo rdma link add rxe_eth0 type rxe netdev eth0
# Run tests with unlimited locked memory
sudo bash -c 'ulimit -l unlimited && cargo test --features="cm raw"'
```

**Lesson**: The server/client loopback pattern over rxe works well for async RDMA testing. The dual rxe+siw setup tests across transport implementations.

### 1.3 rdma (Nugine)

**Test approach**: Minimal inline tests checking FFI layout compatibility (struct sizes, field offsets via `offset_of!`). Functional testing via `rdma-pingpong` and `rdma-async` examples.

**CI**: Builds rdma-core from source, runs format/clippy checks, but does NOT run functional tests or create rxe devices. `cargo test` is only in the local `just dev` command.

**Lesson**: FFI layout tests are valuable for safety — they catch ABI mismatches at compile/test time. Worth adopting.

### 1.4 sideway

**Test approach**: The most sophisticated test infrastructure.

- **Unit tests**: CQ tests, MR+CQ tests, QP state transition tests, post_send tests
- **Compile-fail tests** (`trybuild`): Validates that Rust's type system prevents misuse:
  - `one_guard_has_only_one_handle.rs` — prevents multiple handles per guard
  - `one_guard_has_only_one_wr.rs` — prevents multiple WRs per guard
  - `one_qp_has_only_one_guard.rs` — ensures single concurrent guard per QP
- **Dual CI**: GitHub Actions (smoke/compile) + Cirrus CI (full functional with rxe)
- **Cirrus CI**: Builds rdma-core from source on Rocky Linux, creates rxe device, runs integration tests + examples as coverage targets
- **Coverage**: `cargo-llvm-cov` with Codecov integration

**Lesson**: Compile-fail tests (`trybuild`) are excellent for RDMA safety verification — they prove the type system prevents common RDMA misuse patterns. The dual-CI strategy (lightweight + heavy) is pragmatic.

### 1.5 rrddmma

**Test approach**: Zero tests, zero CI. Academic/research library relying entirely on manual example execution on real Mellanox hardware.

**Lesson**: Don't follow this approach.

---

## 2. Available RDMA Software Simulation Options

### 2.1 SoftRoCE / RXE (Recommended for CI)

**What**: Linux kernel module (`rdma_rxe`) that implements RoCEv2 (RDMA over Converged Ethernet) in software over standard Ethernet interfaces.

**Setup**:
```bash
# Load kernel module
sudo modprobe rdma_rxe
# Create device bound to a network interface
sudo rdma link add rxe0 type rxe netdev eth0
# Verify
rdma link
# Should show: link rxe0/1 state ACTIVE ...
```

**Capabilities**:
- Full ibverbs API support (RC, UC, UD queue pairs)
- Send/Receive, RDMA Read/Write, Atomic operations
- Memory registration, protection domains, completion queues
- Works with loopback (`127.0.0.1`)
- Available in all modern Linux kernels (>= 4.8)

**Limitations**:
- No performance testing (software overhead)
- Some edge-case behaviors differ from hardware
- MTU may need to be set to 1024 (not 4096)
- Requires `ulimit -l unlimited` for memory registration
- Requires root/sudo or appropriate capabilities (CAP_NET_RAW, CAP_IPC_LOCK)

**CI compatibility**: Works on GitHub Actions Ubuntu runners, Cirrus CI, most Linux CI environments.

### 2.2 SoftIWarp / SIW

**What**: Linux kernel module (`siw`) implementing iWARP (RDMA over TCP/IP) in software.

**Setup**:
```bash
sudo modprobe siw
sudo rdma link add siw0 type siw netdev eth0
```

**Capabilities**: Similar to rxe but uses iWARP transport. Useful for testing transport-agnostic code.

**CI compatibility**: Same as rxe. async-rdma tests both rxe and siw.

### 2.3 Google rdma-unit-test Framework

**What**: Open-source unit test framework (C++/Bazel) for testing ibverbs implementations. Designed for single-machine, single-NIC loopback testing.

**Relevance to us**: Not directly usable (C++/Bazel), but provides excellent test case inspiration:
- Device/context creation tests
- QP state machine transition tests  
- CQ polling and event-driven completion tests
- Memory registration edge cases
- Error path validation
- Introspection-based test selection (skip tests based on device capabilities)

**Tested with**: Mellanox ConnectX-3/4, SoftRoCE (limited support with `--ipv4_only --verbs_mtu=1024`).

---

## 3. Testing Layers for Our Library

### Layer 1: Pure Rust Unit Tests (No RDMA device needed)

These tests run without any RDMA hardware or emulation:

| What to test | Technique | Example |
|---|---|---|
| FFI struct layout compatibility | `assert_eq!(size_of::<RustType>(), size_of::<CType>())` | Nugine/rdma approach |
| Enum/flag conversions | Standard unit tests | Bitflags roundtrips |
| Builder validation | Test invalid configs return errors | Missing required fields |
| Serialization/deserialization | Serde roundtrip tests | QP endpoint info |
| Error type mapping | `errno` → Rust error mapping | All ibverbs error codes |
| Address/GID parsing | String/byte conversion tests | IPv4/IPv6/GID formats |
| Configuration validation | Bounds checking, defaults | QP capacities, MTU values |
| Type safety invariants | `trybuild` compile-fail tests | Prevent MR-after-free, double-post |

**Requirements**: None. Runs on any platform.

### Layer 2: Device-Level Tests (Needs rxe/siw, no network)

Tests that need an RDMA device but don't send data over the wire:

| What to test | Technique |
|---|---|
| Device enumeration | List devices, check attributes |
| Context creation/teardown | Open/close, verify RAII cleanup |
| Protection Domain lifecycle | Create, verify, drop |
| Completion Queue creation | Various sizes, event channels |
| Memory Registration | Register, deregister, access flags |
| Queue Pair creation | All QP types (RC, UD, UC) |
| QP state transitions | RESET → INIT → RTR → RTS |
| Address Handle creation | For UD operations |
| Shared Receive Queue | Create, modify, query |

**Requirements**: `sudo modprobe rdma_rxe && sudo rdma link add rxe0 type rxe netdev lo`

### Layer 3: Data Path Tests (Needs rxe/siw, loopback)

Full send/receive tests over loopback:

| What to test | Technique |
|---|---|
| RC Send/Receive | Server/client on localhost |
| RDMA Read/Write | One-sided operations |
| UD Send/Receive | Unreliable datagram |
| Atomic CAS/FAA | Compare-and-swap, fetch-and-add |
| Immediate data | Send/write with immediate |
| Scatter-Gather | Multi-buffer operations |
| Inline sends | Small payload inline optimization |
| CQ polling | Poll-based completion |
| CQ events | Event-driven completion notification |
| Error completions | Trigger and handle WC errors |
| Multiple QPs | Multiple connections simultaneously |

**Requirements**: rxe device on a network interface (not just `lo` for some operations). `ulimit -l unlimited`.

### Layer 4: Async Integration Tests (Needs rxe/siw + async runtime)

If we provide async APIs:

| What to test | Technique |
|---|---|
| Async CQ notification | Event channel + async wakeup |
| Concurrent operations | Multiple async tasks, same QP/CQ |
| Cancellation safety | Drop futures mid-operation |
| Timeout handling | Operation timeouts |
| Connection lifecycle | Async connect/disconnect/reconnect |
| Backpressure | CQ full, SQ full scenarios |

---

## 4. Recommended CI Setup

### GitHub Actions Workflow

```yaml
name: CI
on: [push, pull_request]

jobs:
  # Fast checks (no RDMA needed)
  check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: cargo fmt --check
      - run: cargo clippy --all-features -- -D warnings
      - run: cargo test --lib        # Layer 1 only (pure unit tests)
      - run: cargo doc --no-deps

  # Full RDMA tests (rxe device)
  rdma-test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install RDMA dependencies
        run: |
          sudo apt-get update
          sudo apt-get install -y \
            libibverbs-dev librdmacm-dev \
            ibverbs-utils rdmacm-utils \
            ibverbs-providers rdma-core
      - name: Setup SoftRoCE
        run: |
          sudo modprobe rdma_rxe
          # Use eth0 or the first available interface
          NETDEV=$(ip -o link show | awk -F': ' '{print $2}' | grep -v lo | head -1)
          sudo rdma link add rxe0 type rxe netdev $NETDEV
          rdma link  # verify ACTIVE
      - name: Run all tests
        run: |
          sudo bash -c 'ulimit -l unlimited && cargo test --all-features'
      - name: Run examples
        run: |
          sudo bash -c 'ulimit -l unlimited && cargo run --example loopback'
```

### Conditional Test Execution

Tests that require RDMA devices should be gated:

```rust
/// Check if an RDMA device is available
fn rdma_device_available() -> bool {
    // Try to get device list
    // Return false if no devices found
}

/// Skip test if no RDMA device
macro_rules! require_rdma {
    () => {
        if !rdma_device_available() {
            eprintln!("Skipping: no RDMA device available");
            return;
        }
    };
}

#[test]
fn test_create_context() {
    require_rdma!();
    // ... actual test
}
```

Or use a cargo feature flag:

```toml
[features]
rdma-tests = []  # Enable tests that require RDMA device
```

```rust
#[test]
#[cfg(feature = "rdma-tests")]
fn test_create_context() {
    // ...
}
```

### Permissions

RDMA operations typically require:
- `CAP_NET_RAW` — for creating QPs and posting work requests
- `CAP_IPC_LOCK` — for registering memory regions (mlock)
- Or simply run as root: `sudo cargo test`

In CI, use:
```bash
sudo bash -c 'ulimit -l unlimited && cargo test'
```

---

## 5. Testing Best Practices from the Ecosystem

### Adopt from sideway
- **`trybuild` compile-fail tests** — prove type-safety invariants (prevents MR misuse, QP misuse)
- **Dual CI** — lightweight (compile/lint) + heavyweight (full RDMA)
- **Coverage** via `cargo-llvm-cov` + Codecov

### Adopt from async-rdma
- **Server/client test helpers** — reusable `server_wrapper`/`client_wrapper` for loopback tests
- **Port picker** — avoid port conflicts in parallel tests
- **Test both rxe and siw** — catches transport-specific bugs

### Adopt from Nugine/rdma
- **FFI layout assertions** — `size_of`, `align_of`, `offset_of` checks
- **Resource dependency tracking** — test that resources are properly ordered for cleanup

### Adopt from Google rdma-unit-test
- **Introspection-based test selection** — skip tests based on device capabilities
- **Error path testing** — deliberately trigger error conditions (invalid MR keys, bad QP states)

### Our additions
- **Property-based testing** (proptest) — generate random QP configurations, MR sizes, access patterns
- **Miri** for unsafe code — run pure Rust logic under Miri where possible (no FFI)
- **Sanitizers** — run with AddressSanitizer for FFI boundary checks
- **Deterministic CQ ordering tests** — verify completion ordering invariants

---

## 6. Test Organization Recommendation

```
tests/
├── unit/                    # Layer 1: No RDMA device
│   ├── layout.rs           # FFI struct layout checks
│   ├── flags.rs            # Bitflag conversions
│   ├── builders.rs         # Builder validation
│   ├── errors.rs           # Error mapping
│   └── serde.rs            # Serialization roundtrips
├── device/                  # Layer 2: Needs rxe device
│   ├── context.rs          # Device/context lifecycle
│   ├── pd.rs               # Protection domain tests
│   ├── cq.rs               # Completion queue tests
│   ├── mr.rs               # Memory registration
│   ├── qp.rs               # QP creation and state transitions
│   └── srq.rs              # Shared receive queue
├── datapath/                # Layer 3: Needs rxe + loopback
│   ├── rc_send_recv.rs     # RC send/receive
│   ├── rdma_read_write.rs  # One-sided operations
│   ├── ud_send_recv.rs     # UD datagrams
│   ├── atomics.rs          # CAS, FAA
│   └── completions.rs      # CQ polling + events
├── compile_fail/            # trybuild tests
│   ├── mr_after_pd_drop.rs
│   ├── qp_double_guard.rs
│   └── ...
└── helpers/
    ├── mod.rs              # Shared test infrastructure
    ├── require_rdma.rs     # Device availability check
    └── loopback.rs         # Server/client loopback helpers
```

---

## 7. Open Questions

1. **Docker vs. bare metal CI**: Should we run rxe inside a Docker container (may need `--privileged`) or bare on the CI runner?
2. ~~**Test parallelism**: RDMA device is shared state — do tests need serialization (e.g., `cargo test -- --test-threads=1`) or can they use separate PDs/QPs?~~ **Resolved** — Tests must run single-threaded (`RUST_TEST_THREADS=1` in `.cargo/config.toml`). siw has kernel resource contention when multiple RDMA connections run concurrently.
3. **Cleanup**: If a test crashes, RDMA resources (QPs, MRs) may leak in the kernel. Need cleanup strategy.
4. **Minimum kernel version**: rxe behavior varies across kernel versions. Pin or document minimum?
5. **Performance regression tests**: Worth running micro-benchmarks in CI (latency/throughput with rxe)?

### Findings from Implementation

- **siw loopback quirk**: siw on loopback (127.0.0.1/siw_lo) fails `rdma_listen` with `EADDRINUSE`. Tests must bind to `0.0.0.0` and connect via the eth0 IP (e.g. `10.0.0.4`).
- **Error model difference**: rdma_cm functions return -1 + set errno (use `last_os_error()`), while ibverbs functions return negative errno directly. This caused a subtle EPERM misinterpretation bug during development.
- **CmEvent ownership**: `rdma_ack_cm_event` invalidates the event — save `event_type()` before calling `ack()`. Drop impl also acks to prevent leaks.
- **siw does not support extended ibverbs API**: `ibv_create_cq_ex` and `ibv_qp_to_qp_ex` both return `EOPNOTSUPP`. Phase 3 (new ibverbs API) was skipped.
- **CompChannel race condition**: The `ibv_req_notify_cq` → `ibv_poll_cq` → `ibv_get_cq_event` sequence has a race when multiple completions interleave — the notification can be consumed before `ibv_get_cq_event`, causing a hang. The sync stream resolves this with spin-polling + `thread::yield_now()`. The async path (`AsyncCq`) resolves this with a drain-after-arm pattern: arm → poll → if empty, await fd → consume event → loop.
- **Listener event channel lifetime**: Accepted rdma_cm connections inherit the listener's event channel. Destroying the listener (and its event channel) causes accepted QPs to enter ERROR state with `WR_FLUSH_ERR` on all pending recv WRs. Fix: `rdma_migrate_id()` migrates accepted connections to their own event channel, decoupling them from the listener's lifetime. Applied in `AsyncRdmaListener::accept()`.
- **iWARP RDMA WRITE with IMM**: iWARP (RFC 5040) does not define RDMA Write with Immediate Data — that's an InfiniBand-specific operation. siw correctly rejects it. API method exists for InfiniBand/RoCE but is untestable on siw.
- **siw atomic support**: siw reports `ATOMIC_NONE` — no atomic CAS or FAA support. The `compare_and_swap()` and `fetch_and_add()` API methods require InfiniBand/RoCE hardware with `ATOMIC_HCA` or `ATOMIC_GLOB` capability.

### Current Test Suite (34 tests)

| Category | Count | Description |
|----------|-------|-------------|
| sys_tests | 8 | Raw FFI lifecycle: device, PD, CQ, MR, QP (against siw) |
| safe_api_tests | 10 | Safe API: device enumeration, PD, CQ, QP, MR, query port/GID |
| cm_tests | 2 | rdma_cm: connect/disconnect, send/recv (threaded, loopback) |
| stream_tests | 3 | Stream: echo, multi-message (5 round-trips), large transfer (32 KiB) |
| async_cq_tests | 2 | Async CQ: send/recv via comp_channel notification, poll_wr_id |
| async_qp_tests | 3 | AsyncQp: send/recv, ping-pong, RDMA WRITE+READ roundtrip |
| async_stream_tests | 6 | AsyncRdmaStream: echo, multi-message, large transfer, futures-io echo, tokio compat, tokio::io::copy |

---

## 8. Azure VM RDMA Hardware Assessment (Feb 2026)

Tested on an Azure Standard VM with accelerated networking.

### Environment

- **OS**: Ubuntu (kernel 6.14.0-1017-azure)
- **rdma-core**: 50.0-2ubuntu0.2 (libibverbs 1.14.50.0, librdmacm 1.3.50.0)
- **Hardware**: 2× Mellanox ConnectX-5 Virtual Functions (SR-IOV)
  - `rdmaP22814p0s2` → `enP22814s1` (slave of `eth0`, 10.0.0.4)
  - `rdmaP25034p0s2` → `enP25034s2` (slave of `eth1`, 10.0.0.5)
  - Vendor: 0x02c9, Part: 4120 (MT4120), FW: 16.30.5026
  - Link layer: Ethernet (RoCEv2), Port state: ACTIVE, Active MTU: 1024

### What Works on Azure Accelerated Networking VFs

| Operation | Result |
|---|---|
| `ibv_open_device` | ✅ OK |
| `ibv_alloc_pd` | ✅ OK |
| `ibv_create_cq` | ✅ OK |
| `ibv_reg_mr` (local + remote access) | ✅ OK |
| `ibv_query_device` | ✅ OK — reports max_qp=4096, max_cq=16M, max_mr=16M |
| `ibv_query_port` | ✅ OK — but **GID table length = 0** |

### What Does NOT Work

| Operation | Error | Root Cause |
|---|---|---|
| `ibv_create_qp` (RC) | `EINVAL` (errno 22) | GID table is empty — no RoCE GID entries assigned by hypervisor |
| `ibv_create_qp` (UD) | `EINVAL` (errno 22) | Same — QP transitions need valid GIDs |
| `ibv_create_qp` (RAW_PACKET) | `EPERM` (errno 1) | Not permitted on SR-IOV VF |
| `ibv_rc_pingpong` | "Couldn't create QP" | Fails at QP creation |
| `ibv_ud_pingpong` | "Couldn't create QP" | Fails at QP creation |

### Root Cause: Empty GID Table

Azure standard VMs expose ConnectX-5 VFs for accelerated networking (kernel-bypass packet I/O), but **do not configure RDMA**. The critical indicator is:

```
GID table length: 0
```

Without GID entries, no Queue Pair can be transitioned to RTR (Ready To Receive) state, so no data-path operations are possible. The VFs are useful for accelerated TCP/UDP but not for ibverbs RDMA operations.

### Kernel Module Availability

| Module | In kernel config? | .ko present? | Works? | Notes |
|---|---|---|---|---|
| `rdma_rxe` (SoftRoCE) | `# CONFIG_RDMA_RXE is not set` | ❌ | ❌ | Disabled in Azure kernel config — cannot be loaded |
| `siw` (SoftIWarp) | `CONFIG_RDMA_SIW=m` | ✅ (in `linux-modules-extra`) | ✅ | **Works for loopback RDMA** (see below) |

**Important**: The `siw.ko` module is in the `linux-modules-extra-$(uname -r)` package, which is **not installed by default** on Azure VMs. Install it with:

```bash
sudo apt-get install -y linux-modules-extra-$(uname -r)
```

### SoftIWarp (siw) — Working Loopback Setup

SIW provides full RDMA data-path capability on this VM. Setup:

```bash
sudo modprobe siw
sudo rdma link add siw0 type siw netdev eth0
# Verify:
rdma link  # should show siw0/1 state ACTIVE
```

**Verified working** with `rping` loopback (RDMA send/recv + RDMA read/write):

```bash
sudo bash -c 'ulimit -l unlimited; rping -s -a 0.0.0.0 -v &
sleep 2
rping -c -a 10.0.0.4 -v -C 5'
# Output: successful rdma-ping-0 through rdma-ping-4
```

**Key caveat — iWARP requires `rdma_cm` for connection setup**:
- Manual QP state transitions (`INIT → RTR → RTS`) **do not work** with SIW. The `RTR → RTS` transition fails with `EINVAL` because iWARP establishes a TCP connection under the hood during QP setup, which requires the RDMA CM (`rdma_connect`/`rdma_accept`) protocol.
- Tools like `ibv_rc_pingpong` and `ibv_ud_pingpong` that use manual QP transitions will fail.
- Tools like `rping` and `rdma_server`/`rdma_client` that use `rdma_cm` work correctly.
- **For our library**: tests must use `rdma_cm`-based connection setup when running over SIW. This is fine since we plan to support `rdmacm` anyway, but it means Layer 3 tests cannot rely on manual QP wiring for the SIW path.

**What works with siw (summary)**:

| Operation | Result |
|---|---|
| Device open, PD, CQ, MR | ✅ |
| QP creation (RC) | ✅ |
| QP INIT → RTR (manual) | ✅ |
| QP RTR → RTS (manual) | ❌ `EINVAL` — iWARP needs rdma_cm |
| `rping` (rdma_cm-based loopback) | ✅ Full send/recv + RDMA read |
| `ibv_rc_pingpong` (manual QP) | ❌ Fails at RTS |

### Building rxe/siw from Source

Neither module needs a full kernel source tree — only **kernel headers** (already installed as `linux-headers-$(uname -r)`).

**SIW**: Already available via `linux-modules-extra` — no need to build from source.

**RXE** (`rdma_rxe`): Disabled in the Azure kernel config (`# CONFIG_RDMA_RXE is not set`). Building it out-of-tree is **not straightforward**:

1. **Out-of-tree build is impractical** — `rdma_rxe` depends on internal kernel RDMA subsystem symbols (`ib_core`, `ib_uverbs`). It's not a standalone module; it's tightly coupled to the in-tree RDMA stack. You cannot simply compile `drivers/infiniband/sw/rxe/*.c` against headers alone.
2. **Recompiling the kernel** would work — clone the Ubuntu kernel source, enable `CONFIG_RDMA_RXE=m`, and build. This is a ~30-60 min process and requires ~10GB disk for the build tree.
3. **DKMS is not an option** — no upstream DKMS package for rxe exists.

**Recommendation**: Use SIW (already available) for local loopback testing. Use RXE in CI (GitHub Actions Ubuntu runners include it). If RXE is needed locally, the simplest path is installing the `linux-generic` kernel package (non-Azure kernel) which includes both rxe and siw, but this is not recommended on production Azure VMs.

### Implications for CI/Testing

1. **Standard Azure VMs can run RDMA data-path tests via SIW** — but only with `rdma_cm`-based connection setup, not manual QP wiring.
2. **Layer 2 tests** (device, PD, CQ, MR, QP creation) work with both SIW and the hardware ConnectX-5 VFs.
3. **Layer 3 data-path tests** on Azure require SIW + `rdma_cm`. On CI (GitHub Actions), use RXE which supports both manual QP transitions and `rdma_cm`.
4. **Azure HPC VMs** (HB, HC, ND series) with InfiniBand support would have populated GID tables and full RDMA capability including manual QP transitions.
5. **Test strategy**: Write data-path tests using `rdma_cm` for connection setup (works everywhere: SIW, RXE, real hardware). Optionally add manual-QP-transition tests gated behind a feature flag or runtime check for RXE/hardware availability.
