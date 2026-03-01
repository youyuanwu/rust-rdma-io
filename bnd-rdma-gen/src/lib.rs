use std::path::Path;

/// Generate the bnd-rdma source tree at `output_dir`.
///
/// 1. Runs bnd-winmd on `rdma.toml` to produce a `.winmd`.
///    The toml references `build/winmd/bnd-linux.winmd` (downloaded by CMake).
/// 2. Runs `windows-bindgen --package` to emit `src/rdma/*/mod.rs`.
///    `--reference` suppresses codegen for bnd-linux types; the generated code
///    uses `bnd_linux::libc::posix::…` and `bnd_linux::libc::linux::…` paths.
/// 3. Saves the `.winmd` under `output_dir/winmd/`.
pub fn generate(output_dir: &Path) {
    let gen_dir = Path::new(env!("CARGO_MANIFEST_DIR"));

    // Step 1: Locate linux winmd in build/winmd/ (downloaded by cmake -B build)
    let linux_winmd = gen_dir.join("../build/winmd/bnd-linux.winmd");
    assert!(
        linux_winmd.exists(),
        "linux winmd not found at {}\n\
         Hint: run `cmake -B build` to download it",
        linux_winmd.display()
    );

    // Step 2: Generate .winmd from rdma.toml
    let winmd_dir = output_dir.join("winmd");
    std::fs::create_dir_all(&winmd_dir).expect("failed to create winmd directory");
    let rdma_winmd = winmd_dir.join("bnd-rdma.winmd");
    bnd_winmd::run(&gen_dir.join("rdma.toml"), Some(&rdma_winmd))
        .expect("bnd-winmd failed to generate winmd");

    // Step 3: Generate crate source tree via windows-bindgen package mode
    windows_bindgen::bindgen([
        "--in",
        rdma_winmd.to_str().unwrap(),
        "--in",
        linux_winmd.to_str().unwrap(),
        "--out",
        output_dir.to_str().unwrap(),
        "--filter",
        "rdma",
        "--reference",
        "bnd_linux,full,libc.posix",
        "--reference",
        "bnd_linux,full,libc.linux",
        "--sys",
        "--package",
        "--no-toml",
    ])
    .unwrap();
}
