use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Widget, Wrap};

use crate::model::{AgentId, AgentKind, AgentState};
use crate::pwa::bridge::Phase;
use crate::pwa::qr::Qr;
use crate::tui::app::{AgentEntry, App, Confirm, Hit, Link, Prompt, PwaPanel, SHELL_PREFIX, Tone, View};
use crate::tui::complete::matching_commands;
use crate::tui::format::{duration_short, pad, pad_left, skip_columns, split_columns, truncate, truncate_start, width};
use crate::tui::sprite;
use crate::tui::theme;
use crate::usage::{self, Limits, Usage, Window};
use crate::util::now_ms;
use crate::worktree::Changes;

pub const SIDEBAR_WIDTH: u16 = 24;
pub const SIDEBAR_MIN_SCREEN: u16 = 72;
const MAIN_GAP: u16 = 2;
const NAME_MIN: usize = 14;
const NAME_MAX: usize = 30;
const TIME_WIDTH: usize = 6;
const KIND_WIDTH: usize = 9;
const SUBAGENT_LABEL: &str = "subagent";
const SUBAGENT_INDENT: u16 = 6;
const TAB_NAME_MAX: usize = 18;
pub const MAX_COMPOSER_ROWS: u16 = 6;
const HINT: &str = "↑↓ select · enter open · tab who · ^l where · ^n session · ^e rename · del delete · f1 help";
const CONFIRM_WIDTH: u16 = 64;
const CONFIRM_HEIGHT: u16 = 8;
const CONFIRM_HINT: &str = "y / enter confirm  ·  any other key cancels";
const USAGE_GAP: u16 = 4;
const USAGE_NAME_WIDTH: u16 = 8;
const USAGE_WINDOW_WIDTH: usize = 6;
const USAGE_BAR_WIDTH: usize = 12;
const USAGE_BAR_MIN_WIDTH: usize = 6;
const USAGE_PERCENT_WIDTH: usize = 4;
const USAGE_RESET_WIDTH: usize = 13;
const USAGE_NAME_GAP: usize = 2;
const USAGE_ROWS: u16 = 2;
const USAGE_PER_COLUMN: u16 = 3;
const USAGE_COLUMN_GAP: u16 = 4;
pub const USAGE_SESSION_LABEL: &str = "5h";
pub const USAGE_WEEKLY_LABEL: &str = "week";
pub const USAGE_MONTHLY_LABEL: &str = "month";
const USAGE_CREDITS_LABEL: &str = "funds";
const BRANCH_GLYPH: &str = "⎇";
const DIFF_HINT: &str = "esc close · ↑↓ pgup pgdn scroll · /merge merges it";
const BASE_SHORT: usize = 8;

pub struct Areas {
    pub sidebar: Option<Rect>,
    pub divider: Option<Rect>,
    pub tabs: Option<Rect>,
    pub body: Rect,
    pub rule: Rect,
    pub bottom: Rect,
}

pub fn areas(screen: Rect, agent_view: bool, has_tabs: bool, input_rows: u16) -> Areas {
    let bottom_height = (if agent_view { 1 } else { 2 + input_rows.clamp(1, MAX_COMPOSER_ROWS) }).min(screen.height);
    let rule_height = u16::from(screen.height > bottom_height);
    let top_height = screen.height.saturating_sub(bottom_height + rule_height);
    let top = Rect::new(screen.x, screen.y, screen.width, top_height);
    let rule = Rect::new(screen.x, screen.y + top_height, screen.width, rule_height);
    let bottom = Rect::new(screen.x, rule.y + rule_height, screen.width, bottom_height);
    let (sidebar, divider, main) = if screen.width >= SIDEBAR_MIN_SCREEN {
        let sidebar = Rect::new(top.x, top.y, SIDEBAR_WIDTH, top.height);
        let divider = Rect::new(top.x + SIDEBAR_WIDTH, top.y, 1, top.height);
        let main_x = divider.x + MAIN_GAP;
        let main = Rect::new(main_x, top.y, screen.right().saturating_sub(main_x), top.height);
        (Some(sidebar), Some(divider), main)
    } else {
        (None, None, Rect::new(top.x + 1, top.y, top.width.saturating_sub(1), top.height))
    };
    let tabs = (has_tabs && main.height > 1).then(|| Rect::new(main.x, main.y, main.width, 1));
    let body = match tabs {
        Some(_) => Rect::new(main.x, main.y + 1, main.width, main.height - 1),
        None => main,
    };
    Areas { sidebar, divider, tabs, body, rule, bottom }
}

fn put(buf: &mut Buffer, x: u16, y: u16, text: &str, style: Style, limit: u16) -> u16 {
    if x >= limit {
        return x;
    }
    let (end, _) = buf.set_stringn(x, y, text, usize::from(limit - x), style);
    end
}

fn fg(color: Color) -> Style {
    Style::default().fg(color)
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    app.hits.clear();
    let screen = frame.area();
    let agent_view = matches!(app.view, View::Agent(_));
    let layout = areas(screen, agent_view, agent_view || !app.tabs.is_empty(), composer_rows(app));
    let cursor = {
        let buf = frame.buffer_mut();
        if let Some(sidebar) = layout.sidebar {
            draw_sidebar(app, sidebar, buf);
        }
        if let Some(divider) = layout.divider {
            for y in divider.top()..divider.bottom() {
                put(buf, divider.x, y, "│", fg(theme::RULE), divider.right());
            }
        }
        if layout.rule.height > 0 {
            let line = "─".repeat(usize::from(layout.rule.width));
            put(buf, layout.rule.x, layout.rule.y, &line, fg(theme::RULE), layout.rule.right());
            if let Some(divider) = layout.divider {
                put(buf, divider.x, layout.rule.y, "┴", fg(theme::RULE), divider.right());
            }
        }
        if let Some(tabs) = layout.tabs {
            draw_tabs(app, tabs, buf);
        }
        let cursor = match app.view {
            View::Overview => {
                if app.diff.is_some() {
                    draw_diff(app, layout.body, buf);
                } else {
                    draw_overview(app, layout.body, buf);
                }
                draw_composer(app, layout.bottom, buf)
            }
            View::Agent(id) => {
                let cursor = draw_agent(app, id, layout.body, buf);
                draw_agent_status(app, id, layout.bottom, buf);
                cursor
            }
            View::Pwa => {
                draw_pwa(app, layout.body, buf);
                draw_pwa_status(app, layout.bottom, buf);
                None
            }
        };
        if app.show_help {
            draw_help(app, screen, buf);
            None
        } else if let Some(Prompt::Confirm(confirm)) = app.prompt.as_ref()
            && confirm.is_popup()
        {
            draw_confirm(app, confirm, screen, buf);
            None
        } else {
            cursor
        }
    };
    if let Some(position) = cursor {
        frame.set_cursor_position(position);
    }
}

fn draw_sidebar(app: &mut App, area: Rect, buf: &mut Buffer) {
    let x = area.x + 1;
    let right = area.right().saturating_sub(1);
    let mut y = area.y;
    if area.height == 0 {
        return;
    }
    put(buf, x, y, &app.hostname, fg(theme::TEXT).add_modifier(Modifier::BOLD), right);
    y += 1;
    let sessions = app.sessions.clone();
    for session in &sessions {
        if y >= area.bottom() {
            return;
        }
        let selected = session.name == app.session;
        let count = app.session_count(&session.name).to_string();
        let (marker, style) =
            if selected { ("› ", fg(theme::ACCENT).add_modifier(Modifier::BOLD)) } else { ("  ", fg(theme::TEXT)) };
        let count_x = right.saturating_sub(u16::try_from(count.len()).unwrap_or(0));
        put(buf, x, y, marker, style, right);
        put(buf, x + 2, y, &truncate(&session.name, usize::from(count_x.saturating_sub(x + 3))), style, count_x);
        put(buf, count_x, y, &count, if selected { style } else { fg(theme::MUTED) }, right + 1);
        app.hits.push((Rect::new(area.x, y, area.width, 1), Hit::Session(session.name.clone())));
        y += 1;
    }
    if y < area.bottom() {
        put(buf, x + 2, y, app.t("+ new session", "+ nova sessão"), fg(theme::LINK), right);
        app.hits.push((Rect::new(area.x, y, area.width, 1), Hit::NewSession));
        y += 2;
    }
    if y < area.bottom() {
        let (badge, color) = pwa_badge(app, &app.pwa);
        let label = if app.pwa.enabled { theme::TEXT } else { theme::LINK };
        put(buf, x, y, PWA_LABEL, fg(label), right);
        let badge_x = right.saturating_sub(u16::try_from(width(&badge)).unwrap_or(0));
        put(buf, badge_x, y, &badge, fg(color), right + 1);
        app.hits.push((Rect::new(area.x, y, area.width, 1), Hit::Pwa));
        y += 2;
    }
    let lost = app.lost_in_session().len();
    if lost > 0 && y + 1 < area.bottom() {
        put(
            buf,
            x,
            y,
            &format!("↻ {lost} {}", app.t("interrupted", "interrompido(s)")),
            fg(theme::RED).add_modifier(Modifier::BOLD),
            right,
        );
        put(buf, x, y + 1, app.t("  ctrl+r to recover", "  ctrl+r para recuperar"), fg(theme::MUTED), right);
        app.hits.push((Rect::new(area.x, y, area.width, 2), Hit::Recover));
    }
    if app.launching > 0 && area.height > 2 {
        let text = format!("{} {}…", app.t("starting", "iniciando"), app.launching);
        put(buf, x, area.bottom() - 2, &text, fg(theme::YELLOW), right);
    }
    if area.height > 1 {
        put(buf, x, area.bottom() - 1, app.t("f1 help", "f1 ajuda"), fg(theme::FAINT), right);
    }
}

