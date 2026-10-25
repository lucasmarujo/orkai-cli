use std::io;

use serde::{Deserialize, Serialize};

use crate::fsutil;
use crate::model::AgentKind;
use crate::paths::Home;
use crate::pwa::bridge;
use crate::pwa::tunnel::TunnelKind;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct Config {
    pub shell: Option<String>,
    pub shell_args: Vec<String>,
    pub claude_command: String,
    pub claude_args: Vec<String>,
    pub codex_command: String,
    pub codex_args: Vec<String>,
    pub kilo_command: String,
    pub kilo_args: Vec<String>,
    pub opencode_command: String,
    pub opencode_args: Vec<String>,
    pub agy_command: String,
    pub agy_args: Vec<String>,
    pub grok_command: String,
    pub grok_args: Vec<String>,
    pub qwen_command: String,
    pub qwen_args: Vec<String>,
    pub kimi_command: String,
    pub kimi_args: Vec<String>,
    pub notify: bool,
    pub notify_sound: bool,
    pub notify_command: Option<String>,
    pub notify_args: Vec<String>,
    pub pwa_tunnel: TunnelKind,
    pub pwa_port: u16,
    pub language: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            shell: None,
            shell_args: Vec::new(),
            claude_command: "claude".to_string(),
            claude_args: Vec::new(),
            codex_command: "codex".to_string(),
            codex_args: Vec::new(),
            kilo_command: "kilo".to_string(),
            kilo_args: Vec::new(),
            opencode_command: "opencode".to_string(),
            opencode_args: Vec::new(),
            agy_command: "agy".to_string(),
            agy_args: Vec::new(),
            grok_command: "grok".to_string(),
            grok_args: Vec::new(),
            qwen_command: "qwen".to_string(),
            qwen_args: Vec::new(),
            kimi_command: "kimi".to_string(),
            kimi_args: Vec::new(),
            notify: true,
            notify_sound: true,
            notify_command: None,
            notify_args: Vec::new(),
            pwa_tunnel: TunnelKind::default(),
            pwa_port: bridge::DEFAULT_PORT,
            language: None,
        }
    }
}

impl Config {
    pub fn agent(&self, kind: AgentKind) -> Option<(&str, &[String])> {
        let (command, args) = match kind {
            AgentKind::Claude => (&self.claude_command, &self.claude_args),
            AgentKind::Codex => (&self.codex_command, &self.codex_args),
            AgentKind::Kilo => (&self.kilo_command, &self.kilo_args),
            AgentKind::Opencode => (&self.opencode_command, &self.opencode_args),
            AgentKind::Antigravity => (&self.agy_command, &self.agy_args),
            AgentKind::Grok => (&self.grok_command, &self.grok_args),
            AgentKind::Qwen => (&self.qwen_command, &self.qwen_args),
            AgentKind::Kimi => (&self.kimi_command, &self.kimi_args),
            AgentKind::Terminal => return None,
        };
        Some((command.as_str(), args.as_slice()))
    }

    pub fn load_or_init(home: &Home) -> io::Result<Self> {
        let path = home.config_file();
        match fsutil::read_json_opt::<Config>(&path)? {
            Some(config) => Ok(config),
            None => {
                let config = Config::default();
                fsutil::write_json_atomic(&path, &config)?;
                Ok(config)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_config_uses_defaults_for_missing_fields() {
        let config: Config = serde_json::from_str(r#"{"claude_args":["--model","opus"]}"#).unwrap();
        assert_eq!(config.claude_command, "claude");
        assert_eq!(config.claude_args, vec!["--model", "opus"]);
        assert_eq!(config.codex_command, "codex");
        assert_eq!(config.kimi_command, "kimi");
        assert!(config.kilo_args.is_empty());
        assert!(config.shell.is_none());
        assert!(config.notify);
        assert!(config.notify_sound);
        assert!(config.notify_command.is_none());
        assert!(config.notify_args.is_empty());
        assert_eq!(config.pwa_tunnel, TunnelKind::Cloudflare);
        assert_eq!(config.pwa_port, bridge::DEFAULT_PORT);
    }

    #[test]
    fn the_pwa_can_use_tailscale_on_another_port() {
        let config: Config = serde_json::from_str(r#"{"pwa_tunnel":"tailscale","pwa_port":9100}"#).unwrap();
        assert_eq!(config.pwa_tunnel, TunnelKind::Tailscale);
        assert_eq!(config.pwa_port, 9100);
    }

    #[test]
    fn language_can_be_configured() {
        let config: Config = serde_json::from_str(r#"{"language":"pt-BR"}"#).unwrap();
        assert_eq!(config.language.as_deref(), Some("pt-BR"));
    }

    #[test]
    fn notifications_can_be_turned_off_or_sent_to_a_command() {
        let config: Config = serde_json::from_str(
            r#"{"notify":false,"notify_sound":false,"notify_command":"ntfy","notify_args":["publish","orkai"]}"#,
        )
        .unwrap();
        assert!(!config.notify);
        assert!(!config.notify_sound);
        assert_eq!(config.notify_command.as_deref(), Some("ntfy"));
        assert_eq!(config.notify_args, vec!["publish", "orkai"]);
    }

    #[test]
    fn every_ai_agent_has_its_own_command_and_args() {
        let config: Config = serde_json::from_str(
            r#"{"opencode_command":"/opt/oc","opencode_args":["--model","x/y"],"grok_args":["--minimal"]}"#,
        )
        .unwrap();
        let args = ["--model".to_string(), "x/y".to_string()];
        assert_eq!(config.agent(AgentKind::Opencode), Some(("/opt/oc", &args[..])));
        assert_eq!(config.agent(AgentKind::Grok).unwrap().1, ["--minimal"]);
        assert_eq!(config.agent(AgentKind::Terminal), None);
        let defaults = Config::default();
        for kind in AgentKind::ALL.into_iter().filter(|kind| kind.is_ai()) {
            let (command, _) = defaults.agent(kind).unwrap();
            assert_eq!(command, kind.label(), "the default command of {kind:?} is its own name");
        }
    }

    #[test]
    fn load_or_init_writes_default_file() {
        let dir = std::env::temp_dir().join(format!("orkai-config-{}-{}", std::process::id(), crate::util::now_ms()));
        let home = Home::at(&dir);
        home.ensure().unwrap();
        let config = Config::load_or_init(&home).unwrap();
        assert_eq!(config, Config::default());
        assert!(home.config_file().exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
