use std::path::{Path, PathBuf};
use std::process::Command;

fn compile_blueprint(manifest_dir: &Path, out_dir: &Path, source: &str, output: &str) {
    let blueprint_path = manifest_dir.join(source);
    let output_path = out_dir.join(output);
    println!("cargo:rerun-if-changed={}", blueprint_path.display());

    let status = Command::new("blueprint-compiler")
        .arg("compile")
        .arg(&blueprint_path)
        .arg("--output")
        .arg(&output_path)
        .status()
        .expect("failed to run `blueprint-compiler`; please install blueprint-compiler");

    if !status.success() {
        panic!(
            "blueprint-compiler failed while compiling {}",
            blueprint_path.display()
        );
    }
}

fn main() {
    let manifest_dir = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR should be set by Cargo"),
    );
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR should be set by Cargo"));

    compile_blueprint(&manifest_dir, &out_dir, "src/ui/main.blp", "main.ui");
    compile_blueprint(
        &manifest_dir,
        &out_dir,
        "src/ui/grid_cell.blp",
        "grid_cell.ui",
    );
    compile_blueprint(
        &manifest_dir,
        &out_dir,
        "src/ui/tag_chip.blp",
        "tag_chip.ui",
    );
}
