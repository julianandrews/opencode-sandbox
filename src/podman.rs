use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::path;
use crate::r;
use crate::util;

/// Default `opencode.jsonc` seeded into the host config dir on first run
/// (source: `sandbox/opencode.jsonc.template`). The file is bind-mounted
/// read-only into the container, so an agent can never rewrite its own
/// permissions, and the `deny` policies inside survive any project config.
const DEFAULT_OPENCODE_CONFIG: &str = include_str!("../sandbox/opencode.jsonc.template");

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

/// The workspace name for a host path. `/` has no final component, so it
/// becomes `root`; this also keeps `-m /` from panicking.
fn path_basename(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "root".to_string())
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

    /// `edit_config` is the deliberate `--edit-config` escape hatch: regular
    /// config files are staged as writable copies and regular directories are
    /// bind-mounted writable so config and plugins can be edited from inside
    /// the container. `sync_config` copies the changes back after the run.
    /// `config_dir` is the host XDG config dir and `staging` is the per-run
    /// directory that becomes the container's `/opencode/config/opencode`;
    /// both are injected so the tests never touch the real ones.
    pub fn xdg_dirs(
        &mut self,
        config_dir: &Path,
        edit_config: bool,
        staging: &Path,
    ) -> Result<ConfigPlan> {
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
            // Created host-side so config entries (and the seed) are owned by
            // the host user. The container gets a per-run staging tree mounted
            // over this path, not the host directory itself.
            config.join("opencode"),
            // The plugins dir and node_modules are created here so they exist
            // as real, host-owned directories to mount (read-only in locked
            // mode; writable only with --edit-config).
            config.join("opencode/plugins"),
            config.join("opencode/node_modules"),
            cache.join("opencode"),
            data.join("opencode"),
            data.join("opencode/log"),
            data.join("opencode/bin"),
            data.join("opentui"),
            state.join("opencode"),
            data.join("opencode-sandbox/r-library"),
            data.join("opencode-sandbox/home"),
            // The built-in plan agent may only write plan files under
            // ~/.opencode/plan. Create it host-side (like the home itself) so
            // it exists on the first plan-mode session and is owned by the host
            // user instead of being created by whatever runs in the container
            // first.
            data.join("opencode-sandbox/home/.opencode/plan"),
        ];
        for dir in &dirs_to_create {
            let _ = std::fs::create_dir_all(dir);
        }

        for (name, value) in r::XDG_ENV {
            self.args.push("-e".into());
            self.args.push(format!("{}={}", name, value));
        }
        // A writable, persistent home for the runtime user. The image bakes a
        // passwd entry matching the host UID (see the Containerfile), whose home
        // is /opencode/home; mounting it here keeps `~/.ssh` and other
        // user-scoped files writable across runs despite `--read-only`. This
        // is what lets ssh-keygen save its default ~/.ssh/id_* path.
        self.args.push("-e".into());
        self.args.push("HOME=/opencode/home".into());
        self.args.push("-v".into());
        self.args.push(format!(
            "{}:/opencode/home",
            data.join("opencode-sandbox/home").display()
        ));
        // The container's config dir is a per-run staging tree, not the host
        // config dir. Every host entry is represented by a real file or
        // directory there and bind-mounted over it, so a mount destination is
        // never a symlink. Nix/Home Manager symlinks point at the store, whose
        // target does not exist inside the container; mounting over the link
        // itself fails with ENOENT, so the entry is materialized instead. This
        // also means new files the sandbox creates never land in the host
        // config dir; `sync_config` copies back only the intended ones.
        self.args.push("-v".into());
        self.args
            .push(format!("{}:/opencode/config/opencode", staging.display()));
        // Seed a default opencode config host-side when none exists yet (the
        // host-side creation keeps the source owned by the host user, like the
        // dirs above). Every existing entry is then materialized in the
        // staging tree: read-only entries are bind-mounted ro, writable
        // regular files are copied (a file bind mount would break atomic
        // temp-file+rename writes) and writable directories are bind-mounted
        // rw. Symlinked entries are always read-only.
        let config_root = config.join("opencode");
        let plan = opencode_config_mounts(&config_root, edit_config, staging)?;
        for mount in &plan.mounts {
            self.args.push("-v".into());
            self.args.push(mount.clone());
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
        Ok(plan)
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
        let basename = path_basename(&cwd);

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
        // opencode's own temp dir (`opencode debug paths` -> tmp /tmp/opencode)
        // is created at startup by whichever opencode process runs first. In
        // this sandbox that process can be a root-side bootstrap, which leaves
        // the directory root-owned and therefore unwritable for the runtime
        // user. A nested tmpfs (mode 1777, like /tmp) shadows that directory
        // and guarantees writable scratch regardless of who created it.
        // `mode=1777` is required: without it Podman copies the mountpoint
        // directory's mode (0755 for a runtime-created dir), and the tmpfs is
        // not writable by the runtime user.
        self.args.push("--tmpfs".into());
        self.args.push("/tmp/opencode:exec,mode=1777".into());
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
        let r_lib = data.join("opencode-sandbox/r-library");

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
        let cwd_basename = path_basename(&cwd);

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

            let target = target_name.unwrap_or_else(|| path_basename(&resolved));

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

/// Regular files staged as writable copies for the runtime in locked mode.
/// A symlink with one of these names is mounted read-only instead: its target
/// lives in the Nix store, where writes fail.
const RUNTIME_WRITABLE_CONFIG: &[&str] = &["cli.json", "service.json", "tui.json"];

/// Recognized global config file names. opencode merges all of them, so all
/// are read-only in locked mode (staged writable copies with `--edit-config`);
/// a directory squatting on one of these names is not a config file and is
/// skipped.
const GLOBAL_CONFIG_FILES: &[&str] = &["opencode.jsonc", "opencode.json", "config.json"];

/// A regular config file staged as a writable copy for one run. `original` is
/// the host content at staging time, used to detect concurrent host edits.
struct WritableConfigFile {
    name: OsString,
    host: PathBuf,
    original: Vec<u8>,
}

/// Everything needed to assemble the container's config dir for a run and to
/// sync the intended writes back afterwards.
#[derive(Default)]
pub struct ConfigPlan {
    mounts: Vec<String>,
    initial: HashSet<OsString>,
    writable: Vec<WritableConfigFile>,
}

/// Materialize the global opencode config dir in `staging` and build the
/// bind mounts that expose it inside the container.
///
/// The staging dir is mounted at `/opencode/config/opencode`. Every existing
/// entry becomes a real file or directory there (never a symlink), so the OCI
/// runtime can create the mount target even when the host entry is a symlink
/// into the Nix store. Regular files that must stay writable are copied into
/// the staging tree (a file bind mount would break atomic temp-file+rename
/// writes) and written back by `sync_config`.
///
/// In a locked run every entry is read-only except regular `cli.json`,
/// `service.json` and `tui.json` (staged writable copies). `--edit-config`
/// also makes regular files writable (staged copies) and regular directories
/// writable (bind-mounted rw). Symlinks are always read-only: their target is
/// in the Nix store. Entries with missing/broken sources are skipped.
fn opencode_config_mounts(
    config_dir: &Path,
    edit_config: bool,
    staging: &Path,
) -> Result<ConfigPlan> {
    let _ = seed_opencode_config(config_dir);

    let entries = match fs::read_dir(config_dir) {
        Ok(entries) => entries,
        Err(err) => {
            eprintln!(
                "Warning: could not read opencode config dir '{}' ({err:#}); running with an empty config dir.",
                config_dir.display()
            );
            return Ok(ConfigPlan::default());
        }
    };

    let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
    entries.sort_by_key(|entry| entry.file_name());

    let mut plan = ConfigPlan {
        initial: entries.iter().map(|entry| entry.file_name()).collect(),
        ..ConfigPlan::default()
    };

    for entry in entries {
        let name = entry.file_name();
        let display = name.to_string_lossy();
        let path = entry.path();
        let is_symlink = entry.file_type().map(|t| t.is_symlink()).unwrap_or(false);

        if GLOBAL_CONFIG_FILES.contains(&display.as_ref()) && path.is_dir() {
            continue;
        }
        if !path.exists() {
            // Broken symlink: there is no readable target to mount.
            eprintln!(
                "Warning: '{}' is not a readable config entry; skipping its mount.",
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
                    "Warning: could not resolve config entry '{}' ({err:#}); skipping its mount.",
                    path.display()
                );
                continue;
            }
        };
        let placeholder = staging.join(&name);

        let writable_file = !is_symlink
            && path.is_file()
            && (edit_config || RUNTIME_WRITABLE_CONFIG.contains(&display.as_ref()));
        if writable_file {
            let original = fs::read(&path)
                .with_context(|| format!("failed to read config entry '{}'", path.display()))?;
            fs::write(&placeholder, &original)
                .with_context(|| format!("failed to stage config entry '{}'", path.display()))?;
            fs::set_permissions(&placeholder, fs::metadata(&path)?.permissions()).with_context(
                || {
                    format!(
                        "failed to set permissions on staged config entry '{}'",
                        placeholder.display()
                    )
                },
            )?;
            plan.writable.push(WritableConfigFile {
                name,
                host: path,
                original,
            });
            continue;
        }

        if source.is_dir() {
            fs::create_dir_all(&placeholder).with_context(|| {
                format!(
                    "failed to create config staging dir '{}'",
                    placeholder.display()
                )
            })?;
        } else {
            fs::File::create(&placeholder).with_context(|| {
                format!(
                    "failed to create config staging file '{}'",
                    placeholder.display()
                )
            })?;
        }

        // Writable directories exist only in --edit-config mode; regular files
        // that are not staged above are read-only, as are all symlinks.
        let read_only = is_symlink || !path.is_dir() || !edit_config;
        plan.mounts.push(format!(
            "{}:/opencode/config/opencode/{}:{}",
            source.display(),
            display,
            if read_only { "ro" } else { "rw" }
        ));
    }

    Ok(plan)
}

