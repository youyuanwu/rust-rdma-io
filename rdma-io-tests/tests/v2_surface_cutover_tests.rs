//! Exact selector-bounded checks for the final V2 surface and ownership model.

#[path = "fixtures/v2_surface_manifest.rs"]
mod manifest;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use manifest::{Disposition, Domain, FINAL_MODULES, UNITS};
use syn::visit::Visit;
use syn::{ImplItem, Item, Type, UseTree, Visibility};

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn v2_file(relative: &str) -> PathBuf {
    workspace().join("rdma-io/src").join(relative)
}

fn parse(relative: &str) -> syn::File {
    let path = v2_file(relative);
    syn::parse_file(
        &fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display())),
    )
    .unwrap_or_else(|error| panic!("failed to parse {}: {error}", path.display()))
}

fn is_public(visibility: &Visibility) -> bool {
    matches!(visibility, Visibility::Public(_))
}

fn use_names(tree: &UseTree, names: &mut BTreeSet<String>) {
    match tree {
        UseTree::Path(path) => use_names(&path.tree, names),
        UseTree::Name(name) => {
            names.insert(name.ident.to_string());
        }
        UseTree::Rename(rename) => {
            names.insert(rename.rename.to_string());
        }
        UseTree::Group(group) => {
            for item in &group.items {
                use_names(item, names);
            }
        }
        UseTree::Glob(_) => panic!("V2 public facade must not use glob re-exports"),
    }
}

fn public_reexports(relative: &str) -> BTreeSet<String> {
    parse(relative)
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Use(item) if is_public(&item.vis) => Some(item),
            _ => None,
        })
        .fold(BTreeSet::new(), |mut names, item| {
            use_names(&item.tree, &mut names);
            names
        })
}

fn public_modules(relative: &str) -> BTreeSet<String> {
    parse(relative)
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Mod(item) if is_public(&item.vis) => Some(item.ident.to_string()),
            _ => None,
        })
        .collect()
}

fn collect_public_methods(relative: &str, methods: &mut BTreeMap<String, BTreeSet<String>>) {
    fn walk(items: &[Item], methods: &mut BTreeMap<String, BTreeSet<String>>) {
        for item in items {
            if let Item::Mod(module) = item
                && let Some((_, nested)) = &module.content
            {
                walk(nested, methods);
            }
            let Item::Impl(item) = item else {
                continue;
            };
            if item.trait_.is_some() {
                continue;
            }
            let Type::Path(self_type) = item.self_ty.as_ref() else {
                continue;
            };
            let Some(type_name) = self_type.path.segments.last() else {
                continue;
            };
            let target = methods.entry(type_name.ident.to_string()).or_default();
            for member in &item.items {
                if let ImplItem::Fn(function) = member
                    && is_public(&function.vis)
                    && !function.sig.ident.to_string().starts_with("test_")
                {
                    target.insert(function.sig.ident.to_string());
                }
            }
        }
    }
    walk(&parse(relative).items, methods);
}

fn method_set(type_name: &str, files: &[&str]) -> BTreeSet<String> {
    let mut methods = BTreeMap::new();
    for file in files {
        collect_public_methods(file, &mut methods);
    }
    methods.remove(type_name).unwrap_or_default()
}

