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

struct NamedFieldAccessVisitor<'a> {
    field: &'a str,
    lines: Vec<usize>,
}

impl Visit<'_> for NamedFieldAccessVisitor<'_> {
    fn visit_expr_field(&mut self, access: &ExprField) {
        if matches!(&access.member, syn::Member::Named(member) if member == self.field) {
            self.lines.push(access.span().start().line);
        }
        visit::visit_expr_field(self, access);
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

#[derive(Default)]
struct IoEffectsPublicationAnalysis {
    definitions: usize,
    implementation_blocks: usize,
    publish_methods: Vec<(usize, bool, bool, bool)>,
    after_unlock_accesses: Vec<String>,
    violations: Vec<String>,
}

struct EffectPayloadVisitor<'a> {
    aliases: &'a HashSet<String>,
    found: bool,
}

impl Visit<'_> for EffectPayloadVisitor<'_> {
    fn visit_expr_field(&mut self, field: &ExprField) {
        if matches!(&field.member, syn::Member::Named(member) if member == "after_unlock") {
            self.found = true;
        }
        visit::visit_expr_field(self, field);
    }

    fn visit_expr_path(&mut self, path: &ExprPath) {
        if path.path.segments.len() == 1
            && self
                .aliases
                .contains(&path.path.segments[0].ident.to_string())
        {
            self.found = true;
        }
        visit::visit_expr_path(self, path);
    }
}

fn expression_contains_effect_payload(expression: &Expr, aliases: &HashSet<String>) -> bool {
    let mut visitor = EffectPayloadVisitor {
        aliases,
        found: false,
    };
    visitor.visit_expr(expression);
    visitor.found
}

struct PatternIdentifierVisitor {
    identifiers: Vec<String>,
}

impl<'ast> Visit<'ast> for PatternIdentifierVisitor {
    fn visit_pat_ident(&mut self, pattern: &'ast syn::PatIdent) {
        self.identifiers.push(pattern.ident.to_string());
        visit::visit_pat_ident(self, pattern);
    }
}

fn pattern_identifiers(pattern: &Pat) -> Vec<String> {
    let mut visitor = PatternIdentifierVisitor {
        identifiers: Vec::new(),
    };
    visitor.visit_pat(pattern);
    visitor.identifiers
}

fn pattern_destructures_io_effects(pattern: &Pat, effects_names: &HashSet<String>) -> bool {
    match pattern {
        Pat::Struct(pattern) => pattern
            .path
            .segments
            .last()
            .is_some_and(|segment| effects_names.contains(&segment.ident.to_string())),
        Pat::Type(pattern) => pattern_destructures_io_effects(&pattern.pat, effects_names),
        Pat::Reference(pattern) => pattern_destructures_io_effects(&pattern.pat, effects_names),
        Pat::Paren(pattern) => pattern_destructures_io_effects(&pattern.pat, effects_names),
        _ => false,
    }
}

struct UncheckedEffectPublicationVisitor {
    effects_names: HashSet<String>,
    aliases: HashSet<String>,
    function: String,
    violations: Vec<String>,
}

impl Visit<'_> for UncheckedEffectPublicationVisitor {
    fn visit_item_type(&mut self, alias: &syn::ItemType) {
        if type_path_last(&alias.ty).is_some_and(|name| self.effects_names.contains(&name)) {
            self.effects_names.insert(alias.ident.to_string());
            self.violations.push(format!(
                "{}:block-type-alias:{}",
                self.function,
                alias.ident.span().start().line
            ));
        }
        visit::visit_item_type(self, alias);
    }

    fn visit_item_use(&mut self, item: &syn::ItemUse) {
        let before = self.effects_names.len();
        collect_use_aliases(&item.tree, &mut self.effects_names);
        if self.effects_names.len() != before {
            self.violations.push(format!(
                "{}:block-use-alias:{}",
                self.function,
                item.span().start().line
            ));
        }
        visit::visit_item_use(self, item);
    }

    fn visit_pat_struct(&mut self, pattern: &syn::PatStruct) {
        if pattern
            .path
            .segments
            .last()
            .is_some_and(|segment| self.effects_names.contains(&segment.ident.to_string()))
        {
            self.violations.push(format!(
                "{}:destructure:{}",
                self.function,
                pattern.span().start().line
            ));
            for field in &pattern.fields {
                self.aliases.extend(pattern_identifiers(&field.pat));
            }
        }
        visit::visit_pat_struct(self, pattern);
    }

    fn visit_local(&mut self, local: &Local) {
        if pattern_destructures_io_effects(&local.pat, &self.effects_names) {
            self.violations.push(format!(
                "{}:destructure:{}",
                self.function,
                local.span().start().line
            ));
            self.aliases.extend(pattern_identifiers(&local.pat));
        }
        if local
            .init
            .as_ref()
            .is_some_and(|init| expression_contains_effect_payload(&init.expr, &self.aliases))
        {
            self.aliases.extend(pattern_identifiers(&local.pat));
        }
        visit::visit_local(self, local);
    }

    fn visit_expr_assign(&mut self, assignment: &syn::ExprAssign) {
        if expression_contains_effect_payload(&assignment.right, &self.aliases)
            && let Expr::Path(path) = assignment.left.as_ref()
            && path.path.segments.len() == 1
        {
            self.aliases.insert(path.path.segments[0].ident.to_string());
        }
        visit::visit_expr_assign(self, assignment);
    }

    fn visit_expr_method_call(&mut self, call: &ExprMethodCall) {
        if call.method == "publish"
            && expression_contains_effect_payload(&call.receiver, &self.aliases)
        {
            self.violations.push(format!(
                "{}:method-publish:{}",
                self.function,
                call.span().start().line
            ));
        }
        visit::visit_expr_method_call(self, call);
    }

    fn visit_expr_call(&mut self, call: &ExprCall) {
        if let Expr::Path(path) = call.func.as_ref()
            && path
                .path
                .segments
                .last()
                .is_some_and(|segment| segment.ident == "publish")
            && call
                .args
                .iter()
                .any(|argument| expression_contains_effect_payload(argument, &self.aliases))
        {
            self.violations.push(format!(
                "{}:ufcs-publish:{}",
                self.function,
                call.span().start().line
            ));
        }
        visit::visit_expr_call(self, call);
    }
}

fn expression_is_named_field(expression: &Expr, base: &str, field: &str) -> bool {
    matches!(
        expression,
        Expr::Field(access)
            if matches!(&access.member, syn::Member::Named(member) if member == field)
                && matches!(
                    access.base.as_ref(),
                    Expr::Path(path) if path.path.is_ident(base)
                )
    )
}

fn assert_statement_guards_named_field(statement: &syn::Stmt, base: &str, field: &str) -> bool {
    let syn::Stmt::Macro(statement) = statement else {
        return false;
    };
    if !statement.mac.path.is_ident("assert") {
        return false;
    }
    let Ok(arguments) = statement
        .mac
        .parse_body_with(Punctuated::<Expr, Token![,]>::parse_terminated)
    else {
        return false;
    };
    let Some(Expr::MethodCall(call)) = arguments.first() else {
        return false;
    };
    call.method == "is_empty"
        && call.args.is_empty()
        && expression_is_named_field(&call.receiver, base, field)
}

fn assert_statement_guards_field(statement: &syn::Stmt, field: &str) -> bool {
    assert_statement_guards_named_field(statement, "self", field)
}

fn statement_returns_self_after_unlock(statement: &syn::Stmt) -> bool {
    let syn::Stmt::Expr(expression, None) = statement else {
        return false;
    };
    expression_is_named_field(expression, "self", "after_unlock")
}

fn has_guarded_internal_effect_extraction(source: &str) -> Result<bool, syn::Error> {
    let syntax = syn::parse_file(source)?;
    let Some(function) = syntax.items.iter().find_map(|item| match item {
        Item::Fn(function) if function.sig.ident == "commit_internal_entries" => Some(function),
        _ => None,
    }) else {
        return Ok(false);
    };
    Ok(function
        .block
        .stmts
        .iter()
        .filter_map(|statement| match statement {
            syn::Stmt::Expr(Expr::ForLoop(loop_expression), _) => Some(&loop_expression.body),
            _ => None,
        })
        .any(|loop_body| {
            if loop_body.stmts.len() != 4 {
                return false;
            }
            let syn::Stmt::Local(effects_binding) = &loop_body.stmts[0] else {
                return false;
            };
            if pat_ident(&effects_binding.pat).is_none_or(|identifier| identifier != "effects")
                || effects_binding.init.is_none()
            {
                return false;
            }
            let syn::Stmt::Expr(Expr::MethodCall(extend), _) = &loop_body.stmts[3] else {
                return false;
            };
            assert_statement_guards_named_field(&loop_body.stmts[1], "effects", "quarantine")
                && assert_statement_guards_named_field(&loop_body.stmts[2], "effects", "drained")
                && extend.method == "extend"
                && matches!(
                    extend.receiver.as_ref(),
                    Expr::Path(path) if path.path.is_ident("after_unlock")
                )
                && extend.args.len() == 1
                && expression_is_named_field(&extend.args[0], "effects", "after_unlock")
        }))
}