fn tab_label(entry: &AgentEntry, tick: usize) -> (String, Color) {
    let (glyph, color) = theme::glyph(entry.kind(), entry.state(), tick);
    (format!("{glyph} {}", truncate(&entry.record.name, TAB_NAME_MAX)), color)
}

fn draw_tabs(app: &mut App, area: Rect, buf: &mut Buffer) {
    let right = area.right();
    let mut x = area.x;
    let overview_active = app.view == View::Overview;
    let style = if overview_active {
        fg(theme::TEXT).bg(theme::TAB_ACTIVE_BG).add_modifier(Modifier::BOLD)
    } else {
        fg(theme::MUTED)
    };
    let start = x;
    x = put(buf, x, area.y, app.t(" ◆ overview ", " ◆ visão geral "), style, right);
    app.hits.push((Rect::new(start, area.y, x - start, 1), Hit::Overview));
    let tabs = app.tabs.clone();
    for (index, id) in tabs.iter().enumerate() {
        let Some(entry) = app.agents.get(id) else { continue };
        if x + 4 >= right {
            break;
        }
        x = put(buf, x, area.y, "│", fg(theme::RULE), right);
        let active = app.view == View::Agent(*id);
        let (label, glyph_color) = tab_label(entry, app.tick);
        let base = if active {
            Style::default().bg(theme::TAB_ACTIVE_BG).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let start = x;
        x = put(buf, x, area.y, " ", base, right);
        let mut chars = label.chars();
        let glyph: String = chars.next().map(String::from).unwrap_or_default();
        x = put(buf, x, area.y, &glyph, base.fg(glyph_color), right);
        let rest: String = chars.collect();
        let name_color = if active { theme::TEXT } else { theme::MUTED };
        x = put(buf, x, area.y, &rest, base.fg(name_color), right);
        let hint = if index < 9 { format!(" {}", index + 1) } else { String::new() };
        x = put(buf, x, area.y, &hint, base.fg(theme::FAINT), right);
        app.hits.push((Rect::new(start, area.y, x.saturating_sub(start), 1), Hit::Tab(*id)));
        let close_start = x;
        x = put(buf, x, area.y, " × ", base.fg(theme::DIM), right);
        app.hits.push((Rect::new(close_start, area.y, x.saturating_sub(close_start), 1), Hit::CloseTab(*id)));
    }
}

fn draw_overview(app: &mut App, area: Rect, buf: &mut Buffer) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let sprite_height = sprite::HEIGHT.min(area.height);
    sprite::render(
        Rect::new(area.x, area.y, sprite::WIDTH.min(area.width), sprite_height),
        buf,
        app.any_working(),
        app.tick / 3,
    );
    let stats = app.stats();
    let stats_x = area.x + sprite::WIDTH + 2;
    let right = area.right();
    let rows = [
        (stats.agents, app.t("agents", "agentes"), theme::PAPER, theme::INK),
        (stats.working, app.t("working", "trabalhando"), theme::YELLOW, theme::INK),
        (stats.paused, app.t("paused", "pausados"), theme::RED, theme::PAPER),
        (stats.done, app.t("done", "concluídos"), theme::GREEN, theme::INK),
    ];
    let first = area.y + sprite_height.saturating_sub(4);
    let mut stats_right = stats_x;
    for (i, (value, label, bg, ink)) in rows.iter().enumerate() {
        let y = first + u16::try_from(i).unwrap_or(0);
        if y < area.bottom() {
            let boxed = format!("{value:>3} ");
            let box_style = Style::default().fg(*ink).bg(*bg).add_modifier(Modifier::BOLD);
            let after = put(buf, stats_x, y, &boxed, box_style, right);
            stats_right = stats_right.max(put(buf, after + 1, y, label, fg(theme::MUTED), right));
        }
    }
    let usage_x = stats_right + USAGE_GAP;
    let grid = usage_grid(app, usage_x, right);
    let header_height = sprite_height.max(1 + USAGE_ROWS * grid.rows).min(area.height);
    draw_usage(app, &grid, usage_x, first, Rect::new(area.x, area.y, area.width, header_height), buf);
    let list_top = area.y + header_height + 1;
    if list_top < area.bottom() {
        draw_agent_list(app, Rect::new(area.x, list_top, area.width, area.bottom() - list_top), buf);
    }
}

fn usage_cells(used_percent: f64, cells: usize) -> usize {
    let share = used_percent.clamp(0.0, 100.0) / 100.0;
    ((share * cells as f64).round() as usize).min(cells)
}

#[allow(clippy::too_many_arguments)]
fn draw_usage_window(
    app: &App,
    window: &Window,
    bar: usize,
    now: u64,
    x: u16,
    y: u16,
    right: u16,
    buf: &mut Buffer,
) -> u16 {
    let color = theme::usage_color(window.used_percent);
    let filled = usage_cells(window.used_percent, bar);
    let mut x = put(buf, x, y, &"━".repeat(filled), fg(color), right);
    x = put(buf, x, y, &"─".repeat(bar - filled), fg(theme::RULE), right);
    let percent = pad_left(&format!("{:.0}%", window.used_percent), USAGE_PERCENT_WIDTH);
    x = put(buf, x + 1, y, &percent, fg(color).add_modifier(Modifier::BOLD), right);
    if let Some(resets_at) = window.resets_at_ms {
        let text = if resets_at > now {
            if app.language.is_pt() {
                format!("reinicia em {}", duration_short(resets_at - now))
            } else {
                format!("resets {}", duration_short(resets_at - now))
            }
        } else {
            app.t("resets now", "reinicia agora").to_string()
        };
        x = put(buf, x + 2, y, &text, fg(theme::DIM), right);
    }
    x
}

struct UsageGrid {
    kinds: Vec<AgentKind>,
    columns: u16,
    rows: u16,
    name_width: u16,
    column_width: u16,
    bar_width: usize,
}

fn usage_column_width(name_width: u16, bar: usize) -> u16 {
    let windows = USAGE_WINDOW_WIDTH + bar + 1 + USAGE_PERCENT_WIDTH + 2 + USAGE_RESET_WIDTH;
    name_width.saturating_add(u16::try_from(windows).unwrap_or(u16::MAX))
}

fn usage_grid(app: &App, x: u16, right: u16) -> UsageGrid {
    let kinds: Vec<AgentKind> =
        usage::PROVIDERS.iter().copied().filter(|kind| app.usage.get(kind).is_some_and(Usage::is_signed_in)).collect();
    let longest = kinds.iter().map(|kind| width(kind.label()) + USAGE_NAME_GAP).max().unwrap_or(0);
    let name_width = u16::try_from(longest).unwrap_or(u16::MAX).max(USAGE_NAME_WIDTH);
    let count = u16::try_from(kinds.len()).unwrap_or(u16::MAX);
    let wanted = count.div_ceil(USAGE_PER_COLUMN).max(1);
    let fit = |bar: usize| {
        let column_width = usage_column_width(name_width, bar);
        (
            bar,
            column_width,
            (right.saturating_sub(x) + USAGE_COLUMN_GAP) / column_width.saturating_add(USAGE_COLUMN_GAP),
        )
    };
    let (bar_width, column_width, fit) = (USAGE_BAR_MIN_WIDTH..=USAGE_BAR_WIDTH)
        .rev()
        .step_by(2)
        .map(fit)
        .find(|(_, _, fit)| *fit >= wanted)
        .unwrap_or_else(|| fit(USAGE_BAR_WIDTH));
    let columns = wanted.min(fit).max(1);
    UsageGrid { rows: count.div_ceil(columns), kinds, columns, name_width, column_width, bar_width }
}

