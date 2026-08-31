//! AST-level regression ensuring v2 production code starts no hidden work.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{
    Attribute, ExprBlock, ExprMethodCall, ExprPath, ForeignItem, ImplItem, Item, Meta, Token,
    TraitItem, UseTree,
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
const EXECUTOR_TYPES: &[&str] = &["Builder", "Runtime", "ThreadPool"];
const CONSTRUCTOR_NAMES: &[&str] = &["build", "new", "new_current_thread", "new_multi_thread"];

fn collect_rs_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                files.extend(collect_rs_files(&path));
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
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
            self.call_aliases.insert(alias.to_owned());
        }
        if is_executor_type(original) && is_forbidden_namespace(prefix) {
            self.executor_aliases.insert(alias.to_owned());
        }
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
}

fn is_forbidden_namespace(segments: &[String]) -> bool {
    segments.iter().any(|segment| {
        matches!(
            segment.as_str(),
            "async_std" | "futures" | "rayon" | "runtime" | "smol" | "std" | "thread" | "tokio"
        )
    })
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
        let segments: Vec<_> = path
            .segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect();
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
            && is_forbidden_namespace(&segments)
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

fn production() {
    launch(async {});
    tokio::task::spawn_local(async {});
    std::thread::Builder::new().spawn(|| {});
    RuntimeBuilder::new_multi_thread().build().unwrap();
    std::thread::scope(|_| {});
}
"#;
    let violations = find_spawn_violations(source);
    for expected in ["launch", "spawn_local", "spawn", "RuntimeBuilder", "scope"] {
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains(expected)),
            "missing {expected:?} in {violations:#?}"
        );
    }
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

    let files = collect_rs_files(&v2_dir);
    assert!(
        !files.is_empty(),
        "no .rs files found under {}",
        v2_dir.display()
    );

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
