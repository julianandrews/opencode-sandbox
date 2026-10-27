use std::collections::HashMap;
use std::fs;

use anyhow::{Context, Result};

use crate::podman;
use crate::r;
use crate::util;

/// Ensure the project's R dependencies are installed.
///
/// Runs `renv::restore()` in a throwaway container on every launch instead of
/// relying on a marker file: `renv::restore()` is a fast no-op when the
/// library is synchronized with the lockfile (~1-2s), and self-heals when it
/// is not. No persistent setup state exists that could go stale.
///
/// The script restores into the renv-managed project library when the project
/// activates renv, and falls back to `R_LIBS_SITE` (with a loud warning) when
/// it does not, so setup always targets the library the app reads at runtime.
///
/// Fails the launch on failure so a container never starts with a library
/// that does not satisfy `renv.lock`.
pub fn check_and_restore(image: &str, env_vars: &HashMap<String, String>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let renv_lock = cwd.join("renv.lock");
    if !renv_lock.exists() {
        return Ok(());
    }

    println!("→ Ensuring R dependencies (renv.lock present)...");

    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;
    let r_lib_cache = home.join(".local/share/opencode-sandbox/r-library");
    fs::create_dir_all(&r_lib_cache)?;

    let cwd_basename = cwd
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "root".to_string());

    let tmp = tempfile::tempdir()?;
    let r_script_path = tmp.path().join("r-setup.R");
    std::fs::write(&r_script_path, include_str!("../sandbox/r-setup.R"))?;

    let mut cmd = podman::command();
    cmd.args(["run", "--rm"]);
    cmd.args(podman::identity_args(util::is_rootless()));
    cmd.args(podman::selinux_args(util::selinux_enforcing()));
    cmd.args([
        "-e",
        "HOME=/tmp",
        "-e",
        &format!("R_LIBS_SITE={}", r::R_LIB_SITE),
        "-e",
        &format!("R_REPOSITORIES={}", r::R_CRAN_REPO),
    ]);

    for (name, value) in r::XDG_ENV {
        cmd.args(["-e", &format!("{}={}", name, value)]);
    }

    for (k, v) in env_vars {
        cmd.args(["-e", &format!("{}={}", k, v)]);
    }

    for (host, target) in r::r_user_dir_mounts()? {
        cmd.args(["-v", &format!("{}:{}", host.display(), target)]);
    }

    cmd.args([
        "-v",
        &format!("{}:{}", r_lib_cache.display(), r::R_LIB_SITE),
        "-v",
        &format!("{}:/workspace/{}", cwd.display(), cwd_basename),
        "-v",
        &format!("{}:/setup.R:ro", r_script_path.display()),
        "-w",
        &format!("/workspace/{}", cwd_basename),
        "--tmpfs",
        "/tmp:exec",
        "--read-only",
        "--entrypoint",
        "R",
        image,
        "-f",
        "/setup.R",
    ]);

    let status = cmd.status().context("failed to run R dependency setup")?;
    if !status.success() {
        anyhow::bail!("R dependency setup failed (exit code: {:?})", status.code());
    }

    Ok(())
}
