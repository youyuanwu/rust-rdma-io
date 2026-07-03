# cmake/BuildRxe.cmake
#
# Out-of-tree build of rdma_rxe kernel module.
# Fetches only the drivers/infiniband/sw/rxe/ source subdirectory from the
# torvalds/linux GitHub mirror (at the running kernel's release tag), then builds
# rdma_rxe.ko against the installed kernel headers (/lib/modules/$(uname -r)/build).
#
# Usage: cmake -B build -DBUILD_RXE=ON && cmake --build build --target rxe
# Load:  sudo cmake --build build --target rxe-load

# Detect kernel version
execute_process(
    COMMAND uname -r
    OUTPUT_VARIABLE KERNEL_FULL_VERSION
    OUTPUT_STRIP_TRAILING_WHITESPACE
)
# Strip distro suffix: "6.17.0-1008-azure" -> "6.17" (the upstream release tag).
string(REGEX REPLACE "^([0-9]+\\.[0-9]+).*" "\\1" KERNEL_BASE_VERSION "${KERNEL_FULL_VERSION}")

# rdma_rxe is built out-of-tree against the INSTALLED kernel headers
# (/lib/modules/$(uname -r)/build, i.e. the linux-headers package), so the only
# thing that must be fetched is the rxe driver *source* itself — its .c/.h files
# are NOT shipped in linux-headers. Rather than download the whole ~1.5 GB kernel
# tree from kernel.org (whose CDN has been flaky — 404s on /pub tarballs), fetch
# ONLY drivers/infiniband/sw/rxe/ (~35 small files) from the torvalds/linux
# GitHub mirror at the matching release tag.
set(RXE_GH_REPO "torvalds/linux"
    CACHE STRING "GitHub repo mirror to fetch the rxe driver source from")
set(RXE_GIT_REF "v${KERNEL_BASE_VERSION}"
    CACHE STRING "Git tag (in RXE_GH_REPO) to fetch the rxe driver source from")

set(KERNEL_SRC_RXE_DIR "${CMAKE_BINARY_DIR}/rxe-src")
set(RXE_BUILD_DIR "${CMAKE_BINARY_DIR}/rxe-build")

message(STATUS "RXE build: kernel=${KERNEL_FULL_VERSION}, rxe source=${RXE_GH_REPO}@${RXE_GIT_REF}")

# The fetch below runs at configure time. A ref-encoded stamp lets a plain
# reconfigure (same kernel tag) skip re-downloading, while a changed RXE_GIT_REF
# (or RXE_GH_REPO) misses the stamp and forces a fresh pull. Sanitise only the
# repo/ref (they contain '/') into the stamp filename — not the directory path.
string(REPLACE "/" "_" RXE_STAMP_TAG "${RXE_GH_REPO}-${RXE_GIT_REF}")
set(RXE_FETCH_STAMP "${KERNEL_SRC_RXE_DIR}/.fetched-${RXE_STAMP_TAG}")
if(EXISTS "${RXE_FETCH_STAMP}")
    message(STATUS "RXE build: rxe source already fetched (${RXE_GH_REPO}@${RXE_GIT_REF}); skipping download")
