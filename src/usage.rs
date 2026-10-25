use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::config::Config;
use crate::launch;
use crate::model::AgentKind;
use crate::paths::user_home;
use crate::util::{now_ms, rfc3339_to_ms};

pub const PROVIDERS: [AgentKind; 7] = [
    AgentKind::Claude,
    AgentKind::Codex,
    AgentKind::Kilo,
    AgentKind::Opencode,
    AgentKind::Antigravity,
    AgentKind::Grok,
    AgentKind::Kimi,
];
pub const REFRESH_EVERY: Duration = Duration::from_secs(120);
pub const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
pub const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
pub const CLAUDE_CREDENTIALS_FILE: &str = ".credentials.json";
pub const CODEX_AUTH_FILE: &str = "auth.json";
pub const OPENCODE_GO_USAGE_URL: &str = "https://opencode.ai/zen/go/v1/usage";
pub const KILO_API_URL: &str = "https://api.kilo.ai";
pub const GROK_BILLING_URL: &str = "https://cli-chat-proxy.grok.com/v1/billing?format=credits";
pub const KIMI_BASE_URL: &str = "https://api.kimi.com/coding/v1";
pub const AUTH_FILE: &str = "auth.json";
pub const KIMI_CONFIG_FILE: &str = "config.toml";
pub const KIMI_CREDENTIALS_DIR: &str = "credentials";
pub const ANTIGRAVITY_SETTINGS_FILE: &str = "settings.json";
const CLAUDE_OAUTH_BETA: &str = "oauth-2025-04-20";
const OPENCODE_GO_PROVIDER: &str = "opencode-go";
const OPENCODE_ZEN_PROVIDER: &str = "opencode";
const KILO_PROVIDER: &str = "kilo";
const KILO_API_URL_ENV: &str = "KILO_API_URL";
const KILO_BALANCE_PATH: &str = "/api/profile/balance";
const KILO_PASS_PATH: &str = "/api/trpc/kiloPass.getState?batch=1&input=%7B%220%22%3Anull%7D";
const KILO_PASS_POINTER: &str = "/0/result/data/json/subscription";
const KILO_PASS_ACTIVE: [&str; 3] = ["active", "past_due", "trialing"];
const KILO_EDITOR_HEADER: &str = "X-KILOCODE-EDITORNAME";
const KILO_EDITOR: &str = "Kilo CLI";
const KILO_ORGANIZATION_HEADER: &str = "x-kilocode-organizationid";
const OAUTH_TYPE: &str = "oauth";
const GROK_AUTH_ENV: &str = "GROK_AUTH_PATH";
const GROK_API_KEY_SCOPE: &str = "xai::api_key";
const GROK_LEGACY_SCOPE: &str = "https://accounts.x.ai/sign-in";
const GROK_API_KEY_MODE: &str = "api_key";
const GROK_TOKEN_AUTH_HEADER: &str = "X-XAI-Token-Auth";
const GROK_TOKEN_AUTH: &str = "xai-grok-cli";
const GROK_CLIENT_MODE_HEADER: &str = "x-grok-client-mode";
const GROK_CLIENT_MODE: &str = "interactive";
const GROK_USER_HEADER: &str = "x-userid";
const GROK_MONTHLY_PERIOD: &str = "MONTH";
const KIMI_PROVIDER_SECTION: &str = "[providers.\"managed:kimi-code\"";
const KIMI_DEFAULT_CREDENTIALS: &str = "kimi-code";
const KIMI_OAUTH_PREFIX: &str = "oauth/";
const KIMI_USAGE_PATH: &str = "/usages";
const KIMI_SESSION_MINUTES: f64 = 300.0;
const MINUTES_PER_HOUR: f64 = 60.0;
const MINUTES_PER_DAY: f64 = 1_440.0;
const AGY_PROBE_ARGS: [&str; 4] = ["-p", "/usage", "--output-format", "json"];
const AGY_NO_UPDATE_ENV: &str = "AGY_CLI_DISABLE_AUTO_UPDATE";
const AGY_API_KEY_PROVIDER: &str = "gemini";
const AGY_PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const AGY_BUCKET_NAMES: [&str; 7] = ["bucket_id", "bucketId", "id", "display_name", "displayName", "window", "name"];
const SESSION_HINTS: [&str; 4] = ["5h", "five", "hour", "session"];
const SIGNED_OUT_HINTS: [&str; 6] = ["sign in", "signed in", "log in", "logged in", "login", "not authenticated"];
#[cfg(target_os = "macos")]
const CLAUDE_KEYCHAIN_SERVICE: &str = "Claude Code-credentials";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RESPONSE_BYTES: u64 = 512 * 1024;
const SESSION_WINDOW_MAX_SECS: i64 = 24 * 3_600;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Window {
    pub used_percent: f64,
    pub resets_at_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Limits {
    pub session: Option<Window>,
    pub weekly: Option<Window>,
    pub monthly: Option<Window>,
    pub credits: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Problem {
    SignedOut,
    Expired,
    ApiKey,
    Unavailable,
}

impl Problem {
    pub fn label(self) -> &'static str {
        match self {
            Problem::SignedOut => "not signed in",
            Problem::Expired => "login expired",
            Problem::ApiKey => "api key · no plan limits",
            Problem::Unavailable => "unavailable",
        }
    }

    pub fn display_label(self, lang: crate::i18n::Language) -> &'static str {
        match lang {
            crate::i18n::Language::En => self.label(),
            crate::i18n::Language::PtBr => match self {
                Problem::SignedOut => "não conectado",
                Problem::Expired => "sessão expirada",
                Problem::ApiKey => "chave api · sem limites de plano",
                Problem::Unavailable => "indisponível",
            },
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Usage {
    pub limits: Option<Limits>,
    pub problem: Option<Problem>,
    pub updated_at_ms: Option<u64>,
}

impl Usage {
    pub fn is_signed_in(&self) -> bool {
        self.problem != Some(Problem::SignedOut)
    }

    pub fn apply(&mut self, result: Result<Limits, Problem>, now: u64) {
        match result {
            Ok(limits) => {
                self.limits = Some(limits);
                self.problem = None;
                self.updated_at_ms = Some(now);
            }
            Err(Problem::Unavailable) => self.problem = Some(Problem::Unavailable),
            Err(problem) => {
                self.limits = None;
                self.problem = Some(problem);
            }
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct ClaudeCredentials {
    claude_ai_oauth: Option<ClaudeOauth>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct ClaudeOauth {
    access_token: String,
    expires_at: Option<u64>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ClaudeUsage {
    five_hour: Option<ClaudeWindow>,
    seven_day: Option<ClaudeWindow>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ClaudeWindow {
    utilization: Option<f64>,
    resets_at: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CodexAuth {
    #[serde(rename = "OPENAI_API_KEY")]
    openai_api_key: Option<String>,
    tokens: Option<CodexTokens>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CodexTokens {
    access_token: String,
    account_id: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CodexUsage {
    rate_limit: Option<CodexRateLimit>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CodexRateLimit {
    primary_window: Option<CodexWindow>,
    secondary_window: Option<CodexWindow>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CodexWindow {
    used_percent: f64,
    limit_window_seconds: i64,
    reset_at: i64,
}

fn parse<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, Problem> {
    serde_json::from_slice(bytes).map_err(|_| Problem::Unavailable)
}

fn get(url: &str, headers: &[(&str, String)]) -> Result<Vec<u8>, Problem> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        .https_only(url.starts_with("https://"))
        .user_agent(format!("orkai/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let mut request = agent.get(url).header("Accept", "application/json");
    for (name, value) in headers {
        request = request.header(*name, value);
    }
    match request.call() {
        Ok(mut response) => {
            response.body_mut().with_config().limit(MAX_RESPONSE_BYTES).read_to_vec().map_err(|_| Problem::Unavailable)
        }
        Err(ureq::Error::StatusCode(401 | 403)) => Err(Problem::Expired),
        Err(_) => Err(Problem::Unavailable),
    }
}

pub fn fetch(kind: AgentKind, config: &Config) -> Result<Limits, Problem> {
    match kind {
        AgentKind::Claude => fetch_claude(launch::claude_home().as_deref(), CLAUDE_USAGE_URL, now_ms()),
        AgentKind::Codex => fetch_codex(launch::codex_home().as_deref(), CODEX_USAGE_URL),
        AgentKind::Kilo => fetch_kilo(launch::kilo_data_dir().as_deref(), &kilo_api_url()),
        AgentKind::Opencode => fetch_opencode(launch::opencode_data_dir().as_deref(), OPENCODE_GO_USAGE_URL),
        AgentKind::Antigravity => fetch_antigravity(&config.agy_command, launch::antigravity_home().as_deref()),
        AgentKind::Grok => fetch_grok(grok_auth_file().as_deref(), GROK_BILLING_URL, now_ms()),
        AgentKind::Kimi => fetch_kimi(launch::kimi_home().as_deref(), KIMI_BASE_URL, now_ms()),
        AgentKind::Qwen | AgentKind::Terminal => Err(Problem::Unavailable),
    }
}

fn kilo_api_url() -> String {
    std::env::var(KILO_API_URL_ENV)
        .ok()
        .filter(|url| url.starts_with("https://"))
        .unwrap_or_else(|| KILO_API_URL.into())
}

fn grok_auth_file() -> Option<PathBuf> {
    std::env::var_os(GROK_AUTH_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| launch::grok_home().map(|home| home.join(AUTH_FILE)))
}

fn bearer(token: &str) -> (&'static str, String) {
    ("Authorization", format!("Bearer {token}"))
}

fn number(value: Option<&Value>) -> Option<f64> {
    let value = value?;
    value.as_f64().or_else(|| value.as_str()?.trim().parse().ok())
}

fn text<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| value.get(key)?.as_str())
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ProviderAuth {
    #[serde(rename = "type")]
    kind: String,
    key: Option<String>,
    access: Option<String>,
    #[serde(rename = "accountId")]
    account_id: Option<String>,
}

impl ProviderAuth {
    fn secret(&self) -> Option<&str> {
        self.key.as_deref().or(self.access.as_deref()).filter(|secret| !secret.trim().is_empty())
    }
}

fn provider_auth(data_dir: Option<&Path>) -> Result<BTreeMap<String, ProviderAuth>, Problem> {
    let bytes = data_dir.and_then(|dir| fs::read(dir.join(AUTH_FILE)).ok()).ok_or(Problem::SignedOut)?;
    let entries: BTreeMap<String, Value> = parse(&bytes)?;
    Ok(entries
        .into_iter()
        .filter_map(|(key, value)| Some((key.trim_end_matches('/').to_string(), serde_json::from_value(value).ok()?)))
        .collect())
}

pub fn fetch_opencode(data_dir: Option<&Path>, url: &str) -> Result<Limits, Problem> {
    let auth = provider_auth(data_dir)?;
    if let Some(key) = auth.get(OPENCODE_GO_PROVIDER).and_then(ProviderAuth::secret) {
        return opencode_limits(&get(url, &[bearer(key)])?);
    }
    if auth.get(OPENCODE_ZEN_PROVIDER).and_then(ProviderAuth::secret).is_some() {
        return Err(Problem::ApiKey);
    }
    Err(Problem::SignedOut)
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct OpencodeUsage {
    usage: OpencodeWindows,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct OpencodeWindows {
    rolling: Option<PercentWindow>,
    weekly: Option<PercentWindow>,
    monthly: Option<PercentWindow>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct PercentWindow {
    percent: Option<f64>,
    resets_at: Option<String>,
}

fn percent_window(window: Option<PercentWindow>) -> Option<Window> {
    let window = window?;
    Some(Window { used_percent: window.percent?, resets_at_ms: window.resets_at.as_deref().and_then(rfc3339_to_ms) })
}

pub fn opencode_limits(body: &[u8]) -> Result<Limits, Problem> {
    let usage: OpencodeUsage = parse(body)?;
    Ok(Limits {
        session: percent_window(usage.usage.rolling),
        weekly: percent_window(usage.usage.weekly),
        monthly: percent_window(usage.usage.monthly),
        credits: None,
    })
}

pub fn fetch_kilo(data_dir: Option<&Path>, base: &str) -> Result<Limits, Problem> {
    let auth = provider_auth(data_dir)?;
    let kilo = auth.get(KILO_PROVIDER).ok_or(Problem::SignedOut)?;
    let Some(token) = kilo.secret().filter(|_| kilo.kind == OAUTH_TYPE) else {
        return Err(if kilo.secret().is_some() { Problem::ApiKey } else { Problem::SignedOut });
    };
    let organization = kilo.account_id.as_deref().filter(|id| !id.is_empty());
    let mut headers = vec![bearer(token)];
    if let Some(organization) = organization {
        headers.push((KILO_ORGANIZATION_HEADER, organization.to_string()));
    }
    let credits = kilo_balance(&get(&format!("{base}{KILO_BALANCE_PATH}"), &headers)?)?;
    let monthly = if organization.is_none() {
        headers.push((KILO_EDITOR_HEADER, KILO_EDITOR.to_string()));
        get(&format!("{base}{KILO_PASS_PATH}"), &headers).ok().and_then(|body| kilo_pass(&body))
    } else {
        None
    };
    Ok(Limits { monthly, credits: Some(credits), ..Limits::default() })
}

pub fn kilo_balance(body: &[u8]) -> Result<f64, Problem> {
    let value: Value = parse(body)?;
    number(value.get("balance")).ok_or(Problem::Unavailable)
}

pub fn kilo_pass(body: &[u8]) -> Option<Window> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let subscription = value.pointer(KILO_PASS_POINTER)?;
    if !KILO_PASS_ACTIVE.contains(&subscription.get("status")?.as_str()?) {
        return None;
    }
    let base = number(subscription.get("currentPeriodBaseCreditsUsd")).filter(|base| *base > 0.0)?;
    let used = number(subscription.get("currentPeriodUsageUsd")).unwrap_or(0.0);
    let resets_at_ms = text(subscription, &["nextBillingAt", "nextRenewalAt"]).and_then(rfc3339_to_ms);
    Some(Window { used_percent: used / base * 100.0, resets_at_ms })
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct GrokAuth {
    key: String,
    auth_mode: String,
    user_id: Option<String>,
    expires_at: Option<String>,
}

pub fn fetch_grok(auth_file: Option<&Path>, url: &str, now: u64) -> Result<Limits, Problem> {
    let bytes = auth_file.and_then(|file| fs::read(file).ok()).ok_or(Problem::SignedOut)?;
    let scopes: BTreeMap<String, Value> = parse(&bytes)?;
    let mut api_key = false;
    let mut login = None;
    for (scope, value) in scopes {
        let Ok(auth) = serde_json::from_value::<GrokAuth>(value) else { continue };
        if auth.key.trim().is_empty() || scope == GROK_LEGACY_SCOPE {
            continue;
        }
        if scope == GROK_API_KEY_SCOPE || auth.auth_mode == GROK_API_KEY_MODE {
            api_key = true;
        } else if login.is_none() {
            login = Some(auth);
        }
    }
    let Some(auth) = login else {
        return Err(if api_key { Problem::ApiKey } else { Problem::SignedOut });
    };
    if auth.expires_at.as_deref().and_then(rfc3339_to_ms).is_some_and(|at| at <= now) {
        return Err(Problem::Expired);
    }
    let mut headers = vec![
        bearer(&auth.key),
        (GROK_TOKEN_AUTH_HEADER, GROK_TOKEN_AUTH.to_string()),
        (GROK_CLIENT_MODE_HEADER, GROK_CLIENT_MODE.to_string()),
    ];
    if let Some(user) = auth.user_id.filter(|user| !user.is_empty()) {
        headers.push((GROK_USER_HEADER, user));
    }
    grok_limits(&get(url, &headers)?)
}

pub fn grok_limits(body: &[u8]) -> Result<Limits, Problem> {
    let value: Value = parse(body)?;
    let config = value.get("config").unwrap_or(&value);
    let Some(used_percent) = number(config.get("creditUsagePercent")) else { return Ok(Limits::default()) };
    let period = config.get("currentPeriod");
    let resets_at_ms = period.and_then(|p| text(p, &["end"])).and_then(rfc3339_to_ms);
    let monthly = period
        .and_then(|p| text(p, &["type"]))
        .is_some_and(|kind| kind.to_ascii_uppercase().contains(GROK_MONTHLY_PERIOD));
    let window = Some(Window { used_percent, resets_at_ms });
    Ok(if monthly {
        Limits { monthly: window, ..Limits::default() }
    } else {
        Limits { weekly: window, ..Limits::default() }
    })
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct KimiCredentials {
    access_token: String,
    expires_at: Option<f64>,
}

fn toml_text(value: &str) -> Option<String> {
    let value = value.trim();
    let quoted = value.strip_prefix('"').and_then(|v| v.strip_suffix('"'));
    quoted.or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\''))).map(str::to_string)
}

fn toml_field(line: &str, name: &str) -> Option<String> {
    let body = line.trim().trim_start_matches('{').trim_end_matches('}');
    body.split(',').find_map(|part| {
        let (key, value) = part.split_once('=')?;
        (key.trim() == name).then(|| toml_text(value)).flatten()
    })
}

pub fn kimi_provider(config: &str) -> (Option<String>, Option<String>) {
    let (mut inside, mut base, mut credentials) = (false, None, None);
    for line in config.lines().map(str::trim) {
        if line.starts_with('[') {
            inside = line.replace(' ', "").starts_with(KIMI_PROVIDER_SECTION);
            continue;
        }
        if !inside {
            continue;
        }
        if let Some(url) = toml_field(line, "base_url") {
            base = Some(url);
        }
        let oauth = line.split_once('=').map_or(line, |(_, value)| value);
        if let Some(key) = toml_field(oauth, "key").or_else(|| toml_field(line, "key")) {
            let name = key.strip_prefix(KIMI_OAUTH_PREFIX).unwrap_or(&key).to_string();
            let safe = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            if safe {
                credentials = Some(name);
            }
        }
    }
    (base, credentials)
}

pub fn fetch_kimi(kimi_home: Option<&Path>, default_base: &str, now: u64) -> Result<Limits, Problem> {
    let home = kimi_home.ok_or(Problem::SignedOut)?;
    let (base, credentials) = kimi_provider(&fs::read_to_string(home.join(KIMI_CONFIG_FILE)).unwrap_or_default());
    let base = base.filter(|url| url.starts_with("https://")).unwrap_or_else(|| default_base.to_string());
    let name = credentials.unwrap_or_else(|| KIMI_DEFAULT_CREDENTIALS.to_string());
    let file = home.join(KIMI_CREDENTIALS_DIR).join(format!("{name}.json"));
    let bytes = fs::read(file).map_err(|_| Problem::SignedOut)?;
    let credentials: KimiCredentials = parse(&bytes)?;
    let expired = credentials.expires_at.is_some_and(|at| at * 1_000.0 <= now as f64);
    if credentials.access_token.is_empty() || expired {
        return Err(Problem::Expired);
    }
    let url = format!("{}{KIMI_USAGE_PATH}", base.trim_end_matches('/'));
    kimi_limits(&get(&url, &[bearer(&credentials.access_token)])?)
}

fn kimi_ratio(usages: &Value, keys: &[&str]) -> Option<Window> {
    let entry = keys.iter().find_map(|key| usages.get(key))?;
    let ratio = number(entry.get("used_ratio").or_else(|| entry.get("usedRatio")))?;
    let resets_at_ms = text(entry, &["reset_time", "resetAt"]).and_then(rfc3339_to_ms);
    Some(Window { used_percent: ratio * 100.0, resets_at_ms })
}

fn kimi_detail(detail: &Value) -> Option<Window> {
    let limit = number(detail.get("limit")).filter(|limit| *limit > 0.0)?;
    let used =
        number(detail.get("used")).or_else(|| number(detail.get("remaining")).map(|remaining| limit - remaining))?;
    let resets_at_ms = text(detail, &["resetTime", "reset_time"]).and_then(rfc3339_to_ms);
    Some(Window { used_percent: used / limit * 100.0, resets_at_ms })
}

fn kimi_minutes(window: Option<&Value>) -> Option<f64> {
    let window = window?;
    let duration = number(window.get("duration"))?;
    match text(window, &["timeUnit", "time_unit"])? {
        "TIME_UNIT_MINUTE" => Some(duration),
        "TIME_UNIT_HOUR" => Some(duration * MINUTES_PER_HOUR),
        "TIME_UNIT_DAY" => Some(duration * MINUTES_PER_DAY),
        _ => None,
    }
}

pub fn kimi_limits(body: &[u8]) -> Result<Limits, Problem> {
    let value: Value = parse(body)?;
    if let Some(usages) = value.get("usages").filter(|usages| usages.is_object()) {
        return Ok(Limits {
            session: kimi_ratio(usages, &["limit_5h", "limit5h"]),
            weekly: kimi_ratio(usages, &["limit_7d", "limit7d"]),
            monthly: kimi_ratio(usages, &["limit_month_total", "limitMonthTotal"]),
            credits: None,
        });
    }
    let mut limits = Limits { weekly: value.get("usage").and_then(kimi_detail), ..Limits::default() };
    for item in value.get("limits").and_then(Value::as_array).into_iter().flatten() {
        let Some(window) = item.get("detail").and_then(kimi_detail) else { continue };
        let minutes = kimi_minutes(item.get("window"));
        let slot =
            if minutes.is_some_and(|m| m <= KIMI_SESSION_MINUTES) { &mut limits.session } else { &mut limits.weekly };
        slot.get_or_insert(window);
    }
    Ok(limits)
}

fn antigravity_uses_api_key(agy_home: &Path) -> bool {
    let settings: Value = fs::read(agy_home.join(ANTIGRAVITY_SETTINGS_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    text(&settings, &["modelProvider"]) == Some(AGY_API_KEY_PROVIDER)
}

pub fn fetch_antigravity(program: &str, agy_home: Option<&Path>) -> Result<Limits, Problem> {
    if agy_home.is_some_and(antigravity_uses_api_key) {
        return Err(Problem::ApiKey);
    }
    let program = launch::resolve_program(program).map_err(|_| Problem::SignedOut)?;
    let args = AGY_PROBE_ARGS.map(str::to_string).to_vec();
    let (program, args) = launch::wrap_for_platform(program, args).map_err(|_| Problem::Unavailable)?;
    let mut command = Command::new(program);
    command.args(args).env(AGY_NO_UPDATE_ENV, "true");
    if let Some(dir) = user_home().filter(|dir| dir.is_dir()) {
        command.current_dir(dir);
    }
    antigravity_limits(&launch::command_output(&mut command, AGY_PROBE_TIMEOUT).ok_or(Problem::Unavailable)?)
}

fn collect_quota(value: &Value, name: &str, limits: &mut Limits) {
    match value {
        Value::Object(map) => {
            if let Some(remaining) = number(map.get("remaining_fraction").or_else(|| map.get("remainingFraction"))) {
                let mut label = name.to_lowercase();
                for key in AGY_BUCKET_NAMES {
                    if let Some(part) = map.get(key).and_then(Value::as_str) {
                        label.push(' ');
                        label.push_str(&part.to_lowercase());
                    }
                }
                let slot = if label.contains("week") {
                    &mut limits.weekly
                } else if label.contains("month") {
                    &mut limits.monthly
                } else if SESSION_HINTS.iter().any(|hint| label.contains(hint)) {
                    &mut limits.session
                } else {
                    return;
                };
                let resets_at_ms = text(value, &["reset_time", "resetTime"]).and_then(rfc3339_to_ms);
                let window = Window { used_percent: ((1.0 - remaining) * 100.0).clamp(0.0, 100.0), resets_at_ms };
                if slot.is_none_or(|current| window.used_percent > current.used_percent) {
                    *slot = Some(window);
                }
                return;
            }
            for (key, child) in map {
                collect_quota(child, key, limits);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_quota(item, name, limits);
            }
        }
        _ => {}
    }
}

pub fn antigravity_limits(output: &str) -> Result<Limits, Problem> {
    let values: Vec<Value> = serde_json::from_str(output.trim())
        .map(|value| vec![value])
        .unwrap_or_else(|_| output.lines().filter_map(|line| serde_json::from_str(line).ok()).collect());
    let mut limits = Limits::default();
    for value in &values {
        collect_quota(value, "", &mut limits);
    }
    if limits.session.is_some() || limits.weekly.is_some() || limits.monthly.is_some() {
        return Ok(limits);
    }
    let lower = output.to_lowercase();
    Err(if SIGNED_OUT_HINTS.iter().any(|hint| lower.contains(hint)) {
        Problem::SignedOut
    } else {
        Problem::Unavailable
    })
}

#[cfg(target_os = "macos")]
fn keychain_credentials() -> Option<Vec<u8>> {
    let output = std::process::Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", CLAUDE_KEYCHAIN_SERVICE, "-w"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

#[cfg(not(target_os = "macos"))]
fn keychain_credentials() -> Option<Vec<u8>> {
    None
}

pub fn fetch_claude(claude_home: Option<&Path>, url: &str, now: u64) -> Result<Limits, Problem> {
    let bytes = claude_home
        .and_then(|home| fs::read(home.join(CLAUDE_CREDENTIALS_FILE)).ok())
        .or_else(keychain_credentials)
        .ok_or(Problem::SignedOut)?;
    let credentials: ClaudeCredentials = parse(&bytes)?;
    let oauth = credentials.claude_ai_oauth.filter(|o| !o.access_token.is_empty()).ok_or(Problem::SignedOut)?;
    if oauth.expires_at.is_some_and(|at| at <= now) {
        return Err(Problem::Expired);
    }
    let headers = [
        ("Authorization", format!("Bearer {}", oauth.access_token)),
        ("anthropic-beta", CLAUDE_OAUTH_BETA.to_string()),
    ];
    claude_limits(&get(url, &headers)?)
}

pub fn fetch_codex(codex_home: Option<&Path>, url: &str) -> Result<Limits, Problem> {
    let bytes = codex_home.and_then(|home| fs::read(home.join(CODEX_AUTH_FILE)).ok()).ok_or(Problem::SignedOut)?;
    let auth: CodexAuth = parse(&bytes)?;
    let Some(tokens) = auth.tokens.filter(|t| !t.access_token.is_empty()) else {
        let api_key = auth.openai_api_key.is_some_and(|key| !key.trim().is_empty());
        return Err(if api_key { Problem::ApiKey } else { Problem::SignedOut });
    };
    let mut headers = vec![("Authorization", format!("Bearer {}", tokens.access_token))];
    if let Some(account) = tokens.account_id.filter(|a| !a.is_empty()) {
        headers.push(("ChatGPT-Account-Id", account));
    }
    codex_limits(&get(url, &headers)?)
}

fn claude_window(window: Option<ClaudeWindow>) -> Option<Window> {
    let window = window?;
    Some(Window {
        used_percent: window.utilization?,
        resets_at_ms: window.resets_at.as_deref().and_then(rfc3339_to_ms),
    })
}

pub fn claude_limits(body: &[u8]) -> Result<Limits, Problem> {
    let usage: ClaudeUsage = parse(body)?;
    Ok(Limits { session: claude_window(usage.five_hour), weekly: claude_window(usage.seven_day), ..Limits::default() })
}

pub fn codex_limits(body: &[u8]) -> Result<Limits, Problem> {
    let usage: CodexUsage = parse(body)?;
    let mut limits = Limits::default();
    let Some(rate_limit) = usage.rate_limit else { return Ok(limits) };
    for (window, primary) in [(rate_limit.primary_window, true), (rate_limit.secondary_window, false)] {
        let Some(window) = window else { continue };
        let session = match window.limit_window_seconds {
            seconds if seconds > 0 => seconds <= SESSION_WINDOW_MAX_SECS,
            _ => primary,
        };
        let slot = if session { &mut limits.session } else { &mut limits.weekly };
        if slot.is_none() {
            *slot = Some(Window {
                used_percent: window.used_percent,
                resets_at_ms: u64::try_from(window.reset_at).ok().filter(|at| *at > 0).map(|at| at * 1_000),
            });
        }
    }
    Ok(limits)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE_BODY: &str = r#"{"five_hour":{"utilization":55.0,"resets_at":"2026-10-16T00:00:00.278740+00:00","limit_dollars":null},"seven_day":{"utilization":8.0,"resets_at":"2026-10-17T06:00:00.278758+00:00"},"seven_day_opus":null,"extra_usage":{"is_enabled":false}}"#;

    fn window(used_percent: f64, resets_at_ms: Option<u64>) -> Option<Window> {
        Some(Window { used_percent, resets_at_ms })
    }

    #[test]
    fn claude_usage_reads_five_hour_and_weekly_windows() {
        let limits = claude_limits(CLAUDE_BODY.as_bytes()).unwrap();
        assert_eq!(limits.session, window(55.0, Some(1_792_108_800_278)));
        assert_eq!(limits.weekly, window(8.0, Some(1_792_216_800_278)));
    }

    #[test]
    fn claude_windows_without_utilization_are_missing() {
        let limits = claude_limits(br#"{"five_hour":null,"seven_day":{"utilization":null,"resets_at":null}}"#).unwrap();
        assert_eq!(limits, Limits::default());
        let limits = claude_limits(br#"{"five_hour":{"utilization":0.0,"resets_at":null}}"#).unwrap();
        assert_eq!(limits.session, window(0.0, None));
        assert_eq!(claude_limits(b"<html>"), Err(Problem::Unavailable));
    }

    #[test]
    fn codex_windows_are_classified_by_their_length() {
        let body = br#"{"plan_type":"plus","rate_limit":{"allowed":true,"limit_reached":false,
            "primary_window":{"used_percent":12,"limit_window_seconds":18000,"reset_after_seconds":60,"reset_at":1792108800},
            "secondary_window":{"used_percent":40,"limit_window_seconds":604800,"reset_after_seconds":60,"reset_at":1792216800}}}"#;
        let limits = codex_limits(body).unwrap();
        assert_eq!(limits.session, window(12.0, Some(1_792_108_800_000)));
        assert_eq!(limits.weekly, window(40.0, Some(1_792_216_800_000)));

        let weekly_only = br#"{"rate_limit":{"primary_window":{"used_percent":7,"limit_window_seconds":604800,"reset_at":0},"secondary_window":null}}"#;
        let limits = codex_limits(weekly_only).unwrap();
        assert_eq!(limits.session, None);
        assert_eq!(limits.weekly, window(7.0, None));
    }

    #[test]
    fn codex_windows_without_length_fall_back_to_their_position() {
        let body = br#"{"rate_limit":{"primary_window":{"used_percent":1,"reset_at":1792108800},"secondary_window":{"used_percent":2,"reset_at":1792216800}}}"#;
        let limits = codex_limits(body).unwrap();
        assert_eq!(limits.session.unwrap().used_percent, 1.0);
        assert_eq!(limits.weekly.unwrap().used_percent, 2.0);
        assert_eq!(codex_limits(br#"{"plan_type":"free","rate_limit":null}"#).unwrap(), Limits::default());
    }

    #[test]
    fn transient_failures_keep_the_last_limits_and_others_clear_them() {
        let limits = Limits { session: window(10.0, None), ..Limits::default() };
        let mut usage = Usage::default();
        usage.apply(Ok(limits), 1_000);
        assert_eq!(usage, Usage { limits: Some(limits), problem: None, updated_at_ms: Some(1_000) });
        usage.apply(Err(Problem::Unavailable), 2_000);
        assert_eq!(
            usage,
            Usage { limits: Some(limits), problem: Some(Problem::Unavailable), updated_at_ms: Some(1_000) }
        );
        usage.apply(Ok(limits), 3_000);
        assert_eq!(usage.problem, None);
        usage.apply(Err(Problem::Expired), 4_000);
        assert_eq!(usage.limits, None);
        assert_eq!(usage.problem, Some(Problem::Expired));
    }

    #[test]
    fn terminals_and_qwen_have_no_usage() {
        assert_eq!(fetch(AgentKind::Terminal, &Config::default()), Err(Problem::Unavailable));
        assert_eq!(fetch(AgentKind::Qwen, &Config::default()), Err(Problem::Unavailable));
        assert!(!PROVIDERS.contains(&AgentKind::Terminal));
        assert!(!PROVIDERS.contains(&AgentKind::Qwen));
    }

    #[test]
    fn opencode_go_reports_its_rolling_weekly_and_monthly_windows() {
        let body = br#"{"usage":{"rolling":{"status":"ok","percent":34,"resetsAt":"2026-10-16T00:00:00.000Z"},"weekly":{"status":"rate-limited","percent":100,"resetsAt":"2026-10-17T06:00:00Z"},"monthly":{"status":"ok","percent":5}}}"#;
        let limits = opencode_limits(body).unwrap();
        assert_eq!(limits.session, window(34.0, Some(1_792_108_800_000)));
        assert_eq!(limits.weekly, window(100.0, Some(1_792_216_800_000)));
        assert_eq!(limits.monthly, window(5.0, None));
        assert_eq!(opencode_limits(br#"{"usage":{}}"#).unwrap(), Limits::default());
        assert_eq!(opencode_limits(b"nope"), Err(Problem::Unavailable));
    }

    #[test]
    fn kilo_reports_its_balance_and_an_active_kilo_pass() {
        assert_eq!(kilo_balance(br#"{"balance":12.34}"#), Ok(12.34));
        assert_eq!(kilo_balance(br#"{"other":1}"#), Err(Problem::Unavailable));
        let pass = br#"[{"result":{"data":{"json":{"subscription":{"status":"active","currentPeriodBaseCreditsUsd":19,"currentPeriodUsageUsd":4.75,"nextBillingAt":"2026-10-16T00:00:00Z"}}}}}]"#;
        assert_eq!(kilo_pass(pass), window(25.0, Some(1_792_108_800_000)));
        let canceled = br#"[{"result":{"data":{"json":{"subscription":{"status":"canceled","currentPeriodBaseCreditsUsd":19}}}}}]"#;
        assert_eq!(kilo_pass(canceled), None);
        assert_eq!(kilo_pass(br#"[{"result":{"data":{"json":{"subscription":null}}}}]"#), None);
    }

    #[test]
    fn grok_reports_its_weekly_or_monthly_credit_period() {
        let weekly = br#"{"config":{"creditUsagePercent":42.5,"currentPeriod":{"type":"PERIOD_TYPE_WEEKLY","start":"2026-10-09T00:00:00Z","end":"2026-10-16T00:00:00Z"},"used":{}}}"#;
        let limits = grok_limits(weekly).unwrap();
        assert_eq!(limits.weekly, window(42.5, Some(1_792_108_800_000)));
        assert_eq!(limits.monthly, None);
        let monthly = br#"{"config":{"creditUsagePercent":"7","currentPeriod":{"type":"MONTHLY"}}}"#;
        assert_eq!(grok_limits(monthly).unwrap().monthly, window(7.0, None));
        assert_eq!(grok_limits(br#"{"config":{}}"#).unwrap(), Limits::default());
    }

    #[test]
    fn kimi_reads_the_quota_payload_and_the_legacy_one() {
        let current = br#"{"goods_version":2,"usages":{"limit_5h":{"used_ratio":0.3,"reset_time":"2026-10-16T00:00:00Z"},"limit_7d":{"used_ratio":"0.5"},"limit_month_total":{"used_ratio":0.1},"limit_month_code":{"used_ratio":0.05}}}"#;
        let limits = kimi_limits(current).unwrap();
        assert_eq!(limits.session.map(|w| w.used_percent.round()), Some(30.0));
        assert_eq!(limits.session.unwrap().resets_at_ms, Some(1_792_108_800_000));
        assert_eq!(limits.weekly.map(|w| w.used_percent), Some(50.0));
        assert_eq!(limits.monthly.map(|w| w.used_percent.round()), Some(10.0));
        let camel = br#"{"usages":{"limit5h":{"usedRatio":1,"resetAt":"2026-10-16T00:00:00Z"}}}"#;
        assert_eq!(kimi_limits(camel).unwrap().session, window(100.0, Some(1_792_108_800_000)));
        let legacy = br#"{"usage":{"used":"40","limit":"1000","resetTime":"2026-10-17T06:00:00Z"},"limits":[{"window":{"duration":300,"timeUnit":"TIME_UNIT_MINUTE"},"detail":{"used":"25","limit":"100","resetTime":"2026-10-16T00:00:00Z"}}]}"#;
        let limits = kimi_limits(legacy).unwrap();
        assert_eq!(limits.session, window(25.0, Some(1_792_108_800_000)));
        assert_eq!(limits.weekly, window(4.0, Some(1_792_216_800_000)));
        assert_eq!(
            kimi_limits(br#"{"limits":[{"detail":{"remaining":75,"limit":100}}]}"#).unwrap().weekly,
            window(25.0, None)
        );
    }

    #[test]
    fn kimi_finds_its_provider_in_config_toml() {
        let config = r#"default_model = "kimi-code/k3"

[providers."managed:kimi-code"]
type = "kimi"
base_url = "https://api.kimi.ai/coding/v1"
api_key = ""
oauth = { storage = "file", key = "oauth/kimi-code-env-0123456789abcdef" }

[providers."moonshot-ai"]
base_url = "https://api.moonshot.ai/v1"
"#;
        assert_eq!(
            kimi_provider(config),
            (Some("https://api.kimi.ai/coding/v1".into()), Some("kimi-code-env-0123456789abcdef".into()))
        );
        let table = "[providers.\"managed:kimi-code\"]\nbase_url = 'https://x'\n[providers.\"managed:kimi-code\".oauth]\nkey = \"oauth/kimi-code\"\n";
        assert_eq!(kimi_provider(table), (Some("https://x".into()), Some("kimi-code".into())));
        let unsafe_key = "[providers.\"managed:kimi-code\"]\noauth = { key = \"oauth/../../secrets\" }\n";
        assert_eq!(kimi_provider(unsafe_key), (None, None));
        assert_eq!(kimi_provider(""), (None, None));
    }

    #[test]
    fn antigravity_quota_buckets_are_found_wherever_the_payload_puts_them() {
        let statusline = r#"{"quota":{"gemini-5h":{"remaining_fraction":0.25,"reset_time":"2026-10-16T00:00:00Z"},"gemini-weekly":{"remaining_fraction":0.9375,"reset_time":"2026-10-17T06:00:00Z","reset_in_seconds":5}}}"#;
        let limits = antigravity_limits(statusline).unwrap();
        assert_eq!(limits.session, window(75.0, Some(1_792_108_800_000)));
        assert_eq!(limits.weekly, window(6.25, Some(1_792_216_800_000)));
        let buckets = "{\"event\":\"start\"}\n{\"buckets\":[{\"bucketId\":\"pro-weekly\",\"remainingFraction\":0.5},{\"displayName\":\"Weekly limit\",\"remainingFraction\":0.2},{\"modelId\":\"gemini-3.5-flash\",\"remainingFraction\":0.1}]}\n";
        let limits = antigravity_limits(buckets).unwrap();
        assert_eq!(limits.weekly.map(|w| w.used_percent.round()), Some(80.0));
        assert_eq!(limits.session, None, "per-model daily buckets are not plan windows");
        assert_eq!(
            antigravity_limits(r#"{"error":"You are not signed in. Run agy to sign in."}"#),
            Err(Problem::SignedOut)
        );
        assert_eq!(antigravity_limits("garbage"), Err(Problem::Unavailable));
        assert_eq!(antigravity_limits(""), Err(Problem::Unavailable));
    }

    struct Dir(PathBuf);

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn dir_with(tag: &str, files: &[(&str, &str)]) -> Dir {
        let dir = std::env::temp_dir().join(format!("orkai-usage-unit-{tag}-{}-{}", std::process::id(), now_ms()));
        for (name, contents) in files {
            let path = dir.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
        fs::create_dir_all(&dir).unwrap();
        Dir(dir)
    }

    const UNREACHABLE: &str = "http://127.0.0.1:9";

    #[test]
    fn logins_are_told_apart_without_calling_any_api() {
        let zen = dir_with(
            "zen",
            &[(AUTH_FILE, r#"{"opencode":{"type":"api","key":"sk-zen"},"openrouter":{"type":"api","key":"x"}}"#)],
        );
        assert_eq!(fetch_opencode(Some(&zen.0), UNREACHABLE), Err(Problem::ApiKey));
        let other = dir_with("other", &[(AUTH_FILE, r#"{"openrouter":{"type":"api","key":"x"}}"#)]);
        assert_eq!(fetch_opencode(Some(&other.0), UNREACHABLE), Err(Problem::SignedOut));
        assert_eq!(fetch_opencode(None, UNREACHABLE), Err(Problem::SignedOut));

        let kilo_key = dir_with("kilo-key", &[(AUTH_FILE, r#"{"kilo":{"type":"api","key":"k"}}"#)]);
        assert_eq!(fetch_kilo(Some(&kilo_key.0), UNREACHABLE), Err(Problem::ApiKey));
        assert_eq!(fetch_kilo(Some(&other.0), UNREACHABLE), Err(Problem::SignedOut));

        let grok_key = dir_with(
            "grok-key",
            &[(
                AUTH_FILE,
                r#"{"xai::api_key":{"key":"xai-1","auth_mode":"api_key"},"https://accounts.x.ai/sign-in":{"key":"old"}}"#,
            )],
        );
        assert_eq!(fetch_grok(Some(&grok_key.0.join(AUTH_FILE)), UNREACHABLE, 0), Err(Problem::ApiKey));
        let grok_expired = dir_with(
            "grok-old",
            &[(
                AUTH_FILE,
                r#"{"https://auth.x.ai::b1a00492":{"key":"jwt","auth_mode":"oidc","expires_at":"2026-10-16T00:00:00Z"}}"#,
            )],
        );
        assert_eq!(
            fetch_grok(Some(&grok_expired.0.join(AUTH_FILE)), UNREACHABLE, 1_792_108_800_001),
            Err(Problem::Expired)
        );
        assert_eq!(fetch_grok(Some(&other.0.join("missing.json")), UNREACHABLE, 0), Err(Problem::SignedOut));

        let kimi_tombstone =
            dir_with("kimi", &[("credentials/kimi-code.json", r#"{"access_token":"","expires_at":0}"#)]);
        assert_eq!(fetch_kimi(Some(&kimi_tombstone.0), UNREACHABLE, 1), Err(Problem::Expired));
        let kimi_old =
            dir_with("kimi-old", &[("credentials/kimi-code.json", r#"{"access_token":"t","expires_at":1000.5}"#)]);
        assert_eq!(fetch_kimi(Some(&kimi_old.0), UNREACHABLE, 1_000_600), Err(Problem::Expired));
        assert_eq!(fetch_kimi(Some(&other.0), UNREACHABLE, 1), Err(Problem::SignedOut));

        let agy = dir_with("agy", &[(ANTIGRAVITY_SETTINGS_FILE, r#"{"modelProvider":"gemini"}"#)]);
        assert_eq!(fetch_antigravity("orkai-missing-agy", Some(&agy.0)), Err(Problem::ApiKey));
        assert_eq!(fetch_antigravity("orkai-missing-agy", Some(&other.0)), Err(Problem::SignedOut));
    }
}
