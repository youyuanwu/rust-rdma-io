//! Exact selector-bounded checks for the final V2 surface and ownership model.

#[path = "fixtures/v2_surface_manifest.rs"]
mod manifest;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use manifest::{
    Disposition, Domain, FINAL_MODULES, HOOK_EXPORTS, METHOD_SETS, PRODUCTION_EXPORTS,
    REMOVED_ARCHITECTURE_TYPES, RETAINED_ARCHITECTURE_TYPES, SurfaceProfile, TRAIT_IMPL_REMOVALS,
    UNITS,
};
use syn::parse::Parser;
use syn::visit::Visit;
use syn::{
    Attribute, Expr, GenericArgument, ImplItem, Item, Lit, Meta, PathArguments, Type, UseTree,
    Visibility,
};

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

#[derive(Clone, Copy)]
struct CfgProfile {
    test: bool,
    async_feature: bool,
    tokio: bool,
    test_hooks: bool,
    panic_unwind: bool,
}

impl From<SurfaceProfile> for CfgProfile {
    fn from(profile: SurfaceProfile) -> Self {
        match profile {
            SurfaceProfile::Core => Self {
                test: false,
                async_feature: false,
                tokio: false,
                test_hooks: false,
                panic_unwind: true,
            },
            SurfaceProfile::Production => Self {
                test: false,
                async_feature: true,
                tokio: true,
                test_hooks: false,
                panic_unwind: true,
            },
            SurfaceProfile::Hooks => Self {
                test: false,
                async_feature: true,
                tokio: true,
                test_hooks: true,
                panic_unwind: true,
            },
        }
    }
}

fn cfg_meta_enabled(meta: &Meta, profile: CfgProfile) -> bool {
    match meta {
        Meta::Path(path) if path.is_ident("test") => profile.test,
        Meta::Path(path) if path.is_ident("unix") => true,
        Meta::Path(_) => true,
        Meta::NameValue(value) if value.path.is_ident("feature") => {
            let Expr::Lit(literal) = &value.value else {
                panic!("cfg(feature) must use a string literal");
            };
            let Lit::Str(feature) = &literal.lit else {
                panic!("cfg(feature) must use a string literal");
            };
            match feature.value().as_str() {
                "async" => profile.async_feature,
                "tokio" => profile.tokio,
                "test-hooks" => profile.test_hooks,
                other => panic!("unhandled V2 feature predicate: {other}"),
            }
        }
        Meta::NameValue(value) if value.path.is_ident("panic") => {
            let Expr::Lit(literal) = &value.value else {
                panic!("cfg(panic) must use a string literal");
            };
            let Lit::Str(strategy) = &literal.lit else {
                panic!("cfg(panic) must use a string literal");
            };
            match strategy.value().as_str() {
                "unwind" => profile.panic_unwind,
                "abort" => !profile.panic_unwind,
                other => panic!("unhandled panic strategy: {other}"),
            }
        }
        Meta::NameValue(_) => true,
        Meta::List(list) => {
            let nested = syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated
                .parse2(list.tokens.clone())
                .unwrap_or_else(|error| panic!("invalid cfg predicate: {error}"));
            if list.path.is_ident("all") {
                nested.iter().all(|meta| cfg_meta_enabled(meta, profile))
            } else if list.path.is_ident("any") {
                nested.iter().any(|meta| cfg_meta_enabled(meta, profile))
            } else if list.path.is_ident("not") {
                assert_eq!(nested.len(), 1, "cfg(not) requires one predicate");
                !cfg_meta_enabled(&nested[0], profile)
            } else {
                true
            }
        }
    }
}

fn cfg_enabled(attributes: &[Attribute], profile: CfgProfile) -> bool {
    attributes
        .iter()
        .filter(|attribute| attribute.path().is_ident("cfg"))
        .all(|attribute| {
            let Meta::List(list) = &attribute.meta else {
                panic!("cfg attribute must be a predicate list");
            };
            let predicate = syn::parse2::<Meta>(list.tokens.clone())
                .unwrap_or_else(|error| panic!("invalid cfg attribute: {error}"));
            cfg_meta_enabled(&predicate, profile)
        })
}

