#!/bin/bash
# Check that build-time RDMA dependencies are installed.
#
# Verifies the headers, libraries, and tools required to compile the
# workspace (libibverbs + librdmacm dev packages, a C compiler for the
# `cc`-built wrapper, libclang for bindgen, protoc for the tonic tests, and
# the Rust toolchain).
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
echo "=== libclang (required by bindgen) ==="
# clang-sys loads libclang.so / libclang-*.so directly. A versioned runtime
# library alone (e.g. libclang-18.so.1) does not provide the required symlink.
libclang_so=""
libclang_dirs=()
[[ -n "${LIBCLANG_PATH:-}" ]] && libclang_dirs+=("$LIBCLANG_PATH")
if command -v llvm-config >/dev/null 2>&1; then
    libclang_dirs+=("$(llvm-config --libdir 2>/dev/null)")
fi
libclang_dirs+=(/usr/lib/llvm-*/lib /usr/lib/*/. /usr/lib /usr/lib64 /usr/local/lib)
for d in "${libclang_dirs[@]}"; do
    [[ -d "$d" ]] || continue
    # Match only the filenames clang-sys accepts (libclang.so, libclang-*.so);
    # a versioned-only libclang.so.N does NOT satisfy clang-sys.
    found=$(ls "$d"/libclang.so "$d"/libclang-*.so 2>/dev/null | head -1)
    if [[ -n "$found" ]]; then
        libclang_so="$found"
        break
    fi
done
if [[ -n "$libclang_so" ]]; then
    ok "libclang ($libclang_so)"
else
    fail "libclang.so not found (install libclang-dev or set LIBCLANG_PATH)"
    errors=$((errors + 1))
fi

echo
echo "=== Optional (RXE kernel module build) ==="
# Kernel headers are needed only to build the rdma_rxe module from source
# ('just build-rxe'). They must match the running kernel exactly, so report the
# specific linux-headers-<uname -r> package when the build symlink is missing.
kver=$(uname -r)
if [[ -d "/lib/modules/${kver}/build" ]]; then
    ok "kernel headers (/lib/modules/${kver}/build)"
else
    warn "kernel headers not found at /lib/modules/${kver}/build — needed for"
    warn "  'just build-rxe' (install linux-headers-${kver})"
fi

echo
if [[ "$errors" -eq 0 ]]; then
    ok "all build-time RDMA dependencies are installed"
    exit 0
else
    fail "$errors dependency check(s) failed"
    echo
    echo "On Ubuntu/Debian, install everything with:"
    echo "  sudo apt install build-essential pkg-config libclang-dev libibverbs-dev librdmacm-dev rdma-core protobuf-compiler libssl-dev"
    exit 1
fi
