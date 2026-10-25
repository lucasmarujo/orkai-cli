use std::collections::{BTreeMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};

use crate::client::{self, Connection};
use crate::holder::oplog;
use crate::launch::{self, LaunchMode, Launcher};
use crate::model::{
    AgentId, AgentKind, AgentRecord, AgentState, DEFAULT_SESSION, ExitInfo, NestedAgent, NewAgent, Session,
    StatusReport, UiState, Worktree, validate_agent_name,
};
use crate::notify::Alert;
use crate::paths::{Home, expand_tilde, normalize_for_compare, separator_for, user_home};
use crate::protocol::{ClientMsg, HolderMsg, TermSize};
use crate::pwa::bridge::{self, Info};
use crate::pwa::qr::Qr;
use crate::pwa::tunnel::TunnelKind;
use crate::skills;
use crate::subagents::{Parent, Subagent, SubagentWatcher};
use crate::tui::complete::{PathCompleter, SkillCatalog, complete_command, where_argument};
use crate::tui::format::{default_agent_name, display_path, project_label};
use crate::tui::history::TaskHistory;
use crate::tui::keys;
use crate::tui::line::LineInput;
use crate::tui::render;
use crate::tui::view::TermView;
use crate::update;
use crate::usage::{self, Limits, Problem, Usage};
use crate::util::now_ms;
use crate::worktree::{self, ChangeWatcher, Changes, Merged};

const RESCAN_EVERY: Duration = Duration::from_secs(2);
const START_WAIT: Duration = Duration::from_secs(15);
const FRESH_RECORD_MS: u64 = 15_000;
const SCROLL_STEP: isize = 3;
const ALERT_REPEAT_MS: u64 = 15_000;
const MERGE_SUBJECT_MAX: usize = 60;
const PWA_CHECK_EVERY: Duration = Duration::from_secs(1);
const PWA_REVIVE_AFTER: Duration = Duration::from_secs(20);
const PWA_STOP_WAIT: Duration = Duration::from_secs(10);
pub const SHELL_PREFIX: char = '!';

pub enum AppEvent {
    Input(Event),
    Tick,
    Connected { id: AgentId, generation: u64, result: Result<Arc<Connection>, String> },
    Holder { id: AgentId, generation: u64, msg: Option<HolderMsg> },
    Launched { result: Result<AgentRecord, String> },
    Restarted { id: AgentId, result: Result<AgentRecord, String> },
    Replayed { id: AgentId, parser: Option<Box<vt100::Parser>> },
    Subagents(BTreeMap<AgentId, Vec<Subagent>>),
    Usage { kind: AgentKind, result: Result<Limits, Problem> },
    Changes(BTreeMap<AgentId, Changes>),
    Diff { id: AgentId, result: Result<Vec<u8>, String> },
    Merged { id: AgentId, result: Result<Merged, String> },
    Discarded { name: String, result: Result<(), String> },
    PwaStarted(Result<(), String>),
}

#[derive(Default)]
pub struct PwaPanel {
    pub enabled: bool,
    pub info: Option<Info>,
    pub code: Option<PairingCode>,
    pub starting: bool,
    checked: Option<Instant>,
    started: Option<Instant>,
}

pub struct PairingCode {
    pub pid: u32,
    pub url: String,
    pub link: String,
    pub qr: Option<Qr>,
}

pub enum Link {
    Connecting(u64),
    Live { generation: u64, conn: Arc<Connection> },
    Exited(ExitInfo),
    Lost,
}

pub struct AgentEntry {
    pub record: AgentRecord,
    pub link: Link,
    pub status: Option<StatusReport>,
    pub term: Option<TermView>,
    pub attached: bool,
    pub subagents: Vec<Subagent>,
    pub changes: Option<Changes>,
    failed_runtime: Option<u64>,
    last_alert: Option<(AgentState, u64)>,
}

impl AgentEntry {
    fn new(record: AgentRecord) -> Self {
        Self {
            record,
            link: Link::Lost,
            status: None,
            term: None,
            attached: false,
            subagents: Vec::new(),
            changes: None,
            failed_runtime: None,
            last_alert: None,
        }
    }

    pub fn state(&self) -> AgentState {
        match &self.link {
            Link::Exited(_) => AgentState::Exited,
            Link::Lost => AgentState::Lost,
            _ => self.status.as_ref().map_or(AgentState::Idle, |s| s.state),
        }
    }

    pub fn is_starting(&self) -> bool {
        matches!(self.link, Link::Connecting(_)) && self.status.is_none()
    }

    pub fn exit(&self) -> Option<&ExitInfo> {
        match &self.link {
            Link::Exited(info) => Some(info),
            _ => None,
        }
    }

    fn generation(&self) -> Option<u64> {
        match &self.link {
            Link::Connecting(generation) | Link::Live { generation, .. } => Some(*generation),
            _ => None,
        }
    }

    fn conn(&self) -> Option<&Arc<Connection>> {
        match &self.link {
            Link::Live { conn, .. } => Some(conn),
            _ => None,
        }
    }

    pub fn is_dead(&self) -> bool {
        matches!(self.link, Link::Exited(_) | Link::Lost)
    }

    pub fn nested(&self) -> Option<&NestedAgent> {
        self.status.as_ref().filter(|_| !self.is_dead()).and_then(|s| s.nested.as_ref())
    }

    pub fn kind(&self) -> AgentKind {
        self.nested().map_or(self.record.kind, |nested| nested.kind)
    }

    fn alert(&mut self, state: AgentState, now: u64) -> Option<Alert> {
        if !self.kind().is_ai() {
            return None;
        }
        let repeated =
            self.last_alert.is_some_and(|(last, at)| last == state && now.saturating_sub(at) < ALERT_REPEAT_MS);
        if repeated {
            return None;
        }
        self.last_alert = Some((state, now));
        Some(Alert {
            id: self.record.id,
            name: self.record.name.clone(),
            kind: self.kind(),
            state,
            session: self.record.session.clone(),
            cwd: self.record.cwd.clone(),
            activity: self.status.as_ref().map(|s| s.activity.clone()).unwrap_or_default(),
        })
    }

