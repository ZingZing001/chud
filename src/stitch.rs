//! Selecting more than a screen of a full-screen program. Claude Code and Copilot draw in the
//! alternate screen and keep their own scrollback: chud only ever holds the screen they show,
//! and scrolling is theirs to do. So a selection that runs past the edge collects what it
//! passes over: each time the program scrolls, the new screen is laid against the last one to
//! see how far the text moved, and the rows that came into view are added to the capture.
//!
//! Also the clean-up every copy gets: no scrollbar, no thinking gutter, no padding.

/// One row as the program drew it: each cell's text, a wide character's second cell empty.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Line {
    pub cells: Vec<String>,
    pub wrapped: bool,
}

impl Line {
    /// What the row says, for telling rows apart.
    fn key(&self) -> String {
        self.cells.concat().trim_end().to_string()
    }

    fn blank(&self) -> bool {
        self.cells.iter().all(|c| c.trim().is_empty())
    }

    /// The text from column `from` up to (not including) `to`, without Copilot's thinking bar.
    fn text(&self, from: u16, to: u16) -> String {
        let from = if from < 3 && self.gutter() { 3 } else { from } as usize;
        let to = (to as usize).min(self.cells.len());
        self.cells.get(from..to).map(|c| c.concat()).unwrap_or_default()
    }

    /// A row Copilot drew in its thinking block: " │ " ahead of the text. Exactly that, so a tree
    /// (`│   ├── src`) or a table, which start at the edge or further in, are copied whole.
    fn gutter(&self) -> bool {
        let at = |c: usize| self.cells.get(c).map_or("", String::as_str);
        at(0).trim().is_empty() && at(1) == "│" && at(2).trim().is_empty()
    }
}

/// What a program draws over its own text while you are scrolled back, which is not part of it:
/// Claude Code's "Jump to bottom" on the last row of its transcript.
/// ponytail: Claude's wording; another program's overlay would be copied with its text
const OVERLAYS: &[&str] = &["Jump to bottom (click)"];

fn covered(line: &Line) -> bool {
    let text = line.key();
    OVERLAYS.iter().any(|o| text.contains(o))
}

/// The screen as rows of cells, with a scrollbar in the last column blanked out: it moves as
/// the program scrolls, and would make every row look changed.
pub fn lines_of(screen: &vt100::Screen) -> Vec<Line> {
    let (rows, cols) = screen.size();
    let bar = scrollbar_col(screen);
    (0..rows)
        .map(|r| Line {
            cells: (0..cols)
                .map(|c| match screen.cell(r, c) {
                    _ if Some(c) == bar => " ".to_string(),
                    Some(cell) if cell.is_wide_continuation() => String::new(),
                    Some(cell) if cell.has_contents() => cell.contents().to_string(),
                    _ => " ".to_string(),
                })
                .collect(),
            wrapped: screen.row_wrapped(r),
        })
        .collect()
}

/// A row Copilot drew in its thinking block (see `Line::gutter`), read off the screen.
pub fn gutter(screen: &vt100::Screen, r: u16) -> bool {
    let at = |c| screen.cell(r, c).map(|c| c.contents()).unwrap_or_default();
    at(0).trim().is_empty() && at(1) == "│" && at(2).trim().is_empty()
}

/// The last column, when it holds a scrollbar rather than text: nothing in it but blanks and
/// box-drawing, with a few vertical bars or blocks. ASCII `|` never counts, so a shell pipe or
/// a table that reaches the edge stays.
pub fn scrollbar_col(screen: &vt100::Screen) -> Option<u16> {
    let (rows, cols) = screen.size();
    let last = cols.checked_sub(1)?;
    let mut bars = 0;
    for r in 0..rows {
        let cell = screen.cell(r, last).map(|c| c.contents()).unwrap_or_default();
        match cell.chars().next() {
            None | Some(' ') => {}
            Some(c @ '\u{2500}'..='\u{259f}') => bars += "│┃║╎╏┆┇┊┋▌▐█░▒▓▕▏".contains(c) as u16,
            Some(_) => return None,
        }
    }
    (bars >= 3).then_some(last)
}

