use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::path;
use crate::r;
use crate::util;

/// Default `opencode.jsonc` seeded into the host config dir on first run
/// (source: `docker/opencode.jsonc.template`). The file is bind-mounted
/// read-only into the container, so an agent can never rewrite its own
/// permissions, and the `deny` policies inside survive any project config.
const DEFAULT_OPENCODE_CONFIG: &str = include_str!("../docker/opencode.jsonc.template");

pub const PROGRAM: &str = "podman";

/// The Podman command every subprocess path goes through.
pub fn command() -> std::process::Command {
    std::process::Command::new(PROGRAM)
}

/// Identity flags shared by the main run and the R dependency-setup container.
///
/// Rootless Podman maps the host user to a subuid by default, so a plain
/// `--user <uid>:<gid>` makes bind-mount writes land as an unprivileged subuid
/// (or fail outright). `--userns=keep-id` maps the host UID to the same UID
/// inside the container, matching the passwd entry baked into the image by the
/// `USER_ID` build arg.
pub fn identity_args(rootless: bool) -> Vec<String> {
    let mut args = Vec::new();
    if rootless {
        args.push("--userns=keep-id".into());
    }
    args.push("--user".into());
    args.push(format!("{}:{}", util::uid(), util::gid()));
    args
}

/// SELinux label separation blocks unlabelled bind mounts on enforcing hosts.
/// Relabelling is not an option here: the config mounts include read-only
/// Nix-store paths and the workspace may be large. Disable label separation
/// for this rootless, read-only, no-capabilities container instead.
pub fn selinux_args(enforcing: bool) -> Vec<String> {
    if enforcing {
        vec!["--security-opt=label=disable".into()]
    } else {
        Vec::new()
    }
}

#[derive(Debug)]
pub struct PodmanArgs {
    args: Vec<String>,
}

impl PodmanArgs {
    pub fn new() -> Self {
        PodmanArgs { args: Vec::new() }
    }

    pub fn into_vec(self) -> Vec<String> {
        self.args
    }

    pub fn base(&mut self, rootless: bool) -> &mut Self {
        let term = std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string());
        let lang = std::env::var("LANG").unwrap_or_else(|_| "C.UTF-8".to_string());

