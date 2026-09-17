use crate::chud::{self, Mood};
use crate::session::{Agent, Session, Status};
use crate::theme::p as pal;
use crate::update::Update;
use crate::usage::{local_minute, now_secs, Plan, Window};
use crate::{App, Ask, Diff, DiffAct, Drag, Hit, Menu, Prompt, Row, Tool};
use ratatui::prelude::*;
use ratatui::symbols::Marker;
use ratatui::widgets::{
    Axis, Bar, BarChart, Block, Borders, Chart, Clear, Dataset, GraphType, List, ListItem, ListState, Paragraph,
};
use std::time::Duration;
use tui_term::widget::{Cursor, PseudoTerminal};

/// Clickable regions from the last draw, topmost last.
pub type Hits = Vec<(Rect, Hit)>;

const CARD_W: u16 = 24;
const CARD_H: u16 = 10;

/// PTY size (rows, cols): the toolbar, the session's usage header and the status bar take a row
/// each, the sidebar `side` columns. Never below 4x20: chud.app can start us before its window
/// has a size, and vt100 panics when a line wraps on a 1-row screen (col_wrap underflows the row).
pub fn pane(w: u16, h: u16, side: u16) -> (u16, u16) {
    (h.saturating_sub(3).max(4), w.saturating_sub(side).max(20))
}

pub fn label(s: Status) -> &'static str {
    match s {
        Status::Idle => "idle",
        Status::Working => "working",
        Status::NeedsInput => "needs input",
        Status::Done => "done",
        Status::Exited => "exited",
    }
}

fn badge(s: Status) -> Span<'static> {
    match s {
        Status::Idle => "○ ".dark_gray(),
        Status::Working => "● ".yellow(),
        Status::NeedsInput if now_secs() % 2 == 0 => "◐ ".magenta().bold(),
        Status::NeedsInput => "◑ ".light_magenta().bold(),
        Status::Done => "✓ ".green(),
        Status::Exited => "✗ ".red(),
    }
}

/// Nerd Font icons only where the font is known to have them (chud.app bundles one, or you
/// say so with CHUD_ICONS=nerd); plain Unicode everywhere else, so no terminal shows boxes.
fn nerd_icons(term_program: Option<&str>, chud_icons: Option<&str>) -> bool {
    term_program == Some("chud-app") || chud_icons == Some("nerd")
}

// Nerd Font glyphs: nf-cod-claude, nf-cod-copilot, nf-dev-terminal.
fn icon(agent: &Agent) -> Span<'static> {
    static NERD: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let nerd = *NERD.get_or_init(|| {
        let var = |k| std::env::var(k).ok();
        nerd_icons(var("TERM_PROGRAM").as_deref(), var("CHUD_ICONS").as_deref())
    });
    let pick = |glyph, fallback| if nerd { glyph } else { fallback };
    match agent {
        Agent::Claude => Span::styled(pick("\u{ec82} ", "✳ "), pal().claude),
        Agent::Copilot => Span::styled(pick("\u{ec1e} ", "◆ "), pal().copilot),
        Agent::Profile(i) => Span::styled(format!("{} ", crate::agents::all()[*i].icon), crate::agents::all()[*i].color),
        Agent::Shell => pick("\u{e795} ", "❯ ").gray(),
        Agent::Other(_) => pick("\u{e795} ", "❯ ").dark_gray(),
    }
}

fn agent_color(agent: &Agent) -> Color {
    match agent {
        Agent::Claude => pal().claude,
        Agent::Copilot => pal().copilot,
        Agent::Profile(i) => crate::agents::all()[*i].color,
        _ => Color::Gray,
    }
}

/// Context and usage come from these two agents' logs, which have known formats.
fn has_usage(s: &Session) -> bool {
    matches!(s.agent, Agent::Claude | Agent::Copilot)
}

fn elapsed(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
    }
}

/// Time until a Unix timestamp: "14m05s", "2h14m", "15d 3h".
fn until(at: u64) -> String {
    match at.saturating_sub(now_secs()) {
        0 => "now".into(),
        s @ 1..86_400 => elapsed(Duration::from_secs(s)),
        s => format!("{}d {}h", s / 86_400, s % 86_400 / 3600),
    }
}

fn tokens(n: u64) -> String {
    match n {
        0 => "-".into(),
        1..1000 => n.to_string(),
        1000..1_000_000 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.2}M", n as f64 / 1e6),
    }
}

