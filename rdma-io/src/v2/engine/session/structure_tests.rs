//! Recursive syntax-aware source checks for the session ownership boundary.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use syn::visit::{self, Visit};
use syn::{
    Attribute, Expr, ExprCall, ExprMethodCall, File, ItemEnum, ItemImpl, ItemMod, ItemStruct,
    ItemType, ItemUse, Meta, Path as SynPath, Type, UseTree, punctuated::Punctuated,
};

const CM_CHILDREN: &[&str] = &["event", "outbound", "inbound", "retirement", "shutdown"];

const TEST_ONLY_MODULES: &[(&str, &str, &str)] = &[
    (
        "src/v2/engine/session/cm/tests.rs",
        "src/v2/engine/session/cm/mod.rs",
        "tests",
    ),
    (
        "src/v2/engine/session/connection/tests.rs",
        "src/v2/engine/session/connection/mod.rs",
        "tests",
    ),
    (
        "src/v2/engine/session/structure_tests.rs",
        "src/v2/engine/session/mod.rs",
        "structure_tests",
    ),
];

const QP_AUTHORITY_PATHS: &[&str] = &[
    "src/v2/engine/session/mod.rs",
    "src/v2/engine/session/connection/mod.rs",
    "src/v2/engine/session/cm/retirement.rs",
];

const EFFECT_COMMIT_PATHS: &[&str] = &[
    "src/v2/engine/session/mod.rs",
    "src/v2/engine/session/drain.rs",
];

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn session_root() -> PathBuf {
    manifest_dir().join("src/v2/engine/session")
}

fn collect_rust_sources(root: &Path) -> io::Result<Vec<PathBuf>> {
    fn visit(path: &Path, sources: &mut Vec<PathBuf>) -> io::Result<()> {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let path = entry.path();
            if file_type.is_dir() {
                visit(&path, sources)?;
            } else if file_type.is_file() && path.extension() == Some(OsStr::new("rs")) {
                sources.push(path);
            }
        }
        Ok(())
    }

    let mut sources = Vec::new();
    visit(root, &mut sources)?;
    sources.sort();
    Ok(sources)
}

