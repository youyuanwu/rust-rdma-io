//! AST-level regression ensuring v2 production code starts no hidden work.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use proc_macro2::{TokenStream, TokenTree};
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{
    Attribute, Expr, ExprBlock, ExprCall, ExprMethodCall, ExprPath, ForeignItem, ImplItem, Item,
    Local, Macro, Meta, Pat, Token, TraitItem, Type, UseTree,
};

const SPAWN_NAMES: &[&str] = &[
    "spawn",
    "spawn_blocking",
    "spawn_fifo",
    "spawn_local",
    "spawn_scoped",
    "spawn_unchecked",
    "scope",
    "scope_fifo",
    "pthread_create",
    "thrd_create",
];
const EXECUTOR_TYPES: &[&str] = &[
    "Builder",
    "Executor",
    "LocalExecutor",
    "LocalExecutorBuilder",
    "LocalSet",
    "Runtime",
    "ThreadPool",
    "ThreadPoolBuilder",
];
const CONSTRUCTOR_NAMES: &[&str] = &["build", "new", "new_current_thread", "new_multi_thread"];

fn collect_rs_files(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let metadata = fs::symlink_metadata(dir)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("scan root must be a real directory: {}", dir.display()),
        ));
    }

    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("source scan refuses symlink: {}", path.display()),
            ));
        }
        if metadata.is_dir() {
            files.extend(collect_rs_files(&path)?);
        } else if metadata.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

struct ForbiddenDependencyVisitor<'a> {
    forbidden: &'a HashSet<&'a str>,
    violations: Vec<String>,
}

impl<'ast> Visit<'ast> for ForbiddenDependencyVisitor<'_> {
    fn visit_item(&mut self, item: &'ast Item) {
        if !is_test_only(item_attrs(item)) {
            visit::visit_item(self, item);
        }
    }

    fn visit_impl_item(&mut self, item: &'ast ImplItem) {
        if !is_test_only(impl_item_attrs(item)) {
            visit::visit_impl_item(self, item);
        }
    }

    fn visit_trait_item(&mut self, item: &'ast TraitItem) {
        if !is_test_only(trait_item_attrs(item)) {
            visit::visit_trait_item(self, item);
        }
    }

    fn visit_foreign_item(&mut self, item: &'ast ForeignItem) {
        if !is_test_only(foreign_item_attrs(item)) {
            visit::visit_foreign_item(self, item);
        }
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        for segment in &path.segments {
            let identifier = segment.ident.to_string();
            if self.forbidden.contains(identifier.as_str()) {
                self.violations
                    .push(format!("{identifier}:{}", path.span().start().line));
            }
        }
        visit::visit_path(self, path);
    }

    fn visit_use_tree(&mut self, tree: &'ast UseTree) {
        let identifier = match tree {
            UseTree::Path(path) => Some(&path.ident),
            UseTree::Name(name) => Some(&name.ident),
            UseTree::Rename(rename) => Some(&rename.ident),
            UseTree::Glob(_) | UseTree::Group(_) => None,
        };
        if let Some(identifier) = identifier {
            let identifier = identifier.to_string();
            if self.forbidden.contains(identifier.as_str()) {
                self.violations
                    .push(format!("{identifier}:{}", tree.span().start().line));
            }
        }
        visit::visit_use_tree(self, tree);
    }
}

fn find_forbidden_production_dependencies(
    source: &str,
    forbidden: &[&str],
) -> Result<Vec<String>, syn::Error> {
    let syntax = syn::parse_file(source)?;
    let forbidden = forbidden.iter().copied().collect::<HashSet<_>>();
    let mut visitor = ForbiddenDependencyVisitor {
        forbidden: &forbidden,
        violations: Vec::new(),
    };
    visitor.visit_file(&syntax);
    visitor.violations.sort();
    visitor.violations.dedup();
    Ok(visitor.violations)
}

struct LifecycleCallVisitor<'a> {
    methods: &'a HashSet<&'a str>,
    violations: Vec<String>,
    functions: Vec<String>,
}

impl<'ast> Visit<'ast> for LifecycleCallVisitor<'_> {
    fn visit_item(&mut self, item: &'ast Item) {
        if !is_test_only(item_attrs(item)) {
            visit::visit_item(self, item);
        }
    }

    fn visit_impl_item(&mut self, item: &'ast ImplItem) {
        if !is_test_only(impl_item_attrs(item)) {
            visit::visit_impl_item(self, item);
        }
    }

    fn visit_expr_method_call(&mut self, call: &'ast ExprMethodCall) {
        let method = call.method.to_string();
        if self.methods.contains(method.as_str()) {
            self.violations.push(format!(
                "{method}:{}:{}",
                self.functions.last().map_or("<unknown>", String::as_str),
                call.span().start().line
            ));
        }
        visit::visit_expr_method_call(self, call);
    }

    fn visit_expr_call(&mut self, call: &'ast ExprCall) {
        if let Expr::Path(path) = call.func.as_ref()
            && let Some(segment) = path.path.segments.last()
        {
            let method = segment.ident.to_string();
            if self.methods.contains(method.as_str()) {
                self.violations.push(format!(
                    "{method}:{}:{}",
                    self.functions.last().map_or("<unknown>", String::as_str),
                    call.span().start().line
                ));
            }
        }
        visit::visit_expr_call(self, call);
    }

    fn visit_item_fn(&mut self, function: &'ast syn::ItemFn) {
        if is_test_only(&function.attrs) {
            return;
        }
        self.functions.push(function.sig.ident.to_string());
        visit::visit_item_fn(self, function);
        self.functions.pop();
    }

    fn visit_impl_item_fn(&mut self, function: &'ast syn::ImplItemFn) {
        if is_test_only(&function.attrs) {
            return;
        }
        self.functions.push(function.sig.ident.to_string());
        visit::visit_impl_item_fn(self, function);
        self.functions.pop();
    }

    fn visit_trait_item_fn(&mut self, function: &'ast syn::TraitItemFn) {
        if is_test_only(&function.attrs) {
            return;
        }
        self.functions.push(function.sig.ident.to_string());
        visit::visit_trait_item_fn(self, function);
        self.functions.pop();
    }
}