fn analyze_io_effects_publication(
    source: &str,
) -> Result<IoEffectsPublicationAnalysis, syn::Error> {
    fn inspect_constant_expressions(
        items: &[Item],
        module_path: &mut Vec<String>,
        effects_names: &HashSet<String>,
        analysis: &mut IoEffectsPublicationAnalysis,
    ) {
        fn inspect_expression(
            expression: &Expr,
            function: String,
            effects_names: &HashSet<String>,
            analysis: &mut IoEffectsPublicationAnalysis,
        ) {
            let mut visitor = UncheckedEffectPublicationVisitor {
                effects_names: effects_names.clone(),
                aliases: HashSet::new(),
                function,
                violations: Vec::new(),
            };
            visitor.visit_expr(expression);
            analysis.violations.extend(visitor.violations);
        }

        for item in items {
            if is_test_only(item_attrs(item)) {
                continue;
            }
            match item {
                Item::Const(constant) => inspect_expression(
                    &constant.expr,
                    qualified_name(module_path, &format!("const {}", constant.ident)),
                    effects_names,
                    analysis,
                ),
                Item::Static(constant) => inspect_expression(
                    &constant.expr,
                    qualified_name(module_path, &format!("static {}", constant.ident)),
                    effects_names,
                    analysis,
                ),
                Item::Impl(implementation) => {
                    let owner = type_path_last(&implementation.self_ty)
                        .unwrap_or_else(|| "<impl>".to_owned());
                    let mut names = effects_names.clone();
                    if names.contains(&owner) {
                        names.insert("Self".to_owned());
                    }
                    for implementation_item in &implementation.items {
                        if let ImplItem::Const(constant) = implementation_item
                            && !is_test_only(&constant.attrs)
                        {
                            inspect_expression(
                                &constant.expr,
                                qualified_name(
                                    module_path,
                                    &format!("{owner}::const {}", constant.ident),
                                ),
                                &names,
                                analysis,
                            );
                        }
                    }
                }
                Item::Trait(trait_item) => {
                    let mut names = effects_names.clone();
                    names.insert("Self".to_owned());
                    for trait_member in &trait_item.items {
                        if let TraitItem::Const(constant) = trait_member
                            && !is_test_only(&constant.attrs)
                            && let Some((_, expression)) = &constant.default
                        {
                            inspect_expression(
                                expression,
                                qualified_name(
                                    module_path,
                                    &format!("{}::const {}", trait_item.ident, constant.ident),
                                ),
                                &names,
                                analysis,
                            );
                        }
                    }
                }
                Item::Mod(module) => {
                    if let Some((_, items)) = &module.content {
                        module_path.push(module.ident.to_string());
                        inspect_constant_expressions(items, module_path, effects_names, analysis);
                        module_path.pop();
                    }
                }
                _ => {}
            }
        }
    }

    // This guard intentionally reasons about ordinary parsed Rust syntax.
    // Unlike the hidden-spawn detector, it does not inspect macro token
    // streams and therefore does not claim to detect publication introduced
    // only by macro expansion.
    fn inspect_items(
        items: &[Item],
        module_path: &mut Vec<String>,
        effects_names: &HashSet<String>,
        analysis: &mut IoEffectsPublicationAnalysis,
    ) {
        for item in items {
            if is_test_only(item_attrs(item)) {
                continue;
            }
            match item {
                Item::Struct(structure) if structure.ident == "IoCoreEffects" => {
                    analysis.definitions += 1;
                }
                Item::Type(alias)
                    if type_path_last(&alias.ty)
                        .is_some_and(|name| effects_names.contains(&name)) =>
                {
                    analysis.violations.push(format!(
                        "{}:type-alias:{}",
                        qualified_name(module_path, &alias.ident.to_string()),
                        alias.ident.span().start().line
                    ));
                }
                Item::Fn(function) => {
                    let mut visitor = UncheckedEffectPublicationVisitor {
                        effects_names: effects_names.clone(),
                        aliases: HashSet::new(),
                        function: qualified_name(module_path, &function.sig.ident.to_string()),
                        violations: Vec::new(),
                    };
                    visitor.visit_signature(&function.sig);
                    visitor.visit_block(&function.block);
                    analysis.violations.extend(visitor.violations);
                    let mut field_visitor = NamedFieldAccessVisitor {
                        field: "after_unlock",
                        lines: Vec::new(),
                    };
                    field_visitor.visit_block(&function.block);
                    analysis
                        .after_unlock_accesses
                        .extend(field_visitor.lines.into_iter().map(|line| {
                            format!(
                                "{}:{line}",
                                qualified_name(module_path, &function.sig.ident.to_string())
                            )
                        }));
                }
                Item::Impl(implementation)
                    if type_path_last(&implementation.self_ty)
                        .is_some_and(|name| effects_names.contains(&name)) =>
                {
                    analysis.implementation_blocks += 1;
                    if implementation.trait_.is_some() {
                        analysis.violations.push(format!(
                            "{}:trait-impl:{}",
                            qualified_name(module_path, "IoCoreEffects"),
                            implementation.impl_token.span.start().line
                        ));
                    }
                    for implementation_item in &implementation.items {
                        let ImplItem::Fn(function) = implementation_item else {
                            continue;
                        };
                        let name = function.sig.ident.to_string();
                        let function_name =
                            qualified_name(module_path, &format!("IoCoreEffects::{name}"));
                        let mut field_visitor = NamedFieldAccessVisitor {
                            field: "after_unlock",
                            lines: Vec::new(),
                        };
                        field_visitor.visit_block(&function.block);
                        analysis.after_unlock_accesses.extend(
                            field_visitor
                                .lines
                                .into_iter()
                                .map(|line| format!("{function_name}:{line}")),
                        );
                        if implementation.trait_.is_none() && name == "into_after_unlock" {
                            let statements = &function.block.stmts;
                            analysis.publish_methods.push((
                                function.sig.ident.span().start().line,
                                statements.len() == 3
                                    && assert_statement_guards_field(&statements[0], "quarantine"),
                                statements.len() == 3
                                    && assert_statement_guards_field(&statements[1], "drained"),
                                statements.len() == 3
                                    && statement_returns_self_after_unlock(&statements[2]),
                            ));
                            continue;
                        }
                        if implementation.trait_.is_none() && name == "publish" {
                            analysis.violations.push(format!(
                                "{}:broad-publish:{}",
                                function_name,
                                function.sig.ident.span().start().line
                            ));
                        }
                        if implementation.trait_.is_none() && name != "extend" {
                            let mut field_visitor = NamedFieldAccessVisitor {
                                field: "after_unlock",
                                lines: Vec::new(),
                            };
                            field_visitor.visit_block(&function.block);
                            if !field_visitor.lines.is_empty() {
                                analysis.violations.push(format!(
                                    "{}:payload-extractor:{}",
                                    function_name,
                                    function.sig.ident.span().start().line
                                ));
                            }
                        }
                        let mut effect_names = effects_names.clone();
                        effect_names.insert("Self".to_owned());
                        let mut visitor = UncheckedEffectPublicationVisitor {
                            effects_names: effect_names,
                            aliases: HashSet::new(),
                            function: function_name,
                            violations: Vec::new(),
                        };
                        visitor.visit_signature(&function.sig);
                        visitor.visit_block(&function.block);
                        analysis.violations.extend(visitor.violations);
                    }
                }
                Item::Impl(implementation) => {
                    let owner = type_path_last(&implementation.self_ty)
                        .unwrap_or_else(|| "<impl>".to_owned());
                    for implementation_item in &implementation.items {
                        let ImplItem::Fn(function) = implementation_item else {
                            continue;
                        };
                        if matches!(
                            owner.as_str(),
                            "CommittedIoCoreEffects" | "DetachedIoCoreEffects"
                        ) && function.sig.ident == "publish"
                        {
                            continue;
                        }
                        if is_test_only(&function.attrs) {
                            continue;
                        }
                        let mut visitor = UncheckedEffectPublicationVisitor {
                            effects_names: effects_names.clone(),
                            aliases: HashSet::new(),
                            function: qualified_name(
                                module_path,
                                &format!("{owner}::{}", function.sig.ident),
                            ),
                            violations: Vec::new(),
                        };
                        visitor.visit_signature(&function.sig);
                        visitor.visit_block(&function.block);
                        analysis.violations.extend(visitor.violations);
                        let function_name = qualified_name(
                            module_path,
                            &format!("{owner}::{}", function.sig.ident),
                        );
                        let mut field_visitor = NamedFieldAccessVisitor {
                            field: "after_unlock",
                            lines: Vec::new(),
                        };
                        field_visitor.visit_block(&function.block);
                        analysis.after_unlock_accesses.extend(
                            field_visitor
                                .lines
                                .into_iter()
                                .map(|line| format!("{function_name}:{line}")),
                        );
                    }
                }
                Item::Mod(module) => {
                    if let Some((_, items)) = &module.content {
                        module_path.push(module.ident.to_string());
                        inspect_items(items, module_path, effects_names, analysis);
                        module_path.pop();
                    }
                }
                _ => {}
            }
        }
    }

    let syntax = syn::parse_file(source)?;
    let effects_names = identifiers_and_aliases(&syntax, &["IoCoreEffects"]);
    let mut analysis = IoEffectsPublicationAnalysis::default();
    inspect_items(
        &syntax.items,
        &mut Vec::new(),
        &effects_names,
        &mut analysis,
    );
    inspect_constant_expressions(
        &syntax.items,
        &mut Vec::new(),
        &effects_names,
        &mut analysis,
    );
    analysis.after_unlock_accesses.sort();
    analysis.violations.sort();
    analysis.violations.dedup();
    Ok(analysis)
}

fn find_effect_boundary_constant_bypasses(
    source: &str,
    known_types: &HashSet<String>,
) -> Result<Vec<String>, syn::Error> {
    fn inspect_expression(
        expression: &Expr,
        location: String,
        names: &HashSet<String>,
        violations: &mut Vec<String>,
    ) {
        let mut publication = UncheckedEffectPublicationVisitor {
            effects_names: names.clone(),
            aliases: HashSet::new(),
            function: location.clone(),
            violations: Vec::new(),
        };
        publication.visit_expr(expression);
        violations.extend(publication.violations);

        let mut field_access = NamedFieldAccessVisitor {
            field: "after_unlock",
            lines: Vec::new(),
        };
        field_access.visit_expr(expression);
        violations.extend(
            field_access
                .lines
                .into_iter()
                .map(|line| format!("{location}:after-unlock-field:{line}")),
        );
    }

    fn inspect_items(
        items: &[Item],
        module_path: &mut Vec<String>,
        known_types: &HashSet<String>,
        violations: &mut Vec<String>,
    ) {
        for item in items {
            if is_test_only(item_attrs(item)) {
                continue;
            }
            match item {
                Item::Const(constant) => inspect_expression(
                    &constant.expr,
                    qualified_name(module_path, &format!("const {}", constant.ident)),
                    known_types,
                    violations,
                ),
                Item::Static(constant) => inspect_expression(
                    &constant.expr,
                    qualified_name(module_path, &format!("static {}", constant.ident)),
                    known_types,
                    violations,
                ),
                Item::Impl(implementation) => {
                    let owner = type_path_last(&implementation.self_ty)
                        .unwrap_or_else(|| "<impl>".to_owned());
                    let mut names = known_types.clone();
                    if names.contains(&owner) {
                        names.insert("Self".to_owned());
                    }
                    for implementation_item in &implementation.items {
                        if let ImplItem::Const(constant) = implementation_item
                            && !is_test_only(&constant.attrs)
                        {
                            inspect_expression(
                                &constant.expr,
                                qualified_name(
                                    module_path,
                                    &format!("{owner}::const {}", constant.ident),
                                ),
                                &names,
                                violations,
                            );
                        }
                    }
                }
                Item::Trait(trait_item) => {
                    let mut names = known_types.clone();
                    names.insert("Self".to_owned());
                    for trait_member in &trait_item.items {
                        if let TraitItem::Const(constant) = trait_member
                            && !is_test_only(&constant.attrs)
                            && let Some((_, expression)) = &constant.default
                        {
                            inspect_expression(
                                expression,
                                qualified_name(
                                    module_path,
                                    &format!("{}::const {}", trait_item.ident, constant.ident),
                                ),
                                &names,
                                violations,
                            );
                        }
                    }
                }
                Item::Mod(module) => {
                    if let Some((_, items)) = &module.content {
                        module_path.push(module.ident.to_string());
                        inspect_items(items, module_path, known_types, violations);
                        module_path.pop();
                    }
                }
                _ => {}
            }
        }
    }

    let syntax = syn::parse_file(source)?;
    let mut violations = Vec::new();
    inspect_items(&syntax.items, &mut Vec::new(), known_types, &mut violations);
    violations.sort();
    violations.dedup();
    Ok(violations)
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

struct ZeroArgumentMethodCallVisitor<'a> {
    method: &'a str,
    calls: Vec<String>,
    function: &'a str,
}