    pub fn subagent_parent(&self) -> Option<Parent> {
        if !self.kind().is_ai() || self.is_dead() {
            return None;
        }
        let nested = self.nested();
        Some(Parent {
            id: self.record.id,
            kind: self.kind(),
            conversation_id: nested.map_or(self.record.conversation_id.clone(), |n| n.conversation_id.clone()),
            cwd: nested.and_then(|n| n.cwd.clone()).unwrap_or_else(|| self.record.cwd.clone()),
            created_at_ms: nested.map_or(self.record.created_at_ms, |n| n.since_ms),
            busy: matches!(self.state(), AgentState::Working | AgentState::Paused),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Overview,
    Agent(AgentId),
    Pwa,
}

pub enum Prompt {
    Where(LineInput),
    Rename(AgentId, LineInput),
    NewSession(LineInput),
    RenameSession(LineInput),
    Confirm(Confirm),
}

pub enum Confirm {
    Kill(AgentId),
    Remove(AgentId),
    Delete(AgentId),
    DeleteSession(String),
    Recover(Vec<AgentId>),
    Merge(AgentId),
}

impl Confirm {
    pub fn is_popup(&self) -> bool {
        matches!(self, Confirm::Kill(_) | Confirm::Remove(_) | Confirm::Delete(_) | Confirm::Merge(_))
    }
}

pub struct DiffPane {
    pub id: AgentId,
    pub view: TermView,
    pub empty: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Info,
    Success,
    Warn,
    Error,
}

pub struct Notice {
    pub text: String,
    pub tone: Tone,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Hit {
    Session(String),
    NewSession,
    Agent(AgentId),
    Tab(AgentId),
    CloseTab(AgentId),
    Overview,
    Who,
    Where,
    Worktree,
    Bypass,
    Recover,
    Project(PathBuf),
    Terminal,
    /// A line of the task being written: its number, the columns hidden on its left and the column it starts at.
    Task {
        line: usize,
        skip: usize,
        x: u16,
    },
    Pwa,
    CopyPwaLink,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Attention {
    pub waiting: usize,
    pub done: usize,
}

impl Attention {
    pub fn total(&self) -> usize {
        self.waiting + self.done
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub agents: usize,
    pub working: usize,
    pub paused: usize,
    pub done: usize,
}

pub struct Group {
    pub label: String,
    pub cwd: PathBuf,
    pub ids: Vec<AgentId>,
}

pub struct App {
    pub launcher: Launcher,
    tx: Sender<AppEvent>,
    pub hostname: String,
    pub home_dir: Option<PathBuf>,
    pub sessions: Vec<Session>,
    pub session: String,
    pub agents: BTreeMap<AgentId, AgentEntry>,
    pub tabs: Vec<AgentId>,
    pub view: View,
    pub selected: Option<AgentId>,
    pub list_offset: usize,
    pub task: LineInput,
    pub who: AgentKind,
    pub where_dir: PathBuf,
    pub prompt: Option<Prompt>,
    pub notice: Option<Notice>,
    pub paths: PathCompleter,
    pub skills: SkillCatalog,
    pub show_help: bool,
    pub hits: Vec<(Rect, Hit)>,
    pub tick: usize,
    pub screen: Rect,
    pub should_quit: bool,
    pub launching: usize,
    pub usage: BTreeMap<AgentKind, Usage>,
    pub worktree: bool,
    pub bypass: bool,
    pub diff: Option<DiffPane>,
    history: TaskHistory,
    dirty: bool,
    next_generation: u64,
    last_rescan: Instant,
    pending_input: Vec<(AgentId, Vec<u8>)>,
    attached_size: Option<TermSize>,
    session_dirs: BTreeMap<String, PathBuf>,
    removing: HashSet<AgentId>,
    subagent_requests: Option<Sender<Vec<Parent>>>,
    change_requests: Option<Sender<Vec<(AgentId, Worktree)>>>,
    alerts: Vec<Alert>,
    clipboard: Option<String>,
    installed: Option<Vec<AgentKind>>,
    pub pwa: PwaPanel,
    pub language: crate::i18n::Language,
}

impl App {
    pub fn new(launcher: Launcher, tx: Sender<AppEvent>, screen: Rect) -> io::Result<Self> {
        let store = launcher.store().clone();
        let records = store.list_agents()?;
        let sessions = store.load_sessions_with(&records)?;
        let ui = store.load_ui();
        let session = if sessions.iter().any(|s| s.name == ui.session) {
            ui.session.clone()
        } else {
            DEFAULT_SESSION.to_string()
        };
        let where_dir = ui
            .session_dirs
            .get(&session)
            .or(ui.where_dir.as_ref())
            .cloned()
            .filter(|p| p.is_dir())
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let agents: BTreeMap<AgentId, AgentEntry> = records.into_iter().map(|r| (r.id, AgentEntry::new(r))).collect();
        let tabs = ui.tabs.iter().copied().filter(|id| agents.contains_key(id)).collect();
        launch::prune_holder_binaries(store.home(), launcher.holder_exe());
        let language = launcher.language();
        let mut app = Self {
            launcher,
            tx,
            hostname: gethostname::gethostname().to_string_lossy().to_lowercase(),
            home_dir: user_home(),
            sessions,
            session,
            agents,
            tabs,
            view: View::Overview,
            selected: None,
            list_offset: 0,
            task: LineInput::default(),
            who: ui.who,
            where_dir,
            prompt: None,
            notice: None,
            paths: PathCompleter::default(),
            skills: SkillCatalog::default(),
            show_help: false,
            hits: Vec::new(),
            tick: 0,
            screen,
            should_quit: false,
            launching: 0,
            usage: BTreeMap::new(),
            worktree: ui.worktree,
            bypass: ui.bypass,
            diff: None,
            history: TaskHistory::default(),
            dirty: true,
            next_generation: 1,
            last_rescan: Instant::now(),
            pending_input: Vec::new(),
            attached_size: None,
            session_dirs: ui.session_dirs,
            removing: HashSet::new(),
            subagent_requests: None,
            change_requests: None,
            alerts: Vec::new(),
            clipboard: None,
            installed: None,
            pwa: PwaPanel { enabled: ui.pwa, ..PwaPanel::default() },
            language,
        };
        app.ensure_selection();
        Ok(app)
    }

    pub fn t<'a>(&self, en: &'a str, pt: &'a str) -> &'a str {
        match self.language {
            crate::i18n::Language::PtBr => pt,
            crate::i18n::Language::En => en,
        }
    }

    pub fn start(&mut self) {
        let ids: Vec<AgentId> = self.agents.keys().copied().collect();
        for id in ids {
            self.refresh_link(id);
        }
        self.spawn_subagent_watcher();
        self.request_subagents();
        self.spawn_change_watcher();
        self.request_changes();
        self.spawn_usage_watchers();
        if self.pwa.enabled {
            self.start_pwa();
        }
        if let Ok(commit) = std::env::var(update::UPDATED_ENV) {
            self.set_notice(Tone::Success, format!("orkai updated to {}", update::short_commit(&commit)));
        }
        let lost = self.agents.values().filter(|e| matches!(e.link, Link::Lost)).count();
        if lost > 0 {
            self.set_notice(
                Tone::Warn,
                format!("{lost} agent(s) were interrupted (crash or reboot) — ctrl+r recovers this session, `orkai recover --all` recovers everything"),
            );
        }
    }

    fn spawn_subagent_watcher(&mut self) {
        let (requests, rx) = mpsc::channel::<Vec<Parent>>();
        let tx = self.tx.clone();
        let spawned = thread::Builder::new().name("orkai-subagents".into()).spawn(move || {
            let mut watcher = SubagentWatcher::new(launch::claude_home(), launch::codex_home());
            while let Ok(mut parents) = rx.recv() {
                while let Ok(latest) = rx.try_recv() {
                    parents = latest;
                }
                if tx.send(AppEvent::Subagents(watcher.scan(&parents, now_ms()))).is_err() {
                    return;
                }
            }
        });
        if spawned.is_ok() {
            self.subagent_requests = Some(requests);
        }
    }

    fn spawn_change_watcher(&mut self) {
        let (requests, rx) = mpsc::channel::<Vec<(AgentId, Worktree)>>();
        let tx = self.tx.clone();
        let spawned = thread::Builder::new().name("orkai-worktrees".into()).spawn(move || {
            let mut watcher = ChangeWatcher::default();
            while let Ok(mut targets) = rx.recv() {
                while let Ok(latest) = rx.try_recv() {
                    targets = latest;
                }
                let found = watcher.scan(&targets, now_ms());
                if !found.is_empty() && tx.send(AppEvent::Changes(found)).is_err() {
                    return;
                }
            }
        });
        if spawned.is_ok() {
            self.change_requests = Some(requests);
        }
    }

    fn request_changes(&self) {
        let Some(requests) = &self.change_requests else { return };
        let targets: Vec<(AgentId, Worktree)> = self
            .agents
            .values()
            .filter(|e| e.record.session == self.session)
            .filter_map(|e| e.record.worktree.clone().map(|worktree| (e.record.id, worktree)))
            .collect();
        if !targets.is_empty() {
            let _ = requests.send(targets);
        }
    }

    fn on_changes(&mut self, found: BTreeMap<AgentId, Changes>) {
        for (id, changes) in found {
            if let Some(entry) = self.agents.get_mut(&id)
                && entry.changes != Some(changes)
            {
                entry.changes = Some(changes);
                self.dirty = true;
            }
        }
    }

    fn spawn_usage_watchers(&self) {
        for kind in usage::PROVIDERS {
            let tx = self.tx.clone();
            let config = self.launcher.config().clone();
            let _ = thread::Builder::new().name("orkai-usage".into()).spawn(move || {
                loop {
                    if tx.send(AppEvent::Usage { kind, result: usage::fetch(kind, &config) }).is_err() {
                        return;
                    }
                    thread::sleep(usage::REFRESH_EVERY);
                }
            });
        }
    }

    fn on_usage(&mut self, kind: AgentKind, result: Result<Limits, Problem>) {
        self.usage.entry(kind).or_default().apply(result, now_ms());
        self.dirty = true;
    }

    fn request_subagents(&self) {
        let Some(requests) = &self.subagent_requests else { return };
        let parents = self.agents.values().filter_map(AgentEntry::subagent_parent).collect();
        let _ = requests.send(parents);
    }

    fn on_subagents(&mut self, mut found: BTreeMap<AgentId, Vec<Subagent>>) {
        for entry in self.agents.values_mut() {
            let subagents =
                if entry.is_dead() { Vec::new() } else { found.remove(&entry.record.id).unwrap_or_default() };
            if entry.subagents != subagents {
                entry.subagents = subagents;
                self.dirty = true;
            }
        }
    }

    pub fn shutdown(&mut self) {
        if let View::Agent(id) = self.view {
            self.detach(id);
        }
        self.flush_input();
        self.persist_ui();
    }

    pub fn take_dirty(&mut self) -> bool {
        std::mem::replace(&mut self.dirty, false)
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    fn set_notice(&mut self, tone: Tone, text: impl Into<String>) {
        self.notice = Some(Notice { text: text.into(), tone });
        self.dirty = true;
    }

    fn persist_ui(&self) {
        let pwa = self.launcher.store().pwa_enabled().unwrap_or(self.pwa.enabled);
        let _ = self.save_ui(pwa);
    }

    fn save_ui(&self, pwa: bool) -> io::Result<()> {
        let state = UiState {
            session: self.session.clone(),
            who: self.who,
            where_dir: Some(self.where_dir.clone()),
            tabs: self.tabs.clone(),
            session_dirs: self.session_dirs.clone(),
            worktree: self.worktree,
            bypass: self.bypass,
            pwa,
        };
        self.launcher.store().save_ui(&state)
    }

    fn next_gen(&mut self) -> u64 {
        self.next_generation += 1;
        self.next_generation
    }

    pub fn display(&self, path: &std::path::Path) -> String {
        display_path(path, self.home_dir.as_deref())
    }

    pub fn groups(&self) -> Vec<Group> {
        let mut groups: Vec<Group> = Vec::new();
        for entry in self.agents.values().filter(|e| e.record.session == self.session) {
            let project = entry.record.project_dir();
            let key = normalize_for_compare(project);
            match groups.iter_mut().find(|g| normalize_for_compare(&g.cwd) == key) {
                Some(group) => group.ids.push(entry.record.id),
                None => groups.push(Group {
                    label: project_label(project),
                    cwd: project.to_path_buf(),
                    ids: vec![entry.record.id],
                }),
            }
        }
        groups
    }

    pub fn ordered_ids(&self) -> Vec<AgentId> {
        self.groups().into_iter().flat_map(|g| g.ids).collect()
    }

    pub fn stats(&self) -> Stats {
        let mut stats = Stats::default();
        for entry in self.agents.values().filter(|e| e.record.session == self.session && e.kind().is_ai()) {
            match entry.state() {
                AgentState::Exited => continue,
                AgentState::Working => stats.working += 1,
                AgentState::Paused => stats.paused += 1,
                AgentState::Done => stats.done += 1,
                _ => {}
            }
            stats.agents += 1;
        }
        stats
    }

    pub fn waiting_for_you(&self) -> Vec<AgentId> {
        let mut waiting: Vec<(bool, u64, AgentId)> = self
            .agents
            .values()
            .filter(|e| e.kind().is_ai() && matches!(e.state(), AgentState::Paused | AgentState::Done))
            .map(|e| {
                let since = e.status.as_ref().map_or(0, |s| s.state_since_ms);
                (e.state() != AgentState::Paused, since, e.record.id)
            })
            .collect();
        waiting.sort_unstable();
        waiting.into_iter().map(|(_, _, id)| id).collect()
    }

    pub fn attention(&self, except: Option<AgentId>) -> Attention {
        let mut attention = Attention::default();
        for id in self.waiting_for_you().into_iter().filter(|id| Some(*id) != except) {
            match self.agents[&id].state() {
                AgentState::Paused => attention.waiting += 1,
                _ => attention.done += 1,
            }
        }
        attention
    }

    pub fn window_title(&self) -> String {
        let attention = self.attention(None);
        let mut parts = Vec::new();
        if attention.waiting > 0 {
            parts.push(format!("‖{}", attention.waiting));
        }
        if attention.done > 0 {
            parts.push(format!("✔{}", attention.done));
        }
        if parts.is_empty() {
            crate::tui::WINDOW_TITLE.to_string()
        } else {
            format!("{} · {}", parts.join(" "), crate::tui::WINDOW_TITLE)
        }
    }

    pub fn take_alerts(&mut self) -> Vec<Alert> {
        std::mem::take(&mut self.alerts)
    }

    pub fn take_clipboard(&mut self) -> Option<String> {
        self.clipboard.take()
    }

    pub fn session_count(&self, name: &str) -> usize {
        self.agents.values().filter(|e| e.record.session == name && e.state() != AgentState::Exited).count()
    }

    pub fn lost_in_session(&self) -> Vec<AgentId> {
        self.agents
            .values()
            .filter(|e| e.record.session == self.session && matches!(e.link, Link::Lost))
            .map(|e| e.record.id)
            .collect()
    }

    pub fn any_working(&self) -> bool {
        self.agents.values().any(|e| e.record.session == self.session && e.state() == AgentState::Working)
    }

    fn ensure_selection(&mut self) {
        let ids = self.ordered_ids();
        if self.selected.is_none_or(|id| !ids.contains(&id)) {
            self.selected = ids.first().copied();
        }
    }

    pub fn agent_term_size(&self) -> TermSize {
        let areas = render::areas(self.screen, true, true, 1);
        TermSize::new(areas.body.height, areas.body.width)
    }

    fn refresh_link(&mut self, id: AgentId) {
        let store = self.launcher.store().clone();
        let Some(entry) = self.agents.get_mut(&id) else { return };
        if let Ok(Some(exit)) = store.read_exit(id) {
            entry.link = Link::Exited(exit);
            self.dirty = true;
            return;
        }
        match store.read_runtime(id) {
            Ok(Some(runtime)) if entry.failed_runtime != Some(runtime.started_at_ms) => {
                self.connect_agent(id, Duration::ZERO);
            }
            Ok(None) if now_ms().saturating_sub(entry.record.created_at_ms) < FRESH_RECORD_MS => {
                self.connect_agent(id, START_WAIT);
            }
            _ => {
                entry.link = Link::Lost;
                self.dirty = true;
            }
        }
    }

    fn connect_agent(&mut self, id: AgentId, wait: Duration) {
        let generation = self.next_gen();
        let Some(entry) = self.agents.get_mut(&id) else { return };
        entry.link = Link::Connecting(generation);
        entry.attached = false;
        self.dirty = true;
        let store = self.launcher.store().clone();
        let tx = self.tx.clone();
        let spawned = thread::Builder::new().name("orkai-connect".into()).spawn(move || {
            let runtime = if wait.is_zero() {
                store
                    .read_runtime(id)
                    .and_then(|r| r.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "agent is not running")))
            } else {
                launch::wait_for_runtime(&store, id, wait)
            };
            match runtime.and_then(|rt| client::connect(&rt)) {
                Ok((conn, reader, _)) => {
                    let conn = Arc::new(conn);
                    if tx.send(AppEvent::Connected { id, generation, result: Ok(conn) }).is_err() {
                        return;
                    }
                    let reader_tx = tx.clone();
                    let _ = client::spawn_reader(reader, move |msg| {
                        reader_tx.send(AppEvent::Holder { id, generation, msg }).is_ok()
                    });
                }
                Err(err) => {
                    let _ = tx.send(AppEvent::Connected { id, generation, result: Err(err.to_string()) });
                }
            }
        });
        if spawned.is_err()
            && let Some(entry) = self.agents.get_mut(&id)
        {
            entry.link = Link::Lost;
        }
    }

    pub fn handle(&mut self, event: AppEvent) {
        match event {
            AppEvent::Input(event) => self.on_input(event),
            AppEvent::Tick => self.on_tick(),
            AppEvent::Connected { id, generation, result } => self.on_connected(id, generation, result),
            AppEvent::Holder { id, generation, msg } => self.on_holder(id, generation, msg),
            AppEvent::Launched { result } => self.on_launched(result),
            AppEvent::Restarted { id, result } => self.on_restarted(id, result),
            AppEvent::Subagents(found) => self.on_subagents(found),
            AppEvent::Usage { kind, result } => self.on_usage(kind, result),
            AppEvent::Changes(found) => self.on_changes(found),
            AppEvent::Diff { id, result } => self.on_diff(id, result),
            AppEvent::Merged { id, result } => self.on_merged(id, result),
            AppEvent::Discarded { name, result } => match result {
                Ok(()) => self.set_notice(Tone::Info, format!("{name} removed with its worktree and branch")),
                Err(err) => self.set_notice(Tone::Error, format!("could not delete the worktree of {name}: {err}")),
            },
            AppEvent::Replayed { id, parser } => {
                if let Some(entry) = self.agents.get_mut(&id)
                    && entry.is_dead()
                    && entry.term.is_none()
                {
                    entry.term = Some(match parser {
                        Some(parser) => TermView::from_parser(*parser),
                        None => TermView::from_snapshot(TermSize::new(3, 40), b"(no output was recorded)"),
                    });
                    self.dirty = true;
                }
            }
            AppEvent::PwaStarted(result) => {
                self.pwa.starting = false;
                if let Err(err) = result {
                    self.set_notice(Tone::Error, format!("the orkai pwa did not start: {err}"));
                }
                self.refresh_pwa();
            }
        }
    }

    pub fn open_pwa(&mut self) {
        self.show_overview();
        self.view = View::Pwa;
        self.prompt = None;
        if !self.pwa.enabled {
            self.enable_pwa();
        }
        self.refresh_pwa();
        self.dirty = true;
    }

    fn enable_pwa(&mut self) {
        self.pwa.enabled = true;
        if let Err(err) = self.save_ui(true) {
            self.set_notice(Tone::Error, format!("could not remember that the orkai pwa is on: {err}"));
        }
        self.start_pwa();
    }

    fn pwa_target(&self) -> (Home, PathBuf, TunnelKind, u16) {
        let config = self.launcher.config();
        (
            self.launcher.store().home().clone(),
            self.launcher.holder_exe().to_path_buf(),
            config.pwa_tunnel,
            config.pwa_port,
        )
    }

    fn start_pwa(&mut self) {
        if self.pwa.starting {
            return;
        }
        self.pwa.starting = true;
        self.pwa.started = Some(Instant::now());
        let (home, exe, tunnel, port) = self.pwa_target();
        let tx = self.tx.clone();
        let _ = thread::Builder::new().name("orkai-pwa-start".into()).spawn(move || {
            let result = bridge::start(&home, &exe, tunnel, port).map(|_| ()).map_err(|err| err.to_string());
            let _ = tx.send(AppEvent::PwaStarted(result));
        });
    }

    fn disable_pwa(&mut self) {
        self.pwa.enabled = false;
        match self.save_ui(false).and_then(|()| bridge::stop(self.launcher.store().home())) {
            Ok(()) => self.set_notice(Tone::Info, "the orkai pwa is off · ctrl+p turns it on again"),
            Err(err) => self.set_notice(Tone::Error, format!("could not stop the orkai pwa: {err}")),
        }
        self.pwa.info = None;
        self.pwa.code = None;
        self.dirty = true;
    }

    fn copy_pwa_link(&mut self) {
        let Some(code) = self.pwa.code.as_ref() else { return };
        self.clipboard = Some(code.link.clone());
        self.set_notice(Tone::Success, "copied the pairing link · paste it in the phone's browser");
        self.dirty = true;
    }

    fn renew_pwa_code(&mut self) {
        if self.pwa.starting {
            return;
        }
        self.pwa.starting = true;
        self.pwa.started = Some(Instant::now());
        self.pwa.info = None;
        self.pwa.code = None;
        self.set_notice(Tone::Info, "forgetting the paired phones…");
        let (home, exe, tunnel, port) = self.pwa_target();
        let tx = self.tx.clone();
        let _ = thread::Builder::new().name("orkai-pwa-renew".into()).spawn(move || {
            let result = bridge::stop(&home)
                .and_then(|()| {
                    if bridge::wait_until(&home, PWA_STOP_WAIT, |info| info.is_none()) {
                        Ok(())
                    } else {
                        Err(io::Error::new(io::ErrorKind::TimedOut, "the old bridge did not stop"))
                    }
                })
                .and_then(|()| bridge::forget_phones(&home))
                .and_then(|()| bridge::start(&home, &exe, tunnel, port))
                .map(|_| ())
                .map_err(|err| err.to_string());
            let _ = tx.send(AppEvent::PwaStarted(result));
        });
    }

    fn refresh_pwa(&mut self) {
        self.pwa.checked = Some(Instant::now());
        if let Some(enabled) = self.launcher.store().pwa_enabled()
            && enabled != self.pwa.enabled
        {
            self.pwa.enabled = enabled;
            self.dirty = true;
        }
        let home = self.launcher.store().home().clone();
        let info = bridge::status(&home);
        if self.pwa.enabled
            && info.is_none()
            && !self.pwa.starting
            && self.pwa.started.is_none_or(|at| at.elapsed() >= PWA_REVIVE_AFTER)
        {
            self.start_pwa();
        }
        let ready = info.as_ref().and_then(|info| info.ready_url().map(|url| (info.pid, url.to_string())));
        match ready {
            None => self.pwa.code = None,
            Some((pid, url)) => {
                let stale = self.pwa.code.as_ref().is_none_or(|code| code.pid != pid || code.url != url);
                if stale && let Ok(secret) = bridge::load_secret(&home) {
                    let link = bridge::pairing_link(&url, &secret);
                    self.pwa.code = Some(PairingCode { pid, qr: Qr::encode(&link), url, link });
                    self.dirty = true;
                }
            }
        }
        if self.pwa.info != info {
            self.pwa.info = info;
            self.dirty = true;
        }
    }

    fn on_pwa_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => self.should_quit = true,
            KeyCode::Esc | KeyCode::Char('q') => self.show_overview(),
            KeyCode::Char('d') if self.pwa.enabled => self.disable_pwa(),
            KeyCode::Char('n') if self.pwa.enabled => self.renew_pwa_code(),
            KeyCode::Char('c') => self.copy_pwa_link(),
            KeyCode::Enter | KeyCode::Char('e') if !self.pwa.enabled => self.enable_pwa(),
            _ if keys::is_next_waiting_key(&key) => self.open_next_waiting(),
            _ => {}
        }
        self.dirty = true;
    }

    fn on_tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        let pwa_waiting = self.view == View::Pwa && self.pwa.enabled && self.pwa.code.is_none();
        if self.any_working() || pwa_waiting || self.tick.is_multiple_of(8) {
            self.dirty = true;
        }
        if self.last_rescan.elapsed() >= RESCAN_EVERY {
            self.last_rescan = Instant::now();
            self.rescan();
        }
        if self.pwa.checked.is_none_or(|at| at.elapsed() >= PWA_CHECK_EVERY) {
            self.refresh_pwa();
        }
        self.sweep_removals();
    }

    fn sweep_removals(&mut self) {
        if self.removing.is_empty() {
            return;
        }
        let store = self.launcher.store().clone();
        let ready: Vec<AgentId> = self
            .removing
            .iter()
            .copied()
            .filter(|id| match self.agents.get(id).map(|e| &e.link) {
                Some(Link::Exited(_)) => store.read_runtime(*id).ok().flatten().is_none(),
                Some(Link::Lost) | None => true,
                Some(_) => false,
            })
            .collect();
        for id in ready {
            self.removing.remove(&id);
            if self.agents.contains_key(&id) {
                self.remove_agent(id);
            }
        }
    }

    fn rescan(&mut self) {
        let store = self.launcher.store().clone();
        let Ok(records) = store.list_agents() else { return };
        let ids: HashSet<AgentId> = records.iter().map(|r| r.id).collect();
        let removed: Vec<AgentId> = self.agents.keys().filter(|id| !ids.contains(id)).copied().collect();
        for id in removed {
            self.forget(id);
        }
        let mut refresh = Vec::new();
        for record in &records {
            match self.agents.get_mut(&record.id) {
                Some(entry) => {
                    if &entry.record != record {
                        entry.record = record.clone();
                        self.dirty = true;
                    }
                    if entry.is_dead() {
                        let exit_gone = store.read_exit(record.id).ok().flatten().is_none();
                        let runtime = store.read_runtime(record.id).ok().flatten();
                        if exit_gone && runtime.is_some_and(|r| entry.failed_runtime != Some(r.started_at_ms)) {
                            refresh.push(record.id);
                        }
                    }
                }
                None => {
                    self.agents.insert(record.id, AgentEntry::new(record.clone()));
                    refresh.push(record.id);
                }
            }
        }
        for id in refresh {
            self.refresh_link(id);
        }
        if let Ok(sessions) = store.load_sessions_with(&records)
            && sessions != self.sessions
        {
            self.sessions = sessions;
            if !self.sessions.iter().any(|s| s.name == self.session) {
                self.session = DEFAULT_SESSION.to_string();
            }
            self.dirty = true;
        }
        self.ensure_selection();
        self.request_subagents();
        self.request_changes();
    }

    fn forget(&mut self, id: AgentId) {
        if self.view == View::Agent(id) {
            self.view = View::Overview;
        }
        if self.diff.as_ref().is_some_and(|diff| diff.id == id) {
            self.diff = None;
        }
        self.tabs.retain(|t| *t != id);
        self.agents.remove(&id);
        self.removing.remove(&id);
        self.ensure_selection();
        self.dirty = true;
    }

    fn on_connected(&mut self, id: AgentId, generation: u64, result: Result<Arc<Connection>, String>) {
        let store = self.launcher.store().clone();
        let Some(entry) = self.agents.get_mut(&id) else { return };
        if entry.generation() != Some(generation) {
            return;
        }
        self.dirty = true;
        match result {
            Ok(conn) => {
                entry.link = Link::Live { generation, conn };
                entry.failed_runtime = None;
                if self.view == View::Agent(id) {
                    self.attach(id);
                }
            }
            Err(_) => {
                if let Ok(Some(exit)) = store.read_exit(id) {
                    entry.link = Link::Exited(exit);
                } else {
                    entry.failed_runtime = store.read_runtime(id).ok().flatten().map(|r| r.started_at_ms);
                    entry.link = Link::Lost;
                }
                if self.view == View::Agent(id) {
                    self.attach(id);
                }
            }
        }
    }

    fn on_holder(&mut self, id: AgentId, generation: u64, msg: Option<HolderMsg>) {
        let viewing = self.view == View::Agent(id);
        let desk_size = self.attached_size;
        let store = self.launcher.store().clone();
        let Some(entry) = self.agents.get_mut(&id) else { return };
        if entry.generation() != Some(generation) {
            return;
        }
        match msg {
            None => {
                entry.attached = false;
                if let Ok(Some(exit)) = store.read_exit(id) {
                    entry.link = Link::Exited(exit);
                    self.dirty = true;
                } else {
                    self.connect_agent(id, Duration::ZERO);
                }
            }
            Some(HolderMsg::Welcome(_)) => {}
            Some(HolderMsg::Status(status)) => {
                let previous = entry.status.as_ref().map(|s| s.state);
                let state = status.state;
                let finished = state == AgentState::Done && previous != Some(AgentState::Done);
                if finished
                    && viewing
                    && let Some(conn) = entry.conn()
                {
                    let _ = conn.send(&ClientMsg::Seen);
                }
                if viewing
                    && entry.attached
                    && let (Some(size), Some(conn)) = (desk_size, entry.conn())
                    && (status.rows, status.cols) != (size.rows, size.cols)
                {
                    let _ = conn.send(&ClientMsg::Resize(size));
                }
                let needs_you = state == AgentState::Paused && previous.is_some_and(|s| s != AgentState::Paused);
                let name = entry.record.name.clone();
                entry.status = Some(status);
                self.dirty = true;
                let alert = if !viewing && previous.is_some() && (finished || needs_you) {
                    entry.alert(state, now_ms())
                } else {
                    None
                };
                self.alerts.extend(alert);
                if finished && !viewing {
                    self.set_notice(Tone::Success, format!("{name} finished · alt+n opens it"));
                } else if needs_you && !viewing {
                    self.set_notice(Tone::Warn, format!("{name} is waiting for you · alt+n opens it"));
                }
            }
            Some(HolderMsg::Snapshot(snapshot)) => {
                entry.term = Some(TermView::from_snapshot(snapshot.size, &snapshot.data));
                self.dirty = true;
            }
            Some(HolderMsg::Output(bytes)) => {
                if let Some(term) = entry.term.as_mut() {
                    term.process(&bytes);
                    if viewing {
                        self.dirty = true;
                    }
                }
            }
            Some(HolderMsg::Exited(info)) => {
                let text = format!("{} {}", entry.record.name, info.describe());
                entry.link = Link::Exited(info);
                entry.attached = false;
                self.set_notice(Tone::Info, text);
            }
        }
    }

    fn on_launched(&mut self, result: Result<AgentRecord, String>) {
        self.launching = self.launching.saturating_sub(1);
        match result {
            Ok(record) => {
                let id = record.id;
                let text = match &record.worktree {
                    Some(worktree) => format!(
                        "{} {} started on branch {} in a worktree of {}",
                        record.kind.label(),
                        record.name,
                        worktree.branch,
                        self.display(&worktree.source)
                    ),
                    None => format!("{} {} started in {}", record.kind.label(), record.name, self.display(&record.cwd)),
                };
                let in_session = record.session == self.session;
                self.agents.insert(id, AgentEntry::new(record));
                self.connect_agent(id, START_WAIT);
                if in_session {
                    self.selected = Some(id);
                }
                self.set_notice(Tone::Success, text);
            }
            Err(err) => self.set_notice(Tone::Error, format!("could not start agent: {err}")),
        }
    }

    fn on_restarted(&mut self, id: AgentId, result: Result<AgentRecord, String>) {
        match result {
            Ok(record) => {
                if let Some(entry) = self.agents.get_mut(&id) {
                    entry.record = record;
                    entry.status = None;
                    entry.term = None;
                    entry.failed_runtime = None;
                }
                self.connect_agent(id, START_WAIT);
            }
            Err(err) => {
                let exit = self.launcher.store().read_exit(id).ok().flatten();
                let Some(entry) = self.agents.get_mut(&id) else { return };
                entry.link = match exit {
                    Some(info) => Link::Exited(info),
                    None => Link::Lost,
                };
                let name = entry.record.name.clone();
                self.set_notice(Tone::Error, format!("could not recover {name}: {err}"));
            }
        }
    }

    fn attach(&mut self, id: AgentId) {
        let size = self.agent_term_size();
        let Some(entry) = self.agents.get_mut(&id) else { return };
        match &entry.link {
            Link::Live { conn, .. } => {
                if conn.send(&ClientMsg::Attach(size)).is_ok() {
                    entry.attached = true;
                    self.attached_size = Some(size);
                }
            }
            Link::Exited(_) | Link::Lost => {
                if entry.term.is_none() {
                    self.replay(id);
                }
            }
            Link::Connecting(_) => {}
        }
        self.dirty = true;
    }

    fn detach(&mut self, id: AgentId) {
        let Some(entry) = self.agents.get_mut(&id) else { return };
        if entry.attached {
            if let Some(conn) = entry.conn() {
                let _ = conn.send(&ClientMsg::Detach);
            }
            entry.attached = false;
            entry.term = None;
        }
        self.attached_size = None;
    }

    fn replay(&self, id: AgentId) {
        let path = self.launcher.store().home().output_log(id);
        let tx = self.tx.clone();
        let _ = thread::Builder::new().name("orkai-replay".into()).spawn(move || {
            let parser = oplog::replay(&path).ok().flatten().map(Box::new);
            let _ = tx.send(AppEvent::Replayed { id, parser });
        });
    }

    pub fn open_agent(&mut self, id: AgentId) {
        if !self.agents.contains_key(&id) {
            return;
        }
        if !self.tabs.contains(&id) {
            self.tabs.push(id);
        }
        self.diff = None;
        if let View::Agent(current) = self.view {
            if current == id {
                return;
            }
            self.detach(current);
        }
        self.view = View::Agent(id);
        if self.agents.get(&id).is_some_and(|e| e.record.session == self.session) {
            self.selected = Some(id);
        }
        self.show_help = false;
        self.prompt = None;
        self.paths.clear();
        self.attach(id);
        self.persist_ui();
    }

    fn open_next_waiting(&mut self) {
        let current = match self.view {
            View::Agent(id) => Some(id),
            View::Overview | View::Pwa => None,
        };
        let Some(id) = self.waiting_for_you().into_iter().find(|id| Some(*id) != current) else {
            self.set_notice(Tone::Info, "no agent is waiting for you");
            return;
        };
        if let Some(session) = self.agents.get(&id).map(|e| e.record.session.clone()).filter(|s| *s != self.session) {
            self.switch_session(&session);
        }
        self.open_agent(id);
    }

    pub fn show_overview(&mut self) {
        if let View::Agent(id) = self.view {
            self.detach(id);
        }
        self.view = View::Overview;
        self.dirty = true;
    }

    fn close_tab(&mut self, id: AgentId) {
        if self.view == View::Agent(id) {
            self.show_overview();
        }
        self.tabs.retain(|t| *t != id);
        if let Some(entry) = self.agents.get_mut(&id)
            && entry.is_dead()
        {
            entry.term = None;
        }
        self.persist_ui();
        self.dirty = true;
    }

    fn switch_session(&mut self, name: &str) {
        if !self.sessions.iter().any(|s| s.name == name) {
            self.set_notice(Tone::Error, format!("session '{name}' does not exist"));
            return;
        }
        self.show_overview();
        self.diff = None;
        self.session = name.to_string();
        if let Some(dir) = self.session_dirs.get(name).filter(|dir| dir.is_dir()) {
            self.where_dir = dir.clone();
        }
        self.selected = None;
        self.list_offset = 0;
        self.ensure_selection();
        self.persist_ui();
        self.dirty = true;
    }

    fn cycle_session(&mut self, delta: isize) {
        let Some(index) = self.sessions.iter().position(|s| s.name == self.session) else { return };
        let len = self.sessions.len() as isize;
        let next = (index as isize + delta).rem_euclid(len) as usize;
        let name = self.sessions[next].name.clone();
        self.switch_session(&name);
    }

    fn move_selection(&mut self, delta: isize) {
        let ids = self.ordered_ids();
        if ids.is_empty() {
            return;
        }
        let current = self.selected.and_then(|id| ids.iter().position(|x| *x == id)).unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, ids.len() as isize - 1) as usize;
        self.selected = Some(ids[next]);
        self.dirty = true;
    }

    fn name_taken(&self, name: &str) -> bool {
        self.agents.values().any(|e| e.record.session == self.session && e.record.name == name)
    }

    fn default_name(&self, kind: AgentKind, task: &str) -> String {
        default_agent_name(kind, task, |name| self.name_taken(name))
    }

    fn launch_task(&mut self, kind: AgentKind, task: String) {
        if !self.where_dir.is_dir() {
            self.set_notice(Tone::Error, format!("{} is not a directory", self.display(&self.where_dir)));
            return;
        }
        let name = self.default_name(kind, &task);
        let draft =
            NewAgent { session: self.session.clone(), name: name.clone(), kind, cwd: self.where_dir.clone(), task };
        let size = self.agent_term_size();
        let launcher = self.launcher.clone();
        let tx = self.tx.clone();
        let isolate = self.worktree && kind.is_ai();
        let bypass = self.bypass && kind.is_ai();
        self.launching += 1;
        self.task.clear();
        let place = if isolate { " in a new worktree" } else { "" };
        let mode = if bypass { " with permissions bypassed" } else { "" };
        self.set_notice(Tone::Info, format!("starting {} {name}{place}{mode}…", kind.label()));
        let _ = thread::Builder::new().name("orkai-launch".into()).spawn(move || {
            let result = launcher.create(draft, size, isolate, bypass).map_err(|e| e.to_string());
            let _ = tx.send(AppEvent::Launched { result });
        });
    }

    fn set_worktree(&mut self, on: bool) {
        self.worktree = on;
        self.persist_ui();
        let text = if on {
            "new ai agents start in their own git worktree and branch"
        } else {
            "new agents run directly in the where folder"
        };
        self.set_notice(Tone::Info, text);
    }

    fn set_bypass(&mut self, on: bool) {
        self.bypass = on;
        self.persist_ui();
        if on {
            self.set_notice(Tone::Warn, "new ai agents start with every permission check bypassed");
        } else {
            self.set_notice(Tone::Info, "new ai agents ask before running tools");
        }
    }

    fn selected_worktree(&mut self) -> Option<(AgentId, Worktree)> {
        let Some(id) = self.selected else {
            self.set_notice(Tone::Error, "select an agent first");
            return None;
        };
        match self.agents.get(&id).and_then(|e| e.record.worktree.clone()) {
            Some(worktree) => Some((id, worktree)),
            None => {
                self.set_notice(Tone::Error, "the selected agent does not run in its own worktree");
                None
            }
        }
    }

    fn open_diff(&mut self) {
        let Some((id, worktree)) = self.selected_worktree() else { return };
        let tx = self.tx.clone();
        self.set_notice(Tone::Info, format!("reading the changes on {}…", worktree.branch));
        let _ = thread::Builder::new().name("orkai-diff".into()).spawn(move || {
            let result = worktree::diff(&worktree).map_err(|e| e.to_string());
            let _ = tx.send(AppEvent::Diff { id, result });
        });
    }

    fn on_diff(&mut self, id: AgentId, result: Result<Vec<u8>, String>) {
        if !self.agents.contains_key(&id) {
            return;
        }
        match result {
            Ok(text) => {
                let body = render::areas(self.screen, false, !self.tabs.is_empty(), 1).body;
                let size = TermSize::new(body.height.saturating_sub(1).max(1), body.width.max(1));
                self.diff = Some(DiffPane { id, empty: text.is_empty(), view: TermView::document(size, &text) });
                self.notice = None;
                self.dirty = true;
            }
            Err(err) => self.set_notice(Tone::Error, format!("could not read the changes: {err}")),
        }
    }

    fn on_diff_key(&mut self, key: KeyEvent) {
        if keys::is_next_waiting_key(&key) {
            self.diff = None;
            self.open_next_waiting();
            return;
        }
        let page =
            isize::try_from(self.diff.as_ref().map_or(1, |d| d.view.size().rows.saturating_sub(1).max(1))).unwrap_or(1);
        let Some(diff) = self.diff.as_mut() else { return };
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => self.diff = None,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.diff = None,
            KeyCode::Up | KeyCode::Char('k') => diff.view.scroll_by(1),
            KeyCode::Down | KeyCode::Char('j') => diff.view.scroll_by(-1),
            KeyCode::PageUp => diff.view.scroll_by(page),
            KeyCode::PageDown | KeyCode::Char(' ') => diff.view.scroll_by(-page),
            KeyCode::Home | KeyCode::Char('g') => diff.view.scroll_by(isize::MAX),
            KeyCode::End | KeyCode::Char('G') => diff.view.scroll_to_bottom(),
            _ => {}
        }
    }