fn find_production_lifecycle_calls(
    source: &str,
    methods: &[&str],
) -> Result<Vec<String>, syn::Error> {
    let syntax = syn::parse_file(source)?;
    let methods = methods.iter().copied().collect::<HashSet<_>>();
    let mut visitor = LifecycleCallVisitor {
        methods: &methods,
        violations: Vec::new(),
        functions: Vec::new(),
    };
    visitor.visit_file(&syntax);
    Ok(visitor.violations)
}

fn find_strong_owner_fields(
    source: &str,
    struct_name: &str,
    forbidden_owners: &[&str],
) -> Result<Vec<String>, syn::Error> {
    let syntax = syn::parse_file(source)?;
    let forbidden = forbidden_owners.iter().copied().collect::<HashSet<_>>();
    let item = syntax
        .items
        .iter()
        .find_map(|item| match item {
            Item::Struct(item) if item.ident == struct_name => Some(item),
            _ => None,
        })
        .unwrap_or_else(|| panic!("locate struct {struct_name}"));
    let mut violations = Vec::new();
    for field in &item.fields {
        if is_test_only(&field.attrs) {
            continue;
        }
        let mut visitor = ForbiddenDependencyVisitor {
            forbidden: &forbidden,
            violations: Vec::new(),
        };
        visitor.visit_type(&field.ty);
        if visitor.violations.is_empty() {
            continue;
        }
        let outer = match &field.ty {
            Type::Path(path) => path
                .path
                .segments
                .last()
                .map(|segment| segment.ident.to_string()),
            _ => None,
        };
        if outer.as_deref() != Some("Weak") {
            violations.push(format!(
                "{}: {}",
                field
                    .ident
                    .as_ref()
                    .map_or_else(|| "<unnamed>".into(), ToString::to_string),
                visitor.violations.join(", ")
            ));
        }
    }
    Ok(violations)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CfgValue {
    False,
    Unknown,
    True,
}

fn parse_nested_meta(list: &syn::MetaList) -> Option<Vec<Meta>> {
    list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
        .ok()
        .map(|items| items.into_iter().collect())
}

fn cfg_value(meta: &Meta) -> CfgValue {
    match meta {
        Meta::Path(path) if path.is_ident("test") => CfgValue::False,
        Meta::Path(_) | Meta::NameValue(_) => CfgValue::Unknown,
        Meta::List(list) if list.path.is_ident("all") => {
            let Some(items) = parse_nested_meta(list) else {
                return CfgValue::Unknown;
            };
            if items.iter().any(|item| cfg_value(item) == CfgValue::False) {
                CfgValue::False
            } else if items.iter().all(|item| cfg_value(item) == CfgValue::True) {
                CfgValue::True
            } else {
                CfgValue::Unknown
            }
        }
        Meta::List(list) if list.path.is_ident("any") => {
            let Some(items) = parse_nested_meta(list) else {
                return CfgValue::Unknown;
            };
            if items.iter().any(|item| cfg_value(item) == CfgValue::True) {
                CfgValue::True
            } else if items.iter().all(|item| cfg_value(item) == CfgValue::False) {
                CfgValue::False
            } else {
                CfgValue::Unknown
            }
        }
        Meta::List(list) if list.path.is_ident("not") => {
            let Some(items) = parse_nested_meta(list) else {
                return CfgValue::Unknown;
            };
            match items.as_slice() {
                [item] => match cfg_value(item) {
                    CfgValue::False => CfgValue::True,
                    CfgValue::True => CfgValue::False,
                    CfgValue::Unknown => CfgValue::Unknown,
                },
                _ => CfgValue::Unknown,
            }
        }
        Meta::List(_) => CfgValue::Unknown,
    }
}

fn is_test_only(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        if attr
            .path()
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "test")
            && !attr.path().is_ident("cfg")
        {
            return true;
        }
        match &attr.meta {
            Meta::List(list) if list.path.is_ident("cfg") => parse_nested_meta(list).is_some_and(
                |items| matches!(items.as_slice(), [item] if cfg_value(item) == CfgValue::False),
            ),
            _ => false,
        }
    })
}

fn item_attrs(item: &Item) -> &[Attribute] {
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
        _ => &[],
    }
}

fn impl_item_attrs(item: &ImplItem) -> &[Attribute] {
    match item {
        ImplItem::Const(item) => &item.attrs,
        ImplItem::Fn(item) => &item.attrs,
        ImplItem::Type(item) => &item.attrs,
        ImplItem::Macro(item) => &item.attrs,
        _ => &[],
    }
}