fn mood(s: Status) -> Mood {
    match s {
        Status::Working => Mood::Munching,
        Status::NeedsInput => Mood::Waiting,
        Status::Exited => Mood::Sleepy,
        _ => Mood::Happy,
    }
}

/// How full the session's context is, 0..1.
fn fullness(s: &Session) -> f64 {
    s.usage.context as f64 / s.usage.limit().max(1) as f64
}

fn chud_art(s: &Session) -> Vec<Line<'static>> {
    chud::art(&chud::sprite(chud::fatness(s.worked()), mood(s.status()), now_secs()))
}

/// Block glyphs (▀ ▄ █) only where the font is known to draw them inside their cell: chud.app
/// bundles one, or you say so with CHUD_MASCOT=blocks. Everywhere else the chud and the bars
/// are painted with coloured spaces, which no font can get wrong.
pub fn block_glyphs(term_program: Option<&str>, chud_mascot: Option<&str>) -> bool {
    match chud_mascot {
        Some("blocks") => true,
        Some("safe") => false,
        _ => term_program == Some("chud-app"),
    }
}

/// Draws everything and returns the clickable regions.
pub fn draw(f: &mut Frame, app: &App) -> Hits {
    let mut hits = Hits::new();
    let [top, body, bar] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)]).areas(f.area());
    let [side, main] = Layout::horizontal([Constraint::Length(app.side), Constraint::Min(1)]).areas(body);
    toolbar(f, app, top, &mut hits);
    sidebar(f, app, side, &mut hits);
    if let Some(d) = &app.diff {
        diff(f, d, main, &mut hits);
    } else if app.dash {
        dashboard(f, app, main, &mut hits);
    } else if let Some(s) = app.sessions.get(app.sel) {
        let [head, term] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(main);
        session_header(f, s, &app.plan, head);
        let p = s.parser.lock().unwrap();
        let screen = p.screen();
        let cursor = Cursor::default().visibility(!screen.hide_cursor() && screen.scrollback() == 0);
        f.render_widget(PseudoTerminal::new(screen).cursor(cursor), term);
        picked(f, app, term);
        hits.push((term, Hit::Pane));
    } else {
        f.render_widget(Paragraph::new(" No sessions yet. Click + New, or press Ctrl-a n.").fg(pal().dim), main);
    }
    status_bar(f, app, bar);
    if let Some(m) = &app.menu {
        menu(f, m, &mut hits);
    }
    if app.help {
        help(f);
        hits.push((f.area(), Hit::Backdrop));
    }
    if let Some(p) = &app.prompt {
        prompt(f, p, &mut hits);
    }
    hits
}

/// One line above the terminal: what's running in it and, for an agent, your plan's usage:
/// Claude's rolling 5-hour limit (plus the week), Copilot's monthly premium requests. It follows
/// whichever agent is in front, including one started by hand in a shell. (Context fullness
/// lives in the sidebar.)
fn session_header(f: &mut Frame, s: &Session, plan: &Plan, area: Rect) {
    let mut spans = vec![Span::raw(" "), icon(&s.agent)];
    match &s.agent {
        Agent::Claude | Agent::Copilot => {
            let claude = s.agent == Agent::Claude;
            let model = if s.usage.model.is_empty() { String::new() } else { format!(" · {}", s.usage.model) };
            spans.push(format!("{}{model}   ", if claude { "claude" } else { "copilot" }).bold());
            let bar = |w: Window| chud::bar(w.used / 100.0, (area.width / 5).clamp(10, 30) as usize);
            match (claude, plan.five_hour, plan.copilot) {
                (true, Some(h), _) => {
                    spans.push(Span::raw("5h limit "));
                    spans.extend(bar(h));
                    spans.push(format!(" {:.0}%", h.used).into());
                    spans.push(format!("  resets in {}", until(h.resets_at)).fg(pal().dim));
                    if let Some(w) = plan.seven_day {
                        spans.push(format!(" · week {:.0}%", w.used).fg(pal().dim));
                    }
                }
                (false, _, Some((q, total))) => {
                    spans.push(Span::raw("premium requests "));
                    spans.extend(bar(q));
                    spans.push(format!(" {:.0}% of {total}", q.used).into());
                    spans.push(format!("  resets in {}", until(q.resets_at)).fg(pal().dim));
                }
                (true, None, _) => spans.push("5h limit shows after Claude's next reply".fg(pal().dim)),
                (false, _, None) => spans.push("premium requests: checking with GitHub…".fg(pal().dim)),
            }
        }
        Agent::Profile(_) | Agent::Shell | Agent::Other(_) => {
            let program = match &s.agent {
                Agent::Profile(i) => crate::agents::all()[*i].name.clone(),
                Agent::Other(name) => name.clone(),
                _ => s.argv[0].rsplit('/').next().unwrap_or("shell").to_string(),
            };
            spans.push(format!("{program} · {}", s.cwd.display()).fg(pal().dim));
        }
    }
    f.render_widget(Line::from(spans).fg(pal().bar_fg).bg(pal().bar_bg), area);
}