impl Visit<'_> for ZeroArgumentMethodCallVisitor<'_> {
    fn visit_expr_method_call(&mut self, call: &ExprMethodCall) {
        if call.method == self.method && call.args.is_empty() {
            self.calls
                .push(format!("{}:{}", self.function, call.span().start().line));
        }
        visit::visit_expr_method_call(self, call);
    }
}

fn find_production_zero_argument_method_calls(
    source: &str,
    method: &str,
) -> Result<Vec<String>, syn::Error> {
    fn inspect_items(
        items: &[Item],
        module_path: &mut Vec<String>,
        method: &str,
        calls: &mut Vec<String>,
    ) {
        for item in items {
            if is_test_only(item_attrs(item)) {
                continue;
            }
            match item {
                Item::Fn(function) => {
                    let name = qualified_name(module_path, &function.sig.ident.to_string());
                    let mut visitor = ZeroArgumentMethodCallVisitor {
                        method,
                        calls: Vec::new(),
                        function: &name,
                    };
                    visitor.visit_block(&function.block);
                    calls.extend(visitor.calls);
                }
                Item::Const(constant) => {
                    let name = qualified_name(module_path, &format!("const {}", constant.ident));
                    let mut visitor = ZeroArgumentMethodCallVisitor {
                        method,
                        calls: Vec::new(),
                        function: &name,
                    };
                    visitor.visit_expr(&constant.expr);
                    calls.extend(visitor.calls);
                }
                Item::Static(constant) => {
                    let name = qualified_name(module_path, &format!("static {}", constant.ident));
                    let mut visitor = ZeroArgumentMethodCallVisitor {
                        method,
                        calls: Vec::new(),
                        function: &name,
                    };
                    visitor.visit_expr(&constant.expr);
                    calls.extend(visitor.calls);
                }
                Item::Impl(implementation) => {
                    let owner = type_path_last(&implementation.self_ty)
                        .unwrap_or_else(|| "<impl>".to_owned());
                    for implementation_item in &implementation.items {
                        match implementation_item {
                            ImplItem::Fn(function) if !is_test_only(&function.attrs) => {
                                let name = qualified_name(
                                    module_path,
                                    &format!("{owner}::{}", function.sig.ident),
                                );
                                let mut visitor = ZeroArgumentMethodCallVisitor {
                                    method,
                                    calls: Vec::new(),
                                    function: &name,
                                };
                                visitor.visit_block(&function.block);
                                calls.extend(visitor.calls);
                            }
                            ImplItem::Const(constant) if !is_test_only(&constant.attrs) => {
                                let name = qualified_name(
                                    module_path,
                                    &format!("{owner}::const {}", constant.ident),
                                );
                                let mut visitor = ZeroArgumentMethodCallVisitor {
                                    method,
                                    calls: Vec::new(),
                                    function: &name,
                                };
                                visitor.visit_expr(&constant.expr);
                                calls.extend(visitor.calls);
                            }
                            _ => {}
                        }
                    }
                }
                Item::Trait(trait_item) => {
                    for trait_member in &trait_item.items {
                        let TraitItem::Fn(function) = trait_member else {
                            if let TraitItem::Const(constant) = trait_member
                                && !is_test_only(&constant.attrs)
                                && let Some((_, expression)) = &constant.default
                            {
                                let name = qualified_name(
                                    module_path,
                                    &format!("{}::const {}", trait_item.ident, constant.ident),
                                );
                                let mut visitor = ZeroArgumentMethodCallVisitor {
                                    method,
                                    calls: Vec::new(),
                                    function: &name,
                                };
                                visitor.visit_expr(expression);
                                calls.extend(visitor.calls);
                            }
                            continue;
                        };
                        if !is_test_only(&function.attrs)
                            && let Some(block) = &function.default
                        {
                            let name = qualified_name(
                                module_path,
                                &format!("{}::{}", trait_item.ident, function.sig.ident),
                            );
                            let mut visitor = ZeroArgumentMethodCallVisitor {
                                method,
                                calls: Vec::new(),
                                function: &name,
                            };
                            visitor.visit_block(block);
                            calls.extend(visitor.calls);
                        }
                    }
                }
                Item::Mod(module) => {
                    if let Some((_, items)) = &module.content {
                        module_path.push(module.ident.to_string());
                        inspect_items(items, module_path, method, calls);
                        module_path.pop();
                    }
                }
                _ => {}
            }
        }
    }

    let syntax = syn::parse_file(source)?;
    let mut calls = Vec::new();
    inspect_items(&syntax.items, &mut Vec::new(), method, &mut calls);
    Ok(calls)
}

struct RestrictedFreeFunctionReferenceVisitor<'a> {
    function: &'a str,
    references: Vec<usize>,
}

impl Visit<'_> for RestrictedFreeFunctionReferenceVisitor<'_> {
    fn visit_expr_call(&mut self, call: &ExprCall) {
        if matches!(
            call.func.as_ref(),
            Expr::Path(path)
                if path.path.segments.last().is_some_and(
                    |segment| segment.ident == self.function
                )
        ) {
            for argument in &call.args {
                self.visit_expr(argument);
            }
            return;
        }
        visit::visit_expr_call(self, call);
    }

    fn visit_expr_path(&mut self, path: &ExprPath) {
        if path
            .path
            .segments
            .last()
            .is_some_and(|segment| segment.ident == self.function)
        {
            self.references.push(path.span().start().line);
        }
        visit::visit_expr_path(self, path);
    }
}

fn find_restricted_free_function_references(
    source: &str,
    function: &str,
) -> Result<Vec<usize>, syn::Error> {
    let syntax = syn::parse_file(source)?;
    let mut visitor = RestrictedFreeFunctionReferenceVisitor {
        function,
        references: Vec::new(),
    };
    visitor.visit_file(&syntax);
    Ok(visitor.references)
}

fn find_restricted_free_function_use_aliases(
    source: &str,
    function: &str,
) -> Result<Vec<String>, syn::Error> {
    struct UseAliasCollector<'a> {
        names: &'a mut HashSet<String>,
    }

    impl Visit<'_> for UseAliasCollector<'_> {
        fn visit_item_use(&mut self, item: &syn::ItemUse) {
            collect_use_aliases(&item.tree, self.names);
            visit::visit_item_use(self, item);
        }
    }

    let syntax = syn::parse_file(source)?;
    let mut names = HashSet::from([function.to_owned()]);
    loop {
        let before = names.len();
        UseAliasCollector { names: &mut names }.visit_file(&syntax);
        if names.len() == before {
            break;
        }
    }
    names.remove(function);
    let mut aliases = names.into_iter().collect::<Vec<_>>();
    aliases.sort();
    Ok(aliases)
}

fn type_mentions_any(ty: &Type, names: &HashSet<String>) -> bool {
    struct TypeNameVisitor<'a> {
        names: &'a HashSet<String>,
        found: bool,
    }

    impl Visit<'_> for TypeNameVisitor<'_> {
        fn visit_type_path(&mut self, path: &syn::TypePath) {
            if path
                .path
                .segments
                .iter()
                .any(|segment| self.names.contains(&segment.ident.to_string()))
            {
                self.found = true;
            }
            visit::visit_type_path(self, path);
        }
    }

    let mut visitor = TypeNameVisitor {
        names,
        found: false,
    };
    visitor.visit_type(ty);
    visitor.found
}

fn type_alias_mentions_any(alias: &syn::ItemType, names: &HashSet<String>) -> bool {
    type_mentions_any(&alias.ty, names)
        || alias.generics.params.iter().any(|parameter| {
            matches!(
                parameter,
                syn::GenericParam::Type(parameter)
                    if parameter
                        .default
                        .as_ref()
                        .is_some_and(|(_, default)| type_mentions_any(default, names))
            )
        })
}

fn effect_owner_path(
    path: &ExprPath,
    effect_types: &HashSet<String>,
    self_types: &[String],
) -> bool {
    if let Some(qself) = &path.qself {
        if type_mentions_any(&qself.ty, effect_types) {
            return true;
        }
        if type_path_last(&qself.ty).as_deref() == Some("Self") {
            return self_types
                .last()
                .is_none_or(|owner| effect_types.contains(owner));
        }
    }
    let mut segments = path.path.segments.iter().rev();
    segments.next();
    let Some(owner) = segments.next() else {
        return false;
    };
    if owner.ident == "Self" {
        return self_types
            .last()
            .is_none_or(|concrete| effect_types.contains(concrete));
    }
    if effect_types.contains(&owner.ident.to_string()) {
        return true;
    }
    let syn::PathArguments::AngleBracketed(arguments) = &owner.arguments else {
        return false;
    };
    arguments.args.iter().any(|argument| {
        matches!(
            argument,
            syn::GenericArgument::Type(ty) if type_mentions_any(ty, effect_types)
        )
    })
}

struct EffectUfcsPublishVisitor {
    effect_types: HashSet<String>,
    calls: Vec<usize>,
    self_types: Vec<String>,
}

impl Visit<'_> for EffectUfcsPublishVisitor {
    fn visit_expr_call(&mut self, call: &ExprCall) {
        if let Expr::Path(path) = call.func.as_ref()
            && path
                .path
                .segments
                .last()
                .is_some_and(|segment| segment.ident == "publish")
            && effect_owner_path(path, &self.effect_types, &self.self_types)
        {
            self.calls.push(call.span().start().line);
        }
        visit::visit_expr_call(self, call);
    }

    fn visit_item_impl(&mut self, implementation: &syn::ItemImpl) {
        self.self_types
            .push(type_path_last(&implementation.self_ty).unwrap_or_else(|| "<impl>".to_owned()));
        visit::visit_item_impl(self, implementation);
        self.self_types.pop();
    }
}

fn find_effect_ufcs_publications(source: &str) -> Result<Vec<usize>, syn::Error> {
    let syntax = syn::parse_file(source)?;
    struct AliasCollector<'a> {
        names: &'a mut HashSet<String>,
        changed: bool,
    }

    impl Visit<'_> for AliasCollector<'_> {
        fn visit_item_type(&mut self, alias: &syn::ItemType) {
            if type_alias_mentions_any(alias, self.names) {
                self.changed |= self.names.insert(alias.ident.to_string());
            }
            visit::visit_item_type(self, alias);
        }

        fn visit_item_use(&mut self, item: &syn::ItemUse) {
            let before = self.names.len();
            collect_use_aliases(&item.tree, self.names);
            self.changed |= self.names.len() != before;
            visit::visit_item_use(self, item);
        }
    }

    let mut effect_types = [
        "IoCoreEffects",
        "AfterEngineUnlock",
        "DetachedIoCoreEffects",
        "CommittedIoCoreEffects",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<HashSet<_>>();
    loop {
        let mut collector = AliasCollector {
            names: &mut effect_types,
            changed: false,
        };
        collector.visit_file(&syntax);
        if !collector.changed {
            break;
        }
    }
    let mut visitor = EffectUfcsPublishVisitor {
        effect_types,
        calls: Vec::new(),
        self_types: Vec::new(),
    };
    visitor.visit_file(&syntax);
    Ok(visitor.calls)
}