    fn request_merge(&mut self) {
        let Some((id, _)) = self.selected_worktree() else { return };
        let Some(entry) = self.agents.get(&id) else { return };
        if matches!(entry.state(), AgentState::Working | AgentState::Paused) {
            let name = entry.record.name.clone();
            self.set_notice(Tone::Error, format!("wait for {name} to finish (or stop it) before merging its worktree"));
            return;
        }
        self.prompt = Some(Prompt::Confirm(Confirm::Merge(id)));
    }

    fn merge_agent(&mut self, id: AgentId) {
        let Some(entry) = self.agents.get(&id) else { return };
        let Some(worktree) = entry.record.worktree.clone() else { return };
        let message = merge_message(&entry.record);
        let tx = self.tx.clone();
        self.set_notice(Tone::Info, format!("merging {}…", worktree.branch));
        let _ = thread::Builder::new().name("orkai-merge".into()).spawn(move || {
            let result = worktree::merge(&worktree, &message).map_err(|e| e.to_string());
            let _ = tx.send(AppEvent::Merged { id, result });
        });
    }

    fn on_merged(&mut self, id: AgentId, result: Result<Merged, String>) {
        let merged = match result {
            Ok(merged) => merged,
            Err(err) => return self.set_notice(Tone::Error, format!("merge failed: {err}")),
        };
        let store = self.launcher.store().clone();
        let Some(entry) = self.agents.get_mut(&id) else { return };
        let Some(worktree) = entry.record.worktree.as_mut() else { return };
        worktree.base = merged.tip;
        let branch = worktree.branch.clone();
        entry.changes = Some(Changes::default());
        if let Err(err) = store.save_agent(&entry.record) {
            return self.set_notice(Tone::Error, format!("{branch} was merged, but orkai could not record it: {err}"));
        }
        let text = if merged.up_to_date {
            format!("{branch} had nothing new for {}", merged.target)
        } else {
            format!("{branch} merged into {}", merged.target)
        };
        self.set_notice(Tone::Success, text);
    }

