//! Source-level regression test ensuring no hidden `tokio::spawn` calls
//! exist in v2 production code.
//!
//! This test runs without RDMA hardware — it is a pure source-file analysis.
//! It scans all `.rs` files under `rdma-io/src/v2/` for `tokio::spawn`
//! occurrences that are NOT inside doc-comment lines (`///` or `//!`).

use std::fs;
use std::path::Path;

/// Canonical doc-comment exclusion: lines starting with `///` or `//!`
/// (after optional whitespace) are documentation examples where
/// `tokio::spawn` is expected.
fn is_doc_comment(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("///") || trimmed.starts_with("//!")
}

#[test]
fn test_no_hidden_tokio_spawn_in_v2() {
    // Find the workspace root (this test lives in rdma-io-tests/)
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace_root = Path::new(manifest_dir).parent().expect("workspace root");
    let v2_dir = workspace_root.join("rdma-io").join("src").join("v2");

    assert!(
        v2_dir.exists(),
        "v2 directory not found at {}",
        v2_dir.display()
    );

    let mut violations = Vec::new();

    for entry in fs::read_dir(&v2_dir).expect("read v2 dir") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "rs") {
            let content = fs::read_to_string(&path).expect("read file");
            for (line_num, line) in content.lines().enumerate() {
                if line.contains("tokio::spawn") && !is_doc_comment(line) {
                    violations.push(format!(
                        "{}:{}: {}",
                        path.display(),
                        line_num + 1,
                        line.trim()
                    ));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "Found tokio::spawn in v2 production code (outside doc comments):\n{}",
        violations.join("\n")
    );
}
