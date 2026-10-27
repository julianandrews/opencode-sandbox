use anyhow::{anyhow, Context, Result};
use clap::Parser;

mod config;
mod path;
mod podman;
mod r;
mod r_setup;
mod registry;
mod util;

use config::Feature;

#[derive(Parser)]
#[command(
    name = "opencode-sandbox",
    version,
    about = "Run opencode in a Podman container"
)]
struct Cli {
    #[arg(short = 'p', long = "profile")]
    profile: Option<String>,

    #[arg(short = 'P', long = "port")]
    port: Vec<u16>,

    #[arg(short = 'm', long = "mount")]
    mount: Vec<String>,

    #[arg(short = 'a', long = "audio")]
    audio: Option<bool>,

    #[arg(short = 'e', long = "env")]
    env: Vec<String>,

    #[arg(
        short = 'F',
        long = "feature",
        value_parser = clap::value_parser!(Feature)
    )]
    feature: Vec<Feature>,

    #[arg(long = "no-default-features")]
    no_default_features: bool,

    #[arg(
        long = "edit-config",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        help = "Make regular config files and the plugins dir writable so config and plugins can be edited inside the container (default: every config entry is read-only except the runtime-writable cli.json, service.json and tui.json). Nix-managed symlinks stay read-only even with --edit-config. Use --edit-config=false to force locked mode even when a profile enables it"
    )]
    edit_config: Option<bool>,

    #[arg(short = 'L', long = "list-profiles", conflicts_with_all = &["profile", "port", "mount", "audio", "env", "feature", "no_default_features", "edit_config", "build"])]
    list_profiles: bool,

    #[arg(
        long = "build",
        help = "Build the configured feature variants and exit (used by the systemd timer)",
        conflicts_with_all = &["profile", "port", "mount", "audio", "env", "no_default_features", "edit_config"]
    )]
    build: bool,

    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.list_profiles {
        let profiles = config::list_profiles().context("failed to load profiles")?;
        if profiles.is_empty() {
            println!("No profiles configured.");
        } else {
            println!("Available profiles:");
            for name in profiles {
                println!("  {name}");
            }
        }
        return Ok(());
    }

    if cli.build {
        if !cli.args.is_empty() {
            anyhow::bail!("unexpected arguments with --build: {:?}", cli.args);
        }
        return cmd_build(&cli);
    }

    let profile_name = cli.profile.clone();
    let edit_config_flag = cli.edit_config == Some(true);
    let cfg = config::resolve(&config::ResolveOptions {
        profile: cli.profile,
        env: cli.env,
        ports: cli.port,
        mounts: cli.mount,
        audio: cli.audio,
        features: cli.feature,
        no_default_features: cli.no_default_features,
        edit_config: cli.edit_config,
    })?;

    // A profile can silently turn the hatch on for every run; an explicit
    // `--edit-config` is a deliberate per-run decision and stays quiet.
    if cfg.edit_config && !edit_config_flag {
        let via = profile_name
            .as_deref()
            .map(|n| format!(" (via profile '{n}')"))
            .unwrap_or_default();
        eprintln!(
            "\x1b[33mWarning: edit_config is enabled{via}: regular opencode config files and the plugins dir are writable in-container this run; agent writes persist to the host and all future runs. Nix-managed symlinks stay read-only.\x1b[0m"
        );
    }

    check_cwd()?;

    let image = resolve_image(&cfg)?;

    if cfg.features.contains(&Feature::R) {
        r_setup::check_and_restore(&image, &cfg.env_vars)?;
    }

    run_container(&cfg, &image, &cli.args)
}

fn resolve_image(cfg: &config::ResolvedConfig) -> Result<String> {
    let image = config::image_name_for(&cfg.features);
    if config::variants_contain(&cfg.build, &cfg.default_features, &cfg.features) {
        ensure_image(&image, &cfg.features, &cfg.build.base_image())?;
    } else {
        eprintln!(
            "Feature set {image} is not in the [build] variants; rebuilding on every run (add it there to make runs instant)."
        );
        build_variant(&cfg.features, &image, &cfg.build.base_image(), true)?;
    }
    Ok(image)
}