    fn restart_agents(&mut self, ids: Vec<AgentId>) {
        if ids.is_empty() {
            self.set_notice(Tone::Info, "nothing to recover in this session");
            return;
        }
        let size = self.agent_term_size();
        let mut records = Vec::new();
        for id in &ids {
            let generation = self.next_gen();
            if let Some(entry) = self.agents.get_mut(id) {
                entry.link = Link::Connecting(generation);
                entry.status = None;
                entry.term = None;
                records.push(entry.record.clone());
            }
        }
        let count = records.len();
        let launcher = self.launcher.clone();
        let tx = self.tx.clone();
        let _ = thread::Builder::new().name("orkai-recover".into()).spawn(move || {
            for mut record in records {
                let id = record.id;
                let result =
                    launcher.start(&mut record, LaunchMode::Resume, size).map(|()| record).map_err(|e| e.to_string());
                if tx.send(AppEvent::Restarted { id, result }).is_err() {
                    return;
                }
            }
        });
        self.set_notice(Tone::Info, format!("recovering {count} agent(s)…"));
    }

    fn remove_agent(&mut self, id: AgentId) {
        let name = self.agents.get(&id).map(|e| e.record.name.clone()).unwrap_or_default();
        let worktree = self.agents.get(&id).and_then(|e| e.record.worktree.clone());
        match self.launcher.store().remove_agent(id) {
            Ok(()) => {
                self.forget(id);
                self.persist_ui();
                self.set_notice(Tone::Info, format!("{name} removed"));
                if let Some(worktree) = worktree {
                    self.discard_worktree(name, worktree);
                }
            }
            Err(err) => self.set_notice(Tone::Error, format!("could not remove {name}: {err}")),
        }
    }