fn trait_item_attrs(item: &TraitItem) -> &[Attribute] {
    match item {
        TraitItem::Const(item) => &item.attrs,
        TraitItem::Fn(item) => &item.attrs,
        TraitItem::Type(item) => &item.attrs,
        TraitItem::Macro(item) => &item.attrs,
        _ => &[],
    }
}

fn foreign_item_attrs(item: &ForeignItem) -> &[Attribute] {
    match item {
        ForeignItem::Fn(item) => &item.attrs,
        ForeignItem::Static(item) => &item.attrs,
        ForeignItem::Type(item) => &item.attrs,
        ForeignItem::Macro(item) => &item.attrs,
        _ => &[],
    }
}

fn is_spawn_name(name: &str) -> bool {
    SPAWN_NAMES.contains(&name)
}

fn is_executor_type(name: &str) -> bool {
    EXECUTOR_TYPES.contains(&name)
}

#[derive(Default)]
struct AliasCollector {
    call_aliases: HashSet<String>,
    executor_aliases: HashSet<String>,
    namespace_aliases: HashSet<String>,
}

impl AliasCollector {
    fn collect_use(&mut self, tree: &UseTree, prefix: &mut Vec<String>) {
        match tree {
            UseTree::Path(path) => {
                prefix.push(path.ident.to_string());
                self.collect_use(&path.tree, prefix);
                prefix.pop();
            }
            UseTree::Name(name) => {
                let original = name.ident.to_string();
                self.register_alias(prefix, &original, &original);
            }
            UseTree::Rename(rename) => self.register_alias(
                prefix,
                &rename.ident.to_string(),
                &rename.rename.to_string(),
            ),
            UseTree::Group(group) => {
                for item in &group.items {
                    self.collect_use(item, prefix);
                }
            }
            UseTree::Glob(_) => {}
        }
    }

    fn register_alias(&mut self, prefix: &[String], original: &str, alias: &str) {
        if is_spawn_name(original) {
            self.call_aliases.insert(alias.to_string());
        }
        if is_executor_type(original) && is_forbidden_namespace_base(prefix) {
            self.executor_aliases.insert(alias.to_owned());
        }
        if is_forbidden_namespace_name(original) || is_forbidden_namespace_base(prefix) {
            self.namespace_aliases.insert(alias.to_owned());
        }
    }

    fn collect_local_alias(&mut self, local: &Local) {
        let Some(alias) = pat_ident(&local.pat) else {
            return;
        };
        let Some(init) = &local.init else {
            return;
        };
        let Expr::Path(path) = init.expr.as_ref() else {
            return;
        };
        let segments = path_segments(&path.path);
        let Some(last) = segments.last() else {
            return;
        };
        if is_spawn_name(last) || (segments.len() == 1 && self.call_aliases.contains(last)) {
            self.call_aliases.insert(alias.to_string());
        }
    }

    fn collect_type_alias(&mut self, item: &syn::ItemType) {
        let Type::Path(target) = item.ty.as_ref() else {
            return;
        };
        let segments = path_segments(&target.path);
        if segments.iter().any(|segment| is_executor_type(segment))
            && self.is_forbidden_namespace(&segments)
        {
            self.executor_aliases.insert(item.ident.to_string());
        }
    }

    fn is_forbidden_namespace(&self, segments: &[String]) -> bool {
        is_forbidden_namespace_base(segments)
            || segments
                .iter()
                .any(|segment| self.namespace_aliases.contains(segment))
    }
}

impl<'ast> Visit<'ast> for AliasCollector {
    fn visit_item(&mut self, item: &'ast Item) {
        if !is_test_only(item_attrs(item)) {
            visit::visit_item(self, item);
        }
    }

    fn visit_impl_item(&mut self, item: &'ast ImplItem) {
        if !is_test_only(impl_item_attrs(item)) {
            visit::visit_impl_item(self, item);
        }
    }

    fn visit_trait_item(&mut self, item: &'ast TraitItem) {
        if !is_test_only(trait_item_attrs(item)) {
            visit::visit_trait_item(self, item);
        }
    }

    fn visit_foreign_item(&mut self, item: &'ast ForeignItem) {
        if !is_test_only(foreign_item_attrs(item)) {
            visit::visit_foreign_item(self, item);
        }
    }

    fn visit_expr_block(&mut self, expression: &'ast ExprBlock) {
        if !is_test_only(&expression.attrs) {
            visit::visit_expr_block(self, expression);
        }
    }

    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        self.collect_use(&item.tree, &mut Vec::new());
    }

    fn visit_item_type(&mut self, item: &'ast syn::ItemType) {
        self.collect_type_alias(item);
        visit::visit_item_type(self, item);
    }

    fn visit_local(&mut self, local: &'ast Local) {
        if !is_test_only(&local.attrs) {
            self.collect_local_alias(local);
            visit::visit_local(self, local);
        }
    }
}

fn pat_ident(pattern: &Pat) -> Option<&syn::Ident> {
    match pattern {
        Pat::Ident(ident) => Some(&ident.ident),
        Pat::Type(typed) => pat_ident(&typed.pat),
        _ => None,
    }
}

fn path_segments(path: &syn::Path) -> Vec<String> {
    path.segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect()
}

fn is_forbidden_namespace_name(segment: &str) -> bool {
    matches!(
        segment,
        "async_executor"
            | "async_std"
            | "futures"
            | "glommio"
            | "rayon"
            | "runtime"
            | "smol"
            | "std"
            | "thread"
            | "tokio"
    )
}

