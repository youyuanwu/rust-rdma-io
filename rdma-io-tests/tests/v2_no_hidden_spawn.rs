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
    Attribute, Expr, ExprBlock, ExprCall, ExprField, ExprMethodCall, ExprPath, ForeignItem,
    ImplItem, Item, Local, Macro, Meta, Pat, Token, TraitItem, Type, UseTree,
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

struct AnyDependencyVisitor<'a> {
    forbidden: &'a HashSet<String>,
    violations: Vec<String>,
}

impl<'ast> Visit<'ast> for AnyDependencyVisitor<'_> {
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

fn find_forbidden_dependencies_including_tests(
    source: &str,
    forbidden: &[&str],
) -> Result<Vec<String>, syn::Error> {
    let syntax = syn::parse_file(source)?;
    let forbidden = identifiers_and_aliases(&syntax, forbidden);
    let mut visitor = AnyDependencyVisitor {
        forbidden: &forbidden,
        violations: Vec::new(),
    };
    visitor.visit_file(&syntax);
    visitor.violations.sort();
    visitor.violations.dedup();
    Ok(visitor.violations)
}

fn find_inherent_methods(
    source: &str,
    owner: &str,
    forbidden_methods: &[&str],
) -> Result<Vec<String>, syn::Error> {
    fn inspect_items(
        items: &[Item],
        module_path: &mut Vec<String>,
        owners: &HashSet<String>,
        forbidden: &HashSet<&str>,
        violations: &mut Vec<String>,
    ) {
        for item in items {
            match item {
                Item::Impl(item)
                    if item.trait_.is_none()
                        && type_path_last(&item.self_ty)
                            .is_some_and(|name| owners.contains(&name)) =>
                {
                    for implementation_item in &item.items {
                        if let ImplItem::Fn(function) = implementation_item {
                            let name = function.sig.ident.to_string();
                            if forbidden.contains(name.as_str()) {
                                violations.push(format!(
                                    "{}:{}",
                                    qualified_name(module_path, &name),
                                    function.sig.ident.span().start().line
                                ));
                            }
                        }
                    }
                }
                Item::Mod(module) => {
                    if let Some((_, items)) = &module.content {
                        module_path.push(module.ident.to_string());
                        inspect_items(items, module_path, owners, forbidden, violations);
                        module_path.pop();
                    }
                }
                _ => {}
            }
        }
    }

    let syntax = syn::parse_file(source)?;
    let forbidden = forbidden_methods.iter().copied().collect::<HashSet<_>>();
    let owners = identifiers_and_aliases(&syntax, &[owner]);
    let mut violations = Vec::new();
    inspect_items(
        &syntax.items,
        &mut Vec::new(),
        &owners,
        &forbidden,
        &mut violations,
    );
    Ok(violations)
}

struct SelfFieldAccessVisitor<'a> {
    fields: &'a HashSet<&'a str>,
    found: bool,
}

impl Visit<'_> for SelfFieldAccessVisitor<'_> {
    fn visit_expr_field(&mut self, field: &ExprField) {
        let member = match &field.member {
            syn::Member::Named(member) => member.to_string(),
            syn::Member::Unnamed(_) => String::new(),
        };
        if self.fields.contains(member.as_str()) && expression_is_rooted_at_self(&field.base) {
            self.found = true;
        }
        visit::visit_expr_field(self, field);
    }
}

fn expression_is_rooted_at_self(expression: &Expr) -> bool {
    match expression {
        Expr::Path(path) => path.path.is_ident("self"),
        Expr::Field(field) => expression_is_rooted_at_self(&field.base),
        Expr::Paren(paren) => expression_is_rooted_at_self(&paren.expr),
        Expr::Reference(reference) => expression_is_rooted_at_self(&reference.expr),
        _ => false,
    }
}

fn find_inherent_methods_accessing_fields(
    source: &str,
    owner: &str,
    fields: &[&str],
) -> Result<Vec<String>, syn::Error> {
    fn inspect_items(
        items: &[Item],
        module_path: &mut Vec<String>,
        owners: &HashSet<String>,
        fields: &HashSet<&str>,
        methods: &mut Vec<String>,
    ) {
        for item in items {
            match item {
                Item::Impl(item)
                    if item.trait_.is_none()
                        && type_path_last(&item.self_ty)
                            .is_some_and(|name| owners.contains(&name)) =>
                {
                    for implementation_item in &item.items {
                        let ImplItem::Fn(function) = implementation_item else {
                            continue;
                        };
                        let mut visitor = SelfFieldAccessVisitor {
                            fields,
                            found: false,
                        };
                        visitor.visit_block(&function.block);
                        if visitor.found {
                            methods
                                .push(qualified_name(module_path, &function.sig.ident.to_string()));
                        }
                    }
                }
                Item::Mod(module) => {
                    if let Some((_, items)) = &module.content {
                        module_path.push(module.ident.to_string());
                        inspect_items(items, module_path, owners, fields, methods);
                        module_path.pop();
                    }
                }
                _ => {}
            }
        }
    }

    let syntax = syn::parse_file(source)?;
    let owners = identifiers_and_aliases(&syntax, &[owner]);
    let fields = fields.iter().copied().collect::<HashSet<_>>();
    let mut methods = Vec::new();
    inspect_items(
        &syntax.items,
        &mut Vec::new(),
        &owners,
        &fields,
        &mut methods,
    );
    methods.sort();
    methods.dedup();
    Ok(methods)
}

