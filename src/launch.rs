use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::config::Config;
use crate::fsutil;
use crate::hooks;
use crate::model::{AgentId, AgentKind, AgentRecord, LaunchSpec, NewAgent, RuntimeInfo};
use crate::paths::{HOME_ENV, Home, normalize_for_compare, user_home};
use crate::protocol::TermSize;
use crate::store::Store;
use crate::util::{civil_date_from_ms, fnv1a64, is_uuid, new_uuid_v4};
use crate::worktree;

pub const HOLDER_COMMAND: &str = "__holder";
pub const HOLDER_PREFIX: &str = "orkai-holder-";
const RUNTIME_POLL: Duration = Duration::from_millis(25);
const DAY_MS: u64 = 86_400_000;
const SESSION_CLOCK_SKEW_MS: u64 = 60_000;
const OUTPUT_POLL: Duration = Duration::from_millis(20);
const MAX_OUTPUT_BYTES: u64 = 4 * 1024 * 1024;
const OUTPUT_DRAIN: Duration = Duration::from_secs(2);
const SESSION_LIST_TIMEOUT: Duration = Duration::from_secs(30);
const SESSION_LIST_LIMIT: &str = "100";
const LISTED_SESSION_PREFIX: &str = "ses_";
const OPENCODE_CONFIG_CONTENT: &str = "OPENCODE_CONFIG_CONTENT";
const KILO_CONFIG_CONTENT: &str = "KILO_CONFIG_CONTENT";
const OPENCODE_PURE: &str = "OPENCODE_PURE";
const KILO_PURE: &str = "KILO_PURE";
const PURE_ON: &str = "1";
const KILO_NO_DAEMON: &str = "KILO_NO_DAEMON";
const NO_DAEMON_ON: &str = "1";
const PROMPT_OPTION: &str = "--prompt";
const AGY_STATUS_ENV: &str = "AGY_CLI_OSC7501";
const AGY_STATUS_ON: &str = "on";
const AGY_CONVERSATIONS: [&str; 2] = ["cache", "last_conversations.json"];
const KIMI_SESSION_INDEX: &str = "session_index.jsonl";
const KIMI_STATE: &str = "state.json";
const MAX_INDEX_BYTES: u64 = 8 * 1024 * 1024;
const ENV_REMOVE: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_PID",
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    crate::update::UPDATED_ENV,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchMode {
    Fresh,
    Resume,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

pub fn resolve_program(name: &str) -> io::Result<PathBuf> {
    let path = Path::new(name);
    if path.components().count() > 1 || path.is_absolute() {
        return if path.exists() {
            Ok(path.to_path_buf())
        } else {
            Err(io::Error::new(io::ErrorKind::NotFound, format!("{name} not found")))
        };
    }
    which::which(name).map_err(|_| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("'{name}' was not found in PATH; install it or set its command in config.json"),
        )
    })
}

pub fn installed_kinds(config: &Config) -> Vec<AgentKind> {
    AgentKind::ALL
        .into_iter()
        .filter(|kind| config.agent(*kind).is_none_or(|(command, _)| resolve_program(command).is_ok()))
        .collect()
}

pub fn default_shell(config: &Config) -> (String, Vec<String>) {
    if let Some(shell) = config.shell.as_ref().filter(|s| !s.trim().is_empty()) {
        return (shell.clone(), config.shell_args.clone());
    }
    if cfg!(windows) {
        let shell = if which::which("pwsh").is_ok() { "pwsh" } else { "powershell" };
        (shell.to_string(), vec!["-NoLogo".to_string()])
    } else {
        let shell = std::env::var("SHELL").ok().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| {
            if Path::new("/bin/bash").exists() { "/bin/bash".to_string() } else { "/bin/sh".to_string() }
        });
        (shell, Vec::new())
    }
}

const CMD_UNSAFE: &[char] = &['&', '|', '<', '>', '^', '%', '"', '(', ')', '!', '\r', '\n'];
const SHIM_DIR_MARKERS: [&str; 2] = ["%dp0%\\", "%~dp0"];
const SHIM_ARGS: &str = "%*";
const MAX_SHIM_BYTES: u64 = 4 * 1024;

pub fn npm_shim_target(shim: &Path) -> Option<PathBuf> {
    if fs::metadata(shim).ok()?.len() > MAX_SHIM_BYTES {
        return None;
    }
    let text = fs::read_to_string(shim).ok()?;
    let line = text.lines().map(str::trim).rfind(|line| !line.is_empty())?;
    let quoted = line.strip_suffix(SHIM_ARGS)?.trim();
    let target = quoted.strip_prefix('"')?.strip_suffix('"')?;
    let relative = SHIM_DIR_MARKERS.iter().find_map(|marker| target.strip_prefix(marker))?;
    let is_exe = relative.to_ascii_lowercase().ends_with(".exe");
    if !is_exe || relative.contains(['%', '"']) {
        return None;
    }
    let exe = shim.parent()?.join(relative.replace('/', "\\"));
    exe.is_file().then_some(exe)
}

pub fn wrap_for_platform(program: PathBuf, args: Vec<String>) -> io::Result<(PathBuf, Vec<String>)> {
    let is_batch = cfg!(windows)
        && program
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .is_some_and(|e| e == "cmd" || e == "bat");
    if !is_batch {
        return Ok((program, args));
    }
    if let Some(exe) = npm_shim_target(&program) {
        return Ok((exe, args));
    }
    let program_text = program.to_string_lossy().into_owned();
    if program_text.contains(CMD_UNSAFE) || args.iter().any(|a| a.contains(CMD_UNSAFE)) {
        return Err(invalid(format!(
            "refusing to run {program_text} through cmd.exe with special characters in its arguments"
        )));
    }
    let comspec =
        std::env::var_os("ComSpec").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows\System32\cmd.exe"));
    let mut wrapped = vec!["/d".to_string(), "/c".to_string(), program_text];
    wrapped.extend(args);
    Ok((comspec, wrapped))
}

fn env_dir(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from)
}

fn home_dir(env: &str, default: &str) -> Option<PathBuf> {
    env_dir(env).or_else(|| user_home().map(|h| h.join(default)))
}

fn xdg_data_dir(app: &str) -> Option<PathBuf> {
    env_dir("XDG_DATA_HOME").or_else(|| user_home().map(|h| h.join(".local").join("share"))).map(|dir| dir.join(app))
}

pub fn claude_home() -> Option<PathBuf> {
    home_dir("CLAUDE_CONFIG_DIR", ".claude")
}

pub fn codex_home() -> Option<PathBuf> {
    home_dir("CODEX_HOME", ".codex")
}

fn xdg_config_dir(app: &str) -> Option<PathBuf> {
    env_dir("XDG_CONFIG_HOME").or_else(|| user_home().map(|h| h.join(".config"))).map(|dir| dir.join(app))
}

pub fn kilo_config_dir() -> Option<PathBuf> {
    xdg_config_dir("kilo")
}

pub fn opencode_config_dir() -> Option<PathBuf> {
    xdg_config_dir("opencode")
}

pub fn kilo_data_dir() -> Option<PathBuf> {
    xdg_data_dir("kilo")
}

pub fn opencode_data_dir() -> Option<PathBuf> {
    xdg_data_dir("opencode")
}

pub fn antigravity_home() -> Option<PathBuf> {
    user_home().map(|h| h.join(".gemini").join("antigravity-cli"))
}

pub fn grok_home() -> Option<PathBuf> {
    home_dir("GROK_HOME", ".grok")
}

pub fn qwen_home() -> Option<PathBuf> {
    home_dir("QWEN_HOME", ".qwen")
}

pub fn qwen_runtime_dir() -> Option<PathBuf> {
    env_dir("QWEN_RUNTIME_DIR").or_else(qwen_home)
}

pub fn kimi_home() -> Option<PathBuf> {
    home_dir("KIMI_CODE_HOME", ".kimi-code")
}

#[derive(Clone, Debug, Default)]
pub struct Homes {
    pub claude: Option<PathBuf>,
    pub qwen: Option<PathBuf>,
    pub grok: Option<PathBuf>,
}

impl Homes {
    pub fn detect() -> Self {
        Self { claude: claude_home(), qwen: qwen_runtime_dir(), grok: grok_home() }
    }
}

fn found_under_any(parent: &Path, child: &Path) -> bool {
    fs::read_dir(parent).map(|entries| entries.flatten().any(|e| e.path().join(child).exists())).unwrap_or(false)
}

pub fn claude_transcript_exists(claude_home: &Path, session_id: &str) -> bool {
    found_under_any(&claude_home.join("projects"), Path::new(&format!("{session_id}.jsonl")))
}

pub fn qwen_transcript_exists(qwen_runtime: &Path, session_id: &str) -> bool {
    found_under_any(&qwen_runtime.join("projects"), &Path::new("chats").join(format!("{session_id}.jsonl")))
}

pub fn grok_session_exists(grok_home: &Path, session_id: &str) -> bool {
    found_under_any(&grok_home.join("sessions"), Path::new(session_id))
}

#[cfg(windows)]
pub fn without_console(command: &mut Command) -> &mut Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW)
}

#[cfg(unix)]
pub fn without_console(command: &mut Command) -> &mut Command {
    command
}

pub fn command_output(command: &mut Command, timeout: Duration) -> Option<String> {
    let command = without_console(command).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = command.spawn().ok()?;
    let stdout = child.stdout.take()?;
    let (sender, received) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let mut text = String::new();
        let read = io::Read::read_to_string(&mut io::Read::take(stdout, MAX_OUTPUT_BYTES), &mut text);
        let _ = sender.send(read.ok().map(|_| text));
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => thread::sleep(OUTPUT_POLL),
            _ => {
                kill_process_tree(child.id());
                let _ = child.wait();
                return None;
            }
        }
    }
    received.recv_timeout(deadline.saturating_duration_since(Instant::now()).max(OUTPUT_DRAIN)).ok()?
}