fn toolbar(f: &mut Frame, app: &App, area: Rect, hits: &mut Hits) {
    let mut spans = vec![" chud ".fg(pal().on_accent).bg(pal().accent).bold(), " ".into()];
    let mut x = area.x + 7;
    let buttons = [
        (" + New ▾ ", Tool::New, false),
        (" ▦ Dashboard ", Tool::Dash, app.dash),
        (" ± Diff ", Tool::Diff, app.diff.is_some()),
        (" ? Help ", Tool::Help, app.help),
    ];
    for (text, tool, on) in buttons {
        let w = text.chars().count() as u16;
        hits.push((Rect::new(x, area.y, w, 1), Hit::Tool(tool)));
        spans.push(if on { text.fg(pal().on_accent).bg(pal().accent) } else { text.fg(pal().bar_fg).bg(pal().button_bg) });
        spans.push(" ".into());
        x += w + 1;
    }
    let count = |st| app.sessions.iter().filter(|s| s.status() == st).count();
    let right = Line::from(vec![
        format!("● {} working  ", count(Status::Working)).yellow(),
        format!("◐ {} need you ", count(Status::NeedsInput)).magenta(),
    ]);
    f.render_widget(Line::from(spans).fg(pal().bar_fg).bg(pal().bar_bg), area);
    f.render_widget(right.right_aligned(), area);
}

fn sidebar(f: &mut Frame, app: &App, area: Rect, hits: &mut Hits) {
    let rows = app.rows(false);
    let headers = app.groups.len() > 1;
    let dragging = matches!(app.drag, Some(Drag { from: Hit::Session(_), moved: true, .. }));
    let edge = matches!(app.drag, Some(Drag { from: Hit::Edge, .. }));
    let (mut num, mut selected) = (0, None);
    let items: Vec<ListItem> = rows
        .iter()
        .enumerate()
        .map(|(ri, &row)| {
            let (item, target) = match row {
                Row::Group(gi) => (group_header(app, gi), Hit::Group(gi)),
                Row::Session(i) => {
                    num += 1;
                    if i == app.sel {
                        selected = Some(ri);
                    }
                    (session_item(&app.sessions[i], num, headers), Hit::Session(i))
                }
            };
            // a base style, not List's highlight_style: that paints over the chud's pixel colours
            match row {
                _ if dragging && app.hover == Some(target) => item.style(Style::new().fg(pal().bar_fg).bg(pal().drop_bg)),
                Row::Session(i) if i == app.sel => item.style(Style::new().fg(pal().bar_fg).bg(pal().selected_bg)),
                _ => item,
            }
        })
        .collect();
    let mut state = ListState::default().with_selected(selected);
    let border = Block::new().borders(Borders::RIGHT).border_style(Style::new().fg(if edge { pal().accent } else { pal().dim }));
    let list = List::new(items).block(border);
    f.render_stateful_widget(list, area, &mut state);

    // click regions follow the list's scroll offset
    let right = area.right().saturating_sub(1);
    let mut y = area.y;
    for &row in rows.iter().skip(state.offset()) {
        if y >= area.bottom() {
            break;
        }
        let h = if matches!(row, Row::Group(_)) { 1 } else { 2 };
        let rect = Rect::new(area.x, y, area.width.saturating_sub(1), h.min(area.bottom() - y));
        let (item, menu) = match row {
            Row::Group(gi) => (Hit::Group(gi), Hit::GroupMenu(gi)),
            Row::Session(i) => (Hit::Session(i), Hit::SessionMenu(i)),
        };
        hits.push((rect, item));
        let dots = Rect::new(right.saturating_sub(2), y, 2, 1);
        f.render_widget(Span::from("…").fg(pal().dim), dots);
        hits.push((dots, menu));
        y += h;
    }
    if y < area.bottom() {
        hits.push((Rect::new(area.x, y, area.width.saturating_sub(1), area.bottom() - y), Hit::SidebarEmpty));
    }
    hits.push((Rect::new(right, area.y, 1, area.height), Hit::Edge));
}