fn relative_source(path: &Path) -> String {
    path.strip_prefix(manifest_dir())
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn parse_source(path: &Path) -> File {
    let source = fs::read_to_string(path).expect("Rust source must be readable");
    syn::parse_file(&source)
        .unwrap_or_else(|error| panic!("{} must parse: {error}", relative_source(path)))
}

fn cfg_is_exact_test(attribute: &Attribute) -> bool {
    if !attribute.path().is_ident("cfg") {
        return false;
    }

    match &attribute.meta {
        Meta::List(list) => {
            let predicates = list
                .parse_args_with(Punctuated::<Meta, syn::Token![,]>::parse_terminated)
                .expect("cfg attribute must parse");
            predicates.len() == 1
                && matches!(
                    predicates.first(),
                    Some(Meta::Path(path)) if path.is_ident("test")
                )
        }
        _ => false,
    }
}

fn module_is_test_gated(parent: &Path, module_name: &str) -> bool {
    parse_source(parent).items.iter().any(|item| {
        let syn::Item::Mod(module) = item else {
            return false;
        };
        module.ident == module_name && module.attrs.iter().any(cfg_is_exact_test)
    })
}

fn validate_test_only_paths(sources: &[PathBuf]) {
    for (source, parent, module) in TEST_ONLY_MODULES {
        let source_path = manifest_dir().join(source);
        assert!(
            sources.contains(&source_path),
            "declared test-only source {source} must be discovered"
        );
        assert!(
            module_is_test_gated(&manifest_dir().join(parent), module),
            "{source} may be excluded only while {parent} declares #[cfg(test)] mod {module}"
        );
    }
}

fn is_validated_test_only(path: &Path) -> bool {
    let relative = relative_source(path);
    TEST_ONLY_MODULES
        .iter()
        .any(|(test_path, _, _)| relative == *test_path)
}

fn production_sources(root: &Path) -> Vec<PathBuf> {
    let sources = collect_rust_sources(root).expect("source tree must be recursively readable");
    if root == session_root() {
        validate_test_only_paths(&sources);
    }
    sources
        .into_iter()
        .filter(|path| !is_validated_test_only(path))
        .collect()
}

fn path_segments(path: &SynPath) -> Vec<String> {
    path.segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect()
}

fn path_ends_with(path: &SynPath, suffix: &[&str]) -> bool {
    let segments = path_segments(path);
    segments.len() >= suffix.len()
        && segments[segments.len() - suffix.len()..]
            .iter()
            .map(String::as_str)
            .eq(suffix.iter().copied())
}

fn type_mentions(type_: &Type, expected: &str) -> bool {
    match type_ {
        Type::Path(path) => path.path.segments.iter().any(|segment| {
            segment.ident == expected
                || match &segment.arguments {
                    syn::PathArguments::AngleBracketed(arguments) => {
                        arguments.args.iter().any(|argument| {
                            matches!(
                                argument,
                                syn::GenericArgument::Type(inner)
                                    if type_mentions(inner, expected)
                            )
                        })
                    }
                    _ => false,
                }
        }),
        Type::Reference(reference) => type_mentions(&reference.elem, expected),
        Type::Paren(paren) => type_mentions(&paren.elem, expected),
        _ => false,
    }
}

#[derive(Default)]
struct ArchitectureVisitor {
    skip_test_depth: usize,
    hidden_work: Vec<String>,
    broad_owner_adapters: Vec<String>,
    sibling_references: Vec<String>,
    cm_driver_references: Vec<String>,
    facade_impls: usize,
    qp_authority_calls: Vec<String>,
    effect_commit_calls: Vec<String>,
}

impl ArchitectureVisitor {
    fn record_path(&mut self, path: &SynPath) {
        let segments = path_segments(path);
        let segment_refs = segments.iter().map(String::as_str).collect::<Vec<_>>();
        if segments.iter().any(|segment| segment == "CmState")
            || segments
                .windows(2)
                .any(|window| window[0] == "session" && window[1] == "cm")
        {
            self.cm_driver_references.push(segments.join("::"));
        }

        const HIDDEN_CALLS: &[&[&str]] = &[
            &["std", "thread", "spawn"],
            &["std", "thread", "scope"],
            &["std", "thread", "Builder", "spawn"],
            &["std", "thread", "Builder", "spawn_scoped"],
            &["std", "thread", "Scope", "spawn"],
            &["tokio", "spawn"],
            &["tokio", "task", "spawn"],
            &["tokio", "task", "spawn_blocking"],
            &["tokio", "task", "spawn_local"],
            &["tokio", "runtime", "Handle", "spawn"],
            &["tokio", "runtime", "Handle", "spawn_blocking"],
            &["tokio", "task", "LocalSet", "spawn_local"],
            &["tokio", "task", "JoinSet", "spawn"],
            &["tokio", "task", "JoinSet", "spawn_on"],
            &["tokio", "task", "JoinSet", "spawn_local"],
            &["tokio", "task", "JoinSet", "spawn_local_on"],
            &["tokio", "task", "JoinSet", "spawn_blocking"],
            &["tokio", "task", "JoinSet", "spawn_blocking_on"],
            &["futures_util", "task", "SpawnExt", "spawn"],
            &["futures_util", "task", "LocalSpawnExt", "spawn_local"],
            &["futures_util", "task", "Spawn", "spawn_obj"],
            &["futures_util", "task", "LocalSpawn", "spawn_local_obj"],
            &["libc", "pthread_create"],
        ];
        if HIDDEN_CALLS
            .iter()
            .any(|suffix| path_ends_with(path, suffix))
        {
            self.hidden_work.push(segment_refs.join("::"));
        }

        const HIDDEN_TYPES: &[&[&str]] = &[
            &["std", "thread", "Builder"],
            &["tokio", "runtime", "Runtime"],
            &["tokio", "runtime", "Builder"],
            &["tokio", "task", "JoinSet"],
            &["tokio", "task", "LocalSet"],
        ];
        if HIDDEN_TYPES
            .iter()
            .any(|suffix| path_ends_with(path, suffix))
        {
            self.hidden_work.push(segment_refs.join("::"));
        }
    }

    fn record_call_name(&mut self, name: &str) {
        if matches!(
            name,
            "destroy_qp" | "destroy_connection_resources" | "ensure_qp_destroyed"
        ) {
            self.qp_authority_calls.push(name.to_owned());
        }
        if matches!(
            name,
            "apply_io_effects" | "into_committed" | "commit_io_effects"
        ) {
            self.effect_commit_calls.push(name.to_owned());
        }
    }

    fn record_sibling_path(&mut self, path: &SynPath) {
        let segments = path_segments(path);
        for sibling in CM_CHILDREN {
            let direct_parent = segments
                .windows(2)
                .any(|window| window[0] == "super" && window[1] == *sibling);
            let qualified = segments
                .windows(5)
                .any(|window| window == ["v2", "engine", "session", "cm", *sibling]);
            if direct_parent || qualified {
                self.sibling_references.push(segments.join("::"));
            }
        }
    }

    fn record_use_tree(&mut self, tree: &UseTree, prefix: &mut Vec<String>) {
        match tree {
            UseTree::Path(path) => {
                prefix.push(path.ident.to_string());
                self.record_use_tree(&path.tree, prefix);
                prefix.pop();
            }
            UseTree::Name(name) => {
                prefix.push(name.ident.to_string());
                self.record_use_segments(prefix);
                prefix.pop();
            }
            UseTree::Rename(rename) => {
                prefix.push(rename.ident.to_string());
                self.record_use_segments(prefix);
                prefix.pop();
            }
            UseTree::Glob(_) => {
                if prefix.len() == 1 && prefix[0] == "super" {
                    self.sibling_references
                        .push("wildcard parent import".to_owned());
                }
                self.record_use_segments(prefix);
            }
            UseTree::Group(group) => {
                for item in &group.items {
                    self.record_use_tree(item, prefix);
                }
            }
        }
    }

    fn record_use_segments(&mut self, segments: &[String]) {
        for sibling in CM_CHILDREN {
            let direct_parent = segments
                .windows(2)
                .any(|window| window[0] == "super" && window[1] == *sibling);
            let qualified = segments
                .windows(5)
                .any(|window| window == ["v2", "engine", "session", "cm", *sibling]);
            if direct_parent || qualified {
                self.sibling_references.push(segments.join("::"));
            }
        }
    }

    fn record_effect_publish(&mut self, receiver: &Expr) {
        let effect_receiver = match receiver {
            Expr::Path(path) => path
                .path
                .segments
                .last()
                .is_some_and(|segment| segment.ident.to_string().contains("effect")),
            Expr::MethodCall(call) => matches!(
                call.method.to_string().as_str(),
                "apply_io_effects" | "into_committed" | "commit_io_effects"
            ),
            _ => false,
        };
        if effect_receiver {
            self.effect_commit_calls.push("publish".to_owned());
        }
    }
}

impl<'ast> Visit<'ast> for ArchitectureVisitor {
    fn visit_item_mod(&mut self, module: &'ast ItemMod) {
        if module.attrs.iter().any(cfg_is_exact_test) {
            self.skip_test_depth += 1;
            self.skip_test_depth -= 1;
            return;
        }
        visit::visit_item_mod(self, module);
    }

    fn visit_path(&mut self, path: &'ast SynPath) {
        if self.skip_test_depth == 0 {
            self.record_path(path);
            self.record_sibling_path(path);
        }
        visit::visit_path(self, path);
    }

    fn visit_item_use(&mut self, item: &'ast ItemUse) {
        if self.skip_test_depth == 0 {
            self.record_use_tree(&item.tree, &mut Vec::new());
        }
        visit::visit_item_use(self, item);
    }

    fn visit_item_impl(&mut self, item: &'ast ItemImpl) {
        if self.skip_test_depth == 0 {
            if type_mentions(&item.self_ty, "CmState") {
                self.facade_impls += 1;
            }
            if let Some((_, trait_path, _)) = &item.trait_ {
                let trait_name = trait_path
                    .segments
                    .last()
                    .map(|segment| segment.ident.to_string())
                    .unwrap_or_default();
                let broad_trait = matches!(
                    trait_name.as_str(),
                    "Deref" | "DerefMut" | "AsRef" | "Borrow"
                );
                let target_is_manager = type_mentions(&item.self_ty, "SessionManager")
                    || trait_path.segments.iter().any(|segment| {
                        matches!(
                            &segment.arguments,
                            syn::PathArguments::AngleBracketed(arguments)
                                if arguments.args.iter().any(|argument| {
                                    matches!(
                                        argument,
                                        syn::GenericArgument::Type(type_)
                                            if type_mentions(type_, "SessionManager")
                                    )
                                })
                        )
                    })
                    || item.items.iter().any(|impl_item| {
                        matches!(
                            impl_item,
                            syn::ImplItem::Type(type_item)
                                if type_item.ident == "Target"
                                    && type_mentions(&type_item.ty, "SessionManager")
                        )
                    });
                if broad_trait && target_is_manager {
                    self.broad_owner_adapters.push(trait_name);
                }
            }
        }
        visit::visit_item_impl(self, item);
    }

    fn visit_item_struct(&mut self, item: &'ast ItemStruct) {
        if self.skip_test_depth == 0 && item.ident == "CmState" {
            self.cm_driver_references.push(item.ident.to_string());
        }
        visit::visit_item_struct(self, item);
    }

    fn visit_item_enum(&mut self, item: &'ast ItemEnum) {
        if self.skip_test_depth == 0 && item.ident == "CmState" {
            self.cm_driver_references.push(item.ident.to_string());
        }
        visit::visit_item_enum(self, item);
    }

    fn visit_item_type(&mut self, item: &'ast ItemType) {
        if self.skip_test_depth == 0 && item.ident == "CmState" {
            self.cm_driver_references.push(item.ident.to_string());
        }
        visit::visit_item_type(self, item);
    }

    fn visit_expr_call(&mut self, call: &'ast ExprCall) {
        if self.skip_test_depth == 0
            && let Expr::Path(path) = &*call.func
        {
            self.record_path(&path.path);
            if let Some(segment) = path.path.segments.last() {
                self.record_call_name(&segment.ident.to_string());
            }
        }
        visit::visit_expr_call(self, call);
    }

    fn visit_expr_method_call(&mut self, call: &'ast ExprMethodCall) {
        if self.skip_test_depth == 0 {
            let method = call.method.to_string();
            if matches!(
                method.as_str(),
                "spawn"
                    | "spawn_scoped"
                    | "spawn_local"
                    | "spawn_blocking"
                    | "spawn_on"
                    | "spawn_local_on"
                    | "spawn_blocking_on"
                    | "spawn_obj"
                    | "spawn_local_obj"
            ) {
                self.hidden_work.push(format!(".{method}"));
            }
            self.record_call_name(&method);
            if method == "publish" {
                self.record_effect_publish(&call.receiver);
            }
        }
        visit::visit_expr_method_call(self, call);
    }
}

fn inspect_file(path: &Path) -> ArchitectureVisitor {
    let file = parse_source(path);
    let mut visitor = ArchitectureVisitor::default();
    visitor.visit_file(&file);
    visitor
}

fn assert_no_hidden_work_or_broad_adapters(path: &Path) {
    let findings = inspect_file(path);
    assert!(
        findings.hidden_work.is_empty(),
        "{} contains hidden-work syntax: {:?}",
        relative_source(path),
        findings.hidden_work
    );
    assert!(
        findings.broad_owner_adapters.is_empty(),
        "{} contains broad SessionManager adapter syntax: {:?}",
        relative_source(path),
        findings.broad_owner_adapters
    );
}

fn assert_sensitive_calls_stay_in(
    sources: &[PathBuf],
    select: impl Fn(&ArchitectureVisitor) -> &[String],
    allowed_relative_paths: &[&str],
) {
    for path in sources {
        let findings = inspect_file(path);
        let calls = select(&findings);
        if !calls.is_empty() {
            let relative = relative_source(path);
            assert!(
                allowed_relative_paths.contains(&relative.as_str()),
                "{relative} uses authority-sensitive calls {calls:?}; allowed paths: {allowed_relative_paths:?}"
            );
        }
    }
}

#[test]
fn recursive_session_discovery_reaches_nested_modules() {
    let sources = collect_rust_sources(&session_root()).expect("discover session Rust sources");
    validate_test_only_paths(&sources);
    let relative = sources
        .iter()
        .map(|path| relative_source(path))
        .collect::<Vec<_>>();

    assert!(
        relative
            .iter()
            .any(|path| path.ends_with("session/cm/tests.rs")),
        "recursive discovery must include nested CM tests"
    );
    assert!(
        relative
            .iter()
            .any(|path| path.ends_with("session/connection/tests.rs")),
        "recursive discovery must include nested connection tests"
    );
    assert!(
        relative
            .iter()
            .any(|path| path.ends_with("session/structure_tests.rs")),
        "recursive discovery must include this structural guard"
    );
}

#[test]
fn production_session_sources_reject_hidden_work_and_broad_owner_adapters() {
    for path in production_sources(&session_root()) {
        assert_no_hidden_work_or_broad_adapters(&path);
    }
}

#[test]
fn cm_children_cannot_reference_siblings_or_own_facade_impls() {
    let cm_root = session_root().join("cm");
    for path in production_sources(&cm_root) {
        let Some(module) = path.file_stem().and_then(OsStr::to_str) else {
            continue;
        };
        if !CM_CHILDREN.contains(&module) {
            continue;
        }

        let findings = inspect_file(&path);
        assert!(
            findings.sibling_references.is_empty(),
            "{} references sibling module(s): {:?}",
            relative_source(&path),
            findings.sibling_references
        );
        assert_eq!(
            findings.facade_impls,
            0,
            "{} must expose operations to the parent instead of adding sibling-callable facade methods",
            relative_source(&path)
        );
    }
}

#[test]
fn engine_driver_cannot_own_cm_implementation() {
    let driver_root = manifest_dir().join("src/v2/engine/driver");
    for path in production_sources(&driver_root) {
        let visitor = inspect_file(&path);
        assert!(
            visitor.cm_driver_references.is_empty(),
            "{} must not own or import CM implementation: {:?}",
            relative_source(&path),
            visitor.cm_driver_references
        );
    }
}

#[test]
fn provider_mutation_and_effect_publication_stay_in_owner_paths() {
    let sources = production_sources(&session_root());
    assert_sensitive_calls_stay_in(
        &sources,
        |findings| &findings.qp_authority_calls,
        QP_AUTHORITY_PATHS,
    );
    assert_sensitive_calls_stay_in(
        &sources,
        |findings| &findings.effect_commit_calls,
        EFFECT_COMMIT_PATHS,
    );
}

#[test]
fn recursive_walker_and_syntax_guards_reject_nested_bypasses() {
    let unique = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "rust-rdma-io-session-structure-{}-{unique}",
        std::process::id()
    ));
    let nested = root.join("new/child");
    fs::create_dir_all(&nested).expect("create nested structural-test fixture");
    let source_path = nested.join("tests.rs");
    fs::write(
        &source_path,
        r#"
            use crate::v2::engine::session::cm::{event::Snapshot, shutdown as sibling};
            fn hidden() {
                let _text = "tokio::spawn(async {})";
                // WorkRequestPoster::destroy_qp is documentation, not a call.
                std::thread::scope(|_| {});
                tokio::task::spawn_local(async {});
                tokio::task::JoinSet::new().spawn(async {});
                join_set.spawn_on(async {}, handle);
                join_set.spawn_local_on(async {}, local_set);
                join_set.spawn_blocking_on(|| {}, handle);
                executor.spawn_obj(future);
                executor.spawn_local_obj(future);
                WorkRequestPoster::destroy_qp(poster, authority);
            }
        "#,
    )
    .expect("write structural-test fixture");
    let comments_path = nested.join("comments.rs");
    fs::write(
        &comments_path,
        r#"
            // tokio::spawn(async {}) and WorkRequestPoster::destroy_qp are documentation.
            const EXAMPLE: &str = "use crate::v2::engine::session::cm::event;";
        "#,
    )
    .expect("write comment/string false-positive fixture");

    let sources = collect_rust_sources(&root).expect("walk nested fixture");
    assert_eq!(sources, vec![comments_path.clone(), source_path.clone()]);
    assert!(
        !is_validated_test_only(&source_path),
        "an arbitrary production file named tests.rs must not bypass guards"
    );

    let findings = inspect_file(&source_path);
    assert!(
        findings
            .hidden_work
            .iter()
            .any(|finding| finding.contains("spawn_local"))
    );
    assert!(
        findings
            .hidden_work
            .iter()
            .any(|finding| finding == ".spawn")
    );
    for method in [
        "std::thread::scope",
        ".spawn_on",
        ".spawn_local_on",
        ".spawn_blocking_on",
        ".spawn_obj",
        ".spawn_local_obj",
    ] {
        assert!(
            findings.hidden_work.iter().any(|finding| finding == method),
            "missing hidden-work finding for {method}"
        );
    }
    assert!(
        findings
            .qp_authority_calls
            .iter()
            .any(|finding| finding == "destroy_qp")
    );
    assert!(
        findings
            .sibling_references
            .iter()
            .any(|finding| finding.contains("event"))
    );
    assert!(
        findings
            .sibling_references
            .iter()
            .any(|finding| finding.contains("shutdown"))
    );

    let comment_findings = inspect_file(&comments_path);
    assert!(
        comment_findings.hidden_work.is_empty()
            && comment_findings.qp_authority_calls.is_empty()
            && comment_findings.sibling_references.is_empty()
            && comment_findings.cm_driver_references.is_empty(),
        "comments and strings must not create findings"
    );

    fs::remove_dir_all(&root).expect("remove structural-test fixture");
}

#[test]
fn cfg_test_detection_requires_an_actual_gated_module() {
    let gated: File = syn::parse_quote! {
        #[cfg(test)]
        mod tests;
    };
    let ungated: File = syn::parse_quote! {
        mod tests;
    };
    let mixed: File = syn::parse_quote! {
        #[cfg(any(test, feature = "test-hooks"))]
        mod tests;
    };
    let inverse: File = syn::parse_quote! {
        #[cfg(not(test))]
        mod tests;
    };

    let has_gated_tests = |file: &File| {
        file.items.iter().any(|item| {
            matches!(
                item,
                syn::Item::Mod(module)
                    if module.ident == "tests"
                        && module.attrs.iter().any(cfg_is_exact_test)
            )
        })
    };
    assert!(has_gated_tests(&gated));
    assert!(!has_gated_tests(&ungated));
    assert!(!has_gated_tests(&mixed));
    assert!(!has_gated_tests(&inverse));
}
