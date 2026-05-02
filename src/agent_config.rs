//! `[agent]` block parser for the global `jjr` config file.
//!
//! Schema: `tool` (string, default `claude`) and `extra_args` (list of
//! strings, default `[]`). Any read or parse failure (missing file, malformed
//! TOML, wrong field type) silently falls back to defaults.

use crate::util::load_global_config_table;

pub(crate) const DEFAULT_TOOL: &str = "claude";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentConfig {
    pub(crate) tool: String,
    pub(crate) extra_args: Vec<String>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            tool: DEFAULT_TOOL.to_owned(),
            extra_args: Vec::new(),
        }
    }
}

#[must_use]
pub(crate) fn load_agent_config() -> AgentConfig {
    let Some(table) = load_global_config_table() else {
        return AgentConfig::default();
    };
    let Some(agent) = table.get("agent").and_then(toml::Value::as_table) else {
        return AgentConfig::default();
    };

    let tool = agent
        .get("tool")
        .and_then(toml::Value::as_str)
        .map_or_else(|| DEFAULT_TOOL.to_owned(), str::to_owned);

    let extra_args = agent
        .get("extra_args")
        .and_then(toml::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();

    AgentConfig { tool, extra_args }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::{env_lock, write_global_config_at, EnvGuard};

    #[test]
    fn agent_config_missing_file_defaults() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist.toml");
        let _g = EnvGuard::set_path("JJR_CONFIG_PATH", &missing);
        assert_eq!(load_agent_config(), AgentConfig::default());
    }

    #[test]
    fn agent_config_missing_section_defaults() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let path = write_global_config_at(dir.path(), "[ui]\ntransition_screen = \"auto\"\n");
        let _g = EnvGuard::set_path("JJR_CONFIG_PATH", &path);
        assert_eq!(load_agent_config(), AgentConfig::default());
    }

    #[test]
    fn agent_config_parses_tool_and_extra_args() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let path = write_global_config_at(
            dir.path(),
            "[agent]\ntool = \"opencode\"\nextra_args = [\"--auto-approve\", \"--verbose\"]\n",
        );
        let _g = EnvGuard::set_path("JJR_CONFIG_PATH", &path);
        let cfg = load_agent_config();
        assert_eq!(cfg.tool, "opencode");
        assert_eq!(
            cfg.extra_args,
            vec!["--auto-approve".to_owned(), "--verbose".to_owned()]
        );
    }

    #[test]
    fn agent_config_parses_extra_args_only() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let path = write_global_config_at(
            dir.path(),
            "[agent]\nextra_args = [\"--dangerously-skip-permissions\"]\n",
        );
        let _g = EnvGuard::set_path("JJR_CONFIG_PATH", &path);
        let cfg = load_agent_config();
        assert_eq!(cfg.tool, "claude");
        assert_eq!(
            cfg.extra_args,
            vec!["--dangerously-skip-permissions".to_owned()]
        );
    }

    #[test]
    fn agent_config_malformed_toml_defaults() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let path = write_global_config_at(dir.path(), "[agent\nbroken");
        let _g = EnvGuard::set_path("JJR_CONFIG_PATH", &path);
        assert_eq!(load_agent_config(), AgentConfig::default());
    }

    #[test]
    fn agent_config_section_not_a_table_defaults() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let path = write_global_config_at(dir.path(), "agent = \"claude\"\n");
        let _g = EnvGuard::set_path("JJR_CONFIG_PATH", &path);
        assert_eq!(load_agent_config(), AgentConfig::default());
    }

    #[test]
    fn agent_config_tool_wrong_type_defaults() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let path = write_global_config_at(dir.path(), "[agent]\ntool = 42\n");
        let _g = EnvGuard::set_path("JJR_CONFIG_PATH", &path);
        let cfg = load_agent_config();
        assert_eq!(cfg.tool, "claude");
        assert!(cfg.extra_args.is_empty());
    }

    #[test]
    fn agent_config_extra_args_wrong_type_defaults() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let path = write_global_config_at(dir.path(), "[agent]\nextra_args = \"not-a-list\"\n");
        let _g = EnvGuard::set_path("JJR_CONFIG_PATH", &path);
        let cfg = load_agent_config();
        assert!(cfg.extra_args.is_empty());
    }

    #[test]
    fn agent_config_drops_non_string_array_entries() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let path = write_global_config_at(
            dir.path(),
            "[agent]\nextra_args = [\"--keep\", 42, \"--also-keep\"]\n",
        );
        let _g = EnvGuard::set_path("JJR_CONFIG_PATH", &path);
        let cfg = load_agent_config();
        assert_eq!(
            cfg.extra_args,
            vec!["--keep".to_owned(), "--also-keep".to_owned()]
        );
    }
}
