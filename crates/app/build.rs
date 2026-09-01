use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let source = manifest.join("assets/icons");
    let manifest_xml = source.join("fm-icons.gresource.xml");
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR")).join("fm-icons.gresource");

    println!("cargo:rerun-if-changed={}", manifest_xml.display());
    println!("cargo:rerun-if-changed={}", source.display());

    let status = Command::new("glib-compile-resources")
        .arg(&manifest_xml)
        .arg("--sourcedir")
        .arg(&source)
        .arg("--target")
        .arg(&output)
        .status()
        .expect("glib-compile-resources is required to build Feather Mail icons");
    assert!(
        status.success(),
        "could not compile Feather Mail icon resources"
    );
}
