#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Domain {
    Signature,
    Module,
    Hook,
    Architecture,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Disposition {
    Retain,
    Remove,
    Internalize,
    Consolidate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurfaceProfile {
    Core,
    Production,
    Hooks,
}

#[derive(Clone, Copy, Debug)]
pub struct MethodSet {
    pub profile: SurfaceProfile,
    pub type_name: &'static str,
    pub modules: &'static [&'static str],
    pub methods: &'static [&'static str],
}

#[derive(Clone, Copy, Debug)]
pub struct TraitImplSelector {
    pub module: &'static str,
    pub self_type: &'static str,
    pub trait_path: &'static str,
}

#[derive(Clone, Copy, Debug)]
pub struct Unit {
    pub id: &'static str,
    pub domain: Domain,
    pub disposition: Disposition,
    pub baseline_leaf: &'static str,
    pub final_expectation: &'static str,
    pub doc_anchor: Option<&'static str>,
}

macro_rules! unit {
    ($id:literal, $domain:ident, $disposition:ident, $leaf:literal, $final:literal) => {
        Unit {
            id: $id,
            domain: Domain::$domain,
            disposition: Disposition::$disposition,
            baseline_leaf: $leaf,
            final_expectation: $final,
            doc_anchor: None,
        }
    };
    ($id:literal, $domain:ident, $disposition:ident, $leaf:literal, $final:literal, $doc:literal) => {
        Unit {
            id: $id,
            domain: Domain::$domain,
            disposition: Disposition::$disposition,
            baseline_leaf: $leaf,
            final_expectation: $final,
            doc_anchor: Some($doc),
        }
    };
}