fn item_attributes(item: &Item) -> &[Attribute] {
    match item {
        Item::Const(item) => &item.attrs,
        Item::Enum(item) => &item.attrs,
        Item::ExternCrate(item) => &item.attrs,
        Item::Fn(item) => &item.attrs,
        Item::ForeignMod(item) => &item.attrs,
        Item::Impl(item) => &item.attrs,
        Item::Macro(item) => &item.attrs,
        Item::Mod(item) => &item.attrs,
        Item::Static(item) => &item.attrs,
        Item::Struct(item) => &item.attrs,
        Item::Trait(item) => &item.attrs,
        Item::TraitAlias(item) => &item.attrs,
        Item::Type(item) => &item.attrs,
        Item::Union(item) => &item.attrs,
        Item::Use(item) => &item.attrs,
        Item::Verbatim(_) => &[],
        _ => &[],
    }
}

fn walk_enabled_items(items: &[Item], profile: CfgProfile, visit: &mut impl FnMut(&Item)) {
    for item in items {
        if !cfg_enabled(item_attributes(item), profile) {
            continue;
        }
        visit(item);
        if let Item::Mod(module) = item
            && let Some((_, nested)) = &module.content
        {
            walk_enabled_items(nested, profile, visit);
        }
    }
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

fn public_reexports(relative: &str, profile: CfgProfile) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    walk_enabled_items(&parse(relative).items, profile, &mut |item| {
        if let Item::Use(item) = item
            && is_public(&item.vis)
        {
            use_names(&item.tree, &mut names);
        }
    });
    names
}

fn public_modules(relative: &str, profile: CfgProfile) -> BTreeSet<String> {
    let mut modules = BTreeSet::new();
    walk_enabled_items(&parse(relative).items, profile, &mut |item| {
        if let Item::Mod(item) = item
            && is_public(&item.vis)
        {
            modules.insert(item.ident.to_string());
        }
    });
    modules
}

fn collect_public_methods(
    relative: &str,
    profile: CfgProfile,
    methods: &mut BTreeMap<String, BTreeSet<String>>,
) {
    walk_enabled_items(&parse(relative).items, profile, &mut |item| {
        let Item::Impl(item) = item else {
            return;
        };
        if item.trait_.is_some() {
            return;
        }
        let Type::Path(self_type) = item.self_ty.as_ref() else {
            return;
        };
        let Some(type_name) = self_type.path.segments.last() else {
            return;
        };
        let target = methods.entry(type_name.ident.to_string()).or_default();
        for member in &item.items {
            if let ImplItem::Fn(function) = member
                && cfg_enabled(&function.attrs, profile)
                && is_public(&function.vis)
            {
                target.insert(function.sig.ident.to_string());
            }
        }
    });
}

