use std::path::PathBuf;
use std::process::Command;

fn main() {
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR is set by cargo"));
    let ui_out = out_dir.join("ui");

    // Blueprint → GtkBuilder XML in OUT_DIR; the gresource bundle then picks
    // the .ui files up from there.
    let blueprints = ["data/ui/window.blp", "data/ui/preferences.blp", "data/ui/shortcuts-dialog.blp"];
    let status = Command::new("blueprint-compiler")
        .arg("batch-compile")
        .arg(&ui_out)
        .arg("data/ui")
        .args(blueprints)
        .status()
        .expect("blueprint-compiler must be installed (zypper in blueprint-compiler)");
    assert!(status.success(), "blueprint-compiler failed");

    glib_build_tools::compile_resources(
        &[PathBuf::from("data"), out_dir],
        "data/resources.gresource.xml",
        "zeke.gresource",
    );
    println!("cargo:rerun-if-changed=data");
}