fn group_header(app: &App, gi: usize) -> ListItem<'static> {
    let g = &app.groups[gi];
    let members: Vec<&Session> = app.sessions.iter().filter(|s| s.group == g.id).collect();
    let waiting = members.iter().filter(|s| s.status() == Status::NeedsInput).count();
    let unread = members.iter().filter(|s| s.unread).count();
    let arrow = if g.collapsed { "▸" } else { "▾" };
    let mut line = vec![format!("{arrow} {} ({})", g.name, members.len()).cyan().bold()];
    if waiting > 0 {
        line.push(format!(" ◐{waiting}").magenta().bold());
    }
    if g.collapsed && unread > 0 {
        line.push(format!(" •{unread}").magenta());
    }
    ListItem::new(Line::from(line))
}

/// Two lines: the agent's icon and the name, then its fullness bar and what it's doing.
fn session_item(s: &Session, num: usize, headers: bool) -> ListItem<'static> {
    let st = s.status();
    let pad = if headers { " " } else { "" };
    let name = format!("{num} {}", s.label());
    let mut first =
        vec![Span::raw(pad), icon(&s.agent), badge(st), if s.unread { name.bold() } else { name.into() }];
    if s.unread {
        first.push(" •".magenta().bold());
    }
    let detail = match (st, s.exit) {
        (Status::Exited, Some(code)) => format!("exited {code}"),
        _ => Some(s.detail()).filter(|d| !d.is_empty()).unwrap_or_else(|| label(st).into()),
    };
    let detail = match s.working_since {
        Some(t) => format!("{} · {detail}", elapsed(t.elapsed())),
        None => detail,
    };
    let mut second = vec![Span::raw(format!("{pad}    "))];
    if has_usage(s) {
        second.extend(chud::bar(fullness(s), 8));
        second.push(format!(" {:>3.0}% ", fullness(s) * 100.0).fg(pal().dim));
    } else {
        second.push(format!("{} · ", s.folder()).fg(pal().dim));
    }
    second.push(detail.fg(pal().dim));
    ListItem::new(vec![Line::from(first), Line::from(second)])
}

