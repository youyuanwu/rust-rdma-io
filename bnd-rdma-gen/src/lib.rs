use std::path::Path;

/// Generate the bnd-rdma source tree at `output_dir`.
///
/// 1. Runs bnd-winmd on `rdma.toml` to produce a `.winmd`.
/// 2. Runs `windows-bindgen --package` to emit `src/rdma/*/mod.rs`.
///    Passes posix and linux winmds so cross-winmd type references
///    resolve correctly.  `--reference` suppresses codegen for those
///    types; the generated code uses `bnd_posix::posix::…` and
///    `bnd_linux::linux::…` paths.
/// 3. Saves the `.winmd` under `output_dir/winmd/`.
pub fn generate(output_dir: &Path) {
    let gen_dir = Path::new(env!("CARGO_MANIFEST_DIR"));

    // Step 1: Generate .winmd
    let winmd_dir = output_dir.join("winmd");
    std::fs::create_dir_all(&winmd_dir).expect("failed to create winmd directory");
    let rdma_winmd = winmd_dir.join("bnd-rdma.winmd");
    bnd_winmd::run(&gen_dir.join("rdma.toml"), Some(&rdma_winmd))
        .expect("bnd-winmd failed to generate winmd");

    // Step 2: Locate posix and linux winmds
    let posix_winmd = gen_dir.join("../../bnd/bnd-posix/winmd/bnd-posix.winmd");
    assert!(
        posix_winmd.exists(),
        "posix winmd not found at {}\n\
         Hint: run `cargo run -p bnd-posix-gen` in ../bnd first",
        posix_winmd.display()
    );
    let linux_winmd = gen_dir.join("../../bnd/bnd-linux/winmd/bnd-linux.winmd");
    assert!(
        linux_winmd.exists(),
        "linux winmd not found at {}\n\
         Hint: run `cargo run -p bnd-linux-gen` in ../bnd first",
        linux_winmd.display()
    );

    // Step 3: Generate crate source tree via windows-bindgen package mode
    windows_bindgen::bindgen([
        "--in",
        rdma_winmd.to_str().unwrap(),
        "--in",
        posix_winmd.to_str().unwrap(),
        "--in",
        linux_winmd.to_str().unwrap(),
        "--out",
        output_dir.to_str().unwrap(),
        "--filter",
        "rdma",
        "--reference",
        "bnd_posix,full,posix",
        "--reference",
        "bnd_linux,full,linux",
        "--sys",
        "--package",
        "--no-toml",
    ])
    .unwrap();
}
