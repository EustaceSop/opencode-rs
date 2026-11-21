use std::env;

pub type Lookup<'a> = &'a dyn Fn(&str) -> Option<String>;

pub fn process_env(key: &str) -> Option<String> {
    env::var(key).ok()
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("invalid boolean for {key}: {value:?}")]
pub struct FlagError {
    pub key: String,
    pub value: String,
}

// core/src/flag/flag.ts `truthy`: lower-cased value is "true" or "1".
pub fn truthy(env: Lookup, key: &str) -> bool {
    env(key).is_some_and(|v| matches!(v.to_lowercase().as_str(), "true" | "1"))
}

// core `enabledByExperimental`: unset falls back to OPENCODE_EXPERIMENTAL.
pub fn truthy_or_experimental(env: Lookup, key: &str) -> bool {
    match env(key) {
        None => truthy(env, "OPENCODE_EXPERIMENTAL"),
        Some(_) => truthy(env, key),
    }
}

// Effect `Config.boolean`: case-sensitive literals; anything else is an error.
pub fn effect_bool(env: Lookup, key: &str) -> Result<Option<bool>, FlagError> {
    let Some(value) = env(key) else { return Ok(None) };
    match value.as_str() {
        "true" | "yes" | "on" | "1" | "y" => Ok(Some(true)),
        "false" | "no" | "off" | "0" | "n" => Ok(Some(false)),
        _ => Err(FlagError { key: key.to_string(), value }),
    }
}

fn bool_default(env: Lookup, key: &str) -> Result<bool, FlagError> {
    Ok(effect_bool(env, key)?.unwrap_or(false))
}

// runtime `enabledByExperimental`: absent falls back to Config.boolean(OPENCODE_EXPERIMENTAL).
pub fn effect_bool_or_experimental(env: Lookup, key: &str) -> Result<bool, FlagError> {
    let experimental = bool_default(env, "OPENCODE_EXPERIMENTAL")?;
    Ok(effect_bool(env, key)?.unwrap_or(experimental))
}

// runtime `positiveInteger`: a number that is an integer > 0, otherwise absent.
pub fn positive_int(env: Lookup, key: &str) -> Option<u64> {
    let value = env(key)?.trim().parse::<f64>().ok()?;
    (value.is_finite() && value > 0.0 && value.fract() == 0.0).then_some(value as u64)
}