    fn discard_worktree(&self, name: String, worktree: Worktree) {
        let home = self.launcher.store().home().clone();
        let tx = self.tx.clone();
        let _ = thread::Builder::new().name("orkai-discard".into()).spawn(move || {
            let result = worktree::discard(&home, &worktree).map_err(|e| e.to_string());
            let _ = tx.send(AppEvent::Discarded { name, result });
        });
    }

    fn kill_agent(&mut self, id: AgentId) {
        let Some(entry) = self.agents.get(&id) else { return };
        match entry.conn() {
            Some(conn) => {
                let _ = conn.send(&ClientMsg::Kill);
                let name = entry.record.name.clone();
                self.set_notice(Tone::Info, format!("stopping {name}…"));
            }
            None => self.remove_agent(id),
        }
    }

    fn request_kill_or_remove(&mut self, id: AgentId) {
        let Some(entry) = self.agents.get(&id) else { return };
        self.prompt = Some(Prompt::Confirm(if entry.is_dead() { Confirm::Remove(id) } else { Confirm::Kill(id) }));
        self.dirty = true;
    }

    fn request_delete(&mut self, id: AgentId) {
        let Some(entry) = self.agents.get(&id) else { return };
        self.prompt = Some(Prompt::Confirm(if entry.is_dead() { Confirm::Remove(id) } else { Confirm::Delete(id) }));
        self.dirty = true;
    }

