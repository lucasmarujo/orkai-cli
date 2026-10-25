use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use orkai::client::{self, Connection};
use orkai::fsutil::write_json_atomic;
use orkai::launch::{spawn_holder, wait_for_runtime};
use orkai::model::{AgentKind, LaunchSpec, NewAgent};
use orkai::paths::Home;
use orkai::protocol::{ClientMsg, HolderMsg, TermSize};
use orkai::store::Store;
use orkai::util::now_ms;

const TIMEOUT: Duration = Duration::from_secs(45);
const ROWS: u16 = 40;
const COLS: u16 = 140;
static COUNTER: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    dir: PathBuf,
    outer: Store,
    inner: Store,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for store in [&self.inner, &self.outer] {
            for record in store.list_agents().unwrap_or_default() {
                if let Ok(Some(runtime)) = store.read_runtime(record.id)
                    && let Ok((conn, _, _)) = client::connect(&runtime)
                {
                    let _ = conn.send(&ClientMsg::Kill);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(500));
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn fixture() -> Fixture {
    let dir = std::env::temp_dir().join(format!(
        "orkai-tui-e2e-{}-{}-{}",
        std::process::id(),
        now_ms(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let outer = Store::open(Home::at(dir.join("outer"))).unwrap();
    let inner = Store::open(Home::at(dir.join("inner"))).unwrap();
    std::fs::create_dir_all(dir.join("work")).unwrap();
    let (shell, args) = if cfg!(windows) { ("cmd.exe", vec!["/q"]) } else { ("/bin/sh", vec![]) };
    write_json_atomic(&inner.home().config_file(), &serde_json::json!({ "shell": shell, "shell_args": args, "language": "en" })).unwrap();
    Fixture { dir, outer, inner }
}

struct Session {
    conn: Connection,
    rx: Receiver<Option<HolderMsg>>,
    screen: vt100::Parser,
    title: String,
    raw: Vec<u8>,
}

fn start_tui(fx: &Fixture) -> Session {
    let record = fx
        .outer
        .create_agent(NewAgent {
            session: "default".into(),
            name: "tui".into(),
            kind: AgentKind::Terminal,
            cwd: fx.dir.join("work"),
            task: String::new(),
        })
        .unwrap();
    let spec = LaunchSpec {
        kind: AgentKind::Terminal,
        program: PathBuf::from(env!("CARGO_BIN_EXE_orkai")),
        args: Vec::new(),
        cwd: fx.dir.join("work"),
        env: vec![
            ("ORKAI_HOME".into(), fx.inner.home().root().to_string_lossy().into_owned()),
            ("TERM".into(), "xterm-256color".into()),
        ],
        env_remove: Vec::new(),
        initial_input: None,
        rows: ROWS,
        cols: COLS,
    };
    fx.outer.write_launch(record.id, &spec).unwrap();
    spawn_holder(fx.outer.home(), record.id, Path::new(env!("CARGO_BIN_EXE_orkai"))).unwrap();
    let runtime = wait_for_runtime(&fx.outer, record.id, TIMEOUT).unwrap();
    let (conn, reader, _) = client::connect(&runtime).unwrap();
    let (tx, rx) = mpsc::channel();
    client::spawn_reader(reader, move |msg| tx.send(msg).is_ok()).unwrap();
    conn.send(&ClientMsg::Attach(TermSize { rows: ROWS, cols: COLS })).unwrap();
    Session { conn, rx, screen: vt100::Parser::new(ROWS, COLS, 0), title: String::new(), raw: Vec::new() }
}

impl Session {
    fn pump(&mut self, wait: Duration) -> bool {
        match self.rx.recv_timeout(wait) {
            Ok(Some(HolderMsg::Snapshot(snapshot))) => {
                self.screen = vt100::Parser::new(snapshot.size.rows, snapshot.size.cols, 0);
                self.screen.process(&snapshot.data);
                true
            }
            Ok(Some(HolderMsg::Output(bytes))) => {
                self.screen.process(&bytes);
                self.raw.extend_from_slice(&bytes);
                true
            }
            Ok(Some(HolderMsg::Status(status))) => {
                self.title = status.title;
                true
            }
            Ok(Some(HolderMsg::Exited(_))) | Ok(None) => false,
            Ok(Some(_)) | Err(_) => true,
        }
    }

    fn wait_for_title(&mut self, expected: &str) {
        let deadline = Instant::now() + TIMEOUT;
        while self.title != expected {
            assert!(Instant::now() < deadline, "window title is {:?}, expected {expected:?}", self.title);
            assert!(self.pump(Duration::from_millis(100)), "tui exited before setting the window title");
        }
    }

    fn wait_for_raw(&mut self, needle: &[u8]) {
        let deadline = Instant::now() + TIMEOUT;
        while !self.raw.windows(needle.len()).any(|w| w == needle) {
            assert!(Instant::now() < deadline, "timed out waiting for {:?}", String::from_utf8_lossy(needle));
            assert!(self.pump(Duration::from_millis(100)), "tui exited while waiting for raw output");
        }
    }

    fn wait_for(&mut self, needle: &str) {
        let deadline = Instant::now() + TIMEOUT;
        while !self.screen.screen().contents().contains(needle) {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {needle:?}; screen:\n{}",
                self.screen.screen().contents()
            );
            if !self.pump(Duration::from_millis(100)) {
                panic!("tui exited while waiting for {needle:?}:\n{}", self.screen.screen().contents());
            }
        }
    }

    fn wait_for_exit(&mut self) {
        let deadline = Instant::now() + TIMEOUT;
        while self.pump(Duration::from_millis(100)) {
            assert!(Instant::now() < deadline, "tui did not exit");
        }
    }

    fn send(&self, bytes: &[u8]) {
        self.conn.send(&ClientMsg::Input(bytes.to_vec())).unwrap();
        std::thread::sleep(Duration::from_millis(150));
    }

    fn type_text(&self, text: &str) {
        for c in text.chars() {
            self.conn.send(&ClientMsg::Input(c.to_string().into_bytes())).unwrap();
            std::thread::sleep(Duration::from_millis(15));
        }
    }

    fn dump(&self, name: &str) {
        let Ok(dir) = std::env::var("ORKAI_RENDER_DUMP_DIR") else { return };
        let screen = self.screen.screen();
        let (rows, cols) = screen.size();
        let color = |c: vt100::Color| match c {
            vt100::Color::Rgb(r, g, b) => format!("\"#{r:02x}{g:02x}{b:02x}\""),
            vt100::Color::Default => "null".to_string(),
            vt100::Color::Idx(i) => format!("\"idx{i}\""),
        };
        let mut cells = Vec::new();
        for row in 0..rows {
            for col in 0..cols {
                let cell = screen.cell(row, col).unwrap();
                let symbol = if cell.is_wide_continuation() { "" } else { cell.contents() };
                let (fg, bg) =
                    if cell.inverse() { (cell.bgcolor(), cell.fgcolor()) } else { (cell.fgcolor(), cell.bgcolor()) };
                cells.push(format!(
                    "[{},{},{},{}]",
                    serde_json::to_string(symbol).unwrap(),
                    color(fg),
                    color(bg),
                    cell.bold()
                ));
            }
        }
        let json = format!("{{\"width\":{cols},\"height\":{rows},\"cells\":[{}]}}", cells.join(","));
        std::fs::write(Path::new(&dir).join(format!("{name}.json")), json).unwrap();
    }
}

fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn tui_keeps_pasted_line_breaks_and_deletes_running_agents() {
    let fx = fixture();
    let mut tui = start_tui(&fx);
    tui.wait_for("who claude");
    tui.send(b"\x1b[Z");
    tui.wait_for("who terminal");

    tui.send(b"echo first-line\recho second-line");
    tui.wait_for("echo second-line");
    std::thread::sleep(Duration::from_millis(500));
    assert!(fx.inner.list_agents().unwrap().is_empty(), "a pasted line break must not start an agent");
    tui.dump("multiline");

    tui.send(b"\r");
    tui.wait_for("echo-first-line");
    wait_until("the agent record", || !fx.inner.list_agents().unwrap().is_empty());
    let agents = fx.inner.list_agents().unwrap();
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].task, "echo first-line\necho second-line");
    let id = agents[0].id;
    wait_until("the agent to run", || fx.inner.read_runtime(id).ok().flatten().is_some());

    tui.send(b"\x1b[3~");
    tui.wait_for("delete agent");
    tui.dump("delete-popup");
    tui.send(b"y");
    wait_until("the agent to be removed", || fx.inner.list_agents().unwrap().is_empty());
    assert!(!fx.inner.home().agent_dir(id).exists());

    tui.send(b"\x03");
    tui.wait_for_exit();
}

#[test]
fn tui_launches_agent_opens_it_and_agent_survives_restart() {
    let fx = fixture();
    let mut tui = start_tui(&fx);
    tui.wait_for("write a task");
    tui.wait_for("who claude");
    tui.wait_for_title(orkai::tui::WINDOW_TITLE);

    tui.send(b"\x1b[Z");
    tui.wait_for("who terminal");

    tui.type_text("echo orkai-tui-marker");
    tui.send(b"\r");
    tui.wait_for("$ echo-orkai-tui");
    tui.wait_for("echo orkai-tui-marker");
    tui.dump("overview");

    tui.send(b"\r");
    tui.wait_for("ctrl+] overview");
    tui.wait_for("orkai-tui-marker");
    tui.dump("agent");

    tui.send(b"\x1d");
    tui.wait_for("write a task");

    tui.send(b"\x03");
    tui.wait_for_exit();

    let agents = fx.inner.list_agents().unwrap();
    assert_eq!(agents.len(), 1);
    let agent = &agents[0];
    assert_eq!(agent.name, "echo-orkai-tui");
    let runtime = fx.inner.read_runtime(agent.id).unwrap().expect("agent must survive the tui exiting");
    let (conn, _reader, _) = client::connect(&runtime).expect("agent holder must still accept connections");
    drop(conn);

    let mut again = start_tui(&fx);
    again.wait_for("echo-orkai-tui");
    again.wait_for("$ echo-orkai-tui");
    again.dump("restarted");
    again.send(b"\x03");
    again.wait_for_exit();
}

#[test]
fn ctrl_up_recalls_the_tasks_sent_in_this_session() {
    let fx = fixture();
    let mut tui = start_tui(&fx);
    tui.wait_for("who claude");
    tui.send(b"\x1b[Z");
    tui.wait_for("who terminal");

    tui.type_text("echo orkai-recall");
    tui.send(b"\r");
    wait_until("the first agent", || fx.inner.list_agents().unwrap().len() == 1);

    tui.send(b"\x1b[1;5A");
    tui.send(b"\r");
    wait_until("the second agent", || fx.inner.list_agents().unwrap().len() == 2);
    let agents = fx.inner.list_agents().unwrap();
    assert_eq!(agents[0].task, "echo orkai-recall");
    assert_eq!(agents[1].task, "echo orkai-recall");

    tui.send(b"\x03");
    tui.wait_for_exit();

    let mut again = start_tui(&fx);
    again.wait_for("write a task");
    again.send(b"\x1b[1;5A");
    again.send(b"\r");
    std::thread::sleep(Duration::from_millis(1000));
    assert_eq!(fx.inner.list_agents().unwrap().len(), 2, "a fresh orkai must not recall the previous tasks");
    again.send(b"\x1d");
    again.send(b"\x03");
    again.wait_for_exit();
}

#[test]
fn ctrl_arrows_and_clicks_move_the_cursor_in_the_task() {
    let fx = fixture();
    let mut tui = start_tui(&fx);
    tui.wait_for("who claude");

    tui.type_text("fix the login bug");
    tui.wait_for("fix the login bug");
    for _ in 0..2 {
        tui.send(b"\x1b[1;5D");
    }
    tui.type_text("X");
    tui.wait_for("fix the Xlogin bug");
    tui.dump("ctrl-arrow-left");

    tui.send(b"\x1b[1;5C");
    tui.type_text("Y");
    tui.wait_for("fix the XloginY bug");
    tui.dump("ctrl-arrow-right");

    tui.send(b"\x1b[<0;6;38M");
    tui.type_text("Z");
    tui.wait_for("fix Zthe XloginY bug");
    tui.dump("click-in-the-task");

    tui.send(b"\x15");
    tui.send(b"\x03");
    tui.wait_for_exit();
}

fn fake_agent(dir: &Path) -> PathBuf {
    if cfg!(windows) {
        let script = dir.join("fake-claude.cmd");
        let body = [
            "@echo off",
            "if \"%1\"==\"--version\" exit /b 0",
            "for /l %%i in (1,1,60) do echo.",
            "echo working (esc to interrupt)",
            "ping -n 6 127.0.0.1 >nul",
            "cls",
            "echo all done",
            "ping -n 120 127.0.0.1 >nul",
        ];
        std::fs::write(&script, body.join("\r\n")).unwrap();
        script
    } else {
        let script = dir.join("fake-claude.sh");
        let body = [
            "#!/bin/sh",
            "[ \"$1\" = --version ] && exit 0",
            "i=0; while [ $i -lt 60 ]; do echo; i=$((i+1)); done",
            "echo 'working (esc to interrupt)'",
            "sleep 5",
            "printf '\\033[2J\\033[H'",
            "echo 'all done'",
            "sleep 120",
        ];
        std::fs::write(&script, body.join("\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        script
    }
}

#[test]
fn tui_rings_and_retitles_when_an_agent_finishes_out_of_sight() {
    let fx = fixture();
    let fake = fake_agent(&fx.dir);
    let (shell, args) = if cfg!(windows) { ("cmd.exe", vec!["/q"]) } else { ("/bin/sh", vec![]) };
    write_json_atomic(
        &fx.inner.home().config_file(),
        &serde_json::json!({ "shell": shell, "shell_args": args, "claude_command": fake, "notify_sound": false, "language": "en" }),
    )
    .unwrap();
    let mut tui = start_tui(&fx);
    tui.wait_for("who claude");
    tui.wait_for_title(orkai::tui::WINDOW_TITLE);

    tui.type_text("finish quietly");
    tui.send(b"\r");
    tui.wait_for("finish-quietly");
    tui.wait_for_raw(b"\x07\x1b]9;orkai: finish-quietly finished\x07");
    tui.wait_for_title(&format!("✔1 · {}", orkai::tui::WINDOW_TITLE));
    tui.wait_for("finish-quietly finished · alt+n opens it");

    tui.send(b"\x1bn");
    tui.wait_for("ctrl+] overview");
    tui.wait_for_title(orkai::tui::WINDOW_TITLE);
    tui.send(b"\x1d");
    tui.send(b"\x03");
    tui.wait_for_exit();
}

#[test]
fn tui_runs_in_portuguese_mode() {
    let fx = fixture();
    let (shell, args) = if cfg!(windows) { ("cmd.exe", vec!["/q"]) } else { ("/bin/sh", vec![]) };
    write_json_atomic(
        &fx.inner.home().config_file(),
        &serde_json::json!({ "shell": shell, "shell_args": args, "language": "pt-BR" }),
    )
    .unwrap();
    let mut tui = start_tui(&fx);
    tui.wait_for("quem claude");
    tui.wait_for("escreva uma tarefa");
    tui.wait_for("f1 ajuda");
    tui.send(b"\x1b[Z");
    tui.wait_for("quem terminal");
    tui.send(b"\x03");
    tui.wait_for_exit();
}