/// Write staged config changes back to the host.
///
/// Writable regular files are copied into `staging` by
/// `opencode_config_mounts`; they are written back here unless the host copy
/// changed during the run (the host wins). New top-level entries are copied
/// back in `--edit-config` mode, and the runtime files are copied back in both
/// modes. Symlinks and special files are never copied back. Deletions and
/// renames of existing entries are not propagated: edit the host directly for
/// structural changes.
pub fn sync_config(
    plan: &ConfigPlan,
    config_dir: &Path,
    edit_config: bool,
    staging: &Path,
) -> Result<()> {
    for file in &plan.writable {
        let staged_path = staging.join(&file.name);
        let staged_meta = match fs::symlink_metadata(&staged_path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                eprintln!(
                    "Warning: '{}' was removed in the sandbox; leaving the host copy unchanged.",
                    file.host.display()
                );
                continue;
            }
            Err(err) => {
                eprintln!(
                    "Warning: could not inspect staged config entry '{}': {err:#}",
                    staged_path.display()
                );
                continue;
            }
        };
        if staged_meta.file_type().is_symlink() || !staged_meta.is_file() {
            eprintln!(
                "Warning: '{}' is no longer a regular file in the sandbox; not copying it back.",
                staged_path.display()
            );
            continue;
        }
        let host_meta = match fs::symlink_metadata(&file.host) {
            Ok(meta) => meta,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                eprintln!(
                    "Warning: '{}' disappeared on the host during the run; not recreating it.",
                    file.host.display()
                );
                continue;
            }
            Err(err) => {
                eprintln!(
                    "Warning: could not inspect '{}': {err:#}",
                    file.host.display()
                );
                continue;
            }
        };
        if host_meta.file_type().is_symlink() {
            eprintln!(
                "Warning: '{}' became a symlink on the host; not overwriting it.",
                file.host.display()
            );
            continue;
        }
        let staged = match fs::read(&staged_path) {
            Ok(bytes) => bytes,
            Err(err) => {
                eprintln!(
                    "Warning: could not read staged config entry '{}': {err:#}",
                    staged_path.display()
                );
                continue;
            }
        };
        let host = match fs::read(&file.host) {
            Ok(bytes) => bytes,
            Err(err) => {
                eprintln!("Warning: could not read '{}': {err:#}", file.host.display());
                continue;
            }
        };
        if host != file.original {
            eprintln!(
                "Warning: '{}' changed on the host during the run; keeping the host version.",
                file.host.display()
            );
            continue;
        }
        if staged == host {
            continue;
        }
        // Replace the host file atomically; a failed/partial sync must never
        // leave a torn config file behind.
        let mut tmp = match tempfile::NamedTempFile::new_in(config_dir) {
            Ok(tmp) => tmp,
            Err(err) => {
                eprintln!(
                    "Warning: could not create a temp file next to '{}': {err:#}",
                    file.host.display()
                );
                continue;
            }
        };
        if let Err(err) = tmp.write_all(&staged) {
            eprintln!(
                "Warning: could not write '{}' back: {err:#}",
                file.host.display()
            );
            continue;
        }
        let _ = tmp.as_file().set_permissions(staged_meta.permissions());
        if let Err(err) = tmp.persist(&file.host) {
            eprintln!(
                "Warning: could not replace '{}': {err}",
                file.host.display()
            );
            continue;
        }
    }

    let entries = match fs::read_dir(staging) {
        Ok(entries) => entries,
        Err(err) => {
            eprintln!(
                "Warning: could not read the config staging dir '{}': {err:#}",
                staging.display()
            );
            return Ok(());
        }
    };
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name();
        if plan.initial.contains(&name) {
            continue;
        }
        let display = name.to_string_lossy();
        let meta = match fs::symlink_metadata(entry.path()) {
            Ok(meta) => meta,
            Err(err) => {
                eprintln!(
                    "Warning: could not inspect new config entry '{}': {err:#}",
                    entry.path().display()
                );
                continue;
            }
        };
        if meta.file_type().is_symlink() {
            eprintln!(
                "Warning: not copying symlink '{}' back to the host config dir.",
                entry.path().display()
            );
            continue;
        }
        if !edit_config && (!RUNTIME_WRITABLE_CONFIG.contains(&display.as_ref()) || !meta.is_file())
        {
            continue;
        }
        let dest = config_dir.join(&name);
        if fs::symlink_metadata(&dest).is_ok() {
            eprintln!(
                "Warning: not copying new config entry '{}': it already exists on the host.",
                dest.display()
            );
            continue;
        }
        let result = if meta.is_dir() {
            copy_new_config_dir(&entry.path(), &dest)
        } else if meta.is_file() {
            fs::copy(entry.path(), &dest).map(|_| ()).with_context(|| {
                format!(
                    "failed to copy '{}' to '{}'",
                    entry.path().display(),
                    dest.display()
                )
            })
        } else {
            continue;
        };
        if let Err(err) = result {
            eprintln!(
                "Warning: could not copy new config entry '{}': {err:#}",
                entry.path().display()
            );
        }
    }
    Ok(())
}

