cmake_minimum_required(VERSION 3.20)
project(rust-rdma-io NONE)  # No C/C++ compiler needed for configure step

# ---------------------------------------------------------------------------
# Options
# ---------------------------------------------------------------------------
set(BND_LINUX_WINMD_URL
    "https://github.com/youyuanwu/bnd/raw/refs/heads/main/bnd-linux/winmd/bnd-linux.winmd"
    CACHE STRING "URL to download bnd-linux.winmd from")

set(BND_LINUX_WINMD_DIR
    "${CMAKE_BINARY_DIR}/winmd"
    CACHE PATH "Directory to place bnd-linux.winmd")

# ---------------------------------------------------------------------------
# Configure: download bnd-linux.winmd
# ---------------------------------------------------------------------------
set(BND_LINUX_WINMD "${BND_LINUX_WINMD_DIR}/bnd-linux.winmd")

if(NOT EXISTS "${BND_LINUX_WINMD}")
    message(STATUS "Downloading bnd-linux.winmd to ${BND_LINUX_WINMD}")
    file(MAKE_DIRECTORY "${BND_LINUX_WINMD_DIR}")
    file(DOWNLOAD
        "${BND_LINUX_WINMD_URL}"
        "${BND_LINUX_WINMD}"
        STATUS DOWNLOAD_STATUS
        SHOW_PROGRESS
    )
    list(GET DOWNLOAD_STATUS 0 STATUS_CODE)
    list(GET DOWNLOAD_STATUS 1 STATUS_MSG)
    if(NOT STATUS_CODE EQUAL 0)
        file(REMOVE "${BND_LINUX_WINMD}")
        message(FATAL_ERROR "Failed to download bnd-linux.winmd: ${STATUS_MSG}")
    endif()
    file(SIZE "${BND_LINUX_WINMD}" WINMD_SIZE)
    message(STATUS "Downloaded bnd-linux.winmd (${WINMD_SIZE} bytes)")
else()
    file(SIZE "${BND_LINUX_WINMD}" WINMD_SIZE)
    message(STATUS "bnd-linux.winmd already present (${WINMD_SIZE} bytes)")
endif()

# ---------------------------------------------------------------------------
# Build targets (delegate to cargo)
# ---------------------------------------------------------------------------
find_program(CARGO cargo REQUIRED)

add_custom_target(build ALL
    COMMAND ${CARGO} build --workspace --features tokio
    WORKING_DIRECTORY ${CMAKE_CURRENT_SOURCE_DIR}
    COMMENT "Building rust-rdma-io workspace"
)

add_custom_target(test
    COMMAND ${CARGO} test -p rdma-io-tests
    WORKING_DIRECTORY ${CMAKE_CURRENT_SOURCE_DIR}
    COMMENT "Running rdma-io tests"
)

add_custom_target(clippy
    COMMAND ${CARGO} clippy --workspace --all-targets --features tokio
    WORKING_DIRECTORY ${CMAKE_CURRENT_SOURCE_DIR}
    COMMENT "Running clippy"
)

add_custom_target(generate
    COMMAND ${CARGO} run -p bnd-rdma-gen
    WORKING_DIRECTORY ${CMAKE_CURRENT_SOURCE_DIR}
    COMMENT "Regenerating RDMA bindings"
)

# ---------------------------------------------------------------------------
# Optional: build rdma_rxe kernel module from source
# ---------------------------------------------------------------------------
option(BUILD_RXE "Download kernel source and build rdma_rxe.ko out-of-tree" OFF)
if(BUILD_RXE)
    include(cmake/BuildRxe.cmake)
endif()