fn find_functions_using_dependencies(
    source: &str,
    forbidden: &[&str],
) -> Result<Vec<String>, syn::Error> {
    fn inspect_items(
        items: &[Item],
        module_path: &mut Vec<String>,
        forbidden: &HashSet<String>,
        violations: &mut Vec<String>,
    ) {
        for item in items {
            match item {
                Item::Fn(function) => {
                    let mut visitor = AnyDependencyVisitor {
                        forbidden,
                        violations: Vec::new(),
                    };
                    visitor.visit_signature(&function.sig);
                    visitor.visit_block(&function.block);
                    if !visitor.violations.is_empty() {
                        violations
                            .push(qualified_name(module_path, &function.sig.ident.to_string()));
                    }
                }
                Item::Impl(implementation) => {
                    let owner = type_path_last(&implementation.self_ty)
                        .unwrap_or_else(|| "<impl>".to_owned());
                    for item in &implementation.items {
                        let ImplItem::Fn(function) = item else {
                            continue;
                        };
                        let mut visitor = AnyDependencyVisitor {
                            forbidden,
                            violations: Vec::new(),
                        };
                        visitor.visit_signature(&function.sig);
                        visitor.visit_block(&function.block);
                        if !visitor.violations.is_empty() {
                            let method = format!("{owner}::{}", function.sig.ident);
                            violations.push(qualified_name(module_path, &method));
                        }
                    }
                }
                Item::Mod(module) => {
                    if let Some((_, items)) = &module.content {
                        module_path.push(module.ident.to_string());
                        inspect_items(items, module_path, forbidden, violations);
                        module_path.pop();
                    }
                }
                _ => {}
            }
        }
    }

    let syntax = syn::parse_file(source)?;
    let forbidden = identifiers_and_aliases(&syntax, forbidden);
    let mut violations = Vec::new();
    inspect_items(&syntax.items, &mut Vec::new(), &forbidden, &mut violations);
    violations.sort();
    violations.dedup();
    Ok(violations)
}

fn find_structs_using_dependencies(
    source: &str,
    forbidden: &[&str],
) -> Result<Vec<String>, syn::Error> {
    fn inspect_items(
        items: &[Item],
        module_path: &mut Vec<String>,
        forbidden: &HashSet<String>,
        violations: &mut Vec<String>,
    ) {
        for item in items {
            match item {
                Item::Struct(structure) => {
                    let mut visitor = AnyDependencyVisitor {
                        forbidden,
                        violations: Vec::new(),
                    };
                    for field in &structure.fields {
                        visitor.visit_field(field);
                    }
                    if !visitor.violations.is_empty() {
                        violations.push(qualified_name(module_path, &structure.ident.to_string()));
                    }
                }
                Item::Mod(module) => {
                    if let Some((_, items)) = &module.content {
                        module_path.push(module.ident.to_string());
                        inspect_items(items, module_path, forbidden, violations);
                        module_path.pop();
                    }
                }
                _ => {}
            }
        }
    }

    let syntax = syn::parse_file(source)?;
    let forbidden = identifiers_and_aliases(&syntax, forbidden);
    let mut violations = Vec::new();
    inspect_items(&syntax.items, &mut Vec::new(), &forbidden, &mut violations);
    violations.sort();
    violations.dedup();
    Ok(violations)
}