/// The summary page: totals, a tokens-over-time chart, who ate the most, and a card per chud.
fn dashboard(f: &mut Frame, app: &App, area: Rect, hits: &mut Hits) {
    let order: Vec<usize> =
        app.rows(true).into_iter().filter_map(|r| if let Row::Session(i) = r { Some(i) } else { None }).collect();
    let [head, tiles, charts, heat, cards] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Length(12),
        Constraint::Length(11),
        Constraint::Min(0),
    ])
    .areas(area);

    let munched: Duration = app.sessions.iter().map(|s| s.worked()).sum();
    let intro = format!("  {} sessions · {} of munching · numbers come from each agent's own log", order.len(), elapsed(munched));
    f.render_widget(Line::from(vec![" Summary ".fg(pal().on_accent).bg(pal().accent).bold(), intro.fg(pal().dim)]), head);

    let count = |st| app.sessions.iter().filter(|s| s.status() == st).count();
    let (eaten, produced, credits) = app.sessions.iter().fold((0, 0, 0.0), |(e, p, c), s| {
        (e + s.usage.input + s.usage.output, p + s.usage.output, c + s.usage.credits)
    });
    let tile_data = [
        (count(Status::Working).to_string(), "working", Color::Yellow),
        (count(Status::NeedsInput).to_string(), "need you", Color::Magenta),
        (count(Status::Done).to_string(), "done", Color::Green),
        (tokens(eaten), "tokens eaten", pal().accent),
        (tokens(produced), "produced", Color::Cyan),
        (format!("{credits:.1}"), "copilot credits", pal().copilot),
    ];
    let boxes: [Rect; 6] = Layout::horizontal([Constraint::Fill(1); 6]).spacing(1).areas(tiles);
    for ((value, what, color), r) in tile_data.into_iter().zip(boxes) {
        let text = Line::from(vec![value.fg(color).bold(), format!(" {what}").fg(pal().dim)]);
        f.render_widget(Paragraph::new(text).centered().block(Block::bordered().border_style(pal().dim)), r);
    }

    let [line_area, bar_area] =
        Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)]).spacing(1).areas(charts);
    // output tokens in 2-minute buckets over the last two hours, per agent kind
    let now = now_secs() / 60;
    let start = now.saturating_sub(119);
    let mut buckets = [[0u64; 60]; 2];
    for s in &app.sessions {
        let kind = s.usage.is_copilot() as usize;
        for (&minute, &t) in s.usage.timeline.range(start..=now) {
            buckets[kind][((minute - start) / 2) as usize] += t;
        }
    }
    let peak = buckets.iter().flatten().copied().max().unwrap_or(0).max(1);
    let points: Vec<Vec<(f64, f64)>> =
        buckets.iter().map(|b| b.iter().enumerate().map(|(i, &t)| (i as f64, t as f64)).collect()).collect();
    let line = |name, data, color| {
        Dataset::default().name(name).marker(Marker::Braille).graph_type(GraphType::Line).style(color).data(data)
    };
    let chart = Chart::new(vec![line("claude", &points[0], pal().claude), line("copilot", &points[1], pal().copilot)])
        .block(Block::bordered().title(" tokens produced · last 2 hours ").border_style(pal().dim))
        .x_axis(Axis::default().bounds([0.0, 59.0]).labels(["-2h", "-1h", "now"]).style(pal().dim))
        .y_axis(Axis::default().bounds([0.0, peak as f64]).labels(["0".into(), tokens(peak / 2), tokens(peak)]).style(pal().dim));
    f.render_widget(chart, line_area);

    let mut eaters: Vec<(String, u64, Color)> = order
        .iter()
        .map(|&i| &app.sessions[i])
        .filter(|s| s.usage.output > 0)
        .map(|s| (s.label().chars().take(14).collect(), s.usage.output, agent_color(&s.agent)))
        .collect();
    eaters.sort_by(|a, b| b.1.cmp(&a.1));
    let bars: Vec<Bar> = eaters
        .into_iter()
        .take(bar_area.height.saturating_sub(2) as usize)
        .map(|(name, v, c)| {
            Bar::with_label(name, v).text_value(tokens(v)).style(c).value_style(Style::new().fg(Color::Black).bg(c))
        })
        .collect();
    let block = Block::bordered().title(" who ate the most ").border_style(pal().dim);
    f.render_widget(BarChart::horizontal(bars).bar_width(1).bar_gap(0).block(block), bar_area);

    activity(f, app, heat);

    hits.push((cards, Hit::Cards));
    let per_row = (cards.width / CARD_W).max(1);
    for (k, &i) in order.iter().skip(app.dash_scroll as usize * per_row as usize).enumerate() {
        let (col, row) = (k as u16 % per_row, k as u16 / per_row);
        if (row + 1) * CARD_H > cards.height {
            break;
        }
        let r = Rect::new(cards.x + col * CARD_W, cards.y + row * CARD_H, CARD_W, CARD_H);
        card(f, &app.sessions[i], i == app.sel, r);
        hits.push((r, Hit::Card(i)));
    }
}

/// Lights up the cells a drag covered, the way any terminal shows a selection: from the first
/// cell to the last in reading order, not as a rectangle.
fn picked(f: &mut Frame, app: &App, term: Rect) {
    let Some(((r1, c1), (r2, c2))) = app.picked_cells() else { return };
    let buf = f.buffer_mut();
    for row in r1..=r2.min(term.height.saturating_sub(1)) {
        let from = if row == r1 { c1 } else { 0 };
        let to = if row == r2 { c2 } else { term.width.saturating_sub(1) };
        for col in from..=to.min(term.width.saturating_sub(1)) {
            buf[(term.x + col, term.y + row)].set_style(Style::new().add_modifier(Modifier::REVERSED));
        }
    }
}