        self.args.push("--rm".into());
        self.args.push("-it".into());
        self.args.push("--init".into());
        self.args.push("--hostname".into());
        self.args.push("opencode".into());
        self.args.extend(identity_args(rootless));
        self.args.push("-e".into());
        self.args.push(format!("TERM={}", term));
        self.args.push("-e".into());
        self.args.push("COLORTERM=truecolor".into());
        self.args.push("-e".into());
        self.args.push(format!("LANG={}", lang));
        self
    }

    /// `edit_config` is the deliberate `--edit-config` escape hatch: the config
    /// files and the `plugins`/`node_modules` dirs are then left writable (no
    /// read-only mounts) so config and plugins can be edited from inside the
    /// container. `config_dir` is the host XDG config dir; it is injected so
    /// the tests never touch the real one.
    pub fn xdg_dirs(&mut self, config_dir: &Path, edit_config: bool) -> Result<&mut Self> {
        let config = config_dir.to_path_buf();
        let cache = dirs::cache_dir().context("cache dir required")?;
        let data = dirs::data_dir().context("data dir required")?;
        let state = match dirs::state_dir() {
            Some(s) => s,
            None => {
                let home = dirs::home_dir().context("home dir required")?;
                home.join(".local/state")
            }
        };

        let dirs_to_create = [
            // Created host-side so the bind mount is owned by the host user:
            // Podman would otherwise create it as root and the container's
            // non-root user could not write cli.json/service.json (V2 mounts
            // the config dir read-write for its first-run tui.json migration).
            config.join("opencode"),
            // The plugins dir and node_modules are created (and later mounted
            // read-only) so writable targets for plugin code never exist in
            // the container; see the mounts in xdg_dirs.
            config.join("opencode/plugins"),
            config.join("opencode/node_modules"),
            cache.join("opencode"),
            data.join("opencode"),
            data.join("opencode/log"),
            data.join("opencode/bin"),
            data.join("opentui"),
            state.join("opencode"),
            data.join("opencode-docker/r-library"),
            data.join("opencode-docker/home"),
        ];
        for dir in &dirs_to_create {
            let _ = std::fs::create_dir_all(dir);
        }

        for (name, value) in r::XDG_ENV {
            self.args.push("-e".into());
            self.args.push(format!("{}={}", name, value));
        }
        // A writable, persistent home for the runtime user. The image bakes a
        // passwd entry matching the host UID (see the Dockerfile), whose home
        // is /opencode/home; mounting it here keeps `~/.ssh` and other
        // user-scoped files writable across runs despite `--read-only`. This
        // is what lets ssh-keygen save its default ~/.ssh/id_* path.
        self.args.push("-e".into());
        self.args.push("HOME=/opencode/home".into());
        self.args.push("-v".into());
        self.args.push(format!(
            "{}:/opencode/home",
            data.join("opencode-docker/home").display()
        ));
        self.args.push("-v".into());
        self.args.push(format!(
            "{}:/opencode/config/opencode",
            config.join("opencode").display()
        ));
        // Seed a default opencode config host-side when none exists yet (the
        // host-side creation keeps the bind mount owned by the host user, like
        // the dirs above). Every existing config entry is then mounted
        // read-only, except the runtime-writable regular files cli.json,
        // service.json and tui.json. Symlinked entries are read-only even
        // with `--edit-config`: they point into the Nix store, so a writable
        // mount could never work, and skipping the mount would expose a
        // dangling symlink inside the container. This is what lets Nix manage
        // the server config, CLI settings, sounds, agents, commands and skills
        // with ordinary Home Manager symlinks.
        let config_dir = config.join("opencode");
        for mount in opencode_config_mounts(&config_dir, edit_config)? {
            self.args.push("-v".into());
            self.args.push(mount);
        }
        self.args.push("-v".into());
        self.args.push(format!(
            "{}:/opencode/cache/opencode",
            cache.join("opencode").display()
        ));
        self.args.push("-v".into());
        self.args.push(format!(
            "{}:/opencode/data/opencode",
            data.join("opencode").display()
        ));
        // opentui (the TUI framework V2 renders with) resolves its data path to
        // `$XDG_DATA_HOME/opentui`, where its Tree-sitter highlight worker caches
        // parsers and queries. Without this writable mount the worker fails
        // silently and markdown/code highlighting degrades to raw text for
        // non-root runs, so do not remove it (see README "OpenCode V2").
        self.args.push("-v".into());
        self.args.push(format!(
            "{}:/opencode/data/opentui",
            data.join("opentui").display()
        ));
        self.args.push("-v".into());
        self.args.push(format!(
            "{}:/opencode/state/opencode",
            state.join("opencode").display()
        ));
        Ok(self)
    }

    pub fn cargo_home(&mut self) -> Result<&mut Self> {
        let data = dirs::data_dir().context("data dir required")?;
        let _ = std::fs::create_dir_all(data.join("cargo"));
        self.args.push("-e".into());
        self.args.push("CARGO_HOME=/opencode/cargo".into());
        self.args.push("-v".into());
        self.args
            .push(format!("{}:/opencode/cargo", data.join("cargo").display()));
        Ok(self)
    }

    pub fn workspace(&mut self) -> Result<&mut Self> {
        let cwd = std::env::current_dir().context("failed to get current directory")?;
        let basename = cwd
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "root".to_string());

        self.args.push("-v".into());
        self.args
            .push(format!("{}:/workspace/{}", cwd.display(), basename));
        self.args.push("-w".into());
        self.args.push(format!("/workspace/{}", basename));
        Ok(self)
    }

    pub fn security(&mut self, selinux_enforcing: bool) -> &mut Self {
        self.args.push("--security-opt=no-new-privileges".into());
        self.args.extend(selinux_args(selinux_enforcing));
        self.args.push("--cap-drop".into());
        self.args.push("all".into());
        self.args.push("--read-only".into());
        self.args.push("--tmpfs".into());
        self.args.push("/tmp:exec".into());
        self
    }

    pub fn network(&mut self) -> &mut Self {
        // Podman adds host.containers.internal and host.docker.internal
        // automatically. An explicit `--add-host ...:host-gateway` can make the
        // run fail when Podman cannot determine a gateway address, so rely on
        // the defaults. If host loopback is unreachable on a rootless host, add
        // `--network=pasta:--map-gw` after verifying it on that host.
        self.args.push("-v".into());
        self.args.push("/etc/localtime:/etc/localtime:ro".into());
        self
    }

    pub fn r_library(&mut self) -> Result<&mut Self> {
        let data = dirs::data_dir().context("data dir required")?;
        let r_lib = data.join("opencode-docker/r-library");

        self.args.push("-e".into());
        self.args.push(format!("R_LIBS_SITE={}", r::R_LIB_SITE));
        self.args.push("-v".into());
        self.args
            .push(format!("{}:{}", r_lib.display(), r::R_LIB_SITE));
        Ok(self)
    }

    /// Mount the host R XDG dirs so `tools::R_user_dir()` paths (e.g. renv's
    /// project library, sass's cache) are writable and persist across runs.
    pub fn r_user_dirs(&mut self) -> Result<&mut Self> {
        for (host, target) in r::r_user_dir_mounts()? {
            self.args.push("-v".into());
            self.args.push(format!("{}:{}", host.display(), target));
        }
        Ok(self)
    }

    pub fn git_config(&mut self) -> Result<&mut Self> {
        let home = dirs::home_dir().context("home directory required")?;
        let git_config = home.join(".config/git");
        if git_config.is_dir() {
            self.args.push("-v".into());
            self.args
                .push(format!("{}:/opencode/config/git:ro", git_config.display()));
        }
        Ok(self)
    }

    pub fn ports(&mut self, ports: &[u16]) -> &mut Self {
        for port in ports {
            self.args.push("-p".into());
            self.args.push(format!("127.0.0.1:{}:{}", port, port));
        }
        self
    }

    pub fn extra_mounts(&mut self, mounts: &[String]) -> Result<&mut Self> {
        let cwd =
            std::env::current_dir().context("failed to get current directory for extra mounts")?;
        let cwd_basename = cwd
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "root".to_string());

        let mut seen_targets = HashSet::from([cwd_basename]);

        for mount_spec in mounts {
            let (host_path, target_name) = if let Some(idx) = mount_spec.find(':') {
                let host = &mount_spec[..idx];
                let target = &mount_spec[idx + 1..];
                (host, Some(target.to_string()))
            } else {
                (mount_spec.as_str(), None)
            };

            let resolved = path::resolve_path(host_path)
                .with_context(|| format!("failed to resolve mount path '{}'", host_path))?;

            let target = target_name.unwrap_or_else(|| {
                resolved
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .expect("mount path has no filename")
            });

            if !seen_targets.insert(target.clone()) {
                anyhow::bail!("duplicate mount target '/workspace/{}'", target);
            }

            self.args.push("-v".into());
            self.args
                .push(format!("{}:/workspace/{}", resolved.display(), target));
        }
        Ok(self)
    }

    pub fn audio(&mut self, enabled: bool) -> Result<&mut Self> {
        if !enabled {
            return Ok(self);
        }
        let runtime_dir = Path::new("/run/user").join(util::uid());
        let pulse_socket = runtime_dir.join("pulse/native");
        if !pulse_socket.exists() {
            eprintln!(
                "Warning: PulseAudio socket not found at {}; skipping audio setup",
                pulse_socket.display()
            );
            return Ok(self);
        }

        let pulse_dir = pulse_socket
            .parent()
            .context("pulse socket has no parent")?;
        self.args.push("-e".into());
        self.args
            .push(format!("PULSE_SERVER=unix:{}", pulse_socket.display()));
        self.args.push("-v".into());
        self.args.push(format!(
            "{}:{}:ro",
            pulse_dir.display(),
            pulse_dir.display()
        ));

        let home = dirs::home_dir().context("home directory required")?;
        let cookie = home.join(".config/pulse/cookie");
        if cookie.exists() {
            self.args.push("-e".into());
            self.args.push("PULSE_COOKIE=/opencode/pulse-cookie".into());
            self.args.push("-v".into());
            self.args
                .push(format!("{}:/opencode/pulse-cookie:rw", cookie.display()));
        }
        Ok(self)
    }

    pub fn env_vars(&mut self, vars: &HashMap<String, String>) -> &mut Self {
        for (k, v) in vars {
            self.args.push("-e".into());
            self.args.push(format!("{}={}", k, v));
        }
        self
    }
}

