use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum Feature {
    R,
    Rust,
}

impl std::fmt::Display for Feature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Feature::R => write!(f, "R"),
            Feature::Rust => write!(f, "Rust"),
        }
    }
}

impl FromStr for Feature {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "R" => Ok(Feature::R),
            "Rust" => Ok(Feature::Rust),
            _ => Err(format!("unknown feature '{s}', expected R or Rust")),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub default_features: Vec<Feature>,
    #[serde(default)]
    pub profile: HashMap<String, Profile>,
    #[serde(default)]
    pub build: BuildConfig,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct BuildConfig {
    /// Feature combinations to pre-build (e.g. via the systemd timer).
    /// Each entry is one image variant. If unset, defaults to `[[], default_features]`
    /// (or `[[]]` when no default features are configured).
    #[serde(default)]
    pub variants: Vec<Vec<Feature>>,
    /// Base image (with tag) the variants are built FROM. When unset,
    /// `--build` auto-tracks the newest opencode V2 release tag from the
    /// registry; set this to pin a specific tag (e.g. to freeze on a
    /// known-good release). Falls back to `DEFAULT_BASE_IMAGE` when the
    /// registry can't be reached or at run time.
    #[serde(default)]
    pub base_image: Option<String>,
}

/// Fallback base image for `--build` when the registry can't be reached, and
/// the base used by run-time builds (which never talk to the registry).
/// Keep in sync with the Containerfile `ARG BASE_IMAGE` default.
pub const DEFAULT_BASE_IMAGE: &str = "ghcr.io/anomalyco/opencode:2.0.19";

impl BuildConfig {
    pub fn base_image(&self) -> String {
        self.base_image
            .clone()
            .unwrap_or_else(|| DEFAULT_BASE_IMAGE.to_string())
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub env: Option<HashMap<String, String>>,
    pub mounts: Option<Vec<String>>,
    pub ports: Option<Vec<u16>>,
    pub audio: Option<bool>,
    pub features: Option<Vec<Feature>>,
    /// Mount the opencode config dir fully read-write so config and plugins
    /// can be edited from inside the container (skip the read-only config
    /// file and plugins mounts). Deliberate escape hatch; the CLI flag
    /// `--edit-config` enables the same for a single run.
    pub edit_config: Option<bool>,
}

fn config_path() -> Option<std::path::PathBuf> {
    Some(dirs::config_dir()?.join("opencode-sandbox/profiles.toml"))
}

pub fn load_config() -> Result<Config> {
    let path = match config_path() {
        Some(p) => p,
        None => return Ok(Config::default()),
    };
    if !path.exists() {
        return Ok(Config::default());
    }
    let contents = fs::read_to_string(&path)
        .with_context(|| format!("failed to read config file '{}'", path.display()))?;
    let config: Config = toml::from_str(&contents)
        .with_context(|| format!("failed to parse config file '{}'", path.display()))?;
    Ok(config)
}

pub fn list_profiles() -> Result<Vec<String>> {
    let config = load_config()?;
    Ok(config.profile.into_keys().collect())
}

/// Canonical feature suffix: dedup, sort in declaration order, lowercase,
/// joined with '-'. Empty for the no-feature variant.
fn feature_suffix(features: &[Feature]) -> String {
    let mut set: Vec<Feature> = features.to_vec();
    set.sort();
    set.dedup();
    if set.is_empty() {
        return String::new();
    }
    set.iter()
        .map(|f| format!("-{}", f.to_string().to_ascii_lowercase()))
        .collect()
}

/// Final image name for a feature set, e.g. "opencode-sandbox-r-rust".
pub fn image_name_for(features: &[Feature]) -> String {
    format!("opencode-sandbox{}", feature_suffix(features))
}

/// Image variants to pre-build: the configured `[build]` variants if set, otherwise the
/// no-feature image plus one per `default_features` combination, or `[[]]` if neither is set.
pub fn default_variants(build: &BuildConfig, default_features: &[Feature]) -> Vec<Vec<Feature>> {
    if !build.variants.is_empty() {
        return build.variants.clone();
    }
    if default_features.is_empty() {
        return vec![vec![]];
    }
    vec![vec![], default_features.to_vec()]
}

/// Whether a feature set is a pre-built variant (matched by canonical image name,
/// so ordering or duplicates in the config don't matter).
pub fn variants_contain(
    build: &BuildConfig,
    default_features: &[Feature],
    features: &[Feature],
) -> bool {
    let image = image_name_for(features);
    default_variants(build, default_features)
        .iter()
        .any(|v| image_name_for(v) == image)
}

#[derive(Debug, Clone, Default)]
pub struct ResolveOptions {
    pub profile: Option<String>,
    pub env: Vec<String>,
    pub ports: Vec<u16>,
    pub mounts: Vec<String>,
    pub audio: Option<bool>,
    pub features: Vec<Feature>,
    pub no_default_features: bool,
    /// `Some(true)`/`Some(false)` from the CLI flag; `None` defers to the
    /// profile value (CLI trumps profile, like `audio`).
    pub edit_config: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub env_vars: HashMap<String, String>,
    pub ports: Vec<u16>,
    pub mounts: Vec<String>,
    pub audio: bool,
    pub features: Vec<Feature>,
    pub build: BuildConfig,
    pub default_features: Vec<Feature>,
    pub edit_config: bool,
}

pub fn resolve(opts: &ResolveOptions) -> Result<ResolvedConfig> {
    Ok(resolve_from(&load_config()?, opts))
}

pub(crate) fn resolve_from(config: &Config, opts: &ResolveOptions) -> ResolvedConfig {
    let profile = opts
        .profile
        .as_deref()
        .and_then(|name| config.profile.get(name).cloned());

    let mut env_vars = profile
        .as_ref()
        .and_then(|p| p.env.clone())
        .unwrap_or_default();
    for e in &opts.env {
        if let Some((k, v)) = e.split_once('=') {
            env_vars.insert(k.to_string(), v.to_string());
        }
    }

    let mut ports = profile
        .as_ref()
        .and_then(|p| p.ports.clone())
        .unwrap_or_default();
    ports.extend(&opts.ports);

    let mut mounts = profile
        .as_ref()
        .and_then(|p| p.mounts.clone())
        .unwrap_or_default();
    mounts.extend(opts.mounts.iter().cloned());

    let audio = opts
        .audio
        .or_else(|| profile.as_ref().and_then(|p| p.audio))
        .unwrap_or(true);

    let features: Vec<Feature> = if !opts.features.is_empty() {
        opts.features.clone()
    } else if let Some(pf) = profile.as_ref().and_then(|p| p.features.clone()) {
        pf
    } else if opts.no_default_features {
        Vec::new()
    } else {
        config.default_features.clone()
    };

    let edit_config = opts
        .edit_config
        .or_else(|| profile.as_ref().and_then(|p| p.edit_config))
        .unwrap_or(false);

    ResolvedConfig {
        env_vars,
        ports,
        mounts,
        audio,
        features,
        build: config.build.clone(),
        default_features: config.default_features.clone(),
        edit_config,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_from_str() {
        assert_eq!("R".parse::<Feature>().unwrap(), Feature::R);
        assert_eq!("Rust".parse::<Feature>().unwrap(), Feature::Rust);
        assert!("r".parse::<Feature>().is_err());
        assert!("rust".parse::<Feature>().is_err());
        assert!("Python".parse::<Feature>().is_err());
    }

    #[test]
    fn feature_deserialize() {
        use serde::de::value::StrDeserializer;
        let r = Feature::deserialize(StrDeserializer::<serde::de::value::Error>::new("R")).unwrap();
        assert_eq!(r, Feature::R);
        let rust =
            Feature::deserialize(StrDeserializer::<serde::de::value::Error>::new("Rust")).unwrap();
        assert_eq!(rust, Feature::Rust);
    }

    #[test]
    fn config_empty_profiles() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.profile.is_empty());
        assert!(config.default_features.is_empty());
        assert!(config.build.variants.is_empty());
    }