/// A week of work at a glance: one row per day, one tile per hour, each tile shaded by how
/// many tokens the agents produced in that hour. Empty hours stay dark.
fn activity(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::bordered().title(" activity · tokens produced per hour ").border_style(pal().dim);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 2 {
        return;
    }
    let days = (inner.height - 1).min(7) as u64;
    let today = local_minute(now_secs() / 60) / 1440;
    let first = today + 1 - days;
    let mut grid = vec![[0u64; 24]; days as usize];
    for s in &app.sessions {
        for (&minute, &produced) in &s.usage.timeline {
            let local = local_minute(minute);
            if (first..=today).contains(&(local / 1440)) {
                grid[(local / 1440 - first) as usize][(local % 1440 / 60) as usize] += produced;
            }
        }
    }
    let peak = grid.iter().flatten().copied().max().unwrap_or(0).max(1);
    let shade = |t: u64| match t {
        0 => pal().heat_empty,
        // four steps, like a contribution graph: the lightest still reads against the empties
        t => {
            let step = (t * 4).div_ceil(peak).clamp(1, 4) as u8;
            let (lo, hi) = (pal().heat_low, pal().heat_high);
            let f = |from: u8, to: u8| (from as i16 + (to as i16 - from as i16) * step as i16 / 4) as u8;
            Color::Rgb(f(lo.0, hi.0), f(lo.1, hi.1), f(lo.2, hi.2))
        }
    };
    let mut lines: Vec<Line> = grid
        .iter()
        .enumerate()
        .map(|(row, hours)| {
            let day = first + row as u64;
            let name = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"][((day + 4) % 7) as usize];
            let mut spans = vec![format!("{name} ").fg(pal().dim)];
            spans.extend(hours.iter().map(|&t| Span::styled("██", Style::new().fg(shade(t)))));
            spans.push(format!(" {}", tokens(hours.iter().sum())).fg(pal().dim));
            Line::from(spans)
        })
        .collect();
    let mut axis = vec![Span::raw("    ")];
    axis.extend((0..24).step_by(6).map(|h| format!("{h:02}          ").fg(pal().dim)));
    axis.push(" less ".fg(pal().dim));
    axis.extend([1, peak / 3, peak * 2 / 3, peak].map(|t| Span::styled("█", Style::new().fg(shade(t)))));
    axis.push(" more".fg(pal().dim));
    lines.push(Line::from(axis));
    f.render_widget(Paragraph::new(lines), inner);
}

fn card(f: &mut Frame, s: &Session, selected: bool, r: Rect) {
    let st = s.status();
    let title = Line::from(vec![" ".into(), icon(&s.agent), badge(st)]);
    let block = Block::bordered().title(title).border_style(if selected { pal().accent } else { pal().dim });
    let inner = block.inner(r);
    f.render_widget(block, r);
    let mut lines: Vec<Line> = chud_art(s).into_iter().map(Line::centered).collect();
    let name: String = s.label().chars().take(inner.width as usize).collect();
    lines.push(Line::from(name.bold()).centered());
    if has_usage(s) {
        let p = fullness(s);
        let mut bar = chud::bar(p, inner.width.saturating_sub(6) as usize);
        bar.push(format!(" {:>3.0}%", p * 100.0).into());
        lines.push(Line::from(bar).centered());
    } else {
        lines.push(Line::from(s.folder().fg(pal().dim)).centered());
    }
    let stats = match s.working_since {
        Some(t) => format!("munching {}", elapsed(t.elapsed())),
        None => format!("{} · ate {}", label(st), elapsed(s.worked())),
    };
    lines.push(Line::from(stats.fg(pal().dim)).centered());
    f.render_widget(Paragraph::new(lines), inner);
}

fn diff(f: &mut Frame, d: &Diff, area: Rect, hits: &mut Hits) {
    let [actions, rest] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
    let mut spans = vec![Span::raw(" ")];
    let mut x = actions.x + 1;
    for (text, act) in [
        (" Commit all ", DiffAct::Commit),
        (" Discard file ", DiffAct::Discard),
        (" Refresh ", DiffAct::Refresh),
        (" Close ", DiffAct::Close),
    ] {
        let w = text.chars().count() as u16;
        hits.push((Rect::new(x, actions.y, w, 1), Hit::DiffAct(act)));
        spans.extend([text.fg(pal().bar_fg).bg(pal().button_bg), " ".into()]);
        x += w + 1;
    }
    f.render_widget(Line::from(spans), actions);

    let [list, body] =
        Layout::horizontal([Constraint::Length((rest.width / 3).min(40)), Constraint::Min(1)]).areas(rest);
    let items: Vec<ListItem> =
        d.files.iter().map(|(code, path)| ListItem::new(format!("{code} {path}"))).collect();
    let mut state = ListState::default().with_selected(Some(d.sel));
    let title = format!(" {} changed · {} ", d.files.len(), d.root.display());
    let files = List::new(items)
        .block(Block::new().borders(Borders::RIGHT).title(title))
        .highlight_style(Style::new().reversed());
    f.render_stateful_widget(files, list, &mut state);
    let first = list.y + 1; // below the title row
    for (k, row) in (state.offset()..d.files.len()).zip(first..list.bottom()) {
        hits.push((Rect::new(list.x, row, list.width.saturating_sub(1), 1), Hit::DiffFile(k)));
    }

    let lines: Vec<Line> = d
        .text
        .lines()
        .skip(d.scroll as usize)
        .take(body.height as usize)
        .map(|l| {
            let color = match l.as_bytes().first() {
                _ if l.starts_with("+++") || l.starts_with("---") => Color::Reset,
                Some(b'+') => Color::Green,
                Some(b'-') => Color::Red,
                Some(b'@') => Color::Cyan,
                _ => Color::Reset,
            };
            Line::styled(l.replace('\t', "    "), color)
        })
        .collect();
    f.render_widget(Paragraph::new(lines), body);
}

