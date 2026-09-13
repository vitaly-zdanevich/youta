//! Keeps Tauri's generated capability schemas outside the packaged source tree.
//!
//! Cargo verifies published packages without allowing their build scripts to
//! change source files. Tauri writes `gen/schemas` relative to its working
//! directory, so its helper runs against copied inputs inside `OUT_DIR`.

use std::path::{Path, PathBuf};
use std::{env, fs, io};

/// Placeholder shown when the window is built without its front-end assets.
///
/// The real page is produced by Vite into `frontend/`, which is not checked in.
/// Without this, `cargo build --workspace` would fail for anyone who has not run
/// npm — including the repository's own test and lint runs, which have no reason
/// to need a JavaScript toolchain. The placeholder keeps those green and states
/// plainly what is missing, instead of leaving a blank window.
const PLACEHOLDER: &str = r#"<!doctype html>
<html lang="en" data-theme="dark">
  <head>
    <meta charset="utf-8" />
    <title>Youta</title>
    <style>
      body {
        margin: 0;
        display: grid;
        place-items: center;
        height: 100vh;
        background: #0c0b10;
        color: #a29db1;
        font: 13px/1.5 ui-sans-serif, system-ui, sans-serif;
        text-align: center;
      }
      code {
        color: #e4744f;
        font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
      }
    </style>
  </head>
  <body>
    <p>
      Youta&rsquo;s window assets were not built.<br />
      Run <code>npm --prefix gui/ui ci &amp;&amp; npm --prefix gui/ui run build</code>,
      then rebuild.
    </p>
  </body>
</html>
"#;

fn main() {
    let manifest_directory = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("Cargo must set CARGO_MANIFEST_DIR"),
    );
    let output_directory = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo must set OUT_DIR"));
    let frontend = manifest_directory.join("frontend");
    if manifest_directory.join("Cargo.toml.orig").exists() {
        // Cargo archives must be usable without npm or a surrounding checkout.
        for asset in ["index.html", "app.js", "app.css"] {
            assert!(
                frontend
                    .join(asset)
                    .metadata()
                    .is_ok_and(|file| file.is_file() && file.len() > 0),
                "the published GUI package must contain its built frontend/{asset}"
            );
        }
    }
    let entry = frontend.join("index.html");
    if !entry.exists() {
        fs::create_dir_all(&frontend).expect("create the front-end asset directory");
        fs::write(&entry, PLACEHOLDER).expect("write the front-end placeholder");
    }

    // Watch the original inputs: Tauri also emits paths from its copied input
    // directory, which alone would miss changed or removed source capabilities.
    for relative in [
        "Cargo.toml",
        "tauri.conf.json",
        "capabilities",
        "icons",
        "frontend",
    ] {
        println!(
            "cargo:rerun-if-changed={}",
            manifest_directory.join(relative).display()
        );
    }

    let staging_directory = stage_tauri_inputs(&manifest_directory, &output_directory)
        .expect("stage Tauri build inputs inside OUT_DIR");
    let original_directory = env::current_dir().expect("read the build working directory");
    env::set_current_dir(&staging_directory).expect("enter the Tauri build directory");
    let result = tauri_build::try_build(tauri_build::Attributes::new());
    env::set_current_dir(original_directory).expect("restore the build working directory");
    result.expect("generate Tauri build metadata");
}

/// Copies the configuration and capability inputs used by Tauri's build helper.
///
/// The frontend stays at its original path for `generate_context!`, which reads
/// it relative to `CARGO_MANIFEST_DIR`. Generated ACL metadata still goes into
/// the unchanged `OUT_DIR`, where that macro expects to find it.
fn stage_tauri_inputs(source: &Path, output: &Path) -> io::Result<PathBuf> {
    let staging = output.join("tauri-build");
    if staging.exists() {
        // Removed source capabilities must not survive from an earlier build.
        fs::remove_dir_all(&staging)?;
    }
    fs::create_dir_all(&staging)?;
    for relative in ["Cargo.toml", "tauri.conf.json"] {
        copy_build_file(&source.join(relative), &staging.join(relative))?;
    }
    for relative in ["capabilities", "icons"] {
        copy_build_directory(&source.join(relative), &staging.join(relative))?;
    }
    Ok(staging)
}

/// Copies nested build inputs without sharing writable files with the source.
fn copy_build_directory(source: &Path, destination: &Path) -> io::Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let destination = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_build_directory(&entry.path(), &destination)?;
        } else {
            copy_build_file(&entry.path(), &destination)?;
        }
    }
    Ok(())
}

/// Preserves source timestamps so Tauri's staged-input watches stay fresh.
///
/// A newly timestamped copy would look newer than Cargo's build start and
/// cause the next unchanged build to run this script again indefinitely.
fn copy_build_file(source: &Path, destination: &Path) -> io::Result<()> {
    let mut input = fs::File::open(source)?;
    let mut output = fs::File::create(destination)?;
    io::copy(&mut input, &mut output)?;
    output.set_modified(input.metadata()?.modified()?)
}