fn run_container(cfg: &config::ResolvedConfig, image: &str, extra_args: &[String]) -> Result<()> {
    let subcommand = extra_args.first().map(String::as_str);
    let config_dir = dirs::config_dir().context("config dir required")?;
    let mut da = podman::PodmanArgs::new();
    da.base(util::is_rootless())
        .xdg_dirs(&config_dir, cfg.edit_config)?
        .workspace()?
        .security(util::selinux_enforcing())
        .network()
        .git_config()?;
    // The TUI runs a private server over stdio in V2, so `-P` only maps a host
    // port when running `serve`.
    if subcommand == Some("serve") {
        da.ports(&cfg.ports);
    } else if !cfg.ports.is_empty() {
        let ports = cfg
            .ports
            .iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!(
            "Warning: -P {ports} ignored: ports only apply to `serve` (the TUI runs over stdio in V2)."
        );
    }
    da.extra_mounts(&cfg.mounts)?
        .audio(cfg.audio)?
        .env_vars(&cfg.env_vars);

    if cfg.features.contains(&Feature::R) {
        da.r_library()?.r_user_dirs()?;
    }
    if cfg.features.contains(&Feature::Rust) {
        da.cargo_home()?;
    }

    let podman_args = da.into_vec();

    let opencode_args = opencode_command(cfg.ports.first().copied(), extra_args);

    let status = podman::command()
        .arg("run")
        .args(&podman_args)
        .arg(image)
        .args(&opencode_args)
        .status()
        .context("failed to execute podman")?;

    std::process::exit(status.code().unwrap_or(1));
}

/// Build the `opencode` argument vector for a run.
///
/// V2 is subcommand-based with a strict root: the TUI (default command) runs a
/// private server over stdio via `--standalone`; `serve` takes the network
/// flags after the subcommand; `run` also runs standalone.
fn opencode_command(port: Option<u16>, extra_args: &[String]) -> Vec<String> {
    let subcommand = extra_args.first().map(String::as_str);
    let mut args = Vec::new();

    match subcommand {
        Some("serve") => {
            args.push("serve".to_string());
            push_hostname_port(&mut args, port);
            args.extend(extra_args.iter().skip(1).cloned());
        }
        Some("run") => {
            args.push("run".to_string());
            push_standalone(&mut args, &extra_args[1..]);
            args.extend(extra_args.iter().skip(1).cloned());
        }
        _ => {
            push_standalone(&mut args, extra_args);
            args.extend(extra_args.iter().cloned());
        }
    }
    args
}

fn push_hostname_port(args: &mut Vec<String>, port: Option<u16>) {
    args.push("--hostname".to_string());
    args.push("0.0.0.0".to_string());
    if let Some(port) = port {
        args.push("--port".to_string());
        args.push(port.to_string());
    }
}

/// Add `--standalone` unless the user already picked a connection mode.
fn push_standalone(args: &mut Vec<String>, rest: &[String]) {
    let explicit = rest.iter().any(|a| a == "--standalone" || a == "--server");
    if !explicit {
        args.push("--standalone".to_string());
    }
}

fn check_cwd() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let home = dirs::home_dir().ok_or_else(|| anyhow!("cannot determine home directory"))?;

    if cwd == home {
        confirm_or_exit(&[
            "\x1b[33mWarning: Current directory is your home directory ($HOME).\x1b[0m".into(),
            "This would mount your entire home directory into the container.".into(),
        ])?;
    } else if !cwd.starts_with(&home) {
        confirm_or_exit(&[
            "\x1b[33mWarning: Current directory is outside your home directory.\x1b[0m".into(),
            format!("Mounting '{}' into the container.", cwd.display()),
        ])?;
    }

    Ok(())
}

fn confirm_or_exit(lines: &[String]) -> Result<()> {
    for line in lines {
        eprintln!("{}", line);
    }
    print!("Proceed anyway? [y/N] ");
    std::io::Write::flush(&mut std::io::stdout())?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    if !input.trim().eq_ignore_ascii_case("y") {
        std::process::exit(1);
    }
    Ok(())
}

fn cmd_build(cli: &Cli) -> Result<()> {
    let cfg = config::load_config()?;
    let variants = if cli.feature.is_empty() {
        config::default_variants(&cfg.build, &cfg.default_features)
    } else {
        vec![cli.feature.clone()]
    };

    let base_image = build_base_image(&cfg);

    let mut seen = std::collections::HashSet::new();
    for features in &variants {
        let name = config::image_name_for(features);
        if !seen.insert(name.clone()) {
            continue;
        }
        let label = if features.is_empty() {
            "no features".to_string()
        } else {
            features
                .iter()
                .map(|f| f.to_string())
                .collect::<Vec<_>>()
                .join("+")
        };
        println!("Building {name} ({label})");
        build_variant(features, &name, &base_image, true)?;
    }
    prune_images()?;
    Ok(())
}