fn status_bar(f: &mut Frame, app: &App, area: Rect) {
    if let Some(flash) = &app.flash {
        f.render_widget(Line::from(flash.as_str().black().on_green()).fg(pal().bar_fg).bg(pal().bar_bg), area);
        return;
    }
    let keys = match &app.drag {
        _ if app.select => " selecting text: drag to select · ⌘C copies · C-a v gives the mouse back to chud ",
        Some(Drag { from: Hit::Session(_), moved: true, .. }) => " drop onto a session or a group to move it there ",
        Some(Drag { from: Hit::Edge, .. }) => " drag to resize the sidebar ",
        _ if app.diff.is_some() => " diff: j/k file · J/K scroll · c commit all · r discard file · R refresh · Esc close ",
        _ if app.dash => " summary: click a card to open that session · scroll for more · Esc close ",
        _ if app.prefix => " n new · r rename · g group · z fold · f zoom · y copy · s dashboard · d diff · x kill · q quit · ? help ",
        _ => " C-a ? help · click, drag and scroll with the mouse ",
    };
    let mut spans = vec![if app.prefix { keys.black().on_yellow() } else { keys.into() }];
    let waiting = app.sessions.iter().filter(|s| s.status() == Status::NeedsInput).count();
    if waiting > 0 {
        spans.push(format!(" {waiting} need input ").black().on_magenta());
    }
    match &app.update {
        Some(Update::Ready(commit)) => spans.push(format!(" ⟳ {commit} installed · restart chud ").black().on_green()),
        Some(Update::Failed(why)) => spans.push(format!(" ⟳ update failed: {why} ").black().on_red()),
        None => {}
    }
    f.render_widget(Line::from(spans).fg(pal().bar_fg).bg(pal().bar_bg), area);
}

fn menu(f: &mut Frame, m: &Menu, hits: &mut Hits) {
    let a = f.area();
    let w = m.items.iter().map(|(l, _)| l.chars().count() as u16).max().unwrap_or(0) + 4;
    let h = m.items.len() as u16 + 2;
    let r = Rect::new(m.x.min(a.width.saturating_sub(w)), m.y.min(a.height.saturating_sub(h)), w.min(a.width), h.min(a.height));
    let block = Block::bordered().border_style(pal().accent);
    let inner = block.inner(r);
    f.render_widget(Clear, r);
    f.render_widget(block, r);
    for (k, (text, _)) in m.items.iter().enumerate() {
        let row = Rect::new(inner.x, inner.y + k as u16, inner.width, 1);
        if row.y >= inner.bottom() {
            break;
        }
        f.render_widget(Line::from(format!(" {text}")), row);
        hits.push((row, Hit::MenuItem(k)));
    }
}

