# cmake/BuildRxe.cmake
#
# Out-of-tree build of rdma_rxe kernel module.
# Uses FetchContent to download and extract the kernel source tarball,
# then builds rdma_rxe.ko using installed kernel headers.
#
# Usage: cmake -B build -DBUILD_RXE=ON && cmake --build build --target rxe
# Load:  sudo cmake --build build --target rxe-load

include(FetchContent)

# Detect kernel version
execute_process(
    COMMAND uname -r
    OUTPUT_VARIABLE KERNEL_FULL_VERSION
    OUTPUT_STRIP_TRAILING_WHITESPACE
)
# Strip distro suffix: "6.17.0-1008-azure" -> "6.17"
# kernel.org tarballs use base version without patch .0
string(REGEX REPLACE "^([0-9]+\\.[0-9]+).*" "\\1" KERNEL_BASE_VERSION "${KERNEL_FULL_VERSION}")
string(REGEX REPLACE "^([0-9]+)\\..*" "\\1" KERNEL_MAJOR "${KERNEL_FULL_VERSION}")

set(KERNEL_TARBALL_URL
    "https://cdn.kernel.org/pub/linux/kernel/v${KERNEL_MAJOR}.x/linux-${KERNEL_BASE_VERSION}.tar.xz"
    CACHE STRING "URL for kernel source tarball")

message(STATUS "RXE build: kernel=${KERNEL_FULL_VERSION}, base=${KERNEL_BASE_VERSION}")
message(STATUS "RXE tarball: ${KERNEL_TARBALL_URL}")

# Step 1: Download and extract via FetchContent
FetchContent_Declare(linux_kernel
    URL "${KERNEL_TARBALL_URL}"
    DOWNLOAD_EXTRACT_TIMESTAMP TRUE
)
FetchContent_MakeAvailable(linux_kernel)

set(RXE_BUILD_DIR "${linux_kernel_BINARY_DIR}/rxe")
set(KERNEL_SRC_RXE_DIR "${linux_kernel_SOURCE_DIR}/drivers/infiniband/sw/rxe")

# Step 2: Prepare build directory with Kbuild and Makefile
file(MAKE_DIRECTORY "${RXE_BUILD_DIR}")

# Copy source files to build dir
file(GLOB RXE_SRC_FILES "${KERNEL_SRC_RXE_DIR}/*")
foreach(SRC_FILE ${RXE_SRC_FILES})
    get_filename_component(FNAME "${SRC_FILE}" NAME)
    configure_file("${SRC_FILE}" "${RXE_BUILD_DIR}/${FNAME}" COPYONLY)
endforeach()

# Rename Makefile -> Kbuild and patch CONFIG_RDMA_RXE -> m
if(EXISTS "${RXE_BUILD_DIR}/Makefile")
    file(RENAME "${RXE_BUILD_DIR}/Makefile" "${RXE_BUILD_DIR}/Kbuild")
endif()
file(READ "${RXE_BUILD_DIR}/Kbuild" KBUILD_CONTENT)
string(REPLACE "$(CONFIG_RDMA_RXE)" "m" KBUILD_CONTENT "${KBUILD_CONTENT}")
file(WRITE "${RXE_BUILD_DIR}/Kbuild" "${KBUILD_CONTENT}")

# Write out-of-tree Makefile
file(WRITE "${RXE_BUILD_DIR}/Makefile"
"LINUX_SRC_PATH = /lib/modules/\$(shell uname -r)/build
modules:
\t@\$(MAKE) -C \$(LINUX_SRC_PATH) M=\$(CURDIR) modules
clean:
\t@\$(MAKE) -C \$(LINUX_SRC_PATH) M=\$(CURDIR) clean
.PHONY: modules clean
")

# Step 3: Build target
add_custom_target(rxe
    COMMAND $(MAKE) modules
    WORKING_DIRECTORY "${RXE_BUILD_DIR}"
    COMMENT "Building rdma_rxe.ko (out-of-tree)"
)

# After building, load with: sudo ./scripts/setup-rxe.sh
# The script auto-detects the out-of-tree .ko in the build directory.