/// A scroll a program did itself: the rows it scrolled (first, last) and by how many lines,
/// positive when the text moved up (the view went further down), as `ESC [ n S` does it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scrolled {
    pub top: u16,
    pub bottom: u16,
    pub lines: i32,
}

/// Watches a program's output for how it scrolls: it sets the scroll margins (`ESC [ top ;
/// bottom r`), then moves the text between them, either outright (`ESC [ n S` up, `ESC [ n T`
/// down: Claude Code) or a line at a time from the cursor (reverse index `ESC M` at the top
/// margin, line feeds at the bottom one: Copilot). So chud knows which rows hold the text and how
/// far it went, rather than working it out from what changed.
#[derive(Default)]
pub struct ScrollWatch {
    /// the start of a sequence cut off at the end of the last read
    carry: Vec<u8>,
    margins: Option<(u16, u16)>,
    /// the cursor's row, while the output says where it is
    row: Option<u16>,
    seen: Option<Scrolled>,
    /// scrolls in different places since the last look: no single answer
    mixed: bool,
}

impl ScrollWatch {
    /// Reads `bytes` as written to a screen `rows` tall.
    pub fn feed(&mut self, bytes: &[u8], rows: u16) {
        let mut buf = std::mem::take(&mut self.carry);
        buf.extend_from_slice(bytes);
        let mut i = 0;
        while let Some(at) = buf[i..].iter().position(|&b| b == 0x1b || b == b'\n').map(|p| p + i) {
            i = at + 1;
            if buf[at] == b'\n' {
                self.down(rows);
                continue;
            }
            match buf.get(at + 1) {
                None if buf.len() - at < 32 => {
                    self.carry = buf[at..].to_vec(); // cut off: the rest comes with the next read
                    return;
                }
                Some(b'M') => self.up(),
                Some(b'D' | b'E') => self.down(rows),
                Some(b'[') => {
                    let params = at + 2;
                    let tail = &buf[params.min(buf.len())..];
                    let Some(len) = tail.iter().position(|b| !(b.is_ascii_digit() || *b == b';')) else {
                        if buf.len() - at < 32 {
                            self.carry = buf[at..].to_vec();
                            return;
                        }
                        continue;
                    };
                    let end = params + len;
                    let numbers: Vec<u16> =
                        std::str::from_utf8(&buf[params..end]).unwrap_or("").split(';').map(|n| n.parse().unwrap_or(0)).collect();
                    let n = |k: usize| numbers.get(k).copied().filter(|&v| v > 0);
                    match buf[end] {
                        // setting the margins puts the cursor home
                        b'r' if end == params => (self.margins, self.row) = (None, Some(0)),
                        b'r' => (self.margins, self.row) = (Some((n(0).unwrap_or(1) - 1, n(1).unwrap_or(rows).min(rows) - 1)), Some(0)),
                        b'H' | b'f' => self.row = Some(n(0).unwrap_or(1) - 1),
                        b'd' => self.row = Some(n(0).unwrap_or(1) - 1),
                        b'A' => self.row = self.row.map(|r| r.saturating_sub(n(0).unwrap_or(1))),
                        b'B' => self.row = self.row.map(|r| (r + n(0).unwrap_or(1)).min(rows - 1)),
                        f @ (b'S' | b'T') if numbers.len() == 1 => {
                            let lines = i32::from(n(0).unwrap_or(1)) * if f == b'S' { 1 } else { -1 };
                            self.saw(self.margins.unwrap_or((0, rows.saturating_sub(1))), lines);
                        }
                        _ => {}
                    }
                    i = end + 1;
                }
                _ => {}
            }
        }
    }