    fn delete_agent(&mut self, id: AgentId) {
        let Some(entry) = self.agents.get(&id) else { return };
        if entry.is_dead() {
            self.remove_agent(id);
            return;
        }
        self.removing.insert(id);
        self.kill_agent(id);
    }

    fn rename_agent(&mut self, id: AgentId, name: &str) {
        let name = match validate_agent_name(name) {
            Ok(name) => name,
            Err(err) => return self.set_notice(Tone::Error, err),
        };
        let store = self.launcher.store().clone();
        let Some(entry) = self.agents.get_mut(&id) else { return };
        let old = std::mem::replace(&mut entry.record.name, name.clone());
        match store.save_agent(&entry.record) {
            Ok(()) => self.set_notice(Tone::Success, format!("{old} is now {name}")),
            Err(err) => {
                entry.record.name = old;
                self.set_notice(Tone::Error, format!("rename failed: {err}"));
            }
        }
    }

    fn create_session(&mut self, name: &str) {
        let store = self.launcher.store().clone();
        match store.create_session(name) {
            Ok(session) => {
                self.sessions = store.load_sessions().unwrap_or_else(|_| self.sessions.clone());
                self.switch_session(&session.name);
                self.set_notice(Tone::Success, format!("session {} created", session.name));
            }
            Err(err) => self.set_notice(Tone::Error, err.to_string()),
        }
    }

    fn rename_session(&mut self, name: &str) {
        let store = self.launcher.store().clone();
        let old = self.session.clone();
        match store.rename_session(&old, name) {
            Ok(()) => {
                let new = name.trim().to_string();
                for entry in self.agents.values_mut().filter(|e| e.record.session == old) {
                    entry.record.session = new.clone();
                }
                self.sessions = store.load_sessions().unwrap_or_else(|_| self.sessions.clone());
                if let Some(dir) = self.session_dirs.remove(&old) {
                    self.session_dirs.insert(new.clone(), dir);
                }
                self.session = new.clone();
                self.persist_ui();
                self.set_notice(Tone::Success, format!("session {old} is now {new}"));
            }
            Err(err) => self.set_notice(Tone::Error, err.to_string()),
        }
    }

    fn delete_session(&mut self, name: &str) {
        let store = self.launcher.store().clone();
        match store.delete_session(name) {
            Ok(()) => {
                self.sessions = store.load_sessions().unwrap_or_else(|_| self.sessions.clone());
                self.session_dirs.remove(name);
                self.switch_session(DEFAULT_SESSION);
                self.set_notice(Tone::Info, format!("session {name} deleted"));
            }
            Err(err) => self.set_notice(Tone::Error, err.to_string()),
        }
    }

    fn set_where(&mut self, input: &str) {
        let path = expand_tilde(input);
        let path = if path.is_relative() { self.where_dir.join(path) } else { path };
        match path.canonicalize() {
            Ok(dir) if dir.is_dir() => self.apply_where(strip_verbatim(dir)),
            _ => self.set_notice(Tone::Error, format!("{input} is not a directory")),
        }
    }

    fn apply_where(&mut self, dir: PathBuf) {
        let text = self.display(&dir);
        self.session_dirs.insert(self.session.clone(), dir.clone());
        self.where_dir = dir;
        self.persist_ui();
        self.set_notice(Tone::Info, format!("new tasks in {} will run in {text}", self.session));
    }

    fn open_where_prompt(&mut self) {
        let text = self.display(&self.where_dir);
        let separator = separator_for(&text);
        let text = if text.ends_with(['/', '\\']) { text } else { format!("{text}{separator}") };
        self.prompt = Some(Prompt::Where(LineInput::with_text(&text)));
    }

    fn sync_suggestions(&mut self) {
        if let Some(Prompt::Where(input)) = &self.prompt {
            let text = input.text();
            self.paths.refresh(&text, &self.where_dir);
            return;
        }
        let text = self.task.text();
        if text.starts_with('/') && !text.contains(char::is_whitespace) {
            self.skills.refresh(self.who, &self.where_dir);
        }
        match where_argument(&text) {
            Some((_, arg)) => self.paths.refresh(arg, &self.where_dir),
            None => self.paths.clear(),
        }
    }

    fn on_composer_tab(&mut self, backwards: bool) {
        let text = self.task.text();
        if let Some((head, arg)) = where_argument(&text) {
            if let Some(completed) = self.paths.tab(arg, &self.where_dir, backwards) {
                self.task.set(&format!("{head}{completed}"));
            }
            return;
        }
        if text.starts_with('/') && !text.contains(char::is_whitespace) {
            self.skills.refresh(self.who, &self.where_dir);
            let skills = self.skills.matching(self.who, &text);
            if let Some(completed) = complete_command(&text, &skills) {
                self.task.set(&completed);
            }
            return;
        }
        self.cycle_who(backwards);
    }

    fn installed_kinds(&mut self) -> Vec<AgentKind> {
        let config = self.launcher.config();
        self.installed.get_or_insert_with(|| launch::installed_kinds(config)).clone()
    }

    fn cycle_who(&mut self, backwards: bool) {
        let installed = self.installed_kinds();
        let mut kind = self.who;
        for _ in 0..AgentKind::ALL.len() {
            kind = if backwards { kind.prev() } else { kind.next() };
            if installed.contains(&kind) {
                break;
            }
        }
        self.who = kind;
        self.persist_ui();
    }

    fn queue_input(&mut self, id: AgentId, bytes: Vec<u8>) {
        match self.pending_input.last_mut() {
            Some((last, buffer)) if *last == id => buffer.extend_from_slice(&bytes),
            _ => self.pending_input.push((id, bytes)),
        }
    }

    pub fn flush_input(&mut self) {
        for (id, bytes) in std::mem::take(&mut self.pending_input) {
            if let Some(conn) = self.agents.get(&id).and_then(AgentEntry::conn) {
                let _ = conn.send(&ClientMsg::Input(bytes));
            }
        }
    }

    pub fn sync_attached_size(&mut self) {
        let View::Agent(id) = self.view else { return };
        let size = self.agent_term_size();
        let Some(entry) = self.agents.get_mut(&id) else { return };
        if !entry.attached {
            return;
        }
        if let Some(term) = entry.term.as_mut()
            && term.size() != size
        {
            term.resize(size);
        }
        if self.attached_size != Some(size) {
            if let Some(conn) = entry.conn() {
                let _ = conn.send(&ClientMsg::Resize(size));
            }
            self.attached_size = Some(size);
        }
    }