/// Entries that stay writable for the runtime when they are regular files.
/// A symlink with one of these names is mounted read-only instead: its target
/// lives in the Nix store, where writes fail.
const RUNTIME_WRITABLE_CONFIG: &[&str] = &["cli.json", "service.json", "tui.json"];

/// Recognized global config file names. opencode merges all of them, so all
/// are mounted read-only; a directory squatting on one of these names is not a
/// config file and is skipped.
const GLOBAL_CONFIG_FILES: &[&str] = &["opencode.jsonc", "opencode.json", "config.json"];

/// Build the host-side read-only mounts for the global opencode config dir,
/// creating the default config on first run if none exists yet.
///
/// In a locked run every existing entry is mounted read-only except regular
/// `cli.json`, `service.json` and `tui.json`, which the TUI/server must be able
/// to write. Symlinked entries are mounted read-only even with
/// `--edit-config`: they point into the Nix store, so a writable mount could
/// never work, and skipping the mount would expose a dangling symlink inside
/// the container. Mounts are skipped for missing paths: mounting a nonexistent
/// source would make Podman create a root-owned directory in the user's config
/// tree.
fn opencode_config_mounts(config_dir: &Path, edit_config: bool) -> Result<Vec<String>> {
    let _ = seed_opencode_config(config_dir);

    let entries = match fs::read_dir(config_dir) {
        Ok(entries) => entries,
        Err(err) => {
            eprintln!(
                "Warning: could not read opencode config dir '{}' ({err:#}); skipping read-only config mounts.",
                config_dir.display()
            );
            return Ok(Vec::new());
        }
    };

    let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
    entries.sort_by_key(|entry| entry.file_name());

    let mut mounts = Vec::new();
    for entry in entries {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let path = entry.path();
        let is_symlink = entry.file_type().map(|t| t.is_symlink()).unwrap_or(false);

        if edit_config && !is_symlink {
            continue;
        }
        if !edit_config && !is_symlink && RUNTIME_WRITABLE_CONFIG.contains(&name.as_ref()) {
            continue;
        }
        if GLOBAL_CONFIG_FILES.contains(&name.as_ref()) && path.is_dir() {
            continue;
        }
        if !path.exists() {
            // Broken symlink: there is no readable target to mount, and
            // mounting it would shadow the host entry.
            eprintln!(
                "Warning: '{}' is not a readable config entry; skipping its read-only mount.",
                path.display()
            );
            continue;
        }
        if !path.is_file() && !path.is_dir() {
            continue;
        }
        // Resolve Nix-managed symlinks so the mount source is the store path
        // itself rather than relying on Podman to follow the link.
        let source = match path.canonicalize() {
            Ok(source) => source,
            Err(err) => {
                eprintln!(
                    "Warning: could not resolve config entry '{}' ({err:#}); skipping its read-only mount.",
                    path.display()
                );
                continue;
            }
        };

        mounts.push(format!(
            "{}:/opencode/config/opencode/{}:ro",
            source.display(),
            name
        ));
    }
    Ok(mounts)
}