fn sorted_dir_names(path: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(path)
        .map(|entries| entries.flatten().filter_map(|e| e.file_name().to_str().map(str::to_string)).collect())
        .unwrap_or_default();
    names.sort();
    names
}

fn codex_meta(line: &str) -> Option<(String, String)> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let source = value.get("payload").filter(|p| p.get("id").is_some()).unwrap_or(&value);
    if source.get("source").and_then(|s| s.get("subagent")).is_some() {
        return None;
    }
    let id = source.get("id")?.as_str()?.to_string();
    let cwd = source.get("cwd")?.as_str()?.to_string();
    Some((id, cwd))
}

pub fn is_safe_session_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub fn recent_codex_rollouts(codex_home: &Path, since_ms: u64) -> Vec<PathBuf> {
    let root = codex_home.join("sessions");
    let (y, m, d) = civil_date_from_ms(since_ms.saturating_sub(DAY_MS));
    let floor = format!("{y:04}/{m:02}/{d:02}");
    let threshold = since_ms.saturating_sub(SESSION_CLOCK_SKEW_MS);
    let mut files: Vec<PathBuf> = Vec::new();
    for year in sorted_dir_names(&root) {
        for month in sorted_dir_names(&root.join(&year)) {
            for day in sorted_dir_names(&root.join(&year).join(&month)) {
                if format!("{year}/{month}/{day}") < floor {
                    continue;
                }
                let dir = root.join(&year).join(&month).join(&day);
                for name in sorted_dir_names(&dir) {
                    if !(name.starts_with("rollout-") && name.ends_with(".jsonl")) {
                        continue;
                    }
                    let path = dir.join(&name);
                    let modified = fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
                        .unwrap_or(0);
                    if modified >= threshold {
                        files.push(path);
                    }
                }
            }
        }
    }
    files
}

pub fn find_codex_session(codex_home: &Path, cwd: &Path, since_ms: u64, exclude: &[String]) -> Option<String> {
    let target = normalize_for_compare(cwd);
    for path in recent_codex_rollouts(codex_home, since_ms) {
        let Ok(text) = fs::read_to_string(&path) else { continue };
        let Some((id, session_cwd)) = text.lines().next().and_then(codex_meta) else { continue };
        if normalize_for_compare(Path::new(&session_cwd)) == target && !exclude.contains(&id) && is_safe_session_id(&id)
        {
            return Some(id);
        }
    }
    None
}

#[derive(Deserialize)]
struct ListedSession {
    id: String,
    #[serde(default)]
    created: u64,
    #[serde(default)]
    directory: String,
}

pub fn pick_listed_session(listing: &str, cwd: &Path, since_ms: u64, exclude: &[String]) -> Option<String> {
    let sessions: Vec<ListedSession> = serde_json::from_str(listing.trim()).ok()?;
    let target = normalize_for_compare(cwd);
    let threshold = since_ms.saturating_sub(SESSION_CLOCK_SKEW_MS);
    sessions
        .into_iter()
        .filter(|s| s.id.starts_with(LISTED_SESSION_PREFIX) && is_safe_session_id(&s.id) && !exclude.contains(&s.id))
        .filter(|s| s.created >= threshold && normalize_for_compare(Path::new(&s.directory)) == target)
        .min_by_key(|s| (s.created < since_ms, s.created))
        .map(|s| s.id)
}

fn listed_sessions(kind: AgentKind, program: &str, cwd: &Path) -> Option<String> {
    let args = ["session", "list", "--format", "json", "-n", SESSION_LIST_LIMIT].map(str::to_string).to_vec();
    let (program, args) = wrap_for_platform(resolve_program(program).ok()?, args).ok()?;
    let pure = if kind == AgentKind::Kilo { KILO_PURE } else { OPENCODE_PURE };
    command_output(Command::new(program).args(args).current_dir(cwd).env(pure, PURE_ON), SESSION_LIST_TIMEOUT)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct KimiIndexEntry {
    session_id: String,
    #[serde(default)]
    session_dir: Option<PathBuf>,
    #[serde(default)]
    work_dir: PathBuf,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct KimiState {
    created_at: u64,
}

pub fn find_kimi_session(kimi_home: &Path, cwd: &Path, since_ms: u64, exclude: &[String]) -> Option<String> {
    let index = kimi_home.join(KIMI_SESSION_INDEX);
    if fs::metadata(&index).ok()?.len() > MAX_INDEX_BYTES {
        return None;
    }
    let target = normalize_for_compare(cwd);
    let threshold = since_ms.saturating_sub(SESSION_CLOCK_SKEW_MS);
    fs::read_to_string(index)
        .ok()?
        .lines()
        .filter_map(|line| serde_json::from_str::<KimiIndexEntry>(line).ok())
        .filter(|e| is_safe_session_id(&e.session_id) && !exclude.contains(&e.session_id))
        .filter(|e| normalize_for_compare(&e.work_dir) == target)
        .filter_map(|e| {
            let dir = e.session_dir.map(|dir| kimi_home.join(dir))?;
            let state: KimiState = fsutil::read_json_opt(&dir.join(KIMI_STATE)).ok().flatten()?;
            (state.created_at >= threshold).then_some(((state.created_at < since_ms, state.created_at), e.session_id))
        })
        .min()
        .map(|(_, id)| id)
}

pub fn find_antigravity_conversation(agy_home: &Path, cwd: &Path, exclude: &[String]) -> Option<String> {
    let file = AGY_CONVERSATIONS.iter().fold(agy_home.to_path_buf(), |dir, part| dir.join(part));
    let latest: BTreeMap<String, String> = fsutil::read_json_opt(&file).ok().flatten()?;
    let target = normalize_for_compare(cwd);
    latest
        .into_iter()
        .find(|(dir, _)| normalize_for_compare(Path::new(dir)) == target)
        .map(|(_, id)| id)
        .filter(|id| is_uuid(id) && !exclude.contains(id))
}

pub struct HookSetup {
    pub exe: PathBuf,
    pub claude_settings: PathBuf,
    pub qwen_settings: PathBuf,
    pub plugin: PathBuf,
    pub codex_home: Option<PathBuf>,
}

#[derive(Default)]
struct HookLaunch {
    args: Vec<String>,
    env: Vec<(String, String)>,
}

impl HookLaunch {
    fn args(args: Vec<String>) -> Self {
        Self { args, env: Vec::new() }
    }

    fn env(name: &str, value: String) -> Self {
        Self { args: Vec::new(), env: vec![(name.to_string(), value)] }
    }
}

fn names_option(args: &[String], option: &str) -> bool {
    args.iter().any(|arg| arg == option || arg.starts_with(&format!("{option}=")))
}

fn env_is_set(name: &str) -> bool {
    std::env::var_os(name).is_some()
}

fn hook_launch(config: &Config, record: &AgentRecord, program: &Path, setup: &HookSetup) -> HookLaunch {
    match record.kind {
        AgentKind::Claude => {
            if names_option(&config.claude_args, "--settings")
                || !hooks::git_bash_available()
                || !hooks::claude_supports_hooks(program)
            {
                return HookLaunch::default();
            }
            let settings = hooks::claude_settings(&hooks::claude_hook_command(&setup.exe, record.id));
            match fsutil::write_json_atomic(&setup.claude_settings, &settings) {
                Ok(()) => HookLaunch::args(vec![
                    "--settings".to_string(),
                    setup.claude_settings.to_string_lossy().into_owned(),
                ]),
                Err(_) => HookLaunch::default(),
            }
        }
        AgentKind::Codex => {
            let configured = config.codex_args.iter().any(|arg| arg.trim_start().starts_with("notify"))
                || setup.codex_home.as_deref().is_some_and(hooks::codex_has_own_notify);
            if configured {
                return HookLaunch::default();
            }
            HookLaunch::args(vec!["-c".to_string(), hooks::codex_notify_override(&setup.exe, record.id)])
        }
        AgentKind::Kilo | AgentKind::Opencode => {
            let variable = if record.kind == AgentKind::Kilo { KILO_CONFIG_CONTENT } else { OPENCODE_CONFIG_CONTENT };
            if env_is_set(variable) {
                return HookLaunch::default();
            }
            match fsutil::write_atomic(&setup.plugin, hooks::opencode_plugin(&setup.exe, record.id).as_bytes()) {
                Ok(()) => HookLaunch::env(variable, hooks::plugin_config(&setup.plugin)),
                Err(_) => HookLaunch::default(),
            }
        }
        AgentKind::Antigravity if !env_is_set(AGY_STATUS_ENV) => HookLaunch::env(AGY_STATUS_ENV, AGY_STATUS_ON.into()),
        AgentKind::Qwen => {
            if env_is_set(hooks::QWEN_SYSTEM_DEFAULTS_ENV) || hooks::qwen_system_defaults().exists() {
                return HookLaunch::default();
            }
            let Some(command) = hooks::qwen_hook_command(&setup.exe, record.id) else {
                return HookLaunch::default();
            };
            match fsutil::write_json_atomic(&setup.qwen_settings, &hooks::claude_settings(&command)) {
                Ok(()) => {
                    HookLaunch::env(hooks::QWEN_SYSTEM_DEFAULTS_ENV, setup.qwen_settings.to_string_lossy().into_owned())
                }
                Err(_) => HookLaunch::default(),
            }
        }
        AgentKind::Antigravity | AgentKind::Grok | AgentKind::Kimi | AgentKind::Terminal => HookLaunch::default(),
    }
}

fn chosen_session_id(record: &mut AgentRecord) -> io::Result<String> {
    match record.conversation_id.as_deref() {
        Some(id) if is_uuid(id) => Ok(id.to_string()),
        _ => {
            let id = new_uuid_v4()?;
            record.conversation_id = Some(id.clone());
            Ok(id)
        }
    }
}

fn chosen_session_exists(kind: AgentKind, homes: &Homes, session_id: &str) -> bool {
    match kind {
        AgentKind::Claude => homes.claude.as_deref().is_some_and(|dir| claude_transcript_exists(dir, session_id)),
        AgentKind::Qwen => homes.qwen.as_deref().is_some_and(|dir| qwen_transcript_exists(dir, session_id)),
        AgentKind::Grok => homes.grok.as_deref().is_some_and(|dir| grok_session_exists(dir, session_id)),
        _ => false,
    }
}

fn chosen_session_args(kind: AgentKind, session_id: &str, resume: bool) -> Vec<String> {
    match (kind, resume) {
        (AgentKind::Grok, true) => vec![format!("--resume={session_id}")],
        (AgentKind::Qwen, true) => vec!["--resume".to_string(), session_id.to_string()],
        _ => vec!["--session-id".to_string(), session_id.to_string()],
    }
}

fn resume_args(kind: AgentKind, session_id: &str) -> Vec<String> {
    match kind {
        AgentKind::Antigravity => vec![format!("--conversation={session_id}")],
        _ => vec!["--session".to_string(), session_id.to_string()],
    }
}

fn bypass_flag(kind: AgentKind) -> Option<&'static str> {
    match kind {
        AgentKind::Claude | AgentKind::Antigravity => Some("--dangerously-skip-permissions"),
        AgentKind::Codex => Some("--dangerously-bypass-approvals-and-sandbox"),
        AgentKind::Kilo | AgentKind::Opencode | AgentKind::Kimi => Some("--auto"),
        AgentKind::Grok => Some("--always-approve"),
        AgentKind::Qwen => Some("--yolo"),
        AgentKind::Terminal => None,
    }
}

fn permission_options(kind: AgentKind) -> &'static [&'static str] {
    match kind {
        AgentKind::Claude | AgentKind::Antigravity => &["--dangerously-skip-permissions"],
        AgentKind::Codex => {
            &["--dangerously-bypass-approvals-and-sandbox", "--yolo", "--ask-for-approval", "-a", "--approve-for-me"]
        }
        AgentKind::Kilo | AgentKind::Opencode => &["--auto", "--yolo", "--dangerously-skip-permissions"],
        AgentKind::Grok => &["--always-approve", "--yolo", "--dangerously-skip-permissions"],
        AgentKind::Qwen => &["--yolo", "-y", "--approval-mode"],
        AgentKind::Kimi => &["--auto", "--yolo", "-y", "--yes", "--auto-approve"],
        AgentKind::Terminal => &[],
    }
}