enum UsageRow {
    Window(&'static str, Option<Window>),
    Credits(f64),
}

fn usage_rows(limits: &Limits) -> Vec<UsageRow> {
    let windowed = limits.session.is_some() || limits.weekly.is_some();
    if windowed || (limits.monthly.is_none() && limits.credits.is_none()) {
        let long = match (limits.weekly, limits.monthly) {
            (None, Some(monthly)) => UsageRow::Window(USAGE_MONTHLY_LABEL, Some(monthly)),
            (weekly, _) => UsageRow::Window(USAGE_WEEKLY_LABEL, weekly),
        };
        return vec![UsageRow::Window(USAGE_SESSION_LABEL, limits.session), long];
    }
    let monthly = limits.monthly.map(|monthly| UsageRow::Window(USAGE_MONTHLY_LABEL, Some(monthly)));
    monthly.into_iter().chain(limits.credits.map(UsageRow::Credits)).collect()
}

fn draw_usage(app: &App, grid: &UsageGrid, x: u16, first: u16, area: Rect, buf: &mut Buffer) {
    let right = area.right();
    let checking = app.usage.len() < usage::PROVIDERS.len();
    let top = first.min(area.bottom().saturating_sub(USAGE_ROWS * grid.rows));
    if x >= right || top >= area.bottom() || (grid.kinds.is_empty() && !checking) {
        return;
    }
    let now = now_ms();
    if top > area.y {
        let updated = grid.kinds.iter().filter_map(|kind| app.usage.get(kind)?.updated_at_ms).max();
        let status = match updated {
            Some(at) => {
                if app.language.is_pt() {
                    format!(" · atualizado há {}", duration_short(now.saturating_sub(at)))
                } else {
                    format!(" · updated {} ago", duration_short(now.saturating_sub(at)))
                }
            }
            None => app.t(" · checking…", " · verificando…").to_string(),
        };
        let after = put(buf, x, top - 1, app.t("usage", "uso"), fg(theme::MUTED), right);
        put(buf, after, top - 1, &status, fg(theme::FAINT), right);
    }
    for (index, kind) in grid.kinds.iter().enumerate() {
        let index = u16::try_from(index).unwrap_or(u16::MAX);
        let column = index / grid.rows;
        let column_x = x.saturating_add(column.saturating_mul(grid.column_width + USAGE_COLUMN_GAP));
        let right = if column + 1 < grid.columns {
            column_x.saturating_add(grid.column_width + USAGE_COLUMN_GAP - 1).min(right)
        } else {
            right
        };
        let y = top + USAGE_ROWS * (index % grid.rows);
        if y >= area.bottom() || column_x >= right {
            continue;
        }
        let entry = app.usage.get(kind);
        put(buf, column_x, y, kind.label(), fg(theme::kind_color(*kind)), right);
        let window_x = column_x + grid.name_width;
        let Some(limits) = entry.and_then(|u| u.limits) else {
            let text = entry
                .and_then(|u| u.problem)
                .map_or(app.t("checking…", "verificando…"), |p| p.display_label(app.language));
            put(buf, window_x, y, text, fg(theme::FAINT), right);
            continue;
        };
        for (row, usage_row) in usage_rows(&limits).iter().enumerate() {
            let y = y + u16::try_from(row).unwrap_or(0);
            if y >= area.bottom() {
                break;
            }
            let label = match usage_row {
                UsageRow::Window(label, _) => match *label {
                    USAGE_WEEKLY_LABEL => app.t("week", "sem."),
                    USAGE_MONTHLY_LABEL => app.t("month", "mês"),
                    other => other,
                },
                UsageRow::Credits(_) => app.t(USAGE_CREDITS_LABEL, "saldo"),
            };
            let cx = put(buf, window_x, y, &pad(label, USAGE_WINDOW_WIDTH), fg(theme::DIM), right);
            let end = match usage_row {
                UsageRow::Window(_, Some(window)) => {
                    draw_usage_window(app, window, grid.bar_width, now, cx, y, right, buf)
                }
                UsageRow::Window(_, None) => put(buf, cx, y, "n/a", fg(theme::FAINT), right),
                UsageRow::Credits(credits) => {
                    let color = if *credits > 0.0 { theme::GREEN } else { theme::RED };
                    put(buf, cx, y, &format!("${credits:.2}"), fg(color).add_modifier(Modifier::BOLD), right)
                }
            };
            if row == 0
                && let Some(problem) = entry.and_then(|u| u.problem)
            {
                put(buf, end, y, &format!(" · {}", problem.display_label(app.language)), fg(theme::FAINT), right);
            }
        }
    }
}

enum ListLine {
    Header(String, std::path::PathBuf, usize),
    Agent(AgentId),
    Subagent(AgentId, usize),
}

fn time_columns(entry: &AgentEntry, now: u64) -> (Option<(String, Color)>, Option<String>) {
    let Some(status) = entry.status.as_ref().filter(|_| !entry.is_dead()) else {
        return (None, None);
    };
    let uptime = Some(duration_short(now.saturating_sub(status.started_at_ms)));
    if entry.kind() == AgentKind::Terminal {
        return (None, uptime);
    }
    let first = match status.state {
        AgentState::Working => {
            let start = status.turn_started_ms.unwrap_or(status.state_since_ms);
            Some((duration_short(now.saturating_sub(start)), theme::YELLOW))
        }
        AgentState::Paused => Some((duration_short(now.saturating_sub(status.state_since_ms)), theme::YELLOW)),
        AgentState::Done | AgentState::Idle if status.last_turn_ms.is_some() => {
            Some((duration_short(now.saturating_sub(status.state_since_ms)), theme::DIM))
        }
        _ => None,
    };
    (first, uptime)
}

fn activity(app: &App, entry: &AgentEntry) -> (String, Color) {
    if entry.is_starting() {
        return (app.t("starting…", "iniciando…").to_string(), theme::DIM);
    }
    match &entry.link {
        Link::Lost => (
            app.t("interrupted — ctrl+r to recover", "interrompido — ctrl+r para recuperar").to_string(),
            theme::RED,
        ),
        Link::Exited(info) => (info.describe_lang(app.language), theme::FAINT),
        _ => {
            let status = entry.status.as_ref();
            let text = status
                .map(|s| s.activity.clone())
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| entry.record.task.split_whitespace().collect::<Vec<_>>().join(" "));
            let color = match entry.state() {
                AgentState::Working => theme::TEXT,
                AgentState::Paused => theme::YELLOW,
                _ => theme::MUTED,
            };
            let text = if entry.state() == AgentState::Paused {
                if app.language.is_pt() {
                    format!("aguardando você · {text}")
                } else {
                    format!("waiting for you · {text}")
                }
            } else {
                text
            };
            (text, color)
        }
    }
}

fn changes_text(app: &App, changes: Option<Changes>) -> Vec<(String, Color)> {
    match changes {
        None => vec![("…".to_string(), theme::FAINT)],
        Some(changes) if changes.is_empty() => vec![(app.t("clean", "limpo").to_string(), theme::DIM)],
        Some(changes) => vec![
            (format!("{} {}", changes.files, app.t("files", "arquivos")), theme::DIM),
            (format!(" +{}", changes.insertions), theme::GREEN),
            (format!(" −{}", changes.deletions), theme::RED),
        ],
    }
}

fn changes_summary(app: &App, changes: Option<Changes>) -> String {
    changes
        .filter(|c| !c.is_empty())
        .map_or_else(String::new, |c| {
            format!(" ({} {} +{} −{})", c.files, app.t("files", "arquivos"), c.insertions, c.deletions)
        })
}