    #[test]
    fn config_with_build_variants() {
        let toml_str = r#"
[build]
variants = [["R"], ["Rust"], ["R", "Rust"]]
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(
            config.build.variants,
            vec![
                vec![Feature::R],
                vec![Feature::Rust],
                vec![Feature::R, Feature::Rust],
            ]
        );
    }

    #[test]
    fn base_image_defaults_to_v2() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.build.base_image(), DEFAULT_BASE_IMAGE);
    }

    #[test]
    fn base_image_parses() {
        let toml_str = r#"
[build]
base_image = "ghcr.io/anomalyco/opencode:beta"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.build.base_image(), "ghcr.io/anomalyco/opencode:beta");
    }

    #[test]
    fn image_name_for_none() {
        assert_eq!(image_name_for(&[]), "opencode-sandbox");
    }

    #[test]
    fn image_name_for_single() {
        assert_eq!(image_name_for(&[Feature::Rust]), "opencode-sandbox-rust");
    }

    #[test]
    fn image_name_for_order_independent() {
        assert_eq!(
            image_name_for(&[Feature::R, Feature::Rust]),
            image_name_for(&[Feature::Rust, Feature::R]),
        );
        assert_eq!(
            image_name_for(&[Feature::R, Feature::Rust]),
            "opencode-sandbox-r-rust"
        );
    }

    #[test]
    fn image_name_for_dedup() {
        assert_eq!(
            image_name_for(&[Feature::R, Feature::Rust, Feature::R]),
            "opencode-sandbox-r-rust",
        );
    }

    #[test]
    fn default_variants_configured_wins() {
        let config: Config = toml::from_str(
            r#"
default_features = ["R", "Rust"]
[build]
variants = [["R"]]
"#,
        )
        .unwrap();
        assert_eq!(
            default_variants(&config.build, &config.default_features),
            vec![vec![Feature::R]]
        );
    }

    #[test]
    fn default_variants_derived_from_default_features() {
        let config: Config = toml::from_str(r#"default_features = ["R", "Rust"]"#).unwrap();
        assert_eq!(
            default_variants(&config.build, &config.default_features),
            vec![vec![], vec![Feature::R, Feature::Rust]]
        );
    }

    #[test]
    fn default_variants_empty_falls_back_to_no_features() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(
            default_variants(&config.build, &config.default_features),
            vec![vec![]]
        );
    }

    #[test]
    fn variants_contain_matches_by_canonical_name() {
        let config: Config = toml::from_str(
            r#"
[build]
variants = [["Rust", "R"]]
"#,
        )
        .unwrap();
        assert!(variants_contain(
            &config.build,
            &config.default_features,
            &[Feature::R, Feature::Rust]
        ));
        assert!(variants_contain(
            &config.build,
            &config.default_features,
            &[Feature::Rust, Feature::R, Feature::R]
        ));
        assert!(!variants_contain(
            &config.build,
            &config.default_features,
            &[Feature::R]
        ));
        assert!(!variants_contain(
            &config.build,
            &config.default_features,
            &[]
        ));
    }

    #[test]
    fn variants_contain_default_feature_derived() {
        let config: Config = toml::from_str(r#"default_features = ["R", "Rust"]"#).unwrap();
        assert!(variants_contain(
            &config.build,
            &config.default_features,
            &[Feature::R, Feature::Rust]
        ));
        assert!(variants_contain(
            &config.build,
            &config.default_features,
            &[]
        ));
        assert!(!variants_contain(
            &config.build,
            &config.default_features,
            &[Feature::R]
        ));
    }

    #[test]
    fn config_with_profile() {
        let toml_str = r#"
default_features = ["R", "Rust"]

[profile.myenv]
env = { FOO = "bar" }
ports = [8080, 9090]
audio = false
features = ["R"]

[profile.other]
mounts = ["~/data"]
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.default_features, vec![Feature::R, Feature::Rust]);
        assert_eq!(config.profile.len(), 2);

        let myenv = &config.profile["myenv"];
        assert_eq!(myenv.env.as_ref().unwrap().get("FOO").unwrap(), "bar");
        assert_eq!(myenv.ports.as_ref().unwrap(), &[8080, 9090]);
        assert_eq!(myenv.audio, Some(false));
        assert_eq!(myenv.features.as_ref().unwrap(), &[Feature::R]);
    }

    #[test]
    fn config_rejects_unknown_profile_field() {
        let toml_str = r#"
[profile.intake]
containerfile = "intake"
build_context = "/home/julian/Code/intake"
"#;
        let err = toml::from_str::<Config>(toml_str).unwrap_err();
        assert!(err.to_string().contains("containerfile"));
    }

    #[test]
    fn config_rejects_unknown_build_field() {
        let toml_str = r#"
[build]
variance = [["R"]]
"#;
        let err = toml::from_str::<Config>(toml_str).unwrap_err();
        assert!(err.to_string().contains("variance"));
    }

    #[test]
    fn config_rejects_unknown_top_level_field() {
        let toml_str = r#"
default_feature = ["R"]
"#;
        let err = toml::from_str::<Config>(toml_str).unwrap_err();
        assert!(err.to_string().contains("default_feature"));
    }

    fn run_resolve(
        toml_str: &str,
        profile: Option<&str>,
        cli_features: Vec<Feature>,
        no_default: bool,
    ) -> Vec<Feature> {
        let config: Config = toml::from_str(toml_str).unwrap();
        let opts = ResolveOptions {
            profile: profile.map(String::from),
            env: Vec::new(),
            ports: Vec::new(),
            mounts: Vec::new(),
            audio: None,
            features: cli_features,
            no_default_features: no_default,
            edit_config: None,
        };
        resolve_from(&config, &opts).features
    }

    #[test]
    fn resolve_profile_features_override_defaults() {
        let toml_str = r#"
default_features = ["Rust"]
[profile.harvestflow]
features = ["R"]
"#;
        assert_eq!(
            run_resolve(toml_str, Some("harvestflow"), vec![], false),
            vec![Feature::R]
        );
    }

    #[test]
    fn resolve_cli_features_override_defaults() {
        let toml_str = r#"default_features = ["Rust"]"#;
        assert_eq!(
            run_resolve(toml_str, None, vec![Feature::R], false),
            vec![Feature::R]
        );
    }

    #[test]
    fn resolve_cli_features_override_profile() {
        let toml_str = r#"
default_features = ["Rust"]
[profile.harvestflow]
features = ["R"]
"#;
        assert_eq!(
            run_resolve(toml_str, Some("harvestflow"), vec![Feature::Rust], false),
            vec![Feature::Rust]
        );
    }

    #[test]
    fn resolve_profile_without_features_keeps_defaults() {
        let toml_str = r#"
default_features = ["Rust"]
[profile.plain]
env = { FOO = "bar" }
"#;
        assert_eq!(
            run_resolve(toml_str, Some("plain"), vec![], false),
            vec![Feature::Rust]
        );
    }

    #[test]
    fn resolve_no_default_features_is_empty() {
        let toml_str = r#"default_features = ["Rust"]"#;
        assert_eq!(
            run_resolve(toml_str, None, vec![], true),
            Vec::<Feature>::new()
        );
    }

    #[test]
    fn resolve_no_default_features_profile_features_apply() {
        let toml_str = r#"
default_features = ["Rust"]
[profile.harvestflow]
features = ["R"]
"#;
        assert_eq!(
            run_resolve(toml_str, Some("harvestflow"), vec![], true),
            vec![Feature::R]
        );
    }

    #[test]
    fn resolve_no_default_features_cli_features_apply() {
        let toml_str = r#"default_features = ["Rust"]"#;
        assert_eq!(
            run_resolve(toml_str, None, vec![Feature::R], true),
            vec![Feature::R]
        );
    }

    #[test]
    fn resolve_profile_empty_features_override_defaults() {
        let toml_str = r#"
default_features = ["Rust"]
[profile.plain]
features = []
"#;
        assert_eq!(
            run_resolve(toml_str, Some("plain"), vec![], false),
            Vec::<Feature>::new()
        );
    }

    #[test]
    fn resolve_edit_config_from_flag_or_profile() {
        let toml_str = r#"
[profile.lax]
edit_config = true
[profile.strict]
edit_config = false
"#;
        let config: Config = toml::from_str(toml_str).unwrap();

        let base_opts = || ResolveOptions {
            profile: None,
            env: Vec::new(),
            ports: Vec::new(),
            mounts: Vec::new(),
            audio: None,
            features: Vec::new(),
            no_default_features: false,
            edit_config: None,
        };

        // Default: locked.
        assert!(!resolve_from(&config, &base_opts()).edit_config);
        // Flag true turns it on.
        let flag_on = ResolveOptions {
            edit_config: Some(true),
            ..base_opts()
        };
        assert!(resolve_from(&config, &flag_on).edit_config);
        // Profile true turns it on when the flag is absent.
        let lax = ResolveOptions {
            profile: Some("lax".to_string()),
            ..base_opts()
        };
        assert!(resolve_from(&config, &lax).edit_config);
        // Profile false keeps it off.
        let strict = ResolveOptions {
            profile: Some("strict".to_string()),
            ..base_opts()
        };
        assert!(!resolve_from(&config, &strict).edit_config);
        // CLI beats profile in both directions, like `audio`.
        let strict_flag = ResolveOptions {
            profile: Some("strict".to_string()),
            edit_config: Some(true),
            ..base_opts()
        };
        assert!(resolve_from(&config, &strict_flag).edit_config);
        let lax_flag_off = ResolveOptions {
            profile: Some("lax".to_string()),
            edit_config: Some(false),
            ..base_opts()
        };
        assert!(!resolve_from(&config, &lax_flag_off).edit_config);
    }
}