fn agent_command(config: &Config, kind: AgentKind) -> (String, Vec<String>) {
    config.agent(kind).map(|(command, args)| (command.to_string(), args.to_vec())).unwrap_or_default()
}

fn non_empty(text: &str) -> Option<String> {
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

pub fn build_spec(
    config: &Config,
    record: &mut AgentRecord,
    mode: LaunchMode,
    size: TermSize,
    homes: &Homes,
    hooks: Option<&HookSetup>,
) -> io::Result<LaunchSpec> {
    let (program_name, mut args, initial_input) = match record.kind {
        AgentKind::Claude => {
            let session_id = chosen_session_id(record)?;
            let resume = mode == LaunchMode::Resume && chosen_session_exists(record.kind, homes, &session_id);
            let mut args = config.claude_args.clone();
            args.push("--remote-control".to_string());
            if resume {
                args.extend(["--resume".to_string(), session_id]);
            } else {
                args.extend(["--session-id".to_string(), session_id]);
            }
            let input = if resume { None } else { non_empty(&record.task) };
            (config.claude_command.clone(), args, input)
        }
        AgentKind::Codex => {
            let resume_id =
                record.conversation_id.clone().filter(|id| mode == LaunchMode::Resume && is_safe_session_id(id));
            match resume_id {
                Some(id) => {
                    let mut args = vec!["resume".to_string(), id];
                    args.extend(config.codex_args.clone());
                    (config.codex_command.clone(), args, None)
                }
                None => (config.codex_command.clone(), config.codex_args.clone(), non_empty(&record.task)),
            }
        }
        AgentKind::Grok | AgentKind::Qwen => {
            let session_id = chosen_session_id(record)?;
            let resume = mode == LaunchMode::Resume && chosen_session_exists(record.kind, homes, &session_id);
            let (command, mut args) = agent_command(config, record.kind);
            args.extend(chosen_session_args(record.kind, &session_id, resume));
            let input = if resume { None } else { non_empty(&record.task) };
            (command, args, input)
        }
        AgentKind::Kilo | AgentKind::Opencode | AgentKind::Antigravity | AgentKind::Kimi => {
            let resume_id =
                record.conversation_id.clone().filter(|id| mode == LaunchMode::Resume && is_safe_session_id(id));
            let (command, mut args) = agent_command(config, record.kind);
            let input = match resume_id {
                Some(id) => {
                    args.extend(resume_args(record.kind, &id));
                    None
                }
                None => non_empty(&record.task),
            };
            (command, args, input)
        }
        AgentKind::Terminal => {
            let (shell, args) = default_shell(config);
            (shell, args, non_empty(&record.task))
        }
    };
    if record.bypass
        && let Some(flag) = bypass_flag(record.kind)
        && !permission_options(record.kind).iter().any(|option| names_option(&args, option))
    {
        args.push(flag.to_string());
    }
    if !record.cwd.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("directory {} does not exist", record.cwd.display()),
        ));
    }
    let resolved = resolve_program(&program_name)?;
    let HookLaunch { args: extra, env: hook_env } =
        hooks.map(|setup| hook_launch(config, record, &resolved, setup)).unwrap_or_default();
    let with_hooks: Vec<String> = match record.kind {
        AgentKind::Codex => extra.iter().chain(&args).cloned().collect(),
        _ => args.iter().chain(&extra).cloned().collect(),
    };
    let prompt = match (record.kind, initial_input.as_deref()) {
        (AgentKind::Kilo | AgentKind::Opencode, Some(task)) => vec![PROMPT_OPTION.to_string(), task.to_string()],
        _ => Vec::new(),
    };
    let with_prompt = with_hooks.iter().chain(&prompt).cloned().collect();
    let ((program, args), initial_input) = match wrap_for_platform(resolved.clone(), with_prompt) {
        Ok(wrapped) if !prompt.is_empty() => (wrapped, None),
        Ok(wrapped) => (wrapped, initial_input),
        Err(_) if !prompt.is_empty() => (wrap_for_platform(resolved, with_hooks)?, initial_input),
        Err(_) if !extra.is_empty() => (wrap_for_platform(resolved, args)?, initial_input),
        Err(err) => return Err(err),
    };
    let mut env = vec![
        ("ORKAI_AGENT_ID".to_string(), record.id.to_string()),
        ("ORKAI_SESSION".to_string(), record.session.clone()),
        ("COLORTERM".to_string(), "truecolor".to_string()),
    ];
    if cfg!(unix) {
        env.push(("TERM".to_string(), "xterm-256color".to_string()));
    }
    env.extend(hook_env);
    if record.bypass && record.kind == AgentKind::Kilo && !env_is_set(KILO_NO_DAEMON) {
        env.push((KILO_NO_DAEMON.to_string(), NO_DAEMON_ON.to_string()));
    }
    Ok(LaunchSpec {
        kind: record.kind,
        program,
        args,
        cwd: record.cwd.clone(),
        env,
        env_remove: ENV_REMOVE.iter().map(|s| s.to_string()).collect(),
        initial_input,
        rows: size.rows,
        cols: size.cols,
    })
}

static STAGED: OnceLock<Mutex<Option<(PathBuf, PathBuf)>>> = OnceLock::new();

pub fn holder_executable(home: &Home, source: &Path) -> io::Result<PathBuf> {
    let cache = STAGED.get_or_init(|| Mutex::new(None));
    let mut guard = crate::util::lock(cache);
    if let Some((cached_source, target)) = guard.as_ref()
        && cached_source == source
        && target.exists()
    {
        return Ok(target.clone());
    }
    let data = fs::read(source)?;
    let name =
        format!("{HOLDER_PREFIX}{}-{:016x}{}", env!("CARGO_PKG_VERSION"), fnv1a64(&data), std::env::consts::EXE_SUFFIX);
    let target = home.bin_dir().join(name);
    if !target.exists() {
        let tmp = target.with_file_name(format!(".staging-{}-{}", std::process::id(), crate::util::now_ms()));
        fs::write(&tmp, &data)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o700))?;
        }
        if let Err(err) = fs::rename(&tmp, &target) {
            let _ = fs::remove_file(&tmp);
            if !target.exists() {
                return Err(err);
            }
        }
    }
    *guard = Some((source.to_path_buf(), target.clone()));
    Ok(target)
}

fn holder_binaries_in_use(home: &Home) -> HashSet<String> {
    let bridge = crate::pwa::bridge::read_info(home).and_then(|info| info.exe);
    let Ok(entries) = fs::read_dir(home.agents_dir()) else { return HashSet::new() };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str().and_then(|name| name.parse::<AgentId>().ok()))
        .filter_map(|id| fsutil::read_json_opt::<RuntimeInfo>(&home.runtime_file(id)).ok().flatten())
        .filter_map(|runtime| runtime.exe)
        .chain(bridge)
        .map(|exe| normalize_for_compare(&exe))
        .collect()
}

pub fn prune_holder_binaries(home: &Home, keep: &Path) {
    let Ok(entries) = fs::read_dir(home.bin_dir()) else { return };
    let in_use = holder_binaries_in_use(home);
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if (name.starts_with(HOLDER_PREFIX) || name.starts_with(".staging-"))
            && path != keep
            && !in_use.contains(&normalize_for_compare(&path))
        {
            let _ = fs::remove_file(path);
        }
    }
}