pub const UNITS: &[Unit] = &[
    unit!(
        "S-001",
        Signature,
        Retain,
        "Result<T>; Error variants and named fields",
        "rdma_io::v2::{Result,Error}",
        "Error"
    ),
    unit!(
        "S-002",
        Signature,
        Retain,
        "Context::{open_first,open_by_name,alloc_pd}",
        "owned librdmacm-anchor openers",
        "Context"
    ),
    unit!("S-003", Signature, Remove, "Context::from_cm", "absent"),
    unit!(
        "S-004",
        Signature,
        Remove,
        "Context::{from_inner,inner}",
        "absent"
    ),
    unit!(
        "S-005",
        Signature,
        Retain,
        "Pd::reg_mr",
        "typed registration",
        "Pd"
    ),
    unit!("S-006", Signature, Remove, "Pd::{inner,context}", "absent"),
    unit!(
        "S-007",
        Signature,
        Retain,
        "AccessIntent variants",
        "root-only typed intent",
        "AccessIntent"
    ),
    unit!(
        "S-008",
        Signature,
        Internalize,
        "AccessIntent::to_flags",
        "crate-private"
    ),
    unit!(
        "S-009",
        Signature,
        Retain,
        "Mr typed accessors and to_remote",
        "root-only typed MR",
        "Mr"
    ),
    unit!(
        "S-010",
        Signature,
        Remove,
        "Mr::{inner,inner_mut}",
        "absent"
    ),
    unit!(
        "S-011",
        Signature,
        Retain,
        "RemoteMr::{addr,rkey,len}",
        "root-only typed descriptor",
        "RemoteMr"
    ),
    unit!(
        "S-012",
        Signature,
        Remove,
        "RemoteMr V1 conversions; From<crate::Error>",
        "absent"
    ),
    unit!(
        "S-013",
        Signature,
        Retain,
        "CqBuilder::{new,with_channel,build}",
        "root-only builder",
        "CqBuilder"
    ),
    unit!(
        "S-014",
        Signature,
        Retain,
        "Cq::poll",
        "&mut [Completion]",
        "Cq"
    ),
    unit!(
        "S-015",
        Signature,
        Retain,
        "Cq::{fd,has_channel}",
        "typed readiness discovery",
        "Cq"
    ),
    unit!("S-016", Signature, Remove, "Cq::{inner,channel}", "absent"),
    unit!(
        "S-017",
        Signature,
        Retain,
        "Completions::{new,next,cq}; CqNotifier",
        "typed generic readiness",
        "Completions"
    ),
    unit!(
        "S-018",
        Signature,
        Remove,
        "Completions::poll_next",
        "absent"
    ),
    unit!(
        "S-019",
        Signature,
        Retain,
        "TokioCompletions; completions_tokio",
        "canonical Tokio specialization",
        "TokioCompletions"
    ),
    unit!(
        "S-020",
        Signature,
        Retain,
        "CqPoller::{new,poll_completions,wake,cq}",
        "typed external-wake polling",
        "CqPoller"
    ),
    unit!("S-021", Signature, Remove, "CqPoller::into_cq", "absent"),
    unit!(
        "S-022",
        Signature,
        Retain,
        "QpBuilder typed setters/defaults",
        "root-only builder",
        "QpBuilder"
    ),
    unit!("S-023", Signature, Remove, "QpBuilder::attr", "absent"),
    unit!(
        "S-024",
        Signature,
        Retain,
        "QpBuilder::build_with_cm(&CmId)",
        "sole production CmId signature",
        "QpBuilder"
    ),
    unit!(
        "S-025",
        Signature,
        Retain,
        "Qp named posts, qp_num, to_error",
        "sole direct submission facade",
        "Qp"
    ),
    unit!(
        "S-026",
        Signature,
        Remove,
        "Qp::{from_cm_qp,inner}",
        "absent"
    ),
    unit!(
        "S-027",
        Signature,
        Remove,
        "Op; OpCode; Qp::submit",
        "absent"
    ),
    unit!(
        "S-028",
        Signature,
        Retain,
        "Completion and eight accessors",
        "canonical typed CQE",
        "Completion"
    ),
    unit!(
        "S-029",
        Signature,
        Remove,
        "public WorkCompletion conversions/signatures",
        "absent"
    ),
    unit!(
        "S-030",
        Signature,
        Retain,
        "WcStatus and WcOpcode returns",
        "shared typed vocabulary",
        "Completion"
    ),
    unit!(
        "S-031",
        Signature,
        Internalize,
        "v2::protocol public leaves",
        "private wire implementation"
    ),
    unit!(
        "S-032",
        Signature,
        Retain,
        "engine/listener/connection/message families",
        "root-only production frontends",
        "RdmaEngineBuilder"
    ),
    unit!(
        "S-033",
        Signature,
        Retain,
        "diagnostics types/methods/105 fields",
        "typed O(1)/O(N) diagnostics",
        "RdmaEngineDiagnostics"
    ),
    unit!(
        "S-034",
        Signature,
        Retain,
        "RdmaConnectionIdentity",
        "opaque Eq/Hash/Debug plus qp_num",
        "RdmaConnectionIdentity"
    ),
    unit!(
        "S-035",
        Signature,
        Retain,
        "explicit_engine_drivers; library_owned_tasks",
        "declarative diagnostics invariants",
        "RdmaEngineDiagnostics"
    ),
    unit!(
        "S-036",
        Signature,
        Remove,
        "eight config getters and four *_value methods",
        "absent"
    ),
    unit!(
        "S-037",
        Signature,
        Remove,
        "MessageTransport::buffer_size",
        "absent"
    ),
    unit!(
        "M-001",
        Module,
        Retain,
        "rdma_io::v2",
        "sole production facade",
        "v2"
    ),
    unit!("M-002", Module, Internalize, "v2::context", "private"),
    unit!("M-003", Module, Internalize, "v2::cq", "private"),
    unit!("M-004", Module, Internalize, "v2::error", "private"),
    unit!("M-005", Module, Internalize, "v2::mr", "private"),
    unit!("M-006", Module, Internalize, "v2::op", "private"),
    unit!("M-007", Module, Internalize, "v2::pd", "private"),
    unit!("M-008", Module, Internalize, "v2::qp", "private"),
    unit!("M-009", Module, Internalize, "v2::completion", "private"),
    unit!("M-010", Module, Internalize, "v2::cq_poller", "private"),
    unit!("M-011", Module, Internalize, "v2::engine", "private"),
    unit!(
        "M-012",
        Module,
        Internalize,
        "v2::message_transport",
        "private"
    ),
    unit!("M-013", Module, Internalize, "v2::protocol", "private"),
    unit!(
        "M-014",
        Module,
        Remove,
        "rdma_io::test_support",
        "not externally reachable"
    ),
    unit!(
        "M-015",
        Module,
        Remove,
        "rdma_io::test_support::destruction",
        "not externally reachable"
    ),
    unit!(
        "M-016",
        Module,
        Remove,
        "test_support::engine_driver",
        "file/module absent"
    ),
    unit!(
        "M-017",
        Module,
        Retain,
        "rdma_io::v2::test_support",
        "sole conditional hook namespace"
    ),
    unit!(
        "H-001",
        Hook,
        Retain,
        "test_resources; TestEngineResources; context/provider snapshots",
        "v2::test_support only"
    ),
    unit!(
        "H-002",
        Hook,
        Internalize,
        "TestConnectionIdentity",
        "cfg(test) only"
    ),
    unit!(
        "H-003",
        Hook,
        Retain,
        "TestAcceptedOperation",
        "v2::test_support only"
    ),
    unit!(
        "H-004",
        Hook,
        Internalize,
        "TestCompletionIdentity",
        "replaced by Completion"
    ),
    unit!(
        "H-005",
        Hook,
        Retain,
        "TestEngineQp; TestRouteHandle",
        "v2::test_support only"
    ),
    unit!(
        "H-006",
        Hook,
        Retain,
        "five pause/suppression controls",
        "v2::test_support only"
    ),
    unit!(
        "H-007",
        Hook,
        Retain,
        "aggregate instrumentation and accepted_test_operations",
        "v2::test_support only"
    ),
    unit!(
        "H-008",
        Hook,
        Retain,
        "20 TestEngineResources methods",
        "v2::test_support only"
    ),
    unit!(
        "H-009",
        Hook,
        Retain,
        "control/route/context/provider methods",
        "v2::test_support only"
    ),
    unit!(
        "H-010",
        Hook,
        Internalize,
        "TestRegistryProbe; probe_connection_registry",
        "cfg(test) registry proof"
    ),
    unit!(
        "H-011",
        Hook,
        Retain,
        "accept_with_test_setup_failure",
        "listener inherent hook"
    ),
    unit!(
        "H-012",
        Hook,
        Retain,
        "message hook types and inherent methods",
        "v2::test_support types"
    ),
    unit!(
        "H-013",
        Hook,
        Retain,
        "DestructionEvent fields",
        "v2::test_support only"
    ),
    unit!(
        "H-014",
        Hook,
        Retain,
        "DestructionKind variants",
        "v2::test_support only"
    ),
    unit!(
        "H-015",
        Hook,
        Retain,
        "DestructionRecorder; RecorderArmError",
        "v2::test_support only"
    ),
    unit!(
        "A-001",
        Architecture,
        Consolidate,
        "ListenResult; AcceptResult; OutboundResult",
        "TakeOnceResult<T>"
    ),
    unit!(
        "A-002",
        Architecture,
        Consolidate,
        "EngineOutcome; EngineFailure; CloseOutcome; ListenerCloseOutcome",
        "MemoizedTerminalResult"
    ),
    unit!(
        "A-003",
        Architecture,
        Retain,
        "PollState; RdmaEngineLifecycle",
        "distinct"
    ),
    unit!(
        "A-004",
        Architecture,
        Retain,
        "operation poll/post/transfer family",
        "nine non-overlapping members"
    ),
    unit!(
        "A-005",
        Architecture,
        Retain,
        "ReservationState",
        "single admission transition owner"
    ),
    unit!(
        "A-006",
        Architecture,
        Retain,
        "SelectedAccept; InboundState; OutboundState; ConnectionCmOwner",
        "distinct"
    ),
    unit!(
        "A-007",
        Architecture,
        Retain,
        "message lifecycle/event/send action",
        "distinct"
    ),
    unit!(
        "A-008",
        Architecture,
        Retain,
        "PagedRegistry; SlotState; typed adapters",
        "one paged storage owner"
    ),
    unit!(
        "A-009",
        Architecture,
        Retain,
        "OperationState; QuarantineTransition; QuarantineKey",
        "one reclamation path"
    ),
    unit!(
        "A-010",
        Architecture,
        Retain,
        "published-ready and scheduler queues",
        "one staged ready path"
    ),
    unit!(
        "A-011",
        Architecture,
        Retain,
        "deadline requests and heap",
        "one staged deadline path"
    ),
    unit!(
        "A-012",
        Architecture,
        Retain,
        "CM route/dispatch/retirement/destruction family",
        "non-overlapping queues"
    ),
    unit!(
        "A-013",
        Architecture,
        Retain,
        "ListenerQueues",
        "one arbitration owner"
    ),
    unit!(
        "A-014",
        Architecture,
        Retain,
        "completion/message/delivery FIFOs",
        "distinct payload owners"
    ),
    unit!(
        "A-015",
        Architecture,
        Retain,
        "six canonical builders",
        "no alternate builders"
    ),
];

