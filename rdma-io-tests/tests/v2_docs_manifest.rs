//! Stable source-documentation anchor checks for retained V2 production units.

#[allow(dead_code)]
#[path = "fixtures/v2_surface_manifest.rs"]
mod manifest;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use syn::{Attribute, Item};

struct Anchor<'a> {
    key: &'a str,
    file: &'a str,
    item: Option<&'a str>,
    rendered: &'a str,
}

fn source_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .join("rdma-io/src/v2")
}

fn doc_text(attributes: &[Attribute]) -> String {
    attributes
        .iter()
        .filter(|attribute| attribute.path().is_ident("doc"))
        .filter_map(|attribute| match &attribute.meta {
            syn::Meta::NameValue(value) => match &value.value {
                syn::Expr::Lit(literal) => match &literal.lit {
                    syn::Lit::Str(text) => Some(text.value()),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn item_name(item: &Item) -> Option<String> {
    match item {
        Item::Const(item) => Some(item.ident.to_string()),
        Item::Enum(item) => Some(item.ident.to_string()),
        Item::Fn(item) => Some(item.sig.ident.to_string()),
        Item::Struct(item) => Some(item.ident.to_string()),
        Item::Trait(item) => Some(item.ident.to_string()),
        Item::Type(item) => Some(item.ident.to_string()),
        _ => None,
    }
}

fn anchors() -> Vec<Anchor<'static>> {
    manifest::RUSTDOC_MANIFEST
        .lines()
        .filter_map(|line| {
            let columns = line.split('|').collect::<Vec<_>>();
            match columns.as_slice() {
                ["anchor", key, file, item, rendered] => Some(Anchor {
                    key,
                    file,
                    item: (*item != "-").then_some(*item),
                    rendered,
                }),
                ["removed", "-", "-", "-", _] => None,
                _ => panic!("invalid rustdoc manifest row: {line}"),
            }
        })
        .collect()
}

#[test]
fn every_retained_anchor_has_all_four_literal_sections() {
    let anchors = anchors();
    let keys = anchors
        .iter()
        .map(|anchor| anchor.key)
        .collect::<BTreeSet<_>>();
    for unit in manifest::UNITS.iter().filter(|unit| {
        unit.disposition == manifest::Disposition::Retain
            && matches!(
                unit.domain,
                manifest::Domain::Signature | manifest::Domain::Module
            )
            && unit.id != "M-017"
    }) {
        let key = unit
            .doc_anchor
            .expect("retained production unit doc anchor");
        assert!(
            keys.contains(key),
            "{} references unknown documentation anchor {key}",
            unit.id
        );
    }

    for anchor in &anchors {
        let path = source_root().join(anchor.file);
        let parsed = syn::parse_file(
            &fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display())),
        )
        .unwrap_or_else(|error| panic!("failed to parse {}: {error}", path.display()));
        let docs = match anchor.item {
            None => doc_text(&parsed.attrs),
            Some(expected) => parsed
                .items
                .iter()
                .find(|item| item_name(item).as_deref() == Some(expected))
                .map(|item| match item {
                    Item::Const(item) => doc_text(&item.attrs),
                    Item::Enum(item) => doc_text(&item.attrs),
                    Item::Fn(item) => doc_text(&item.attrs),
                    Item::Struct(item) => doc_text(&item.attrs),
                    Item::Trait(item) => doc_text(&item.attrs),
                    Item::Type(item) => doc_text(&item.attrs),
                    _ => unreachable!(),
                })
                .unwrap_or_else(|| {
                    panic!("missing documentation anchor {expected} in {}", anchor.file)
                }),
        };
        for section in [
            "# Use case",
            "# Ownership and progress",
            "# Safety and limits",
            "# Availability",
        ] {
            assert!(
                docs.contains(section),
                "{}::{:?} is missing {section}",
                anchor.file,
                anchor.item
            );
        }
        assert!(
            !anchor.rendered.is_empty(),
            "{} must name a rendered rustdoc page",
            anchor.key
        );
    }
}

#[test]
fn canonical_hook_module_is_hidden_and_explains_v2_ownership() {
    let module = fs::read_to_string(source_root().join("test_support.rs")).unwrap();
    assert!(module.contains("V2 lifecycle observations"));
    assert!(module.contains("not a V1 consumer API"));

    let facade = fs::read_to_string(source_root().join("mod.rs")).unwrap();
    assert!(facade.contains("#[doc(hidden)]\npub mod test_support;"));
}