    /// A reverse index: at the top margin the text between the margins moves down a line.
    fn up(&mut self) {
        match (self.margins, self.row) {
            (Some(m), Some(r)) if r == m.0 => self.saw(m, -1),
            (_, Some(r)) => self.row = Some(r.saturating_sub(1)),
            _ => {}
        }
    }

    /// A line feed: at the bottom margin the text between the margins moves up a line.
    fn down(&mut self, rows: u16) {
        match (self.margins, self.row) {
            (Some(m), Some(r)) if r == m.1 => self.saw(m, 1),
            (_, Some(r)) => self.row = Some((r + 1).min(rows.saturating_sub(1))),
            _ => {}
        }
    }

    fn saw(&mut self, (top, bottom): (u16, u16), lines: i32) {
        match &mut self.seen {
            Some(s) if (s.top, s.bottom) == (top, bottom) => s.lines += lines,
            Some(_) => self.mixed = true,
            None => self.seen = Some(Scrolled { top, bottom, lines }),
        }
    }

    /// What scrolled since the last call, if it all happened in one place.
    pub fn take(&mut self) -> Option<Scrolled> {
        let seen = self.seen.take().filter(|s| !self.mixed && s.lines != 0);
        self.mixed = false;
        seen
    }
}

/// Rows that must agree before a move counts as found: enough that a couple of repeated
/// lines (blank ones, a lone `}`) can't fake one.
const SURE: usize = 3;

/// Everything a selection has passed over, in reading order, and where each of the rows on
/// screen now sits in it.
#[derive(Clone, Debug)]
pub struct Capture {
    lines: Vec<Line>,
    /// for each row of the last screen followed, its line; None for a row that is neither
    /// text that scrolled nor something fixed in place (a spinner, say)
    view: Vec<Option<usize>>,
    last: Vec<Line>,
    /// the rows of the screen the text scrolls through (first, last), once a scroll has shown
    /// them: a header above or an input box below stays out of a selection held past the edge
    moving: Option<(usize, usize)>,
    /// where in `lines` the text that scrolls sits (first, last). Rows coming into view go next
    /// to it by position, never by what they say: a blank row would match any blank line
    body: Option<(usize, usize)>,
    /// lost track of the scroll once: the capture stops growing, and keeps what it has
    lost: bool,
    /// the selection's ends, (line, column): kept here, because a line coming into view above
    /// one moves it down
    pub from: (usize, u16),
    pub to: (usize, u16),
}

impl Capture {
    /// A capture of `screen`, with a selection starting (and ending) at `row`, `col`.
    pub fn new(screen: Vec<Line>, (row, col): (u16, u16)) -> Capture {
        let at = (usize::from(row).min(screen.len().saturating_sub(1)), col);
        let view = (0..screen.len()).map(Some).collect();
        Capture { view, lines: screen.clone(), last: screen, moving: None, body: None, lost: false, from: at, to: at }
    }

    pub fn lost(&self) -> bool {
        self.lost
    }

