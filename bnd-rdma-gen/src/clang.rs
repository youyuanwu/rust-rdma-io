use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

const ROOT_HEADERS: &[&str] = &["infiniband/verbs.h", "rdma/rdma_cma.h"];

const SCOPE_HEADERS: &[&str] = &[
    "infiniband/verbs.h",
    "infiniband/verbs_api.h",
    "infiniband/ib_user_ioctl_verbs.h",
    "rdma/ib_user_verbs.h",
    "rdma/rdma_cma.h",
    "infiniband/sa.h",
];

const EXTERNAL_REFERENCE_ROUTES: &[(&str, &str)] = &[
    (
        "__atomic_wide_counter",
        "bnd_linux::libc::atomic_wide_counter",
    ),
    ("__be16", "bnd_linux::libc::types"),
    ("__be32", "bnd_linux::libc::types"),
    ("__be64", "bnd_linux::libc::types"),
    ("__pthread_cond_s", "bnd_linux::libc::thread_shared_types"),
    ("__pthread_list_t", "bnd_linux::libc::thread_shared_types"),
    ("__pthread_mutex_s", "bnd_linux::libc::struct_mutex"),
    ("__socklen_t", "bnd_linux::libc::types"),
    ("__ssize_t", "bnd_linux::libc::types"),
    ("__syscall_slong_t", "bnd_linux::libc::types"),
    ("__time_t", "bnd_linux::libc::types"),
    ("__u16", "bnd_linux::libc::int_ll64"),
    ("__u32", "bnd_linux::libc::int_ll64"),
    ("__u64", "bnd_linux::libc::int_ll64"),
    ("in6_addr", "bnd_linux::libc::r#in"),
    ("in_addr", "bnd_linux::libc::r#in"),
    ("in_addr_t", "bnd_linux::libc::r#in"),
    ("in_port_t", "bnd_linux::libc::r#in"),
    ("pthread_cond_t", "bnd_linux::libc::pthreadtypes"),
    ("pthread_mutex_t", "bnd_linux::libc::pthreadtypes"),
    ("sa_family_t", "bnd_linux::libc::sockaddr"),
    ("sockaddr", "bnd_linux::libc::socket"),
    ("sockaddr_in", "bnd_linux::libc::r#in"),
    ("sockaddr_in6", "bnd_linux::libc::r#in"),
    ("sockaddr_storage", "bnd_linux::libc::socket"),
    ("socklen_t", "bnd_linux::libc::unistd"),
    ("ssize_t", "bnd_linux::libc::types"),
    ("timespec", "bnd_linux::libc::struct_timespec"),
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
    // Scrape before assigning external ownership. Passing the full Linux WinMD to
    // bnd-clang currently drops RDMA structs that embed referenced socket types.
    let generated_winmd = generate_metadata(temp.path(), &linux_winmd);
    let external_type_names = shared_type_names(&generated_winmd, &linux_winmd);

    let winmd_dir = output_dir.join("winmd");
    std::fs::create_dir_all(&winmd_dir).expect("failed to create RDMA WinMD directory");
    let winmd = winmd_dir.join("bnd-rdma.winmd");
    std::fs::copy(&generated_winmd, &winmd).expect("failed to save RDMA WinMD");

    let remapped_winmd = temp.path().join("bnd-rdma.remapped.winmd");
    remap_metadata(
        &temp.path().join("metadata"),
        &generated_winmd,
        &remapped_winmd,
        &external_type_names,
    );
    assert_metadata_contract(&remapped_winmd);

    let external_routes = checked_external_routes(&external_type_names);

    let manifest_path = output_dir.join("Cargo.toml");
    let manifest =
        std::fs::read(&manifest_path).expect("failed to preserve rdma-io-sys Cargo.toml");
    let generation = std::panic::catch_unwind(|| {
        let mut bindgen = windows_bindgen::Bindgen::new();
        bindgen
            .input(&remapped_winmd)
            .output(output_dir)
            .filter("rdma")
            .sys()
            .package()
            .package_feature_root("rdma");
        for (type_name, rust_path) in &external_routes {
            bindgen.external_reference(&format!("libc.{type_name}"), rust_path);
        }
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
    let rdma_winmd = winmd_dir.join("bnd-rdma.winmd");
    let wrapper = wrapper_header();

    let mut source = ROOT_HEADERS
        .iter()
        .map(|header| format!("#include <{header}>\n"))
        .collect::<String>();
    source.push_str(&format!("#include \"{}\"\n", wrapper.display()));

    let mut clang = windows_clang::clang();
    clang
        .input_text(&source)
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
        .scope_headers(SCOPE_HEADERS.iter().copied())
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
        .output(&rdma_winmd)
        .write()
        .expect("windows-rdl failed to compile RDMA metadata");
    rdma_winmd
}

fn remap_metadata(
    rdl_dir: &Path,
    input: &Path,
    output: &Path,
    external_type_names: &BTreeSet<String>,
) {
    let mut rdl_files: Vec<_> = std::fs::read_dir(rdl_dir)
        .expect("failed to read RDMA RDL directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "rdl"))
        .collect();
    rdl_files.sort();

    let mut routes: HashMap<_, _> = external_type_names
        .iter()
        .map(|name| (name.clone(), "libc".to_string()))
        .collect();
    for path in rdl_files {
        let stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .expect("RDMA RDL file has no UTF-8 stem");
        let namespace = format!("rdma.{}", module_for_header(stem));
        for name in
            windows_rdl::item_names(&path, "rdma").expect("failed to read RDMA RDL item names")
        {
            routes.entry(name).or_insert_with(|| namespace.clone());
        }
    }

    windows_metadata::remap()
        .source("rdma")
        .fallback("rdma.ibverbs")
        .routes(routes)
        .input(input)
        .output(output)
        .remap()
        .expect("failed to remap RDMA metadata");
}

fn shared_type_names(input: &Path, linux_winmd: &Path) -> BTreeSet<String> {
    let rdma = open_index(input);
    let linux = open_index(linux_winmd);
    let linux_names: BTreeSet<_> = linux
        .types()
        .filter(|ty| ty.namespace() == "libc")
        .map(|ty| ty.name().to_string())
        .collect();
    rdma.types()
        .filter(|ty| {
            // Both files use Apis as their synthetic function container; it is
            // not a shared C type and must remain owned by RDMA.
            ty.namespace() == "rdma" && ty.name() != "Apis" && linux_names.contains(ty.name())
        })
        .map(|ty| ty.name().to_string())
        .collect()
}

fn module_for_header(stem: &str) -> &'static str {
    match stem {
        "rdma_cma" | "sa" => "rdmacm",
        "wrapper" => "wrapper",
        _ => "ibverbs",
    }
}

fn checked_external_routes(types: &BTreeSet<String>) -> BTreeMap<String, String> {
    let available: BTreeMap<_, _> = EXTERNAL_REFERENCE_ROUTES.iter().copied().collect();
    let missing: Vec<_> = types
        .iter()
        .filter(|name| !available.contains_key(name.as_str()))
        .cloned()
        .collect();
    assert!(
        missing.is_empty(),
        "missing bnd-linux Rust routes for external libc types: {missing:?}"
    );
    types
        .iter()
        .map(|name| {
            (
                name.clone(),
                available
                    .get(name.as_str())
                    .expect("external route checked above")
                    .to_string(),
            )
        })
        .collect()
}

fn assert_metadata_contract(path: &Path) {
    let index = open_index(path);
    for (namespace, type_name) in [
        ("rdma.ibverbs", "ibv_context"),
        ("rdma.ibverbs", "ibv_send_wr"),
        ("rdma.rdmacm", "rdma_cm_event"),
        ("rdma.rdmacm", "rdma_cm_id"),
    ] {
        index.expect(namespace, type_name);
    }
    for (namespace, library, methods) in [
        (
            "rdma.ibverbs",
            "ibverbs",
            &["ibv_get_device_list", "ibv_reg_mr"][..],
        ),
        (
            "rdma.rdmacm",
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
