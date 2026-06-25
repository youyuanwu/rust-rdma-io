#!/bin/bash
# Check that build-time RDMA dependencies are installed.
#
# Verifies the headers, libraries, and tools required to compile the
# workspace (libibverbs + librdmacm dev packages, a C compiler for the
# `cc`-built wrapper, protoc for the tonic tests, and the Rust toolchain).
#
# Usage: ./scripts/check-build-deps.sh
#
# Exit code is 0 when every required dependency is present, 1 otherwise.

set -uo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
NC='\033[0m'

ok()   { echo -e "${GREEN}✓${NC} $*"; }
warn() { echo -e "${YELLOW}!${NC} $*"; }
fail() { echo -e "${RED}✗${NC} $*"; }

errors=0

# Check for a header file on the default include path.
check_header() {
    local header="$1" pkg="$2"
    if [[ -f "/usr/include/$header" ]]; then
        ok "header $header"
    else
        fail "missing header $header (install $pkg)"
        errors=$((errors + 1))
    fi
}

# Check for a shared library, preferring pkg-config when available.
check_lib() {
    local pc="$1" soname="$2" pkg="$3"
    if command -v pkg-config >/dev/null 2>&1 && pkg-config --exists "$pc" 2>/dev/null; then
        ok "library $pc ($(pkg-config --modversion "$pc"))"
    elif ldconfig -p 2>/dev/null | grep -q "$soname"; then
        ok "library $soname"
    else
        fail "missing library $soname (install $pkg)"
        errors=$((errors + 1))
    fi
}

# Check for an executable on PATH.
check_tool() {
    local tool="$1" pkg="$2"
    if command -v "$tool" >/dev/null 2>&1; then
        ok "tool $tool ($(command -v "$tool"))"
    else
        fail "missing tool $tool (install $pkg)"
        errors=$((errors + 1))
    fi
}

echo "=== Headers (libibverbs-dev, librdmacm-dev, libssl-dev) ==="
check_header "infiniband/verbs.h"  "libibverbs-dev"
check_header "rdma/rdma_cma.h"     "librdmacm-dev"
check_header "openssl/ssl.h"       "libssl-dev"

echo
echo "=== Libraries ==="
check_lib "libibverbs" "libibverbs.so" "libibverbs-dev"
check_lib "librdmacm"  "librdmacm.so"  "librdmacm-dev"
check_lib "openssl"    "libssl.so"     "libssl-dev"

echo
echo "=== Build tools ==="
# A C compiler is required by the `cc` crate to build wrapper/wrapper.c.
if command -v cc >/dev/null 2>&1 || command -v gcc >/dev/null 2>&1 || command -v clang >/dev/null 2>&1; then
    ok "C compiler ($(command -v cc || command -v gcc || command -v clang))"
else
    fail "missing C compiler (install build-essential)"
    errors=$((errors + 1))
fi
check_tool "cargo" "rustup or rust toolchain"
check_tool "protoc" "protobuf-compiler"
# pkg-config is used by -sys crates (e.g. openssl-sys) to locate system
# libraries at build time; the build fails without it.
check_tool "pkg-config" "pkg-config"

echo
echo "=== Optional (binding regeneration / full workspace build) ==="
# libclang is used by bnd-winmd (via clang-sys) in the dev-only bnd-rdma-gen
# crate. It is NOT needed to build the rdma-io library crates, but a
# workspace-wide `cargo build`/`cargo check` compiles bnd-rdma-gen too and
# will fail without it. The generated bindings are checked in, so this only
# warns. Note: clang-sys needs the libclang.so symlink + llvm-config from the
# -dev package, not just the versioned runtime library.
if command -v llvm-config >/dev/null 2>&1; then
    ok "libclang ($(llvm-config --version), $(command -v llvm-config))"
elif command -v pkg-config >/dev/null 2>&1 && pkg-config --exists "clang" 2>/dev/null; then
    ok "libclang ($(pkg-config --modversion clang))"
else
    # clang-sys resolves the libclang.so symlink shipped by libclang-dev,
    # typically under /usr/lib/llvm-*/lib or the multiarch lib dir.
    libclang_so=$(ls /usr/lib/llvm-*/lib/libclang.so /usr/lib/*/libclang.so 2>/dev/null | head -1)
    if [[ -n "$libclang_so" ]]; then
        ok "libclang ($libclang_so)"
    else
        warn "libclang not found — needed for 'just gen-bindings' and"
        warn "  workspace-wide builds of bnd-rdma-gen (install libclang-dev)"
    fi
fi

echo
if [[ "$errors" -eq 0 ]]; then
    ok "all build-time RDMA dependencies are installed"
    exit 0
else
    fail "$errors dependency check(s) failed"
    echo
    echo "On Ubuntu/Debian, install everything with:"
    echo "  sudo apt install build-essential pkg-config libibverbs-dev librdmacm-dev rdma-core protobuf-compiler libssl-dev"
    exit 1
fi