fn draw_diff(app: &mut App, area: Rect, buf: &mut Buffer) {
    if area.height == 0 {
        return;
    }
    let Some(diff) = app.diff.as_ref() else { return };
    let right = area.right();
    let entry = app.agents.get(&diff.id);
    let name = entry.map(|e| e.record.name.clone()).unwrap_or_default();
    let worktree = entry.and_then(|e| e.record.worktree.as_ref());
    let mut x = put(buf, area.x, area.y, "diff ", fg(theme::MUTED), right);
    x = put(buf, x, area.y, &name, fg(theme::TEXT).add_modifier(Modifier::BOLD), right);
    if let Some(worktree) = worktree {
        x = put(buf, x + 2, area.y, &format!("{BRANCH_GLYPH} {}", worktree.branch), fg(theme::LINK), right);
        let base: String = worktree.base.chars().take(BASE_SHORT).collect();
        x = put(buf, x, area.y, &format!(" {}{base}", app.t("since ", "desde ")), fg(theme::FAINT), right);
    }
    x = put(buf, x, area.y, "  ", Style::default(), right);
    for (text, color) in changes_text(app, entry.and_then(|e| e.changes)) {
        x = put(buf, x, area.y, &text, fg(color), right);
    }
    let body = Rect::new(area.x, area.y + 1, area.width, area.height.saturating_sub(1));
    if diff.empty {
        put(
            buf,
            body.x,
            body.y,
            &format!("{name} {}", app.t("has not changed anything yet", "não alterou nada ainda")),
            fg(theme::DIM),
            right,
        );
        return;
    }
    diff.view.render(body, buf);
}

fn draw_agent_list(app: &mut App, area: Rect, buf: &mut Buffer) {
    let groups = app.groups();
    if groups.is_empty() {
        let lines = [
            (app.t("no agents in this session yet", "nenhum agente nesta sessão ainda"), theme::MUTED),
            (
                app.t(
                    "write a task below and press enter to start one",
                    "escreva uma tarefa abaixo e pressione enter para iniciar",
                ),
                theme::DIM,
            ),
            (
                app.t(
                    "tab switches who runs it, ctrl+l changes where",
                    "tab alterna quem executa, ctrl+l altera onde",
                ),
                theme::DIM,
            ),
        ];
        for (i, (text, color)) in lines.iter().enumerate() {
            let y = area.y + u16::try_from(i).unwrap_or(0);
            if y < area.bottom() {
                put(buf, area.x, y, text, fg(*color), area.right());
            }
        }
        return;
    }
    let mut lines = Vec::new();
    for group in &groups {
        lines.push(ListLine::Header(group.label.clone(), group.cwd.clone(), group.ids.len()));
        for id in &group.ids {
            lines.push(ListLine::Agent(*id));
            let count = app.agents.get(id).map_or(0, |e| e.subagents.len());
            lines.extend((0..count).map(|index| ListLine::Subagent(*id, index)));
        }
    }
    let height = usize::from(area.height);
    let selected_line =
        app.selected.and_then(|sel| lines.iter().position(|l| matches!(l, ListLine::Agent(id) if *id == sel)));
    if let Some(index) = selected_line {
        if index < app.list_offset {
            app.list_offset = index.saturating_sub(1);
        } else if index >= app.list_offset + height {
            app.list_offset = index + 1 - height;
        }
    }
    app.list_offset = app.list_offset.min(lines.len().saturating_sub(height));

    let name_width = app
        .agents
        .values()
        .filter(|e| e.record.session == app.session)
        .map(|e| width(&e.record.name))
        .max()
        .unwrap_or(NAME_MIN)
        .clamp(NAME_MIN, NAME_MAX);
    let fixed = 8 + name_width + 2 + TIME_WIDTH + 1 + TIME_WIDTH + 2 + KIND_WIDTH;
    let show_uptime = usize::from(area.width) >= fixed + 16;
    let now = now_ms();
    let right = area.right();

    for (row, line) in lines.iter().skip(app.list_offset).take(height).enumerate() {
        let y = area.y + u16::try_from(row).unwrap_or(0);
        match line {
            ListLine::Header(label, cwd, count) => {
                let color = theme::project_color(label);
                let x = put(buf, area.x, y, label, fg(color).add_modifier(Modifier::BOLD), right);
                let x = put(buf, x + 1, y, "›", fg(theme::DIM), right);
                put(buf, x + 1, y, &count.to_string(), fg(theme::FAINT), right);
                app.hits.push((Rect::new(area.x, y, area.width, 1), Hit::Project(cwd.clone())));
            }
            ListLine::Agent(id) => {
                let Some(entry) = app.agents.get(id) else { continue };
                let state = entry.state();
                let selected = app.selected == Some(*id);
                let mut x = area.x;
                x = put(buf, x, y, &format!("{:>3} ", id), fg(theme::DIM), right);
                x = put(buf, x, y, if selected { "▸" } else { " " }, fg(theme::ACCENT), right);
                x = put(buf, x, y, " ", Style::default(), right);
                let (glyph, glyph_color) =
                    if entry.is_starting() { ("◌", theme::DIM) } else { theme::glyph(entry.kind(), state, app.tick) };
                x = put(buf, x, y, glyph, fg(glyph_color), right);
                x = put(buf, x, y, " ", Style::default(), right);
                let mut name_style = fg(theme::name_color(entry.kind(), state));
                if selected {
                    name_style = name_style.add_modifier(Modifier::BOLD);
                }
                x = put(buf, x, y, &pad(&entry.record.name, name_width), name_style, right);
                x = put(buf, x, y, "  ", Style::default(), right);
                let (first, uptime) = time_columns(entry, now);
                let (first_text, first_color) = first.unwrap_or_default();
                x = put(buf, x, y, &pad_left(&first_text, TIME_WIDTH), fg(first_color), right);
                if show_uptime {
                    x = put(buf, x, y, " ", Style::default(), right);
                    x = put(buf, x, y, &pad_left(&uptime.unwrap_or_default(), TIME_WIDTH), fg(theme::DIM), right);
                }
                x = put(buf, x, y, "  ", Style::default(), right);
                x = put(buf, x, y, &pad(entry.kind().label(), KIND_WIDTH), fg(theme::kind_color(entry.kind())), right);
                if entry.record.worktree.is_some() {
                    x = put(buf, x, y, &format!("{BRANCH_GLYPH} "), fg(theme::LINK), right);
                    for (text, color) in changes_text(app, entry.changes) {
                        x = put(buf, x, y, &text, fg(color), right);
                    }
                    x = put(buf, x, y, "  ", Style::default(), right);
                }
                let (text, color) = activity(app, entry);
                let available = usize::from(right.saturating_sub(x));
                put(buf, x, y, &truncate(&text, available), fg(color), right);
                app.hits.push((Rect::new(area.x, y, area.width, 1), Hit::Agent(*id)));
            }
            ListLine::Subagent(parent, index) => {
                let Some(sub) = app.agents.get(parent).and_then(|e| e.subagents.get(*index)) else { continue };
                let mut x = area.x + SUBAGENT_INDENT;
                x = put(buf, x, y, "↳ ", fg(theme::FAINT), right);
                x = put(buf, x, y, theme::SPINNER[app.tick % theme::SPINNER.len()], fg(theme::YELLOW), right);
                x = put(buf, x, y, " ", Style::default(), right);
                let role_width = name_width.saturating_sub(2);
                x = put(buf, x, y, &pad(&truncate(&sub.role, role_width), role_width), fg(theme::MUTED), right);
                x = put(buf, x, y, "  ", Style::default(), right);
                let elapsed = duration_short(now.saturating_sub(sub.started_at_ms));
                x = put(buf, x, y, &pad_left(&elapsed, TIME_WIDTH), fg(theme::DIM), right);
                if show_uptime {
                    x = put(buf, x, y, &" ".repeat(TIME_WIDTH + 1), Style::default(), right);
                }
                x = put(buf, x, y, "  ", Style::default(), right);
                x = put(buf, x, y, &pad(app.t(SUBAGENT_LABEL, "subagente"), KIND_WIDTH), fg(theme::FAINT), right);
                let text = match (sub.description.is_empty(), sub.activity.is_empty()) {
                    (false, false) => format!("{} · {}", sub.description, sub.activity),
                    (false, true) => sub.description.clone(),
                    _ => sub.activity.clone(),
                };
                let available = usize::from(right.saturating_sub(x));
                put(buf, x, y, &truncate(&text, available), fg(theme::MUTED), right);
                app.hits.push((Rect::new(area.x, y, area.width, 1), Hit::Agent(*parent)));
            }
        }
    }
}