pub fn spawn_holder(home: &Home, id: AgentId, exe: &Path) -> io::Result<()> {
    let mut cmd = Command::new(exe);
    cmd.arg(HOLDER_COMMAND)
        .arg(id.to_string())
        .env(HOME_ENV, home.root())
        .current_dir(home.root())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = spawn_detached(&mut cmd)?;
    std::thread::Builder::new().name("orkai-holder-reaper".into()).spawn(move || {
        let _ = child.wait();
    })?;
    Ok(())
}

#[cfg(windows)]
fn disable_std_handle_inheritance() {
    use std::ffi::c_void;
    const STD_HANDLES: [u32; 3] = [-10i32 as u32, -11i32 as u32, -12i32 as u32];
    const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetStdHandle(std_handle: u32) -> *mut c_void;
        fn SetHandleInformation(handle: *mut c_void, mask: u32, flags: u32) -> i32;
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        for id in STD_HANDLES {
            unsafe {
                let handle = GetStdHandle(id);
                if !handle.is_null() && handle as isize != -1 {
                    SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
                }
            }
        }
    });
}

#[cfg(windows)]
pub fn spawn_detached(cmd: &mut Command) -> io::Result<std::process::Child> {
    use std::os::windows::process::CommandExt;
    disable_std_handle_inheritance();
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB);
    match cmd.spawn() {
        Ok(child) => Ok(child),
        Err(_) => {
            cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
            cmd.spawn()
        }
    }
}

#[cfg(unix)]
pub fn spawn_detached(cmd: &mut Command) -> io::Result<std::process::Child> {
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn()
}

pub fn wait_for_runtime(store: &Store, id: AgentId, timeout: Duration) -> io::Result<RuntimeInfo> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(runtime) = store.read_runtime(id).ok().flatten() {
            return Ok(runtime);
        }
        if let Some(exit) = store.read_exit(id).ok().flatten() {
            return Err(io::Error::other(exit.describe()));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "agent did not start in time"));
        }
        std::thread::sleep(RUNTIME_POLL);
    }
}

pub fn process_is_holder(pid: u32) -> bool {
    let output = if cfg!(windows) {
        Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
    } else {
        Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "comm="])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
    };
    output
        .map(|o| {
            let text = String::from_utf8_lossy(&o.stdout).to_ascii_lowercase();
            text.contains(HOLDER_PREFIX)
        })
        .unwrap_or(false)
}