fn is_forbidden_namespace_base(segments: &[String]) -> bool {
    segments
        .iter()
        .any(|segment| is_forbidden_namespace_name(segment))
}

struct SpawnDetector<'a> {
    aliases: &'a AliasCollector,
    source_lines: Vec<&'a str>,
    violations: BTreeMap<(usize, String), String>,
}

impl<'a> SpawnDetector<'a> {
    fn new(aliases: &'a AliasCollector, source: &'a str) -> Self {
        Self {
            aliases,
            source_lines: source.lines().collect(),
            violations: BTreeMap::new(),
        }
    }

    fn forbidden_path(&self, path: &syn::Path) -> Option<String> {
        let segments = path_segments(path);
        let last = segments.last()?;
        if is_spawn_name(last) || (segments.len() == 1 && self.aliases.call_aliases.contains(last))
        {
            return Some(last.clone());
        }
        let first = segments.first()?;
        if self.aliases.executor_aliases.contains(first)
            && CONSTRUCTOR_NAMES.contains(&last.as_str())
        {
            return Some(format!("{first}::{last}"));
        }
        if CONSTRUCTOR_NAMES.contains(&last.as_str())
            && segments.iter().any(|segment| is_executor_type(segment))
            && self.aliases.is_forbidden_namespace(&segments)
        {
            return Some(segments.join("::"));
        }
        None
    }

    fn record(&mut self, span: proc_macro2::Span, label: String) {
        let line = span.start().line;
        let snippet = self
            .source_lines
            .get(line.saturating_sub(1))
            .map(|line| line.trim())
            .unwrap_or("");
        self.violations
            .entry((line, label.clone()))
            .or_insert_with(|| format!("line {line}: [{label}] {snippet}"));
    }

    fn scan_macro_tokens(&mut self, tokens: TokenStream) {
        for token in tokens {
            match token {
                TokenTree::Group(group) => self.scan_macro_tokens(group.stream()),
                TokenTree::Ident(ident) => {
                    let name = ident.to_string();
                    if is_spawn_name(&name) || self.aliases.call_aliases.contains(&name) {
                        self.record(ident.span(), format!("macro-token::{name}"));
                    }
                }
                TokenTree::Punct(_) | TokenTree::Literal(_) => {}
            }
        }
    }
}

impl<'ast> Visit<'ast> for SpawnDetector<'_> {
    fn visit_item(&mut self, item: &'ast Item) {
        if !is_test_only(item_attrs(item)) {
            visit::visit_item(self, item);
        }
    }

    fn visit_impl_item(&mut self, item: &'ast ImplItem) {
        if !is_test_only(impl_item_attrs(item)) {
            visit::visit_impl_item(self, item);
        }
    }

    fn visit_trait_item(&mut self, item: &'ast TraitItem) {
        if !is_test_only(trait_item_attrs(item)) {
            visit::visit_trait_item(self, item);
        }
    }

    fn visit_foreign_item(&mut self, item: &'ast ForeignItem) {
        if !is_test_only(foreign_item_attrs(item)) {
            visit::visit_foreign_item(self, item);
        }
    }

    fn visit_expr_block(&mut self, expression: &'ast ExprBlock) {
        if !is_test_only(&expression.attrs) {
            visit::visit_expr_block(self, expression);
        }
    }

    fn visit_expr_path(&mut self, expression: &'ast ExprPath) {
        if let Some(label) = self.forbidden_path(&expression.path) {
            self.record(expression.span(), label);
        }
        visit::visit_expr_path(self, expression);
    }

    fn visit_expr_method_call(&mut self, expression: &'ast ExprMethodCall) {
        let method = expression.method.to_string();
        if is_spawn_name(&method) {
            self.record(expression.method.span(), method);
        }
        visit::visit_expr_method_call(self, expression);
    }

    fn visit_macro(&mut self, mac: &'ast Macro) {
        if let Some(label) = self.forbidden_path(&mac.path) {
            self.record(mac.path.span(), format!("macro::{label}"));
        }
        self.scan_macro_tokens(mac.tokens.clone());
        visit::visit_macro(self, mac);
    }
}

fn find_spawn_violations(source: &str) -> Vec<String> {
    let syntax = match syn::parse_file(source) {
        Ok(syntax) => syntax,
        Err(error) => return vec![format!("source parse failed: {error}")],
    };
    let mut aliases = AliasCollector::default();
    aliases.visit_file(&syntax);
    let mut detector = SpawnDetector::new(&aliases, source);
    detector.visit_file(&syntax);
    detector.violations.into_values().collect()
}

#[test]
fn detector_catches_aliases_qualified_calls_and_runtime_builders() {
    let source = r#"
use tokio::spawn as launch;
use tokio::runtime::Builder as RuntimeBuilder;
use std::thread as os_thread;
type PoolMaker = rayon::ThreadPoolBuilder;

fn production() {
    launch(async {});
    let local_launch = tokio::task::spawn;
    let chained_launch = local_launch;
    chained_launch(async {});
    tokio::task::spawn_local(async {});
    <tokio::runtime::Handle>::spawn(&tokio::runtime::Handle::current(), async {});
    std::thread::Builder::new().spawn(|| {});
    os_thread::Builder::new().spawn(|| {});
    RuntimeBuilder::new_multi_thread().build().unwrap();
    PoolMaker::new().build().unwrap();
    async_executor::Executor::new();
    std::thread::scope(|_| {});
    wrapper! {
        tokio::task::spawn(async {});
        launch(async {});
    }
}
"#;
    let violations = find_spawn_violations(source);
    for expected in [
        "launch",
        "chained_launch",
        "spawn_local",
        "spawn",
        "RuntimeBuilder",
        "PoolMaker",
        "Executor",
        "scope",
        "macro-token::spawn",
        "macro-token::launch",
    ] {
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains(expected)),
            "missing {expected:?} in {violations:#?}"
        );
    }
}