fn prompt_prefix(app: &App) -> Option<(String, Color, String, usize)> {
    match app.prompt.as_ref()? {
        Prompt::Where(input) => {
            Some((format!("{} › ", app.t("where", "onde")), theme::LINK, input.text(), width(&input.before_cursor())))
        }
        Prompt::Rename(id, input) => {
            let old = app.agents.get(id).map(|e| e.record.name.clone()).unwrap_or_default();
            Some((
                format!("{} {old} › ", app.t("rename", "renomear")),
                theme::ACCENT,
                input.text(),
                width(&input.before_cursor()),
            ))
        }
        Prompt::NewSession(input) => Some((
            format!("{} › ", app.t("new session", "nova sessão")),
            theme::LINK,
            input.text(),
            width(&input.before_cursor()),
        )),
        Prompt::RenameSession(input) => Some((
            format!("{} › ", app.t("rename session", "renomear sessão")),
            theme::LINK,
            input.text(),
            width(&input.before_cursor()),
        )),
        Prompt::Confirm(_) => None,
    }
}

fn confirm_text(app: &App, confirm: &Confirm) -> String {
    let name = |id: &AgentId| app.agents.get(id).map(|e| e.record.name.clone()).unwrap_or_default();
    let text = match confirm {
        Confirm::Kill(id) => {
            if app.language.is_pt() {
                format!("parar agente {}? o processo será finalizado", name(id))
            } else {
                format!("stop agent {}? the process will be terminated", name(id))
            }
        }
        Confirm::Remove(id) => {
            if app.language.is_pt() {
                format!("remover {} e sua saída gravada?", name(id))
            } else {
                format!("remove {} and its recorded output?", name(id))
            }
        }
        Confirm::Delete(id) => {
            if app.language.is_pt() {
                format!("excluir {}? o processo será parado e removido com sua saída gravada", name(id))
            } else {
                format!("delete {}? the process will be stopped and removed with its recorded output", name(id))
            }
        }
        Confirm::DeleteSession(session) => {
            if app.language.is_pt() {
                format!("excluir sessão {session}?  [s/N]")
            } else {
                format!("delete session {session}?  [y/N]")
            }
        }
        Confirm::Recover(ids) => {
            if app.language.is_pt() {
                format!("recuperar {} agente(s) interrompido(s) em suas pastas?  [S/n]", ids.len())
            } else {
                format!("recover {} interrupted agent(s) in their folders?  [Y/n]", ids.len())
            }
        }
        Confirm::Merge(id) => {
            let entry = app.agents.get(id);
            let branch = entry.and_then(|e| e.record.worktree.as_ref()).map(|w| w.branch.clone()).unwrap_or_default();
            let summary = changes_summary(app, entry.and_then(|e| e.changes));
            if app.language.is_pt() {
                format!(
                    "mesclar {branch}{summary} no branch atual do repositório? alterações pendentes são commitadas em {branch} primeiro"
                )
            } else {
                format!(
                    "merge {branch}{summary} into the repository's current branch? pending changes are committed on {branch} first"
                )
            }
        }
    };
    let worktree = match confirm {
        Confirm::Remove(id) | Confirm::Delete(id) => app.agents.get(id).and_then(|e| e.record.worktree.as_ref()),
        _ => None,
    };
    match worktree {
        Some(worktree) => {
            if app.language.is_pt() {
                format!("{text} — seu worktree e branch {} também serão removidos", worktree.branch)
            } else {
                format!("{text} — its worktree and branch {} go too", worktree.branch)
            }
        }
        None => text,
    }
}

const MORE_RESERVED: usize = 6;

fn first_visible(labels: &[String], selected: Option<usize>, available: usize) -> usize {
    let Some(selected) = selected else { return 0 };
    let mut used = 0;
    let mut start = selected + 1;
    while start > 0 {
        let needed = width(&labels[start - 1]) + 2;
        if used + needed > available {
            break;
        }
        used += needed;
        start -= 1;
    }
    start.min(selected)
}

fn draw_path_suggestions(app: &App, x: u16, y: u16, right: u16, buf: &mut Buffer) {
    let listing = app.paths.listing().map(|dir| app.display(dir)).unwrap_or_default();
    let mut cx = put(buf, x, y, &truncate_start(&listing, 48), fg(theme::FAINT), right);
    cx = put(buf, cx, y, " › ", fg(theme::FAINT), right);
    let suggestions = app.paths.suggestions();
    if suggestions.is_empty() {
        put(buf, cx, y, app.t("no folders here", "nenhuma pasta aqui"), fg(theme::FAINT), right);
        return;
    }
    let labels: Vec<String> = suggestions.iter().map(|name| format!("{name}/")).collect();
    let available = usize::from(right.saturating_sub(cx)).saturating_sub(MORE_RESERVED);
    let selected = app.paths.selected();
    let start = first_visible(&labels, selected, available);
    let limit = right.saturating_sub(u16::try_from(MORE_RESERVED).unwrap_or(0));
    let mut shown = start;
    for (index, label) in labels.iter().enumerate().skip(start) {
        let needed = u16::try_from(width(label)).unwrap_or(u16::MAX);
        if cx.saturating_add(needed) > limit {
            break;
        }
        let style = if selected == Some(index) {
            fg(theme::ACCENT).bg(theme::TAB_ACTIVE_BG).add_modifier(Modifier::BOLD)
        } else {
            fg(theme::LINK)
        };
        cx = put(buf, cx, y, label, style, right);
        cx = put(buf, cx, y, "  ", Style::default(), right);
        shown = index + 1;
    }
    let hidden = labels.len() - shown + start;
    if hidden > 0 {
        put(buf, cx, y, &format!("+{hidden}"), fg(theme::FAINT), right);
    }
}

fn composer_rows(app: &App) -> u16 {
    match &app.prompt {
        Some(Prompt::Confirm(confirm)) if confirm.is_popup() => {}
        Some(_) => return 1,
        None => {}
    }
    u16::try_from(app.task.line_count()).unwrap_or(MAX_COMPOSER_ROWS).clamp(1, MAX_COMPOSER_ROWS)
}

fn draw_task(app: &mut App, x: u16, y: u16, rows: u16, right: u16, buf: &mut Buffer) -> Position {
    let text = app.task.text();
    let before = app.task.before_cursor();
    let cursor_row = before.matches('\n').count();
    let cursor_width = width(before.rsplit('\n').next().unwrap_or_default());
    let rows = usize::from(rows.max(1));
    let first = (cursor_row + 1).saturating_sub(rows);
    let visible = usize::from(right.saturating_sub(x + 1));
    let skip = cursor_width.saturating_sub(visible);
    let color = if text.starts_with('/') {
        theme::LINK
    } else if text.starts_with(SHELL_PREFIX) {
        theme::TERMINAL
    } else {
        theme::TEXT
    };
    for (index, line) in text.split('\n').enumerate().skip(first).take(rows) {
        let row = y + u16::try_from(index - first).unwrap_or(0);
        let hidden = if index == cursor_row { skip } else { 0 };
        let shown = skip_columns(line, hidden);
        put(buf, x, row, &shown, fg(color), right);
        app.hits.push((Rect::new(x, row, right.saturating_sub(x), 1), Hit::Task { line: index, skip: hidden, x }));
    }
    let column = x + u16::try_from(cursor_width - skip).unwrap_or(0);
    let row = y + u16::try_from(cursor_row - first).unwrap_or(0);
    Position::new(column.min(right.saturating_sub(1)), row)
}

fn tone_color(tone: Tone) -> Color {
    match tone {
        Tone::Info => theme::TEXT,
        Tone::Success => theme::GREEN,
        Tone::Warn => theme::YELLOW,
        Tone::Error => theme::RED,
    }
}