else()
    # Start from a clean dir so a ref switch can't leave stale files behind
    # (the rxe file set changes between versions, e.g. rxe_odp.c added in 6.17).
    file(REMOVE_RECURSE "${KERNEL_SRC_RXE_DIR}")
    file(MAKE_DIRECTORY "${KERNEL_SRC_RXE_DIR}")

    # Step 1: list the rxe directory via the GitHub Contents API. This is a single
    # api.github.com request; the per-file downloads below hit raw.githubusercontent.com,
    # which is not subject to the API's unauthenticated rate limit.
    set(RXE_LISTING "${CMAKE_BINARY_DIR}/rxe-listing.json")
    file(DOWNLOAD
        "https://api.github.com/repos/${RXE_GH_REPO}/contents/drivers/infiniband/sw/rxe?ref=${RXE_GIT_REF}"
        "${RXE_LISTING}"
        HTTPHEADER "Accept: application/vnd.github+json"
        HTTPHEADER "User-Agent: rdma-io-bench-BuildRxe"
        TLS_VERIFY ON
        STATUS RXE_LIST_STATUS)
    list(GET RXE_LIST_STATUS 0 RXE_LIST_CODE)
    if(NOT RXE_LIST_CODE EQUAL 0)
        list(GET RXE_LIST_STATUS 1 RXE_LIST_MSG)
        message(FATAL_ERROR
            "Failed to list rxe source at ${RXE_GH_REPO}@${RXE_GIT_REF}: ${RXE_LIST_MSG}.\n"
            "If this kernel's release tag differs, override it with -DRXE_GIT_REF=<vX.Y>.")
    endif()

    # Step 2: parse the JSON listing and download each file's raw contents.
    file(READ "${RXE_LISTING}" RXE_JSON)
    string(JSON RXE_TOP_TYPE ERROR_VARIABLE RXE_JSON_ERR TYPE "${RXE_JSON}")
    if(RXE_JSON_ERR OR NOT RXE_TOP_TYPE STREQUAL "ARRAY")
        # A non-array response is usually a GitHub error object (e.g. rate limit).
        message(FATAL_ERROR
            "Unexpected response listing rxe source (expected a file array):\n${RXE_JSON}")
    endif()
    string(JSON RXE_COUNT LENGTH "${RXE_JSON}")
    math(EXPR RXE_LAST "${RXE_COUNT} - 1")
    foreach(i RANGE ${RXE_LAST})
        string(JSON RXE_TYPE GET "${RXE_JSON}" ${i} type)
        if(NOT RXE_TYPE STREQUAL "file")
            continue()  # rxe/ is flat today, but skip any subdirectory defensively
        endif()
        string(JSON RXE_NAME GET "${RXE_JSON}" ${i} name)
        string(JSON RXE_URL GET "${RXE_JSON}" ${i} download_url)
        file(DOWNLOAD "${RXE_URL}" "${KERNEL_SRC_RXE_DIR}/${RXE_NAME}"
            HTTPHEADER "User-Agent: rdma-io-bench-BuildRxe"
            TLS_VERIFY ON
            STATUS RXE_DL_STATUS)
        list(GET RXE_DL_STATUS 0 RXE_DL_CODE)
        if(NOT RXE_DL_CODE EQUAL 0)
            list(GET RXE_DL_STATUS 1 RXE_DL_MSG)
            message(FATAL_ERROR "Failed to download rxe file '${RXE_NAME}': ${RXE_DL_MSG}")
        endif()
    endforeach()

    # Only stamp after every file has downloaded, so a partial/failed fetch retries.
    file(WRITE "${RXE_FETCH_STAMP}" "${RXE_GH_REPO}@${RXE_GIT_REF}\n")
    message(STATUS "RXE build: fetched ${RXE_COUNT} rxe source file(s) to ${KERNEL_SRC_RXE_DIR}")
endif()


# Step 3: Prepare build directory with Kbuild and Makefile
file(MAKE_DIRECTORY "${RXE_BUILD_DIR}")

# Copy source files to build dir
file(GLOB RXE_SRC_FILES "${KERNEL_SRC_RXE_DIR}/*")
foreach(SRC_FILE ${RXE_SRC_FILES})
    get_filename_component(FNAME "${SRC_FILE}" NAME)
    if(FNAME MATCHES "^\\.")
        continue()  # skip the .fetched-* stamp and any other dotfiles
    endif()
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

# Step 4: Build target
add_custom_target(rxe
    COMMAND $(MAKE) modules
    WORKING_DIRECTORY "${RXE_BUILD_DIR}"
    COMMENT "Building rdma_rxe.ko (out-of-tree)"
)

# After building, load with: sudo ./scripts/setup-rxe.sh
# The script auto-detects the out-of-tree .ko in the build directory.