#[test]
fn recursive_source_collection_fails_closed() {
    let missing = Path::new(env!("CARGO_MANIFEST_DIR")).join("missing-v2-source-root");
    assert!(collect_rs_files(&missing).is_err());
}

#[test]
fn detector_skips_only_code_that_is_impossible_outside_tests() {
    let source = r#"
#[cfg(test)]
mod tests {
    fn allowed() {
        std::thread::scope(|scope| scope.spawn(|| {}));
    }
}

#[test]
fn also_allowed() {
    tokio::spawn(async {});
}

#[cfg(not(test))]
fn production() {
    tokio::spawn(async {});
}

#[cfg(any(test, feature = "test-hooks"))]
fn feature_build() {
    tokio::task::spawn_local(async {});
}

fn expression_block() {
    #[cfg(test)]
    {
        std::thread::scope(|scope| scope.spawn(|| {}));
    }
}
"#;
    let violations = find_spawn_violations(source);
    assert_eq!(violations.len(), 2, "{violations:#?}");
    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("spawn]"))
    );
    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("spawn_local"))
    );
}

#[test]
fn dependency_detector_ignores_test_only_items_but_fails_closed_for_production() {
    let source = r#"
        use crate::EngineShared;

        #[cfg(test)]
        use crate::ConnectionState;

        fn production(value: crate::WorkRequestPoster) {
            let _ = value;
        }
    "#;
    let violations = find_forbidden_production_dependencies(
        source,
        &["EngineShared", "ConnectionState", "WorkRequestPoster"],
    )
    .unwrap();
    assert_eq!(violations.len(), 2, "{violations:#?}");
    assert!(
        violations
            .iter()
            .any(|violation| violation.starts_with("EngineShared:"))
    );
    assert!(
        violations
            .iter()
            .any(|violation| violation.starts_with("WorkRequestPoster:"))
    );
    assert!(
        !violations
            .iter()
            .any(|violation| violation.starts_with("ConnectionState:"))
    );
}

#[test]
fn lifecycle_detector_rejects_method_and_ufcs_calls() {
    let source = r#"
        fn production(poster: &Poster) {
            poster.to_error();
            WorkRequestPoster::destroy_qp(poster);
            <Poster as WorkRequestPoster>::destroy_connection(poster, true);
        }

        #[cfg(test)]
        fn test_only(poster: &Poster) {
            WorkRequestPoster::destroy_qp(poster);
        }
    "#;
    let mut violations =
        find_production_lifecycle_calls(source, &["to_error", "destroy_qp", "destroy_connection"])
            .unwrap();
    violations.sort();
    assert_eq!(violations.len(), 3, "{violations:#?}");
    assert!(violations.iter().any(|item| item.starts_with("to_error:")));
    assert!(
        violations
            .iter()
            .any(|item| item.starts_with("destroy_qp:"))
    );
    assert!(
        violations
            .iter()
            .any(|item| item.starts_with("destroy_connection:"))
    );
}

#[test]
fn test_no_hidden_spawn_in_v2() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace_root = Path::new(manifest_dir).parent().expect("workspace root");
    let v2_dir = workspace_root.join("rdma-io").join("src").join("v2");
    assert!(
        v2_dir.exists(),
        "v2 directory not found at {}",
        v2_dir.display()
    );

    let files = collect_rs_files(&v2_dir).expect("recursively enumerate every v2 Rust source");
    assert!(
        !files.is_empty(),
        "no .rs files found under {}",
        v2_dir.display()
    );
    for required in [
        v2_dir.join("mod.rs"),
        v2_dir.join("message_transport.rs"),
        v2_dir.join("engine").join("mod.rs"),
        v2_dir.join("engine").join("driver.rs"),
        v2_dir.join("engine").join("session").join("mod.rs"),
        v2_dir.join("engine").join("session").join("connection.rs"),
        v2_dir.join("engine").join("session").join("registry.rs"),
        v2_dir.join("engine").join("io_core").join("mod.rs"),
        v2_dir.join("engine").join("io_core").join("operation.rs"),
        v2_dir.join("engine").join("io.rs"),
    ] {
        assert!(
            files.binary_search(&required).is_ok(),
            "expected production source missing from scan scope: {}",
            required.display()
        );
    }

    let mut violations = Vec::new();
    for path in &files {
        let content = fs::read_to_string(path).expect("read file");
        violations.extend(
            find_spawn_violations(&content)
                .into_iter()
                .map(|violation| format!("{}:{violation}", path.display())),
        );
    }

    assert!(
        violations.is_empty(),
        "found hidden work creation in v2 production code:\n{}",
        violations.join("\n")
    );
}

