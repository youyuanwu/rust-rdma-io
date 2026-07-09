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

# Fetch the rxe driver source (sparse, partial git checkout — avoids the per-file
# raw.githubusercontent.com rate limiting) and build rdma_rxe.ko out-of-tree
# against the running kernel's headers.
# Build the rdma_rxe kernel module from source (no CMake required).
build-rxe:
    #!/usr/bin/env bash
    set -euo pipefail
    repo="torvalds/linux"
    kver="$(uname -r)"
    base="$(echo "$kver" | sed -E 's/^([0-9]+\.[0-9]+).*/\1/')"
    ref="v${base}"
    src_dir="build/rxe-src"
    git_dir="build/rxe-git"
    build_dir="build/rxe-build"
    stamp="${src_dir}/.fetched-${repo//\//_}-${ref}"
    if [[ -f "$stamp" ]]; then
        echo "RXE source already fetched (${repo}@${ref}); skipping git checkout"
    else
        echo "Fetching rxe driver source ${repo}@${ref} via sparse partial clone"
        rm -rf "$git_dir" "$src_dir"
        git clone --filter=blob:none --sparse --depth 1 --branch "$ref" \
            "https://github.com/${repo}.git" "$git_dir"
        git -C "$git_dir" sparse-checkout set drivers/infiniband/sw/rxe
        mkdir -p "$src_dir"
        cp "$git_dir"/drivers/infiniband/sw/rxe/* "$src_dir"/
        echo "${repo}@${ref}" > "$stamp"
        echo "Fetched $(ls -1 "$src_dir" | wc -l) rxe source file(s) to ${src_dir}"
    fi
    # Stage an out-of-tree module build: copy sources, rename Makefile -> Kbuild and
    # force CONFIG_RDMA_RXE to build as a module, then write a thin driver Makefile.
    headers="/lib/modules/${kver}/build"
    rm -rf "$build_dir"
    mkdir -p "$build_dir"
    cp "$src_dir"/*.c "$src_dir"/*.h "$build_dir"/
    cp "$src_dir"/Kconfig "$src_dir"/Makefile "$build_dir"/
    mv "$build_dir/Makefile" "$build_dir/Kbuild"
    sed -i 's/\$(CONFIG_RDMA_RXE)/m/' "$build_dir/Kbuild"
    printf 'modules:\n\t$(MAKE) -C %s M=$(CURDIR) modules\nclean:\n\t$(MAKE) -C %s M=$(CURDIR) clean\n.PHONY: modules clean\n' \
        "$headers" "$headers" > "$build_dir/Makefile"
    if [[ ! -d "$headers" ]]; then
        echo "error: kernel headers not found at ${headers}" >&2
        echo "install them with: sudo apt-get install -y linux-headers-${kver}" >&2
        exit 1
    fi
    make -C "$build_dir" modules -j"$(nproc)"
    echo "Built ${build_dir}/rdma_rxe.ko"

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
    #!/usr/bin/env bash
    set -euo pipefail
    winmd_url="https://github.com/youyuanwu/bnd/raw/refs/heads/main/bnd-linux/winmd/bnd-linux.winmd"
    winmd="build/winmd/bnd-linux.winmd"
    if [[ -f "$winmd" ]]; then
        echo "bnd-linux.winmd already present ($(stat -c%s "$winmd") bytes)"
    else
        echo "Downloading bnd-linux.winmd to ${winmd}"
        mkdir -p "$(dirname "$winmd")"
        curl --proto '=https' --tlsv1.2 -fSL "$winmd_url" -o "$winmd"
        echo "Downloaded bnd-linux.winmd ($(stat -c%s "$winmd") bytes)"
    fi
    cargo run -p bnd-rdma-gen

# Remove build artifacts.
clean:
    cargo clean
    rm -rf build