fn trait_method_names(source: &str, trait_name: &str) -> Result<Vec<String>, syn::Error> {
    let syntax = syn::parse_file(source)?;
    let mut methods = syntax
        .items
        .into_iter()
        .find_map(|item| {
            let Item::Trait(item) = item else {
                return None;
            };
            (item.ident == trait_name).then(|| {
                item.items
                    .into_iter()
                    .filter_map(|item| match item {
                        TraitItem::Fn(function) => Some(function.sig.ident.to_string()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            })
        })
        .unwrap_or_default();
    methods.sort();
    Ok(methods)
}

fn named_struct_fields(source: &str, struct_name: &str) -> Result<Vec<String>, syn::Error> {
    let syntax = syn::parse_file(source)?;
    let mut fields = syntax
        .items
        .into_iter()
        .find_map(|item| {
            let Item::Struct(item) = item else {
                return None;
            };
            (item.ident == struct_name).then(|| {
                item.fields
                    .into_iter()
                    .filter_map(|field| field.ident.map(|ident| ident.to_string()))
                    .collect::<Vec<_>>()
            })
        })
        .unwrap_or_default();
    fields.sort();
    Ok(fields)
}

fn find_trait_dependencies(
    source: &str,
    trait_name: &str,
    forbidden: &[&str],
) -> Result<Vec<String>, syn::Error> {
    let syntax = syn::parse_file(source)?;
    let forbidden = identifiers_and_aliases(&syntax, forbidden);
    let mut violations = Vec::new();
    for item in &syntax.items {
        let Item::Trait(item) = item else {
            continue;
        };
        if item.ident != trait_name {
            continue;
        }
        let mut visitor = AnyDependencyVisitor {
            forbidden: &forbidden,
            violations: Vec::new(),
        };
        visitor.visit_item_trait(item);
        violations.extend(visitor.violations);
    }
    violations.sort();
    violations.dedup();
    Ok(violations)
}

fn has_trait_impl(source: &str, owner: &str, trait_name: &str) -> Result<bool, syn::Error> {
    fn inspect_items(items: &[Item], owners: &HashSet<String>, traits: &HashSet<String>) -> bool {
        items.iter().any(|item| match item {
            Item::Impl(item) => {
                type_path_last(&item.self_ty).is_some_and(|name| owners.contains(&name))
                    && item
                        .trait_
                        .as_ref()
                        .and_then(|(path, _)| path.segments.last())
                        .is_some_and(|segment| traits.contains(&segment.ident.to_string()))
            }
            Item::Mod(module) => module
                .content
                .as_ref()
                .is_some_and(|(_, items)| inspect_items(items, owners, traits)),
            _ => false,
        })
    }

    let syntax = syn::parse_file(source)?;
    let owners = identifiers_and_aliases(&syntax, &[owner]);
    let traits = identifiers_and_aliases(&syntax, &[trait_name]);
    Ok(inspect_items(&syntax.items, &owners, &traits))
}

fn find_method_parameter_dependencies(
    source: &str,
    owner: &str,
    method: &str,
    forbidden: &[&str],
) -> Result<Vec<String>, syn::Error> {
    let syntax = syn::parse_file(source)?;
    let forbidden = identifiers_and_aliases(&syntax, forbidden);
    let owners = identifiers_and_aliases(&syntax, &[owner]);
    let mut violations = Vec::new();
    for item in syntax.items {
        let Item::Impl(item) = item else {
            continue;
        };
        if !type_path_last(&item.self_ty).is_some_and(|name| owners.contains(&name)) {
            continue;
        }
        for implementation_item in item.items {
            let ImplItem::Fn(function) = implementation_item else {
                continue;
            };
            if function.sig.ident != method {
                continue;
            }
            let mut visitor = AnyDependencyVisitor {
                forbidden: &forbidden,
                violations: Vec::new(),
            };
            for input in &function.sig.inputs {
                visitor.visit_fn_arg(input);
            }
            violations.extend(visitor.violations);
        }
    }
    Ok(violations)
}

fn type_path_last(ty: &Type) -> Option<String> {
    match ty {
        Type::Path(path) => path
            .path
            .segments
            .last()
            .map(|segment| segment.ident.to_string()),
        _ => None,
    }
}

fn qualified_name(module_path: &[String], name: &str) -> String {
    if module_path.is_empty() {
        name.to_owned()
    } else {
        format!("{}::{name}", module_path.join("::"))
    }
}

fn identifiers_and_aliases(syntax: &syn::File, identifiers: &[&str]) -> HashSet<String> {
    fn collect_aliases(items: &[Item], names: &mut HashSet<String>) -> bool {
        let mut changed = false;
        for item in items {
            match item {
                Item::Use(item) => {
                    let before = names.len();
                    collect_use_aliases(&item.tree, names);
                    changed |= names.len() != before;
                }
                Item::Type(alias)
                    if type_path_last(&alias.ty).is_some_and(|name| names.contains(&name)) =>
                {
                    changed |= names.insert(alias.ident.to_string());
                }
                Item::Mod(module) => {
                    if let Some((_, items)) = &module.content {
                        changed |= collect_aliases(items, names);
                    }
                }
                _ => {}
            }
        }
        changed
    }

    let mut names = identifiers
        .iter()
        .map(|identifier| (*identifier).to_owned())
        .collect::<HashSet<_>>();
    while collect_aliases(&syntax.items, &mut names) {}
    names
}

fn collect_use_aliases(tree: &UseTree, names: &mut HashSet<String>) {
    match tree {
        UseTree::Path(path) => collect_use_aliases(&path.tree, names),
        UseTree::Rename(rename) if names.contains(&rename.ident.to_string()) => {
            names.insert(rename.rename.to_string());
        }
        UseTree::Group(group) => {
            for item in &group.items {
                collect_use_aliases(item, names);
            }
        }
        UseTree::Name(_) | UseTree::Rename(_) | UseTree::Glob(_) => {}
    }
}

struct LiveIoProofIssuanceVisitor {
    locations: Vec<usize>,
}

impl<'ast> Visit<'ast> for LiveIoProofIssuanceVisitor {
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
        if path
            .segments
            .iter()
            .any(|segment| segment.ident == "issue_live_io_proof")
        {
            self.locations.push(path.span().start().line);
        }
        visit::visit_path(self, path);
    }
}

fn find_live_io_proof_issuance(source: &str) -> Result<Vec<usize>, syn::Error> {
    let syntax = syn::parse_file(source)?;
    let mut visitor = LiveIoProofIssuanceVisitor {
        locations: Vec::new(),
    };
    visitor.visit_file(&syntax);
    Ok(visitor.locations)
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
        use crate::EngineShared as RootAlias;
        use crate::v2::engine::session::SessionManager;

        #[cfg(test)]
        use crate::ConnectionState;

        fn production(root: RootAlias, value: crate::WorkRequestPoster) {
            let _ = (root, value);
        }
    "#;
    let violations = find_forbidden_production_dependencies(
        source,
        &[
            "EngineShared",
            "ConnectionState",
            "WorkRequestPoster",
            "SessionManager",
        ],
    )
    .unwrap();
    assert_eq!(violations.len(), 3, "{violations:#?}");
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
        violations
            .iter()
            .any(|violation| violation.starts_with("SessionManager:"))
    );
    assert!(
        !violations
            .iter()
            .any(|violation| violation.starts_with("ConnectionState:"))
    );
}

#[test]
fn owner_boundary_detectors_reject_test_only_aliases_and_renamed_adapters() {
    let aliased_root = r#"
        #[cfg(test)]
        use crate::EngineShared as RootAlias;

        #[cfg(test)]
        struct Fixture {
            root: std::sync::Arc<RootAlias>,
        }
    "#;
    assert!(
        !find_forbidden_dependencies_including_tests(aliased_root, &["EngineShared"])
            .unwrap()
            .is_empty(),
        "test-only root aliases must remain visible to the ownership guard"
    );

    let aliased_deref = r#"
        use std::ops::Deref as BroadAccess;
        struct EngineShared;
        struct SessionManager;
        impl BroadAccess for EngineShared {
            type Target = SessionManager;
            fn deref(&self) -> &Self::Target { unimplemented!() }
        }
    "#;
    assert!(
        has_trait_impl(aliased_deref, "EngineShared", "Deref").unwrap(),
        "renaming Deref must not evade the broad-adapter guard"
    );

    let aliased_owner = r#"
        struct EngineShared;
        type RootAlias = EngineShared;
        impl RootAlias {
            fn enqueue_completion(&self) {}
        }
    "#;
    assert_eq!(
        find_inherent_methods(aliased_owner, "EngineShared", &["enqueue_completion"]).unwrap(),
        vec!["enqueue_completion:5"]
    );

    let aliased_parameter = r#"
        struct EngineShared;
        type RootAlias = EngineShared;
        struct IoConnection;
        impl IoConnection {
            fn new(root: std::sync::Arc<RootAlias>) { let _ = root; }
        }
    "#;
    assert!(
        !find_method_parameter_dependencies(
            aliased_parameter,
            "IoConnection",
            "new",
            &["EngineShared"],
        )
        .unwrap()
        .is_empty(),
        "renaming EngineShared must not restore a full-root I/O fixture"
    );

    let renamed_forwarder = r#"
        struct EngineShared { io_core: IoCore }
        struct IoCore;
        impl IoCore { fn arbitrary_name(&self) {} }
        impl EngineShared {
            fn newly_named_adapter(&self) { self.io_core.arbitrary_name(); }
        }
    "#;
    assert_eq!(
        find_inherent_methods_accessing_fields(
            renamed_forwarder,
            "EngineShared",
            &["io_core", "session"],
        )
        .unwrap(),
        vec!["newly_named_adapter"],
        "a renamed root-to-owner forwarder must be detected by field access, not method name"
    );

    let root_test_fixture = r#"
        use crate::EngineShared as RootAlias;
        #[test]
        fn root_spanning_fixture() {
            let _: std::sync::Arc<RootAlias> = todo!();
        }
        fn owner_focused_fixture(io_core: IoCore, session: SessionManager) {
            let _ = (io_core, session);
        }
    "#;
    assert_eq!(
        find_functions_using_dependencies(root_test_fixture, &["EngineShared"]).unwrap(),
        vec!["root_spanning_fixture"],
        "aliased concrete roots in test functions must be detected without rejecting owner parts"
    );
    let nested_root_wrapper = r#"
        struct EngineShared;
        mod tests {
            type HiddenRoot = super::EngineShared;
            struct WrappedFixture {
                root: std::sync::Arc<HiddenRoot>,
            }
        }
    "#;
    assert_eq!(
        find_structs_using_dependencies(nested_root_wrapper, &["EngineShared"]).unwrap(),
        vec!["tests::WrappedFixture"],
        "nested aliases and module-level root wrappers must not evade the fixture guard"
    );

    let nested_adapters = r#"
        struct IoCore;
        struct SessionManager;
        struct EngineShared { io_core: IoCore }
        mod tests {
            use std::ops::Deref as BroadAccess;
            type RootAlias = super::EngineShared;
            impl RootAlias {
                fn enqueue_completion(&self) { self.io_core.submit(); }
            }
            impl BroadAccess for RootAlias {
                type Target = super::SessionManager;
                fn deref(&self) -> &Self::Target { unimplemented!() }
            }
        }
    "#;
    assert!(
        find_inherent_methods(nested_adapters, "EngineShared", &["enqueue_completion"])
            .unwrap()
            .iter()
            .any(|method| method.starts_with("tests::enqueue_completion:")),
        "nested EngineShared implementations must not evade the obsolete-forwarder guard"
    );
    assert_eq!(
        find_inherent_methods_accessing_fields(
            nested_adapters,
            "EngineShared",
            &["io_core", "session"],
        )
        .unwrap(),
        vec!["tests::enqueue_completion"],
        "nested EngineShared implementations must not evade the owner-forwarder guard"
    );
    assert!(
        has_trait_impl(nested_adapters, "EngineShared", "Deref").unwrap(),
        "nested and renamed Deref implementations must not evade the broad-adapter guard"
    );

    assert_eq!(
        trait_method_names(
            "trait Runtime { fn publish(&self); fn fail(&self, error: Error); }",
            "Runtime",
        )
        .unwrap(),
        ["fail", "publish"],
        "runtime capability method changes must be structurally visible"
    );
    assert!(
        !find_trait_dependencies(
            "type BroadConfig = EngineConfig; trait Runtime { fn config(&self) -> BroadConfig; }",
            "Runtime",
            &["EngineConfig"],
        )
        .unwrap()
        .is_empty(),
        "aliased broad configuration in a runtime capability must be detected"
    );
    assert_eq!(
        named_struct_fields(
            "struct SessionConfig { drain: Duration, capacity: usize }",
            "SessionConfig",
        )
        .unwrap(),
        ["capacity", "drain"],
        "session configuration breadth must be structurally visible"
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
        v2_dir.join("engine").join("session").join("cm.rs"),
        v2_dir.join("engine").join("session").join("connection.rs"),
        v2_dir.join("engine").join("session").join("drain.rs"),
        v2_dir.join("engine").join("session").join("listener.rs"),
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
fn test_live_io_proof_issuance_is_confined_to_session_registry() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace_root = Path::new(manifest_dir).parent().expect("workspace root");
    let engine_dir = workspace_root
        .join("rdma-io")
        .join("src")
        .join("v2")
        .join("engine");
    let issuer_path = engine_dir.join("session").join("registry.rs");

    for path in collect_rs_files(&engine_dir).expect("enumerate live-I/O proof issuance sites") {
        let source = fs::read_to_string(&path).expect("read engine source");
        let locations =
            find_live_io_proof_issuance(&source).expect("parse live-I/O proof issuance source");
        if path == issuer_path {
            assert_eq!(
                locations.len(),
                1,
                "{} must contain the sole live-I/O proof issuance site",
                path.display()
            );
        } else {
            assert!(
                locations.is_empty(),
                "{} must not issue LiveIoConnectionProof values at lines {locations:?}",
                path.display()
            );
        }
    }

    let controlled_source = r#"
        fn forbidden_production_issuance() {
            let _ = LiveIoConnectionProof::issue_live_io_proof(connection, qp_num);
        }

        #[cfg(test)]
        fn allowed_test_fixture() {
            let _ = LiveIoConnectionProof::issue_live_io_proof(connection, qp_num);
        }
    "#;
    assert_eq!(
        find_live_io_proof_issuance(controlled_source).expect("parse controlled source"),
        vec![3],
        "the structural guard must reject production issuance while ignoring test-only fixtures"
    );
}

#[test]
fn test_v2_io_boundary_dependency_direction_and_visibility() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace_root = Path::new(manifest_dir).parent().expect("workspace root");
    let v2_dir = workspace_root.join("rdma-io").join("src").join("v2");
    let engine_dir = v2_dir.join("engine");
    let completion_path = v2_dir.join("completion.rs");
    let message_path = v2_dir.join("message_transport.rs");
    let io_path = v2_dir.join("engine").join("io.rs");
    let io_core_dir = v2_dir.join("engine").join("io_core");
    let io_core_mod_path = io_core_dir.join("mod.rs");
    let io_core_operation_path = v2_dir.join("engine").join("io_core").join("operation.rs");
    let io_core_progress_path = v2_dir.join("engine").join("io_core").join("progress.rs");
    let engine_mod_path = v2_dir.join("engine").join("mod.rs");
    let config_path = v2_dir.join("engine").join("config.rs");
    let progress_path = v2_dir.join("engine").join("progress.rs");
    let scheduler_path = v2_dir.join("engine").join("scheduler.rs");
    let connection_path = v2_dir.join("engine").join("session").join("connection.rs");
    let cm_path = v2_dir.join("engine").join("session").join("cm.rs");
    let listener_path = v2_dir.join("engine").join("session").join("listener.rs");
    let driver_path = v2_dir.join("engine").join("driver.rs");
    let drain_path = v2_dir.join("engine").join("session").join("drain.rs");
    let session_path = v2_dir.join("engine").join("session").join("mod.rs");
    let session_progress_path = v2_dir.join("engine").join("session").join("progress.rs");
    let session_registry_path = v2_dir.join("engine").join("session").join("registry.rs");
    let v2_mod_path = v2_dir.join("mod.rs");

    let message = fs::read_to_string(&message_path).expect("read message transport source");
    let completion_source = fs::read_to_string(&completion_path).expect("read CQ readiness source");
    assert!(
        completion_source.contains("drain_and_ack_channel_events(cq, buf.len().max(1))")
            && completion_source.contains("while (count as usize) < budget"),
        "{} must bound CQ notification draining by the owner turn buffer",
        completion_path.display()
    );
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

    // Activated incrementally as the owner-local progress migrations remove each
    // dependency from the driver. Keeping the final assertion vocabulary together
    // prevents later phases from weakening the intended boundary.
    #[allow(dead_code)]
    const FINAL_DRIVER_FORBIDDEN_IO_KNOWLEDGE: &[&str] = &[
        "CqReadiness",
        "cq_buffer",
        "resources.cq",
        "cq_async_fd",
        "take_published_connection",
        "CompletionReadyConnection",
        "DeadlineKind::Reclamation",
    ];

    #[allow(dead_code)]
    const FINAL_DRIVER_FORBIDDEN_SESSION_KNOWLEDGE: &[&str] = &[
        "cm_async_fd",
        "cm_event_channel",
        "resources.cm",
        "try_process_cm_event",
        "service_cm_software",
        "service_deferred_cm_destructions",
        "handle_connection_drain_deadline",
        ".connections",
        ".listeners",
        "begin_connection_close",
        "begin_cm_shutdown",
        "begin_all_connection_close",
        "destroy_qp",
        "retire_registered_connection",
        "DeadlineKind::ConnectionDrain",
        "DeadlineKind::EngineShutdown",
    ];

    #[allow(dead_code)]
    const FINAL_DRIVER_FORBIDDEN_TERMINAL_KNOWLEDGE: &[&str] = &[
        "pending_cm_route_count",
        "live_connection_count",
        "accepted_operations",
        "accepted_count",
        "retained_bundle_count",
        "begin_all_connection_close",
        "begin_cm_shutdown",
        "synchronously_prepare_driver_drop",
        ".finish(",
    ];

    #[allow(dead_code)]
    fn assert_final_driver_boundary(path: &Path, source: &str, forbidden: &[&str]) {
        let violations = forbidden
            .iter()
            .copied()
            .filter(|needle| source.contains(needle))
            .collect::<Vec<_>>();
        assert!(
            violations.is_empty(),
            "{} retains layer-owned driver knowledge: {}",
            path.display(),
            violations.join(", ")
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
    for path in collect_rs_files(&io_core_dir).expect("enumerate I/O core source") {
        let source = fs::read_to_string(&path).expect("read I/O core source");
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
    assert!(
        engine_mod.contains("pub fn io_reclamation_budget(")
            && engine_mod.contains("pub fn session_reclamation_budget(")
            && !engine_mod.contains("pub fn reclamation_budget("),
        "{} must expose owner-local reclamation controls without a compatibility alias",
        engine_mod_path.display()
    );
    let progress_source =
        fs::read_to_string(&progress_path).expect("read owner-neutral progress source");
    for forbidden in [
        "ConnectionToken",
        "OperationToken",
        "ConnectionState",
        "SessionManager",
        "CmId",
        "WorkCompletion",
        "DeadlineKind",
        "IoCoreEffects",
        "IoEvent",
    ] {
        assert!(
            !progress_source.contains(forbidden),
            "{} must not expose layer-private `{forbidden}`",
            progress_path.display()
        );
    }
    for required in ["units_consumed", "immediate_work", "readiness"] {
        assert!(
            progress_source.contains(required),
            "{} must report `{required}`",
            progress_path.display()
        );
    }
    for forbidden in [
        "next_deadline",
        "ProgressTerminal",
        "EffectsPublication",
        "effects:",
    ] {
        assert!(
            !progress_source.contains(forbidden),
            "{} must not retain redundant progress-report state `{forbidden}`",
            progress_path.display()
        );
    }
    let io_core_source =
        fs::read_to_string(&io_core_mod_path).expect("read I/O core module source");
    assert!(
        io_core_source.contains("trait IoSessionBridge")
            && !io_core_source.contains("session_bridge:")
            && !io_core_source.contains("bind_session_bridge")
            && !io_core_source.contains("fn session_bridge("),
        "{} must define the narrow bridge without storing or binding it",
        io_core_mod_path.display()
    );
    let io_progress_source =
        fs::read_to_string(&io_core_progress_path).expect("read I/O progress source");
    assert!(
        io_progress_source.contains("bridge: Arc<dyn IoSessionBridge>")
            && io_progress_source.contains("bridge: Arc<dyn IoSessionBridge>,"),
        "{} must own the session bridge directly",
        io_core_progress_path.display()
    );
    assert!(
        !engine_mod.contains("bind_session_bridge"),
        "{} must not post-bind the I/O/session bridge",
        engine_mod_path.display()
    );
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
    for path in [
        &session_path,
        &cm_path,
        &connection_path,
        &listener_path,
        &drain_path,
        &session_progress_path,
        &session_registry_path,
    ] {
        let source = fs::read_to_string(path).expect("read session owner source");
        let violations = find_forbidden_production_dependencies(&source, &["EngineShared"])
            .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()));
        assert!(
            violations.is_empty(),
            "{} bypasses the narrow session runtime capability: {}",
            path.display(),
            violations.join(", ")
        );
        let all_code_violations =
            find_forbidden_dependencies_including_tests(&source, &["EngineShared"])
                .unwrap_or_else(|error| panic!("parse all code in {}: {error}", path.display()));
        assert!(
            all_code_violations.is_empty(),
            "{} restores a direct or aliased test root dependency: {}",
            path.display(),
            all_code_violations.join(", ")
        );
    }
    assert!(
        engine_mod.contains("trait SessionEngineRuntime: Send + Sync")
            && session_source.contains("engine: OnceLock<Weak<dyn SessionEngineRuntime>>"),
        "session-to-engine access must use one bind-once weak object-safe capability"
    );
    assert_eq!(
        trait_method_names(&engine_mod, "SessionEngineRuntime")
            .expect("parse SessionEngineRuntime methods"),
        [
            "admission_error",
            "begin_driver_failure",
            "outcome",
            "pending_terminal_outcome",
            "publish_io_work",
            "publish_session_work",
            "shutdown_deadline_failure",
            "shutdown_requested",
        ],
        "SessionEngineRuntime must expose only the reviewed global runtime operations"
    );
    let runtime_dependency_violations = find_trait_dependencies(
        &engine_mod,
        "SessionEngineRuntime",
        &[
            "EngineConfig",
            "SessionConfig",
            "ProviderLimits",
            "IoCore",
            "SessionManager",
            "ConnectionRegistry",
            "OperationRegistry",
        ],
    )
    .expect("parse SessionEngineRuntime dependency surface");
    assert!(
        runtime_dependency_violations.is_empty(),
        "SessionEngineRuntime exposes owner configuration or registries: {}",
        runtime_dependency_violations.join(", ")
    );
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
        session_manager.contains("config: SessionConfig")
            && !session_manager.contains("config: EngineConfig"),
        "{} must retain only the narrow immutable SessionConfig",
        session_path.display()
    );
    assert!(
        find_forbidden_dependencies_including_tests(&session_source, &["EngineConfig"])
            .expect("parse session configuration dependencies")
            .is_empty(),
        "{} must not regain the complete EngineConfig",
        session_path.display()
    );
    let config_source = fs::read_to_string(&config_path).expect("read engine configuration");
    assert_eq!(
        named_struct_fields(&config_source, "SessionConfig").expect("parse SessionConfig fields"),
        [
            "connection_drain_deadline",
            "cq_capacity",
            "max_inflight_operations",
            "max_live_connections",
        ],
        "SessionConfig must remain limited to session capacity, validation, and drain policy"
    );
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
    let cm_source = fs::read_to_string(&cm_path).expect("read CM source");
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
        drain_source.contains("impl SessionManager") && !drain_source.contains("impl EngineShared"),
        "{} close/drain/retirement policy and test access must remain on SessionManager",
        drain_path.display()
    );
    let mut broad_adapter_violations = Vec::new();
    for path in collect_rs_files(&engine_dir).expect("enumerate root/session Deref adapters") {
        let source = fs::read_to_string(&path).expect("read engine source");
        for owner in ["EngineShared", "SessionManager"] {
            if has_trait_impl(&source, owner, "Deref").unwrap_or_else(|error| {
                panic!("parse {} {owner} Deref impls: {error}", path.display())
            }) {
                broad_adapter_violations.push(format!("{}::{owner}", path.display()));
            }
        }
    }
    assert!(
        broad_adapter_violations.is_empty(),
        "tests must not restore broad root/session Deref adapters: {}",
        broad_adapter_violations.join(", ")
    );
    assert!(
        find_forbidden_dependencies_including_tests(&connection_source, &["EngineShared"])
            .expect("parse connection test ownership")
            .is_empty(),
        "RdmaConnection must not restore direct or aliased EngineShared ownership"
    );
    let allowed_root_functions = [
        "io.rs::IoConnection::with_delayed_close_event_for_test",
        "io_core/operation.rs::tests::synthetic_engine_root",
        "io_core/operation.rs::tests::terminal_wakers_can_reenter_after_terminal_guards_drop",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<HashSet<_>>();
    let mut concrete_root_functions = HashSet::new();
    let mut root_fixture_paths =
        collect_rs_files(&io_core_dir).expect("enumerate I/O core test fixtures");
    root_fixture_paths.push(io_path.clone());
    for path in root_fixture_paths {
        let source = fs::read_to_string(&path).expect("read I/O test fixture source");
        let relative = path
            .strip_prefix(&engine_dir)
            .expect("I/O fixture source beneath engine directory")
            .to_string_lossy();
        concrete_root_functions.extend(
            find_functions_using_dependencies(&source, &["EngineShared"])
                .unwrap_or_else(|error| {
                    panic!("parse {} root dependencies: {error}", path.display())
                })
                .into_iter()
                .map(|function| format!("{relative}::{function}")),
        );
        let concrete_root_structs = find_structs_using_dependencies(&source, &["EngineShared"])
            .unwrap_or_else(|error| {
                panic!("parse {} root-bearing structs: {error}", path.display())
            });
        assert!(
            concrete_root_structs.is_empty(),
            "{} retains a concrete-root fixture wrapper: {}",
            path.display(),
            concrete_root_structs.join(", ")
        );
    }
    let mut forbidden_root_functions = concrete_root_functions
        .difference(&allowed_root_functions)
        .cloned()
        .collect::<Vec<_>>();
    forbidden_root_functions.sort();
    assert!(
        forbidden_root_functions.is_empty(),
        "I/O sources retain the concrete composition root outside reviewed fixtures: {}",
        forbidden_root_functions.join(", ")
    );
    let mut missing_root_functions = allowed_root_functions
        .difference(&concrete_root_functions)
        .cloned()
        .collect::<Vec<_>>();
    missing_root_functions.sort();
    assert!(
        missing_root_functions.is_empty(),
        "reviewed I/O root-fixture allowlist is stale: {}",
        missing_root_functions.join(", ")
    );
    let io_operation_source =
        fs::read_to_string(&io_core_operation_path).expect("read I/O operation source");
    assert!(
        io_operation_source.contains("struct OperationOwners")
            && io_operation_source.contains("io_core: Arc<IoCore>")
            && io_operation_source.contains("session: Arc<SessionManager>")
            && io_operation_source.contains("_runtime: Arc<dyn SessionEngineRuntime>"),
        "{} must expose explicit owner-focused fixture parts with only an opaque runtime retain",
        io_core_operation_path.display()
    );
    let allowed_root_owner_methods = [
        "begin_driver_failure",
        "diagnostics",
        "finish",
        "handle_driver_drop",
        "mark_shutdown_requested",
        "request_shutdown",
        "retained_bundle_count",
        "shutdown_deadline_failure",
        "unsafe_outstanding_operations",
    ];
    let mut unexpected_root_owner_methods = Vec::new();
    for path in collect_rs_files(&engine_dir).expect("enumerate EngineShared implementations") {
        let source = fs::read_to_string(&path).expect("read EngineShared implementation");
        unexpected_root_owner_methods.extend(
            find_inherent_methods_accessing_fields(
                &source,
                "EngineShared",
                &["io_core", "session"],
            )
            .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
            .into_iter()
            .filter(|method| !allowed_root_owner_methods.contains(&method.as_str()))
            .map(|method| format!("{}::{method}", path.display())),
        );
    }
    assert!(
        unexpected_root_owner_methods.is_empty(),
        "EngineShared gained an unreviewed owner forwarder: {}",
        unexpected_root_owner_methods.join(", ")
    );
    let obsolete_forwarders = [
        "register_memory",
        "has_published_completions",
        "fn apply_io_effects(",
        "fn enqueue_completion(",
        "fn dispatch_connection_completions(",
        "fn reclaim_after_qp_destroy(",
        "fn handle_reclamation_deadline(",
    ];
    let obsolete_method_names = obsolete_forwarders.map(|name| {
        name.strip_prefix("fn ")
            .and_then(|name| name.strip_suffix('('))
            .unwrap_or(name)
    });
    let mut root_forwarder_violations = Vec::new();
    for path in collect_rs_files(&engine_dir).expect("enumerate root forwarders") {
        let source = fs::read_to_string(&path).expect("read engine source");
        root_forwarder_violations.extend(
            find_inherent_methods(&source, "EngineShared", &obsolete_method_names)
                .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
                .into_iter()
                .map(|violation| format!("{}:{violation}", path.display())),
        );
    }
    assert!(
        root_forwarder_violations.is_empty(),
        "obsolete root forwarders were restored: {}",
        root_forwarder_violations.join(", ")
    );
    for obsolete_forwarder in obsolete_forwarders {
        assert!(
            !engine_mod.contains(obsolete_forwarder),
            "{} must not restore obsolete root forwarder `{obsolete_forwarder}`",
            engine_mod_path.display()
        );
    }
    let driver_source = fs::read_to_string(&driver_path).expect("read engine driver source");
    let production_driver = driver_source
        .split(
            "#[cfg(any(test, feature = \"test-hooks\"))]\n#[doc(hidden)]\npub(super) mod test_api",
        )
        .next()
        .expect("locate production driver prefix");
    let session_progress_source =
        fs::read_to_string(&session_progress_path).expect("read session progress source");
    assert_final_driver_boundary(
        &driver_path,
        production_driver,
        FINAL_DRIVER_FORBIDDEN_IO_KNOWLEDGE,
    );
    assert_final_driver_boundary(
        &driver_path,
        production_driver,
        FINAL_DRIVER_FORBIDDEN_SESSION_KNOWLEDGE,
    );
    assert_final_driver_boundary(
        &driver_path,
        production_driver,
        FINAL_DRIVER_FORBIDDEN_TERMINAL_KNOWLEDGE,
    );
    assert!(
        production_driver.contains("io_progress")
            && production_driver.contains("session_progress")
            && production_driver.contains("fn poll_once(")
            && production_driver.contains("progress_driver_terminal(")
            && !production_driver.contains("session.cm")
            && !production_driver.contains("TERMINAL_WORK")
            && !production_driver.contains("OwnerClass::Terminal")
            && !production_driver.contains("service_terminal")
            && !driver_source.contains("tokio::spawn("),
        "{} must implement the bounded two-owner turn and terminal epilogue without spawning",
        driver_path.display()
    );
    let scheduler_source =
        fs::read_to_string(&scheduler_path).expect("read owner scheduler source");
    assert!(
        scheduler_source.contains("const OWNER_CLASS_COUNT: usize = 2")
            && !scheduler_source.contains("OwnerClass::Terminal"),
        "{} must rotate exactly the I/O and session owners",
        scheduler_path.display()
    );
    assert!(
        scheduler_source.contains("struct DeadlineQueue<P>")
            && scheduler_source.contains("struct DeadlineEntry<P>")
            && scheduler_source.contains(".checked_add(1)")
            && scheduler_source.contains("fn pop_one_due(")
            && scheduler_source.contains("struct AlternatingSources")
            && scheduler_source.contains("enum Source"),
        "{} must contain only stable generic deadline and alternating-source mechanics",
        scheduler_path.display()
    );
    for forbidden in [
        "DeadlineKind",
        "OperationToken",
        "ConnectionToken",
        "WorkCompletion",
        "ConnectionDrain",
        "EngineShutdown",
    ] {
        assert!(
            !scheduler_source.contains(forbidden),
            "{} must not interpret owner payload `{forbidden}`",
            scheduler_path.display()
        );
    }
    assert!(
        io_progress_source.contains("DeadlineQueue<OperationToken>")
            && session_progress_source.contains("DeadlineQueue<SessionDeadline>"),
        "each owner must bind the generic deadline queue to its local payload"
    );
    assert!(
        !progress_source.contains("Terminal"),
        "{} must not define a terminal scheduler owner",
        progress_path.display()
    );
    assert!(
        !io_core_source.contains("publish_terminal"),
        "{} must publish final-drain reconsideration through I/O work",
        io_core_mod_path.display()
    );
    let poll_once_source = production_driver
        .split("fn poll_once(")
        .nth(1)
        .and_then(|tail| tail.split("\n}\n\nimpl Future").next())
        .expect("locate bounded driver turn");
    let owner_loop = poll_once_source
        .find("for _ in 0..owner_budget")
        .expect("locate ready-at-entry owner loop");
    let io_turn = poll_once_source
        .find("OwnerClass::Io")
        .expect("locate I/O owner branch");
    let session_turn = poll_once_source
        .find("OwnerClass::Session")
        .expect("locate session owner branch");
    let terminal_epilogue = poll_once_source
        .find("progress_driver_terminal(")
        .expect("locate terminal epilogue");
    assert!(
        owner_loop < io_turn
            && owner_loop < session_turn
            && io_turn < terminal_epilogue
            && session_turn < terminal_epilogue,
        "terminal eligibility must follow the bounded two-owner pass"
    );
    let future_poll = production_driver
        .split("fn poll(mut self:")
        .nth(1)
        .expect("locate Future::poll");
    let first_timer = future_poll
        .find("self.poll_deadline_timer(cx)")
        .expect("locate initial timer processing");
    let observed_epoch = future_poll
        .find("let observed_epoch")
        .expect("locate epoch observation");
    let bounded_turn = future_poll
        .find("self.poll_once(cx)")
        .expect("locate bounded owner turn");
    let refreshed_timer = future_poll
        .rfind("self.poll_deadline_timer(cx)")
        .expect("locate final timer refresh");
    let register_recheck = future_poll
        .find("register_and_recheck")
        .expect("locate readiness register/recheck");
    assert!(
        first_timer < observed_epoch
            && observed_epoch < bounded_turn
            && bounded_turn < refreshed_timer
            && refreshed_timer < register_recheck,
        "driver poll ordering must be timer, epoch/work, bounded turn, timer refresh, register/recheck"
    );
    for required in [
        "SessionProgressResources",
        "poll_readiness_events",
        "service_cm_software",
        "service_deferred_cm_destructions",
        "handle_connection_drain_deadline",
        "service_bounded_shutdown",
        "terminal_completion_ready",
    ] {
        assert!(
            session_progress_source.contains(required),
            "{} must own `{required}`",
            session_progress_path.display()
        );
    }
    assert!(
        !session_progress_source.contains("shared.io_core"),
        "{} must report only session-owned terminal readiness",
        session_progress_path.display()
    );
    let io_progress_source =
        fs::read_to_string(&io_core_progress_path).expect("read I/O progress source");
    assert!(
        io_progress_source.contains("terminalize_operations_bounded")
            && io_progress_source.contains("fn can_finish(&self) -> bool")
            && io_progress_source.contains("terminal_complete"),
        "{} must own bounded I/O terminal eligibility",
        io_core_progress_path.display()
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
    let operation_state = io_core_operation_source
        .split("pub(in crate::v2::engine) struct OperationState {")
        .nth(1)
        .and_then(|tail| tail.split("\n}\n\nstruct OperationInner").next())
        .expect("locate OperationState fields");
    assert!(
        operation_state.contains("inner: Mutex<OperationInner>")
            && operation_state.contains("waker: AtomicWaker")
            && operation_state.contains("cancelled: AtomicBool")
            && operation_state.contains("quarantined: AtomicBool"),
        "OperationState must retain one coupled inner mutex and only independent wake/cancel/quarantine state"
    );
    let operation_inner = io_core_operation_source
        .split("struct OperationInner {")
        .nth(1)
        .and_then(|tail| tail.split("\n}\n\nenum CompletionOwnership").next())
        .expect("locate OperationInner fields");
    for coupled in [
        "lifecycle:",
        "mr:",
        "completion:",
        "output:",
        "detached:",
        "reclamation_pending:",
        "event_destination:",
    ] {
        assert!(
            operation_inner.contains(coupled),
            "OperationInner lost coupled state `{coupled}`"
        );
    }
    assert!(
        !operation_inner.contains("Atomic"),
        "coupled operation lifecycle/completion/output/resource state must not be split into atomics"
    );
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
    assert!(
        find_method_parameter_dependencies(&io_source, "IoConnection", "new", &["EngineShared"])
            .expect("parse IoConnection test constructor")
            .is_empty(),
        "IoConnection test construction must use owner-focused capabilities, not EngineShared"
    );
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

    for relocated in [
        "mod.rs",
        "cm.rs",
        "connection.rs",
        "drain.rs",
        "listener.rs",
        "registry.rs",
    ] {
        assert!(
            v2_dir
                .join("engine")
                .join("session")
                .join(relocated)
                .is_file(),
            "session relocation requires engine/session/{relocated}"
        );
    }
    for obsolete in [
        "session.rs",
        "cm.rs",
        "connection.rs",
        "drain.rs",
        "listener.rs",
    ] {
        assert!(
            !v2_dir.join("engine").join(obsolete).exists(),
            "session relocation must remove obsolete engine/{obsolete}"
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
        "SessionProgress",
        "ProgressReport",
        "IoSessionBridge",
    ] {
        assert!(
            !public_reexports.contains(internal),
            "crate-private I/O boundary type `{internal}` was publicly re-exported"
        );
    }
}
