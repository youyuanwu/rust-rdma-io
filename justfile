# rust-rdma-io task runner — run `just` to list recipes.
#
# Requires: https://github.com/casey/just

# Single-threaded test runs avoid kernel resource contention (see README).
export RUST_TEST_THREADS := "1"

# List available recipes.
default:
    @just --list

# Check that build-time RDMA dependencies are installed.
check-deps:
    ./scripts/check-build-deps.sh

# Build the workspace (default features).
build:
    cargo build

# Build with futures-only async (no tokio).
build-no-tokio:
    cargo build --no-default-features --features async

# Run clippy across the workspace, denying warnings.
clippy:
    cargo clippy --all-targets -- -D warnings

# Format all crates and run clippy (deny warnings).
lint: fmt clippy

# Format all crates.
fmt:
    cargo fmt --all

# Check formatting without writing changes.
fmt-check:
    cargo fmt --all -- --check

# Run the integration tests (requires a software RDMA provider; see setup-siw).
test:
    cargo test -p rdma-io-tests -- --nocapture

# Run doc tests.
test-doc:
    cargo test --doc

# Set up the Soft-iWARP (siw) software RDMA provider.
setup-siw:
    sudo ./scripts/setup-siw.sh

# Set up the Soft-RoCE (rxe) software RDMA provider.
setup-rxe:
    sudo ./scripts/setup-rxe.sh

# Tear down the siw provider.
teardown-siw:
    sudo ./scripts/teardown-siw.sh

# Tear down the rxe provider.
teardown-rxe:
    sudo ./scripts/teardown-rxe.sh

# Regenerate the FFI bindings from the WinMD metadata.
gen-bindings:
    cargo run -p bnd-rdma-gen

# Remove build artifacts.
clean:
    cargo clean
