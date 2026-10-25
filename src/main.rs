use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use orkai::cli::PwaAction;
use orkai::config::Config;
use orkai::launch::Launcher;
use orkai::model::{AgentId, AgentKind, DEFAULT_SESSION};
use orkai::paths::Home;
use orkai::pwa::tunnel::TunnelKind;
use orkai::store::Store;
use orkai::{cli, holder, hooks, pwa, tui, update};

#[derive(Parser)]
#[command(
    name = "orkai",
    version = update::VERSION.as_str(),
    about = "Orchestrate many AI coding agents across many projects from one terminal"
)]
struct Cli {
    #[arg(long, global = true, help = "Language: pt-br or en")]
    lang: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    #[command(about = "List agents with their live state", alias = "list")]
    Ls {
        #[arg(short, long, help = "Only agents of this session")]
        session: Option<String>,
    },
    #[command(about = "List sessions")]
    Sessions,
    #[command(about = "Start a new agent")]
    New {
        #[arg(
            short,
            long,
            default_value = "claude",
            help = "claude, codex, kilo, opencode, agy, grok, qwen, kimi or terminal"
        )]
        kind: String,
        #[arg(short, long, default_value = DEFAULT_SESSION)]
        session: String,
        #[arg(short = 'C', long, help = "Working directory (defaults to the current one)")]
        cwd: Option<PathBuf>,
        #[arg(short, long)]
        name: Option<String>,
        #[arg(short, long, help = "Run the agent in its own git worktree and branch (orkai/<name>)")]
        worktree: bool,
        #[arg(trailing_var_arg = true, help = "Task for the agent (or command for terminals)")]
        task: Vec<String>,
    },
    #[command(about = "Attach this terminal to a running agent (ctrl+] detaches)")]
    Attach { id: AgentId },
    #[command(about = "Type text into an agent and press enter (orchestrate from scripts)")]
    Send {
        id: AgentId,
        #[arg(long, help = "Type the text without pressing enter")]
        no_enter: bool,
        #[arg(trailing_var_arg = true)]
        text: Vec<String>,
    },
    #[command(about = "Stop a running agent")]
    Kill { id: AgentId },
    #[command(about = "Remove a stopped agent and its recorded output", alias = "remove")]
    Rm { id: AgentId },
    #[command(about = "Relaunch interrupted agents in their folders, resuming their conversations")]
    Recover {
        #[arg(short, long, help = "Only this session (default: every session)")]
        session: Option<String>,
    },
    #[command(about = "Print an agent's recorded transcript")]
    Logs {
        id: AgentId,
        #[arg(short = 'n', long, help = "Only the last N lines")]
        lines: Option<usize>,
    },
    #[command(about = "Print the data directory")]
    Home,
    #[command(about = "Open your orkai on the phone, from anywhere: prints the QR code to pair it")]
    Pwa {
        #[arg(long, value_parser = tunnel_kind, help = "cloudflare (default, no account) or tailscale (your tailnet)")]
        tunnel: Option<TunnelKind>,
        #[arg(long, conflicts_with = "new_code", help = "Turn the orkai PWA off")]
        stop: bool,
        #[arg(long, help = "Forget the paired phones and show a new code")]
        new_code: bool,
    },
    #[command(about = "Download and install the latest orkai build now")]
    Update,
    #[command(name = "__holder", hide = true)]
    Holder { id: AgentId },
    #[command(name = "__hook", hide = true)]
    Hook { source: String, id: AgentId, payload: Option<String> },
    #[command(name = "__pwa", hide = true)]
    PwaBridge {
        #[arg(long, value_parser = tunnel_kind)]
        tunnel: TunnelKind,
        #[arg(long)]
        port: u16,
    },
}

fn tunnel_kind(text: &str) -> Result<TunnelKind, String> {
    TunnelKind::parse(text).ok_or_else(|| "use cloudflare or tailscale".to_string())
}

fn launcher(store: Store, language: orkai::i18n::Language) -> std::io::Result<Launcher> {
    let config = Config::load_or_init(store.home())?;
    let exe = std::env::current_exe()?;
    let mut l = Launcher::new(store, config, &exe)?;
    l.set_language(language);
    Ok(l)
}

fn run(cli: Cli) -> std::io::Result<()> {
    let home = Home::resolve()?;
    if let Some(Command::Holder { id }) = cli.command {
        return holder::run(&home, id);
    }
    if let Some(Command::Hook { source, id, payload }) = cli.command {
        let _ = hooks::record(&home, id, &source, payload.as_deref());
        return Ok(());
    }
    if let Some(Command::PwaBridge { tunnel, port }) = cli.command {
        return pwa::bridge::run(&home, tunnel, port);
    }
    let store = Store::open(home)?;
    let config = Config::load_or_init(store.home())?;
    let language = orkai::i18n::Language::resolve(cli.lang.as_deref(), config.language.as_deref());
    orkai::i18n::set_current_language(language);

    match cli.command {
        None => tui::run(launcher(store, language)?),
        Some(Command::Ls { session }) => cli::list(&store, session.as_deref()),
        Some(Command::Sessions) => cli::sessions(&store),
        Some(Command::New { kind, session, cwd, name, worktree, task }) => {
            let kind = AgentKind::parse(&kind).ok_or_else(|| {
                let msg = if language.is_pt() {
                    format!("o tipo deve ser um de {}", AgentKind::names())
                } else {
                    format!("kind must be one of {}", AgentKind::names())
                };
                std::io::Error::new(std::io::ErrorKind::InvalidInput, msg)
            })?;
            cli::new_agent(&launcher(store, language)?, kind, session, cwd, name, task.join(" "), worktree)
        }
        Some(Command::Attach { id }) => cli::attach(&store, id),
        Some(Command::Send { id, no_enter, text }) => cli::send(&store, id, &text.join(" "), !no_enter),
        Some(Command::Kill { id }) => cli::kill(&store, id),
        Some(Command::Rm { id }) => cli::remove(&store, id),
        Some(Command::Recover { session }) => cli::recover(&launcher(store, language)?, session.as_deref()),
        Some(Command::Logs { id, lines }) => cli::logs(&store, id, lines),
        Some(Command::Home) => {
            println!("{}", store.home().root().display());
            Ok(())
        }
        Some(Command::Update) => update::update_now(),
        Some(Command::Pwa { tunnel, stop, new_code }) => {
            let action = match (stop, new_code) {
                (true, _) => PwaAction::Stop,
                (false, true) => PwaAction::NewCode,
                (false, false) => PwaAction::Show,
            };
            cli::pwa(&launcher(store, language)?, tunnel, action)
        }
        Some(Command::Holder { .. }) | Some(Command::Hook { .. }) | Some(Command::PwaBridge { .. }) => Ok(()),
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if cli.command.is_none() {
        let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
        if let Some(code) = update::on_launch(&args) {
            return code;
        }
    }
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("orkai: {err}");
            ExitCode::FAILURE
        }
    }
}
