use shadow_rs::ShadowBuilder;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    ShadowBuilder::builder()
        .deny_const(Default::default())
        .build()
        .expect("shadow build");

    let unix = cfg!(unix);
    let macos = cfg!(target_os = "macos");

    let layer_shell_capture = cfg!(feature = "layer_shell_capture");
    let libei_capture = cfg!(feature = "libei_capture");
    let x11_capture = cfg!(feature = "x11_capture");

    let libei_emulation = cfg!(feature = "libei_emulation");
    let x11_emulation = cfg!(feature = "x11_emulation");
    let wlroots_emulation = cfg!(feature = "wlroots_emulation");
    let rdp_emulation = cfg!(feature = "rdp_emulation");

    let layer_shell_capture = unix && !macos && layer_shell_capture;
    let libei_capture = unix && !macos && libei_capture;
    let x11_capture = unix && !macos && x11_capture;

    let libei_emulation = unix && !macos && libei_emulation;
    let rdp_emulation = unix && !macos && rdp_emulation;
    let wlroots_emulation = unix && !macos && wlroots_emulation;
    let x11_emulation = unix && !macos && x11_emulation;

    println!("cargo::rustc-check-cfg=cfg(layer_shell_capture)");
    println!("cargo::rustc-check-cfg=cfg(libei_capture)");
    println!("cargo::rustc-check-cfg=cfg(x11_capture)");

    println!("cargo::rustc-check-cfg=cfg(libei_emulation)");
    println!("cargo::rustc-check-cfg=cfg(rdp_emulation)");
    println!("cargo::rustc-check-cfg=cfg(wlroots_emulation)");
    println!("cargo::rustc-check-cfg=cfg(x11_emulation)");

    if layer_shell_capture {
        println!("cargo::rustc-cfg=layer_shell_capture");
    }
    if libei_capture {
        println!("cargo::rustc-cfg=libei_capture");
    }
    if x11_capture {
        println!("cargo::rustc-cfg=x11_capture");
    }

    if libei_emulation {
        println!("cargo::rustc-cfg=libei_emulation");
    }
    if rdp_emulation {
        println!("cargo::rustc-cfg=rdp_emulation");
    }
    if wlroots_emulation {
        println!("cargo::rustc-cfg=wlroots_emulation");
    }
    if x11_emulation {
        println!("cargo::rustc-cfg=x11_emulation");
    }

    // Build the embedded web UI. Skipped for `cargo check` /
    // `cargo test` (we don't want to spin up npm in CI just to type-
    // check the Rust side) — those targets set `CARGO_CFG_TEST` or
    // are invoked from a context where the user already knows they
    // only want the Rust bits.
    //
    // The Vue dist must exist before `rust-embed` resolves the
    // `#[folder = "lan-mouse-vue/dist/"]` macro, so a fresh clone
    // needs `npm ci && npm run build` once. We do that here rather
    // than require every contributor to remember, because the web UI
    // is the only frontend in the project.
    if should_build_web() {
        build_web_ui();
    }
}

/// Don't rebuild the Vue bundle on `cargo check` (slow), but do rebuild
/// on any actual `cargo build`. The heuristic: if any of the source
/// files (Vue src, package.json, vite config) changed since the last
/// build, rerun. Touch the dist with `cargo:rerun-if-changed` so cargo
/// knows to redo the link step on next `build` even if no Rust file
/// changed.
fn should_build_web() -> bool {
    // CARGO_CFG_TEST is set by `cargo test` for build scripts. Skip.
    if std::env::var_os("CARGO_CFG_TEST").is_some() {
        return false;
    }
    // Always run the script's body once; the actual work is gated on
    // dist staleness inside `build_web_ui`.
    true
}

fn build_web_ui() {
    let web_dir = PathBuf::from("lan-mouse-vue");
    let dist = web_dir.join("dist");
    let index = dist.join("index.html");

    // Rebuild if any tracked source changed OR dist is missing.
    println!("cargo:rerun-if-changed=lan-mouse-vue/src");
    println!("cargo:rerun-if-changed=lan-mouse-vue/package.json");
    println!("cargo:rerun-if-changed=lan-mouse-vue/package-lock.json");
    println!("cargo:rerun-if-changed=lan-mouse-vue/vite.config.ts");
    println!("cargo:rerun-if-changed=lan-mouse-vue/index.html");
    println!("cargo:rerun-if-changed=lan-mouse-vue/tsconfig.json");
    println!("cargo:rerun-if-changed=lan-mouse-vue/public");

    let needs_build = !index.exists()
        || source_newer_than_dist(&web_dir, &dist)
        || std::env::var_os("LAN_MOUSE_FORCE_WEB_BUILD").is_some();

    if !needs_build {
        return;
    }

    eprintln!("lan-mouse-vue/dist missing or stale — running `npm ci && npm run build`");
    if !Path::new(&web_dir.join("node_modules")).exists() {
        run_cmd(&web_dir, "npm", &["ci"]);
    }
    run_cmd(&web_dir, "npm", &["run", "build"]);
}

fn source_newer_than_dist(web_dir: &Path, dist: &Path) -> bool {
    let Ok(dist_meta) = std::fs::metadata(dist).and_then(|m| m.modified()) else {
        return true;
    };
    let Ok(entries) = std::fs::read_dir(web_dir) else {
        return true;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let s = name.to_string_lossy();
        if s == "node_modules" || s == "dist" || s.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            if any_newer(&path, dist_meta).unwrap_or(false) {
                return true;
            }
        } else if let Ok(m) = path.metadata().and_then(|m| m.modified()) {
            if m > dist_meta {
                return true;
            }
        }
    }
    false
}

fn any_newer(dir: &Path, threshold: std::time::SystemTime) -> std::io::Result<bool> {
    for entry in std::fs::read_dir(dir)?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if any_newer(&path, threshold)? {
                return Ok(true);
            }
        } else if let Ok(m) = path.metadata().and_then(|m| m.modified()) {
            if m > threshold {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn run_cmd(cwd: &Path, program: &str, args: &[&str]) {
    eprintln!("$ {program} {}", args.join(" "));
    let status = Command::new(program)
        .current_dir(cwd)
        .args(args)
        .status()
        .unwrap_or_else(|e| panic!("failed to spawn `{program}`: {e}"));
    if !status.success() {
        panic!("`{program} {}` failed with {status}", args.join(" "));
    }
}