fn draw_composer(app: &mut App, area: Rect, buf: &mut Buffer) -> Option<Position> {
    if area.height == 0 {
        return None;
    }
    let x = area.x + 1;
    let right = area.right();
    let rows = composer_rows(app).min(area.height);
    let mut cursor = None;
    if let Some(Prompt::Confirm(confirm)) = app.prompt.as_ref()
        && !confirm.is_popup()
    {
        let text = confirm_text(app, confirm);
        put(buf, x, area.y, &text, fg(theme::YELLOW).add_modifier(Modifier::BOLD), right);
    } else if let Some((prefix, color, text, before)) = prompt_prefix(app) {
        let after = put(buf, x, area.y, &prefix, fg(color), right);
        put(buf, after, area.y, &text, fg(theme::TEXT), right);
        let column = after + u16::try_from(before).unwrap_or(0);
        cursor = Some(Position::new(column.min(right.saturating_sub(1)), area.y));
    } else if app.task.is_empty() {
        put(buf, x, area.y, app.t("write a task", "escreva uma tarefa"), fg(theme::FAINT), right);
        cursor = Some(Position::new(x, area.y));
    } else {
        cursor = Some(draw_task(app, x, area.y, rows, right, buf));
    }
    if area.height <= rows {
        return cursor;
    }
    let y = area.y + rows;
    let mut cx = put(buf, x, y, "pc ", fg(theme::DIM), right);
    cx = put(buf, cx, y, &app.hostname, fg(theme::HOST), right);
    cx = put(buf, cx, y, "   ", Style::default(), right);
    let who_start = cx;
    cx = put(buf, cx, y, &format!("{} ", app.t("who", "quem")), fg(theme::DIM), right);
    cx = put(buf, cx, y, app.who.label(), fg(theme::kind_color(app.who)).add_modifier(Modifier::BOLD), right);
    app.hits.push((Rect::new(who_start, y, cx - who_start, 1), Hit::Who));
    cx = put(buf, cx, y, "   ", Style::default(), right);
    let worktree_start = cx;
    cx = put(buf, cx, y, &format!("{} ", app.t("worktree", "worktree")), fg(theme::DIM), right);
    let (state, style) = if app.worktree {
        (app.t("on", "ligado"), fg(theme::GREEN).add_modifier(Modifier::BOLD))
    } else {
        (app.t("off", "desligado"), fg(theme::FAINT))
    };
    cx = put(buf, cx, y, state, style, right);
    app.hits.push((Rect::new(worktree_start, y, cx - worktree_start, 1), Hit::Worktree));
    cx = put(buf, cx, y, "   ", Style::default(), right);
    let bypass_start = cx;
    cx = put(buf, cx, y, &format!("{} ", app.t("bypass", "bypass")), fg(theme::DIM), right);
    let (state, style) = if app.bypass {
        (app.t("on", "ligado"), fg(theme::YELLOW).add_modifier(Modifier::BOLD))
    } else {
        (app.t("off", "desligado"), fg(theme::FAINT))
    };
    cx = put(buf, cx, y, state, style, right);
    app.hits.push((Rect::new(bypass_start, y, cx - bypass_start, 1), Hit::Bypass));
    cx = put(buf, cx, y, "   ", Style::default(), right);
    let where_start = cx;
    cx = put(buf, cx, y, &format!("{} ", app.t("where", "onde")), fg(theme::DIM), right);
    let where_text = app.display(&app.where_dir);
    cx = put(buf, cx, y, &where_text, fg(theme::LINK), right);
    app.hits.push((Rect::new(where_start, y, cx - where_start, 1), Hit::Where));
    if area.height <= rows + 1 {
        return cursor;
    }
    let y = area.y + rows + 1;
    if app.paths.is_active() {
        draw_path_suggestions(app, x, y, right, buf);
    } else if app.prompt.is_none() && app.task.text().starts_with('/') {
        let typed = app.task.text();
        let word = typed.split_whitespace().next().unwrap_or("/");
        let commands = matching_commands(word).join("  ");
        let mut cx = put(buf, x, y, &commands, fg(theme::MUTED), right);
        let skills = app.skills.matching(app.who, word);
        if !skills.is_empty() {
            if !commands.is_empty() {
                cx = put(buf, cx, y, "  ", Style::default(), right);
            }
            put(buf, cx, y, &skills.join("  "), fg(theme::kind_color(app.who)), right);
        }
    } else if app.diff.is_some() && app.prompt.is_none() && app.notice.is_none() {
        put(buf, x, y, app.t(DIFF_HINT, "esc fechar · ↑↓ pgup pgdn rolar · /merge mescla"), fg(theme::FAINT), right);
    } else if app.prompt.is_none() && app.task.text().starts_with(SHELL_PREFIX) {
        let text = if app.language.is_pt() {
            format!("enter executa em um novo terminal · quem permanece {}", app.who.label())
        } else {
            format!("enter runs it in a new terminal · who stays {}", app.who.label())
        };
        put(buf, x, y, &text, fg(theme::TERMINAL), right);
    } else if let Some(notice) = app.notice.as_ref() {
        put(buf, x, y, &notice.text, fg(tone_color(notice.tone)), right);
    } else {
        put(
            buf,
            x,
            y,
            app.t(
                HINT,
                "↑↓ selecionar · enter abrir · tab agente · ^l pasta · ^n sessão · ^e renomear · del excluir · f1 ajuda",
            ),
            fg(theme::FAINT),
            right,
        );
    }
    cursor
}

pub const PWA_LABEL: &str = "▣ orkai pwa";
const PWA_TITLE: &str = "orkai pwa";
const PWA_TAGLINE: &str = "your orkai on the phone, from anywhere · end-to-end encrypted";
const PWA_HINT_ON: &str = "esc back · c copy link · n new code (forgets the phones) · d turn off";
const PWA_COPY: &str = " copy ";
const PWA_HINT_OFF: &str = "enter turns it on · esc back";
const PWA_LABEL_WIDTH: usize = 10;
const PWA_SIDE_MIN: u16 = 44;
const PWA_STEPS: [&str; 3] = [
    "point the phone camera at the code",
    "your orkai opens in the browser, already paired",
    "share › add to home screen keeps it one tap away",
];
const PWA_PRIVACY: [&str; 2] =
    ["the tunnel only carries encrypted bytes; the key", "lives in this code, never on a server"];

fn pwa_badge(app: &App, pwa: &PwaPanel) -> (String, Color) {
    match pwa.info.as_ref() {
        _ if !pwa.enabled => (app.t("off", "desligado").to_string(), theme::FAINT),
        Some(info) if info.phase == Phase::Failed => (app.t("error", "erro").to_string(), theme::RED),
        Some(info) if info.phase == Phase::Ready && info.phones > 0 => (format!("● {}", info.phones), theme::GREEN),
        Some(info) if info.phase == Phase::Ready => (app.t("on", "ligado").to_string(), theme::GREEN),
        _ => ("…".to_string(), theme::YELLOW),
    }
}

fn pwa_waiting(app: &App, pwa: &PwaPanel) -> (String, Color) {
    match pwa.info.as_ref() {
        None => (app.t("starting the orkai pwa…", "iniciando o orkai pwa…").to_string(), theme::YELLOW),
        Some(info) => match info.phase {
            Phase::Installing => (
                app.t(
                    "downloading cloudflared, the official build from github.com/cloudflare…",
                    "baixando cloudflared, a compilação oficial de github.com/cloudflare…",
                )
                .to_string(),
                theme::YELLOW,
            ),
            Phase::Connecting => (
                if app.language.is_pt() {
                    format!("abrindo o túnel {}…", info.tunnel.label())
                } else {
                    format!("opening the {} tunnel…", info.tunnel.label())
                },
                theme::YELLOW,
            ),
            Phase::Failed => (
                format!(
                    "{} · {}",
                    info.error.as_deref().unwrap_or_else(|| app.t("the tunnel failed", "o túnel falhou")),
                    app.t("retrying", "tentando novamente")
                ),
                theme::RED,
            ),
            Phase::Ready => (
                app.t("the pairing code is not ready", "o código de pareamento não está pronto").to_string(),
                theme::MUTED,
            ),
        },
    }
}

fn draw_qr(qr: &Qr, x: u16, y: u16, buf: &mut Buffer) {
    let color = |dark: bool| {
        let (r, g, b) = if dark { crate::pwa::qr::DARK } else { crate::pwa::qr::LIGHT };
        Color::Rgb(r, g, b)
    };
    for row in 0..qr.rows() {
        for column in 0..qr.size() {
            let (top, bottom) = qr.cell(column, row);
            let position = (x + u16::try_from(column).unwrap_or(0), y + u16::try_from(row).unwrap_or(0));
            if let Some(cell) = buf.cell_mut(position) {
                cell.set_symbol(crate::pwa::qr::UPPER_HALF)
                    .set_style(Style::default().fg(color(top)).bg(color(bottom)));
            }
        }
    }
}

fn copy_button() -> Style {
    Style::default().fg(theme::INK).bg(theme::ACCENT).add_modifier(Modifier::BOLD)
}