fn method_set(type_name: &str, files: &[&str], profile: CfgProfile) -> BTreeSet<String> {
    let mut methods = BTreeMap::new();
    for file in files {
        collect_public_methods(file, profile, &mut methods);
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
    profile: CfgProfile,
) -> (usize, usize) {
    let mut counts = (0, 0);
    walk_enabled_items(&parse(relative).items, profile, &mut |item| {
        let Item::Impl(item) = item else {
            return;
        };
        let Type::Path(self_type) = item.self_ty.as_ref() else {
            return;
        };
        let Some(type_name) = self_type.path.segments.last() else {
            return;
        };
        if !allowed_types.contains(&type_name.ident.to_string()) {
            return;
        }
        for member in &item.items {
            if let ImplItem::Fn(function) = member
                && cfg_enabled(&function.attrs, profile)
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
    });
    counts
}

fn declared_types(relative: &str, profile: CfgProfile) -> BTreeSet<String> {
    let mut declared = BTreeSet::new();
    walk_enabled_items(&parse(relative).items, profile, &mut |item| {
        let name = match item {
            Item::Enum(item) => Some(item.ident.to_string()),
            Item::Struct(item) => Some(item.ident.to_string()),
            Item::Type(item) => Some(item.ident.to_string()),
            Item::Trait(item) => Some(item.ident.to_string()),
            _ => None,
        };
        declared.extend(name);
    });
    declared
}

fn type_path(path: &syn::Path) -> String {
    path.segments
        .iter()
        .map(|segment| {
            let mut normalized = segment.ident.to_string();
            if let PathArguments::AngleBracketed(arguments) = &segment.arguments {
                let arguments = arguments
                    .args
                    .iter()
                    .filter_map(|argument| match argument {
                        GenericArgument::Type(ty) => Some(type_name(ty)),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                normalized.push('<');
                normalized.push_str(&arguments.join(","));
                normalized.push('>');
            }
            normalized
        })
        .collect::<Vec<_>>()
        .join("::")
}

fn type_name(ty: &Type) -> String {
    match ty {
        Type::Path(path) if path.qself.is_none() => type_path(&path.path),
        Type::Reference(reference) => {
            let mut normalized = "&".to_owned();
            if reference.mutability.is_some() {
                normalized.push_str("mut ");
            }
            normalized.push_str(&type_name(&reference.elem));
            normalized
        }
        _ => panic!("unsupported trait-selector type"),
    }
}

fn trait_impls(relative: &str, profile: CfgProfile) -> BTreeSet<(String, String)> {
    let mut implementations = BTreeSet::new();
    walk_enabled_items(&parse(relative).items, profile, &mut |item| {
        let Item::Impl(item) = item else {
            return;
        };
        let Some((_, trait_path, _)) = &item.trait_ else {
            return;
        };
        implementations.insert((type_name(&item.self_ty), type_path(trait_path)));
    });
    implementations
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
    assert!(!manifest::RUSTDOC_MANIFEST.is_empty());
}

#[test]
fn api_fixture_manifest_is_complete_unique_and_has_real_positive_controls() {
    let fixture_root = workspace().join("rdma-io-tests/api-fixtures/v2-surface");
    let mut positive_profiles = BTreeSet::new();
    let mut cases = BTreeSet::new();
    for (line_number, line) in manifest::API_FIXTURE_MANIFEST.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        let columns = line.split('|').collect::<Vec<_>>();
        match columns.as_slice() {
            ["positive", fixture, binary] => {
                assert!(
                    positive_profiles.insert(*fixture),
                    "duplicate positive fixture profile {fixture}"
                );
                assert!(cases.insert((*fixture, *binary)));
                let source = fixture_root
                    .join(fixture)
                    .join("src/bin")
                    .join(format!("{binary}.rs"));
                let source_text = fs::read_to_string(&source).unwrap_or_else(|error| {
                    panic!(
                        "failed to read positive fixture {}: {error}",
                        source.display()
                    )
                });
                assert!(
                    source_text
                        .lines()
                        .filter(|line| !line.trim().is_empty())
                        .count()
                        >= 10,
                    "{} must exercise nontrivial retained behavior",
                    source.display()
                );
                let marker = match *fixture {
                    "production" => "build_with_cm",
                    "hooks" => "require_context",
                    "no-hooks" => "operation_traits",
                    other => panic!("unexpected positive fixture profile {other}"),
                };
                assert!(
                    source_text.contains(marker),
                    "{} is missing positive control marker {marker}",
                    source.display()
                );
            }
            ["negative", fixture, binary, diagnostic, symbol, message] => {
                assert!(
                    diagnostic.starts_with('E') && diagnostic.len() == 5,
                    "invalid diagnostic at fixture-manifest line {}",
                    line_number + 1
                );
                assert!(!symbol.is_empty() && !message.is_empty());
                assert!(
                    cases.insert((*fixture, *binary)),
                    "duplicate fixture case {fixture}/{binary}"
                );
                let source = fixture_root
                    .join(fixture)
                    .join("src/bin")
                    .join(format!("{binary}.rs"));
                let source_text = fs::read_to_string(&source).unwrap_or_else(|error| {
                    panic!(
                        "failed to read negative fixture {}: {error}",
                        source.display()
                    )
                });
                assert!(
                    source_text.contains(symbol),
                    "{} does not name removed symbol {symbol}",
                    source.display()
                );
            }
            _ => panic!(
                "invalid API fixture manifest line {}: {line}",
                line_number + 1
            ),
        }
    }
    assert_eq!(
        positive_profiles,
        ["hooks", "no-hooks", "production"]
            .into_iter()
            .collect::<BTreeSet<_>>()
    );
    assert_eq!(
        cases.len(),
        manifest::API_FIXTURE_MANIFEST.lines().count(),
        "every fixture manifest row must select one unique binary"
    );
}

#[test]
fn root_exports_and_module_paths_are_exact() {
    let production = CfgProfile::from(SurfaceProfile::Production);
    let hooks = CfgProfile::from(SurfaceProfile::Hooks);
    assert_eq!(
        public_reexports("v2/mod.rs", production),
        expected(PRODUCTION_EXPORTS)
    );
    assert!(public_modules("v2/mod.rs", production).is_empty());
    assert_eq!(
        public_modules("v2/mod.rs", hooks),
        expected(&["test_support"])
    );
    assert!(public_modules("lib.rs", production).contains("v2"));
    assert!(!public_modules("lib.rs", production).contains("test_support"));
    assert!(!v2_file("test_support/engine_driver.rs").exists());
}

#[test]
fn retained_and_removed_inherent_method_sets_are_exact() {
    for expectation in METHOD_SETS {
        assert_eq!(
            method_set(
                expectation.type_name,
                expectation.modules,
                expectation.profile.into(),
            ),
            expected(expectation.methods),
            "{:?} method set for {}",
            expectation.profile,
            expectation.type_name
        );
    }

    let protocol = parse("v2/protocol.rs");
    let mut leaked = 0usize;
    walk_enabled_items(
        &protocol.items,
        SurfaceProfile::Production.into(),
        &mut |item| {
            let private = match item {
                Item::Const(item) => !is_public(&item.vis),
                Item::Enum(item) => !is_public(&item.vis),
                Item::Fn(item) => !is_public(&item.vis),
                Item::Struct(item) => !is_public(&item.vis),
                Item::Type(item) => !is_public(&item.vis),
                _ => true,
            };
            if !private {
                leaked += 1;
            }
        },
    );
    assert_eq!(leaked, 0, "protocol leaked {leaked} public item(s)");
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
        let counts = public_signature_type_counts(
            file,
            &production_types,
            SurfaceProfile::Production.into(),
        );
        production.0 += counts.0;
        production.1 += counts.1;
    }
    assert_eq!(production, (1, 0));

    let hook_exports = public_reexports("v2/test_support.rs", SurfaceProfile::Hooks.into());
    assert_eq!(hook_exports, expected(HOOK_EXPORTS));
    let hook_types = expected(&[
        "TestEngineResources",
        "TestContextIdentity",
        "TestProviderLimits",
    ]);
    assert_eq!(
        public_signature_type_counts(
            "v2/engine/driver.rs",
            &hook_types,
            SurfaceProfile::Hooks.into(),
        ),
        (2, 1)
    );
}

#[test]
fn trait_removals_and_architecture_consolidations_are_exact() {
    for selector in TRAIT_IMPL_REMOVALS {
        let implementations = trait_impls(selector.module, SurfaceProfile::Production.into());
        assert!(
            !implementations.contains(&(
                selector.self_type.to_owned(),
                selector.trait_path.to_owned()
            )),
            "removed trait impl remains: {} for {} in {}",
            selector.trait_path,
            selector.self_type,
            selector.module
        );
    }

    let mut declared = BTreeSet::new();
    for file in FINAL_MODULES
        .iter()
        .copied()
        .filter(|file| file.starts_with("v2/"))
    {
        declared.extend(declared_types(file, SurfaceProfile::Hooks.into()));
    }
    for removed in REMOVED_ARCHITECTURE_TYPES {
        assert!(
            !declared.contains(*removed),
            "removed type remains: {removed}"
        );
    }
    for retained in RETAINED_ARCHITECTURE_TYPES {
        assert!(
            declared.contains(*retained),
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
    for command in [
        "check -p rdma-io --no-default-features",
        "check -p rdma-io --no-default-features --features tokio",
    ] {
        assert!(
            justfile.contains(&format!("cargo {command}")),
            "just validate-v2-engine is missing cargo {command}"
        );
        assert!(
            provider.contains(&format!("\"$CARGO\" {command}")),
            "provider full validation is missing $CARGO {command}"
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