struct RestrictedEffectFunctionVisitor {
    effect_types: HashSet<String>,
    session_types: HashSet<String>,
    references: Vec<usize>,
    self_types: Vec<String>,
}

impl Visit<'_> for RestrictedEffectFunctionVisitor {
    fn visit_expr_path(&mut self, expression: &ExprPath) {
        let method = expression
            .path
            .segments
            .last()
            .map(|segment| segment.ident.to_string());
        if matches!(
            method.as_deref(),
            Some(
                "publish"
                    | "take_quarantine"
                    | "take_drained"
                    | "into_after_unlock"
                    | "into_committed"
                    | "apply_terminal_io_effects"
            )
        ) {
            let owner_types = if method.as_deref() == Some("apply_terminal_io_effects") {
                &self.session_types
            } else {
                &self.effect_types
            };
            if effect_owner_path(expression, owner_types, &self.self_types) {
                self.references.push(expression.span().start().line);
            }
        }
        visit::visit_expr_path(self, expression);
    }

    fn visit_item_impl(&mut self, implementation: &syn::ItemImpl) {
        self.self_types
            .push(type_path_last(&implementation.self_ty).unwrap_or_else(|| "<impl>".to_owned()));
        visit::visit_item_impl(self, implementation);
        self.self_types.pop();
    }
}

fn expand_type_aliases(source: &str, names: &mut HashSet<String>) -> Result<bool, syn::Error> {
    let syntax = syn::parse_file(source)?;
    struct AliasCollector<'a> {
        names: &'a mut HashSet<String>,
        changed: bool,
    }

    impl Visit<'_> for AliasCollector<'_> {
        fn visit_item_type(&mut self, alias: &syn::ItemType) {
            if type_alias_mentions_any(alias, self.names) {
                self.changed |= self.names.insert(alias.ident.to_string());
            }
            visit::visit_item_type(self, alias);
        }

        fn visit_item_use(&mut self, item: &syn::ItemUse) {
            let before = self.names.len();
            collect_use_aliases(&item.tree, self.names);
            self.changed |= self.names.len() != before;
            visit::visit_item_use(self, item);
        }
    }

    let mut changed_any = false;
    loop {
        let mut collector = AliasCollector {
            names,
            changed: false,
        };
        collector.visit_file(&syntax);
        if !collector.changed {
            break;
        }
        changed_any = true;
    }
    Ok(changed_any)
}