const LOW_LEVEL_MODULES: &[&str] = &[
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

const CORE_LOW_LEVEL_MODULES: &[&str] = &[
    "v2/context.rs",
    "v2/cq.rs",
    "v2/mr.rs",
    "v2/pd.rs",
    "v2/qp.rs",
    "v2/op.rs",
];

pub const METHOD_SETS: &[MethodSet] = &[
    MethodSet {
        profile: SurfaceProfile::Core,
        type_name: "Context",
        modules: CORE_LOW_LEVEL_MODULES,
        methods: &["alloc_pd", "open_by_name", "open_first"],
    },
    MethodSet {
        profile: SurfaceProfile::Core,
        type_name: "Cq",
        modules: CORE_LOW_LEVEL_MODULES,
        methods: &["fd", "has_channel", "poll"],
    },
    MethodSet {
        profile: SurfaceProfile::Production,
        type_name: "Context",
        modules: LOW_LEVEL_MODULES,
        methods: &["alloc_pd", "open_by_name", "open_first"],
    },
    MethodSet {
        profile: SurfaceProfile::Production,
        type_name: "Pd",
        modules: LOW_LEVEL_MODULES,
        methods: &["reg_mr"],
    },
    MethodSet {
        profile: SurfaceProfile::Production,
        type_name: "CqBuilder",
        modules: LOW_LEVEL_MODULES,
        methods: &["build", "new", "with_channel"],
    },
    MethodSet {
        profile: SurfaceProfile::Production,
        type_name: "Cq",
        modules: LOW_LEVEL_MODULES,
        methods: &["completions_tokio", "fd", "has_channel", "poll"],
    },
    MethodSet {
        profile: SurfaceProfile::Production,
        type_name: "Mr",
        modules: LOW_LEVEL_MODULES,
        methods: &[
            "addr",
            "as_mut_slice",
            "as_slice",
            "is_empty",
            "len",
            "lkey",
            "rkey",
            "to_remote",
        ],
    },
    MethodSet {
        profile: SurfaceProfile::Production,
        type_name: "RemoteMr",
        modules: LOW_LEVEL_MODULES,
        methods: &[],
    },
    MethodSet {
        profile: SurfaceProfile::Production,
        type_name: "QpBuilder",
        modules: LOW_LEVEL_MODULES,
        methods: &[
            "build_with_cm",
            "max_recv_sge",
            "max_recv_wr",
            "max_send_sge",
            "max_send_wr",
            "new",
            "sq_sig_all",
        ],
    },
    MethodSet {
        profile: SurfaceProfile::Production,
        type_name: "Qp",
        modules: LOW_LEVEL_MODULES,
        methods: &[
            "post_read",
            "post_recv",
            "post_send",
            "post_write",
            "qp_num",
            "to_error",
        ],
    },
    MethodSet {
        profile: SurfaceProfile::Production,
        type_name: "Completion",
        modules: LOW_LEVEL_MODULES,
        methods: &[
            "byte_len",
            "is_success",
            "opcode",
            "qp_num",
            "result",
            "status",
            "vendor_err",
            "wr_id",
        ],
    },
    MethodSet {
        profile: SurfaceProfile::Production,
        type_name: "Completions",
        modules: LOW_LEVEL_MODULES,
        methods: &["cq", "new", "next"],
    },
    MethodSet {
        profile: SurfaceProfile::Production,
        type_name: "CqPoller",
        modules: LOW_LEVEL_MODULES,
        methods: &["cq", "new", "poll_completions", "wake"],
    },
    MethodSet {
        profile: SurfaceProfile::Production,
        type_name: "RdmaConnectionConfig",
        modules: &["v2/engine/config.rs"],
        methods: &[
            "initiator_depth",
            "max_recv_sge",
            "max_recv_wr",
            "max_send_sge",
            "max_send_wr",
            "responder_resources",
            "retry_count",
            "rnr_retry_count",
        ],
    },
    MethodSet {
        profile: SurfaceProfile::Production,
        type_name: "RdmaConnectionIdentity",
        modules: &["v2/engine/connection.rs"],
        methods: &["qp_num"],
    },
    MethodSet {
        profile: SurfaceProfile::Production,
        type_name: "MessageTransport",
        modules: &["v2/message_transport.rs"],
        methods: &["close", "ready", "recv", "send"],
    },
    MethodSet {
        profile: SurfaceProfile::Hooks,
        type_name: "TestEngineResources",
        modules: &["v2/engine/driver.rs"],
        methods: &[
            "accepted_operation_wr_ids",
            "connection_registry_identity",
            "context_identity",
            "create_qp",
            "disconnect_connection",
            "fail_next_connection_qp_destroy",
            "inject_completion",
            "inject_driver_failure",
            "install_connection",
            "install_idle_connections",
            "install_route",
            "instrumentation",
            "operation_registry_identity",
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
            "suppress_next_connection_flush_cqe",
            "transition_connection_to_error",
        ],
    },
    MethodSet {
        profile: SurfaceProfile::Hooks,
        type_name: "TestProviderLimits",
        modules: &["v2/engine/driver.rs"],
        methods: &[
            "max_cqe",
            "max_qp",
            "max_qp_init_rd_atom",
            "max_qp_rd_atom",
            "max_qp_wr",
            "max_sge",
        ],
    },
    MethodSet {
        profile: SurfaceProfile::Hooks,
        type_name: "TestRouteHandle",
        modules: &["v2/engine/driver.rs"],
        methods: &[
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
        ],
    },
];

pub const PRODUCTION_EXPORTS: &[&str] = &[
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
];

pub const HOOK_EXPORTS: &[&str] = &[
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
];

pub const REMOVED_ARCHITECTURE_TYPES: &[&str] = &[
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
];

pub const RETAINED_ARCHITECTURE_TYPES: &[&str] = &[
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
];

pub const TRAIT_IMPL_REMOVALS: &[TraitImplSelector] = &[
    TraitImplSelector {
        module: "v2/error.rs",
        self_type: "Error",
        trait_path: "From<crate::Error>",
    },
    TraitImplSelector {
        module: "v2/op.rs",
        self_type: "Completion",
        trait_path: "From<WorkCompletion>",
    },
    TraitImplSelector {
        module: "v2/op.rs",
        self_type: "Completion",
        trait_path: "AsRef<WorkCompletion>",
    },
];

pub const API_FIXTURE_MANIFEST: &str = include_str!("v2_api_fixture_manifest.tsv");
pub const RUSTDOC_MANIFEST: &str = include_str!("v2_rustdoc_manifest.tsv");

pub const BASELINE_MODULES: &[&str] = &[
    "v2/mod.rs",
    "v2/context.rs",
    "v2/cq.rs",
    "v2/error.rs",
    "v2/mr.rs",
    "v2/op.rs",
    "v2/pd.rs",
    "v2/qp.rs",
    "v2/completion.rs",
    "v2/cq_poller.rs",
    "v2/tokio_support.rs",
    "v2/protocol.rs",
    "v2/message_transport.rs",
    "v2/engine/mod.rs",
    "v2/engine/config.rs",
    "v2/engine/connection.rs",
    "v2/engine/diagnostics.rs",
    "v2/engine/listener.rs",
    "v2/engine/operation.rs",
    "v2/engine/resources.rs",
    "v2/engine/lifecycle.rs",
    "v2/engine/drain.rs",
    "v2/engine/driver.rs",
    "v2/engine/registry.rs",
    "v2/engine/scheduler.rs",
    "v2/engine/cm.rs",
    "v2/engine/api_tests.rs",
    "lib.rs",
    "test_support/mod.rs",
    "test_support/destruction.rs",
    "test_support/engine_driver.rs",
];

pub const FINAL_MODULES: &[&str] = &[
    "v2/mod.rs",
    "v2/context.rs",
    "v2/cq.rs",
    "v2/error.rs",
    "v2/mr.rs",
    "v2/op.rs",
    "v2/pd.rs",
    "v2/qp.rs",
    "v2/completion.rs",
    "v2/cq_poller.rs",
    "v2/tokio_support.rs",
    "v2/protocol.rs",
    "v2/message_transport.rs",
    "v2/test_support.rs",
    "v2/engine/mod.rs",
    "v2/engine/config.rs",
    "v2/engine/connection.rs",
    "v2/engine/diagnostics.rs",
    "v2/engine/listener.rs",
    "v2/engine/operation.rs",
    "v2/engine/resources.rs",
    "v2/engine/lifecycle.rs",
    "v2/engine/drain.rs",
    "v2/engine/driver.rs",
    "v2/engine/registry.rs",
    "v2/engine/scheduler.rs",
    "v2/engine/cm.rs",
    "v2/engine/api_tests.rs",
    "lib.rs",
    "test_support/mod.rs",
    "test_support/destruction.rs",
];
