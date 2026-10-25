use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub type AgentId = u32;

pub const DEFAULT_SESSION: &str = "default";
pub const MAX_SESSION_NAME_LEN: usize = 32;
pub const MAX_AGENT_NAME_LEN: usize = 40;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    Claude,
    Codex,
    Kilo,
    Opencode,
    Antigravity,
    Grok,
    Qwen,
    Kimi,
    Terminal,
}

impl AgentKind {
    pub const ALL: [AgentKind; 9] = [
        AgentKind::Claude,
        AgentKind::Codex,
        AgentKind::Kilo,
        AgentKind::Opencode,
        AgentKind::Antigravity,
        AgentKind::Grok,
        AgentKind::Qwen,
        AgentKind::Kimi,
        AgentKind::Terminal,
    ];

    pub fn label(self) -> &'static str {
        match self {
            AgentKind::Claude => "claude",
            AgentKind::Codex => "codex",
            AgentKind::Kilo => "kilo",
            AgentKind::Opencode => "opencode",
            AgentKind::Antigravity => "agy",
            AgentKind::Grok => "grok",
            AgentKind::Qwen => "qwen",
            AgentKind::Kimi => "kimi",
            AgentKind::Terminal => "terminal",
        }
    }

    pub fn is_ai(self) -> bool {
        !matches!(self, AgentKind::Terminal)
    }

    fn position(self) -> usize {
        AgentKind::ALL.iter().position(|kind| *kind == self).unwrap_or(0)
    }

    pub fn next(self) -> Self {
        AgentKind::ALL[(self.position() + 1) % AgentKind::ALL.len()]
    }

    pub fn prev(self) -> Self {
        AgentKind::ALL[(self.position() + AgentKind::ALL.len() - 1) % AgentKind::ALL.len()]
    }

    pub fn names() -> String {
        AgentKind::ALL.iter().map(|kind| kind.label()).collect::<Vec<_>>().join("|")
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "claude" | "cc" => Some(AgentKind::Claude),
            "codex" => Some(AgentKind::Codex),
            "kilo" | "kilocode" => Some(AgentKind::Kilo),
            "opencode" | "oc" => Some(AgentKind::Opencode),
            "agy" | "antigravity" | "gemini" => Some(AgentKind::Antigravity),
            "grok" => Some(AgentKind::Grok),
            "qwen" => Some(AgentKind::Qwen),
            "kimi" => Some(AgentKind::Kimi),
            "terminal" | "term" | "shell" => Some(AgentKind::Terminal),
            _ => None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum AgentState {
    Idle,
    Working,
    Paused,
    Done,
    Exited,
    Lost,
}

impl AgentState {
    pub fn label(self) -> &'static str {
        match self {
            AgentState::Idle => "idle",
            AgentState::Working => "working",
            AgentState::Paused => "paused",
            AgentState::Done => "done",
            AgentState::Exited => "exited",
            AgentState::Lost => "lost",
        }
    }

    pub fn display_label(self, lang: crate::i18n::Language) -> &'static str {
        match lang {
            crate::i18n::Language::En => self.label(),
            crate::i18n::Language::PtBr => match self {
                AgentState::Idle => "ocioso",
                AgentState::Working => "ativo",
                AgentState::Paused => "pausado",
                AgentState::Done => "pronto",
                AgentState::Exited => "parado",
                AgentState::Lost => "perdido",
            },
        }
    }

    pub fn is_alive(self) -> bool {
        !matches!(self, AgentState::Exited | AgentState::Lost)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Worktree {
    pub repo: PathBuf,
    pub path: PathBuf,
    pub branch: String,
    pub base: String,
    pub source: PathBuf,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AgentRecord {
    pub id: AgentId,
    pub session: String,
    pub name: String,
    pub kind: AgentKind,
    pub cwd: PathBuf,
    #[serde(default)]
    pub task: String,
    pub created_at_ms: u64,
    #[serde(default)]
    pub conversation_id: Option<String>,
    #[serde(default)]
    pub launches: u32,
    #[serde(default)]
    pub worktree: Option<Worktree>,
    #[serde(default)]
    pub bypass: bool,
}

impl AgentRecord {
    pub fn project_dir(&self) -> &Path {
        self.worktree.as_ref().map_or(&self.cwd, |worktree| &worktree.source)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct NewAgent {
    pub session: String,
    pub name: String,
    pub kind: AgentKind,
    pub cwd: PathBuf,
    pub task: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct LaunchSpec {
    pub kind: AgentKind,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    #[serde(default)]
    pub env: Vec<(String, String)>,
    #[serde(default)]
    pub env_remove: Vec<String>,
    #[serde(default)]
    pub initial_input: Option<String>,
    pub rows: u16,
    pub cols: u16,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RuntimeInfo {
    pub holder_pid: u32,
    #[serde(default)]
    pub child_pid: Option<u32>,
    pub port: u16,
    pub token: String,
    pub started_at_ms: u64,
    pub protocol: u32,
    pub version: String,
    #[serde(default)]
    pub exe: Option<PathBuf>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ExitInfo {
    #[serde(default)]
    pub code: Option<u32>,
    #[serde(default)]
    pub error: Option<String>,
    pub at_ms: u64,
}

impl ExitInfo {
    pub fn describe(&self) -> String {
        match (&self.error, self.code) {
            (Some(error), _) => format!("failed: {error}"),
            (None, Some(code)) => format!("exited with code {code}"),
            (None, None) => "exited".to_string(),
        }
    }

    pub fn describe_lang(&self, lang: crate::i18n::Language) -> String {
        match lang {
            crate::i18n::Language::En => self.describe(),
            crate::i18n::Language::PtBr => match (&self.error, self.code) {
                (Some(error), _) => format!("falhou: {error}"),
                (None, Some(code)) => format!("encerrou com código {code}"),
                (None, None) => "encerrou".to_string(),
            },
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct NestedAgent {
    pub kind: AgentKind,
    pub pid: u32,
    #[serde(default)]
    pub conversation_id: Option<String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    pub since_ms: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct StatusReport {
    pub state: AgentState,
    pub state_since_ms: u64,
    #[serde(default)]
    pub turn_started_ms: Option<u64>,
    #[serde(default)]
    pub last_turn_ms: Option<u64>,
    pub started_at_ms: u64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub activity: String,
    pub rows: u16,
    pub cols: u16,
    #[serde(default)]
    pub exit: Option<ExitInfo>,
    #[serde(default)]
    pub nested: Option<NestedAgent>,
    #[serde(default)]
    pub viewers: Option<u16>,
}

impl StatusReport {
    pub fn running_kind(&self, launched: AgentKind) -> AgentKind {
        self.nested.as_ref().map_or(launched, |nested| nested.kind)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub name: String,
    pub created_at_ms: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
pub struct SessionList {
    pub sessions: Vec<Session>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct UiState {
    pub session: String,
    pub who: AgentKind,
    #[serde(default)]
    pub where_dir: Option<PathBuf>,
    #[serde(default)]
    pub tabs: Vec<AgentId>,
    #[serde(default)]
    pub session_dirs: BTreeMap<String, PathBuf>,
    #[serde(default)]
    pub worktree: bool,
    #[serde(default)]
    pub bypass: bool,
    #[serde(default)]
    pub pwa: bool,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            session: DEFAULT_SESSION.to_string(),
            who: AgentKind::Claude,
            where_dir: None,
            tabs: Vec::new(),
            session_dirs: BTreeMap::new(),
            worktree: false,
            bypass: false,
            pwa: false,
        }
    }
}

pub fn validate_session_name(name: &str) -> Result<String, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("session name cannot be empty".into());
    }
    if trimmed.chars().count() > MAX_SESSION_NAME_LEN {
        return Err(format!("session name must have at most {MAX_SESSION_NAME_LEN} characters"));
    }
    if !trimmed.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
        return Err("session name may only contain letters, digits, '-', '_' and '.'".into());
    }
    Ok(trimmed.to_string())
}

pub fn validate_agent_name(name: &str) -> Result<String, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("agent name cannot be empty".into());
    }
    if trimmed.chars().count() > MAX_AGENT_NAME_LEN {
        return Err(format!("agent name must have at most {MAX_AGENT_NAME_LEN} characters"));
    }
    if trimmed.chars().any(char::is_control) {
        return Err("agent name cannot contain control characters".into());
    }
    Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_cycles_through_all_values() {
        let mut kind = AgentKind::Claude;
        let mut seen = Vec::new();
        for _ in 0..AgentKind::ALL.len() {
            seen.push(kind);
            kind = kind.next();
        }
        assert_eq!(kind, AgentKind::Claude);
        assert_eq!(seen, AgentKind::ALL);
        assert_eq!(AgentKind::Claude.prev(), AgentKind::Terminal);
        assert_eq!(AgentKind::Terminal.next(), AgentKind::Claude);
        assert_eq!(AgentKind::Codex.next(), AgentKind::Kilo);
        assert_eq!(AgentKind::Kimi.next(), AgentKind::Terminal);
        for kind in AgentKind::ALL {
            assert_eq!(kind.next().prev(), kind);
        }
    }

    #[test]
    fn kind_parses_aliases() {
        assert_eq!(AgentKind::parse("Claude"), Some(AgentKind::Claude));
        assert_eq!(AgentKind::parse("shell"), Some(AgentKind::Terminal));
        assert_eq!(AgentKind::parse("codex"), Some(AgentKind::Codex));
        assert_eq!(AgentKind::parse("vim"), None);
        for kind in AgentKind::ALL {
            assert_eq!(AgentKind::parse(kind.label()), Some(kind));
        }
        assert_eq!(AgentKind::parse("KiloCode"), Some(AgentKind::Kilo));
        assert_eq!(AgentKind::parse("oc"), Some(AgentKind::Opencode));
        assert_eq!(AgentKind::parse("gemini"), Some(AgentKind::Antigravity));
        assert_eq!(AgentKind::parse("Antigravity"), Some(AgentKind::Antigravity));
        assert_eq!(AgentKind::names(), "claude|codex|kilo|opencode|agy|grok|qwen|kimi|terminal");
    }

    #[test]
    fn kind_serializes_lowercase() {
        assert_eq!(serde_json::to_string(&AgentKind::Terminal).unwrap(), "\"terminal\"");
        assert_eq!(serde_json::to_string(&AgentKind::Opencode).unwrap(), "\"opencode\"");
        assert_eq!(serde_json::from_str::<AgentKind>("\"kimi\"").unwrap(), AgentKind::Kimi);
        assert_eq!(serde_json::to_string(&AgentKind::Antigravity).unwrap(), "\"antigravity\"");
        assert_eq!(serde_json::to_string(&AgentState::Paused).unwrap(), "\"paused\"");
    }

    #[test]
    fn session_names_are_validated() {
        assert_eq!(validate_session_name(" work-1 ").unwrap(), "work-1");
        assert!(validate_session_name("").is_err());
        assert!(validate_session_name("has space").is_err());
        assert!(validate_session_name("../escape").is_err());
        assert!(validate_session_name(&"a".repeat(MAX_SESSION_NAME_LEN + 1)).is_err());
    }

    #[test]
    fn agent_names_are_validated() {
        assert_eq!(validate_agent_name(" localhost-server ").unwrap(), "localhost-server");
        assert!(validate_agent_name("   ").is_err());
        assert!(validate_agent_name("bad\nname").is_err());
        assert!(validate_agent_name(&"x".repeat(MAX_AGENT_NAME_LEN + 1)).is_err());
    }

    #[test]
    fn record_deserializes_with_missing_optional_fields() {
        let json = r#"{"id":3,"session":"default","name":"a","kind":"claude","cwd":"/x","created_at_ms":1}"#;
        let record: AgentRecord = serde_json::from_str(json).unwrap();
        assert_eq!(record.task, "");
        assert_eq!(record.conversation_id, None);
        assert_eq!(record.launches, 0);
        assert!(!record.bypass, "agents saved before bypass existed keep asking for permissions");
    }

    #[test]
    fn status_without_nested_agent_keeps_the_launched_kind() {
        let json = r#"{"state":"idle","state_since_ms":1,"started_at_ms":1,"rows":24,"cols":80}"#;
        let mut status: StatusReport = serde_json::from_str(json).unwrap();
        assert_eq!(status.nested, None);
        assert_eq!(status.running_kind(AgentKind::Terminal), AgentKind::Terminal);
        status.nested =
            Some(NestedAgent { kind: AgentKind::Codex, pid: 42, conversation_id: None, cwd: None, since_ms: 5 });
        assert_eq!(status.running_kind(AgentKind::Terminal), AgentKind::Codex);
    }

    #[test]
    fn agents_in_a_worktree_belong_to_the_folder_they_were_started_from() {
        let mut record: AgentRecord = serde_json::from_str(
            r#"{"id":1,"session":"s","name":"n","kind":"claude","cwd":"/repo/app","created_at_ms":1}"#,
        )
        .unwrap();
        assert_eq!(record.worktree, None);
        assert_eq!(record.project_dir(), Path::new("/repo/app"));
        record.cwd = PathBuf::from("/orkai/worktrees/1/app");
        record.worktree = Some(Worktree {
            repo: PathBuf::from("/repo"),
            path: PathBuf::from("/orkai/worktrees/1"),
            branch: "orkai/n".into(),
            base: "abc".into(),
            source: PathBuf::from("/repo/app"),
        });
        assert_eq!(record.project_dir(), Path::new("/repo/app"));
        let saved = serde_json::to_string(&record).unwrap();
        assert_eq!(serde_json::from_str::<AgentRecord>(&saved).unwrap(), record);
    }

    #[test]
    fn exit_info_describes_outcome() {
        let ok = ExitInfo { code: Some(0), error: None, at_ms: 1 };
        let failed = ExitInfo { code: None, error: Some("not found".into()), at_ms: 1 };
        assert_eq!(ok.describe(), "exited with code 0");
        assert_eq!(failed.describe(), "failed: not found");
    }
}