    /// Lays a new screen against the last one. `told` is the scroll the program did itself, if it
    /// said (see `ScrollWatch`); without it, how far the text moved is worked out from the rows.
    /// False if it can't tell (the program redrew something else altogether): from then on the
    /// capture stays as it is.
    pub fn follow(&mut self, screen: Vec<Line>, told: Option<Scrolled>) -> bool {
        if self.lost {
            return false;
        }
        let n = screen.len();
        if n != self.last.len() {
            return self.lose();
        }
        if screen == self.last && told.is_none() {
            return true;
        }
        let (old, new) = (&self.last, &screen);
        let same = |r: usize, d: isize| {
            let o = r as isize + d;
            (0..n as isize).contains(&o) && new[r].key() == old[o as usize].key()
        };
        // a row the program keeps in place, like a header or an input box: still there, and
        // saying something (blank rows stay blank inside the text too)
        let fixed = |r: usize| same(r, 0) && !new[r].blank();
        let scroll = told.and_then(|t| told_rows(t, n, &same)).or_else(|| found_rows(n, &same, &fixed, |r| new[r].blank()));
        let Some((moved, region, kept, fresh)) = scroll else {
            // nothing scrolled: rows that stayed keep their lines, the rest changed in place
            let still = (0..n).filter(|&r| fixed(r)).count();
            if still < SURE {
                return self.lose();
            }
            self.view = (0..n).map(|r| if same(r, 0) { self.view[r] } else { None }).collect();
            self.last = screen;
            return true;
        };
        let inside = |r: usize| (region.0..=region.1).contains(&r);
        let mut view: Vec<Option<usize>> = (0..n)
            .map(|r| match r {
                _ if kept.contains(&r) => self.view[(r as isize + moved) as usize],
                _ if !inside(r) && same(r, 0) => self.view[r],
                _ => None,
            })
            .collect();
        // the first time, the text is whatever the scrolling rows showed before
        let body = self.body.or_else(|| {
            let was: Vec<usize> = (region.0..=region.1).filter_map(|r| self.view[r]).collect();
            Some((*was.iter().min()?, *was.iter().max()?))
        });
        let (Some(mut body), Some(&nearest)) = (body, kept.first()) else { return self.lose() };
        // each new row goes next to the one before it, outwards from the rows that moved: the
        // line already captured there when coming back over ground covered, else a new one
        let Some(mut next) = view[nearest] else { return self.lose() };
        for &r in &fresh {
            let at = match moved > 0 {
                true if next < body.1 => next + 1,
                false if next > body.0 => next - 1,
                down => {
                    let at = if down { body.1 + 1 } else { body.0 };
                    self.lines.insert(at, new[r].clone());
                    let later = view.iter_mut().flatten().chain([&mut self.from.0, &mut self.to.0, &mut body.1]);
                    for v in later.filter(|v| **v >= at) {
                        *v += 1;
                    }
                    if down {
                        body.1 = at; // the insert put it past the end; it is the end now
                    }
                    at
                }
            };
            (view[r], next) = (Some(at), at);
        }
        // a line first seen under an overlay is taken again once the program redraws it clear
        for &r in &kept {
            if let Some(i) = view[r].filter(|&i| covered(&self.lines[i]) && !covered(&new[r])) {
                self.lines[i] = new[r].clone();
            }
        }
        (self.view, self.last, self.body, self.moving) = (view, screen, Some(body), Some(region));
        true
    }

    fn lose(&mut self) -> bool {
        self.lost = true;
        self.view.iter_mut().for_each(|v| *v = None);
        false
    }

    /// The first or last row the text scrolls through, once a scroll has shown where that is.
    /// The last one stops short of a row under an overlay: what is beneath it isn't on screen.
    pub fn moving_edge(&self, bottom: bool) -> Option<u16> {
        let (first, last) = self.moving?;
        let hidden = |r: usize| self.line_on(r as u16).is_some_and(|i| covered(&self.lines[i]));
        Some(match bottom {
            true if last > first && hidden(last) => last - 1,
            true => last,
            false => first,
        } as u16)
    }

    /// The line on screen row `row`, or the nearest row that has one.
    pub fn line_at(&self, row: u16) -> Option<usize> {
        let row = (row as usize).min(self.view.len().saturating_sub(1));
        (0..self.view.len()).flat_map(|d| [row.checked_sub(d), Some(row + d)]).flatten().find_map(|r| *self.view.get(r)?)
    }

    /// The line screen row `row` shows, if any.
    pub fn line_on(&self, row: u16) -> Option<usize> {
        *self.view.get(row as usize)?
    }

    /// The selection, first end first; None while it is a single cell.
    pub fn ends(&self) -> Option<((usize, u16), (usize, u16))> {
        (self.from != self.to).then(|| if self.from < self.to { (self.from, self.to) } else { (self.to, self.from) })
    }