fn pwa_details(app: &App, room: usize) -> Vec<(String, String, Style, Option<Hit>)> {
    let Some(info) = app.pwa.info.as_ref() else { return Vec::new() };
    let mut rows = vec![(
        app.t("tunnel", "túnel").to_string(),
        format!("{} · {}", info.tunnel.label(), app.t("ready", "pronto")),
        fg(theme::MUTED),
        None,
    )];
    if let Some(code) = app.pwa.code.as_ref() {
        for (index, line) in split_columns(&code.link, room).into_iter().enumerate() {
            let label = if index == 0 { app.t("link", "link") } else { "" };
            rows.push((label.to_string(), line, fg(theme::LINK), None));
        }
        rows.push((
            String::new(),
            app.t(PWA_COPY, " copiar ").to_string(),
            copy_button(),
            Some(Hit::CopyPwaLink),
        ));
    }
    let phones = match info.phones {
        0 => (app.t("no phone connected", "nenhum celular conectado").to_string(), theme::DIM),
        1 => (app.t("1 connected", "1 conectado").to_string(), theme::GREEN),
        count => (format!("{count} {}", app.t("connected", "conectados")), theme::GREEN),
    };
    rows.push((app.t("phones", "celulares").to_string(), phones.0, fg(phones.1), None));
    let privacy_lines = if app.language.is_pt() {
        &[
            "o túnel transmite apenas bytes criptografados; a chave",
            "reside neste código, nunca em um servidor",
        ][..]
    } else {
        &PWA_PRIVACY[..]
    };
    for (index, line) in privacy_lines.iter().enumerate() {
        let label = if index == 0 { app.t("privacy", "privacidade") } else { "" };
        rows.push((label.to_string(), (*line).to_string(), fg(theme::DIM), None));
    }
    rows
}

fn draw_pwa_side(app: &mut App, x: u16, y: u16, right: u16, bottom: u16, buf: &mut Buffer) {
    let mut y = y;
    let steps = if app.language.is_pt() {
        &[
            "aponte a câmera do celular para o código",
            "seu orkai abre no navegador, já conectado",
            "compartilhar › adicionar à tela de início deixa a um toque de distância",
        ][..]
    } else {
        &PWA_STEPS[..]
    };
    for (index, step) in steps.iter().enumerate() {
        if y >= bottom {
            return;
        }
        let after = put(buf, x, y, &format!("{}  ", index + 1), fg(theme::ACCENT).add_modifier(Modifier::BOLD), right);
        put(buf, after, y, step, fg(theme::TEXT), right);
        y += 1;
    }
    y += 1;
    let room = usize::from(right.saturating_sub(x)).saturating_sub(PWA_LABEL_WIDTH);
    for (label, value, style, hit) in pwa_details(app, room) {
        if y >= bottom {
            return;
        }
        let after = put(buf, x, y, &pad(&label, PWA_LABEL_WIDTH), fg(theme::DIM), right);
        let end = put(buf, after, y, &truncate(&value, usize::from(right.saturating_sub(after))), style, right);
        if let Some(hit) = hit {
            app.hits.push((Rect::new(after, y, end.saturating_sub(after), 1), hit));
        }
        y += 1;
    }
}

fn draw_pwa(app: &mut App, area: Rect, buf: &mut Buffer) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let right = area.right();
    let bottom = area.bottom();
    let mut y = area.y;
    let after = put(buf, area.x, y, PWA_TITLE, fg(theme::TEXT).add_modifier(Modifier::BOLD), right);
    let (badge, color) = pwa_badge(app, &app.pwa);
    put(buf, after + 2, y, &badge, fg(color).add_modifier(Modifier::BOLD), right);
    y += 1;
    if y < bottom {
        put(
            buf,
            area.x,
            y,
            app.t(
                PWA_TAGLINE,
                "seu orkai no celular, de onde estiver · criptografado de ponta a ponta",
            ),
            fg(theme::DIM),
            right,
        );
    }
    y += 2;
    if y >= bottom {
        return;
    }
    if !app.pwa.enabled {
        put(buf, area.x, y, app.t("the orkai pwa is off", "o orkai pwa está desligado"), fg(theme::MUTED), right);
        if y + 1 < bottom {
            put(
                buf,
                area.x,
                y + 1,
                app.t(
                    "press enter to turn it on and show the pairing code",
                    "pressione enter para ligar e exibir o código de pareamento",
                ),
                fg(theme::DIM),
                right,
            );
        }
        return;
    }
    let Some(code) = app.pwa.code.as_ref() else {
        let (text, color) = pwa_waiting(app, &app.pwa);
        let spinner = theme::SPINNER[app.tick % theme::SPINNER.len()];
        let after = put(buf, area.x, y, spinner, fg(color), right);
        put(buf, after + 1, y, &text, fg(color), right);
        return;
    };
    let available = bottom.saturating_sub(y);
    let Some(qr) =
        code.qr.as_ref().filter(|qr| qr.rows() <= usize::from(available) && qr.size() <= usize::from(area.width))
    else {
        put(
            buf,
            area.x,
            y,
            app.t(
                "make the terminal bigger to show the code, or open on the phone:",
                "aumente o terminal para exibir o código, ou abra no celular:",
            ),
            fg(theme::YELLOW),
            right,
        );
        let lines = split_columns(&code.link, usize::from(area.width));
        let button = (y + 1).saturating_add(u16::try_from(lines.len()).unwrap_or(u16::MAX));
        for (row, line) in (y + 1..bottom).zip(lines) {
            put(buf, area.x, row, &line, fg(theme::LINK), right);
        }
        if button < bottom {
            let end = put(buf, area.x, button, app.t(PWA_COPY, " copiar "), copy_button(), right);
            app.hits.push((Rect::new(area.x, button, end.saturating_sub(area.x), 1), Hit::CopyPwaLink));
        }
        return;
    };
    draw_qr(qr, area.x, y, buf);
    let qr_width = u16::try_from(qr.size()).unwrap_or(u16::MAX);
    let qr_height = u16::try_from(qr.rows()).unwrap_or(u16::MAX);
    let side_x = area.x + qr_width + 3;
    if side_x + PWA_SIDE_MIN <= right {
        draw_pwa_side(app, side_x, y + 1, right, bottom, buf);
    } else {
        draw_pwa_side(app, area.x, y + qr_height + 1, right, bottom, buf);
    }
}

fn draw_pwa_status(app: &App, area: Rect, buf: &mut Buffer) {
    if area.height == 0 {
        return;
    }
    let right = area.right();
    let x = area.x + 1;
    if let Some(notice) = app.notice.as_ref() {
        put(buf, x, area.y, &notice.text, fg(tone_color(notice.tone)), right);
    }
    let hint = if app.pwa.enabled {
        app.t(
            PWA_HINT_ON,
            "esc voltar · c copiar link · n novo código (esquece os celulares) · d desligar",
        )
    } else {
        app.t(PWA_HINT_OFF, "enter liga · esc voltar")
    };
    put(buf, x, area.bottom() - 1, hint, fg(theme::FAINT), right);
}

fn draw_agent(app: &mut App, id: AgentId, area: Rect, buf: &mut Buffer) -> Option<Position> {
    app.hits.push((area, Hit::Terminal));
    let entry = app.agents.get(&id)?;
    match entry.term.as_ref() {
        Some(term) => term.render(area, buf),
        None => {
            let message = if entry.is_dead() {
                app.t("loading the last recorded screen…", "carregando a última tela gravada…")
            } else {
                app.t("connecting…", "conectando…")
            };
            let y = area.y + area.height / 2;
            let x = area.x + area.width.saturating_sub(u16::try_from(width(message)).unwrap_or(0)) / 2;
            put(buf, x, y, message, fg(theme::DIM), area.right());
            None
        }
    }
}