/// The base image for a `--build` run: the configured pin, or the newest
/// OpenCode V2 release tag from the registry, falling back to the default
/// when the registry can't be reached (the daily timer must not hard-fail).
fn build_base_image(cfg: &config::Config) -> String {
    build_base_image_with(cfg, registry::latest_base_image)
}

fn build_base_image_with(
    cfg: &config::Config,
    discover: impl Fn() -> Result<registry::Latest>,
) -> String {
    if let Some(pin) = &cfg.build.base_image {
        println!("Using pinned base image {pin}");
        return pin.clone();
    }
    match discover() {
        Ok(latest) => {
            if let Some(image) = latest.image {
                println!("Newest OpenCode V2 release: {image}");
                image
            } else {
                let reason = if latest.truncated {
                    "registry tag list was cut off before it ended"
                } else {
                    "registry has no 2.x release tags"
                };
                eprintln!(
                    "Warning: {reason}; using default {}. Pin one with [build] base_image.",
                    config::DEFAULT_BASE_IMAGE
                );
                config::DEFAULT_BASE_IMAGE.to_string()
            }
        }
        Err(err) => {
            eprintln!(
                "Warning: could not check the registry for the newest OpenCode V2 release ({err:#}); using default {}. Pin one with [build] base_image.",
                config::DEFAULT_BASE_IMAGE
            );
            config::DEFAULT_BASE_IMAGE.to_string()
        }
    }
}

fn prune_images() -> Result<()> {
    // Podman has no `builder prune --keep-storage`: Buildah's intermediate
    // layers are ordinary dangling images, so `image prune -f` drops the
    // unreferenced ones while keeping layers still used by the variant images.
    let prune = podman::command()
        .args(["image", "prune", "-f"])
        .status()
        .context("failed to run podman image prune")?;
    if !prune.success() {
        anyhow::bail!("podman image prune failed (exit code: {:?})", prune.code());
    }
    Ok(())
}