    fn on_input(&mut self, event: Event) {
        match event {
            Event::Key(key) if keys::is_press(&key) => {
                self.dirty = true;
                if self.show_help {
                    self.show_help = false;
                    return;
                }
                match self.view {
                    View::Agent(id) => self.on_agent_key(id, key),
                    View::Overview => self.on_overview_key(key),
                    View::Pwa => self.on_pwa_key(key),
                }
            }
            Event::Mouse(mouse) => self.on_mouse(mouse),
            Event::Paste(text) => {
                self.dirty = true;
                match self.view {
                    View::Agent(id) => {
                        let bracketed = self
                            .agents
                            .get(&id)
                            .and_then(|e| e.term.as_ref())
                            .is_some_and(|t| t.screen().bracketed_paste());
                        if self.agents.get(&id).is_some_and(|e| e.conn().is_some()) {
                            self.queue_input(id, keys::encode_paste(&text, bracketed));
                        }
                    }
                    View::Overview | View::Pwa => {
                        match self.prompt.as_mut() {
                            Some(
                                Prompt::Where(input)
                                | Prompt::Rename(_, input)
                                | Prompt::NewSession(input)
                                | Prompt::RenameSession(input),
                            ) => input.insert_str(&text),
                            _ => self.task.insert_multiline(&text),
                        }
                        self.sync_suggestions();
                    }
                }
            }
            Event::Resize(width, height) => {
                self.screen = Rect::new(0, 0, width, height);
                self.dirty = true;
            }
            _ => {}
        }
    }

    fn on_agent_key(&mut self, id: AgentId, key: KeyEvent) {
        if keys::is_detach_key(&key) {
            self.show_overview();
            return;
        }
        if keys::is_next_waiting_key(&key) {
            self.open_next_waiting();
            return;
        }
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let page = isize::try_from(self.agent_term_size().rows.saturating_sub(2).max(1)).unwrap_or(1);
        let Some(entry) = self.agents.get_mut(&id) else {
            self.show_overview();
            return;
        };
        match key.code {
            KeyCode::PageUp if shift || entry.is_dead() => {
                if let Some(term) = entry.term.as_mut() {
                    term.scroll_by(page);
                }
                return;
            }
            KeyCode::PageDown if shift || entry.is_dead() => {
                if let Some(term) = entry.term.as_mut() {
                    term.scroll_by(-page);
                }
                return;
            }
            _ => {}
        }
        if entry.is_dead() {
            match key.code {
                KeyCode::Esc => self.show_overview(),
                KeyCode::Enter => self.restart_agents(vec![id]),
                KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => self.restart_agents(vec![id]),
                _ => {}
            }
            return;
        }
        let app_cursor = entry.term.as_ref().is_some_and(|t| t.screen().application_cursor());
        if let Some(term) = entry.term.as_mut() {
            term.scroll_to_bottom();
        }
        if entry.conn().is_none() {
            return;
        }
        if let Some(bytes) = keys::encode_key(&key, app_cursor) {
            self.queue_input(id, bytes);
        }
    }

    fn on_prompt_key(&mut self, key: KeyEvent) {
        let Some(prompt) = self.prompt.take() else { return };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if let Prompt::Confirm(confirm) = prompt {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => match confirm {
                    Confirm::Kill(id) => self.kill_agent(id),
                    Confirm::Remove(id) => self.remove_agent(id),
                    Confirm::Delete(id) => self.delete_agent(id),
                    Confirm::DeleteSession(name) => self.delete_session(&name),
                    Confirm::Recover(ids) => self.restart_agents(ids),
                    Confirm::Merge(id) => self.merge_agent(id),
                },
                _ => self.set_notice(Tone::Info, "cancelled"),
            }
            return;
        }
        match prompt {
            Prompt::Confirm(_) => {}
            Prompt::Where(mut input) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => self.set_where(&input.text()),
                KeyCode::Tab | KeyCode::BackTab => {
                    let backwards = key.code == KeyCode::BackTab;
                    if let Some(completed) = self.paths.tab(&input.text(), &self.where_dir, backwards) {
                        input.set(&completed);
                    }
                    self.prompt = Some(Prompt::Where(input));
                }
                _ => {
                    edit_line(&mut input, &key, ctrl);
                    self.prompt = Some(Prompt::Where(input));
                }
            },
            Prompt::Rename(id, mut input) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => self.rename_agent(id, &input.text()),
                _ => {
                    edit_line(&mut input, &key, ctrl);
                    self.prompt = Some(Prompt::Rename(id, input));
                }
            },
            Prompt::NewSession(mut input) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => self.create_session(&input.text()),
                _ => {
                    edit_line(&mut input, &key, ctrl);
                    self.prompt = Some(Prompt::NewSession(input));
                }
            },
            Prompt::RenameSession(mut input) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => self.rename_session(&input.text()),
                _ => {
                    edit_line(&mut input, &key, ctrl);
                    self.prompt = Some(Prompt::RenameSession(input));
                }
            },
        }
    }

    fn on_overview_key(&mut self, key: KeyEvent) {
        if self.diff.is_some() && self.prompt.is_none() {
            self.on_diff_key(key);
            return;
        }
        if self.prompt.is_some() {
            self.on_prompt_key(key);
        } else {
            self.on_composer_key(key);
        }
        self.sync_suggestions();
    }

    fn on_composer_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Char('c') if ctrl && !alt => {
                if self.task.is_empty() {
                    self.should_quit = true;
                } else {
                    self.task.clear();
                }
            }
            KeyCode::Char('q') | KeyCode::Char('d') if ctrl && !alt && self.task.is_empty() => {
                self.should_quit = true;
            }
            KeyCode::Char('l') if ctrl && !alt => self.open_where_prompt(),
            KeyCode::Char('n') if ctrl && !alt => {
                let name = self.launcher.store().next_session_name().unwrap_or_default();
                self.prompt = Some(Prompt::NewSession(LineInput::with_text(&name)));
            }
            KeyCode::Char('r') if ctrl && !alt => {
                let lost = self.lost_in_session();
                if lost.is_empty() {
                    self.set_notice(Tone::Info, "no interrupted agents in this session");
                } else {
                    self.prompt = Some(Prompt::Confirm(Confirm::Recover(lost)));
                }
            }
            KeyCode::Char('x') if ctrl && !alt => {
                if let Some(id) = self.selected {
                    self.request_kill_or_remove(id);
                }
            }
            KeyCode::Char('e') if ctrl && !alt => self.begin_rename(),
            KeyCode::Char('t') if ctrl && !alt => self.set_worktree(!self.worktree),
            KeyCode::Char('p') if ctrl && !alt => self.open_pwa(),
            KeyCode::Delete if self.task.is_empty() => {
                if let Some(id) = self.selected {
                    self.request_delete(id);
                }
            }
            KeyCode::Char('w') if ctrl && !alt => self.task.delete_word(),
            KeyCode::Char('u') if ctrl && !alt => self.task.clear(),
            KeyCode::F(2) => self.begin_rename(),
            KeyCode::F(1) => self.show_help = !self.show_help,
            KeyCode::Char('?') if self.task.is_empty() => self.show_help = !self.show_help,
            KeyCode::Char(d @ '1'..='9') if alt && !ctrl => {
                let index = usize::from(d as u8 - b'1');
                if let Some(id) = self.tabs.get(index).copied() {
                    self.open_agent(id);
                }
            }
            KeyCode::Char(keys::NEXT_WAITING_KEY) if alt && !ctrl => self.open_next_waiting(),
            KeyCode::Up if ctrl && !alt => self.recall_older(),
            KeyCode::Down if ctrl && !alt => self.recall_newer(),
            KeyCode::Up if alt => self.cycle_session(-1),
            KeyCode::Down if alt => self.cycle_session(1),
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-10),
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::Tab => self.on_composer_tab(false),
            KeyCode::BackTab => self.on_composer_tab(true),
            KeyCode::Esc => {
                self.task.clear();
                self.notice = None;
            }
            KeyCode::Enter if shift || alt => self.task.insert_newline(),
            KeyCode::Enter => self.submit(),
            _ => edit_line(&mut self.task, &key, ctrl),
        }
    }

    fn recall_older(&mut self) {
        if let Some(text) = self.history.older(&self.task.text()) {
            self.replace_task(&text);
        }
    }

    fn recall_newer(&mut self) {
        if let Some(text) = self.history.newer() {
            self.replace_task(&text);
        }
    }

    fn replace_task(&mut self, text: &str) {
        self.task.clear();
        self.task.insert_multiline(text);
    }

    fn begin_rename(&mut self) {
        if let Some(id) = self.selected
            && let Some(entry) = self.agents.get(&id)
        {
            self.prompt = Some(Prompt::Rename(id, LineInput::with_text(&entry.record.name)));
        }
    }

    fn submit(&mut self) {
        let text = self.task.text().trim().to_string();
        if text.is_empty() {
            if let Some(id) = self.selected {
                self.open_agent(id);
            }
            return;
        }
        if text.starts_with('/') {
            self.task.clear();
            self.run_command(&text);
            return;
        }
        if let Some(command) = text.strip_prefix(SHELL_PREFIX) {
            match command.trim() {
                "" => self.set_notice(Tone::Error, "type a command after ! to run it in a new terminal"),
                command => {
                    self.history.record(&text);
                    self.launch_task(AgentKind::Terminal, command.to_string());
                }
            }
            return;
        }
        self.history.record(&text);
        self.launch_task(self.who, text);
    }

    pub fn run_command(&mut self, line: &str) {
        let mut parts = line.trim_start_matches('/').splitn(2, char::is_whitespace);
        let command = parts.next().unwrap_or_default().to_ascii_lowercase();
        let arg = parts.next().unwrap_or_default().trim().to_string();
        match command.as_str() {
            "help" | "h" => self.show_help = true,
            "quit" | "q" | "exit" => self.should_quit = true,
            "cd" | "where" => {
                if arg.is_empty() {
                    self.open_where_prompt();
                    self.sync_suggestions();
                } else {
                    self.set_where(&arg);
                }
            }
            "who" => match AgentKind::parse(&arg) {
                Some(kind) => {
                    self.who = kind;
                    self.persist_ui();
                }
                None => self.set_notice(Tone::Error, format!("usage: /who {}", AgentKind::names())),
            },
            "session" | "s" => {
                if arg.is_empty() {
                    self.set_notice(Tone::Error, "usage: /session <name>");
                } else {
                    self.switch_session(&arg);
                }
            }
            "new-session" | "ns" => {
                if arg.is_empty() {
                    let name = self.launcher.store().next_session_name().unwrap_or_default();
                    self.prompt = Some(Prompt::NewSession(LineInput::with_text(&name)));
                } else {
                    self.create_session(&arg);
                }
            }
            "rename-session" => {
                if arg.is_empty() {
                    let current = self.session.clone();
                    self.prompt = Some(Prompt::RenameSession(LineInput::with_text(&current)));
                } else {
                    self.rename_session(&arg);
                }
            }
            "delete-session" => {
                let name = if arg.is_empty() { self.session.clone() } else { arg };
                self.prompt = Some(Prompt::Confirm(Confirm::DeleteSession(name)));
            }
            "rename" => match (self.selected, arg.is_empty()) {
                (Some(id), false) => self.rename_agent(id, &arg),
                (Some(_), true) => self.begin_rename(),
                (None, _) => self.set_notice(Tone::Error, "select an agent first"),
            },
            "kill" | "rm" | "remove" => match self.selected {
                Some(id) => self.request_kill_or_remove(id),
                None => self.set_notice(Tone::Error, "select an agent first"),
            },
            "recover" => {
                let lost = self.lost_in_session();
                self.restart_agents(lost);
            }
            "restart" => match self.selected {
                Some(id) if self.agents.get(&id).is_some_and(AgentEntry::is_dead) => self.restart_agents(vec![id]),
                Some(_) => self.set_notice(Tone::Error, "the selected agent is still running"),
                None => self.set_notice(Tone::Error, "select an agent first"),
            },
            "worktree" | "wt" => match arg.as_str() {
                "" => self.set_worktree(!self.worktree),
                "on" => self.set_worktree(true),
                "off" => self.set_worktree(false),
                _ => self.set_notice(Tone::Error, "usage: /worktree [on|off]"),
            },
            "bypass" => match arg.as_str() {
                "" => self.set_bypass(!self.bypass),
                "on" => self.set_bypass(true),
                "off" => self.set_bypass(false),
                _ => self.set_notice(Tone::Error, "usage: /bypass [on|off]"),
            },
            "diff" => self.open_diff(),
            "merge" => self.request_merge(),
            "pwa" | "phone" => match arg.as_str() {
                "off" if self.pwa.enabled => self.disable_pwa(),
                "off" => self.set_notice(Tone::Info, "the orkai pwa is already off"),
                "new" => {
                    self.open_pwa();
                    self.renew_pwa_code();
                }
                _ => self.open_pwa(),
            },
            "open" => match arg.parse::<AgentId>() {
                Ok(id) if self.agents.contains_key(&id) => self.open_agent(id),
                _ => self.set_notice(Tone::Error, "usage: /open <agent number>"),
            },
            "" => {}
            other => {
                self.skills.refresh(self.who, &self.where_dir);
                if self.skills.contains(self.who, other) {
                    self.launch_task(self.who, skills::invocation(self.who, line));
                } else {
                    self.set_notice(Tone::Error, format!("unknown command /{other} — try /help"));
                }
            }
        }
        self.dirty = true;
    }

    fn hit_at(&self, column: u16, row: u16) -> Option<Hit> {
        let position = Position::new(column, row);
        self.hits.iter().rev().find(|(rect, _)| rect.contains(position)).map(|(_, hit)| hit.clone())
    }

    fn on_mouse(&mut self, mouse: MouseEvent) {
        let hit = self.hit_at(mouse.column, mouse.row);
        if hit == Some(Hit::Terminal)
            && let View::Agent(id) = self.view
            && self.forward_mouse(id, &mouse)
        {
            return;
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.dirty = true;
                if self.show_help {
                    self.show_help = false;
                    return;
                }
                if matches!(&self.prompt, Some(Prompt::Confirm(confirm)) if confirm.is_popup()) {
                    self.prompt = None;
                    self.set_notice(Tone::Info, "cancelled");
                    return;
                }
                match hit {
                    Some(Hit::Session(name)) => self.switch_session(&name),
                    Some(Hit::NewSession) => {
                        let name = self.launcher.store().next_session_name().unwrap_or_default();
                        self.prompt = Some(Prompt::NewSession(LineInput::with_text(&name)));
                    }
                    Some(Hit::Agent(id)) => {
                        self.selected = Some(id);
                        self.open_agent(id);
                    }
                    Some(Hit::Tab(id)) => self.open_agent(id),
                    Some(Hit::CloseTab(id)) => self.close_tab(id),
                    Some(Hit::Overview) => self.show_overview(),
                    Some(Hit::Who) => {
                        self.cycle_who(false);
                        self.sync_suggestions();
                    }
                    Some(Hit::Where) => {
                        self.open_where_prompt();
                        self.sync_suggestions();
                    }
                    Some(Hit::Worktree) => self.set_worktree(!self.worktree),
                    Some(Hit::Bypass) => self.set_bypass(!self.bypass),
                    Some(Hit::Project(cwd)) => self.apply_where(cwd),
                    Some(Hit::Task { line, skip, x }) => self.place_cursor(mouse.column, x, line, skip),
                    Some(Hit::Recover) => {
                        let lost = self.lost_in_session();
                        if !lost.is_empty() {
                            self.prompt = Some(Prompt::Confirm(Confirm::Recover(lost)));
                        }
                    }
                    Some(Hit::Pwa) => self.open_pwa(),
                    Some(Hit::CopyPwaLink) => self.copy_pwa_link(),
                    Some(Hit::Terminal) | None => {}
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = mouse.kind == MouseEventKind::ScrollUp;
                self.dirty = true;
                match self.view {
                    View::Agent(id) => {
                        if let Some(term) = self.agents.get_mut(&id).and_then(|e| e.term.as_mut()) {
                            term.scroll_by(if up { SCROLL_STEP } else { -SCROLL_STEP });
                        }
                    }
                    View::Overview => match self.diff.as_mut() {
                        Some(diff) => diff.view.scroll_by(if up { SCROLL_STEP } else { -SCROLL_STEP }),
                        None => self.move_selection(if up { -1 } else { 1 }),
                    },
                    View::Pwa => {}
                }
            }
            _ => {}
        }
    }

    /// Puts the cursor where the user clicked inside the task being written.
    fn place_cursor(&mut self, column: u16, x: u16, line: usize, skip: usize) {
        let offset = usize::from(column.saturating_sub(x));
        let Some(index) = self.task.index_at(line, skip + offset) else { return };
        self.task.set_cursor(index);
        self.dirty = true;
    }

    fn forward_mouse(&mut self, id: AgentId, mouse: &MouseEvent) -> bool {
        let body = render::areas(self.screen, true, true, 1).body;
        let Some(entry) = self.agents.get(&id) else { return false };
        let Some(term) = entry.term.as_ref() else { return false };
        if entry.conn().is_none() || term.is_scrolled() {
            return false;
        }
        let screen = term.screen();
        let col = mouse.column.saturating_sub(body.x);
        let row = mouse.row.saturating_sub(body.y);
        match keys::encode_mouse(mouse, col, row, screen.mouse_protocol_mode(), screen.mouse_protocol_encoding()) {
            Some(bytes) => {
                self.queue_input(id, bytes);
                true
            }
            None => false,
        }
    }
}