fn draw_agent_status(app: &App, id: AgentId, area: Rect, buf: &mut Buffer) {
    if area.height == 0 {
        return;
    }
    let Some(entry) = app.agents.get(&id) else { return };
    let right = area.right();
    let y = area.y;
    let scrolled = entry.term.as_ref().is_some_and(|t| t.is_scrolled());
    let hint = if entry.is_dead() {
        app.t("enter restart · esc back · pgup/pgdn scroll", "enter reiniciar · esc voltar · pgup/pgdn rolar")
    } else if scrolled {
        app.t("scrollback · type to return · ctrl+] overview", "histórico · digite para voltar · ctrl+] visão geral")
    } else {
        app.t("ctrl+] overview · shift+pgup scroll", "ctrl+] visão geral · shift+pgup rolar")
    };
    let others = app.attention(Some(id)).total();
    let lead = if others > 0 {
        if app.language.is_pt() {
            format!("‖ {others} precisam de você · alt+n · ")
        } else {
            format!("‖ {others} need you · alt+n · ")
        }
    } else {
        String::new()
    };
    let hint_width = u16::try_from(width(&lead) + width(hint)).unwrap_or(0);
    let hint_x = right.saturating_sub(hint_width + 1);
    let limit = if hint_x > area.x + 20 { hint_x.saturating_sub(2) } else { right };
    if limit < right {
        let x = put(buf, hint_x, y, &lead, fg(theme::YELLOW), right);
        put(buf, x, y, hint, fg(theme::FAINT), right);
    }
    let (state_text, state_color) = match &entry.link {
        Link::Exited(info) => (info.describe_lang(app.language), theme::MUTED),
        Link::Lost => (app.t("interrupted", "interrompido").to_string(), theme::RED),
        _ => {
            let state = entry.state();
            let color = match state {
                AgentState::Working => theme::YELLOW,
                AgentState::Paused => theme::YELLOW,
                AgentState::Done => theme::GREEN,
                _ => theme::MUTED,
            };
            (state.display_label(app.language).to_string(), color)
        }
    };
    let mut x = area.x + 1;
    let kind = entry.kind();
    let (glyph, glyph_color) = theme::glyph(kind, entry.state(), app.tick);
    x = put(buf, x, y, glyph, fg(glyph_color), limit);
    x = put(buf, x + 1, y, &entry.record.name, fg(theme::TEXT).add_modifier(Modifier::BOLD), limit);
    x = put(buf, x + 2, y, kind.label(), fg(theme::kind_color(kind)), limit);
    if let Some(worktree) = entry.record.worktree.as_ref() {
        x = put(buf, x + 2, y, &format!("{BRANCH_GLYPH} {}", worktree.branch), fg(theme::LINK), limit);
    }
    if kind.is_ai() || entry.is_dead() {
        x = put(buf, x + 2, y, &state_text, fg(state_color), limit);
    }
    let path_x = x + 2;
    if path_x < limit {
        let path = truncate(&app.display(&entry.record.cwd), usize::from(limit - path_x));
        put(buf, path_x, y, &path, fg(theme::LINK), limit);
    }
}

fn draw_confirm(app: &App, confirm: &Confirm, screen: Rect, buf: &mut Buffer) {
    let (title, color) = match confirm {
        Confirm::Delete(_) => (app.t(" delete agent ", " excluir agente "), theme::RED),
        Confirm::Remove(_) => (app.t(" remove agent ", " remover agente "), theme::RED),
        Confirm::Merge(_) => (app.t(" merge worktree ", " mesclar worktree "), theme::GREEN),
        _ => (app.t(" stop agent ", " parar agente "), theme::YELLOW),
    };
    let width = CONFIRM_WIDTH.min(screen.width.saturating_sub(4));
    let height = CONFIRM_HEIGHT.min(screen.height.saturating_sub(2));
    let area = Rect::new(
        screen.x + screen.width.saturating_sub(width) / 2,
        screen.y + screen.height.saturating_sub(height) / 2,
        width,
        height,
    );
    Clear.render(area, buf);
    let lines = vec![
        Line::default(),
        Line::styled(confirm_text(app, confirm), fg(theme::TEXT).add_modifier(Modifier::BOLD)),
        Line::default(),
        Line::styled(
            app.t(CONFIRM_HINT, "y / enter confirma  ·  qualquer outra tecla cancela"),
            fg(theme::DIM),
        ),
    ];
    Paragraph::new(lines)
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(fg(color))
                .title(title)
                .title_style(fg(color).add_modifier(Modifier::BOLD)),
        )
        .render(area, buf);
}

fn draw_help(app: &App, screen: Rect, buf: &mut Buffer) {
    let help = crate::i18n::help_items(app.language);
    let title = crate::i18n::help_title(app.language);
    let width = 70.min(screen.width.saturating_sub(4));
    let height = (u16::try_from(help.len()).unwrap_or(0) + 4).min(screen.height.saturating_sub(2));
    let area = Rect::new(
        screen.x + (screen.width.saturating_sub(width)) / 2,
        screen.y + (screen.height.saturating_sub(height)) / 2,
        width,
        height,
    );
    Clear.render(area, buf);
    let lines: Vec<Line> = help
        .iter()
        .map(|(key, text)| {
            Line::from(vec![
                ratatui::text::Span::styled(format!(" {key:<18}"), fg(theme::ACCENT)),
                ratatui::text::Span::styled(*text, fg(theme::TEXT)),
            ])
        })
        .collect();
    Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(fg(theme::RULE))
                .title(title)
                .title_style(fg(theme::TEXT).add_modifier(Modifier::BOLD)),
        )
        .render(area, buf);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_reserves_sidebar_composer_and_tabs() {
        let screen = Rect::new(0, 0, 120, 40);
        let overview = areas(screen, false, false, 1);
        assert_eq!(overview.bottom.height, 3);
        assert_eq!(overview.rule.y, 36);
        assert!(overview.sidebar.is_some());
        assert_eq!(overview.body.x, SIDEBAR_WIDTH + MAIN_GAP);
        assert!(overview.tabs.is_none());
        let agent = areas(screen, true, true, 1);
        assert_eq!(agent.bottom.height, 1);
        assert_eq!(agent.tabs.unwrap().height, 1);
        assert_eq!(agent.body.height, 40 - 1 - 1 - 1);
        assert_eq!(agent.body.right(), 120);
    }

    #[test]
    fn composer_grows_with_task_lines_up_to_a_limit() {
        let screen = Rect::new(0, 0, 120, 40);
        assert_eq!(areas(screen, false, false, 3).bottom.height, 5);
        assert_eq!(areas(screen, false, false, 40).bottom.height, 2 + MAX_COMPOSER_ROWS);
        assert_eq!(areas(screen, false, false, 0).bottom.height, 3);
        assert_eq!(areas(screen, true, true, 5).bottom.height, 1);
    }

    #[test]
    fn narrow_screens_hide_the_sidebar() {
        let layout = areas(Rect::new(0, 0, 60, 20), false, false, 1);
        assert!(layout.sidebar.is_none());
        assert_eq!(layout.body.x, 1);
    }

    #[test]
    fn usage_rows_keep_the_5h_and_week_layout_and_fall_back_to_month_and_funds() {
        let window = |used_percent| Some(Window { used_percent, resets_at_ms: None });
        let labels = |limits: &Limits| -> Vec<String> {
            usage_rows(limits)
                .iter()
                .map(|row| match row {
                    UsageRow::Window(label, Some(w)) => format!("{label} {}", w.used_percent),
                    UsageRow::Window(label, None) => format!("{label} n/a"),
                    UsageRow::Credits(credits) => format!("funds {credits}"),
                })
                .collect()
        };
        let claude = Limits { session: window(55.0), weekly: window(92.0), monthly: window(1.0), ..Limits::default() };
        assert_eq!(labels(&claude), ["5h 55", "week 92"]);
        let free_codex = Limits { weekly: window(75.0), ..Limits::default() };
        assert_eq!(labels(&free_codex), ["5h n/a", "week 75"]);
        let kimi = Limits { session: window(30.0), monthly: window(10.0), ..Limits::default() };
        assert_eq!(labels(&kimi), ["5h 30", "month 10"]);
        let grok = Limits { monthly: window(7.0), ..Limits::default() };
        assert_eq!(labels(&grok), ["month 7"]);
        let kilo = Limits { monthly: window(25.0), credits: Some(12.5), ..Limits::default() };
        assert_eq!(labels(&kilo), ["month 25", "funds 12.5"]);
        assert_eq!(labels(&Limits { credits: Some(0.0), ..Limits::default() }), ["funds 0"]);
        assert_eq!(labels(&Limits::default()), ["5h n/a", "week n/a"]);
    }

    #[test]
    fn usage_bars_round_and_clamp_to_their_width() {
        assert_eq!(usage_cells(0.0, 12), 0);
        assert_eq!(usage_cells(55.0, 12), 7);
        assert_eq!(usage_cells(8.0, 12), 1);
        assert_eq!(usage_cells(100.0, 12), 12);
        assert_eq!(usage_cells(140.0, 12), 12);
        assert_eq!(usage_cells(-5.0, 12), 0);
        assert_eq!(usage_cells(f64::NAN, 12), 0);
    }

    #[test]
    fn tiny_screens_do_not_panic() {
        for (w, h) in [(0, 0), (1, 1), (5, 2), (10, 4)] {
            let _ = areas(Rect::new(0, 0, w, h), true, true, 1);
            let _ = areas(Rect::new(0, 0, w, h), false, true, MAX_COMPOSER_ROWS);
        }
    }
}