fn non_empty(env: Lookup, key: &str) -> Option<String> {
    env(key).filter(|v| !v.is_empty())
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Flags {
    pub config: Option<String>,
    pub config_content: Option<String>,
    pub config_dir: Option<String>,
    pub permission: Option<String>,
    pub test_managed_config_dir: Option<String>,
    pub disable_project_config: bool,
    pub disable_autocompact: bool,
    pub disable_prune: bool,
    pub pure: bool,
    pub client: String,
    pub experimental: bool,
    pub auto_share: bool,
    pub disable_default_plugins: bool,
    pub disable_external_skills: bool,
    pub disable_lsp_download: bool,
    pub disable_claude_code_prompt: bool,
    pub disable_claude_code_skills: bool,
    pub enable_exa: bool,
    pub enable_parallel: bool,
    pub enable_experimental_models: bool,
    pub enable_question_tool: bool,
    pub experimental_background_subagents: bool,
    pub experimental_lsp_tool: bool,
    pub experimental_plan_mode: bool,
    pub experimental_code_mode: bool,
    pub output_token_max: Option<u64>,
    pub bash_default_timeout_ms: Option<u64>,
}

impl Flags {
    pub fn from_env() -> Result<Self, FlagError> {
        Self::from_lookup(&process_env)
    }

    pub fn from_lookup(env: Lookup) -> Result<Self, FlagError> {
        let claude = bool_default(env, "OPENCODE_DISABLE_CLAUDE_CODE")?;
        let experimental = bool_default(env, "OPENCODE_EXPERIMENTAL")?;
        Ok(Self {
            config: non_empty(env, "OPENCODE_CONFIG"),
            config_content: non_empty(env, "OPENCODE_CONFIG_CONTENT"),
            config_dir: non_empty(env, "OPENCODE_CONFIG_DIR"),
            permission: non_empty(env, "OPENCODE_PERMISSION"),
            test_managed_config_dir: non_empty(env, "OPENCODE_TEST_MANAGED_CONFIG_DIR"),
            disable_project_config: truthy(env, "OPENCODE_DISABLE_PROJECT_CONFIG"),
            disable_autocompact: truthy(env, "OPENCODE_DISABLE_AUTOCOMPACT"),
            disable_prune: truthy(env, "OPENCODE_DISABLE_PRUNE"),
            pure: bool_default(env, "OPENCODE_PURE")?,
            client: env("OPENCODE_CLIENT").unwrap_or_else(|| "cli".into()),
            experimental,
            auto_share: bool_default(env, "OPENCODE_AUTO_SHARE")?,
            disable_default_plugins: bool_default(env, "OPENCODE_DISABLE_DEFAULT_PLUGINS")?,
            disable_external_skills: bool_default(env, "OPENCODE_DISABLE_EXTERNAL_SKILLS")?,
            disable_lsp_download: bool_default(env, "OPENCODE_DISABLE_LSP_DOWNLOAD")?,
            disable_claude_code_prompt: claude || bool_default(env, "OPENCODE_DISABLE_CLAUDE_CODE_PROMPT")?,
            disable_claude_code_skills: claude || bool_default(env, "OPENCODE_DISABLE_CLAUDE_CODE_SKILLS")?,
            enable_exa: experimental
                || bool_default(env, "OPENCODE_ENABLE_EXA")?
                || bool_default(env, "OPENCODE_EXPERIMENTAL_EXA")?,
            enable_parallel: bool_default(env, "OPENCODE_ENABLE_PARALLEL")?
                || bool_default(env, "OPENCODE_EXPERIMENTAL_PARALLEL")?,
            enable_experimental_models: bool_default(env, "OPENCODE_ENABLE_EXPERIMENTAL_MODELS")?,
            enable_question_tool: bool_default(env, "OPENCODE_ENABLE_QUESTION_TOOL")?,
            experimental_background_subagents: effect_bool_or_experimental(
                env,
                "OPENCODE_EXPERIMENTAL_BACKGROUND_SUBAGENTS",
            )?,
            experimental_lsp_tool: effect_bool_or_experimental(env, "OPENCODE_EXPERIMENTAL_LSP_TOOL")?,
            experimental_plan_mode: effect_bool_or_experimental(env, "OPENCODE_EXPERIMENTAL_PLAN_MODE")?,
            experimental_code_mode: effect_bool_or_experimental(env, "OPENCODE_EXPERIMENTAL_CODE_MODE")?,
            output_token_max: positive_int(env, "OPENCODE_EXPERIMENTAL_OUTPUT_TOKEN_MAX"),
            bash_default_timeout_ms: positive_int(env, "OPENCODE_EXPERIMENTAL_BASH_DEFAULT_TIMEOUT_MS"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn truthy_family() {
        let e = env(&[("A", "TRUE"), ("B", "1"), ("C", "yes"), ("D", ""), ("OPENCODE_EXPERIMENTAL", "true")]);
        assert!(truthy(&e, "A"));
        assert!(truthy(&e, "B"));
        assert!(!truthy(&e, "C"));
        assert!(!truthy(&e, "D"));
        assert!(!truthy(&e, "MISSING"));
        assert!(truthy_or_experimental(&e, "MISSING"));
        assert!(!truthy_or_experimental(&e, "C"));
    }

    #[test]
    fn effect_bool_family() {
        let e = env(&[("A", "yes"), ("B", "off"), ("C", "TRUE"), ("D", "y"), ("E", "n")]);
        assert_eq!(effect_bool(&e, "A"), Ok(Some(true)));
        assert_eq!(effect_bool(&e, "B"), Ok(Some(false)));
        assert_eq!(effect_bool(&e, "D"), Ok(Some(true)));
        assert_eq!(effect_bool(&e, "E"), Ok(Some(false)));
        assert_eq!(effect_bool(&e, "MISSING"), Ok(None));
        assert!(effect_bool(&e, "C").is_err());
    }

    #[test]
    fn positive_int_family() {
        let e = env(&[("A", "12"), ("B", "0"), ("C", "1.5"), ("D", "x"), ("E", "-3"), ("F", "4.0")]);
        assert_eq!(positive_int(&e, "A"), Some(12));
        assert_eq!(positive_int(&e, "F"), Some(4));
        for key in ["B", "C", "D", "E", "MISSING"] {
            assert_eq!(positive_int(&e, key), None);
        }
    }

    #[test]
    fn flags_combine() {
        let e = env(&[
            ("OPENCODE_DISABLE_CLAUDE_CODE", "1"),
            ("OPENCODE_EXPERIMENTAL", "true"),
            ("OPENCODE_EXPERIMENTAL_PLAN_MODE", "false"),
            ("OPENCODE_DISABLE_PROJECT_CONFIG", "True"),
            ("OPENCODE_CONFIG", ""),
        ]);
        let f = Flags::from_lookup(&e).unwrap();
        assert!(f.disable_claude_code_prompt && f.disable_claude_code_skills);
        assert!(f.experimental_lsp_tool && !f.experimental_plan_mode);
        assert!(f.enable_exa && !f.enable_parallel);
        assert!(f.disable_project_config);
        assert_eq!(f.config, None);
        assert_eq!(f.client, "cli");
        assert!(Flags::from_lookup(&env(&[("OPENCODE_PURE", "TRUE")])).is_err());
    }
}