fn edit_line(input: &mut LineInput, key: &KeyEvent, ctrl: bool) {
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Backspace if ctrl || alt => input.delete_word(),
        KeyCode::Backspace => input.backspace(),
        KeyCode::Delete => input.delete(),
        KeyCode::Left if ctrl && !alt => input.left_word(),
        KeyCode::Right if ctrl && !alt => input.right_word(),
        KeyCode::Left => input.left(),
        KeyCode::Right => input.right(),
        KeyCode::Home => input.home(),
        KeyCode::End => input.end(),
        KeyCode::Char('a') if ctrl && !alt => input.home(),
        KeyCode::Char('e') if ctrl && !alt => input.end(),
        KeyCode::Char('w') if ctrl && !alt => input.delete_word(),
        KeyCode::Char('u') if ctrl && !alt => input.clear(),
        KeyCode::Char(c) if !ctrl || alt => input.insert(c),
        _ => {}
    }
}

pub fn merge_message(record: &AgentRecord) -> String {
    let subject: String = record
        .task
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .chars()
        .take(MERGE_SUBJECT_MAX)
        .collect();
    if subject.is_empty() { record.name.clone() } else { format!("{}: {subject}", record.name) }
}

fn strip_verbatim(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbatim_prefixes_are_removed() {
        assert_eq!(strip_verbatim(PathBuf::from(r"\\?\C:\work")), PathBuf::from(r"C:\work"));
        assert_eq!(strip_verbatim(PathBuf::from(r"\\?\UNC\srv\share")), PathBuf::from(r"\\srv\share"));
        assert_eq!(strip_verbatim(PathBuf::from("/home/me")), PathBuf::from("/home/me"));
    }

    #[test]
    fn merge_commits_are_named_after_the_agent_and_its_task() {
        let mut record: AgentRecord = serde_json::from_str(
            r#"{"id":1,"session":"s","name":"fix-login","kind":"claude","cwd":"/x","created_at_ms":1}"#,
        )
        .unwrap();
        assert_eq!(merge_message(&record), "fix-login");
        record.task = "\n  Fix the login redirect loop\nand add tests".into();
        assert_eq!(merge_message(&record), "fix-login: Fix the login redirect loop");
        record.task = "x".repeat(200);
        assert_eq!(merge_message(&record).chars().count(), "fix-login: ".len() + MERGE_SUBJECT_MAX);
    }

    #[test]
    fn edit_line_handles_altgr_and_shortcuts() {
        let mut input = LineInput::default();
        edit_line(&mut input, &KeyEvent::new(KeyCode::Char('/'), KeyModifiers::CONTROL | KeyModifiers::ALT), true);
        edit_line(&mut input, &KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE), false);
        assert_eq!(input.text(), "/x");
        edit_line(&mut input, &KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL), true);
        assert!(input.is_empty());
    }
}
