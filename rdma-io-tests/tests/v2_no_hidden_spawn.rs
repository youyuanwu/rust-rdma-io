//! Source-level regression test ensuring no hidden spawning calls exist
//! in v2 production code.
//!
//! This test runs without RDMA hardware — it is a pure source-file analysis.
//! It recursively scans all `.rs` files under `rdma-io/src/v2/` for spawn
//! patterns that are NOT inside comments or documentation examples.

use std::fs;
use std::path::{Path, PathBuf};

/// Spawn patterns that must not appear in v2 production code.
const SPAWN_PATTERNS: &[(&str, &str)] = &[
    ("tokio::spawn", "tokio::spawn"),
    ("tokio::task::spawn", "tokio::task::spawn"),
    ("spawn_blocking", "spawn_blocking"),
    ("std::thread::spawn", "std::thread::spawn"),
    ("Handle::current().spawn", "Handle::current().spawn("),
];

/// Recursively collect all .rs files under a directory.
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

fn raw_string_start(bytes: &[u8], start: usize) -> Option<(usize, usize)> {
    let mut i = start;
    if bytes.get(i) == Some(&b'b') {
        i += 1;
    }
    if bytes.get(i) != Some(&b'r') {
        return None;
    }
    i += 1;

    let mut hashes = 0;
    while bytes.get(i) == Some(&b'#') {
        hashes += 1;
        i += 1;
    }

    (bytes.get(i) == Some(&b'"')).then_some((i - start + 1, hashes))
}

/// Strip comments, doc examples, strings, and whitespace while preserving a
/// source line mapping for the remaining executable code tokens.
fn normalize_code(source: &str) -> (Vec<u8>, Vec<usize>) {
    #[derive(Clone, Copy)]
    enum State {
        Code,
        LineComment,
        BlockComment { depth: usize },
        String { escaped: bool },
        RawString { hashes: usize },
    }

    let bytes = source.as_bytes();
    let mut normalized = Vec::with_capacity(bytes.len());
    let mut line_map = Vec::with_capacity(bytes.len());
    let mut state = State::Code;
    let mut i = 0;
    let mut line = 1usize;

    while i < bytes.len() {
        match state {
            State::Code => {
                if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'/') {
                    state = State::LineComment;
                    i += 2;
                    continue;
                }
                if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                    state = State::BlockComment { depth: 1 };
                    i += 2;
                    continue;
                }
                if let Some((consumed, hashes)) = raw_string_start(bytes, i) {
                    state = State::RawString { hashes };
                    i += consumed;
                    continue;
                }
                if bytes[i] == b'b' && bytes.get(i + 1) == Some(&b'"') {
                    state = State::String { escaped: false };
                    i += 2;
                    continue;
                }
                if bytes[i] == b'"' {
                    state = State::String { escaped: false };
                    i += 1;
                    continue;
                }
                if bytes[i] == b'\n' {
                    line += 1;
                } else if !bytes[i].is_ascii_whitespace() {
                    normalized.push(bytes[i]);
                    line_map.push(line);
                }
                i += 1;
            }
            State::LineComment => {
                if bytes[i] == b'\n' {
                    line += 1;
                    state = State::Code;
                }
                i += 1;
            }
            State::BlockComment { depth } => {
                if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                    state = State::BlockComment { depth: depth + 1 };
                    i += 2;
                    continue;
                }
                if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    if depth == 1 {
                        state = State::Code;
                    } else {
                        state = State::BlockComment { depth: depth - 1 };
                    }
                    i += 2;
                    continue;
                }
                if bytes[i] == b'\n' {
                    line += 1;
                }
                i += 1;
            }
            State::String { escaped } => {
                if bytes[i] == b'\n' {
                    line += 1;
                }
                if escaped {
                    state = State::String { escaped: false };
                    i += 1;
                    continue;
                }
                if bytes[i] == b'\\' {
                    state = State::String { escaped: true };
                    i += 1;
                    continue;
                }
                if bytes[i] == b'"' {
                    state = State::Code;
                }
                i += 1;
            }
            State::RawString { hashes } => {
                if bytes[i] == b'\n' {
                    line += 1;
                }
                if bytes[i] == b'"'
                    && i + hashes < bytes.len()
                    && bytes[i + 1..i + 1 + hashes].iter().all(|&b| b == b'#')
                {
                    state = State::Code;
                    i += 1 + hashes;
                    continue;
                }
                i += 1;
            }
        }
    }

    (normalized, line_map)
}

fn find_spawn_violations(path: &Path, source: &str) -> Vec<String> {
    let (normalized, line_map) = normalize_code(source);
    let source_lines: Vec<_> = source.lines().collect();
    let mut violations = Vec::new();

    for (label, needle) in SPAWN_PATTERNS {
        let needle = needle.as_bytes();
        if normalized.len() < needle.len() {
            continue;
        }

        for start in 0..=normalized.len() - needle.len() {
            if &normalized[start..start + needle.len()] == needle {
                let line = line_map[start];
                let snippet = source_lines
                    .get(line.saturating_sub(1))
                    .map(|line| line.trim())
                    .unwrap_or("");
                violations.push(format!(
                    "{}:{}: [{}] {}",
                    path.display(),
                    line,
                    label,
                    snippet
                ));
            }
        }
    }

    violations
}

#[test]
fn test_no_hidden_spawn_in_v2() {
    // Find the workspace root (this test lives in rdma-io-tests/)
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
        violations.extend(find_spawn_violations(path, &content));
    }

    assert!(
        violations.is_empty(),
        "Found spawn calls in v2 production code (outside comments):\n{}",
        violations.join("\n")
    );
}
