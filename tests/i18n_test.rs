use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use orkai::config::Config;
use orkai::i18n::{self, Language};
use orkai::launch::Launcher;
use orkai::model::{AgentKind, AgentState, ExitInfo, NewAgent, StatusReport};
use orkai::paths::Home;
use orkai::store::Store;
use orkai::tui::app::{App, AppEvent, Confirm, Link, Prompt, View};
use orkai::tui::render;
use orkai::usage::Problem;
use orkai::util::now_ms;

const WIDTH: u16 = 150;
const HEIGHT: u16 = 40;
static COUNTER: AtomicU64 = AtomicU64::new(0);

struct Scenario {
    app: App,
    #[allow(dead_code)]
    events: mpsc::Receiver<AppEvent>,
    dir: PathBuf,
}

impl Drop for Scenario {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn pt_scenario() -> Scenario {
    let dir = std::env::temp_dir().join(format!(
        "orkai-i18n-test-{}-{}-{}",
        std::process::id(),
        now_ms(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let store = Store::open(Home::at(dir.join("home"))).unwrap();
    store.create_session("sessao-2").unwrap();

    let cwd = dir.join("projeto/web");
    std::fs::create_dir_all(&cwd).unwrap();
    let record = store
        .create_agent(NewAgent {
            session: "default".into(),
            name: "agente-teste".into(),
            kind: AgentKind::Claude,
            cwd,
            task: "tarefa de teste".into(),
        })
        .unwrap();

    let id = record.id;
    let config = Config { language: Some("pt-BR".into()), ..Default::default() };

    let mut launcher = Launcher::new(store, config, &std::env::current_exe().unwrap()).unwrap();
    launcher.set_language(Language::PtBr);

    let (tx, events) = mpsc::channel();
    let mut app = App::new(launcher, tx, Rect::new(0, 0, WIDTH, HEIGHT)).unwrap();
    app.language = Language::PtBr;
    app.tabs.push(id);

    let entry = app.agents.get_mut(&id).unwrap();
    entry.link = Link::Connecting(0);
    entry.status = Some(StatusReport {
        state: AgentState::Working,
        state_since_ms: now_ms() - 10_000,
        turn_started_ms: Some(now_ms() - 5_000),
        last_turn_ms: None,
        started_at_ms: now_ms() - 60_000,
        title: "Claude".into(),
        activity: "Compilando código".into(),
        rows: 24,
        cols: 80,
        exit: None,
        nested: None,
        viewers: None,
    });
    app.selected = Some(id);

    Scenario { app, events, dir }
}

fn buffer_text(buf: &Buffer) -> String {
    let mut text = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            text.push_str(buf[(x, y)].symbol());
        }
        text.push('\n');
    }
    text
}

#[test]
fn test_language_parsing() {
    assert_eq!(Language::parse("pt"), Some(Language::PtBr));
    assert_eq!(Language::parse("PT"), Some(Language::PtBr));
    assert_eq!(Language::parse("pt-br"), Some(Language::PtBr));
    assert_eq!(Language::parse("pt-BR"), Some(Language::PtBr));
    assert_eq!(Language::parse("pt_BR"), Some(Language::PtBr));
    assert_eq!(Language::parse("pt-pt"), Some(Language::PtBr));

    assert_eq!(Language::parse("en"), Some(Language::En));
    assert_eq!(Language::parse("EN"), Some(Language::En));
    assert_eq!(Language::parse("en-us"), Some(Language::En));
    assert_eq!(Language::parse("en-US"), Some(Language::En));
    assert_eq!(Language::parse("en_US"), Some(Language::En));
    assert_eq!(Language::parse("en-gb"), Some(Language::En));

    assert_eq!(Language::parse("es"), None);
    assert_eq!(Language::parse("de"), None);
    assert_eq!(Language::parse(""), None);
    assert_eq!(Language::parse("   "), None);
}

#[test]
fn test_language_resolution_precedence() {
    // 1. CLI arg wins over config
    assert_eq!(Language::resolve(Some("pt-br"), Some("en")), Language::PtBr);
    assert_eq!(Language::resolve(Some("en"), Some("pt-BR")), Language::En);

    // 2. Config wins when CLI is absent or invalid
    assert_eq!(Language::resolve(None, Some("pt-BR")), Language::PtBr);
    assert_eq!(Language::resolve(Some("invalid"), Some("pt-BR")), Language::PtBr);
    assert_eq!(Language::resolve(None, Some("en")), Language::En);
    assert_eq!(Language::resolve(Some("unknown"), Some("en-US")), Language::En);

    // 3. Fallback when both are absent or invalid
    let resolved = Language::resolve(None, None);
    assert!(resolved == Language::En || resolved == Language::PtBr);
}

#[test]
fn test_agent_state_display_labels() {
    assert_eq!(AgentState::Idle.display_label(Language::En), "idle");
    assert_eq!(AgentState::Working.display_label(Language::En), "working");
    assert_eq!(AgentState::Paused.display_label(Language::En), "paused");
    assert_eq!(AgentState::Done.display_label(Language::En), "done");
    assert_eq!(AgentState::Exited.display_label(Language::En), "exited");
    assert_eq!(AgentState::Lost.display_label(Language::En), "lost");

    assert_eq!(AgentState::Idle.display_label(Language::PtBr), "ocioso");
    assert_eq!(AgentState::Working.display_label(Language::PtBr), "ativo");
    assert_eq!(AgentState::Paused.display_label(Language::PtBr), "pausado");
    assert_eq!(AgentState::Done.display_label(Language::PtBr), "pronto");
    assert_eq!(AgentState::Exited.display_label(Language::PtBr), "parado");
    assert_eq!(AgentState::Lost.display_label(Language::PtBr), "perdido");

    // Ensure all Portuguese state labels fit in the standard 8-character column width
    for state in [
        AgentState::Idle,
        AgentState::Working,
        AgentState::Paused,
        AgentState::Done,
        AgentState::Exited,
        AgentState::Lost,
    ] {
        assert!(state.display_label(Language::PtBr).len() <= 8);
    }
}

#[test]
fn test_exit_info_describe_lang() {
    let failed = ExitInfo { code: Some(1), error: Some("crash".into()), at_ms: 1000 };
    assert_eq!(failed.describe_lang(Language::En), "failed: crash");
    assert_eq!(failed.describe_lang(Language::PtBr), "falhou: crash");

    let code = ExitInfo { code: Some(0), error: None, at_ms: 1000 };
    assert_eq!(code.describe_lang(Language::En), "exited with code 0");
    assert_eq!(code.describe_lang(Language::PtBr), "encerrou com código 0");

    let clean = ExitInfo { code: None, error: None, at_ms: 1000 };
    assert_eq!(clean.describe_lang(Language::En), "exited");
    assert_eq!(clean.describe_lang(Language::PtBr), "encerrou");
}

#[test]
fn test_problem_display_labels() {
    assert_eq!(Problem::SignedOut.display_label(Language::En), "not signed in");
    assert_eq!(Problem::Expired.display_label(Language::En), "login expired");
    assert_eq!(Problem::ApiKey.display_label(Language::En), "api key · no plan limits");
    assert_eq!(Problem::Unavailable.display_label(Language::En), "unavailable");

    assert_eq!(Problem::SignedOut.display_label(Language::PtBr), "não conectado");
    assert_eq!(Problem::Expired.display_label(Language::PtBr), "sessão expirada");
    assert_eq!(Problem::ApiKey.display_label(Language::PtBr), "chave api · sem limites de plano");
    assert_eq!(Problem::Unavailable.display_label(Language::PtBr), "indisponível");
}

#[test]
fn test_help_items_structure() {
    assert_eq!(i18n::HELP_EN.len(), i18n::HELP_PT_BR.len());
    assert_eq!(i18n::HELP_EN.len(), 24);

    assert_eq!(i18n::help_title(Language::En), " orkai · keys ");
    assert_eq!(i18n::help_title(Language::PtBr), " orkai · atalhos ");

    for ((en_k, en_v), (pt_k, pt_v)) in i18n::HELP_EN.iter().zip(i18n::HELP_PT_BR.iter()) {
        assert!(!en_k.is_empty());
        assert!(!en_v.is_empty());
        assert!(!pt_k.is_empty());
        assert!(!pt_v.is_empty());
    }
}

#[test]
fn test_pt_br_tui_overview_rendering() {
    let mut scenario = pt_scenario();
    let backend = TestBackend::new(WIDTH, HEIGHT);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal.draw(|f| render::draw(f, &mut scenario.app)).unwrap();
    let text = buffer_text(terminal.backend().buffer());

    // Sidebar labels
    assert!(text.contains("+ nova sessão"), "Sidebar should show '+ nova sessão'");
    assert!(text.contains("f1 ajuda"), "Sidebar should show 'f1 ajuda'");

    // Overview tab
    assert!(text.contains("visão geral"), "Overview tab should show 'visão geral'");

    // Stats
    assert!(text.contains("agentes"), "Stats should show 'agentes'");
    assert!(text.contains("trabalhando"), "Stats should show 'trabalhando'");
    assert!(text.contains("pausados"), "Stats should show 'pausados'");
    assert!(text.contains("concluídos"), "Stats should show 'concluídos'");

    // Bottom hints
    assert!(text.contains("f1 ajuda"), "Bottom hint should include 'f1 ajuda'");
    assert!(text.contains("selecionar"), "Bottom hint should include 'selecionar'");

    // Composer row
    assert!(text.contains("quem "), "Composer should show 'quem '");
    assert!(text.contains("worktree "), "Composer should show 'worktree '");
    assert!(text.contains("onde "), "Composer should show 'onde '");
}

#[test]
fn test_pt_br_confirmation_dialog() {
    let mut scenario = pt_scenario();
    let backend = TestBackend::new(WIDTH, HEIGHT);
    let mut terminal = Terminal::new(backend).unwrap();

    let id = *scenario.app.agents.keys().next().unwrap();
    scenario.app.prompt = Some(Prompt::Confirm(Confirm::Delete(id)));

    terminal.draw(|f| render::draw(f, &mut scenario.app)).unwrap();
    let text = buffer_text(terminal.backend().buffer());

    assert!(text.contains("excluir agente"), "Popup title should be 'excluir agente'");
    assert!(text.contains("o processo será parado e removido"), "Confirmation text should explain process stop");
    assert!(text.contains("y / enter confirma"), "Hint should mention 'y / enter confirma'");
    assert!(text.contains("qualquer outra tecla cancela"), "Hint should mention 'qualquer outra tecla cancela'");
}

#[test]
fn test_pt_br_agent_status_bar() {
    let mut scenario = pt_scenario();
    let id = *scenario.app.agents.keys().next().unwrap();
    scenario.app.view = View::Agent(id);

    let backend = TestBackend::new(WIDTH, HEIGHT);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal.draw(|f| render::draw(f, &mut scenario.app)).unwrap();
    let text = buffer_text(terminal.backend().buffer());

    assert!(text.contains("ctrl+] visão geral"), "Status bar should show 'ctrl+] visão geral'");
    assert!(text.contains("shift+pgup rolar"), "Status bar should show 'shift+pgup rolar'");
    assert!(text.contains("ativo"), "Agent status label should be 'ativo'");
}

#[test]
fn test_pt_br_pwa_view_rendering() {
    let mut scenario = pt_scenario();
    scenario.app.view = View::Pwa;
    scenario.app.pwa.enabled = false;

    let backend = TestBackend::new(WIDTH, HEIGHT);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal.draw(|f| render::draw(f, &mut scenario.app)).unwrap();
    let text = buffer_text(terminal.backend().buffer());

    assert!(text.contains("o orkai pwa está desligado"));
    assert!(text.contains("pressione enter para ligar"));
    assert!(text.contains("desligado"));
    assert!(text.contains("enter liga · esc voltar"));
}

#[test]
fn test_posix_utf8_locale_parsing() {
    assert_eq!(Language::parse_system_locale_string("C.UTF-8"), Some(Language::En));
    assert_eq!(Language::parse_system_locale_string("c.utf-8"), Some(Language::En));
    assert_eq!(Language::parse_system_locale_string("c.utf8"), Some(Language::En));
    assert_eq!(Language::parse_system_locale_string("POSIX.UTF-8"), Some(Language::En));
    assert_eq!(Language::parse_system_locale_string("posix.utf-8"), Some(Language::En));
    assert_eq!(Language::parse_system_locale_string("pt_BR.utf8"), Some(Language::PtBr));
    assert_eq!(Language::parse_system_locale_string("pt_PT.ISO8859-1"), Some(Language::PtBr));
    assert_eq!(Language::parse_system_locale_string(""), None);
    assert_eq!(Language::parse_system_locale_string("   "), None);
}

#[test]
fn test_layout_preservation_small_screen_pt_br() {
    let mut scenario = pt_scenario();
    // Test a constrained terminal (80x24)
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();

    // Must not panic or wrap beyond bounds
    terminal.draw(|f| render::draw(f, &mut scenario.app)).unwrap();
    let buf = terminal.backend().buffer();
    assert_eq!(buf.area.width, 80);
    assert_eq!(buf.area.height, 24);

    let text = buffer_text(buf);
    assert!(text.contains("visão geral"));
    assert!(text.contains("agentes"));
}