/// Seed the default opencode config if none exists, logging a warning when
/// that fails (e.g. because the config dir is not writable): the run then
/// simply proceeds without a config file to protect.
fn seed_opencode_config(config_dir: &Path) -> Option<PathBuf> {
    match ensure_opencode_config(config_dir) {
        Ok(Some(file)) => Some(file),
        Ok(None) => None,
        Err(err) => {
            eprintln!(
                "Warning: could not seed a default opencode config in '{}' ({err:#}); running without one.",
                config_dir.display()
            );
            None
        }
    }
}

/// Locate an existing opencode config file in `config_dir`, seeding the
/// default template when none of the recognized names exists.
///
/// Existing configs are never rewritten: the template is only a first-run
/// default (see `docker/opencode.jsonc.template`). A directory or broken
/// symlink squatting on a recognized name is left untouched and blocks
/// seeding rather than being clobbered.
fn ensure_opencode_config(config_dir: &Path) -> Result<Option<PathBuf>> {
    for name in ["opencode.jsonc", "opencode.json", "config.json"] {
        let path = config_dir.join(name);
        if let Ok(meta) = fs::symlink_metadata(&path) {
            if meta.file_type().is_file() || meta.file_type().is_symlink() {
                // A regular file, or a symlink to one (the bind mount
                // resolves valid symlinks). Broken symlinks report is_file()
                // false and fall through to the warning below.
                if path.is_file() {
                    return Ok(Some(path));
                }
            }
            eprintln!(
                "Warning: '{}' is not a readable config file; leaving it untouched and skipping config seeding.",
                path.display()
            );
            return Ok(None);
        }
    }
    let default = config_dir.join("opencode.jsonc");
    fs::create_dir_all(config_dir).with_context(|| {
        format!(
            "failed to create opencode config directory '{}'",
            config_dir.display()
        )
    })?;
    // Write to a uniquely named temp file and rename into place, so
    // concurrent first runs never observe (or publish) a torn file.
    let mut tmp = tempfile::NamedTempFile::new_in(config_dir)
        .with_context(|| format!("failed to create a temp file in '{}'", config_dir.display()))?;
    tmp.write_all(DEFAULT_OPENCODE_CONFIG.as_bytes())
        .with_context(|| {
            format!(
                "failed to write default opencode config to '{}'",
                tmp.path().display()
            )
        })?;
    if let Err(err) = tmp.persist(&default) {
        // A concurrent first run may have published the config already;
        // treat that as success rather than failing the run.
        if default.is_file() {
            return Ok(Some(default));
        }
        return Err(err).with_context(|| {
            format!(
                "failed to move default opencode config into place at '{}'",
                default.display()
            )
        });
    }
    Ok(Some(default))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn new_args_empty() {
        let da = PodmanArgs::new();
        assert!(da.into_vec().is_empty());
    }

    #[test]
    fn builder_ports_and_env_vars() {
        let mut da = PodmanArgs::new();
        da.ports(&[3000, 4000]);
        let mut vars = HashMap::new();
        vars.insert("KEY".to_string(), "val".to_string());
        da.env_vars(&vars);
        let args = da.into_vec();
        assert_eq!(
            args,
            &[
                "-p",
                "127.0.0.1:3000:3000",
                "-p",
                "127.0.0.1:4000:4000",
                "-e",
                "KEY=val",
            ]
        );
    }

    #[test]
    fn builder_security_network() {
        let mut da = PodmanArgs::new();
        da.security(false).network();
        let args = da.into_vec();
        assert!(args.contains(&"--security-opt=no-new-privileges".to_string()));
        assert!(args.contains(&"--cap-drop".to_string()));
        assert!(args.contains(&"all".to_string()));
        assert!(args.contains(&"--read-only".to_string()));
        assert!(args.contains(&"--tmpfs".to_string()));
        assert!(args.contains(&"/tmp:exec".to_string()));
        assert!(!args.contains(&"host.docker.internal:host-gateway".to_string()));
        assert!(args.contains(&"/etc/localtime:/etc/localtime:ro".to_string()));
    }

    #[test]
    fn identity_args_keep_id_only_for_rootless() {
        let user = format!("{}:{}", util::uid(), util::gid());
        assert_eq!(
            identity_args(true),
            vec![
                "--userns=keep-id".to_string(),
                "--user".to_string(),
                user.clone()
            ]
        );
        assert_eq!(identity_args(false), vec!["--user".to_string(), user]);
    }

    #[test]
    fn security_adds_selinux_label_disable_only_when_enforcing() {
        let mut locked = PodmanArgs::new();
        locked.security(false);
        assert!(!locked
            .into_vec()
            .contains(&"--security-opt=label=disable".to_string()));

        let mut selinux = PodmanArgs::new();
        selinux.security(true);
        assert!(selinux
            .into_vec()
            .contains(&"--security-opt=label=disable".to_string()));
    }

    #[test]
    fn base_uses_keep_id_when_rootless() {
        let user = format!("{}:{}", util::uid(), util::gid());
        let mut rootless = PodmanArgs::new();
        rootless.base(true);
        let args = rootless.into_vec();
        assert!(args.contains(&"--userns=keep-id".to_string()));
        assert!(args.windows(2).any(|w| w[0] == "--user" && w[1] == user));

        let mut rootful = PodmanArgs::new();
        rootful.base(false);
        assert!(!rootful.into_vec().contains(&"--userns=keep-id".to_string()));
    }

    #[test]
    fn xdg_dirs_mounts_writable_home_for_runtime_user() {
        // The image bakes a passwd entry whose home is /opencode/home; it must
        // be mounted writable (the rootfs is read-only) and HOME pointed at it,
        // or ssh-keygen's default ~/.ssh write fails after the passwd fix.
        let tmp = tempfile::tempdir().unwrap();
        let mut da = PodmanArgs::new();
        da.xdg_dirs(tmp.path(), false).unwrap();
        let args = da.into_vec();
        assert!(
            args.windows(2)
                .any(|w| w == ["-e".to_string(), "HOME=/opencode/home".to_string()]),
            "HOME=/opencode/home missing: {args:?}"
        );
        assert!(
            args.iter().any(|a| a.ends_with(":/opencode/home")),
            "home mount missing: {args:?}"
        );
    }

    #[test]
    fn xdg_dirs_mounts_opentui_data_path() {
        // The TUI's Tree-sitter highlight worker caches under `$XDG_DATA_HOME/opentui`;
        // without this writable mount the worker fails silently and markdown/code
        // highlighting degrades to raw text for non-root runs.
        let tmp = tempfile::tempdir().unwrap();
        let mut da = PodmanArgs::new();
        da.xdg_dirs(tmp.path(), false).unwrap();
        let args = da.into_vec();
        assert!(
            args.iter().any(|a| a.ends_with(":/opencode/data/opentui")),
            "opentui data-path mount missing: {args:?}"
        );
    }

    #[test]
    fn xdg_dirs_mounts_plugin_surface_readonly() {
        // opencode auto-discovers plugins under the global config dir, and the
        // npm plugin surface (node_modules + manifests) is installed there, so
        // both must be read-only for agents in locked mode.
        let tmp = tempfile::tempdir().unwrap();
        let mut da = PodmanArgs::new();
        da.xdg_dirs(tmp.path(), false).unwrap();
        let args = da.into_vec();
        assert!(
            args.iter()
                .any(|a| a.ends_with(":/opencode/config/opencode/plugins:ro")),
            "plugins ro mount missing: {args:?}"
        );
        assert!(
            args.iter()
                .any(|a| a.ends_with(":/opencode/config/opencode/node_modules:ro")),
            "node_modules ro mount missing: {args:?}"
        );
        // The seeded default config is mounted ro too.
        assert!(
            args.iter()
                .any(|a| a.ends_with(":/opencode/config/opencode/opencode.jsonc:ro")),
            "config file ro mount missing: {args:?}"
        );
    }

    #[test]
    fn xdg_dirs_edit_config_skips_regular_readonly_mounts() {
        // `--edit-config` is the deliberate escape hatch: regular config
        // files, the plugins dir and the npm plugin surface are left writable
        // (no ro mounts) so config and plugins can be edited from inside the
        // container. Nix-managed symlinks are covered by the unit tests; they
        // stay read-only even in this mode.
        let tmp = tempfile::tempdir().unwrap();
        let mut da = PodmanArgs::new();
        da.xdg_dirs(tmp.path(), true).unwrap();
        let args = da.into_vec();
        for forbidden in [
            "opencode.jsonc:ro",
            "opencode.json:ro",
            "config.json:ro",
            "plugins:ro",
            "node_modules:ro",
            "package.json:ro",
        ] {
            assert!(
                !args.iter().any(|a| a.contains(forbidden)),
                "'{forbidden}' mount should be skipped with --edit-config: {args:?}"
            );
        }
    }

    #[test]
    fn builder_chaining() {
        let mut da = PodmanArgs::new();
        da.ports(&[8080]).security(false).network();
        let args = da.into_vec();
        assert!(args.contains(&"-p".to_string()));
        assert!(args.contains(&"127.0.0.1:8080:8080".to_string()));
    }

    #[test]
    fn extra_mounts_rejects_duplicate() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("subdir");
        std::fs::create_dir(&sub).unwrap();
        let mounts = vec![
            sub.to_string_lossy().to_string(),
            tmp.path().join("subdir").to_string_lossy().to_string(),
        ];
        let mut da = PodmanArgs::new();
        let result = da.extra_mounts(&mounts);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("duplicate"));
    }

    #[test]
    fn extra_mounts_rejects_nonexistent() {
        let mounts = vec!["/nonexistent_opencode_test_xyz".to_string()];
        let mut da = PodmanArgs::new();
        let result = da.extra_mounts(&mounts);
        assert!(result.is_err());
    }

    #[test]
    fn config_mounts_seed_default_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("opencode");
        let mounts = opencode_config_mounts(&dir, false).unwrap();
        assert_eq!(mounts.len(), 1);
        assert!(mounts[0].ends_with(":/opencode/config/opencode/opencode.jsonc:ro"));
        assert!(mounts[0].starts_with(&dir.display().to_string()));
        let content = fs::read_to_string(dir.join("opencode.jsonc")).unwrap();
        assert!(content.contains("\"permissions\""));
        assert!(content.contains("\"policies\""));
        assert!(!content.contains("\"experimental\""));
        // The seed is deliberately minimal: the V2 base policy already allows
        // everything by default (asking only for external directories and
        // `.env` reads), so on top of it the seed ships two global rules and
        // one plan-agent exception. The catch-all `*` rule is load-bearing:
        // without it the base policy's `external_directory * -> ask` applies
        // and headless agents get blocked on every external path, including
        // the tmpfs `/tmp` scratch (regression: agents blocked writing /tmp).
        for rule in [
            "{ \"action\": \"shell\", \"resource\": \"git push *\", \"effect\": \"deny\" }",
            "{ \"action\": \"external_directory\", \"resource\": \"*\", \"effect\": \"allow\" }",
            "{ \"action\": \"edit\", \"resource\": \"/tmp/*\", \"effect\": \"allow\" }",
        ] {
            assert!(content.contains(rule), "missing seeded rule: {rule}");
        }
        // The old per-path allowances are gone: `*` matches across `/`, so the
        // catch-all covers the workspace, /tmp, the feature-toolchain homes
        // (`CARGO_HOME`, R's XDG dirs) and opencode's own dirs alike. Keeping
        // the stale rules would invite edits that re-break that contract.
        for rule in [
            "{ \"action\": \"external_directory\", \"resource\": \"/workspace/*\", \"effect\": \"allow\" }",
            "{ \"action\": \"external_directory\", \"resource\": \"/tmp/*\", \"effect\": \"allow\" }",
            "{ \"action\": \"external_directory\", \"resource\": \"/opencode/*\", \"effect\": \"ask\" }",
            "{ \"action\": \"read\", \"resource\": \"*.env\", \"effect\": \"ask\" }",
            "{ \"action\": \"read\", \"resource\": \"/opencode/cargo/credentials*\", \"effect\": \"ask\" }",
        ] {
            assert!(!content.contains(rule), "stale seeded rule still present: {rule}");
        }
        // Policies are the hard-deny layer: tool access to every host secret
        // and to opencode's own state is denied outright, and policies cannot
        // be lifted by project configs or saved approvals. Cargo's credential
        // files and the session DB are read-denied too, not just asked about.
        for rule in [
            "{ \"action\": \"tool.use\", \"resource\": \"edit:/opencode/data/opencode/auth.json\", \"effect\": \"deny\" }",
            "{ \"action\": \"tool.use\", \"resource\": \"read:/opencode/data/opencode/auth.json\", \"effect\": \"deny\" }",
            "{ \"action\": \"tool.use\", \"resource\": \"edit:/opencode/data/opencode/account.json\", \"effect\": \"deny\" }",
            "{ \"action\": \"tool.use\", \"resource\": \"read:/opencode/data/opencode/account.json\", \"effect\": \"deny\" }",
            "{ \"action\": \"tool.use\", \"resource\": \"edit:/opencode/config/opencode/service.json\", \"effect\": \"deny\" }",
            "{ \"action\": \"tool.use\", \"resource\": \"read:/opencode/config/opencode/service.json\", \"effect\": \"deny\" }",
            "{ \"action\": \"tool.use\", \"resource\": \"edit:/opencode/state/opencode/service.json\", \"effect\": \"deny\" }",
            "{ \"action\": \"tool.use\", \"resource\": \"read:/opencode/state/opencode/service.json\", \"effect\": \"deny\" }",
            "{ \"action\": \"tool.use\", \"resource\": \"read:/opencode/cargo/credentials*\", \"effect\": \"deny\" }",
            "{ \"action\": \"tool.use\", \"resource\": \"read:/opencode/config/git/*\", \"effect\": \"deny\" }",
            "{ \"action\": \"tool.use\", \"resource\": \"edit:/opencode/data/opencode/shell/*\", \"effect\": \"deny\" }",
            "{ \"action\": \"tool.use\", \"resource\": \"edit:/opencode/data/opencode/tool-output/*\", \"effect\": \"deny\" }",
            "{ \"action\": \"tool.use\", \"resource\": \"edit:/opencode/data/opencode/opencode.db*\", \"effect\": \"deny\" }",
            "{ \"action\": \"tool.use\", \"resource\": \"read:/opencode/data/opencode/opencode.db*\", \"effect\": \"deny\" }",
        ] {
            assert!(content.contains(rule), "missing seeded policy: {rule}");
        }
        // The broad config-dir edit policy is gone by design: the config files
        // are read-only mounted in locked mode, so only `service.json` -- which
        // must stay writable for the server -- keeps an explicit tripwire.
        assert!(!content.contains("edit:/opencode/config/opencode/*"));
    }

    #[test]
    fn config_mounts_keep_existing_jsonc() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("opencode");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("opencode.jsonc"), "{\"model\":\"x\"}").unwrap();
        let mounts = opencode_config_mounts(&dir, false).unwrap();
        assert_eq!(mounts.len(), 1);
        assert!(mounts[0].ends_with(":/opencode/config/opencode/opencode.jsonc:ro"));
        assert_eq!(
            fs::read_to_string(dir.join("opencode.jsonc")).unwrap(),
            "{\"model\":\"x\"}"
        );
    }

    #[test]
    fn config_mounts_protect_every_existing_config_file() {
        // opencode merges all existing global config files, so all of them
        // must be mounted read-only -- not just the precedence winner.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("opencode");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("opencode.json"), "{\"model\":\"y\"}").unwrap();
        fs::write(dir.join("config.json"), "{\"model\":\"c\"}").unwrap();
        let mounts = opencode_config_mounts(&dir, false).unwrap();
        assert_eq!(mounts.len(), 2);
        assert!(mounts.iter().any(|m| m.ends_with("opencode.json:ro")));
        assert!(mounts.iter().any(|m| m.ends_with("config.json:ro")));
        assert!(
            !dir.join("opencode.jsonc").exists(),
            "no seeding when a config exists"
        );
        // When a jsonc appears later it is mounted too, on top of the others.
        fs::write(dir.join("opencode.jsonc"), "{\"model\":\"z\"}").unwrap();
        let mounts = opencode_config_mounts(&dir, false).unwrap();
        assert_eq!(mounts.len(), 3);
        assert!(mounts.iter().any(|m| m.ends_with("opencode.jsonc:ro")));
        assert_eq!(
            fs::read_to_string(dir.join("opencode.json")).unwrap(),
            "{\"model\":\"y\"}"
        );
    }

    #[test]
    fn config_mounts_skip_when_unwritable() {
        // A file where the config directory would be needed makes seeding fail;
        // the mount is skipped instead of failing the run.
        let tmp = tempfile::tempdir().unwrap();
        let blocker = tmp.path().join("opencode");
        fs::write(&blocker, "x").unwrap();
        assert!(opencode_config_mounts(&blocker, false).unwrap().is_empty());
    }

    #[test]
    fn config_mounts_ignore_directory_shadowing_config() {
        // A directory named like a config file is not a config file; the seed
        // write then fails (the name is taken by a dir), so no mount is
        // produced rather than mounting a directory over the file path.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("opencode");
        fs::create_dir_all(dir.join("opencode.jsonc")).unwrap();
        assert!(opencode_config_mounts(&dir, false).unwrap().is_empty());
    }

    fn has_mount(mounts: &[String], name: &str) -> bool {
        mounts
            .iter()
            .any(|m| m.ends_with(&format!(":/opencode/config/opencode/{name}:ro")))
    }

    #[test]
    fn config_mounts_locked_mode_mounts_entries_but_not_runtime_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("opencode");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("opencode.jsonc"), "{}").unwrap();
        for name in [
            "cli.json",
            "service.json",
            "tui.json",
            "AGENTS.md",
            "package.json",
        ] {
            fs::write(dir.join(name), "{}").unwrap();
        }
        fs::create_dir_all(dir.join("assets")).unwrap();
        fs::write(dir.join("assets/church-bell.mp3"), "").unwrap();
        fs::create_dir_all(dir.join("plugins")).unwrap();
        fs::create_dir_all(dir.join("node_modules")).unwrap();

        let mounts = opencode_config_mounts(&dir, false).unwrap();
        for name in [
            "opencode.jsonc",
            "AGENTS.md",
            "package.json",
            "assets",
            "plugins",
            "node_modules",
        ] {
            assert!(
                has_mount(&mounts, name),
                "missing read-only mount for {name}: {mounts:?}"
            );
        }
        for name in ["cli.json", "service.json", "tui.json"] {
            assert!(
                !has_mount(&mounts, name),
                "runtime file {name} should stay writable: {mounts:?}"
            );
        }
    }

    #[test]
    fn config_mounts_locked_mode_mounts_symlinked_runtime_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("opencode");
        let store = tmp.path().join("store");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&store).unwrap();
        fs::write(dir.join("opencode.jsonc"), "{}").unwrap();
        fs::write(store.join("cli.json"), "{}").unwrap();
        symlink(store.join("cli.json"), dir.join("cli.json")).unwrap();

        let mounts = opencode_config_mounts(&dir, false).unwrap();
        assert!(
            has_mount(&mounts, "cli.json"),
            "symlinked cli.json must be mounted read-only: {mounts:?}"
        );
        assert!(
            mounts
                .iter()
                .any(|m| m.starts_with(&store.display().to_string())),
            "the mount source should be the resolved store path: {mounts:?}"
        );
    }

    #[test]
    fn config_mounts_edit_config_mounts_only_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("opencode");
        let store = tmp.path().join("store");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&store).unwrap();
        fs::write(dir.join("opencode.jsonc"), "{}").unwrap();
        fs::write(dir.join("cli.json"), "{}").unwrap();
        fs::create_dir_all(dir.join("assets")).unwrap();
        fs::write(store.join("AGENTS.md"), "rules").unwrap();
        symlink(store.join("AGENTS.md"), dir.join("AGENTS.md")).unwrap();

        let mounts = opencode_config_mounts(&dir, true).unwrap();
        assert!(
            has_mount(&mounts, "AGENTS.md"),
            "Nix-managed symlink must stay read-only with --edit-config: {mounts:?}"
        );
        for name in ["opencode.jsonc", "cli.json", "assets"] {
            assert!(
                !has_mount(&mounts, name),
                "regular {name} should stay writable with --edit-config: {mounts:?}"
            );
        }
    }

    #[test]
    fn config_mounts_edit_config_mounts_symlinked_runtime_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("opencode");
        let store = tmp.path().join("store");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&store).unwrap();
        fs::write(dir.join("opencode.jsonc"), "{}").unwrap();
        fs::write(store.join("cli.json"), "{}").unwrap();
        symlink(store.join("cli.json"), dir.join("cli.json")).unwrap();

        let mounts = opencode_config_mounts(&dir, true).unwrap();
        assert!(
            has_mount(&mounts, "cli.json"),
            "symlinked cli.json must stay read-only with --edit-config: {mounts:?}"
        );
        assert!(
            !has_mount(&mounts, "opencode.jsonc"),
            "regular config must stay writable with --edit-config: {mounts:?}"
        );
    }

    #[test]
    fn config_mounts_skip_broken_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("opencode");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("opencode.jsonc"), "{}").unwrap();
        symlink(dir.join("missing"), dir.join("AGENTS.md")).unwrap();

        let mounts = opencode_config_mounts(&dir, false).unwrap();
        assert!(
            !has_mount(&mounts, "AGENTS.md"),
            "broken symlinks must be skipped: {mounts:?}"
        );
    }
}
