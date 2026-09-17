use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const ROOT_HEADERS: &[&str] = &["infiniband/verbs.h", "rdma/rdma_cma.h"];

const PARTITION_HEADERS: &[&str] = &[
    "infiniband/verbs.h",
    "infiniband/verbs_api.h",
    "infiniband/ib_user_ioctl_verbs.h",
    "rdma/ib_user_verbs.h",
    "rdma/rdma_cma.h",
    "infiniband/sa.h",
];

/// Generate the rdma-io-sys bindings through one direct-Clang translation unit.
pub fn generate(output_dir: &Path) {
    let linux_winmd = linux_winmd();
    let temp = tempfile::tempdir_in(
        output_dir
            .parent()
            .expect("rdma-io-sys output must have a parent directory"),
    )
    .expect("failed to create temporary RDMA metadata directory");
    let flat_winmd = generate_metadata(temp.path(), &linux_winmd);

    let winmd_dir = output_dir.join("winmd");
    std::fs::create_dir_all(&winmd_dir).expect("failed to create RDMA WinMD directory");
    let winmd = winmd_dir.join("bnd-rdma.winmd");
    remap_metadata(
        &temp.path().join("metadata"),
        &flat_winmd,
        &winmd,
        &temp.path().join("remap"),
        &linux_winmd,
    );
    assert_metadata_contract(&winmd);

    let manifest_path = output_dir.join("Cargo.toml");
    let manifest =
        std::fs::read(&manifest_path).expect("failed to preserve rdma-io-sys Cargo.toml");
    let generation = std::panic::catch_unwind(|| {
        let mut bindgen = windows_bindgen::Bindgen::new();
        bindgen
            .inputs([&winmd, &linux_winmd])
            .output(output_dir)
            .filter("rdma")
            .filter("!libc")
            .sys()
            .package()
            .package_feature_root("rdma")
            .reference("bnd_linux", windows_bindgen::ReferenceStyle::Full, "libc");
        bindgen.write();
    });
    if let Err(payload) = generation {
        std::fs::write(manifest_path, manifest).expect("failed to restore rdma-io-sys Cargo.toml");
        std::panic::resume_unwind(payload);
    }
}

fn linux_winmd() -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../build/winmd/bnd-linux.winmd");
    assert!(
        path.exists(),
        "bnd-linux WinMD not found at {}\nHint: run `just gen-bindings`",
        path.display()
    );
    path
}

fn wrapper_header() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../rdma-io-sys/wrapper/wrapper.h")
}

fn generate_metadata(output_dir: &Path, linux_winmd: &Path) -> PathBuf {
    let rdl_dir = output_dir.join("metadata");
    std::fs::create_dir_all(&rdl_dir).expect("failed to create RDMA RDL directory");
    let winmd_dir = output_dir.join("winmd");
    std::fs::create_dir_all(&winmd_dir).expect("failed to create generated WinMD directory");
    let flat_winmd = winmd_dir.join("bnd-rdma.flat.winmd");
    let wrapper = wrapper_header();

    let mut source = ROOT_HEADERS
        .iter()
        .map(|header| format!("#include <{header}>\n"))
        .collect::<String>();
    source.push_str(&format!("#include \"{}\"\n", wrapper.display()));

    windows_clang::clang()
        .input_text(&source)
        .reference(linux_winmd)
        .args(["-x", "c", "-std=gnu11"])
        .namespace("rdma")
        .library("ibverbs")
        .header_libraries([
            ("rdma/rdma_cma.h", "rdmacm"),
            (
                wrapper
                    .to_str()
                    .expect("wrapper header path must be valid UTF-8"),
                "rdma_wrapper",
            ),
        ])
        .scope_headers(PARTITION_HEADERS.iter().copied())
        .scope_header(
            wrapper
                .to_str()
                .expect("wrapper header path must be valid UTF-8"),
        )
        .output(&rdl_dir)
        .write_by_header()
        .expect("bnd-clang failed to generate RDMA RDL partitions");

    windows_rdl::reader()
        .input(&rdl_dir)
        .reference(linux_winmd)
        .reference_default()
        .output(&flat_winmd)
        .write()
        .expect("windows-rdl failed to compile flat RDMA metadata");
    flat_winmd
}

fn remap_metadata(
    rdl_dir: &Path,
    flat_winmd: &Path,
    output: &Path,
    scratch_dir: &Path,
    linux_winmd: &Path,
) {
    let mut remap = windows_clang::remap_by_header();
    remap
        .rdl_dir(rdl_dir)
        .input(flat_winmd)
        .output(output)
        .scratch_dir(scratch_dir)
        .source("rdma")
        .import("Windows::Win32")
        .reference(linux_winmd)
        .reference_default();
    for namespace in metadata_namespaces(linux_winmd, "libc") {
        remap.import(&namespace.replace('.', "::"));
    }
    remap
        .write()
        .expect("failed to remap canonical RDMA metadata");
}

fn metadata_namespaces(input: &Path, root: &str) -> BTreeSet<String> {
    open_index(input)
        .types()
        .map(|ty| ty.namespace().to_string())
        .filter(|namespace| {
            namespace == root
                || namespace
                    .strip_prefix(root)
                    .is_some_and(|suffix| suffix.starts_with('.'))
        })
        .collect()
}

fn assert_metadata_contract(path: &Path) {
    let index = open_index(path);
    for (namespace, type_name) in [
        ("rdma.verbs", "ibv_context"),
        ("rdma.verbs", "ibv_send_wr"),
        ("rdma.rdma_cma", "rdma_cm_event"),
        ("rdma.rdma_cma", "rdma_cm_id"),
    ] {
        index.expect(namespace, type_name);
    }
    for (namespace, library, methods) in [
        (
            "rdma.verbs",
            "ibverbs",
            &["ibv_get_device_list", "ibv_reg_mr"][..],
        ),
        (
            "rdma.rdma_cma",
            "rdmacm",
            &[
                "rdma_accept",
                "rdma_bind_addr",
                "rdma_connect",
                "rdma_create_id",
                "rdma_create_qp",
                "rdma_destroy_id",
                "rdma_disconnect",
                "rdma_get_cm_event",
                "rdma_resolve_addr",
            ][..],
        ),
        (
            "rdma.wrapper",
            "rdma_wrapper",
            &["rdma_wrap_ibv_poll_cq", "rdma_wrap_ibv_post_send"][..],
        ),
    ] {
        let apis = index.expect(namespace, "Apis");
        for name in methods {
            let method = apis
                .methods()
                .find(|method| method.name() == *name)
                .unwrap_or_else(|| panic!("{namespace}.{name} is missing"));
            assert_eq!(
                method
                    .impl_map()
                    .unwrap_or_else(|| panic!("{namespace}.{name} has no native import"))
                    .import_scope()
                    .name(),
                library,
                "{namespace}.{name} has the wrong native library"
            );
        }
    }
}

fn open_index(path: &Path) -> windows_metadata::reader::Index {
    let file = windows_metadata::reader::File::new(
        std::fs::read(path)
            .unwrap_or_else(|error| panic!("failed to read `{}`: {error}", path.display())),
    )
    .unwrap_or_else(|| panic!("failed to parse `{}`", path.display()));
    windows_metadata::reader::Index::new(vec![file])
}
