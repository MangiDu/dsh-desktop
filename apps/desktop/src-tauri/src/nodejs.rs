//! Bundled Node.js toolchain (M5-1): the app ships its own Node + npm so
//! it runs on machines without a system toolchain, and never depends on a
//! user-level npm cache (which this project has seen break in the wild).
//!
//! Resolution order for the runtime root:
//! 1. `DSH_NODE_DIR` env (debug/testing)
//! 2. packaged layout: `<exe>/../Resources/node-runtime/<os>-<arch>`
//! 3. dev layout: `<exe>/node-runtime/<os>-<arch>`
//! 4. dev layout: `<cwd>/binaries/node-runtime/<os>-<arch>`
//!
//! A system `node`/`npm` remains the fallback when no bundle is found.

use std::path::{Path, PathBuf};

use tauri::{AppHandle, Manager};

/// Concrete binaries of one bundled toolchain.
#[derive(Debug, Clone)]
pub struct NodeToolchain {
    pub node: PathBuf,
    pub npm_cli: PathBuf,
    pub source: String,
}

fn platform_dir() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

fn candidate_roots(app: &AppHandle) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(dir) = std::env::var("DSH_NODE_DIR") {
        if !dir.trim().is_empty() {
            roots.push(PathBuf::from(dir));
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            // packaged macOS .app: Contents/MacOS/<exe> -> ../Resources
            roots.push(exe_dir.join("../Resources/node-runtime"));
            // dev: target/debug/<exe>
            roots.push(exe_dir.join("node-runtime"));
        }
    }
    // dev: tauri dev runs the app with cwd = src-tauri
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd.join("binaries/node-runtime"));
    }
    let _ = app.path().resource_dir().map(|d| roots.push(d.join("node-runtime")));
    roots
}

fn toolchain_at(root: &Path) -> Option<NodeToolchain> {
    let base = root.join(platform_dir());
    let node = base.join("bin/node");
    let npm_cli = base.join("lib/node_modules/npm/bin/npm-cli.js");
    if node.is_file() && npm_cli.is_file() {
        return Some(NodeToolchain {
            node,
            npm_cli,
            source: base.display().to_string(),
        });
    }
    None
}

/// Locate the bundled toolchain, if one is present.
pub fn bundled_toolchain(app: &AppHandle) -> Option<NodeToolchain> {
    candidate_roots(app).iter().find_map(|root| toolchain_at(root))
}

/// The `node` executable to use (bundled first, system `node` as fallback).
pub fn node_program(app: &AppHandle) -> (String, Option<String>) {
    match bundled_toolchain(app) {
        Some(tc) => (tc.node.to_string_lossy().to_string(), Some(tc.source)),
        None => ("node".to_string(), None),
    }
}

/// The bundled toolchain's bin dir (node/npm/npx), if present.
pub fn bundled_bin_dir(app: &AppHandle) -> Option<PathBuf> {
    bundled_toolchain(app).and_then(|tc| tc.node.parent().map(|p| p.to_path_buf()))
}

/// `PATH` with the bundled bin dir prepended, so lifecycle scripts and
/// child processes find `node`/`npm`/`npx` even on a bare PATH.
pub fn enriched_path(app: &AppHandle) -> Option<String> {
    let bin = bundled_bin_dir(app)?;
    let existing = std::env::var("PATH").unwrap_or_default();
    Some(format!("{}:{}", bin.display(), existing))
}

/// An `npm` invocation as (program, prefix-args): bundled node + npm-cli.js
/// when available, plain `npm` otherwise. Prefix args slot in before the
/// subcommand (e.g. `view`, `install`).
pub fn npm_invocation(app: &AppHandle) -> (String, Vec<String>) {
    match bundled_toolchain(app) {
        Some(tc) => (
            tc.node.to_string_lossy().to_string(),
            vec![tc.npm_cli.to_string_lossy().to_string()],
        ),
        None => ("npm".to_string(), Vec::new()),
    }
}