/// Check whether an image exists locally.
///
/// `podman image exists` returns 0 when the image is present, 1 when it is
/// absent, and 125 when local storage cannot be accessed. That distinction is
/// why this uses it instead of `image inspect`, whose exit code cannot
/// separate "not found" from a real error.
fn image_exists(name: &str) -> Result<bool> {
    let output = podman::command()
        .args(["image", "exists", name])
        .output()
        .context("failed to run podman image exists")?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        code => anyhow::bail!(
            "podman image exists failed (exit code: {code:?}): {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

fn ensure_image(name: &str, features: &[Feature], base_image: &str) -> Result<()> {
    if image_exists(name)? {
        // The tag does not encode the runtime UID, so an image built for a
        // different host user (e.g. the host UID changed, or a shared image
        // cache) would silently start with the wrong passwd entry. The build
        // stamps the UID/GID into a label; rebuild when it is missing or
        // stale.
        if image_user_ids(name)? == Some(current_user_ids()) {
            return Ok(());
        }
        eprintln!(
            "Image '{name}' was built for a different user; rebuilding it for UID:GID {}.",
            current_user_ids()
        );
        return build_variant(features, name, base_image, false);
    }
    eprintln!(
        "Image '{name}' not found locally; building it now (pre-build it with `opencode-sandbox --build` or the systemd timer to keep startups instant)."
    );
    build_variant(features, name, base_image, false)
}

const USER_ID_LABEL: &str = "org.opencode-sandbox.user-ids";

/// The `USER_ID:USER_GID` baked into the image, read from its label. `None`
/// when the image or label is absent (an image predating the label, or a
/// non-Podman error); the caller then rebuilds.
fn image_user_ids(name: &str) -> Result<Option<String>> {
    let output = podman::command()
        .args([
            "image",
            "inspect",
            "--format",
            &format!(r#"{{{{index .Config.Labels "{USER_ID_LABEL}"}}}}"#),
            name,
        ])
        .output()
        .context("failed to run podman image inspect")?;
    if !output.status.success() {
        return Ok(None);
    }
    let label = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if label.is_empty() {
        Ok(None)
    } else {
        Ok(Some(label))
    }
}

fn current_user_ids() -> String {
    format!("{}:{}", util::uid(), util::gid())
}

fn build_variant(features: &[Feature], tag: &str, base_image: &str, pull: bool) -> Result<()> {
    let has_r = features.contains(&Feature::R);
    let has_rust = features.contains(&Feature::Rust);
    let tmp = tempfile::tempdir()?;
    let df = tmp.path().join("Containerfile");
    std::fs::write(&df, include_str!("../sandbox/Containerfile"))?;
    let mut cmd = podman::command();
    cmd.args(["build", "-t", tag]);
    if pull {
        // Bare `--pull` is accepted by Podman and means `always`, but spell the
        // policy out so the intent is obvious.
        cmd.arg("--pull=always");
    }
    cmd.args(["--build-arg", &format!("BASE_IMAGE={base_image}")]);
    cmd.args(["--build-arg", &format!("USER_ID={}", util::uid())]);
    cmd.args(["--build-arg", &format!("USER_GID={}", util::gid())]);
    cmd.args([
        "--label",
        &format!("{USER_ID_LABEL}={}", current_user_ids()),
    ]);
    if has_r {
        cmd.args(["--build-arg", "INCLUDE_R=true"]);
    }
    cmd.args(["--build-arg", &format!("INCLUDE_RUST={has_rust}")]);
    cmd.arg(tmp.path());
    let status = cmd.status().context("failed to run podman build")?;
    if !status.success() {
        anyhow::bail!("podman build failed (exit code: {:?})", status.code());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    const GRPCURL_URL: &str = "https://github.com/fullstorydev/grpcurl/releases/download/v${GRPCURL_VERSION}/grpcurl_${GRPCURL_VERSION}_linux_${GRPCURL_ARCH}.tar.gz";

    /// `uname -m` label -> grpcurl release asset arch. Pairing matters, not just
    /// presence: a swapped mapping still builds, but installs a binary for the
    /// wrong CPU and fails the version-check at the end of the RUN.
    const GRPCURL_ARCH_ARMS: [(&str, &str); 2] = [("x86_64", "x86_64"), ("aarch64", "arm64")];

    /// The embedded Containerfile is the contract for the CLI tools every variant
    /// ships. grpcurl is packaged for Alpine only in edge/testing, so it has to
    /// come from the version-pinned upstream release tarball (covering both
    /// arches), and `openssl` needs its own apk package: `openssl-dev` provides
    /// headers and pkgconfig only, so without it there is no `openssl` binary.
    fn check_extra_tools(containerfile: &str) -> Result<(), String> {
        let (global, build) = containerfile
            .split_once("FROM ${BASE_IMAGE}")
            .ok_or("Containerfile must build FROM ${BASE_IMAGE}")?;
        let build_lines: Vec<&str> = build.lines().collect();

        // The release asset name embeds the version, so `releases/latest` (which
        // dbmate can use) is not an option here and the default must be bumped
        // by hand.
        if !global.lines().any(|l| {
            l.trim()
                .strip_prefix("ARG GRPCURL_VERSION=")
                .is_some_and(|v| v.starts_with(|c: char| c.is_ascii_digit()))
        }) {
            return Err("ARG GRPCURL_VERSION needs a pinned default before FROM (bump it to update grpcurl)".into());
        }

        // Re-declared after FROM with no default (like INCLUDE_R), or the RUN
        // expands an empty version and fetches a 404 URL.
        let declared_at = build_lines
            .iter()
            .position(|l| l.trim_start().starts_with("ARG GRPCURL_VERSION"))
            .ok_or("ARG GRPCURL_VERSION must be re-declared after FROM")?;

        // apk cannot supply grpcurl on a stable base, so the upstream release
        // tarball is what actually fetches the binary.
        if !containerfile.contains(GRPCURL_URL) {
            return Err("grpcurl must be downloaded from its upstream release tarball".into());
        }
        if !containerfile.contains("tar -xzf /tmp/grpcurl.tgz -C /usr/local/bin grpcurl") {
            return Err("the grpcurl tarball must be extracted into /usr/local/bin".into());
        }

        // The case/label pairs, scoped to the RUN that consumes the ARG: the
        // unrelated dbmate block below/above maps `uname -m` to *its* own arch
        // vocabulary (amd64/arm64), so a whole-file scan would let dbmate's arms
        // satisfy this check and miss a deleted grpcurl arm entirely.
        // Case arms carry a trailing `\` continuation; compare the arm body only, so
        // rewrapping the RUN does not read as a mapping change.
        fn case_arm(line: &str) -> &str {
            line.trim().trim_end_matches('\\').trim_end()
        }

        let arm_at = |label: &str| {
            build_lines.iter().position(|l| {
                let t = case_arm(l);
                t.starts_with(&format!("{label}) GRPCURL_ARCH=")) && t.ends_with(";;")
            })
        };
        let mut arms = Vec::new();
        for (label, asset_arch) in GRPCURL_ARCH_ARMS {
            match arm_at(label) {
                None => return Err(format!("grpcurl install must map {label}")),
                Some(line) => {
                    let got = case_arm(build_lines[line]);
                    let want = format!("{label}) GRPCURL_ARCH={asset_arch} ;;");
                    if got != want {
                        return Err(format!("grpcurl install must map `{want}`, found `{got}`"));
                    }
                    arms.push(line);
                }
            }
        }
        // Unsupported arches must abort the build, not silently fall through to
        // an empty asset arch (which would 404 with a far less obvious message).
        if !build_lines[arms[1] + 1..]
            .iter()
            .any(|l| l.trim_start().starts_with("*)"))
        {
            return Err("the grpcurl arch mapping must reject unsupported architectures".into());
        }

        // Declared immediately before its consumer: every RUN after an ARG uses
        // it implicitly as an environment variable, so hoisting this next to
        // INCLUDE_R would make a version bump invalidate the expensive R/Rust
        // RUN above -- the one cost this layer exists to avoid paying.
        let run_at = build_lines
            .iter()
            .position(|l| l.contains("apk add --no-cache openssl"))
            .ok_or("the extra-tools RUN must install the openssl package")?;
        if declared_at + 1 != run_at {
            return Err(format!(
                "ARG GRPCURL_VERSION must be declared immediately before the extra-tools RUN, \
                 but it is {run_at} lines away from it: a version bump would then invalidate \
                 the R/Rust RUN too, which is what this separate layer avoids"
            ));
        }

        // A broken or partial install must fail the build here rather than in
        // the first agent session.
        for check in [
            "grpcurl --version",
            "openssl version",
            "ssh -V",
            "import paramiko",
        ] {
            if !containerfile.contains(check) {
                return Err(format!("the build should end by checking `{check}`"));
            }
        }
        // Token-exact, so the already-present `openssl-dev` cannot satisfy the
        // openssl requirement.
        if !apk_add_has_package(containerfile, "openssl") {
            return Err(
                "openssl must be added as its own apk package (openssl-dev ships no CLI)".into(),
            );
        }
        // openssh-client resolves to openssh-client-default, which is where
        // ssh-keygen comes from; py3-paramiko is Alpine's own build, so it
        // matches the image's python3 instead of a pip one.
        for pkg in ["openssh-client", "py3-paramiko"] {
            if !apk_add_has_package(containerfile, pkg) {
                return Err(format!("{pkg} must be installed in the extra-tools layer"));
            }
        }
        Ok(())
    }

    /// Token-exact `apk add` membership, so `openssl-dev` never counts as
    /// `openssl`.
    fn apk_add_has_package(containerfile: &str, pkg: &str) -> bool {
        containerfile.lines().any(|line| {
            let words: Vec<_> = line.split_whitespace().filter(|w| *w != "RUN").collect();
            words.starts_with(&["apk", "add"]) && words.contains(&pkg)
        })
    }

    /// The runtime user contract. The container always runs as the host UID
    /// (`podman run --userns=keep-id --user`), so the image must create a passwd entry for it --
    /// otherwise ssh-keygen's `getpwuid(getuid())` fatal ("No user exists for
    /// uid") makes ssh tools unusable. The entry's home must also be a real,
    /// writable dir, since ssh-keygen builds its default key path from it.
    fn check_runtime_user(containerfile: &str) -> Result<(), String> {
        let (_, build) = containerfile
            .split_once("FROM ${BASE_IMAGE}")
            .ok_or("Containerfile must build FROM ${BASE_IMAGE}")?;
        let build_lines: Vec<&str> = build.lines().collect();

        // Build args the CLI passes; without them the layer would always use
        // the 1000 default and never match a different host UID.
        for arg in ["ARG USER_ID", "ARG USER_GID"] {
            if !build_lines.iter().any(|l| l.trim_start().starts_with(arg)) {
                return Err(format!("{arg} must be declared after FROM"));
            }
        }

        // The passwd/group creation plus the home directory. Match the
        // load-bearing tokens, so rewrapping the RUN does not break the check.
        let run = build_lines
            .iter()
            .position(|l| l.contains("adduser -D -u \"$USER_ID\""))
            .ok_or("the runtime user must be created with adduser -u \"$USER_ID\"")?;
        // The user-creation RUN is the one line that both creates the user and
        // sets up the home; requiring both on the same logical RUN keeps them
        // from drifting apart.
        let run_block: String = build_lines[run..]
            .iter()
            .take_while(|l| {
                // Stop at the ENV that follows, i.e. include continuations.
                !l.trim_start().starts_with("ENV ")
            })
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        for token in [
            "/opencode/home",
            "getent passwd \"$USER_ID\"",
            "mkdir -p /opencode/home",
            "chown \"$USER_ID:$USER_GID\" /opencode/home",
        ] {
            if !run_block.contains(token) {
                return Err(format!("the runtime-user layer must contain `{token}`"));
            }
        }

        // A build-time smoke check turns a broken user layer into a build
        // failure instead of a first-session ssh-keygen crash. It must run as
        // the new user (not root, which always exists), or it would pass even
        // when the passwd entry is broken.
        if !run_block.contains("su opencode -c 'ssh-keygen -t ed25519") {
            return Err("the runtime-user layer must smoke-test ssh-keygen as the new user".into());
        }
        Ok(())
    }

    #[test]
    fn containerfile_creates_runtime_user() {
        check_runtime_user(include_str!("../sandbox/Containerfile")).unwrap();
    }

    #[test]
    fn image_user_ids_label_is_stable() {
        // The label value must be the exact `uid:gid` string the label is set
        // to at build time; `ensure_image` compares them verbatim.
        assert_eq!(
            current_user_ids(),
            format!("{}:{}", util::uid(), util::gid())
        );
        assert!(!USER_ID_LABEL.is_empty());
    }

    #[test]
    fn runtime_user_contract_detects_regressions() {
        let original = include_str!("../sandbox/Containerfile");
        let mutations: Vec<(&str, String)> = vec![
            (
                "USER_ID arg removed",
                original.replace("ARG USER_ID=1000", "ARG SOMETHING=1000"),
            ),
            (
                "adduser removed",
                original.replace("adduser -D -u \"$USER_ID\"", "true -D -u \"$USER_ID\""),
            ),
            (
                "home chown removed",
                original.replace("chown \"$USER_ID:$USER_GID\" /opencode/home", "true"),
            ),
            (
                "smoke test removed",
                original.replace("ssh-keygen -t ed25519", "true ssh-keygen"),
            ),
            (
                "smoke test runs as root",
                original.replace("su opencode -c 'ssh-keygen", "ssh-keygen"),
            ),
        ];
        for (name, mutated) in mutations {
            assert_ne!(mutated, original, "mutation '{name}' changed nothing");
            assert!(
                check_runtime_user(&mutated).is_err(),
                "mutation '{name}' slipped past the contract check"
            );
        }
    }

    #[test]
    fn containerfile_ships_extra_tools() {
        check_extra_tools(include_str!("../sandbox/Containerfile")).unwrap();
    }

    /// An assertion set that only ever passes proves nothing. Re-run
    /// `check_extra_tools` against deliberately broken copies of the real
    /// Containerfile so each invariant is known to still bite. Every mutation is
    /// derived from the current file (never a hardcoded version or line), so
    /// bumping `GRPCURL_VERSION` -- the documented maintenance step -- keeps
    /// this test green instead of failing on an intended change.
    #[test]
    fn extra_tools_contract_detects_regressions() {
        // Apply `edit` to the Containerfile's lines and rejoin, preserving the
        // trailing newline so the result stays a valid Containerfile.
        fn mutate(text: &str, edit: impl FnOnce(&mut Vec<String>)) -> String {
            let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
            edit(&mut lines);
            let mut out = lines.join("\n");
            if text.ends_with('\n') {
                out.push('\n');
            }
            out
        }
        // Case arms carry a trailing `\` continuation; compare the arm body only, so
        // rewrapping the RUN does not read as a mapping change.
        fn case_arm(line: &str) -> &str {
            line.trim().trim_end_matches('\\').trim_end()
        }

        // Match on the arm body so the trailing `\` continuation does not have
        // to be spelled out in every fixture string.
        let find = |lines: &[String], needle: &str| {
            lines
                .iter()
                .position(|l| case_arm(l) == needle)
                .unwrap_or_else(|| panic!("fixture line not found: {needle}"))
        };
        // The extra-tools layer's `apk add` line, for dropping one package.
        let apk_line = |lines: &[String]| {
            lines
                .iter()
                .position(|l| l.contains("apk add --no-cache openssl"))
                .expect("fixture: extra-tools apk add line")
        };
        let drop_apk_pkg = |text: &str, pkg: &str| {
            mutate(text, |l| {
                let at = apk_line(l);
                l[at] = l[at].replacen(&format!(" {pkg}"), "", 1);
            })
        };

        let original = include_str!("../sandbox/Containerfile");
        let mutations: Vec<(&str, String)> = vec![
            // Losing the re-declaration is the easiest slip when editing the
            // ARG block, and it surfaces only as a 404 at build time.
            (
                "no post-FROM ARG",
                mutate(original, |l| {
                    l.remove(find(l, "ARG GRPCURL_VERSION"));
                }),
            ),
            // An unversioned ARG would silently disable the pin. Matches the
            // declaration generically so bumping the pin stays a no-op here.
            (
                "unpinned ARG",
                mutate(original, |l| {
                    let at = l
                        .iter()
                        .position(|x| x.trim_start().starts_with("ARG GRPCURL_VERSION="))
                        .expect("fixture: pre-FROM GRPCURL_VERSION default");
                    l[at] = "ARG GRPCURL_VERSION=".into();
                }),
            ),
            // The cache bug this contract now guards: hoisting the ARG up with
            // the others is a tidy-looking edit that silently re-couples a
            // version bump to the R/Rust rebuild.
            (
                "ARG hoisted away from its consumer",
                mutate(original, |l| {
                    let at = find(l, "ARG GRPCURL_VERSION");
                    let arg = l.remove(at);
                    let after_include = find(l, "ARG INCLUDE_RUST");
                    l.insert(after_include + 1, arg);
                }),
            ),
            // Dropping one arch arm breaks that platform only.
            (
                "x86_64 arm removed",
                mutate(original, |l| {
                    l.remove(find(l, "x86_64) GRPCURL_ARCH=x86_64 ;;"));
                }),
            ),
            (
                "arm64 arm removed",
                mutate(original, |l| {
                    l.remove(find(l, "aarch64) GRPCURL_ARCH=arm64 ;;"));
                }),
            ),
            // Both arms present but paired wrongly: builds fine on one arch,
            // installs an unrunnable binary on the other.
            (
                "arch labels swapped",
                mutate(original, |l| {
                    let a = find(l, "x86_64) GRPCURL_ARCH=x86_64 ;;");
                    l[a] = l[a].replace("GRPCURL_ARCH=x86_64", "GRPCURL_ARCH=arm64");
                    let b = find(l, "aarch64) GRPCURL_ARCH=arm64 ;;");
                    l[b] = l[b].replace("GRPCURL_ARCH=arm64", "GRPCURL_ARCH=x86_64");
                }),
            ),
            // The dbmate case block has a byte-identical `*)` arm, so this
            // deletes the one that actually follows the grpcurl arms.
            (
                "grpcurl unsupported-arch arm removed",
                mutate(original, |l| {
                    let arm = find(l, "aarch64) GRPCURL_ARCH=arm64 ;;");
                    // Guard the fixture itself: the dbmate block has a
                    // byte-identical `*)` arm earlier in the file.
                    assert!(
                        case_arm(&l[arm + 1])
                            .starts_with(r#"*) echo "Unsupported arch: $ARCH"; exit 1 ;;"#),
                        "unexpected arm after the grpcurl case: {}",
                        case_arm(&l[arm + 1])
                    );
                    l.remove(arm + 1);
                }),
            ),
            // Swapping to an apk-only install is the mistake this contract
            // exists to catch: grpcurl is not packaged for stable Alpine.
            ("tarball URL removed", original.replace(GRPCURL_URL, "")),
            (
                "tarball extraction removed",
                original.replace(
                    "tar -xzf /tmp/grpcurl.tgz -C /usr/local/bin grpcurl",
                    "true",
                ),
            ),
            // Without the smoke checks a bad download or a broken Python
            // package only fails later, in an agent session.
            (
                "grpcurl version-check removed",
                original.replace("grpcurl --version && \\\n", ""),
            ),
            (
                "openssl version-check removed",
                original.replace("openssl version && \\\n", ""),
            ),
            (
                "ssh version-check removed",
                original.replace("ssh -V && \\\n", ""),
            ),
            (
                "paramiko import check removed",
                original.replace("python3 -c 'import paramiko'", "true"),
            ),
            // `openssl-dev` is already there, so a substring check would pass.
            (
                "openssl CLI package removed",
                drop_apk_pkg(original, "openssl"),
            ),
            (
                "openssh-client package removed",
                drop_apk_pkg(original, "openssh-client"),
            ),
            (
                "py3-paramiko package removed",
                drop_apk_pkg(original, "py3-paramiko"),
            ),
        ];

        for (name, mutated) in mutations {
            assert_ne!(mutated, original, "mutation '{name}' changed nothing");
            assert!(
                check_extra_tools(&mutated).is_err(),
                "mutation '{name}' slipped past the contract check"
            );
        }
    }

    #[test]
    fn tui_default_gets_standalone() {
        assert_eq!(opencode_command(None, &s(&[])), s(&["--standalone"]));
        assert_eq!(
            opencode_command(Some(8080), &s(&["--model", "foo/bar"])),
            s(&["--standalone", "--model", "foo/bar"])
        );
    }

    #[test]
    fn tui_with_server_skips_standalone() {
        assert_eq!(
            opencode_command(None, &s(&["--server", "http://localhost:4096"])),
            s(&["--server", "http://localhost:4096"])
        );
        assert_eq!(
            opencode_command(None, &s(&["--standalone", "--model", "foo/bar"])),
            s(&["--standalone", "--model", "foo/bar"])
        );
    }

    #[test]
    fn serve_places_network_flags_after_subcommand() {
        assert_eq!(
            opencode_command(Some(8080), &s(&["serve"])),
            s(&["serve", "--hostname", "0.0.0.0", "--port", "8080"])
        );
        assert_eq!(
            opencode_command(None, &s(&["serve"])),
            s(&["serve", "--hostname", "0.0.0.0"])
        );
        assert_eq!(
            opencode_command(Some(8080), &s(&["serve", "--cors", "http://x"])),
            s(&[
                "serve",
                "--hostname",
                "0.0.0.0",
                "--port",
                "8080",
                "--cors",
                "http://x"
            ])
        );
    }

    #[test]
    fn run_gets_standalone() {
        assert_eq!(
            opencode_command(None, &s(&["run", "hello"])),
            s(&["run", "--standalone", "hello"])
        );
        assert_eq!(
            opencode_command(None, &s(&["run", "--server", "http://x", "hello"])),
            s(&["run", "--server", "http://x", "hello"])
        );
    }

    #[test]
    fn build_base_image_uses_pin_without_discovery() {
        let config: config::Config = toml::from_str(
            r#"
[build]
base_image = "ghcr.io/anomalyco/opencode:2.0.18"
"#,
        )
        .unwrap();
        let discovered = std::cell::Cell::new(false);
        let image = build_base_image_with(&config, || {
            discovered.set(true);
            Ok(registry::Latest {
                image: Some("ghcr.io/anomalyco/opencode:9.9.9".to_string()),
                truncated: false,
            })
        });
        assert_eq!(image, "ghcr.io/anomalyco/opencode:2.0.18");
        assert!(!discovered.get());
    }

    #[test]
    fn build_base_image_uses_discovered_when_unset() {
        let config: config::Config = toml::from_str("").unwrap();
        let image = build_base_image_with(&config, || {
            Ok(registry::Latest {
                image: Some("ghcr.io/anomalyco/opencode:2.0.19".to_string()),
                truncated: false,
            })
        });
        assert_eq!(image, "ghcr.io/anomalyco/opencode:2.0.19");
    }

    #[test]
    fn build_base_image_falls_back_when_registry_has_no_v2_tags() {
        let config: config::Config = toml::from_str("").unwrap();
        let image = build_base_image_with(&config, || {
            Ok(registry::Latest {
                image: None,
                truncated: false,
            })
        });
        assert_eq!(image, config::DEFAULT_BASE_IMAGE);
    }

    #[test]
    fn build_base_image_falls_back_when_tag_list_truncated() {
        let config: config::Config = toml::from_str("").unwrap();
        let image = build_base_image_with(&config, || {
            Ok(registry::Latest {
                image: None,
                truncated: true,
            })
        });
        assert_eq!(image, config::DEFAULT_BASE_IMAGE);
    }

    #[test]
    fn build_base_image_falls_back_when_registry_unreachable() {
        let config: config::Config = toml::from_str("").unwrap();
        let image = build_base_image_with(&config, || Err(anyhow!("registry unreachable")));
        assert_eq!(image, config::DEFAULT_BASE_IMAGE);
    }
}
