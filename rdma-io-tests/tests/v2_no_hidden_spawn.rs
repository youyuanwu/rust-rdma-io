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
    Attribute, Expr, ExprBlock, ExprMethodCall, ExprPath, ForeignItem, ImplItem, Item, Local,
    Macro, Meta, Pat, Token, TraitItem, Type, UseTree,
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