#[test]
fn provider_validation_propagates_the_cargo_job_limit() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace_root = Path::new(manifest_dir).parent().expect("workspace root");
    let script_path = workspace_root
        .join("scripts")
        .join("validate-v2-engine-providers.sh");
    let script = fs::read_to_string(&script_path).expect("read provider validation script");
    let mut logical_commands = Vec::new();
    let mut current = String::new();
    for line in script.lines() {
        let trimmed = line.trim();
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(trimmed.trim_end_matches('\\').trim_end());
        if !trimmed.ends_with('\\') {
            logical_commands.push(std::mem::take(&mut current));
        }
    }
    assert!(
        current.is_empty(),
        "provider script ends in a continued command"
    );
    let cargo_commands = logical_commands
        .iter()
        .filter(|command| {
            command.contains("\"$CARGO\" test")
                || command.contains("\"$CARGO\" build")
                || command.contains("\"$CARGO\" check")
        })
        .collect::<Vec<_>>();

    assert!(
        !cargo_commands.is_empty(),
        "provider script contains no Cargo commands"
    );
    for command in cargo_commands {
        assert!(
            command.contains("CARGO_BUILD_JOBS=\"$CARGO_BUILD_JOBS\""),
            "{} Cargo command omits CARGO_BUILD_JOBS propagation: {command}",
            script_path.display()
        );
    }
    assert!(
        script.contains("CARGO_BUILD_JOBS=\"${CARGO_BUILD_JOBS:-2}\""),
        "{} must default provider validation to two Cargo jobs",
        script_path.display()
    );
}