const HELP: &[(&str, &str)] = &[
    ("click", "select a session · … opens its menu · a group header folds it"),
    ("right-click", "a session or group header opens its menu"),
    ("drag", "a session onto another session or a group to move it"),
    ("drag in the terminal", "select text; letting go copies it"),
    ("drag edge", "the sidebar's right edge to resize it"),
    ("toolbar", "+ New (terminal or group) · Dashboard · Diff · Help"),
    ("C-a n", "new terminal (zsh) in this group"),
    ("C-a j / k / 1-9", "next / previous / nth session"),
    ("C-a Tab", "jump to the next session that needs you"),
    ("C-a r", "rename session (empty = automatic name)"),
    ("C-a g", "move session to a group (new name creates it)"),
    ("C-a G", "rename this session's group"),
    ("C-a z", "fold / unfold this group"),
    ("C-a J / K", "move session down / up in its group"),
    ("C-a y", "copy what this session shows to the clipboard"),
    ("⌘C / ⌘V", "copy what you selected · paste"),
    ("C-a v", "hand the mouse to the terminal, to select outside the pane"),
    ("C-a f", "zoom: hide or show the sidebar"),
    ("C-a d", "diff review (c commit, r discard, R refresh)"),
    ("C-a s", "summary dashboard"),
    ("C-a x", "kill session"),
    ("C-a q", "quit; running `chud` alone restores the layout"),
    ("C-a C-a", "send a literal Ctrl-a"),
];

fn help(f: &mut Frame) {
    let a = f.area();
    let (w, h) = (74.min(a.width), (HELP.len() as u16 + 2).min(a.height));
    let r = Rect::new((a.width - w) / 2, (a.height - h) / 2, w, h);
    let lines: Vec<Line> =
        HELP.iter().map(|(k, v)| Line::from(vec![format!(" {k:<17}").yellow(), (*v).into()])).collect();
    f.render_widget(Clear, r);
    let title = " chud · the chud gets fatter the longer its agent works · click or any key closes ";
    f.render_widget(Paragraph::new(lines).block(Block::bordered().title(title)), r);
}

fn prompt(f: &mut Frame, p: &Prompt, hits: &mut Hits) {
    let title = match p.ask {
        Ask::Commit => " commit message (commits everything in the repo) ",
        Ask::Rename => " rename session (empty = automatic name) ",
        Ask::Group => " move to group: name (new name creates it, empty = ungrouped) ",
        Ask::GroupRename(_) => " rename group ",
        Ask::NewGroup => " new group: name ",
        Ask::Kill => " kill this session? ",
        Ask::Discard => " discard all changes to this file? ",
        Ask::Quit => " sessions still running, quit and kill them? ",
    };
    let yes_no = matches!(p.ask, Ask::Kill | Ask::Discard | Ask::Quit);
    let text = if yes_no { String::new() } else { format!("{}▏", p.input) };
    let a = f.area();
    let w = 72.min(a.width);
    let r = Rect::new((a.width - w) / 2, a.height / 3, w, 3.min(a.height));
    f.render_widget(Clear, r);
    f.render_widget(Paragraph::new(text).block(Block::bordered().title(title)), r);
    let (ok, cancel) = if yes_no { (" Yes ", " No ") } else { (" OK ", " Cancel ") };
    let y = r.bottom().saturating_sub(1);
    let cancel_r = Rect::new(r.right().saturating_sub(cancel.len() as u16 + 2), y, cancel.len() as u16, 1);
    let ok_r = Rect::new(cancel_r.x.saturating_sub(ok.len() as u16 + 1), y, ok.len() as u16, 1);
    f.render_widget(ok.fg(pal().on_accent).bg(pal().accent), ok_r);
    f.render_widget(cancel.fg(pal().bar_fg).bg(pal().button_bg), cancel_r);
    hits.push((ok_r, Hit::Ok));
    hits.push((cancel_r, Hit::Cancel));
}

#[cfg(test)]
mod tests {
    #[test]
    fn block_glyphs_only_where_they_draw_right() {
        assert!(super::block_glyphs(Some("chud-app"), None), "chud.app bundles a font that draws them");
        assert!(!super::block_glyphs(Some("iTerm.app"), None), "unknown font: play safe");
        assert!(!super::block_glyphs(None, None));
        assert!(super::block_glyphs(Some("iTerm.app"), Some("blocks")), "you can say your font is fine");
        assert!(!super::block_glyphs(Some("chud-app"), Some("safe")), "or that it is not");
    }

    #[test]
    fn nerd_icons_only_where_the_font_has_them() {
        assert!(super::nerd_icons(Some("chud-app"), None));
        assert!(super::nerd_icons(Some("WarpTerminal"), Some("nerd")));
        assert!(!super::nerd_icons(Some("WarpTerminal"), None));
        assert!(!super::nerd_icons(None, None));
    }

    #[test]
    fn sessions_never_get_a_tiny_screen() {
        assert_eq!(super::pane(120, 40, 38), (37, 82));
        assert_eq!(super::pane(0, 0, 38), (4, 20), "a window app's first, unsized frame");
    }
}