pub fn kill_process_tree(pid: u32) {
    if cfg!(windows) {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    } else {
        #[cfg(unix)]
        if let Ok(pid) = libc::pid_t::try_from(pid) {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
}

#[derive(Clone)]
pub struct Launcher {
    store: Store,
    config: Config,
    source: PathBuf,
    exe: PathBuf,
    language: crate::i18n::Language,
}

impl Launcher {
    pub fn new(store: Store, config: Config, source_exe: &Path) -> io::Result<Self> {
        let exe = holder_executable(store.home(), source_exe)?;
        let language =
            config.language.as_deref().and_then(crate::i18n::Language::parse).unwrap_or(crate::i18n::Language::En);
        Ok(Self { store, config, source: source_exe.to_path_buf(), exe, language })
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn holder_exe(&self) -> &Path {
        &self.exe
    }

    pub fn language(&self) -> crate::i18n::Language {
        self.language
    }

    pub fn set_language(&mut self, language: crate::i18n::Language) {
        self.language = language;
    }

    pub fn create(&self, draft: NewAgent, size: TermSize, isolate: bool, bypass: bool) -> io::Result<AgentRecord> {
        let mut record = self.store.create_agent(draft)?;
        record.bypass = bypass;
        let started =
            self.isolate(&mut record, isolate).and_then(|()| self.start(&mut record, LaunchMode::Fresh, size));
        if let Err(err) = started {
            if let Some(worktree) = record.worktree.as_ref() {
                let _ = worktree::discard(self.store.home(), worktree);
            }
            let _ = self.store.remove_agent(record.id);
            return Err(err);
        }
        Ok(record)
    }

    fn isolate(&self, record: &mut AgentRecord, isolate: bool) -> io::Result<()> {
        if !isolate {
            return Ok(());
        }
        let (worktree, cwd) = worktree::create(self.store.home(), record.id, &record.name, &record.cwd)?;
        record.cwd = cwd;
        record.worktree = Some(worktree);
        self.store.save_agent(record)
    }

    pub fn start(&self, record: &mut AgentRecord, mode: LaunchMode, size: TermSize) -> io::Result<()> {
        if mode == LaunchMode::Resume {
            self.stop_stale_holder(record.id);
            if record.conversation_id.is_none() {
                record.conversation_id = self.discover_session(record)?;
            }
        }
        let exe = holder_executable(self.store.home(), &self.source).unwrap_or_else(|_| self.exe.clone());
        let home = self.store.home();
        let setup = HookSetup {
            exe: exe.clone(),
            claude_settings: home.claude_settings_file(record.id),
            qwen_settings: home.qwen_settings_file(record.id),
            plugin: home.plugin_file(record.id),
            codex_home: codex_home(),
        };
        let spec = build_spec(&self.config, record, mode, size, &Homes::detect(), Some(&setup))?;
        record.launches = record.launches.saturating_add(1);
        self.store.save_agent(record)?;
        self.store.clear_runtime_markers(record.id)?;
        self.store.write_launch(record.id, &spec)?;
        spawn_holder(self.store.home(), record.id, &exe)
    }

    fn discover_session(&self, record: &AgentRecord) -> io::Result<Option<String>> {
        let discovers = matches!(
            record.kind,
            AgentKind::Codex | AgentKind::Kilo | AgentKind::Opencode | AgentKind::Antigravity | AgentKind::Kimi
        );
        if !discovers {
            return Ok(None);
        }
        let claimed: Vec<String> = self
            .store
            .list_agents()?
            .into_iter()
            .filter(|r| r.id != record.id && r.kind == record.kind)
            .filter_map(|r| r.conversation_id)
            .collect();
        let (cwd, since) = (&record.cwd, record.created_at_ms);
        Ok(match record.kind {
            AgentKind::Codex => codex_home().and_then(|home| find_codex_session(&home, cwd, since, &claimed)),
            AgentKind::Kimi => kimi_home().and_then(|home| find_kimi_session(&home, cwd, since, &claimed)),
            AgentKind::Antigravity => {
                antigravity_home().and_then(|home| find_antigravity_conversation(&home, cwd, &claimed))
            }
            AgentKind::Kilo | AgentKind::Opencode => {
                let (program, _) = agent_command(&self.config, record.kind);
                listed_sessions(record.kind, &program, cwd)
                    .and_then(|listing| pick_listed_session(&listing, cwd, since, &claimed))
            }
            _ => None,
        })
    }

    fn stop_stale_holder(&self, id: AgentId) {
        if let Ok(Some(runtime)) = self.store.read_runtime(id)
            && runtime.holder_pid != std::process::id()
            && process_is_holder(runtime.holder_pid)
        {
            kill_process_tree(runtime.holder_pid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::now_ms;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("orkai-launch-{tag}-{}-{}", std::process::id(), now_ms()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn record(kind: AgentKind, cwd: PathBuf, task: &str) -> AgentRecord {
        AgentRecord {
            id: 5,
            session: "default".into(),
            name: "agent".into(),
            kind,
            cwd,
            task: task.into(),
            created_at_ms: now_ms(),
            conversation_id: None,
            launches: 0,
            worktree: None,
            bypass: false,
        }
    }

    fn config_with(program: &Path) -> Config {
        Config {
            shell: Some(program.to_string_lossy().into_owned()),
            claude_command: program.to_string_lossy().into_owned(),
            codex_command: program.to_string_lossy().into_owned(),
            ..Config::default()
        }
    }

    fn fake_program(dir: &Path) -> PathBuf {
        let path = dir.join(if cfg!(windows) { "fake.exe" } else { "fake" });
        fs::write(&path, b"").unwrap();
        path
    }

    #[test]
    fn fresh_claude_gets_new_session_id_and_task_as_input() {
        let dir = temp_dir("claude-fresh");
        let program = fake_program(&dir);
        let mut r = record(AgentKind::Claude, dir.clone(), "  fix the build ");
        let spec = build_spec(
            &config_with(&program),
            &mut r,
            LaunchMode::Fresh,
            TermSize::new(30, 100),
            &Homes::default(),
            None,
        )
        .unwrap();
        let id = r.conversation_id.clone().unwrap();
        assert!(is_uuid(&id));
        assert_eq!(spec.args, vec!["--remote-control".to_string(), "--session-id".to_string(), id]);
        assert_eq!(spec.initial_input.as_deref(), Some("fix the build"));
        assert_eq!((spec.rows, spec.cols), (30, 100));
        assert!(spec.env.iter().any(|(k, v)| k == "ORKAI_AGENT_ID" && v == "5"));
        assert!(spec.env_remove.contains(&"CLAUDECODE".to_string()));
        assert!(spec.env_remove.contains(&"CLAUDE_CODE_CHILD_SESSION".to_string()));
        assert!(spec.env_remove.contains(&"CLAUDE_CODE_SESSION_ID".to_string()));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn claude_resume_uses_existing_transcript() {
        let dir = temp_dir("claude-resume");
        let program = fake_program(&dir);
        let claude_dir = dir.join("claude-home");
        let id = "123e4567-e89b-42d3-a456-426614174000";
        fs::create_dir_all(claude_dir.join("projects").join("C--work")).unwrap();
        fs::write(claude_dir.join("projects").join("C--work").join(format!("{id}.jsonl")), b"{}").unwrap();
        let mut r = record(AgentKind::Claude, dir.clone(), "task");
        r.conversation_id = Some(id.into());
        let spec = build_spec(
            &config_with(&program),
            &mut r,
            LaunchMode::Resume,
            TermSize::new(24, 80),
            &Homes { claude: Some(claude_dir.clone()), ..Homes::default() },
            None,
        )
        .unwrap();
        assert_eq!(spec.args, vec!["--remote-control".to_string(), "--resume".to_string(), id.to_string()]);
        assert_eq!(spec.initial_input, None);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn claude_resume_without_transcript_restarts_with_same_id() {
        let dir = temp_dir("claude-no-transcript");
        let program = fake_program(&dir);
        let id = "123e4567-e89b-42d3-a456-426614174000";
        let mut r = record(AgentKind::Claude, dir.clone(), "task");
        r.conversation_id = Some(id.into());
        let spec = build_spec(
            &config_with(&program),
            &mut r,
            LaunchMode::Resume,
            TermSize::new(24, 80),
            &Homes { claude: Some(dir.clone()), ..Homes::default() },
            None,
        )
        .unwrap();
        assert_eq!(spec.args, vec!["--remote-control".to_string(), "--session-id".to_string(), id.to_string()]);
        assert_eq!(spec.initial_input.as_deref(), Some("task"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn claude_keeps_configured_args_and_always_enables_remote_control() {
        let dir = temp_dir("claude-remote-control");
        let program = fake_program(&dir);
        let config = Config { claude_args: vec!["--model".into(), "opus".into()], ..config_with(&program) };
        let id = "123e4567-e89b-42d3-a456-426614174000";
        let mut r = record(AgentKind::Claude, dir.clone(), "task");
        r.conversation_id = Some(id.into());
        let spec =
            build_spec(&config, &mut r, LaunchMode::Fresh, TermSize::new(24, 80), &Homes::default(), None).unwrap();
        assert_eq!(spec.args, vec!["--model", "opus", "--remote-control", "--session-id", id]);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn codex_resume_uses_known_conversation() {
        let dir = temp_dir("codex");
        let program = fake_program(&dir);
        let mut r = record(AgentKind::Codex, dir.clone(), "task");
        r.conversation_id = Some("abc-123".into());
        let spec = build_spec(
            &config_with(&program),
            &mut r,
            LaunchMode::Resume,
            TermSize::new(24, 80),
            &Homes::default(),
            None,
        )
        .unwrap();
        assert_eq!(spec.args, vec!["resume".to_string(), "abc-123".to_string()]);
        assert_eq!(spec.initial_input, None);
        let fresh = build_spec(
            &config_with(&program),
            &mut r,
            LaunchMode::Fresh,
            TermSize::new(24, 80),
            &Homes::default(),
            None,
        )
        .unwrap();
        assert!(fresh.args.is_empty());
        assert_eq!(fresh.initial_input.as_deref(), Some("task"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn terminal_reruns_its_command() {
        let dir = temp_dir("terminal");
        let program = fake_program(&dir);
        let mut r = record(AgentKind::Terminal, dir.clone(), "npm run dev");
        let spec = build_spec(
            &config_with(&program),
            &mut r,
            LaunchMode::Resume,
            TermSize::new(24, 80),
            &Homes::default(),
            None,
        )
        .unwrap();
        assert_eq!(spec.program, program);
        assert_eq!(spec.initial_input.as_deref(), Some("npm run dev"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_directory_is_rejected() {
        let dir = temp_dir("missing");
        let program = fake_program(&dir);
        let mut r = record(AgentKind::Terminal, dir.join("does-not-exist"), "");
        let err = build_spec(
            &config_with(&program),
            &mut r,
            LaunchMode::Fresh,
            TermSize::new(24, 80),
            &Homes::default(),
            None,
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_program_reports_not_found() {
        let err = resolve_program("orkai-definitely-missing-program").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[cfg(windows)]
    #[test]
    fn batch_files_are_wrapped_and_validated() {
        let (program, args) =
            wrap_for_platform(PathBuf::from(r"C:\npm\claude.cmd"), vec!["--session-id".into(), "x".into()]).unwrap();
        assert!(program.to_string_lossy().to_ascii_lowercase().ends_with("cmd.exe"));
        assert_eq!(args, vec!["/d", "/c", r"C:\npm\claude.cmd", "--session-id", "x"]);
        assert!(wrap_for_platform(PathBuf::from(r"C:\npm\claude.cmd"), vec!["a&calc".into()]).is_err());
        let (exe, exe_args) = wrap_for_platform(PathBuf::from(r"C:\bin\claude.exe"), vec!["a&b".into()]).unwrap();
        assert_eq!(exe, PathBuf::from(r"C:\bin\claude.exe"));
        assert_eq!(exe_args, vec!["a&b"]);
    }

    #[test]
    fn finds_codex_session_by_cwd_and_time() {
        let dir = temp_dir("codex-home");
        let since = now_ms();
        let (y, m, d) = civil_date_from_ms(since);
        let day = dir.join("sessions").join(format!("{y:04}")).join(format!("{m:02}")).join(format!("{d:02}"));
        fs::create_dir_all(&day).unwrap();
        let project = dir.join("project");
        let other = dir.join("other");
        let meta = |id: &str, cwd: &Path| {
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"cwd\":{}}}}}\n",
                serde_json::to_string(&cwd.to_string_lossy()).unwrap()
            )
        };
        fs::write(day.join("rollout-a-1.jsonl"), meta("other-session", &other)).unwrap();
        fs::write(day.join("rollout-b-2.jsonl"), meta("claimed-session", &project)).unwrap();
        fs::write(day.join("rollout-c-3.jsonl"), meta("wanted-session", &project)).unwrap();
        let found = find_codex_session(&dir, &project, since, &["claimed-session".to_string()]);
        assert_eq!(found.as_deref(), Some("wanted-session"));
        assert_eq!(find_codex_session(&dir, &dir.join("nowhere"), since, &[]), None);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn codex_meta_supports_flat_format() {
        assert_eq!(
            codex_meta(r#"{"id":"abc","cwd":"/x","timestamp":"t"}"#),
            Some(("abc".to_string(), "/x".to_string()))
        );
        assert_eq!(codex_meta("not json"), None);
    }

    #[test]
    fn codex_subagent_threads_are_never_taken_for_the_main_session() {
        let main = r#"{"type":"session_meta","payload":{"id":"main-1","cwd":"/x","source":"cli"}}"#;
        assert_eq!(codex_meta(main), Some(("main-1".to_string(), "/x".to_string())));
        let spawned = r#"{"type":"session_meta","payload":{"id":"child-1","cwd":"/x","source":{"subagent":{"thread_spawn":{"parent_thread_id":"main-1"}}}}}"#;
        assert_eq!(codex_meta(spawned), None);
    }

    #[test]
    fn session_ids_must_be_safe() {
        assert!(is_safe_session_id("0199a-abc_DEF"));
        assert!(!is_safe_session_id("a&b"));
        assert!(!is_safe_session_id(""));
    }

    #[test]
    fn holder_binary_is_staged_once_and_pruned() {
        let dir = temp_dir("stage");
        let home = Home::at(dir.join("home"));
        home.ensure().unwrap();
        let source = dir.join("source-bin");
        fs::write(&source, b"binary-content").unwrap();
        let staged = holder_executable(&home, &source).unwrap();
        assert!(staged.exists());
        assert!(staged.file_name().unwrap().to_string_lossy().starts_with(HOLDER_PREFIX));
        assert_eq!(holder_executable(&home, &source).unwrap(), staged);
        let stale = home.bin_dir().join(format!("{HOLDER_PREFIX}old"));
        fs::write(&stale, b"old").unwrap();
        let running = home.bin_dir().join(format!("{HOLDER_PREFIX}running"));
        fs::write(&running, b"running").unwrap();
        fs::create_dir_all(home.agent_dir(4)).unwrap();
        let runtime = RuntimeInfo {
            holder_pid: 1,
            child_pid: None,
            port: 1,
            token: String::new(),
            started_at_ms: 1,
            protocol: 1,
            version: String::new(),
            exe: Some(PathBuf::from(running.to_string_lossy().replace('\\', "/"))),
        };
        fsutil::write_json_atomic(&home.runtime_file(4), &runtime).unwrap();
        prune_holder_binaries(&home, &staged);
        assert!(!stale.exists());
        assert!(staged.exists());
        assert!(running.exists(), "agents call their holder binary from hooks, so it must stay while they run");
        fs::remove_dir_all(dir).unwrap();
    }

    fn versioned_claude(dir: &Path, version: &str) -> PathBuf {
        let stem: String = version.chars().filter(char::is_ascii_alphanumeric).collect();
        if cfg!(windows) {
            let script = dir.join(format!("claude{stem}.cmd"));
            fs::write(&script, format!("@echo {version}\r\n")).unwrap();
            script
        } else {
            let script = dir.join(format!("claude{stem}"));
            fs::write(&script, format!("#!/bin/sh\necho '{version}'\n")).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
            }
            script
        }
    }

    fn setup(dir: &Path) -> HookSetup {
        HookSetup {
            exe: dir.join("bin").join("orkai-holder"),
            claude_settings: dir.join("claude-settings.json"),
            qwen_settings: dir.join("qwen-settings.json"),
            plugin: dir.join("orkai-plugin.js"),
            codex_home: Some(dir.join("codex-home")),
        }
    }

    fn claude_config(program: &Path, args: &[&str]) -> Config {
        Config {
            claude_command: program.to_string_lossy().into_owned(),
            claude_args: args.iter().map(|a| a.to_string()).collect(),
            ..Config::default()
        }
    }

    #[test]
    fn claude_gets_async_hooks_through_a_settings_file_when_its_version_supports_them() {
        let dir = temp_dir("claude-hooks");
        let program = versioned_claude(&dir, "2.1.302 (Claude Code)");
        let hooks = setup(&dir);
        let mut r = record(AgentKind::Claude, dir.clone(), "task");
        let spec = build_spec(
            &claude_config(&program, &[]),
            &mut r,
            LaunchMode::Fresh,
            TermSize::new(24, 80),
            &Homes::default(),
            Some(&hooks),
        )
        .unwrap();
        let settings_arg = hooks.claude_settings.to_string_lossy().into_owned();
        let tail: Vec<&str> = spec.args.iter().rev().take(2).rev().map(String::as_str).collect();
        assert_eq!(tail, vec!["--settings", settings_arg.as_str()]);
        assert!(spec.args.contains(&"--remote-control".to_string()));
        let written: serde_json::Value = fsutil::read_json(&hooks.claude_settings).unwrap();
        let command = written["hooks"]["Stop"][0]["hooks"][0]["command"].as_str().unwrap().to_string();
        assert_eq!(command, hooks::claude_hook_command(&hooks.exe, r.id));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn old_claude_versions_and_user_settings_keep_launching_without_hooks() {
        let dir = temp_dir("claude-no-hooks");
        let hooks = setup(&dir);
        let old = versioned_claude(&dir, "2.1.77 (Claude Code)");
        let mut r = record(AgentKind::Claude, dir.clone(), "task");
        let spec = build_spec(
            &claude_config(&old, &[]),
            &mut r,
            LaunchMode::Fresh,
            TermSize::new(24, 80),
            &Homes::default(),
            Some(&hooks),
        )
        .unwrap();
        assert!(!spec.args.contains(&"--settings".to_string()));
        assert!(!hooks.claude_settings.exists());

        let current = versioned_claude(&dir, "2.1.302 (Claude Code)");
        let own = claude_config(&current, &["--settings", "mine.json"]);
        let spec = build_spec(&own, &mut r, LaunchMode::Fresh, TermSize::new(24, 80), &Homes::default(), Some(&hooks))
            .unwrap();
        assert_eq!(spec.args.iter().filter(|a| *a == "--settings").count(), 1);
        assert!(spec.args.contains(&"mine.json".to_string()));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn codex_reports_finished_turns_through_notify_unless_one_is_configured() {
        let dir = temp_dir("codex-notify");
        let program = fake_program(&dir);
        let hooks = setup(&dir);
        let notify = hooks::codex_notify_override(&hooks.exe, 5);
        let mut r = record(AgentKind::Codex, dir.clone(), "task");
        let fresh = build_spec(
            &config_with(&program),
            &mut r,
            LaunchMode::Fresh,
            TermSize::new(24, 80),
            &Homes::default(),
            Some(&hooks),
        )
        .unwrap();
        assert_eq!(fresh.args, vec!["-c".to_string(), notify.clone()]);
        r.conversation_id = Some("abc-123".into());
        let resumed = build_spec(
            &config_with(&program),
            &mut r,
            LaunchMode::Resume,
            TermSize::new(24, 80),
            &Homes::default(),
            Some(&hooks),
        )
        .unwrap();
        assert_eq!(resumed.args, vec!["-c".to_string(), notify, "resume".into(), "abc-123".into()]);

        let own_args = Config { codex_args: vec!["-c".into(), "notify=[\"say\"]".into()], ..config_with(&program) };
        let spec =
            build_spec(&own_args, &mut r, LaunchMode::Fresh, TermSize::new(24, 80), &Homes::default(), Some(&hooks))
                .unwrap();
        assert_eq!(spec.args, vec!["-c", "notify=[\"say\"]"]);

        fs::create_dir_all(dir.join("codex-home")).unwrap();
        fs::write(dir.join("codex-home").join("config.toml"), "notify = [\"notify-send\", \"Codex\"]\n").unwrap();
        let spec = build_spec(
            &config_with(&program),
            &mut r,
            LaunchMode::Fresh,
            TermSize::new(24, 80),
            &Homes::default(),
            Some(&hooks),
        )
        .unwrap();
        assert!(spec.args.is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn terminals_never_get_hooks() {
        let dir = temp_dir("terminal-hooks");
        let program = fake_program(&dir);
        let hooks = setup(&dir);
        let mut r = record(AgentKind::Terminal, dir.clone(), "npm run dev");
        let spec = build_spec(
            &config_with(&program),
            &mut r,
            LaunchMode::Fresh,
            TermSize::new(24, 80),
            &Homes::default(),
            Some(&hooks),
        )
        .unwrap();
        assert!(spec.args.is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn batch_programs_launch_without_hooks_when_the_hook_arguments_are_unsafe_for_cmd() {
        let dir = temp_dir("batch-hooks");
        let program = dir.join("codex.cmd");
        fs::write(&program, b"@echo off\r\n").unwrap();
        let hooks = HookSetup { exe: PathBuf::from(r"C:\Users\me (work)\orkai-holder.exe"), ..setup(&dir) };
        let mut r = record(AgentKind::Codex, dir.clone(), "task");
        let spec = build_spec(
            &config_with(&program),
            &mut r,
            LaunchMode::Fresh,
            TermSize::new(24, 80),
            &Homes::default(),
            Some(&hooks),
        )
        .unwrap();
        assert!(spec.program.to_string_lossy().to_ascii_lowercase().ends_with("cmd.exe"));
        assert!(!spec.args.iter().any(|a| a.starts_with("notify=")));
        let safe = setup(&dir);
        let spec = build_spec(
            &config_with(&program),
            &mut r,
            LaunchMode::Fresh,
            TermSize::new(24, 80),
            &Homes::default(),
            Some(&safe),
        )
        .unwrap();
        assert!(spec.args.iter().any(|a| a.starts_with("notify=")));
        fs::remove_dir_all(dir).unwrap();
    }

    fn every_agent(program: &Path) -> Config {
        let command = program.to_string_lossy().into_owned();
        Config {
            kilo_command: command.clone(),
            opencode_command: command.clone(),
            agy_command: command.clone(),
            grok_command: command.clone(),
            qwen_command: command.clone(),
            kimi_command: command,
            ..config_with(program)
        }
    }

    fn spec_for(config: &Config, r: &mut AgentRecord, mode: LaunchMode, homes: &Homes) -> LaunchSpec {
        build_spec(config, r, mode, TermSize::new(24, 80), homes, None).unwrap()
    }

    #[test]
    fn grok_and_qwen_start_on_a_chosen_session_and_resume_it_once_it_exists() {
        let dir = temp_dir("chosen-sessions");
        let config = every_agent(&fake_program(&dir));
        let grok_home = dir.join("grok-home");
        let qwen_runtime = dir.join("qwen-runtime");
        let homes = Homes { grok: Some(grok_home.clone()), qwen: Some(qwen_runtime.clone()), ..Homes::default() };

        let mut grok = record(AgentKind::Grok, dir.clone(), " fix the login ");
        let fresh = spec_for(&config, &mut grok, LaunchMode::Fresh, &homes);
        let id = grok.conversation_id.clone().unwrap();
        assert!(is_uuid(&id));
        assert_eq!(fresh.args, vec!["--session-id".to_string(), id.clone()]);
        assert_eq!(fresh.initial_input.as_deref(), Some("fix the login"));
        let not_saved = spec_for(&config, &mut grok, LaunchMode::Resume, &homes);
        assert_eq!(not_saved.args, vec!["--session-id".to_string(), id.clone()], "a session never saved starts again");
        fs::create_dir_all(grok_home.join("sessions").join("C%3A%5Cwork").join(&id)).unwrap();
        let resumed = spec_for(&config, &mut grok, LaunchMode::Resume, &homes);
        assert_eq!(resumed.args, vec![format!("--resume={id}")]);
        assert_eq!(resumed.initial_input, None);

        let mut qwen = record(AgentKind::Qwen, dir.clone(), "write docs");
        let fresh = spec_for(&config, &mut qwen, LaunchMode::Fresh, &homes);
        let id = qwen.conversation_id.clone().unwrap();
        assert_eq!(fresh.args, vec!["--session-id".to_string(), id.clone()]);
        let chats = qwen_runtime.join("projects").join("c--work").join("chats");
        fs::create_dir_all(&chats).unwrap();
        fs::write(chats.join(format!("{id}.jsonl")), b"{}\n").unwrap();
        let resumed = spec_for(&config, &mut qwen, LaunchMode::Resume, &homes);
        assert_eq!(resumed.args, vec!["--resume".to_string(), id]);
        assert_eq!(resumed.initial_input, None);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn discovered_sessions_are_resumed_with_each_agent_flag_after_its_own_args() {
        let dir = temp_dir("discovered-sessions");
        let program = fake_program(&dir);
        let config = Config { opencode_args: vec!["--model".into(), "x/y".into()], ..every_agent(&program) };
        let cases = [
            (
                AgentKind::Opencode,
                "ses_f5ded97fdffeDWMNJBgPBTSqRU",
                vec!["--model", "x/y", "--session", "ses_f5ded97fdffeDWMNJBgPBTSqRU"],
            ),
            (AgentKind::Kilo, "ses_f5ded97fdffeDWMNJBgPBTSqRU", vec!["--session", "ses_f5ded97fdffeDWMNJBgPBTSqRU"]),
            (
                AgentKind::Kimi,
                "session_123e4567-e89b-42d3-a456-426614174000",
                vec!["--session", "session_123e4567-e89b-42d3-a456-426614174000"],
            ),
            (
                AgentKind::Antigravity,
                "055a398f-db14-4c5f-abbb-1bf03f8120a7",
                vec!["--conversation=055a398f-db14-4c5f-abbb-1bf03f8120a7"],
            ),
        ];
        for (kind, id, expected) in cases {
            let mut r = record(kind, dir.clone(), "task");
            let fresh = spec_for(&config, &mut r, LaunchMode::Fresh, &Homes::default());
            let prompted = matches!(kind, AgentKind::Opencode | AgentKind::Kilo);
            let typed = if prompted { None } else { Some("task") };
            assert_eq!(fresh.initial_input.as_deref(), typed, "{kind:?}");
            assert_eq!(fresh.args.ends_with(&["--prompt".to_string(), "task".to_string()]), prompted, "{kind:?}");
            assert_eq!(r.conversation_id, None, "{kind:?} never invents a session id");
            r.conversation_id = Some(id.into());
            let resumed = spec_for(&config, &mut r, LaunchMode::Resume, &Homes::default());
            assert_eq!(resumed.args, expected, "{kind:?}");
            assert_eq!(resumed.initial_input, None, "{kind:?}");
            let restarted = spec_for(&config, &mut r, LaunchMode::Fresh, &Homes::default());
            assert!(!restarted.args.iter().any(|arg| arg.contains(id)), "{kind:?}");
            r.conversation_id = Some("a&b".into());
            let unsafe_id = spec_for(&config, &mut r, LaunchMode::Resume, &Homes::default());
            assert_eq!(unsafe_id.initial_input.as_deref(), typed, "{kind:?}");
            assert!(!unsafe_id.args.iter().any(|arg| arg.contains("a&b")), "{kind:?}");
        }
        fs::remove_dir_all(dir).unwrap();
    }

    fn env_of<'a>(spec: &'a LaunchSpec, name: &str) -> Option<&'a str> {
        spec.env.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }

    #[test]
    fn opencode_and_kilo_load_the_orkai_plugin_from_their_config_env() {
        let dir = temp_dir("plugin");
        let config = every_agent(&fake_program(&dir));
        let hooks = setup(&dir);
        for (kind, variable, other) in [
            (AgentKind::Opencode, OPENCODE_CONFIG_CONTENT, KILO_CONFIG_CONTENT),
            (AgentKind::Kilo, KILO_CONFIG_CONTENT, OPENCODE_CONFIG_CONTENT),
        ] {
            let _ = fs::remove_file(&hooks.plugin);
            let mut r = record(kind, dir.clone(), "task");
            let spec =
                build_spec(&config, &mut r, LaunchMode::Fresh, TermSize::new(24, 80), &Homes::default(), Some(&hooks))
                    .unwrap();
            assert_eq!(spec.args, vec!["--prompt", "task"], "{kind:?}");
            assert_eq!(env_of(&spec, other), None);
            if env_is_set(variable) {
                assert_eq!(env_of(&spec, variable), None, "a config the user already passes is left alone");
                continue;
            }
            assert_eq!(env_of(&spec, variable), Some(hooks::plugin_config(&hooks.plugin).as_str()), "{kind:?}");
            let plugin = fs::read_to_string(&hooks.plugin).unwrap();
            assert_eq!(plugin, hooks::opencode_plugin(&hooks.exe, r.id));
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn antigravity_is_asked_to_report_its_status_over_osc_7501() {
        let dir = temp_dir("agy-status");
        let config = every_agent(&fake_program(&dir));
        let hooks = setup(&dir);
        let mut r = record(AgentKind::Antigravity, dir.clone(), "task");
        let spec =
            build_spec(&config, &mut r, LaunchMode::Fresh, TermSize::new(24, 80), &Homes::default(), Some(&hooks))
                .unwrap();
        let expected = if env_is_set(AGY_STATUS_ENV) { None } else { Some(AGY_STATUS_ON) };
        assert_eq!(env_of(&spec, AGY_STATUS_ENV), expected);
        assert!(spec.args.is_empty());
        let plain = spec_for(&config, &mut r, LaunchMode::Fresh, &Homes::default());
        assert_eq!(env_of(&plain, AGY_STATUS_ENV), None);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn qwen_gets_claude_style_hooks_through_its_system_defaults_file() {
        let dir = temp_dir("qwen-hooks");
        let config = every_agent(&fake_program(&dir));
        let hooks = setup(&dir);
        let mut r = record(AgentKind::Qwen, dir.clone(), "task");
        let spec =
            build_spec(&config, &mut r, LaunchMode::Fresh, TermSize::new(24, 80), &Homes::default(), Some(&hooks))
                .unwrap();
        let taken = env_is_set(hooks::QWEN_SYSTEM_DEFAULTS_ENV) || hooks::qwen_system_defaults().exists();
        match hooks::qwen_hook_command(&hooks.exe, r.id).filter(|_| !taken) {
            Some(command) => {
                let path = hooks.qwen_settings.to_string_lossy().into_owned();
                assert_eq!(env_of(&spec, hooks::QWEN_SYSTEM_DEFAULTS_ENV), Some(path.as_str()));
                let written: serde_json::Value = fsutil::read_json(&hooks.qwen_settings).unwrap();
                assert_eq!(written, hooks::claude_settings(&command));
            }
            None => assert_eq!(env_of(&spec, hooks::QWEN_SYSTEM_DEFAULTS_ENV), None),
        }
        assert!(spec.args.starts_with(&["--session-id".to_string()]));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn grok_and_kimi_have_no_hooks_to_inject() {
        let dir = temp_dir("no-hooks");
        let config = every_agent(&fake_program(&dir));
        let hooks = setup(&dir);
        for kind in [AgentKind::Grok, AgentKind::Kimi] {
            let mut r = record(kind, dir.clone(), "task");
            let with =
                build_spec(&config, &mut r, LaunchMode::Fresh, TermSize::new(24, 80), &Homes::default(), Some(&hooks))
                    .unwrap();
            let without = spec_for(&config, &mut r, LaunchMode::Fresh, &Homes::default());
            assert_eq!(with.args, without.args, "{kind:?}");
            assert_eq!(with.env, without.env, "{kind:?}");
        }
        assert!(!hooks.plugin.exists() && !hooks.qwen_settings.exists() && !hooks.claude_settings.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn bypass_starts_each_agent_with_its_own_flag_and_keeps_it_when_resumed() {
        let dir = temp_dir("bypass");
        let config = every_agent(&fake_program(&dir));
        let cases = [
            (AgentKind::Claude, Some("--dangerously-skip-permissions")),
            (AgentKind::Codex, Some("--dangerously-bypass-approvals-and-sandbox")),
            (AgentKind::Kilo, Some("--auto")),
            (AgentKind::Opencode, Some("--auto")),
            (AgentKind::Antigravity, Some("--dangerously-skip-permissions")),
            (AgentKind::Grok, Some("--always-approve")),
            (AgentKind::Qwen, Some("--yolo")),
            (AgentKind::Kimi, Some("--auto")),
            (AgentKind::Terminal, None),
        ];
        assert_eq!(cases.len(), AgentKind::ALL.len(), "every agent kind is covered");
        let id = "123e4567-e89b-42d3-a456-426614174000";
        for (kind, flag) in cases {
            let count = |spec: &LaunchSpec| spec.args.iter().filter(|arg| Some(arg.as_str()) == flag).count();
            let mut r = record(kind, dir.clone(), "task");
            let asking = spec_for(&config, &mut r, LaunchMode::Fresh, &Homes::default());
            assert_eq!(count(&asking), 0, "{kind:?} asks for permissions unless bypass is on");
            r.bypass = true;
            let fresh = spec_for(&config, &mut r, LaunchMode::Fresh, &Homes::default());
            assert_eq!(count(&fresh), usize::from(flag.is_some()), "{kind:?}: {:?}", fresh.args);
            r.conversation_id = Some(id.into());
            let resumed = spec_for(&config, &mut r, LaunchMode::Resume, &Homes::default());
            assert_eq!(count(&resumed), usize::from(flag.is_some()), "{kind:?}: {:?}", resumed.args);
        }
        let mut codex = record(AgentKind::Codex, dir.clone(), "task");
        codex.bypass = true;
        codex.conversation_id = Some("abc-123".into());
        let resumed = spec_for(&config, &mut codex, LaunchMode::Resume, &Homes::default());
        assert_eq!(resumed.args, vec!["resume", "abc-123", "--dangerously-bypass-approvals-and-sandbox"]);
        let mut kilo = record(AgentKind::Kilo, dir.clone(), "task");
        kilo.bypass = true;
        let fresh = spec_for(&config, &mut kilo, LaunchMode::Fresh, &Homes::default());
        assert_eq!(fresh.args, vec!["--auto", "--prompt", "task"]);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn bypass_leaves_a_permission_mode_chosen_in_the_config_alone() {
        let dir = temp_dir("bypass-configured");
        let program = fake_program(&dir);
        let cases: [(AgentKind, &[&str], &str); 6] = [
            (AgentKind::Codex, &["-a", "on-request"], "--dangerously-bypass-approvals-and-sandbox"),
            (AgentKind::Codex, &["--yolo"], "--dangerously-bypass-approvals-and-sandbox"),
            (AgentKind::Qwen, &["--approval-mode", "auto-edit"], "--yolo"),
            (AgentKind::Kimi, &["--yolo"], "--auto"),
            (AgentKind::Grok, &["--yolo"], "--always-approve"),
            (AgentKind::Claude, &["--dangerously-skip-permissions"], "--dangerously-skip-permissions"),
        ];
        for (kind, configured, flag) in cases {
            let mut config = every_agent(&program);
            let own: Vec<String> = configured.iter().map(|arg| arg.to_string()).collect();
            match kind {
                AgentKind::Codex => config.codex_args = own.clone(),
                AgentKind::Qwen => config.qwen_args = own.clone(),
                AgentKind::Kimi => config.kimi_args = own.clone(),
                AgentKind::Grok => config.grok_args = own.clone(),
                _ => config.claude_args = own.clone(),
            }
            let mut r = record(kind, dir.clone(), "task");
            r.bypass = true;
            let spec = spec_for(&config, &mut r, LaunchMode::Fresh, &Homes::default());
            let expected = usize::from(configured.contains(&flag));
            assert_eq!(spec.args.iter().filter(|arg| *arg == flag).count(), expected, "{kind:?}: {:?}", spec.args);
            assert!(spec.args.starts_with(&own), "{kind:?}: {:?}", spec.args);
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn kilo_bypass_runs_without_the_daemon_that_would_drop_auto() {
        let dir = temp_dir("kilo-bypass");
        let config = every_agent(&fake_program(&dir));
        let mut kilo = record(AgentKind::Kilo, dir.clone(), "task");
        let asking = spec_for(&config, &mut kilo, LaunchMode::Fresh, &Homes::default());
        assert_eq!(env_of(&asking, KILO_NO_DAEMON), None);
        kilo.bypass = true;
        let bypassed = spec_for(&config, &mut kilo, LaunchMode::Fresh, &Homes::default());
        let expected = if env_is_set(KILO_NO_DAEMON) { None } else { Some(NO_DAEMON_ON) };
        assert_eq!(env_of(&bypassed, KILO_NO_DAEMON), expected, "a value the user already sets is left alone");
        let mut opencode = record(AgentKind::Opencode, dir.clone(), "task");
        opencode.bypass = true;
        let spec = spec_for(&config, &mut opencode, LaunchMode::Fresh, &Homes::default());
        assert_eq!(env_of(&spec, KILO_NO_DAEMON), None);
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn npm_shims_of_native_programs_run_the_program_itself() {
        let dir = temp_dir("npm-shims");
        let exe = dir.join("node_modules").join("opencode-ai").join("bin").join("opencode.exe");
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::write(&exe, b"").unwrap();
        let classic = dir.join("opencode.cmd");
        let shim = "@ECHO off\r\nGOTO start\r\n:find_dp0\r\nSET dp0=%~dp0\r\nEXIT /b\r\n:start\r\nSETLOCAL\r\nCALL :find_dp0\r\n\"%dp0%\\node_modules\\opencode-ai\\bin\\opencode.exe\"   %*\r\n";
        fs::write(&classic, shim).unwrap();
        let short = dir.join("short.cmd");
        fs::write(&short, "@ECHO off\r\n\"%~dp0node_modules\\opencode-ai\\bin\\opencode.exe\" %*\r\n").unwrap();
        for shim in [&classic, &short] {
            assert_eq!(npm_shim_target(shim).as_deref(), Some(exe.as_path()));
            let (program, args) =
                wrap_for_platform(shim.clone(), vec!["--prompt".into(), "fix (it) & ship".into()]).unwrap();
            assert_eq!(program, exe);
            assert_eq!(args, vec!["--prompt", "fix (it) & ship"]);
        }
        let node = dir.join("codex.cmd");
        fs::write(&node, "@ECHO off\r\n\"%_prog%\"  \"%dp0%\\node_modules\\@openai\\codex\\bin\\codex.js\" %*\r\n")
            .unwrap();
        assert_eq!(npm_shim_target(&node), None);
        let missing = dir.join("missing.cmd");
        fs::write(&missing, "\"%~dp0nowhere\\gone.exe\" %*\r\n").unwrap();
        assert_eq!(npm_shim_target(&missing), None);
        let escape = dir.join("escape.cmd");
        fs::write(&escape, "\"%~dp0%OTHER%\\x.exe\" %*\r\n").unwrap();
        assert_eq!(npm_shim_target(&escape), None);
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn a_task_that_cmd_cannot_carry_is_typed_into_opencode_instead() {
        let dir = temp_dir("prompt-fallback");
        let shim = dir.join("kilo.cmd");
        fs::write(&shim, "@ECHO off\r\nnode \"%~dp0kilo.js\" %*\r\n").unwrap();
        let config = Config { kilo_command: shim.to_string_lossy().into_owned(), ..Config::default() };
        let mut r = record(AgentKind::Kilo, dir.clone(), "fix the login");
        let safe = spec_for(&config, &mut r, LaunchMode::Fresh, &Homes::default());
        assert!(safe.args.ends_with(&["--prompt".to_string(), "fix the login".to_string()]), "{:?}", safe.args);
        assert_eq!(safe.initial_input, None);
        r.task = "fix the login (urgent) & deploy".into();
        let typed = spec_for(&config, &mut r, LaunchMode::Fresh, &Homes::default());
        assert!(!typed.args.iter().any(|arg| arg == "--prompt"), "{:?}", typed.args);
        assert_eq!(typed.initial_input.as_deref(), Some("fix the login (urgent) & deploy"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn listed_opencode_sessions_are_matched_by_folder_and_start_time() {
        let cwd = if cfg!(windows) { PathBuf::from(r"C:\Users\me\work") } else { PathBuf::from("/home/me/work") };
        let directory = if cfg!(windows) { "C:/Users/me/work" } else { "/home/me/work" };
        let since = 1_800_000_000_000u64;
        let listing = serde_json::json!([
            { "id": "ses_newest", "created": since + 9_000, "updated": since + 9_500, "directory": directory },
            { "id": "ses_claimed", "created": since + 1_000, "directory": directory },
            { "id": "ses_wanted", "created": since + 2_000, "directory": directory },
            { "id": "ses_other_folder", "created": since + 1_500, "directory": "/elsewhere" },
            { "id": "ses_before", "created": since - SESSION_CLOCK_SKEW_MS - 1, "directory": directory },
            { "id": "not-a-session", "created": since + 1_200, "directory": directory },
            { "id": "ses_bad&id", "created": since + 1_100, "directory": directory },
        ])
        .to_string();
        let claimed = vec!["ses_claimed".to_string()];
        assert_eq!(pick_listed_session(&listing, &cwd, since, &claimed).as_deref(), Some("ses_wanted"));
        let neighbours = serde_json::json!([
            { "id": "ses_started_just_before", "created": since - 5_000, "directory": directory },
            { "id": "ses_mine", "created": since + 3_000, "directory": directory },
        ])
        .to_string();
        assert_eq!(
            pick_listed_session(&neighbours, &cwd, since, &[]).as_deref(),
            Some("ses_mine"),
            "an agent started a moment earlier in the same folder keeps its own session"
        );
        let skewed =
            serde_json::json!([{ "id": "ses_skewed", "created": since - 5_000, "directory": directory }]).to_string();
        assert_eq!(pick_listed_session(&skewed, &cwd, since, &[]).as_deref(), Some("ses_skewed"));
        assert_eq!(pick_listed_session("", &cwd, since, &[]), None, "no sessions print nothing");
        assert_eq!(pick_listed_session("Error: boom", &cwd, since, &[]), None);
        assert_eq!(pick_listed_session("[]", &cwd, since, &[]), None);
    }

    #[test]
    fn kimi_sessions_are_found_in_the_session_index() {
        let dir = temp_dir("kimi-index");
        let home = dir.join("kimi-home");
        let cwd = dir.join("project");
        let since = now_ms();
        let session = |id: &str, work: &Path, created: u64| {
            let session_dir = home.join("sessions").join("wd_project_0123").join(id);
            fs::create_dir_all(&session_dir).unwrap();
            fs::write(session_dir.join(KIMI_STATE), serde_json::json!({ "id": id, "createdAt": created }).to_string())
                .unwrap();
            serde_json::json!({ "sessionId": id, "sessionDir": session_dir, "workDir": work }).to_string()
        };
        let lines = [
            session("session_old", &cwd, since - SESSION_CLOCK_SKEW_MS - 10),
            session("session_neighbour", &cwd, since - 5_000),
            session("session_claimed", &cwd, since + 10),
            session("session_wanted", &cwd, since + 20),
            session("session_later", &cwd, since + 30),
            session("session_elsewhere", &dir.join("other"), since + 5),
            "not json".to_string(),
        ];
        fs::write(home.join(KIMI_SESSION_INDEX), lines.join("\n")).unwrap();
        let claimed = vec!["session_claimed".to_string()];
        assert_eq!(find_kimi_session(&home, &cwd, since, &claimed).as_deref(), Some("session_wanted"));
        assert_eq!(find_kimi_session(&home, &dir.join("nowhere"), since, &[]), None);
        assert_eq!(find_kimi_session(&dir.join("missing"), &cwd, since, &[]), None);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn antigravity_resumes_the_last_conversation_of_the_folder() {
        let dir = temp_dir("agy-conversations");
        let home = dir.join("agy-home");
        let cwd = dir.join("project");
        let id = "055a398f-db14-4c5f-abbb-1bf03f8120a7";
        let file = home.join("cache").join("last_conversations.json");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        let map = serde_json::json!({ cwd.to_string_lossy(): id, "/other": "123e4567-e89b-42d3-a456-426614174000" });
        fs::write(&file, map.to_string()).unwrap();
        assert_eq!(find_antigravity_conversation(&home, &cwd, &[]).as_deref(), Some(id));
        assert_eq!(find_antigravity_conversation(&home, &cwd, &[id.to_string()]), None, "another agent owns it");
        assert_eq!(find_antigravity_conversation(&home, &dir.join("x"), &[]), None);
        fs::write(&file, serde_json::json!({ cwd.to_string_lossy(): "../escape" }).to_string()).unwrap();
        assert_eq!(find_antigravity_conversation(&home, &cwd, &[]), None);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn command_output_returns_stdout_and_gives_up_on_hung_programs() {
        let (program, quick, slow): (&str, Vec<&str>, Vec<&str>) = if cfg!(windows) {
            ("cmd.exe", vec!["/d", "/c", "echo orkai-output"], vec!["/d", "/c", "ping -n 30 127.0.0.1 >nul"])
        } else {
            ("/bin/sh", vec!["-c", "echo orkai-output"], vec!["-c", "sleep 30"])
        };
        let output = command_output(Command::new(program).args(&quick), Duration::from_secs(20)).unwrap();
        assert_eq!(output.trim(), "orkai-output");
        let started = Instant::now();
        assert_eq!(command_output(Command::new(program).args(&slow), Duration::from_millis(300)), None);
        assert!(started.elapsed() < Duration::from_secs(20));
        assert_eq!(command_output(&mut Command::new("orkai-definitely-missing-program"), Duration::from_secs(1)), None);
        let orphan: Vec<&str> = if cfg!(windows) {
            vec!["/d", "/c", "start /b ping -n 20 127.0.0.1 & echo orkai-output"]
        } else {
            vec!["-c", "sleep 20 & echo orkai-output"]
        };
        let started = Instant::now();
        let _ = command_output(Command::new(program).args(&orphan), Duration::from_secs(2));
        assert!(started.elapsed() < Duration::from_secs(10), "a child left holding the output never blocks the caller");
    }
}