fn expected(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

#[derive(Default)]
struct TypeReferenceCounter {
    cm_id: usize,
    async_cm_id: usize,
}

impl<'ast> Visit<'ast> for TypeReferenceCounter {
    fn visit_type_path(&mut self, path: &'ast syn::TypePath) {
        if let Some(segment) = path.path.segments.last() {
            match segment.ident.to_string().as_str() {
                "CmId" => self.cm_id += 1,
                "AsyncCmId" => self.async_cm_id += 1,
                _ => {}
            }
        }
        syn::visit::visit_type_path(self, path);
    }
}

fn public_signature_type_counts(
    relative: &str,
    allowed_types: &BTreeSet<String>,
) -> (usize, usize) {
    fn walk(items: &[Item], allowed_types: &BTreeSet<String>, counts: &mut (usize, usize)) {
        for item in items {
            if let Item::Mod(module) = item
                && let Some((_, nested)) = &module.content
            {
                walk(nested, allowed_types, counts);
            }
            let Item::Impl(item) = item else {
                continue;
            };
            let Type::Path(self_type) = item.self_ty.as_ref() else {
                continue;
            };
            let Some(type_name) = self_type.path.segments.last() else {
                continue;
            };
            if !allowed_types.contains(&type_name.ident.to_string()) {
                continue;
            }
            for member in &item.items {
                if let ImplItem::Fn(function) = member
                    && is_public(&function.vis)
                {
                    let mut visitor = TypeReferenceCounter::default();
                    visitor.visit_signature(&function.sig);
                    if visitor.cm_id > 0 {
                        counts.0 += 1;
                    }
                    if visitor.async_cm_id > 0 {
                        counts.1 += 1;
                    }
                }
            }
        }
    }
    let mut counts = (0, 0);
    walk(&parse(relative).items, allowed_types, &mut counts);
    counts
}

fn declared_types(relative: &str) -> BTreeSet<String> {
    parse(relative)
        .items
        .into_iter()
        .filter_map(|item| match item {
            Item::Enum(item) => Some(item.ident.to_string()),
            Item::Struct(item) => Some(item.ident.to_string()),
            Item::Type(item) => Some(item.ident.to_string()),
            Item::Trait(item) => Some(item.ident.to_string()),
            _ => None,
        })
        .collect()
}

#[test]
fn manifest_has_exact_ids_domains_and_dispositions() {
    let ids = UNITS.iter().map(|unit| unit.id).collect::<BTreeSet<_>>();
    assert_eq!(ids.len(), 84, "duplicate manifest ID");
    assert_eq!(UNITS.len(), 84);
    for (prefix, count) in [("S", 37), ("M", 17), ("H", 15), ("A", 15)] {
        let expected_ids = (1..=count)
            .map(|index| format!("{prefix}-{index:03}"))
            .collect::<BTreeSet<_>>();
        let actual = ids
            .iter()
            .filter(|id| id.starts_with(prefix))
            .map(|id| (*id).to_owned())
            .collect::<BTreeSet<_>>();
        assert_eq!(actual, expected_ids, "{prefix} manifest set mismatch");
    }

    let domain_counts = UNITS.iter().fold(BTreeMap::new(), |mut counts, unit| {
        *counts.entry(unit.domain).or_insert(0usize) += 1;
        counts
    });
    assert_eq!(domain_counts[&Domain::Signature], 37);
    assert_eq!(domain_counts[&Domain::Module], 17);
    assert_eq!(domain_counts[&Domain::Hook], 15);
    assert_eq!(domain_counts[&Domain::Architecture], 15);

    let dispositions = UNITS.iter().fold(BTreeMap::new(), |mut counts, unit| {
        *counts.entry(unit.disposition).or_insert(0usize) += 1;
        counts
    });
    assert_eq!(dispositions[&Disposition::Retain], 48);
    assert_eq!(dispositions[&Disposition::Remove], 17);
    assert_eq!(dispositions[&Disposition::Internalize], 17);
    assert_eq!(dispositions[&Disposition::Consolidate], 2);
    assert!(
        UNITS
            .iter()
            .all(|unit| !unit.baseline_leaf.is_empty() && !unit.final_expectation.is_empty())
    );
    assert!(
        UNITS
            .iter()
            .filter(|unit| {
                unit.disposition == Disposition::Retain
                    && matches!(unit.domain, Domain::Signature | Domain::Module)
                    && unit.id != "M-017"
            })
            .all(|unit| unit.doc_anchor.is_some()),
        "every retained production S/M unit must have a documentation anchor"
    );
    assert_eq!(FINAL_MODULES.len(), 31);
    assert!(manifest::BASELINE_MODULES.contains(&"test_support/engine_driver.rs"));
    assert!(!FINAL_MODULES.contains(&"test_support/engine_driver.rs"));
}

#[test]
fn root_exports_and_module_paths_are_exact() {
    let exports = public_reexports("v2/mod.rs");
    assert_eq!(
        exports,
        expected(&[
            "AccessIntent",
            "Completion",
            "CompletionMode",
            "Completions",
            "Context",
            "Cq",
            "CqBuilder",
            "CqNotifier",
            "CqPoller",
            "Error",
            "MessageTransport",
            "MessageTransportBuilder",
            "Mr",
            "Pd",
            "Qp",
            "QpBuilder",
            "RdmaConnection",
            "RdmaConnectionConfig",
            "RdmaConnectionDiagnostics",
            "RdmaConnectionIdentity",
            "RdmaEngine",
            "RdmaEngineBuilder",
            "RdmaEngineDiagnostics",
            "RdmaEngineDriver",
            "RdmaEngineLifecycle",
            "RdmaEngineTerminalError",
            "RdmaListener",
            "RdmaListenerConfig",
            "RdmaListenerDiagnostics",
            "RdmaOperation",
            "ReceivedMessage",
            "RemoteMr",
            "Result",
            "TokioCompletions",
        ])
    );
    assert_eq!(public_modules("v2/mod.rs"), expected(&["test_support"]));
    assert!(public_modules("lib.rs").contains("v2"));
    assert!(!public_modules("lib.rs").contains("test_support"));
    assert!(!v2_file("test_support/engine_driver.rs").exists());
}

#[test]
fn retained_and_removed_inherent_method_sets_are_exact() {
    let low_level = [
        "v2/context.rs",
        "v2/cq.rs",
        "v2/mr.rs",
        "v2/pd.rs",
        "v2/qp.rs",
        "v2/op.rs",
        "v2/completion.rs",
        "v2/cq_poller.rs",
        "v2/tokio_support.rs",
    ];
    assert_eq!(
        method_set("Context", &low_level),
        expected(&["alloc_pd", "open_by_name", "open_first"])
    );
    assert_eq!(method_set("Pd", &low_level), expected(&["reg_mr"]));
    assert_eq!(
        method_set("CqBuilder", &low_level),
        expected(&["build", "new", "with_channel"])
    );
    assert_eq!(
        method_set("Cq", &low_level),
        expected(&["completions_tokio", "fd", "has_channel", "poll"])
    );
    assert_eq!(
        method_set("Mr", &low_level),
        expected(&[
            "addr",
            "as_mut_slice",
            "as_slice",
            "is_empty",
            "len",
            "lkey",
            "rkey",
            "to_remote",
        ])
    );
    assert!(method_set("RemoteMr", &low_level).is_empty());
    assert_eq!(
        method_set("QpBuilder", &low_level),
        expected(&[
            "build_with_cm",
            "max_recv_sge",
            "max_recv_wr",
            "max_send_sge",
            "max_send_wr",
            "new",
            "sq_sig_all",
        ])
    );
    assert_eq!(
        method_set("Qp", &low_level),
        expected(&[
            "post_read",
            "post_recv",
            "post_send",
            "post_write",
            "qp_num",
            "to_error",
        ])
    );
    assert_eq!(
        method_set("Completion", &low_level),
        expected(&[
            "byte_len",
            "is_success",
            "opcode",
            "qp_num",
            "result",
            "status",
            "vendor_err",
            "wr_id",
        ])
    );
    assert_eq!(
        method_set("Completions", &low_level),
        expected(&["cq", "new", "next"])
    );
    assert_eq!(
        method_set("CqPoller", &low_level),
        expected(&["cq", "new", "poll_completions", "wake"])
    );

    assert_eq!(
        method_set("RdmaConnectionConfig", &["v2/engine/config.rs"]),
        expected(&[
            "initiator_depth",
            "max_recv_sge",
            "max_recv_wr",
            "max_send_sge",
            "max_send_wr",
            "responder_resources",
            "retry_count",
            "rnr_retry_count",
        ])
    );
    assert_eq!(
        method_set("RdmaConnectionIdentity", &["v2/engine/connection.rs"]),
        expected(&["qp_num"])
    );
    assert_eq!(
        method_set("MessageTransport", &["v2/message_transport.rs"]),
        expected(&["close", "ready", "recv", "send"])
    );

    let protocol = parse("v2/protocol.rs");
    assert!(
        protocol.items.iter().all(|item| match item {
            Item::Const(item) => !is_public(&item.vis),
            Item::Enum(item) => !is_public(&item.vis),
            Item::Fn(item) => !is_public(&item.vis),
            Item::Struct(item) => !is_public(&item.vis),
            Item::Type(item) => !is_public(&item.vis),
            _ => true,
        }),
        "protocol implementation leaked a public item"
    );
}

#[test]
fn signature_profiles_and_hook_namespace_are_exact() {
    let production_types = expected(&[
        "Context",
        "Cq",
        "CqBuilder",
        "CqPoller",
        "Completions",
        "Completion",
        "Mr",
        "Pd",
        "Qp",
        "QpBuilder",
        "RdmaConnection",
        "RdmaConnectionConfig",
        "RdmaEngine",
        "RdmaEngineBuilder",
        "RdmaListener",
        "MessageTransport",
        "MessageTransportBuilder",
    ]);
    let mut production = (0, 0);
    for file in FINAL_MODULES.iter().copied().filter(|file| {
        file.starts_with("v2/")
            && *file != "v2/test_support.rs"
            && *file != "v2/engine/driver.rs"
            && *file != "v2/engine/api_tests.rs"
    }) {
        let counts = public_signature_type_counts(file, &production_types);
        production.0 += counts.0;
        production.1 += counts.1;
    }
    assert_eq!(production, (1, 0));

    let hook_exports = public_reexports("v2/test_support.rs");
    assert_eq!(
        hook_exports,
        expected(&[
            "DestructionEvent",
            "DestructionKind",
            "DestructionRecorder",
            "RecorderArmError",
            "TestAcceptedOperation",
            "TestAdmissionBarrier",
            "TestConnectionCqeSuppression",
            "TestContextIdentity",
            "TestCqArmWindowControl",
            "TestCqeSuppression",
            "TestEngineInstrumentation",
            "TestEngineQp",
            "TestEngineResources",
            "TestHelloAttachHook",
            "TestHelloOverride",
            "TestProviderLimits",
            "TestReadyWorkControl",
            "TestRouteHandle",
            "TestSteadyFrame",
        ])
    );
    let hook_types = expected(&[
        "TestEngineResources",
        "TestContextIdentity",
        "TestProviderLimits",
    ]);
    assert_eq!(
        public_signature_type_counts("v2/engine/driver.rs", &hook_types),
        (2, 1)
    );
    assert_eq!(
        method_set("TestEngineResources", &["v2/engine/driver.rs"]),
        expected(&[
            "context_identity",
            "create_qp",
            "disconnect_connection",
            "inject_completion",
            "inject_driver_failure",
            "install_connection",
            "install_idle_connections",
            "install_route",
            "instrumentation",
            "pause_next_connect_before_enqueue",
            "pause_next_cq_arm_window",
            "pause_next_cq_pre_arm_window",
            "pause_next_operation_before_register",
            "pause_ready_work",
            "provider_limits",
            "register_memory",
            "require_context",
            "suppress_next_connection_cqe",
            "suppress_next_connection_cqe_with_opcode",
            "transition_connection_to_error",
        ])
    );
    assert_eq!(
        method_set("TestProviderLimits", &["v2/engine/driver.rs"]),
        expected(&[
            "max_cqe",
            "max_qp",
            "max_qp_init_rd_atom",
            "max_qp_rd_atom",
            "max_qp_wr",
            "max_sge",
        ])
    );
    assert_eq!(
        method_set("TestRouteHandle", &["v2/engine/driver.rs"]),
        expected(&[
            "accepted_outstanding",
            "completions",
            "qp",
            "qp_num",
            "remove",
            "retain",
            "retain_until_completion",
            "suppress_next",
            "wait_for_completion_count",
            "wait_until_drained",
        ])
    );
}

#[test]
fn trait_removals_and_architecture_consolidations_are_exact() {
    let error_source = fs::read_to_string(v2_file("v2/error.rs")).unwrap();
    let completion_source = fs::read_to_string(v2_file("v2/op.rs")).unwrap();
    assert!(!error_source.contains("impl From<crate::Error> for Error"));
    assert!(!completion_source.contains("impl From<WorkCompletion> for Completion"));
    assert!(!completion_source.contains("impl AsRef<WorkCompletion> for Completion"));

    let mut declared = BTreeSet::new();
    for file in FINAL_MODULES
        .iter()
        .copied()
        .filter(|file| file.starts_with("v2/"))
    {
        declared.extend(declared_types(file));
    }
    for removed in [
        "AcceptResult",
        "CloseOutcome",
        "EngineFailure",
        "EngineOutcome",
        "ListenResult",
        "ListenerCloseOutcome",
        "OutboundResult",
        "Op",
        "OpCode",
        "TestCompletionIdentity",
        "TestRegistryProbe",
    ] {
        assert!(
            !declared.contains(removed),
            "removed type remains: {removed}"
        );
    }
    for retained in [
        "TakeOnceResult",
        "MemoizedTerminalResult",
        "PollState",
        "FutureState",
        "OperationLifecycle",
        "BatchPostOutcome",
        "InternalPreparedBatch",
        "PreparedBatchOwnership",
        "CompletionDisposition",
        "DetachedOperationCompletion",
        "StartResult",
        "BatchOwnershipTransfer",
        "ReservationState",
        "SelectedAccept",
        "InboundState",
        "OutboundState",
        "ConnectionCmOwner",
        "EngineMessageEvent",
        "EngineSendRequestAction",
        "PagedRegistry",
        "SlotState",
        "ConnectionRegistry",
        "OperationRegistry",
        "OperationState",
        "QuarantineTransition",
        "QuarantineKey",
        "ContextRoute",
        "CmDispatchRoute",
        "EventDisposition",
        "RouteRetirement",
        "PendingCmDestruction",
        "ListenerQueues",
    ] {
        assert!(
            declared.contains(retained),
            "named architecture leaf missing: {retained}"
        );
    }
}

#[test]
fn retained_external_traits_and_exact_buffer_signatures_compile() {
    fn assert_engine<T: Clone + Send + Sync + 'static>() {}
    fn assert_identity<T: Clone + Copy + std::fmt::Debug + Eq + std::hash::Hash>() {}
    fn assert_message<T: AsRef<[u8]> + std::ops::Deref<Target = [u8]>>() {}
    assert_engine::<rdma_io::v2::RdmaEngine>();
    assert_identity::<rdma_io::v2::RdmaConnectionIdentity>();
    assert_message::<rdma_io::v2::ReceivedMessage>();

    let _: fn(&rdma_io::v2::Cq, &mut [rdma_io::v2::Completion]) -> rdma_io::v2::Result<usize> =
        rdma_io::v2::Cq::poll;
    let _: fn(&rdma_io::v2::RdmaConnectionIdentity) -> u32 =
        rdma_io::v2::RdmaConnectionIdentity::qp_num;
    let _: rdma_io::v2::Error = std::io::Error::other("positive conversion").into();
    assert!(std::mem::needs_drop::<rdma_io::v2::RdmaEngineDriver>());
    assert!(std::mem::needs_drop::<rdma_io::v2::ReceivedMessage>());
}

#[test]
fn aggregate_validation_wires_static_guards_once_per_entry_point() {
    let root = workspace();
    let justfile = fs::read_to_string(root.join("justfile")).unwrap();
    let provider =
        fs::read_to_string(root.join("scripts/validate-v2-engine-providers.sh")).unwrap();
    for guard in [
        "v2_surface_cutover_tests",
        "v2_docs_manifest",
        "v2_docs_legacy_surface",
        "v2_no_hidden_spawn",
        "check-v2-api-surface.sh",
        "check-v2-rustdoc.sh",
    ] {
        assert!(
            justfile.contains(guard),
            "just validate-v2-engine is missing {guard}"
        );
        assert!(
            provider.contains(guard),
            "provider full validation is missing {guard}"
        );
    }
    assert_eq!(
        provider.matches("check-v2-api-surface.sh").count(),
        2,
        "the privilege and non-privilege branches must each name the guard once"
    );
    assert_eq!(provider.matches("check-v2-rustdoc.sh").count(), 2);
    let preflight = provider
        .find("run_static_preflight")
        .expect("static preflight definition");
    let provider_switch = provider
        .find("Unload hardware RDMA providers")
        .expect("provider switching");
    assert!(preflight < provider_switch);
}
