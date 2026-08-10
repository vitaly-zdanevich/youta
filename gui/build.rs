use std::fs;
use std::path::Path;

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
    let frontend = Path::new("frontend");
    let entry = frontend.join("index.html");
    if !entry.exists() {
        fs::create_dir_all(frontend).expect("create the front-end asset directory");
        fs::write(&entry, PLACEHOLDER).expect("write the front-end placeholder");
    }
    // This build script deliberately emits no `cargo:rerun-if-changed`. Naming
    // any path would narrow Cargo to that path alone, and the capability files
    // this crate's security boundary rests on would stop being regenerated.

    tauri_build::build();
}