#[test]
fn test_v2_io_boundary_dependency_direction_and_visibility() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace_root = Path::new(manifest_dir).parent().expect("workspace root");
    let v2_dir = workspace_root.join("rdma-io").join("src").join("v2");
    let message_path = v2_dir.join("message_transport.rs");
    let io_path = v2_dir.join("engine").join("io.rs");
    let io_core_mod_path = v2_dir.join("engine").join("io_core").join("mod.rs");
    let io_core_operation_path = v2_dir.join("engine").join("io_core").join("operation.rs");
    let engine_mod_path = v2_dir.join("engine").join("mod.rs");
    let connection_path = v2_dir.join("engine").join("session").join("connection.rs");
    let listener_path = v2_dir.join("engine").join("listener.rs");
    let driver_path = v2_dir.join("engine").join("driver.rs");
    let drain_path = v2_dir.join("engine").join("drain.rs");
    let session_path = v2_dir.join("engine").join("session").join("mod.rs");
    let v2_mod_path = v2_dir.join("mod.rs");

    let message = fs::read_to_string(&message_path).expect("read message transport source");
    for forbidden in [
        "EngineShared",
        "ConnectionState",
        "ConnectionTerminalSink",
        "DetachedCallbackAfterUnlock",
        "DetachedOperationCompletion",
        "ConnectionRegistry",
        "OperationRegistry",
        "PagedRegistry",
        "ConnectionToken",
        "OperationToken",
        "from_state",
        "attach_terminal_sink",
        "post_detached_",
    ] {
        assert!(
            !message.contains(forbidden),
            "{} must not depend on prohibited engine internal `{forbidden}`",
            message_path.display()
        );
    }

    let forbidden_core_dependencies = [
        "EngineShared",
        "ConnectionState",
        "WorkRequestPoster",
        "cm",
        "listener",
        "CmState",
        "ListenerState",
        "MessageTransportDriver",
        "message_transport",
        "SessionManager",
        "SessionConnection",
        "SessionListener",
        "SessionLifecycleAuthority",
        "QpDestructionProof",
    ];
    for path in [&io_core_mod_path, &io_core_operation_path] {
        let source = fs::read_to_string(path).expect("read I/O core source");
        let violations =
            find_forbidden_production_dependencies(&source, &forbidden_core_dependencies)
                .expect("parse I/O core source");
        assert!(
            violations.is_empty(),
            "{} has forbidden production dependencies: {}",
            path.display(),
            violations.join(", ")
        );
    }

    let engine_mod = fs::read_to_string(&engine_mod_path).expect("read engine module source");
    let engine_shared = engine_mod
        .split("struct EngineShared {")
        .nth(1)
        .and_then(|tail| tail.split("\n}").next())
        .expect("locate EngineShared fields");
    for extracted in [
        "operations:",
        "cq_credits:",
        "accepted_operations:",
        "pending_reclamations:",
        "rejected_cqes:",
        "rejected_cqe_reasons:",
        "quarantined_operations:",
        "quarantined_mrs:",
        "quarantined_bytes:",
        "published_completion_connections:",
    ] {
        assert!(
            !engine_shared.contains(extracted),
            "{} must compose IoCore instead of declaring `{extracted}`",
            engine_mod_path.display()
        );
    }
    assert!(engine_shared.contains("io_core: Arc<IoCore>"));
    assert!(engine_shared.contains("session: Arc<SessionManager>"));
    for session_owned in [
        "connection_admission:",
        "connections:",
        "cm:",
        "rejected_cm_events:",
        "deadline_requests:",
        "admission:",
        "shutdown_connection_close_started:",
        "quarantines:",
    ] {
        assert!(
            !engine_shared.contains(session_owned),
            "{} must compose SessionManager instead of declaring `{session_owned}`",
            engine_mod_path.display()
        );
    }
    assert!(engine_mod.contains("pub use io_core::RdmaOperation;"));

    let session_source = fs::read_to_string(&session_path).expect("read session manager source");
    let session_manager = session_source
        .split("pub(super) struct SessionManager {")
        .nth(1)
        .and_then(|tail| tail.split("\n}").next())
        .expect("locate SessionManager fields");
    for owned in [
        "connection_admission:",
        "connections:",
        "cm:",
        "deadline_requests:",
        "admission:",
        "shutdown_connection_close_started:",
        "quarantines:",
    ] {
        assert!(
            session_manager.contains(owned),
            "{} must own `{owned}`",
            session_path.display()
        );
    }
    assert!(
        session_source.contains("pub(crate) struct SessionConnection")
            && session_source.contains("manager: Weak<SessionManager>")
            && session_source.contains("fn request_connection_close("),
        "{} must define an opaque request-only close capability interpreted by SessionManager",
        session_path.display()
    );

    let connection_source = fs::read_to_string(&connection_path).expect("read connection source");
    assert!(
        connection_source.contains("owner: Weak<dyn WorkRequestPoster>"),
        "session posting adapter must not strongly retain the QP/CmId owner"
    );
    let connection_owner_violations = find_strong_owner_fields(
        &connection_source,
        "RdmaConnection",
        &["EngineShared", "ConnectionState", "SessionManager"],
    )
    .expect("parse RdmaConnection ownership fields");
    assert!(
        connection_owner_violations.is_empty(),
        "{} production frontend strongly owns session internals: {}",
        connection_path.display(),
        connection_owner_violations.join(", ")
    );
    assert!(
        connection_source.contains(
            "pub async fn close(&self) -> Result<()> {\n        self.session.close().await"
        ) && connection_source.contains("self.session.request_close();"),
        "{} public close and last-frontend close must use the opaque session capability",
        connection_path.display()
    );

    let listener_source = fs::read_to_string(&listener_path).expect("read listener source");
    for waiter in ["RdmaListener", "ListenWaiter", "AcceptWaiter"] {
        let violations = find_strong_owner_fields(
            &listener_source,
            waiter,
            &[
                "EngineShared",
                "ListenerState",
                "SessionManager",
                "ListenRequest",
                "AcceptRequest",
            ],
        )
        .unwrap_or_else(|error| panic!("parse {waiter} ownership fields: {error}"));
        assert!(
            violations.is_empty(),
            "{} `{waiter}` strongly owns session internals: {}",
            listener_path.display(),
            violations.join(", ")
        );
    }
    let cm_source =
        fs::read_to_string(v2_dir.join("engine").join("cm.rs")).expect("read CM source");
    let connect_waiter_violations = find_strong_owner_fields(
        &cm_source,
        "ConnectWaiter",
        &[
            "EngineShared",
            "ConnectionState",
            "SessionManager",
            "OutboundRequest",
        ],
    )
    .expect("parse ConnectWaiter ownership fields");
    assert!(
        connect_waiter_violations.is_empty(),
        "ConnectWaiter strongly owns session internals: {}",
        connect_waiter_violations.join(", ")
    );

    let drain_source = fs::read_to_string(&drain_path).expect("read drain source");
    assert!(
        drain_source.contains("impl SessionManager")
            && drain_source.contains("#[cfg(test)]\nimpl EngineShared"),
        "{} production close/drain/retirement policy must be implemented on SessionManager",
        drain_path.display()
    );
    let driver_source = fs::read_to_string(&driver_path).expect("read engine driver source");
    assert!(
        driver_source.contains(".session")
            && driver_source.contains(".io_core")
            && driver_source.contains("service_cm_software(")
            && driver_source.contains("try_process_cm_event(")
            && driver_source.contains("service_deferred_cm_destructions(")
            && !driver_source.contains("session.cm")
            && !driver_source.contains("tokio::spawn("),
        "{} must explicitly compose SessionManager and IoCore without spawning",
        driver_path.display()
    );
    assert!(
        cm_source.contains("impl SessionManager")
            && cm_source.contains("fn retire_registered_connection("),
        "connection retirement policy must be implemented on SessionManager"
    );
    assert!(
        session_source.contains("struct SessionLifecycleAuthority")
            && session_source.contains("lifecycle_authority: SessionLifecycleAuthority")
            && session_source.contains("struct QpDestructionProof")
            && !session_source.contains("derive(Clone)]\npub(super) struct QpDestructionProof")
            && !session_source.contains("derive(Copy)]\npub(super) struct QpDestructionProof"),
        "SessionManager must own non-cloneable lifecycle authority and QP proof"
    );
    assert_eq!(
        session_source.matches("QpDestructionProof {").count(),
        5,
        "QP proof occurrences are limited to its definition, mint methods, constructors, and one consuming destructure"
    );
    assert!(
        connection_source.contains(
            "fn destroy_qp_for_session(\n        &self,\n        _authority: &SessionLifecycleAuthority,"
        ) && connection_source.contains(
            "fn transition_to_error_once(\n        &self,\n        _authority: &SessionLifecycleAuthority,"
        ),
        "QP destroy and error transition must require SessionManager lifecycle authority"
    );
    let engine_dir = v2_dir.join("engine");
    for path in collect_rs_files(&engine_dir).expect("enumerate engine sources") {
        let source = fs::read_to_string(&path).expect("read engine source");
        let mut calls = find_production_lifecycle_calls(
            &source,
            &["to_error", "destroy_qp", "destroy_connection"],
        )
        .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()));
        if path == connection_path {
            let authorized_adapters = [
                "transition_to_error_once",
                "destroy_connection_resources",
                "destroy_qp_for_session",
                "destroy_unregistered_for_session",
                "to_error",
                "destroy_connection",
            ];
            calls.retain(|call| {
                let function = call.split(':').nth(1).unwrap_or("<unknown>");
                !authorized_adapters.contains(&function)
            });
        }
        assert!(
            calls.is_empty(),
            "{} bypasses SessionManager lifecycle authority: {}",
            path.display(),
            calls.join(", ")
        );
    }
    assert!(
        connection_source.contains(
            "transition_to_error_for_test(&self) -> Result<()> {\n        self.session.transition_to_error_for_test()"
        ) && cm_source.matches("destroy_unregistered_connection(&verbs)").count() == 2
            && !cm_source.contains(".destroy_connection(true)"),
        "test hooks and setup rollback must route provider-visible lifecycle work through SessionManager"
    );
    for authorized in [
        "transition_to_error_once",
        "destroy_connection_resources",
        "destroy_qp_for_session",
        "destroy_unregistered_for_session",
    ] {
        let signature = format!("fn {authorized}(");
        let start = connection_source
            .find(&signature)
            .unwrap_or_else(|| panic!("locate authorized lifecycle adapter {authorized}"));
        let declaration = &connection_source[start..connection_source.len().min(start + 260)];
        assert!(
            declaration.contains("SessionLifecycleAuthority"),
            "authorized lifecycle adapter `{authorized}` must require SessionLifecycleAuthority"
        );
    }
    let io_core_operation_source =
        fs::read_to_string(&io_core_operation_path).expect("read I/O operation source");
    assert!(
        !io_core_operation_source.contains("QpDestructionProof"),
        "production I/O core must not depend on the session destruction proof type"
    );
    assert!(
        io_core_operation_source.contains("struct QpReclaimCapability")
            && io_core_operation_source.contains("pub(super) fn new(core: &Arc<IoCore>)")
            && io_core_operation_source.contains("\n    fn reclaim_after_qp_destroy(")
            && !io_core_operation_source
                .contains("pub(in crate::v2::engine) fn reclaim_after_qp_destroy(")
            && session_source.contains("qp_reclaim: QpReclaimCapability")
            && session_source.contains("self.qp_reclaim.reclaim("),
        "IoCore reclamation must be private behind the one non-forgeable SessionManager capability"
    );
    for path in collect_rs_files(&engine_dir).expect("enumerate reclaim call sites") {
        if path == io_core_operation_path || path == session_path {
            continue;
        }
        let source = fs::read_to_string(&path).expect("read engine source");
        assert!(
            !source.contains(".io_core.reclaim_after_qp_destroy("),
            "{} bypasses the proof-gated QP reclaim capability",
            path.display()
        );
    }

    let io_source = fs::read_to_string(&io_path).expect("read engine I/O boundary source");
    let io_connection = io_source
        .split("pub(crate) struct IoConnection {")
        .nth(1)
        .and_then(|tail| tail.split("\n}").next())
        .expect("locate IoConnection fields");
    assert!(
        !io_connection.contains("EngineShared")
            && !io_connection.contains("ConnectionState")
            && io_connection.contains("session: SessionConnection"),
        "{} must retain only the opaque session request capability, not engine/session owners",
        io_path.display()
    );
    assert!(
        io_source.contains("self.session.request_close()")
            && io_source.contains("self.session.close().await"),
        "{} close paths must use the opaque session request capability",
        io_path.display()
    );
    for forbidden in [
        "super::cm",
        "super::listener",
        "engine::cm",
        "engine::listener",
        "message_transport",
    ] {
        assert!(
            !io_source.contains(forbidden),
            "{} must not depend on `{forbidden}`",
            io_path.display()
        );
    }

    for fixed_path in ["cm.rs", "listener.rs", "drain.rs"] {
        assert!(
            v2_dir.join("engine").join(fixed_path).is_file(),
            "phase-one relocation must keep engine/{fixed_path} at its transitional path"
        );
        assert!(
            !v2_dir
                .join("engine")
                .join("session")
                .join(fixed_path)
                .exists(),
            "phase-one relocation must defer moving {fixed_path}"
        );
    }
    for relocated in ["mod.rs", "connection.rs", "registry.rs"] {
        assert!(
            v2_dir
                .join("engine")
                .join("session")
                .join(relocated)
                .is_file(),
            "phase-one relocation requires engine/session/{relocated}"
        );
    }
    for obsolete in ["session.rs", "connection.rs"] {
        assert!(
            !v2_dir.join("engine").join(obsolete).exists(),
            "phase-one relocation must remove obsolete engine/{obsolete}"
        );
    }

    let v2_mod = fs::read_to_string(&v2_mod_path).expect("read public v2 module source");
    let public_reexports = v2_mod
        .lines()
        .filter(|line| line.trim_start().starts_with("pub use "))
        .collect::<Vec<_>>()
        .join("\n");
    for internal in [
        "IoConnection",
        "IoEvent",
        "IoEventReceiver",
        "IoOperationContext",
        "IoRecvRequest",
        "IoSendRequest",
        "IoSubmissionDisposition",
        "SessionConnection",
        "SessionListener",
        "SessionManager",
        "SessionLifecycleAuthority",
        "QpDestructionProof",
    ] {
        assert!(
            !public_reexports.contains(internal),
            "crate-private I/O boundary type `{internal}` was publicly re-exported"
        );
    }
}
