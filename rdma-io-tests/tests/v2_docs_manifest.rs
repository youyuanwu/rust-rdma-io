//! Stable source-documentation anchor checks for retained V2 production units.

use std::fs;
use std::path::{Path, PathBuf};

use syn::{Attribute, Item};

struct Anchor {
    file: &'static str,
    item: Option<&'static str>,
}

const ANCHORS: &[Anchor] = &[
    Anchor {
        file: "mod.rs",
        item: None,
    },
    Anchor {
        file: "error.rs",
        item: Some("Error"),
    },
    Anchor {
        file: "context.rs",
        item: Some("Context"),
    },
    Anchor {
        file: "pd.rs",
        item: Some("Pd"),
    },
    Anchor {
        file: "mr.rs",
        item: Some("AccessIntent"),
    },
    Anchor {
        file: "mr.rs",
        item: Some("Mr"),
    },
    Anchor {
        file: "mr.rs",
        item: Some("RemoteMr"),
    },
    Anchor {
        file: "cq.rs",
        item: Some("CqBuilder"),
    },
    Anchor {
        file: "cq.rs",
        item: Some("Cq"),
    },
    Anchor {
        file: "completion.rs",
        item: Some("Completions"),
    },
    Anchor {
        file: "tokio_support.rs",
        item: Some("TokioCompletions"),
    },
    Anchor {
        file: "cq_poller.rs",
        item: Some("CqPoller"),
    },
    Anchor {
        file: "qp.rs",
        item: Some("QpBuilder"),
    },
    Anchor {
        file: "qp.rs",
        item: Some("Qp"),
    },
    Anchor {
        file: "op.rs",
        item: Some("Completion"),
    },
    Anchor {
        file: "engine/mod.rs",
        item: Some("RdmaEngineBuilder"),
    },
    Anchor {
        file: "engine/diagnostics.rs",
        item: Some("RdmaEngineDiagnostics"),
    },
    Anchor {
        file: "engine/connection.rs",
        item: Some("RdmaConnectionIdentity"),
    },
];

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

#[test]
fn every_retained_anchor_has_all_four_literal_sections() {
    for anchor in ANCHORS {
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
