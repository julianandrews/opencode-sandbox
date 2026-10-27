# opencode-sandbox

Run [opencode](https://opencode.ai) inside a Podman container with your workspace mounted in, XDG state
persisted, and optional feature toolchains (R, Rust).

`opencode-sandbox` is the renamed successor to `opencode-docker`; see the
[migration section](#migrating-from-opencode-docker).

## How it works

Runs use the pre-built image for your feature set (`opencode-sandbox`, `opencode-sandbox-r`,
`opencode-sandbox-rust`, `opencode-sandbox-r-rust`) when that set is one of the `[build]` variants, so startups
are instant. Freshness is maintained out-of-band:

- `opencode-sandbox --build` rebuilds the configured feature variants FROM the newest OpenCode V2 release:
  when `[build] base_image` is unset it queries the registry and uses the newest `2.x` release tag; set
  `base_image` to pin a specific tag instead. This is the only command that queries the registry for the
  release tag. It records a pinned or freshly discovered base in
  `$XDG_DATA_HOME/opencode-sandbox/base-image` (default `~/.local/share/...`); fallback builds keep the
  previous record, and run-time builds reuse it.
- A systemd timer runs `--build` daily (see below), so variant images are at most ~1 day behind the latest
  OpenCode V2 release.
- If a variant image is missing locally (fresh install, pruned cache), a one-time build happens automatically
  on the next run, using only local layers — the base image is pulled only if it is missing too, so variant
  runs never talk to the registry.
- Feature sets that are *not* in `[build]` variants are rebuilt on every run with
  `podman build --pull=always` from the base recorded by the last successful `--build` (or the pin/default
  if no successful build has recorded one). They do not query the release-tag API themselves; the pull still
  checks the configured base tag. Add the set to `variants` to have the daily timer keep it current. The run
  prints a reminder if you use it often.

Rootless Podman typically stores images under `~/.local/share/containers/storage`, separate from Docker's
store. The R/Rust variants are large; see [Rootless storage](#rootless-storage).

## Installation

### Prerequisites

- **Podman ≥ 5.0** (verified against 5.8.9).
- Rootless Podman with a subordinate UID/GID range:

  ```sh
  grep "^$(id -un):" /etc/subuid /etc/subgid
  ```

  If those are empty, add a range (the `shadow`/`uidmap` package provides `newuidmap`/`newgidmap`):

  ```sh
  sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 "$USER"
  podman system migrate
  ```

- `catatonit` (for `--init`) and `pasta` (rootless networking); both are normally pulled in by the Podman
  package.
- Do **not** run `opencode-sandbox` with `sudo`: rootful Podman uses a different image store and the tool is
  designed around the user's rootless store.
- Remote Podman (`CONTAINER_HOST` / `CONTAINER_CONNECTION`) is not supported.

### Binary

```sh
cargo build --release
install -m755 target/release/opencode-sandbox ~/.local/bin/
```

### Nix

The flake exposes `packages.default` and a Home Manager module:

```nix
{
  # Adjust the URL if you have not renamed the repository itself.
  inputs.opencode-sandbox.url = "github:julianandrews/opencode-sandbox";
}
```

```nix
{
  imports = [ inputs.opencode-sandbox.homeManagerModules.default ];

  programs.opencode-sandbox = {
    enable = true;

    # Optional: rebuild the configured image variants daily with a user timer.
    build.enable = true;
    # build.calendar = "daily";
  };
}
```

`packages.default` wraps the CLI with `podman`, so user services do not depend on the host `PATH`. The
module installs the package when either `programs.opencode-sandbox.enable` or `build.enable` is set. It does
not manage `~/.config/opencode-sandbox/profiles.toml`; keep that config and the OpenCode config under
`~/.config/opencode` as usual.

On NixOS, enable Podman and give your user subordinate ID ranges system-wide:

```nix
virtualisation.podman.enable = true;

users.users.<you>.subUidRanges = [ { startUid = 100000; count = 65536; } ];
users.users.<you>.subGidRanges = [ { startGid = 100000; count = 65536; } ];
```

Rootless Podman needs setuid `newuidmap`/`newgidmap`; the Nix package cannot provide those helpers itself.

## Configuration

`~/.config/opencode-sandbox/profiles.toml`:

```toml
# Feature toolchains enabled by default on every run.
default_features = ["R", "Rust"]

# Feature combinations to pre-build with `opencode-sandbox --build`.
# Each entry becomes one image variant; runs for these are instant.
# If unset, defaults to the no-feature image plus `default_features`.
[build]
variants = [[], ["R"], ["Rust"], ["R", "Rust"]]

# Base image (with tag) the variants are built FROM. Leave unset to
# auto-track the newest OpenCode V2 release on every --build (the daily timer
# then keeps variants current without manual bumps). Set it to pin an exact
# tag, e.g. to freeze on a known-good release. When unset, runs and failed
# registry lookups fall back to the compiled-in default (2.0.19). Note that
# the `latest` tag still tracks the V1 release, so always use an explicit V2
# tag when pinning.
# base_image = "ghcr.io/anomalyco/opencode:2.0.19"

[profile.myenv]
env = { FOO = "bar" }
ports = [8080]
# edit_config = true   # make config editable in-container; syncs back on exit (see Permissions)
```

Image names are canonical: features are deduplicated, sorted (R before Rust), and lowercased, so
`opencode-sandbox-r-rust` is the same whether the features came from the CLI, a profile, or
`default_features`. Feature precedence is "most specific wins": a profile's `features` replace
`default_features`, and `--feature` on the command line replaces both. `--no-default-features` drops
`default_features` (a profile or `--feature` still applies on top).

Profile `env` entries (overridable with `--env K=V`) are passed to the container; they are also forwarded to
the R dependency-setup container, so anything your R scripts need (e.g. `HF_DB_URL`) goes in the profile. The
host environment is not forwarded. `R_REPOSITORIES` is available in the setup container (defaults to CRAN)
and can be overridden through a profile's `env` too.

When a project has a `renv.lock`, dependency setup runs on every launch: `renv::restore()` is a fast no-op
when the library already satisfies the lockfile, so a synced project pays ~1-2s and setup self-heals if the
library drifts. There is no cached setup state that can go stale. When the project activates renv (its
`.Rprofile`), the restore targets the renv-managed project library -- the same library the app and tests use
at runtime. If a project has a `renv.lock` but does not activate renv, setup warns loudly and falls back to
the shared site library (`R_LIBS_SITE`, see below), which the runtime always reads.

The config file is strict: unknown fields anywhere (top level, `[build]`, or a profile) fail to parse instead
of being silently ignored.

With the R feature enabled, R's user directories (`tools::R_user_dir()`) are persisted to the host XDG dirs:
`$XDG_CACHE_HOME/R` maps to `/opencode/cache/R`, `$XDG_DATA_HOME/R` to `/opencode/data/R`,
`$XDG_CONFIG_HOME/R` to `/opencode/config/R`, and `$XDG_STATE_HOME/R` to `/opencode/state/R`. Their defaults
are `~/.cache/R`, `~/.local/share/R`, `~/.config/R`, and `~/.local/state/R`. The R dependency-setup container
uses the same XDG dirs, so the renv package cache is shared and warm across setup runs and opencode sessions.
This keeps renv libraries/caches and sass caches writable (the container runs `--read-only`) and warm across
runs. renv's OS-specific path prefix (`RENV_PATHS_PREFIX_AUTO`) keeps Alpine-container artifacts separate
from anything produced by R on the host itself.

The shared site library (`$XDG_DATA_HOME/opencode-sandbox/r-library`, defaulting to
`~/.local/share/opencode-sandbox/r-library`) maps to `R_LIBS_SITE=/opt/r-library` in both the setup and
runtime containers. It is the persistent library for R projects without renv (in-session package installs
survive across runs, since the container `HOME` is ephemeral) and the fallback target for `renv.lock`
projects that do not activate renv.

## The container user

Under rootless Podman, every run uses
`podman run --userns=keep-id --user <host uid>:<host gid>`, so files the agent creates in the workspace are
owned by you on the host. `--userns=keep-id` is what maps your host UID to the same UID inside the container;
without it, bind-mount writes land as an unprivileged subuid (or fail outright). Rootful Podman uses the
plain `--user` mapping instead.

The image is Alpine and the base image only knows root plus system accounts, so the variant builds create a
passwd/group entry for your host UID via the `USER_ID`/`USER_GID` build args. Without that entry, any OpenSSH
tool dies at startup with `No user exists for uid <uid>`: `ssh-keygen` calls `getpwuid(getuid())` before
parsing its arguments, so even `ssh-keygen -f /tmp/k` fails. That entry's home is `/opencode/home`; it is
mounted writable and `HOME` points at it, so `~/.ssh` (and anything else user-scoped) is writable and
persists across runs despite `--read-only`. Host side it lives at
`~/.local/share/opencode-sandbox/home`.

Because the image tag does not encode the UID, each image is stamped with an `org.opencode-sandbox.user-ids`
label and `ensure_image` rebuilds it when the label is missing or does not match the current host user (e.g.
after changing UID). Rebuild the variants with `opencode-sandbox --build`, or just start a run and let it
rebuild once.

### SELinux

On SELinux-enforcing hosts, `opencode-sandbox` adds `--security-opt=label=disable` to the run. Relabelling is
not viable here: the config mounts include canonicalized read-only Nix-store paths and the workspace can be
large. The sandbox is still rootless, read-only, and capability-free, but SELinux separation between the
container and your user session is not active. If you need SELinux confinement, this project is not a fit
without changes.

## OpenCode V2

Runs use the OpenCode V2 release, auto-tracked by `--build` (fallback default `2.0.19`, or pin with
`[build] base_image`). Moving to V2:

- V1 session history is imported into the shared `~/.local/share/opencode/opencode.db` on first V2 run. Make
  a backup of that database before switching if you want a rollback path (V2 is beta and data may be reset).
- Existing `opencode.json(c)` config and `.opencode/` definitions keep working (V2 reads them in-memory).
  Terminal settings are auto-migrated from `tui.json` to `cli.json` on first startup, which is why the
  runtime needs a writable config dir; it gets a per-run staging tree (see Permissions).
- The `~/.local/share/opentui` mount is kept: V2's TUI renders through opentui, which resolves its data path
  to `$XDG_DATA_HOME/opentui` (`/opencode/data/opentui`). Its Tree-sitter highlight worker caches parsers and
  queries there. Removing the mount makes the worker fail silently and markdown/code highlighting degrade to
  raw text for non-root runs.
- opencode resolves its temp dir to `/tmp/opencode` (`opencode debug paths`). The sandbox mounts a nested
  tmpfs there (mode 1777, like `/tmp`): the directory is otherwise created by whichever opencode process runs
  first, and a root-side bootstrap leaves it root-owned and unwritable for the runtime user.
- The TUI runs `opencode --standalone` (a private server over stdio) instead of V1's
  `--hostname 0.0.0.0 --port`, which V2 does not accept on the default command. `serve` still takes
  `--hostname`/`--port`, and `run` runs standalone.

## Permissions

Agents run under a locked-down, opinionated permission policy. It lives in the host's
`~/.config/opencode/opencode.json(c)`; there is no project-level config by default. If no opencode config
file exists, the first run seeds the default policy (kept in `sandbox/opencode.jsonc.template`) host-side
before starting the container — existing configs are never rewritten. If you upgraded from an older
`opencode-docker`, or opencode itself created a bare `$schema`-only config, your live config may predate the
seeded policy; replace it with `sandbox/opencode.jsonc.template` (keeping your model/agent settings) or
delete the config files so the next run re-seeds.

What the default policy does:

- **Reads are quiet for system paths.** The container rootfs is a read-only, ephemeral image with no host
  data, so `/etc`, `/usr`, `/bin`, `/sbin`, `/lib`, `/opt`, `/var` etc. can be read without prompting
  (OS-level protections like `/etc/shadow`'s root-only mode still apply). Real secrets are never asked
  about: credential files (`auth.json`, `account.json`, `service.json` in the config and state dirs, cargo's
  `credentials*`, and the host git config under `/opencode/config/git`, which can embed tokens in
  `insteadOf` URLs or `http.extraheader`) and the session database are denied to the agent's tools outright
  by the top-level `policies`, and `*.env*` files keep prompting (opencode's base policy). (opencode reads
  its own global config directory without prompting.)
- **Tool reads and writes never prompt.** The seed replaces the base policy's `external_directory * -> ask`
  with a single catch-all `external_directory * -> allow`, covering the workspace, the tmpfs `/tmp`, the
  feature-toolchain homes and opencode's own dirs alike (`*` matches across `/`). Without it, headless agents
  (subagents, `opencode run`) are blocked instead of prompted on every external path. The toolchain homes
  (`CARGO_HOME` registry index and crate sources, R's XDG dirs, `/opencode/cache/opencode`) are build cache
  and need no per-path rules.
- **Plan mode keeps its scratch.** The built-in `plan` agent denies all edits except opencode plan files. The
  seed adds a global `edit /tmp/* -> allow` rule (global rules run after the agent's built-in defaults, so it
  covers `plan` too), letting plan mode write tmpfs scratch without touching persistent state. The rule is
  deliberately global rather than an `agents.plan.permissions` entry: when both `agent` and `agents` define
  the same agent, opencode lets the `agents` entry win, so seeding one would silently discard your
  `agent.plan` model/variant/temperature settings. The plan directory itself
  (`~/.local/share/opencode-sandbox/home/.opencode/plan`) is created host-side with the rest of the home, so
  it is writable from the first run.
- **Agents cannot persist new config.** Each run assembles the container's config dir in a per-run staging
  tree rather than bind-mounting the host config dir: every existing entry gets a **real** file or directory
  as its mount target, read-only entries are mounted read-only, writable regular files are staged copies,
  and writable directories exist only with `--edit-config`. Symlinked entries (Nix/Home Manager) are resolved
  and mounted read-only, so they stay visible instead of dangling — the container never sees a symlink as a
  mount destination. New top-level config files created in a locked run are discarded when the sandbox exits;
  only the runtime files `cli.json`, `service.json` and `tui.json` are copied back. That closes the
  "write a new `opencode.json`/agent/plugin into the rw parent" self-escalation path. Hard `deny` `policies`
  additionally deny tool reads of the credential files and the session database, and tool edits of the
  recorded shell/tool-output archives, `service.json` and the session database, regardless of any approval.
  Reads of the shell/tool-output archives are allowed. Policies cannot be overridden by project configs or
  "allow always" approvals.
- **Shell runs without prompting** (V2's allow-by-default base policy keeps the agent productive); only
  `git push` is always denied.

The host config dir (`~/.config/opencode/`) is never bind-mounted read-write. In a locked run every existing
entry is read-only except the runtime-writable regular files `cli.json`, `service.json` and `tui.json` (staged
copies, written back on exit); a symlinked one is read-only like the rest. In-container commands that write
global config (`opencode config set --global`, `opencode mcp add --global`) therefore fail with a read-only
filesystem error by design, and plugin management is host-side: the plugin surfaces — the discovery directory
(`~/.config/opencode/plugins/`), `node_modules` and the package manifests — are mounted read-only, so a
writable plugin target never exists for an agent to persist code that runs on every launch.

Prefer editing from inside the container? Run with `--edit-config` (or set `edit_config = true` in a
profile). It makes every existing regular config file a writable **staged copy** and every existing regular
directory (the plugins dir, `node_modules`, `assets`, `agent`, ...) a writable bind mount. Symlinked entries
stay read-only (they point into the Nix store). Changes to regular files and new top-level files/dirs are
copied back to the host config dir when the sandbox exits; changes inside the writable directories are
immediate. `opencode config set --global`, `opencode plugin add`, `opencode mcp add --global` and direct file
edits therefore work in-container and persist. Deletions and renames of existing entries are not propagated
— edit the host directly for structural changes. Because the sync happens at exit, a forcibly killed CLI can
lose unsynced file edits (directory changes are already on the host). Use it deliberately — in an
`--edit-config` session an agent could also write regular config files via shell — and note that a profile
turning it on prints a warning at launch; `--edit-config=false` forces locked mode back on even for such a
profile.

### Known limitations

- Permission rules govern the agent's *tools*; a shell command still has the container user's full filesystem
  authority. Anything an agent can `sh` can read (credentials, session data, the serve password) and — except
  for the read-only mounts — write. The host config dir itself is not mounted; in-container writes to it land
  in the per-run staging tree (plus the staged runtime files, which are synced back). The hardened mounts
  exist to prevent *self-escalation* (rewriting permissions) and silent plugin persistence, not to sandbox
  hostile code: **the container is the trust boundary**.
- **The config dir is assembled per run.** Writable regular files are staged copies (`cli.json`,
  `service.json` and `tui.json` in locked mode; every regular file with `--edit-config`) and are written back
  when the sandbox exits normally — a SIGKILL loses unsynced file edits. Changes inside existing writable
  directories are immediate. In locked mode, new top-level config files are per-run, and a newly created
  runtime file is copied back. Deletions and renames of existing entries are not propagated. Host changes to
  the config dir during a run are not visible to that run, and a host edit that races the copy-back wins, with
  a warning. Symlinked config entries are always read-only.
- Project-level plugins (`.opencode/plugins/` in a repository the agent writes) cannot be blocked without also
  blocking workspace writes. The global `plugins` control list (`-` prefixes, e.g. `"-*"`) is the only lever
  against them, and a repository can re-enable plugins it declares itself, so treat project plugin runs as
  "the agent wrote your repo".
- **The container may reach your host loopback.** Podman automatically adds `host.containers.internal` and
  `host.docker.internal` to `/etc/hosts`; on rootless pasta they may resolve to a host-accessible address. If
  they do, anything bound to `127.0.0.1` on the host — local databases, dashboards, other `opencode serve`
  instances — is reachable from the sandbox, and the serve password in `service.json` is readable by a
  shell-wielding agent. If your setup needs the gateway mapping, add `--network=pasta:--map-gw` in
  `network()` (or `slirp4netns:allow_host_loopback=true` for slirp4netns). Don't run host services that
  authenticate with secrets you would not hand the agent.
- **The workspace is your real repo**: an agent can write `.git/hooks/` or rewrite remotes, and those execute
  on the **host** at the next push/checkout with your full rights (including ssh-agent). The `git push` deny
  only blocks in-session pushes.
- **`-m/--mount` binds arbitrary host paths read-write** with no allowlist (`~/.ssh`, `/`, ... are accepted);
  mount deliberately, and prefer `:ro`-style care — the passwordless sandbox is not a jail for host
  credentials.
- **Environment values are agent-readable** (`-e`, profile `env`, `/proc/*/environ`); treat anything passed
  via env as public to the session. Credentials belong in the workspace/`auth.json` flows you control, not in
  env.
- **Rootless ports below 1024** cannot be published by default. Use a high host port (e.g. `-P 8080`) or set
  `net.ipv4.ip_unprivileged_port_start` on the host.
- **Rootless storage:** images live under `~/.local/share/containers/storage`. If the kernel lacks rootless
  overlay support and `fuse-overlayfs` is not installed, Podman may fall back to `vfs`, which is much slower
  and uses far more disk for these R/Rust images. Check `podman info` if builds are unexpectedly slow.
- **Remote Podman** (`CONTAINER_HOST`, `podman --remote`, `podman machine`) is not supported.

## Keeping images fresh with systemd

The timer runs `opencode-sandbox --build` once a day and rebuilds only what changed: with
`[build] base_image` unset it resolves the newest OpenCode V2 release and the `--pull=always` build is a cache
hit when there is no new release. Install it for your user:

```sh
install -m644 packaging/opencode-sandbox-build.timer packaging/opencode-sandbox-build.service \
  ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now opencode-sandbox-build.timer
systemctl --user list-timers opencode-sandbox-build.timer
```

Notes:

- The unit runs `%h/.local/bin/opencode-sandbox`, where `%h` is systemd's specifier for your home directory.
  If the binary is installed elsewhere, adjust the `ExecStart` line in
  `opencode-sandbox-build.service`.
- The build runs at idle priority (`Nice=10`, `IOSchedulingClass=idle`) so it never slows down foreground
  work.
- On headless machines, run `loginctl enable-linger $USER` so user units start at boot.
- `Persistent=true` catches up after the machine was off.
- Manual refresh: `opencode-sandbox --build` or `opencode-sandbox --build --feature R` for a single variant.
- New releases are picked up automatically: when `base_image` is unset, `--build` prints the resolved release
  (e.g. `Newest OpenCode V2 release: ghcr.io/anomalyco/opencode:2.2.0`) before building and records it for
  later runs. If the registry is unreachable or its tag list is truncated, `--build` keeps existing variant
  images and uses the last recorded base for any missing one (or the compiled-in fallback if nothing has been
  recorded), so a transient registry failure cannot downgrade working images. Discovery failure alone does
  not fail the timer; a missing image or a build failure can still fail it.
- After building, `--build` runs `podman image prune -f` to drop dangling images and unused Buildah layers.
  Podman has no `--keep-storage` equivalent; monitor `podman system df` if disk usage matters.

## Usage

```sh
opencode-sandbox                          # run in the current directory
opencode-sandbox -p myenv                 # use a profile
opencode-sandbox --feature Rust           # enable Rust toolchain for this run
opencode-sandbox --edit-config            # edit existing config in-container; sync back on exit
opencode-sandbox -P 8080 serve            # run `serve` (headless API + web), passing remaining args to opencode
```

The TUI runs `opencode --standalone`, so each container gets a private server over stdio (no port to expose).
The `-P <port>` flag only applies to `serve`, which runs `opencode serve --hostname 0.0.0.0 --port <port>`.
Note that `serve` prints a random server password you'll need to connect a browser or `opencode --server` from
the host; set `OPENCODE_SERVER_PASSWORD` via a profile `env` to use a fixed one. The V1 `web` subcommand is
gone in V2 (`serve` serves both the API and web UI).

## Migrating from opencode-docker

The rename changes the binary name, config path, data path, image names, label, systemd units and the Nix
option. Docker's image store is separate from Podman's, so images were never shared anyway.

| Old | New |
|---|---|
| `opencode-docker` | `opencode-sandbox` |
| `~/.config/opencode-docker/docker-profiles.toml` | `~/.config/opencode-sandbox/profiles.toml` |
| `~/.local/share/opencode-docker/` | `~/.local/share/opencode-sandbox/` |
| `opencode-local`, `opencode-local-r`, ... | `opencode-sandbox`, `opencode-sandbox-r`, ... |
| `org.opencode-docker.user-ids` | `org.opencode-sandbox.user-ids` |
| `opencode-docker-build.{service,timer}` | `opencode-sandbox-build.{service,timer}` |
| `programs.opencode-docker` | `programs.opencode-sandbox` |
| `docker/` + `Dockerfile` | `sandbox/` + `Containerfile` |

1. Install the new binary as above, then remove the old one:

   ```sh
   rm -f ~/.local/bin/opencode-docker
   ```

2. Move the profile config. Do this **before** the first run under the new name, or the new tool will create
   an empty config and ignore your profiles:

   ```sh
   mkdir -p ~/.config/opencode-sandbox
   mv ~/.config/opencode-docker/docker-profiles.toml ~/.config/opencode-sandbox/profiles.toml
   rmdir ~/.config/opencode-docker 2>/dev/null || true
   ```

   If `~/.config/opencode-sandbox/profiles.toml` already exists, review both files before overwriting the
   new one.

3. Move the persistent data directory. This contains the R site library and `~/.ssh`/home files, so move it
   before running the new version:

   ```sh
   mv ~/.local/share/opencode-docker ~/.local/share/opencode-sandbox
   ```

   If `~/.local/share/opencode-sandbox` already exists, move the two contents instead of the whole
   directory:

   ```sh
   mv ~/.local/share/opencode-docker/r-library ~/.local/share/opencode-sandbox/
   mv ~/.local/share/opencode-docker/home      ~/.local/share/opencode-sandbox/
   ```

4. Rebuild the images. The image names and UID label both changed, so the old images cannot be reused:

   ```sh
   opencode-sandbox --build
   ```

   Remove the old Podman images if you want to reclaim space:

   ```sh
   podman images --format '{{.Repository}}:{{.Tag}}' \
     | grep 'opencode-local' \
     | xargs -r podman rmi
   ```

   If you are coming from the Docker-era version, its images live in Docker's store; remove them separately
   if desired:

   ```sh
   docker images --format '{{.Repository}}:{{.Tag}}' \
     | grep 'opencode-local' \
     | xargs -r docker rmi
   ```

5. Update systemd units:

   ```sh
   systemctl --user disable --now opencode-docker-build.timer 2>/dev/null || true
   rm -f ~/.config/systemd/user/opencode-docker-build.timer \
         ~/.config/systemd/user/opencode-docker-build.service
   install -m644 packaging/opencode-sandbox-build.timer packaging/opencode-sandbox-build.service \
     ~/.config/systemd/user/
   systemctl --user daemon-reload
   systemctl --user enable --now opencode-sandbox-build.timer
   ```

6. Update Nix/Home Manager:

   ```nix
   # Adjust the URL if you have not renamed the repository itself.
   inputs.opencode-sandbox.url = "github:julianandrews/opencode-sandbox";

   imports = [ inputs.opencode-sandbox.homeManagerModules.default ];

   programs.opencode-sandbox = {
     enable = true;
     build.enable = true;
   };
   ```

   Remove the old `programs.opencode-docker` block. If you kept the flake input attribute named
   `opencode-docker`, that is fine (the attribute name is local), but the module options are now
   `programs.opencode-sandbox.*`.

7. Update anything else that mentions the old name: shell aliases, scripts, `AGENTS.md` project notes, systemd
   drop-ins, and the repository/remote name if you choose to rename it. The code itself does not care what the
   checkout directory or GitHub repository is called.

Notes:

- If you already migrated the runtime to Podman under the old name, the same steps apply: the config/data
  paths and image names are what changed, not the Podman setup.
- The old `~/.config/opencode` and `~/.local/share/opencode` directories belong to opencode itself and are
  untouched.
- The seeded permission policy content is unchanged, but the template path is now
  `sandbox/opencode.jsonc.template`.