    /// The selected text, as you meant it rather than as the program drew it.
    pub fn selected(&self) -> String {
        self.ends().map(|(a, b)| self.text(a, b)).unwrap_or_default()
    }

    /// The text from `from` to `to` (line, column), both included: wrapped rows joined, padding
    /// trimmed.
    fn text(&self, from: (usize, u16), to: (usize, u16)) -> String {
        let (a, b) = if from <= to { (from, to) } else { (to, from) };
        let mut text = String::new();
        for i in a.0..=b.0.min(self.lines.len().saturating_sub(1)) {
            let line = &self.lines[i];
            let start = if i == a.0 { a.1 } else { 0 };
            let end = if i == b.0 { b.1 + 1 } else { line.cells.len() as u16 };
            text += &line.text(start, end);
            if i < b.0 && !line.wrapped {
                text.push('\n');
            }
        }
        text.lines().map(str::trim_end).collect::<Vec<_>>().join("\n")
    }
}

/// A scroll the program reported, checked against the screens: (shift, rows it covers, rows
/// that moved nearest-first, rows that came into view nearest-first). None if the screens
/// don't bear it out.
type Rows = (isize, (usize, usize), Vec<usize>, Vec<usize>);

fn told_rows(t: Scrolled, n: usize, same: &dyn Fn(usize, isize) -> bool) -> Option<Rows> {
    let (top, bottom, moved) = (usize::from(t.top), usize::from(t.bottom).min(n - 1), t.lines as isize);
    let shift = moved.unsigned_abs();
    if top >= bottom || shift > bottom - top {
        return None; // more than the whole area: nothing of before is left to line up with
    }
    let (kept, fresh): (Vec<usize>, Vec<usize>) = match moved > 0 {
        true => ((top..=bottom - shift).rev().collect(), (bottom + 1 - shift..=bottom).collect()),
        false => ((top + shift..=bottom).collect(), (top..top + shift).rev().collect()),
    };
    // most of what moved must be where the program said it went
    let agree = kept.iter().filter(|&&r| same(r, moved)).count();
    (agree * 2 >= kept.len()).then_some((moved, (top, bottom), kept, fresh))
}