fn base_effect_boundary_types() -> HashSet<String> {
    [
        "IoCoreEffects",
        "AfterEngineUnlock",
        "DetachedIoCoreEffects",
        "CommittedIoCoreEffects",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn find_restricted_effect_function_references_with_types(
    source: &str,
    known_types: &HashSet<String>,
    known_session_types: &HashSet<String>,
) -> Result<Vec<usize>, syn::Error> {
    let syntax = syn::parse_file(source)?;
    let mut effect_types = known_types.clone();
    expand_type_aliases(source, &mut effect_types)?;
    let mut session_types = known_session_types.clone();
    expand_type_aliases(source, &mut session_types)?;
    let mut visitor = RestrictedEffectFunctionVisitor {
        effect_types,
        session_types,
        references: Vec::new(),
        self_types: Vec::new(),
    };
    visitor.visit_file(&syntax);
    Ok(visitor.references)
}

fn find_restricted_effect_function_references(source: &str) -> Result<Vec<usize>, syn::Error> {
    find_restricted_effect_function_references_with_types(
        source,
        &base_effect_boundary_types(),
        &HashSet::from(["SessionManager".to_owned()]),
    )
}

fn find_effect_trait_impls(
    source: &str,
    known_types: &HashSet<String>,
) -> Result<Vec<String>, syn::Error> {
    fn inspect(
        items: &[Item],
        module_path: &mut Vec<String>,
        known_types: &HashSet<String>,
        found: &mut Vec<String>,
    ) {
        for item in items {
            if is_test_only(item_attrs(item)) {
                continue;
            }
            match item {
                Item::Impl(implementation)
                    if implementation.trait_.is_some()
                        && type_mentions_any(&implementation.self_ty, known_types) =>
                {
                    found.push(qualified_name(
                        module_path,
                        &type_path_last(&implementation.self_ty)
                            .unwrap_or_else(|| "<effect>".to_owned()),
                    ));
                }
                Item::Mod(module) => {
                    if let Some((_, items)) = &module.content {
                        module_path.push(module.ident.to_string());
                        inspect(items, module_path, known_types, found);
                        module_path.pop();
                    }
                }
                _ => {}
            }
        }
    }

    let syntax = syn::parse_file(source)?;
    let mut found = Vec::new();
    inspect(&syntax.items, &mut Vec::new(), known_types, &mut found);
    Ok(found)
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

fn source_is_test_only(source: &str) -> Result<bool, syn::Error> {
    Ok(is_test_only(&syn::parse_file(source)?.attrs))
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
    if is_test_only(&syntax.attrs) {
        return Vec::new();
    }
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

    let test_file = r#"
#![cfg(test)]

fn allowed_in_a_test_only_file() {
    tokio::spawn(async {});
}
"#;
    assert!(
        find_spawn_violations(test_file).is_empty(),
        "a recursively discovered test-only source must retain its file-level cfg context"
    );

    let feature_file = r#"
#![cfg(any(test, feature = "test-hooks"))]

fn reachable_with_test_hooks() {
    tokio::spawn(async {});
}
"#;
    assert_eq!(
        find_spawn_violations(feature_file).len(),
        1,
        "a test-hooks source remains production-reachable and must be scanned"
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
fn io_effect_publication_detector_rejects_unchecked_routes() {
    let guarded = r#"
        struct IoCoreEffects {
            after_unlock: AfterEngineUnlock,
            quarantine: Vec<Effect>,
            drained: Vec<Token>,
        }
        impl IoCoreEffects {
            fn into_after_unlock(self) -> AfterEngineUnlock {
                assert!(self.quarantine.is_empty());
                assert!(self.drained.is_empty());
                self.after_unlock
            }
        }
    "#;
    let guarded = analyze_io_effects_publication(guarded).unwrap();
    assert_eq!(guarded.publish_methods.len(), 1);
    let (_, quarantine_guarded, drained_guarded, detached_publish) = guarded.publish_methods[0];
    assert!(quarantine_guarded && drained_guarded && detached_publish);
    assert!(guarded.violations.is_empty(), "{:#?}", guarded.violations);

    let broad_publish = r#"
        struct IoCoreEffects {
            after_unlock: AfterEngineUnlock,
            quarantine: Vec<Effect>,
            drained: Vec<Token>,
        }
        impl IoCoreEffects {
            fn publish(self) {
                assert!(self.quarantine.is_empty());
                assert!(self.drained.is_empty());
                self.after_unlock.publish();
            }
        }
    "#;
    assert!(
        analyze_io_effects_publication(broad_publish)
            .unwrap()
            .violations
            .iter()
            .any(|violation| violation.contains("broad-publish")),
        "a broad IoCoreEffects publication surface must be rejected"
    );

    let nested_bypass = r#"
        struct IoCoreEffects { after_unlock: AfterEngineUnlock }
        mod nested {
            impl IoCoreEffects {
                fn bypass(self) { self.after_unlock.publish(); }
            }
        }
    "#;
    assert!(
        analyze_io_effects_publication(nested_bypass)
            .unwrap()
            .violations
            .iter()
            .any(|violation| violation.contains("nested::IoCoreEffects::bypass")),
        "a publication bypass in a nested child module must be rejected"
    );
    assert_eq!(
        find_production_zero_argument_method_calls(
            "mod nested { fn bypass(value: CommittedIoCoreEffects) { value.publish(); } }",
            "publish",
        )
        .unwrap(),
        ["nested::bypass:1"],
        "nested committed/detached publication calls must be visible to the route allowlist"
    );
    let nested_ufcs = r#"
        type Direct = AfterEngineUnlock;
        mod nested {
            fn bypass(
                direct: Direct,
                detached: DetachedIoCoreEffects,
                committed: CommittedIoCoreEffects,
            ) {
                Direct::publish(direct);
                DetachedIoCoreEffects::publish(detached);
                <CommittedIoCoreEffects>::publish(committed);
            }
        }
    "#;
    assert_eq!(
        find_effect_ufcs_publications(nested_ufcs).unwrap().len(),
        3,
        "nested UFCS publication must be detected for every effect publication type and alias"
    );
    let block_alias_ufcs = r#"
        fn bypass(effect: AfterEngineUnlock) {
            type LocalEffect = AfterEngineUnlock;
            LocalEffect::publish(effect);
        }
        fn bypass_use(effect: DetachedIoCoreEffects) {
            use crate::DetachedIoCoreEffects as LocalEffect;
            LocalEffect::publish(effect);
        }
    "#;
    assert_eq!(
        find_effect_ufcs_publications(block_alias_ufcs)
            .unwrap()
            .len(),
        2,
        "function-local type and use aliases must not hide effect UFCS publication"
    );
    assert_eq!(
        find_effect_ufcs_publications(
            "fn bypass(effect: AfterEngineUnlock) { Local::publish(effect); type Local = AfterEngineUnlock; }",
        )
        .unwrap()
        .len(),
        1,
        "a block-local alias declared after its use must not hide effect UFCS publication"
    );
    let self_ufcs = r#"
        struct CommittedIoCoreEffects;
        impl CommittedIoCoreEffects {
            fn bypass(self) { Self::publish(self); }
        }
        struct DetachedIoCoreEffects;
        impl DetachedIoCoreEffects {
            fn bypass(self) { <Self>::publish(self); }
        }
    "#;
    assert_eq!(
        find_effect_ufcs_publications(self_ufcs).unwrap().len(),
        2,
        "Self-qualified UFCS publication must be detected in effect-type implementations"
    );
    let function_item_aliases = r#"
        fn bypass(
            full: &mut IoCoreEffects,
            owned: IoCoreEffects,
            detached: DetachedIoCoreEffects,
            committed: CommittedIoCoreEffects,
            manager: &SessionManager,
        ) {
            let take_quarantine = IoCoreEffects::take_quarantine;
            let take_drained = IoCoreEffects::take_drained;
            let extract = FullEffects::into_after_unlock;
            let publish_detached = DetachedAlias::publish;
            let publish_committed = CommittedIoCoreEffects::publish;
            let terminal_apply = SessionAlias::apply_terminal_io_effects;
            type DetachedAlias = DetachedIoCoreEffects;
            type FullEffects = IoCoreEffects;
            type SessionAlias = SessionManager;
            take_quarantine(full);
            take_drained(full);
            extract(owned);
            publish_detached(detached);
            publish_committed(committed);
            terminal_apply(manager, IoCoreEffects::default());
        }
    "#;
    assert_eq!(
        find_restricted_effect_function_references(function_item_aliases)
            .unwrap()
            .len(),
        6,
        "restricted effect associated methods must not escape through function-item aliases"
    );
    let self_function_items = r#"
        struct IoCoreEffects;
        impl IoCoreEffects {
            fn bypass(self) {
                let extract = Self::into_after_unlock;
                extract(self);
            }
        }
        struct SessionManager;
        impl SessionManager {
            fn bypass(&self, effects: IoCoreEffects) {
                let apply = Self::apply_terminal_io_effects;
                apply(self, effects);
            }
        }
    "#;
    assert_eq!(
        find_restricted_effect_function_references(self_function_items)
            .unwrap()
            .len(),
        2,
        "Self-qualified extraction and terminal-application function items must be detected"
    );
    let mut cross_file_types = base_effect_boundary_types();
    assert!(
        expand_type_aliases(
            "pub(super) type CrossFileEffects = CommittedIoCoreEffects;",
            &mut cross_file_types,
        )
        .unwrap()
    );
    assert_eq!(
        find_restricted_effect_function_references_with_types(
            "fn bypass(effect: CrossFileEffects) { let publish = CrossFileEffects::publish; publish(effect); }",
            &cross_file_types,
            &HashSet::from(["SessionManager".to_owned()]),
        )
        .unwrap()
        .len(),
        1,
        "cross-file effect aliases must not hide function-item publication"
    );
    let generic_alias = r#"
        type Identity<T> = T;
        type Hidden = Identity<CommittedIoCoreEffects>;
        fn bypass(effect: Hidden) {
            let publish = Hidden::publish;
            publish(effect);
        }
    "#;
    assert_eq!(
        find_restricted_effect_function_references(generic_alias)
            .unwrap()
            .len(),
        1,
        "generic identity aliases must not hide effect function items"
    );
    let generic_default_alias = r#"
        type Hidden<T = CommittedIoCoreEffects> = T;
        fn bypass(effect: Hidden) {
            let publish = Hidden::publish;
            publish(effect);
        }
    "#;
    assert_eq!(
        find_restricted_effect_function_references(generic_default_alias)
            .unwrap()
            .len(),
        1,
        "generic defaults must not hide effect function items"
    );
    assert_eq!(
        find_effect_ufcs_publications("trait Escape { fn bypass(self) { Self::publish(self); } }",)
            .unwrap()
            .len(),
        1,
        "Self-qualified publication in a default trait method must be rejected"
    );
    assert_eq!(
        find_effect_trait_impls(
            "trait Escape {} impl Escape for CommittedIoCoreEffects {}",
            &base_effect_boundary_types(),
        )
        .unwrap()
        .len(),
        1,
        "effect boundary types must not acquire alternate trait routes"
    );
    assert_eq!(
        find_restricted_free_function_references(
            "fn allowed(a: A, b: B, e: E) { publish_after_post_guards(a, b, e); }\nfn bypass() { let call = publish_after_post_guards; }",
            "publish_after_post_guards",
        )
        .unwrap(),
        [2],
        "the post-guard publication helper must not escape through a function-item alias"
    );
    assert_eq!(
        find_restricted_free_function_use_aliases(
            "fn bypass() { use self::publish_after_post_guards as call; call(); }",
            "publish_after_post_guards",
        )
        .unwrap(),
        ["call"],
        "use aliases must not hide the post-guard publication helper"
    );
    let self_destructure = r#"
        struct IoCoreEffects {
            after_unlock: AfterEngineUnlock,
            quarantine: Vec<Effect>,
            drained: Vec<Token>,
        }
        impl IoCoreEffects {
            fn bypass(self) {
                let Self { after_unlock, .. } = self;
                after_unlock.publish();
            }
        }
    "#;
    assert!(
        analyze_io_effects_publication(self_destructure)
            .unwrap()
            .violations
            .iter()
            .any(|violation| violation.contains("destructure")),
        "Self destructuring inside IoCoreEffects must not bypass payload confinement"
    );
    let constant_bypass = r#"
        struct IoCoreEffects {
            after_unlock: AfterEngineUnlock,
            quarantine: Vec<Effect>,
            drained: Vec<Token>,
        }
        const BYPASS: fn(IoCoreEffects) = |effects| {
            let IoCoreEffects { after_unlock, .. } = effects;
            after_unlock.publish();
        };
        static STATIC_BYPASS: fn(IoCoreEffects) = |effects| {
            let IoCoreEffects { after_unlock, .. } = effects;
            after_unlock.publish();
        };
    "#;
    let constant_analysis = analyze_io_effects_publication(constant_bypass).unwrap();
    assert!(
        constant_analysis
            .violations
            .iter()
            .filter(|violation| violation.contains("destructure"))
            .count()
            == 2,
        "const and static initializers must be inspected for effect destructuring"
    );
    assert_eq!(
        find_production_zero_argument_method_calls(constant_bypass, "publish")
            .unwrap()
            .len(),
        2,
        "const and static initializers must be included in publication inventory"
    );
    let wrapper_const_bypass = r#"
        struct CommittedIoCoreEffects { after_unlock: AfterEngineUnlock }
        impl CommittedIoCoreEffects {
            const BYPASS: fn(Self) -> AfterEngineUnlock = |value| {
                let Self { after_unlock } = value;
                after_unlock
            };
        }
        trait Escape {
            const BYPASS: fn(Self) = |value| {
                let Self { after_unlock } = value;
                after_unlock.publish();
            };
        }
    "#;
    assert!(
        find_effect_boundary_constant_bypasses(wrapper_const_bypass, &base_effect_boundary_types(),)
            .unwrap()
            .len() >= 2,
        "impl and trait associated constants must not extract effect payloads through Self"
    );
    let mut constant_aliases = base_effect_boundary_types();
    expand_type_aliases(
        "type Identity<T> = T; type Hidden<T = CommittedIoCoreEffects> = T;",
        &mut constant_aliases,
    )
    .unwrap();
    assert!(
        !find_effect_boundary_constant_bypasses(
            "const BYPASS: fn(Hidden) = |value| { let Hidden { after_unlock } = value; after_unlock.publish(); };",
            &constant_aliases,
        )
        .unwrap()
        .is_empty(),
        "generic and cross-file aliases must not hide constant effect extraction"
    );
    assert_eq!(
        find_production_zero_argument_method_calls(
            "trait Publisher { fn bypass(value: CommittedIoCoreEffects) { value.publish(); } }",
            "publish",
        )
        .unwrap(),
        ["Publisher::bypass:1"],
        "default trait-method publication must be visible to the exact route allowlist"
    );

    let additional_inherent_method = r#"
        struct IoCoreEffects { after_unlock: AfterEngineUnlock }
        impl IoCoreEffects {
            fn into_after_unlock(self) -> AfterEngineUnlock {
                assert!(self.quarantine.is_empty());
                assert!(self.drained.is_empty());
                self.after_unlock
            }
            fn bypass(self) { self.after_unlock.publish(); }
        }
    "#;
    assert!(
        analyze_io_effects_publication(additional_inherent_method)
            .unwrap()
            .violations
            .iter()
            .any(|violation| violation.contains("bypass:method-publish")),
        "an additional inherent publication method must be rejected"
    );

    let trait_publication = r#"
        struct IoCoreEffects { after_unlock: AfterEngineUnlock }
        trait PublishUnchecked { fn publish_unchecked(self); }
        impl PublishUnchecked for IoCoreEffects {
            fn publish_unchecked(self) { self.after_unlock.publish(); }
        }
    "#;
    let trait_analysis = analyze_io_effects_publication(trait_publication).unwrap();
    assert!(
        trait_analysis
            .violations
            .iter()
            .any(|violation| violation.contains("trait-impl")),
        "an IoCoreEffects trait implementation must be rejected"
    );
    assert!(
        trait_analysis
            .violations
            .iter()
            .any(|violation| violation.contains("publish_unchecked:method-publish")),
        "publication through an IoCoreEffects trait method must be rejected"
    );

    let destructured_publication = r#"
        struct IoCoreEffects { after_unlock: AfterEngineUnlock }
        fn bypass(effects: IoCoreEffects) {
            let IoCoreEffects { after_unlock } = effects;
            after_unlock.publish();
        }
    "#;
    let destructured_analysis = analyze_io_effects_publication(destructured_publication).unwrap();
    assert!(
        destructured_analysis
            .violations
            .iter()
            .any(|violation| violation.contains("bypass:destructure")),
        "destructuring IoCoreEffects must be rejected"
    );
    assert!(
        destructured_analysis
            .violations
            .iter()
            .any(|violation| violation.contains("bypass:method-publish")),
        "publication through a destructured alias must be rejected"
    );

    let aliased_ufcs_publication = r#"
        struct IoCoreEffects { after_unlock: AfterEngineUnlock }
        fn bypass(effects: IoCoreEffects) {
            let detached = effects.after_unlock;
            AfterEngineUnlock::publish(detached);
        }
    "#;
    assert!(
        analyze_io_effects_publication(aliased_ufcs_publication)
            .unwrap()
            .violations
            .iter()
            .any(|violation| violation.contains("bypass:ufcs-publish")),
        "UFCS publication through an aliased detached value must be rejected"
    );

    let conditional_guards = r#"
        struct IoCoreEffects { after_unlock: AfterEngineUnlock }
        impl IoCoreEffects {
            fn into_after_unlock(self) -> AfterEngineUnlock {
                if should_check() {
                    assert!(self.quarantine.is_empty());
                    assert!(self.drained.is_empty());
                }
                self.after_unlock
            }
        }
    "#;
    let conditional_analysis = analyze_io_effects_publication(conditional_guards).unwrap();
    assert!(
        !conditional_analysis
            .publish_methods
            .iter()
            .any(|(_, quarantine, drained, publish)| *quarantine && *drained && *publish),
        "conditional guards must not satisfy the publication contract"
    );

    let non_guard_assertions = r#"
        struct IoCoreEffects { after_unlock: AfterEngineUnlock }
        impl IoCoreEffects {
            fn into_after_unlock(self) -> AfterEngineUnlock {
                assert!(true || self.quarantine.is_empty());
                assert!(self.drained.is_empty() || true);
                self.after_unlock
            }
        }
    "#;
    let non_guard_analysis = analyze_io_effects_publication(non_guard_assertions).unwrap();
    assert!(
        !non_guard_analysis
            .publish_methods
            .iter()
            .any(|(_, quarantine, drained, publish)| *quarantine && *drained && *publish),
        "non-guarding assertions must not satisfy the publication contract"
    );

    let multiple_publications = r#"
        struct IoCoreEffects { after_unlock: AfterEngineUnlock }
        impl IoCoreEffects {
            fn into_after_unlock(self) -> AfterEngineUnlock {
                let first = self.after_unlock;
                assert!(self.quarantine.is_empty());
                assert!(self.drained.is_empty());
                first
            }
        }
    "#;
    let multiple_analysis = analyze_io_effects_publication(multiple_publications).unwrap();
    assert!(
        !multiple_analysis
            .publish_methods
            .iter()
            .any(|(_, quarantine, drained, publish)| *quarantine && *drained && *publish),
        "multiple publication sites must not satisfy the publication contract"
    );

    let parameter_destructure = r#"
        struct IoCoreEffects { after_unlock: AfterEngineUnlock }
        fn bypass(IoCoreEffects { after_unlock }: IoCoreEffects) {
            after_unlock.publish();
        }
    "#;
    assert!(
        analyze_io_effects_publication(parameter_destructure)
            .unwrap()
            .violations
            .iter()
            .any(|violation| violation.contains("bypass:destructure")),
        "parameter destructuring must be rejected"
    );

    let match_destructure = r#"
        struct IoCoreEffects { after_unlock: AfterEngineUnlock }
        fn bypass(effects: IoCoreEffects) {
            match effects {
                IoCoreEffects { after_unlock } => after_unlock.publish(),
            }
        }
    "#;
    let match_analysis = analyze_io_effects_publication(match_destructure).unwrap();
    assert!(
        match_analysis
            .violations
            .iter()
            .any(|violation| violation.contains("bypass:destructure")),
        "match destructuring must be rejected"
    );
    assert!(
        match_analysis
            .violations
            .iter()
            .any(|violation| violation.contains("bypass:method-publish")),
        "publication through a match-bound alias must be rejected"
    );

    let aliased_effect_type = r#"
        struct IoCoreEffects { after_unlock: AfterEngineUnlock }
        type EffectsAlias = IoCoreEffects;
        fn bypass(EffectsAlias { after_unlock }: EffectsAlias) {
            after_unlock.publish();
        }
    "#;
    let alias_analysis = analyze_io_effects_publication(aliased_effect_type).unwrap();
    assert!(
        alias_analysis
            .violations
            .iter()
            .any(|violation| violation.contains("EffectsAlias:type-alias")),
        "an IoCoreEffects type alias must be rejected"
    );
    assert!(
        alias_analysis
            .violations
            .iter()
            .any(|violation| violation.contains("bypass:destructure")),
        "destructuring through an IoCoreEffects alias must be rejected"
    );

    let block_local_alias = r#"
        struct IoCoreEffects { after_unlock: AfterEngineUnlock }
        fn bypass(effects: IoCoreEffects) {
            type LocalEffects = IoCoreEffects;
            let LocalEffects { after_unlock } = effects;
            after_unlock.publish();
        }
    "#;
    let block_alias_analysis = analyze_io_effects_publication(block_local_alias).unwrap();
    assert!(
        block_alias_analysis
            .violations
            .iter()
            .any(|violation| violation.contains("bypass:block-type-alias")),
        "a block-local IoCoreEffects type alias must be rejected"
    );
    assert!(
        block_alias_analysis
            .violations
            .iter()
            .any(|violation| violation.contains("bypass:destructure")),
        "destructuring through a block-local type alias must be rejected"
    );

    let block_local_use_alias = r#"
        struct IoCoreEffects { after_unlock: AfterEngineUnlock }
        fn bypass(effects: IoCoreEffects) {
            use crate::IoCoreEffects as LocalEffects;
            let LocalEffects { after_unlock } = effects;
            after_unlock.publish();
        }
    "#;
    let block_use_analysis = analyze_io_effects_publication(block_local_use_alias).unwrap();
    assert!(
        block_use_analysis
            .violations
            .iter()
            .any(|violation| violation.contains("bypass:block-use-alias")),
        "a block-local IoCoreEffects use alias must be rejected"
    );
    assert!(
        block_use_analysis
            .violations
            .iter()
            .any(|violation| violation.contains("bypass:destructure")),
        "destructuring through a block-local use alias must be rejected"
    );

    let payload_extractor = r#"
        struct IoCoreEffects { after_unlock: AfterEngineUnlock }
        impl IoCoreEffects {
            fn into_after_unlock(self) -> AfterEngineUnlock { self.after_unlock }
        }
    "#;
    assert!(
        analyze_io_effects_publication(payload_extractor)
            .unwrap()
            .publish_methods
            .iter()
            .all(|(_, quarantine, drained, extraction)| !(*quarantine && *drained && *extraction)),
        "an unguarded payload-extractor method must be rejected"
    );

    let transformed_payload_extractor = r#"
        struct IoCoreEffects { after_unlock: AfterEngineUnlock }
        impl IoCoreEffects {
            fn into_after_unlock(mut self) -> AfterEngineUnlock {
                std::mem::take(&mut self).after_unlock
            }
        }
    "#;
    assert!(
        analyze_io_effects_publication(transformed_payload_extractor)
            .unwrap()
            .publish_methods
            .iter()
            .all(|(_, quarantine, drained, extraction)| !(*quarantine && *drained && *extraction)),
        "a transformed payload-extractor method must be rejected"
    );

    let guarded_internal_extraction = r#"
        fn commit_internal_entries() {
            for entry in early {
                let effects = finish(entry);
                assert!(effects.quarantine.is_empty());
                assert!(effects.drained.is_empty());
                after_unlock.extend(effects.after_unlock);
            }
        }
    "#;
    assert!(
        has_guarded_internal_effect_extraction(guarded_internal_extraction).unwrap(),
        "the narrow guarded internal extraction must remain accepted"
    );
    let conditional_internal_extraction = r#"
        fn commit_internal_entries() {
            for entry in early {
                let effects = finish(entry);
                if should_check() {
                    assert!(effects.quarantine.is_empty());
                    assert!(effects.drained.is_empty());
                }
                after_unlock.extend(effects.after_unlock);
            }
        }
    "#;
    assert!(
        !has_guarded_internal_effect_extraction(conditional_internal_extraction).unwrap(),
        "conditional guards must not authorize direct internal effect extraction"
    );

    let ufcs_consumer = r#"
        fn bypass(effects: &mut IoCoreEffects) {
            IoCoreEffects::take_quarantine(effects);
        }
    "#;
    assert!(
        !find_production_lifecycle_calls(ufcs_consumer, &["take_quarantine"])
            .unwrap()
            .is_empty(),
        "UFCS effect consumers must be visible to consumer confinement"
    );

    let duplicate_definitions = r#"
        struct IoCoreEffects;
        mod nested { struct IoCoreEffects; }
    "#;
    assert_eq!(
        analyze_io_effects_publication(duplicate_definitions)
            .unwrap()
            .definitions,
        2,
        "recursive duplicate IoCoreEffects definitions must be visible"
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
        v2_dir.join("engine").join("driver").join("mod.rs"),
        v2_dir.join("engine").join("driver").join("test_api.rs"),
        v2_dir.join("engine").join("driver").join("tests.rs"),
        v2_dir.join("engine").join("session").join("mod.rs"),
        v2_dir
            .join("engine")
            .join("session")
            .join("cm")
            .join("mod.rs"),
        v2_dir
            .join("engine")
            .join("session")
            .join("cm")
            .join("tests.rs"),
        v2_dir
            .join("engine")
            .join("session")
            .join("connection")
            .join("mod.rs"),
        v2_dir
            .join("engine")
            .join("session")
            .join("connection")
            .join("tests.rs"),
        v2_dir.join("engine").join("session").join("drain.rs"),
        v2_dir.join("engine").join("session").join("listener.rs"),
        v2_dir.join("engine").join("session").join("registry.rs"),
        v2_dir.join("engine").join("io_core").join("mod.rs"),
        v2_dir
            .join("engine")
            .join("io_core")
            .join("operation")
            .join("mod.rs"),
        v2_dir
            .join("engine")
            .join("io_core")
            .join("operation")
            .join("tests.rs"),
        v2_dir.join("engine").join("io.rs"),
    ] {
        assert!(
            files.binary_search(&required).is_ok(),
            "expected v2 source missing from scan scope: {}",
            required.display()
        );
    }

    let mut violations = Vec::new();
    for path in &files {
        let content = fs::read_to_string(path).expect("read file");
        if source_is_test_only(&content)
            .unwrap_or_else(|error| panic!("parse file-level cfg for {}: {error}", path.display()))
        {
            continue;
        }
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
        if source_is_test_only(&source)
            .unwrap_or_else(|error| panic!("parse file-level cfg for {}: {error}", path.display()))
        {
            continue;
        }
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
    let io_core_operation_dir = io_core_dir.join("operation");
    let io_core_operation_path = io_core_operation_dir.join("mod.rs");
    let io_core_operation_tests_path = io_core_operation_dir.join("tests.rs");
    let io_core_progress_path = v2_dir.join("engine").join("io_core").join("progress.rs");
    let engine_mod_path = v2_dir.join("engine").join("mod.rs");
    let config_path = v2_dir.join("engine").join("config.rs");
    let progress_path = v2_dir.join("engine").join("progress.rs");
    let scheduler_path = v2_dir.join("engine").join("scheduler.rs");
    let session_dir = v2_dir.join("engine").join("session");
    let connection_path = session_dir.join("connection").join("mod.rs");
    let connection_tests_path = session_dir.join("connection").join("tests.rs");
    let cm_path = session_dir.join("cm").join("mod.rs");
    let listener_path = session_dir.join("listener.rs");
    let driver_path = v2_dir.join("engine").join("driver").join("mod.rs");
    let drain_path = session_dir.join("drain.rs");
    let session_path = session_dir.join("mod.rs");
    let session_progress_path = session_dir.join("progress.rs");
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
        if source_is_test_only(&source)
            .unwrap_or_else(|error| panic!("parse file-level cfg for {}: {error}", path.display()))
        {
            continue;
        }
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
    for path in collect_rs_files(&session_dir).expect("recursively enumerate session sources") {
        let source = fs::read_to_string(&path).expect("read session owner source");
        if !source_is_test_only(&source)
            .unwrap_or_else(|error| panic!("parse file-level cfg for {}: {error}", path.display()))
        {
            let violations = find_forbidden_production_dependencies(&source, &["EngineShared"])
                .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()));
            assert!(
                violations.is_empty(),
                "{} bypasses the narrow session runtime capability: {}",
                path.display(),
                violations.join(", ")
            );
        }
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
        if source_is_test_only(&source)
            .unwrap_or_else(|error| panic!("parse file-level cfg for {}: {error}", path.display()))
        {
            continue;
        }
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
    let connection_tests_source =
        fs::read_to_string(&connection_tests_path).expect("read connection test source");
    assert!(
        find_forbidden_dependencies_including_tests(&connection_tests_source, &["EngineShared"])
            .expect("parse connection test ownership")
            .is_empty(),
        "RdmaConnection must not restore direct or aliased EngineShared ownership"
    );
    let allowed_root_functions = [
        "io.rs::IoConnection::with_delayed_close_event_for_test",
        "io_core/operation/tests.rs::synthetic_engine_root",
        "io_core/operation/tests.rs::terminal_wakers_can_reenter_after_terminal_guards_drop",
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
    let io_operation_tests_source =
        fs::read_to_string(&io_core_operation_tests_path).expect("read I/O operation tests");
    assert!(
        io_operation_tests_source.contains("struct OperationOwners")
            && io_operation_tests_source.contains("io_core: Arc<IoCore>")
            && io_operation_tests_source.contains("session: Arc<SessionManager>")
            && io_operation_tests_source.contains("_runtime: Arc<dyn SessionEngineRuntime>"),
        "{} must expose explicit owner-focused fixture parts with only an opaque runtime retain",
        io_core_operation_tests_path.display()
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
        if source_is_test_only(&source)
            .unwrap_or_else(|error| panic!("parse file-level cfg for {}: {error}", path.display()))
        {
            continue;
        }
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
        if source_is_test_only(&source)
            .unwrap_or_else(|error| panic!("parse file-level cfg for {}: {error}", path.display()))
        {
            continue;
        }
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
    let production_driver = driver_source.as_str();
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
            && scheduler_source.contains("first_starts_next_pass")
            && scheduler_source.contains("fn begin_pass(")
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
        if source_is_test_only(&source)
            .unwrap_or_else(|error| panic!("parse file-level cfg for {}: {error}", path.display()))
        {
            continue;
        }
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
    let effect_source_paths =
        collect_rs_files(&engine_dir).expect("enumerate I/O effect publication paths");
    let mut global_effect_boundary_types = base_effect_boundary_types();
    let mut global_session_manager_types = HashSet::from(["SessionManager".to_owned()]);
    loop {
        let mut changed = false;
        for path in &effect_source_paths {
            let source = fs::read_to_string(path).expect("read engine source for effect aliases");
            changed |= expand_type_aliases(&source, &mut global_effect_boundary_types)
                .unwrap_or_else(|error| {
                    panic!(
                        "parse {} cross-file effect aliases: {error}",
                        path.display()
                    )
                });
            changed |= expand_type_aliases(&source, &mut global_session_manager_types)
                .unwrap_or_else(|error| {
                    panic!(
                        "parse {} cross-file session aliases: {error}",
                        path.display()
                    )
                });
        }
        if !changed {
            break;
        }
    }
    let mut io_effects_definition_paths = Vec::new();
    let mut io_effects_impl_paths = Vec::new();
    let mut io_effects_publish_methods = Vec::new();
    let mut after_unlock_accesses = BTreeMap::<(PathBuf, String), usize>::new();
    let mut unchecked_io_effect_publications = Vec::new();
    let mut quarantine_consumer_paths = Vec::new();
    let mut drained_consumer_paths = Vec::new();
    let mut zero_argument_publish_calls = BTreeMap::<(PathBuf, String), usize>::new();
    let mut effect_ufcs_publish_calls = Vec::new();
    let mut restricted_effect_function_references = Vec::new();
    let mut effect_trait_impls = Vec::new();
    let mut post_guard_publish_calls = Vec::new();
    let mut post_guard_function_references = Vec::new();
    let mut post_guard_use_aliases = Vec::new();
    let mut effect_constant_bypasses = Vec::new();
    for path in effect_source_paths {
        let source = fs::read_to_string(&path).expect("read engine source");
        if source_is_test_only(&source)
            .unwrap_or_else(|error| panic!("parse file-level cfg for {}: {error}", path.display()))
        {
            continue;
        }
        let publication = analyze_io_effects_publication(&source)
            .unwrap_or_else(|error| panic!("parse {} publication paths: {error}", path.display()));
        io_effects_definition_paths
            .extend(std::iter::repeat_n(path.clone(), publication.definitions));
        io_effects_impl_paths.extend(std::iter::repeat_n(
            path.clone(),
            publication.implementation_blocks,
        ));
        io_effects_publish_methods.extend(
            publication
                .publish_methods
                .into_iter()
                .map(|method| (path.clone(), method)),
        );
        unchecked_io_effect_publications.extend(
            publication
                .violations
                .into_iter()
                .map(|violation| format!("{}:{violation}", path.display())),
        );
        for access in publication.after_unlock_accesses {
            let function = access
                .rsplit_once(':')
                .map_or(access.as_str(), |(function, _)| function)
                .to_owned();
            *after_unlock_accesses
                .entry((path.clone(), function))
                .or_default() += 1;
        }
        quarantine_consumer_paths.extend(
            find_production_lifecycle_calls(&source, &["take_quarantine"])
                .unwrap_or_else(|error| {
                    panic!("parse {} quarantine consumers: {error}", path.display())
                })
                .into_iter()
                .map(|call| (path.clone(), call)),
        );
        drained_consumer_paths.extend(
            find_production_lifecycle_calls(&source, &["take_drained"])
                .unwrap_or_else(|error| {
                    panic!("parse {} accepted-zero consumers: {error}", path.display())
                })
                .into_iter()
                .map(|call| (path.clone(), call)),
        );
        for call in find_production_zero_argument_method_calls(&source, "publish")
            .unwrap_or_else(|error| panic!("parse {} publish calls: {error}", path.display()))
        {
            let function = call
                .rsplit_once(':')
                .map_or(call.as_str(), |(function, _)| function)
                .to_owned();
            *zero_argument_publish_calls
                .entry((path.clone(), function))
                .or_default() += 1;
        }
        effect_ufcs_publish_calls.extend(
            find_effect_ufcs_publications(&source)
                .unwrap_or_else(|error| {
                    panic!("parse {} effect UFCS publications: {error}", path.display())
                })
                .into_iter()
                .map(|line| (path.clone(), line)),
        );
        restricted_effect_function_references.extend(
            find_restricted_effect_function_references_with_types(
                &source,
                &global_effect_boundary_types,
                &global_session_manager_types,
            )
            .unwrap_or_else(|error| {
                panic!(
                    "parse {} restricted effect function references: {error}",
                    path.display()
                )
            })
            .into_iter()
            .map(|line| (path.clone(), line)),
        );
        effect_trait_impls.extend(
            find_effect_trait_impls(&source, &global_effect_boundary_types)
                .unwrap_or_else(|error| {
                    panic!(
                        "parse {} effect trait implementations: {error}",
                        path.display()
                    )
                })
                .into_iter()
                .map(|implementation| (path.clone(), implementation)),
        );
        post_guard_publish_calls.extend(
            find_production_lifecycle_calls(&source, &["publish_after_post_guards"])
                .unwrap_or_else(|error| {
                    panic!("parse {} scalar publication calls: {error}", path.display())
                })
                .into_iter()
                .map(|call| (path.clone(), call)),
        );
        post_guard_function_references.extend(
            find_restricted_free_function_references(&source, "publish_after_post_guards")
                .unwrap_or_else(|error| {
                    panic!(
                        "parse {} scalar publication references: {error}",
                        path.display()
                    )
                })
                .into_iter()
                .map(|line| (path.clone(), line)),
        );
        post_guard_use_aliases.extend(
            find_restricted_free_function_use_aliases(&source, "publish_after_post_guards")
                .unwrap_or_else(|error| {
                    panic!(
                        "parse {} scalar publication aliases: {error}",
                        path.display()
                    )
                })
                .into_iter()
                .map(|alias| (path.clone(), alias)),
        );
        effect_constant_bypasses.extend(
            find_effect_boundary_constant_bypasses(&source, &global_effect_boundary_types)
                .unwrap_or_else(|error| {
                    panic!(
                        "parse {} constant effect boundaries: {error}",
                        path.display()
                    )
                })
                .into_iter()
                .map(|violation| (path.clone(), violation)),
        );
    }
    assert_eq!(
        io_effects_definition_paths.as_slice(),
        std::slice::from_ref(&io_core_operation_path),
        "IoCoreEffects must have one production definition"
    );
    assert_eq!(
        io_effects_impl_paths.as_slice(),
        std::slice::from_ref(&io_core_operation_path),
        "IoCoreEffects must have one production implementation block"
    );
    assert_eq!(
        io_effects_publish_methods.len(),
        1,
        "IoCoreEffects must have one checked detached-payload extraction method: {io_effects_publish_methods:#?}"
    );
    let (publish_path, (_, quarantine_guarded, drained_guarded, detached_extraction)) =
        &io_effects_publish_methods[0];
    assert!(
        publish_path == &io_core_operation_path
            && *quarantine_guarded
            && *drained_guarded
            && *detached_extraction,
        "IoCoreEffects::into_after_unlock must reject both session-facing effect classes before releasing detached publication: {io_effects_publish_methods:#?}"
    );
    assert!(
        unchecked_io_effect_publications.is_empty(),
        "IoCoreEffects has an alternate unchecked publication route: {}",
        unchecked_io_effect_publications.join(", ")
    );
    assert_eq!(
        zero_argument_publish_calls,
        BTreeMap::from([
            (
                (engine_mod_path.clone(), "EngineShared::finish".to_owned()),
                1,
            ),
            (
                (io_core_operation_path.clone(), "post_io_batch".to_owned(),),
                14,
            ),
            (
                (
                    io_core_operation_path.clone(),
                    "publish_after_post_guards".to_owned(),
                ),
                1,
            ),
            (
                (
                    io_core_operation_path.clone(),
                    "CommittedIoCoreEffects::publish".to_owned(),
                ),
                1,
            ),
            (
                (
                    io_core_operation_path.clone(),
                    "DetachedIoCoreEffects::publish".to_owned(),
                ),
                1,
            ),
            (
                (
                    session_path.clone(),
                    "SessionManager::commit_io_effects".to_owned(),
                ),
                1,
            ),
            (
                (
                    drain_path.clone(),
                    "SessionManager::begin_connection_close".to_owned(),
                ),
                1,
            ),
        ]),
        "every source-visible zero-argument publish route must remain explicitly classified"
    );
    assert!(
        effect_ufcs_publish_calls.is_empty(),
        "effect publication through UFCS is forbidden outside the method-owned boundaries: {effect_ufcs_publish_calls:#?}"
    );
    assert!(
        restricted_effect_function_references.is_empty(),
        "effect methods must not escape through UFCS calls or function-item aliases: {restricted_effect_function_references:#?}"
    );
    assert!(
        effect_trait_impls.is_empty(),
        "effect boundary types must not gain alternate trait-based conversion or publication routes: {effect_trait_impls:#?}"
    );
    assert!(
        post_guard_function_references.is_empty(),
        "the scalar post-guard publication helper must not escape as a function item: {post_guard_function_references:#?}"
    );
    assert!(
        post_guard_use_aliases.is_empty(),
        "the scalar post-guard publication helper must not escape through a use alias: {post_guard_use_aliases:#?}"
    );
    assert!(
        effect_constant_bypasses.is_empty(),
        "constant/static effect expressions must not extract or publish guarded payloads: {effect_constant_bypasses:#?}"
    );
    assert!(
        post_guard_publish_calls.len() == 3
            && post_guard_publish_calls
                .iter()
                .all(|(path, call)| path == &io_core_operation_path
                    && call.split(':').nth(1) == Some("start_operation")),
        "all and only scalar early-completion branches may use the guard-consuming publication helper: {post_guard_publish_calls:#?}"
    );
    assert_eq!(
        after_unlock_accesses,
        BTreeMap::from([
            (
                (
                    io_core_operation_path.clone(),
                    "IoCore::terminalize_operations".to_owned(),
                ),
                1,
            ),
            (
                (
                    io_core_operation_path.clone(),
                    "IoCore::terminalize_operations_bounded".to_owned(),
                ),
                1,
            ),
            (
                (
                    io_core_operation_path.clone(),
                    "IoCoreEffects::extend".to_owned(),
                ),
                2,
            ),
            (
                (
                    io_core_operation_path.clone(),
                    "commit_internal_entries".to_owned(),
                ),
                1,
            ),
            (
                (
                    io_core_operation_path.clone(),
                    "IoCoreEffects::into_after_unlock".to_owned(),
                ),
                1,
            ),
        ]),
        "IoCoreEffects detached payload access escaped the reviewed mutation and publication sites"
    );
    assert!(
        has_guarded_internal_effect_extraction(&io_core_operation_source)
            .expect("parse direct internal effect publication"),
        "the narrow direct I/O publication path must prove that no session-facing effects exist"
    );
    assert_eq!(
        quarantine_consumer_paths.len(),
        1,
        "operation quarantine effects must have one production consumer: {quarantine_consumer_paths:#?}"
    );
    assert!(
        quarantine_consumer_paths[0].0 == session_path
            && quarantine_consumer_paths[0]
                .1
                .starts_with("take_quarantine:apply_io_effects:"),
        "operation quarantine effects must be consumed only by SessionManager"
    );
    assert_eq!(
        drained_consumer_paths.len(),
        1,
        "accepted-zero effects must have one production consumer: {drained_consumer_paths:#?}"
    );
    assert!(
        drained_consumer_paths[0].0 == session_path
            && drained_consumer_paths[0]
                .1
                .starts_with("take_drained:apply_io_effects:"),
        "accepted-zero effects must be consumed only by SessionManager"
    );
    let io_core_operation_syntax =
        syn::parse_file(&io_core_operation_source).expect("parse I/O operation source");
    let io_effects = io_core_operation_syntax
        .items
        .iter()
        .find_map(|item| match item {
            Item::Struct(item) if item.ident == "IoCoreEffects" => Some(item),
            _ => None,
        })
        .expect("locate IoCoreEffects");
    assert_eq!(
        io_effects
            .fields
            .iter()
            .filter_map(|field| field.ident.as_ref().map(ToString::to_string))
            .collect::<Vec<_>>(),
        ["after_unlock", "quarantine", "drained"],
        "IoCoreEffects must retain the reviewed session-facing effect fields"
    );
    assert!(
        io_effects
            .fields
            .iter()
            .all(|field| matches!(field.vis, syn::Visibility::Inherited)),
        "IoCoreEffects fields must remain private"
    );
    let apply_io_effects = session_source
        .split("fn apply_io_effects(&self, mut effects: IoCoreEffects) -> CommittedIoCoreEffects {")
        .nth(1)
        .and_then(|tail| {
            tail.split("\n    }\n\n    /// Consume session-facing")
                .next()
        })
        .expect("locate SessionManager::apply_io_effects");
    assert!(
        apply_io_effects.contains("effects.take_quarantine()")
            && apply_io_effects.contains("effects.take_drained()")
            && apply_io_effects.contains("effects.into_committed()"),
        "SessionManager must consume quarantine and accepted-zero effects before producing committed detached effects"
    );
    let committed_io_effects = io_core_operation_syntax
        .items
        .iter()
        .find_map(|item| match item {
            Item::Struct(item) if item.ident == "CommittedIoCoreEffects" => Some(item),
            _ => None,
        })
        .expect("locate CommittedIoCoreEffects");
    assert_eq!(
        committed_io_effects
            .fields
            .iter()
            .filter_map(|field| field.ident.as_ref().map(ToString::to_string))
            .collect::<Vec<_>>(),
        ["after_unlock"],
        "CommittedIoCoreEffects must contain only detached publication"
    );
    assert!(
        committed_io_effects
            .fields
            .iter()
            .all(|field| matches!(field.vis, syn::Visibility::Inherited)),
        "CommittedIoCoreEffects fields must remain private"
    );
    let detached_io_effects = io_core_operation_syntax
        .items
        .iter()
        .find_map(|item| match item {
            Item::Struct(item) if item.ident == "DetachedIoCoreEffects" => Some(item),
            _ => None,
        })
        .expect("locate DetachedIoCoreEffects");
    assert_eq!(
        detached_io_effects
            .fields
            .iter()
            .filter_map(|field| field.ident.as_ref().map(ToString::to_string))
            .collect::<Vec<_>>(),
        ["after_unlock"],
        "DetachedIoCoreEffects must contain only detached publication"
    );
    assert!(
        detached_io_effects
            .fields
            .iter()
            .all(|field| matches!(field.vis, syn::Visibility::Inherited)),
        "DetachedIoCoreEffects fields must remain private"
    );
    assert!(
        io_core_operation_source.contains("\nstruct AfterEngineUnlock {")
            && io_core_operation_source
                .contains("pub(in crate::v2::engine) struct DetachedIoCoreEffects {")
            && io_core_operation_source
                .contains("pub(in crate::v2::engine) struct CommittedIoCoreEffects {")
            && io_core_operation_source.contains(
                "pub(in crate::v2::engine) fn into_committed(self) -> CommittedIoCoreEffects"
            )
            && io_core_operation_source.contains("pub(in crate::v2::engine) fn publish(self) {"),
        "detached and committed publication types must retain engine-only visibility"
    );
    assert_eq!(
        find_production_lifecycle_calls(&io_core_operation_source, &["into_after_unlock"])
            .expect("find guarded full-effect extraction calls")
            .into_iter()
            .map(|call| call.split(':').nth(1).unwrap_or("<unknown>").to_owned())
            .collect::<Vec<_>>(),
        ["into_committed", "finish_early_completion"],
        "guarded full-effect extraction is confined to scalar early completion and session commit conversion"
    );
    let mut into_committed_calls = Vec::new();
    let mut terminal_apply_calls = Vec::new();
    for path in collect_rs_files(&engine_dir).expect("enumerate consuming I/O effect calls") {
        let source = fs::read_to_string(&path).expect("read engine source");
        if source_is_test_only(&source)
            .unwrap_or_else(|error| panic!("parse file-level cfg for {}: {error}", path.display()))
        {
            continue;
        }
        into_committed_calls.extend(
            find_production_lifecycle_calls(&source, &["into_committed"])
                .unwrap_or_else(|error| {
                    panic!("parse {} committed conversion: {error}", path.display())
                })
                .into_iter()
                .map(|call| (path.clone(), call)),
        );
        terminal_apply_calls.extend(
            find_production_lifecycle_calls(&source, &["apply_terminal_io_effects"])
                .unwrap_or_else(|error| {
                    panic!("parse {} terminal application: {error}", path.display())
                })
                .into_iter()
                .map(|call| (path.clone(), call)),
        );
    }
    assert_eq!(
        into_committed_calls.len(),
        1,
        "full I/O effects must have one production conversion to committed effects: {into_committed_calls:#?}"
    );
    assert!(
        into_committed_calls[0].0 == session_path
            && into_committed_calls[0]
                .1
                .starts_with("into_committed:apply_io_effects:"),
        "only the private SessionManager application helper may construct committed effects"
    );
    assert_eq!(
        terminal_apply_calls.len(),
        1,
        "terminal-only session effect application must have one production caller: {terminal_apply_calls:#?}"
    );
    assert!(
        terminal_apply_calls[0].0 == engine_mod_path
            && terminal_apply_calls[0]
                .1
                .starts_with("apply_terminal_io_effects:finish:"),
        "only EngineShared::finish may request terminal-applied I/O effects"
    );
    assert!(
        session_source.contains(
            "pub(super) fn commit_io_effects(&self, effects: IoCoreEffects)"
        ) && session_source.contains(
            "pub(super) fn apply_terminal_io_effects(\n        &self,\n        effects: IoCoreEffects,\n    ) -> CommittedIoCoreEffects"
        ) && session_source.contains("fn commit_terminal_effects(&self, effects: IoCoreEffects)")
            && session_source.contains("self.commit_io_effects(effects);")
            && !session_source.contains("effects: &mut IoCoreEffects"),
        "SessionManager must expose only by-value ordinary, terminal, and bridge effect boundaries"
    );
    assert!(
        io_progress_source.contains("self.bridge.commit_terminal_effects(effects);")
            && !io_progress_source.contains("apply_terminal_effects"),
        "bounded I/O terminalization must delegate by value to the ordinary commit boundary"
    );
    assert!(
        engine_mod.contains(
            "let committed_io_effects = self.session.apply_terminal_io_effects(io_effects);"
        ) && engine_mod.contains("committed_io_effects.publish();")
            && !engine_mod.contains("\n        io_effects.publish();"),
        "root terminal composition must consume only the session-applied effect state"
    );
    let terminal_apply = engine_mod
        .find("let committed_io_effects = self.session.apply_terminal_io_effects(io_effects);")
        .expect("root terminal session application");
    let terminalize_cm = engine_mod[terminal_apply..]
        .find("self.session.terminalize_cm(&outcome);")
        .map(|offset| terminal_apply + offset)
        .expect("root CM terminalization");
    let finalize_connection = engine_mod[terminalize_cm..]
        .find(".finalize_connection_engine(connection, &outcome)")
        .map(|offset| terminalize_cm + offset)
        .expect("root connection terminalization");
    let publish_operations = engine_mod[finalize_connection..]
        .find("committed_io_effects.publish();")
        .map(|offset| finalize_connection + offset)
        .expect("root operation publication");
    let wake_close = engine_mod[publish_operations..]
        .find("connection.wake_close();")
        .map(|offset| publish_operations + offset)
        .expect("root close wake");
    let wake_terminal = engine_mod[wake_close..]
        .find("self.terminal_notify.notify_waiters();")
        .map(|offset| wake_close + offset)
        .expect("root terminal wake");
    assert!(
        terminal_apply < terminalize_cm
            && terminalize_cm < finalize_connection
            && finalize_connection < publish_operations
            && publish_operations < wake_close
            && wake_close < wake_terminal,
        "root terminal composition order must remain session apply, CM, connections, operations, close, terminal"
    );
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
        if source_is_test_only(&source)
            .unwrap_or_else(|error| panic!("parse file-level cfg for {}: {error}", path.display()))
        {
            continue;
        }
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

    for relocated in ["mod.rs", "drain.rs", "listener.rs", "registry.rs"] {
        assert!(
            v2_dir
                .join("engine")
                .join("session")
                .join(relocated)
                .is_file(),
            "session relocation requires engine/session/{relocated}"
        );
    }
    for owner in ["cm", "connection"] {
        for child in ["mod.rs", "tests.rs"] {
            assert!(
                session_dir.join(owner).join(child).is_file(),
                "session owner extraction requires engine/session/{owner}/{child}"
            );
        }
        assert!(
            !session_dir.join(format!("{owner}.rs")).exists(),
            "session owner extraction must remove engine/session/{owner}.rs"
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
