use std::fs;
use std::path::{Path, PathBuf};

const LEGACY_NAMES: &[&str] = &[
    "SharedQp",
    "MessageTransportDriver",
    "separate_cqs",
    "v2_shared_qp_tests",
];

fn collect_rs_files(root: &Path, files: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(root)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", root.display()));
    for entry in entries {
        let entry = entry.unwrap_or_else(|error| {
            panic!("failed to read entry under {}: {error}", root.display())
        });
        let path = entry.path();
        let file_type = entry
            .file_type()
            .unwrap_or_else(|error| panic!("failed to inspect {}: {error}", path.display()));
        assert!(
            !file_type.is_symlink(),
            "documentation scan refuses symlink {}",
            path.display()
        );
        if file_type.is_dir() {
            collect_rs_files(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}

fn rustdoc_markdown(source: &str) -> String {
    source
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim_start();
            trimmed
                .strip_prefix("//!")
                .or_else(|| trimmed.strip_prefix("///"))
        })
        .map(|line| line.strip_prefix(' ').unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_no_legacy_public_surface(name: &str, markdown: &str) {
    let mut in_fence = false;
    for (index, line) in markdown.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        for legacy in LEGACY_NAMES {
            assert!(
                !(in_fence && line.contains(legacy)),
                "{name}:{} contains legacy {legacy} in a public snippet",
                index + 1
            );
            let rustdoc_link = format!("[`{legacy}`]");
            let markdown_link = format!("[{legacy}](");
            let code_markdown_link = format!("[`{legacy}`](");
            assert!(
                !line.contains(&rustdoc_link)
                    && !line.contains(&markdown_link)
                    && !line.contains(&code_markdown_link),
                "{name}:{} contains a legacy API link for {legacy}",
                index + 1
            );
        }
    }
    assert!(!in_fence, "{name} has an unterminated fenced code block");
}

#[test]
fn public_v2_documentation_has_no_legacy_endpoint_surface() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    for relative in ["README.md", "docs/design/v2-rdma-engine.md"] {
        let path = workspace.join(relative);
        let source = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
        assert_no_legacy_public_surface(relative, &source);
    }

    let mut rust_files = Vec::new();
    collect_rs_files(&workspace.join("rdma-io/src/v2"), &mut rust_files);
    assert!(!rust_files.is_empty(), "v2 rustdoc scope must not be empty");
    rust_files.sort();
    for path in rust_files {
        let source = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
        assert_no_legacy_public_surface(
            path.to_str().expect("v2 path must be valid UTF-8"),
            &rustdoc_markdown(&source),
        );
    }
}