/// Recursively copy a new config directory to the host, without overwriting
/// anything that appeared there concurrently and without following or copying
/// symlinks.
fn copy_new_config_dir(src: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)
        .with_context(|| format!("failed to create config dir '{}'", dest.display()))?;
    if let Ok(meta) = fs::metadata(src) {
        let _ = fs::set_permissions(dest, meta.permissions());
    }
    for entry in fs::read_dir(src)
        .with_context(|| format!("failed to read staged config dir '{}'", src.display()))?
    {
        let entry = entry
            .with_context(|| format!("failed to read staged config dir '{}'", src.display()))?;
        let meta = fs::symlink_metadata(entry.path())
            .with_context(|| format!("failed to inspect '{}'", entry.path().display()))?;
        if meta.file_type().is_symlink() {
            eprintln!(
                "Warning: not copying symlink '{}' back to the host config dir.",
                entry.path().display()
            );
            continue;
        }
        let target = dest.join(entry.file_name());
        if fs::symlink_metadata(&target).is_ok() {
            eprintln!(
                "Warning: not overwriting '{}' while copying a new config dir.",
                target.display()
            );
            continue;
        }
        if meta.is_dir() {
            copy_new_config_dir(&entry.path(), &target)?;
        } else if meta.is_file() {
            fs::copy(entry.path(), &target).with_context(|| {
                format!(
                    "failed to copy '{}' to '{}'",
                    entry.path().display(),
                    target.display()
                )
            })?;
            let _ = fs::set_permissions(&target, meta.permissions());
        }
    }
    Ok(())
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
/// default (see `sandbox/opencode.jsonc.template`). A directory or broken
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
        assert!(args.contains(&"/tmp/opencode:exec,mode=1777".to_string()));
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
        let args = xdg_args(tmp.path(), false);
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
        let args = xdg_args(tmp.path(), false);
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
        let args = xdg_args(tmp.path(), false);
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
    fn xdg_dirs_edit_config_mounts_regular_entries_writable() {
        // `--edit-config` is the deliberate escape hatch: regular directories
        // (plugins, node_modules, assets, agent, ...) are bind-mounted rw and
        // regular files are staged as writable copies (a file bind mount would
        // break atomic writes). Nix-managed symlinks stay read-only.
        let tmp = tempfile::tempdir().unwrap();
        let args = xdg_args(tmp.path(), true);
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
                "'{forbidden}' mount should not be read-only with --edit-config: {args:?}"
            );
        }
        assert!(
            args.iter()
                .any(|a| a.ends_with(":/opencode/config/opencode/plugins:rw")),
            "plugins should be writable with --edit-config: {args:?}"
        );
        assert!(
            args.iter()
                .any(|a| a.ends_with(":/opencode/config/opencode/node_modules:rw")),
            "node_modules should be writable with --edit-config: {args:?}"
        );
        // Regular files are staged copies, not mounts, so they do not appear as
        // config mount destinations at all.
        for name in ["opencode.jsonc", "package.json"] {
            let suffix = format!("/opencode/config/opencode/{name}:");
            assert!(
                !args.iter().any(|a| a.contains(&suffix)),
                "{name} should be staged, not mounted: {args:?}"
            );
        }
        // The host config dir is never bind-mounted read-write; the container
        // gets a per-run staging tree instead.
        let host_mount = format!(
            "{}:/opencode/config/opencode",
            tmp.path().join("opencode").display()
        );
        assert!(
            !args.contains(&host_mount),
            "host config dir must not be mounted: {args:?}"
        );
        assert!(
            args.iter()
                .any(|a| a.ends_with(":/opencode/config/opencode")),
            "staging config dir mount missing: {args:?}"
        );
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
    fn path_basename_handles_root_and_regular_paths() {
        assert_eq!(path_basename(Path::new("/")), "root");
        assert_eq!(path_basename(Path::new("/tmp/project")), "project");
    }

    #[test]
    fn extra_mounts_accepts_root_without_target() {
        // `/` has no final path component; it must mount as `root` rather than
        // panicking (README documents that `/` is an accepted mount source).
        let mut da = PodmanArgs::new();
        da.extra_mounts(&["/".to_string()]).unwrap();
        assert!(da
            .into_vec()
            .windows(2)
            .any(|w| w == ["-v".to_string(), "/:/workspace/root".to_string()]));
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
        let mounts = plan_mounts(&dir, false);
        assert_eq!(mounts.len(), 1);
        assert!(mounts[0].ends_with(":/opencode/config/opencode/opencode.jsonc:ro"));
        assert!(mounts[0].starts_with(&dir.display().to_string()));
        let content = fs::read_to_string(dir.join("opencode.jsonc")).unwrap();
        assert!(content.contains("\"permissions\""));
        assert!(content.contains("\"policies\""));
        assert!(!content.contains("\"experimental\""));
        // The seed is deliberately minimal: the V2 base policy already allows
        // everything by default (asking only for external directories and
        // `.env` reads), so on top of it the seed ships three global rules.
        // The catch-all `*` rule is load-bearing: without it the base policy's
        // `external_directory * -> ask` applies and headless agents get
        // blocked on every external path, including the tmpfs `/tmp` scratch
        // (regression: agents blocked writing /tmp).
        //
        // The `/tmp` edit rule is global, not an `agents.plan` override:
        // global rules are evaluated after the plan agent's built-in defaults,
        // so they cover plan mode too. The legacy `agents` key must stay out
        // of the seed -- when `agent` and `agents` define the same agent,
        // opencode lets `agents` win, so seeding `agents.plan` would silently
        // discard a user's `agent.plan` model/variant/temperature settings.
        assert!(!content.contains("\"agents\""));
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
        let mounts = plan_mounts(&dir, false);
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
        let mounts = plan_mounts(&dir, false);
        assert_eq!(mounts.len(), 2);
        assert!(mounts.iter().any(|m| m.ends_with("opencode.json:ro")));
        assert!(mounts.iter().any(|m| m.ends_with("config.json:ro")));
        assert!(
            !dir.join("opencode.jsonc").exists(),
            "no seeding when a config exists"
        );
        // When a jsonc appears later it is mounted too, on top of the others.
        fs::write(dir.join("opencode.jsonc"), "{\"model\":\"z\"}").unwrap();
        let mounts = plan_mounts(&dir, false);
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
        assert!(plan_mounts(&blocker, false).is_empty());
    }

    #[test]
    fn config_mounts_ignore_directory_shadowing_config() {
        // A directory named like a config file is not a config file; the seed
        // write then fails (the name is taken by a dir), so no mount is
        // produced rather than mounting a directory over the file path.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("opencode");
        fs::create_dir_all(dir.join("opencode.jsonc")).unwrap();
        assert!(plan_mounts(&dir, false).is_empty());
    }

    fn has_mount(mounts: &[String], name: &str) -> bool {
        mounts
            .iter()
            .any(|m| m.ends_with(&format!(":/opencode/config/opencode/{name}:ro")))
    }

    fn has_rw_mount(mounts: &[String], name: &str) -> bool {
        mounts
            .iter()
            .any(|m| m.ends_with(&format!(":/opencode/config/opencode/{name}:rw")))
    }

    fn plan_mounts(dir: &Path, edit_config: bool) -> Vec<String> {
        let staging = tempfile::tempdir().unwrap();
        opencode_config_mounts(dir, edit_config, staging.path())
            .unwrap()
            .mounts
    }

    fn xdg_args(config_dir: &Path, edit_config: bool) -> Vec<String> {
        let staging = tempfile::tempdir().unwrap();
        let mut da = PodmanArgs::new();
        da.xdg_dirs(config_dir, edit_config, staging.path())
            .unwrap();
        da.into_vec()
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

        let staging = tempfile::tempdir().unwrap();
        let plan = opencode_config_mounts(&dir, false, staging.path()).unwrap();
        let mounts = &plan.mounts;
        for name in [
            "opencode.jsonc",
            "AGENTS.md",
            "package.json",
            "assets",
            "plugins",
            "node_modules",
        ] {
            assert!(
                has_mount(mounts, name),
                "missing read-only mount for {name}: {mounts:?}"
            );
        }
        for name in ["cli.json", "service.json", "tui.json"] {
            assert!(
                !has_mount(mounts, name) && !has_rw_mount(mounts, name),
                "runtime file {name} should be staged, not mounted: {mounts:?}"
            );
            assert!(
                staging.path().join(name).is_file(),
                "runtime file {name} should be a staged copy: {mounts:?}"
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

        let mounts = plan_mounts(&dir, false);
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
    fn config_mounts_edit_config_mounts_symlinks_ro_and_dirs_rw() {
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

        let staging = tempfile::tempdir().unwrap();
        let plan = opencode_config_mounts(&dir, true, staging.path()).unwrap();
        let mounts = &plan.mounts;
        assert!(
            has_mount(mounts, "AGENTS.md"),
            "Nix-managed symlink must stay read-only with --edit-config: {mounts:?}"
        );
        let agents_staged = fs::symlink_metadata(staging.path().join("AGENTS.md")).unwrap();
        assert!(
            agents_staged.is_file() && !agents_staged.file_type().is_symlink(),
            "symlinked entry needs a real staged destination: {mounts:?}"
        );
        assert!(
            has_rw_mount(mounts, "assets"),
            "a regular directory should be writable with --edit-config: {mounts:?}"
        );
        for name in ["opencode.jsonc", "cli.json"] {
            assert!(
                !has_mount(mounts, name) && !has_rw_mount(mounts, name),
                "regular file {name} should be staged, not mounted: {mounts:?}"
            );
            assert!(
                staging.path().join(name).is_file(),
                "regular file {name} should be a staged copy: {mounts:?}"
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

        let mounts = plan_mounts(&dir, true);
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
    fn config_mounts_symlinked_entry_uses_real_destination() {
        // This is the regression that made Nix/Home Manager configs fail:
        // the mount destination must be a real file/dir in the staging tree,
        // never the host symlink (whose store target is absent in-container).
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("opencode");
        let store = tmp.path().join("store");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(store.join("assets")).unwrap();
        fs::write(store.join("assets/church-bell.mp3"), "x").unwrap();
        fs::write(store.join("opencode.jsonc"), "{}").unwrap();
        symlink(store.join("assets"), dir.join("assets")).unwrap();
        symlink(store.join("opencode.jsonc"), dir.join("opencode.jsonc")).unwrap();

        let staging = tempfile::tempdir().unwrap();
        let plan = opencode_config_mounts(&dir, false, staging.path()).unwrap();

        let assets = fs::symlink_metadata(staging.path().join("assets")).unwrap();
        assert!(
            assets.is_dir() && !assets.file_type().is_symlink(),
            "assets needs a real staged dir"
        );
        let config = fs::symlink_metadata(staging.path().join("opencode.jsonc")).unwrap();
        assert!(
            config.is_file() && !config.file_type().is_symlink(),
            "opencode.jsonc needs a real staged file"
        );
        assert!(has_mount(&plan.mounts, "assets"));
        assert!(has_mount(&plan.mounts, "opencode.jsonc"));
        assert!(
            plan.mounts
                .iter()
                .all(|m| m.starts_with(&store.display().to_string())),
            "mounts should use the resolved store sources: {:?}",
            plan.mounts
        );
    }

    #[test]
    fn config_mounts_edit_config_stages_regular_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("opencode");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("opencode.jsonc"), "{\"model\":\"x\"}").unwrap();
        fs::write(dir.join("package.json"), "{}").unwrap();
        fs::create_dir_all(dir.join("plugins")).unwrap();

        let staging = tempfile::tempdir().unwrap();
        let plan = opencode_config_mounts(&dir, true, staging.path()).unwrap();
        assert_eq!(
            fs::read_to_string(staging.path().join("opencode.jsonc")).unwrap(),
            "{\"model\":\"x\"}"
        );
        assert!(staging.path().join("plugins").is_dir());
        assert!(has_rw_mount(&plan.mounts, "plugins"));
        assert!(!has_rw_mount(&plan.mounts, "opencode.jsonc"));
        assert!(!has_mount(&plan.mounts, "opencode.jsonc"));
        assert_eq!(plan.writable.len(), 2);
    }

    #[test]
    fn sync_config_writes_back_edited_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("opencode");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("cli.json"), "{\"a\":1}").unwrap();

        let staging = tempfile::tempdir().unwrap();
        let plan = opencode_config_mounts(&dir, false, staging.path()).unwrap();
        fs::write(staging.path().join("cli.json"), "{\"a\":2}").unwrap();
        sync_config(&plan, &dir, false, staging.path()).unwrap();
        assert_eq!(
            fs::read_to_string(dir.join("cli.json")).unwrap(),
            "{\"a\":2}"
        );
    }

    #[test]
    fn sync_config_keeps_host_version_on_concurrent_change() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("opencode");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("cli.json"), "{\"a\":1}").unwrap();

        let staging = tempfile::tempdir().unwrap();
        let plan = opencode_config_mounts(&dir, false, staging.path()).unwrap();
        fs::write(dir.join("cli.json"), "{\"host\":true}").unwrap();
        fs::write(staging.path().join("cli.json"), "{\"sandbox\":true}").unwrap();
        sync_config(&plan, &dir, false, staging.path()).unwrap();
        assert_eq!(
            fs::read_to_string(dir.join("cli.json")).unwrap(),
            "{\"host\":true}"
        );
    }

    #[test]
    fn sync_config_copies_new_runtime_file_in_locked_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("opencode");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("opencode.jsonc"), "{}").unwrap();

        let staging = tempfile::tempdir().unwrap();
        let plan = opencode_config_mounts(&dir, false, staging.path()).unwrap();
        fs::write(staging.path().join("service.json"), "{\"password\":\"x\"}").unwrap();
        sync_config(&plan, &dir, false, staging.path()).unwrap();
        assert_eq!(
            fs::read_to_string(dir.join("service.json")).unwrap(),
            "{\"password\":\"x\"}"
        );
    }

    #[test]
    fn sync_config_discards_new_non_runtime_file_in_locked_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("opencode");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("opencode.jsonc"), "{}").unwrap();

        let staging = tempfile::tempdir().unwrap();
        let plan = opencode_config_mounts(&dir, false, staging.path()).unwrap();
        fs::write(staging.path().join("opencode.json"), "{}").unwrap();
        sync_config(&plan, &dir, false, staging.path()).unwrap();
        assert!(!dir.join("opencode.json").exists());
    }

    #[test]
    fn sync_config_copies_new_entries_in_edit_config() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("opencode");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("opencode.jsonc"), "{}").unwrap();

        let staging = tempfile::tempdir().unwrap();
        let plan = opencode_config_mounts(&dir, true, staging.path()).unwrap();
        fs::write(staging.path().join("opencode.json"), "{\"model\":\"y\"}").unwrap();
        fs::create_dir_all(staging.path().join("agent")).unwrap();
        fs::write(staging.path().join("agent/review.md"), "rules").unwrap();
        sync_config(&plan, &dir, true, staging.path()).unwrap();
        assert_eq!(
            fs::read_to_string(dir.join("opencode.json")).unwrap(),
            "{\"model\":\"y\"}"
        );
        assert_eq!(
            fs::read_to_string(dir.join("agent/review.md")).unwrap(),
            "rules"
        );
    }

    #[test]
    fn sync_config_skips_new_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("opencode");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("opencode.jsonc"), "{}").unwrap();

        let staging = tempfile::tempdir().unwrap();
        let plan = opencode_config_mounts(&dir, true, staging.path()).unwrap();
        symlink("/etc/passwd", staging.path().join("passwd")).unwrap();
        sync_config(&plan, &dir, true, staging.path()).unwrap();
        assert!(
            !dir.join("passwd").exists(),
            "new symlinks must not be copied back"
        );
    }

    #[test]
    fn sync_config_does_not_overwrite_new_host_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("opencode");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("opencode.jsonc"), "{}").unwrap();

        let staging = tempfile::tempdir().unwrap();
        let plan = opencode_config_mounts(&dir, true, staging.path()).unwrap();
        fs::write(staging.path().join("AGENTS.md"), "sandbox").unwrap();
        fs::write(dir.join("AGENTS.md"), "host").unwrap();
        sync_config(&plan, &dir, true, staging.path()).unwrap();
        assert_eq!(fs::read_to_string(dir.join("AGENTS.md")).unwrap(), "host");
    }

    #[test]
    fn sync_config_leaves_host_file_when_staged_copy_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("opencode");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("cli.json"), "{\"a\":1}").unwrap();

        let staging = tempfile::tempdir().unwrap();
        let plan = opencode_config_mounts(&dir, false, staging.path()).unwrap();
        fs::remove_file(staging.path().join("cli.json")).unwrap();
        sync_config(&plan, &dir, false, staging.path()).unwrap();
        assert_eq!(
            fs::read_to_string(dir.join("cli.json")).unwrap(),
            "{\"a\":1}"
        );
    }

    #[test]
    fn sync_config_does_not_follow_host_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("opencode");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("cli.json"), "{\"a\":1}").unwrap();
        let target = tmp.path().join("target");
        fs::write(&target, "secret").unwrap();

        let staging = tempfile::tempdir().unwrap();
        let plan = opencode_config_mounts(&dir, false, staging.path()).unwrap();
        fs::remove_file(dir.join("cli.json")).unwrap();
        symlink(&target, dir.join("cli.json")).unwrap();
        fs::write(staging.path().join("cli.json"), "{\"a\":2}").unwrap();
        sync_config(&plan, &dir, false, staging.path()).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "secret");
        assert!(fs::symlink_metadata(dir.join("cli.json"))
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn config_mounts_skip_broken_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("opencode");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("opencode.jsonc"), "{}").unwrap();
        symlink(dir.join("missing"), dir.join("AGENTS.md")).unwrap();

        let mounts = plan_mounts(&dir, false);
        assert!(
            !has_mount(&mounts, "AGENTS.md"),
            "broken symlinks must be skipped: {mounts:?}"
        );
    }
}