fn found_rows(n: usize, same: &dyn Fn(usize, isize) -> bool, fixed: &dyn Fn(usize) -> bool, blank: impl Fn(usize) -> bool) -> Option<Rows> {
    // how far the text moved: the shift most non-blank rows agree on (the smaller one when two
    // tie, since a short scroll is the likelier); none of them, if nothing moved
    let score = |d: isize| (0..n).filter(|&r| !blank(r) && same(r, d)).count();
    let still = score(0);
    let (moved, best) = (1..n as isize)
        .flat_map(|d| [d, -d])
        .map(|d| (d, score(d)))
        .fold((0, 0), |(bd, bs), (d, s)| if s > bs { (d, s) } else { (bd, bs) });
    if best < SURE || best < still {
        return None;
    }
    // the rows that moved: from the first to the last that agree, and blank rows at either end
    // that line up as well
    let agree: Vec<usize> = (0..n).filter(|&r| !blank(r) && same(r, moved)).collect();
    let (mut lo, mut hi) = (agree[0], agree[agree.len() - 1]);
    while hi + 1 < n && same(hi + 1, moved) && !fixed(hi + 1) {
        hi += 1;
    }
    while lo > 0 && same(lo - 1, moved) && !fixed(lo - 1) {
        lo -= 1;
    }
    let shift = moved.unsigned_abs();
    // what came into view: the rows past them on the side the text moved away from, as many
    // as it moved, short of anything the program keeps in place
    let (kept, fresh): (Vec<usize>, Vec<usize>) = match moved > 0 {
        true => ((lo..=hi).rev().collect(), (hi + 1..n).take(shift).take_while(|&r| !fixed(r)).collect()),
        false => ((lo..=hi).collect(), (0..lo).rev().take(shift).take_while(|&r| !fixed(r)).collect()),
    };
    let region = match moved > 0 {
        true => (lo, hi + fresh.len()),
        false => (lo - fresh.len(), hi),
    };
    Some((moved, region, kept, fresh))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A full-screen program's view of a long transcript: a header, `rows` lines of it from
    /// `top`, a spinner that changes every frame, and an input box.
    fn view(transcript: &[String], top: usize, rows: usize, frame: usize) -> Vec<Line> {
        let row = |s: &str| Line { cells: format!("{s:<30}").chars().map(String::from).collect(), wrapped: false };
        let mut screen = vec![row("  Claude Code · ~/repo")];
        screen.extend(transcript[top..top + rows].iter().map(|s| row(s)));
        screen.push(row(&format!("{} Thinking… ({frame}s)", ["✻", "✶", "✳"][frame % 3])));
        screen.extend(["╭────────────────╮", "│ >              │", "╰────────────────╯"].map(row));
        screen
    }

    fn transcript() -> Vec<String> {
        // repeats on purpose: blank lines and closing braces look alike wherever they are
        (0..120).map(|i| match i % 6 {
            2 => String::new(),
            4 => "}".into(),
            _ => format!("line {i:03} of the reply"),
        }).collect()
    }

    fn lines(t: &[String], from: usize, to: usize) -> String {
        t[from..=to].join("\n")
    }

    impl Capture {
        fn follow_rows(&mut self, screen: Vec<Line>) -> bool {
            self.follow(screen, None)
        }
    }

    #[test]
    fn follows_the_text_down_and_back_up() {
        let t = transcript();
        let mut cap = Capture::new(view(&t, 0, 20, 0), (6, 0));
        for (frame, top) in [3, 6, 9, 12].into_iter().enumerate() {
            assert!(cap.follow_rows(view(&t, top, 20, frame + 1)), "scrolled to {top}");
        }
        // from transcript line 5 (row 6 when it started) to the last line now on screen (row 20
        // shows transcript line 12 + 19)
        cap.to = (cap.line_on(20).unwrap(), 29);
        assert_eq!(cap.selected(), lines(&t, 5, 31));
        // back up past where it started: nothing doubled, and the top grows
        for (frame, top) in [9, 6, 3, 0].into_iter().enumerate() {
            assert!(cap.follow_rows(view(&t, top, 20, frame + 10)));
        }
        cap.from = (cap.line_on(1).unwrap(), 0);
        assert_eq!(cap.selected(), lines(&t, 0, 31));
    }

    #[test]
    fn the_header_spinner_and_input_box_stay_out_of_the_text() {
        let t = transcript();
        let mut cap = Capture::new(view(&t, 10, 20, 0), (1, 0));
        assert!(cap.follow_rows(view(&t, 13, 20, 1)));
        assert!(cap.follow_rows(view(&t, 16, 20, 2)));
        (cap.from, cap.to) = ((cap.line_on(1).unwrap(), 0), (cap.line_on(20).unwrap(), 29));
        assert_eq!(cap.selected(), lines(&t, 16, 35));
        assert!(cap.line_on(21).is_none(), "the spinner row changed: it is nobody's line");
        assert_eq!(cap.line_on(0), Some(0), "the header stays put");
    }

    #[test]
    fn scrolling_up_from_where_it_started() {
        let t = transcript();
        // a selection from the end of row 10, which shows transcript line 69, dragged upwards
        let mut cap = Capture::new(view(&t, 60, 20, 0), (10, 29));
        for (frame, top) in (0..=57).rev().step_by(3).enumerate() {
            assert!(cap.follow_rows(view(&t, top, 20, frame + 1)), "scrolled up to {top}");
            cap.to = (cap.line_at(1).unwrap(), 0);
        }
        assert_eq!(cap.selected(), lines(&t, 0, 69));
        assert_eq!((cap.moving_edge(false), cap.moving_edge(true)), (Some(1), Some(20)));
    }

    #[test]
    fn a_spinner_alone_is_not_a_scroll() {
        let t = transcript();
        let mut cap = Capture::new(view(&t, 0, 20, 0), (6, 0));
        assert!(cap.follow_rows(view(&t, 0, 20, 1)));
        assert_eq!(cap.line_on(5), Some(5));
        assert_eq!(cap.lines.len(), 25);
    }

    #[test]
    fn something_else_altogether_stops_the_capture_where_it_was() {
        let t = transcript();
        let mut cap = Capture::new(view(&t, 0, 20, 0), (6, 0));
        assert!(cap.follow_rows(view(&t, 3, 20, 1)));
        let other: Vec<String> = (0..120).map(|i| format!("a different screen {i}")).collect();
        let mut elsewhere = view(&other, 0, 20, 2);
        elsewhere[0] = Line { cells: "help".chars().map(String::from).collect(), wrapped: false };
        elsewhere.truncate(21); // no input box either
        elsewhere.extend((0..4).map(|i| Line { cells: vec![format!("{i}")], wrapped: false }));
        assert!(!cap.follow(elsewhere, None));
        assert!(cap.lost());
        assert!(!cap.follow_rows(view(&t, 6, 20, 3)), "once lost, it stays as it was");
        assert_eq!(cap.text((1, 0), (23, 29)), lines(&t, 0, 22));
    }

    /// A screen drawn the way Claude Code scrolls: margins set around the transcript, the text
    /// moved one line within them, the margins reset, the new line drawn.
    #[test]
    fn follows_a_program_that_scrolls_with_margins() {
        let t: Vec<String> = (0..80).map(|i| if i % 3 == 1 { String::new() } else { format!("reply line {i:02}") }).collect();
        let mut p = vt100::Parser::new(12, 30, 0);
        let mut watch = ScrollWatch::default();
        let feed = |p: &mut vt100::Parser, watch: &mut ScrollWatch, bytes: &[u8]| {
            watch.feed(bytes, 12);
            p.process(bytes);
        };
        // rows 1..=8 show the transcript from line 40; a header above, a blank and an input box below
        let mut draw = format!("\x1b[?1049h\x1b[1;1Hheader");
        for r in 0..8 {
            draw += &format!("\x1b[{};1H{}", r + 2, t[40 + r]);
        }
        draw += "\x1b[11;1H\u{256d}\u{2500}\u{2500}\u{256e}\x1b[12;1H\u{2502} > \u{2502}";
        feed(&mut p, &mut watch, draw.as_bytes());
        let mut cap = Capture::new(lines_of(p.screen()), (8, 13)); // the end of row 8: line 47
        for top in (30..40).rev() {
            // one notch up: the text moves down a line, and line `top` is drawn on row 1
            let step = format!("\x1b[2;9r\x1b[1T\x1b[r\x1b[2;1H\x1b[2K{}", t[top]);
            // sometimes the bytes arrive in two reads, cut inside the sequence
            let (a, b) = step.as_bytes().split_at(4);
            feed(&mut p, &mut watch, a);
            feed(&mut p, &mut watch, b);
            assert!(cap.follow(lines_of(p.screen()), watch.take()), "up to {top}");
            cap.to = (cap.line_at(1).unwrap(), 0);
        }
        assert_eq!(cap.selected(), t[30..=47].join("\n"));
        assert_eq!((cap.moving_edge(false), cap.moving_edge(true)), (Some(1), Some(8)));
    }

    /// Scrolled back, Claude Code draws "Jump to bottom" over the last row of its transcript;
    /// each new row arrives under it, and is drawn again clear a line higher once the next
    /// one comes in.
    #[test]
    fn what_the_program_draws_over_its_text_is_not_copied() {
        let t: Vec<String> = (0..60).map(|i| format!("reply line {i:02} with some words")).collect();
        let mut p = vt100::Parser::new(10, 40, 0);
        let mut watch = ScrollWatch::default();
        let jump = "\x1b[8;18HJump to bottom (click)";
        let mut draw = String::from("\x1b[?1049h\x1b[1;1Hheader");
        for r in 0..7 {
            draw += &format!("\x1b[{};1H{}", r + 2, t[10 + r]);
        }
        draw += jump;
        draw += "\x1b[10;1H> ";
        p.process(draw.as_bytes());
        let mut cap = Capture::new(lines_of(p.screen()), (1, 0)); // from line 10, at row 1
        for top in 11..20 {
            // one notch down: the text moves up a line; the row that moved up is drawn clear,
            // the new bottom row gets the line and the overlay
            let step = format!("\x1b[2;8r\x1b[1S\x1b[r\x1b[7;1H\x1b[2K{}\x1b[8;1H\x1b[2K{}{jump}", t[top + 5], t[top + 6]);
            watch.feed(step.as_bytes(), 10);
            p.process(step.as_bytes());
            assert!(cap.follow(lines_of(p.screen()), watch.take()), "down to {top}");
        }
        cap.to = (cap.line_at(cap.moving_edge(true).unwrap()).unwrap(), 39);
        // through the last line shown clear; the one under the overlay isn't on screen yet
        assert_eq!(cap.selected(), t[10..=24].join("\n"));
    }

    #[test]
    fn the_watch_reads_scrolls_and_nothing_else() {
        let mut w = ScrollWatch::default();
        w.feed(b"\x1b[2;30r\x1b[1T\x1b[r\x1b[2;30r\x1b[2T\x1b[r", 40);
        assert_eq!(w.take(), Some(Scrolled { top: 1, bottom: 29, lines: -3 }));
        assert_eq!(w.take(), None, "taken");
        w.feed(b"\x1b[3S", 40);
        assert_eq!(w.take(), Some(Scrolled { top: 0, bottom: 39, lines: 3 }), "no margins: the whole screen");
        w.feed(b"\x1b[?2026h\x1b[38;2;1;2;3mtext\x1b[2;5H\x1b[K\x1b[?2026l", 40);
        assert_eq!(w.take(), None, "colours, moves and modes are not scrolls");
        w.feed(b"\x1b[2;10r\x1b[1S\x1b[r\x1b[1S", 40);
        assert_eq!(w.take(), None, "two places at once: no single answer");
        // Copilot's way: the cursor to the top margin, then reverse index once per line
        w.feed(b"\x1b[2;29r\x1b[2;1H\x1bM\x1bM\x1bM\x1b[r\x1b[3;4Hdrawn", 36);
        assert_eq!(w.take(), Some(Scrolled { top: 1, bottom: 28, lines: -3 }));
        // and the other way: line feeds at the bottom margin
        w.feed(b"\x1b[2;29r\x1b[29;1H\n\n\x1b[r", 36);
        assert_eq!(w.take(), Some(Scrolled { top: 1, bottom: 28, lines: 2 }));
        // line feeds and reverse indexes elsewhere just move the cursor
        w.feed(b"\x1b[2;29r\x1b[10;1H\n\n\x1bM\x1b[rtext\n\n", 36);
        assert_eq!(w.take(), None);
    }

    #[test]
    fn copying_part_of_a_line_and_joining_wrapped_rows() {
        let row = |s: &str, wrapped| Line { cells: s.chars().map(String::from).collect(), wrapped };
        let mut cap = Capture::new(vec![row("first half of a ", true), row("long line   ", false), row(" │ thinking   ", false)], (0, 6));
        cap.to = (2, 13);
        assert_eq!(cap.selected(), "half of a long line\nthinking");
        (cap.from, cap.to) = (cap.to, cap.from);
        assert_eq!(cap.selected(), "half of a long line\nthinking", "either way round");
    }
}

