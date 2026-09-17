//! The chud: a cute pixel critter for each session. "Chud" means eating a lot, so it gets
//! fatter the longer its agent works. Drawn with half-block pixels, like Claude's crab.
use ratatui::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::Duration;

/// Draw with background-coloured spaces instead of block glyphs (▀ ▄ █ ▏…). Some fonts paint
/// those glyphs wider than their cell, over the neighbouring one, which turns a row of chuds
/// into what looks like several of them piled on each other. A space has nothing to paint.
static SAFE: AtomicBool = AtomicBool::new(false);

pub fn set_safe(on: bool) {
    SAFE.store(on, Relaxed);
}

fn safe() -> bool {
    SAFE.load(Relaxed)
}

const BODY: Color = Color::Rgb(0xff, 0xc2, 0x7a);
const EDGE: Color = Color::Rgb(0xe8, 0x94, 0x4f);
const BELLY: Color = Color::Rgb(0xff, 0xe6, 0xc2);
const EYE: Color = Color::Rgb(0x2b, 0x1d, 0x16);
const SHINE: Color = Color::Rgb(0xff, 0xff, 0xff);
const BLUSH: Color = Color::Rgb(0xff, 0x8f, 0xa8);
const MOUTH: Color = Color::Rgb(0x9c, 0x33, 0x3f);

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mood {
    Munching,
    Waiting,
    Happy,
    Sleepy,
}

/// 0..=4 by minutes the agent has spent working: snack, meal, feast, buffet, food coma.
pub fn fatness(worked: Duration) -> usize {
    match worked.as_secs() / 60 {
        0..5 => 0,
        5..20 => 1,
        20..60 => 2,
        60..180 => 3,
        _ => 4,
    }
}

pub type Pixels = Vec<Vec<Option<Color>>>;

fn put(px: &mut Pixels, x: usize, y: usize, c: Color) {
    if let Some(p) = px.get_mut(y).and_then(|row| row.get_mut(x)) {
        *p = Some(c);
    }
}

/// A chud 10 px tall (5 lines, for dashboard cards) that widens with `fat`;
/// `frame` alternates the chewing animation.
pub fn sprite(fat: usize, mood: Mood, frame: u64) -> Pixels {
    let fat = fat.min(4);
    let (h, w) = (10, 10 + 2 * fat);
    let body_h = h - 1; // the bottom row is feet
    let inside = |x: isize, y: isize| {
        if x < 0 || y < 0 || x >= w as isize || y >= body_h as isize {
            return false;
        }
        let dx = (x as f32 + 0.5 - w as f32 / 2.0) / (w as f32 / 2.0);
        let dy = (y as f32 + 0.5 - body_h as f32 / 2.0) / (body_h as f32 / 2.0);
        dx * dx + dy * dy <= 1.0
    };
    let mut px: Pixels = vec![vec![None; w]; h];
    for y in 0..body_h {
        for x in 0..w {
            let (xi, yi) = (x as isize, y as isize);
            if inside(xi, yi) {
                let edge = [(1, 0), (-1, 0), (0, 1), (0, -1)].iter().any(|(a, b)| !inside(xi + a, yi + b));
                put(&mut px, x, y, if edge { EDGE } else { BODY });
            }
        }
    }
    let chewing = mood == Mood::Munching && frame % 2 == 0;

    // belly: a lighter patch below the face that grows with the body
    let (bx, by) = (w as f32 / 2.0, body_h as f32 * 0.74);
    let (rx, ry) = (w as f32 * 0.32, body_h as f32 * 0.22);
    for y in 5..body_h {
        for x in 0..w {
            let dx = (x as f32 + 0.5 - bx) / rx;
            let dy = (y as f32 + 0.5 - by) / ry;
            if dx * dx + dy * dy <= 1.0 && px[y][x] == Some(BODY) {
                put(&mut px, x, y, BELLY);
            }
        }
    }

    let (c0, c1, d) = (w / 2 - 1, w / 2, 1 + fat / 2);
    let (left, right) = (c0 - d, c1 + d); // inner pixel of each 2x2 eye
    for (inner, outer) in [(left, left - 1), (right, right + 1)] {
        if mood != Mood::Sleepy {
            put(&mut px, inner, 3, SHINE);
            put(&mut px, outer, 3, EYE);
        }
        put(&mut px, inner, 4, EYE);
        put(&mut px, outer, 4, EYE);
    }
    put(&mut px, left - 1, 5, BLUSH);
    put(&mut px, right + 1, 5, BLUSH);
    let mouth = match mood {
        Mood::Munching if chewing => vec![(c0, 5, MOUTH), (c1, 5, MOUTH), (c0, 6, MOUTH), (c1, 6, MOUTH)],
        Mood::Munching | Mood::Sleepy => vec![(c0, 6, EDGE), (c1, 6, EDGE)],
        Mood::Waiting => vec![(c0, 6, MOUTH), (c1, 6, MOUTH)],
        Mood::Happy => vec![(c0 - 1, 5, EYE), (c1 + 1, 5, EYE), (c0, 6, EYE), (c1, 6, EYE)],
    };
    for (x, y, c) in mouth {
        put(&mut px, x, y, c);
    }
    let foot = c0 - 1 - fat / 2;
    for x in [foot - 1, foot, w - 1 - foot, w - foot] {
        put(&mut px, x, h - 1, EDGE);
    }
    px
}

/// The chud as text, in whichever style this terminal can show (see `set_safe`).
pub fn art(px: &Pixels) -> Vec<Line<'static>> {
    if safe() { lines_safe(px) } else { lines(px) }
}

/// Renders pixels two rows per line: ▀ takes the top pixel's colour, its background the bottom's.
pub fn lines(px: &Pixels) -> Vec<Line<'static>> {
    px.chunks(2)
        .map(|rows| {
            let bottom = rows.get(1);
            let cells = (0..rows[0].len()).map(|x| match (rows[0][x], bottom.and_then(|r| r[x])) {
                (Some(t), Some(b)) => Span::styled("▀", Style::new().fg(t).bg(b)),
                (Some(t), None) => Span::styled("▀", Style::new().fg(t)),
                (None, Some(b)) => Span::styled("▄", Style::new().fg(b)),
                (None, None) => Span::raw(" "),
            });
            Line::from(cells.collect::<Vec<_>>())
        })
        .collect()
}

/// Two pixel rows per line again, but painted as spaces, so a line holds one colour per cell,
/// not two. Where the pair disagrees the more telling pixel wins — an eye over the body, a
/// mouth over the belly — so the face reads the same and only the outline gets chunkier.
pub fn lines_safe(px: &Pixels) -> Vec<Line<'static>> {
    let weight = |c: Option<Color>| match c {
        None => 0,
        Some(EYE) => 7,
        Some(SHINE) => 6,
        Some(MOUTH) => 5,
        Some(BLUSH) => 4,
        Some(EDGE) => 3,
        Some(BELLY) => 2,
        Some(_) => 1,
    };
    px.chunks(2)
        .map(|rows| {
            let cells = (0..rows[0].len()).map(|x| {
                let (top, bottom) = (rows[0][x], rows.get(1).and_then(|r| r[x]));
                match if weight(bottom) > weight(top) { bottom } else { top } {
                    Some(c) => Span::styled(" ", Style::new().bg(c)),
                    None => Span::raw(" "),
                }
            });
            Line::from(cells.collect::<Vec<_>>())
        })
        .collect()
}

/// A progress bar whose fill runs green → amber → red across its own length, so how full it is
/// reads from the colour as well as the length. One span per cell; the empty part is one more.
pub fn bar(ratio: f64, width: usize) -> Vec<Span<'static>> {
    const TRACK: Color = Color::Rgb(0x33, 0x33, 0x3a);
    const STOPS: [(u8, u8, u8); 3] = [(0x8b, 0xd4, 0x7a), (0xf5, 0xc1, 0x5a), (0xf2, 0x6d, 0x6d)];
    let r = ratio.clamp(0.0, 1.0);
    let eighths = (r * width as f64 * 8.0).round() as usize;
    let (full, part) = (eighths / 8, eighths % 8);
    let colour = |cell: usize| {
        // where this cell sits along the bar, mixed between the two stops it falls between
        let t = if width > 1 { cell as f64 / (width - 1) as f64 * 2.0 } else { 0.0 };
        let (a, b, f) = (STOPS[t as usize % 3], STOPS[(t as usize + 1).min(2)], t.fract());
        let mix = |a: u8, b: u8| (a as f64 + (b as f64 - a as f64) * f).round() as u8;
        Color::Rgb(mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2))
    };
    if safe() {
        // whole cells only: the eighth-width tips are block glyphs too
        let filled = (r * width as f64).round() as usize;
        let mut spans: Vec<Span<'static>> =
            (0..filled.min(width)).map(|i| Span::styled(" ", Style::new().bg(colour(i)))).collect();
        if width > spans.len() {
            spans.push(Span::styled(" ".repeat(width - spans.len()), Style::new().bg(TRACK)));
        }
        return spans;
    }
    let mut spans: Vec<Span<'static>> = (0..full.min(width))
        .map(|i| Span::styled("█", Style::new().fg(colour(i)).bg(TRACK)))
        .collect();
    if part > 0 && full < width {
        let tip = [' ', '\u{258f}', '\u{258e}', '\u{258d}', '\u{258c}', '\u{258b}', '\u{258a}', '\u{2589}'][part];
        spans.push(Span::styled(tip.to_string(), Style::new().fg(colour(full)).bg(TRACK)));
    }
    let empty = width - spans.len();
    if empty > 0 {
        spans.push(Span::styled(" ".repeat(empty), Style::new().bg(TRACK)));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ascii(px: &Pixels) -> String {
        let ch = |c: Option<Color>| match c {
            None => ' ',
            Some(BODY) => 'o',
            Some(EDGE) => '#',
            Some(BELLY) => '.',
            Some(EYE) => '@',
            Some(SHINE) => '*',
            Some(BLUSH) => '+',
            Some(MOUTH) => 'm',
            Some(_) => '?',
        };
        px.iter().map(|r| r.iter().map(|&c| ch(c)).collect::<String>() + "\n").collect()
    }

    #[test]
    fn grows_with_work() {
        let widths: Vec<usize> = (0..5).map(|f| sprite(f, Mood::Happy, 0)[0].len()).collect();
        assert_eq!(widths, [10, 12, 14, 16, 18]);
        assert_eq!(fatness(Duration::from_secs(3 * 60)), 0);
        assert_eq!(fatness(Duration::from_secs(90 * 60)), 3);
        assert_eq!(lines(&sprite(2, Mood::Happy, 0)).len(), 5, "10 px tall = 5 lines");
    }

    #[test]
    fn chews_while_working() {
        assert_ne!(sprite(1, Mood::Munching, 0), sprite(1, Mood::Munching, 1));
        assert_eq!(sprite(1, Mood::Happy, 0), sprite(1, Mood::Happy, 1));
    }

    #[test]
    fn progress_bar() {
        let text = |r, w| bar(r, w).iter().map(|s| s.content.as_ref()).collect::<String>();
        assert_eq!(text(0.0, 4), "    ");
        assert_eq!(text(0.5, 4), "██  ");
        assert_eq!(text(1.0, 4), "████");
        assert_eq!(text(0.53, 4), "██▏ ");
        assert_eq!(text(3.0, 4), "████", "clamped");
        let colours: Vec<_> = bar(1.0, 4).iter().map(|s| s.style.fg).collect();
        assert_eq!(colours.iter().collect::<std::collections::HashSet<_>>().len(), 4, "each cell its own colour");
    }

    /// The safe style draws no glyph a font could smear, keeps the card's five lines, and still
    /// tells every mood apart — which is the whole point of the chud.
    #[test]
    fn safe_style() {
        let text = |ls: &[Line]| ls.iter().flat_map(|l| l.spans.iter()).map(|s| s.content.to_string()).collect::<String>();
        let moods = [(Mood::Happy, 0), (Mood::Munching, 0), (Mood::Munching, 1), (Mood::Waiting, 0), (Mood::Sleepy, 0)];
        let mut seen = std::collections::HashSet::new();
        for fat in 0..=4 {
            for (mood, frame) in moods {
                let px = sprite(fat, mood, frame);
                let ls = lines_safe(&px);
                assert_eq!(ls.len(), 5, "fits the card's five art lines");
                assert!(ls.iter().all(|l| l.width() == px[0].len()), "one cell per pixel column");
                assert!(!text(&ls).contains(['▀', '▄', '█']), "no block glyphs at all");
                let bgs: Vec<_> = ls.iter().flat_map(|l| l.spans.iter().map(|s| s.style.bg)).collect();
                assert!(bgs.contains(&Some(EYE)), "{mood:?} fat {fat}: the eyes survive");
                if fat == 2 {
                    assert!(seen.insert(format!("{bgs:?}")), "{mood:?} frame {frame} looks like another mood");
                }
            }
        }
        set_safe(true);
        let bar_text = bar(0.5, 6).iter().map(|s| s.content.to_string()).collect::<String>();
        set_safe(false);
        assert!(!bar_text.contains(['█', '▏', '▌']), "the bar follows the same switch: {bar_text:?}");
        assert_eq!(bar_text.chars().count(), 6);
    }

    // `cargo test preview -- --nocapture` prints every sprite, for eyeballing the art
    #[test]
    fn preview() {
        // the safe style, back into the same letters: one line per text row, as it will show
        let safe = |px: &Pixels| -> String {
            let as_px: Pixels = lines_safe(px).iter().map(|l| l.spans.iter().map(|s| s.style.bg).collect()).collect();
            ascii(&as_px)
        };
        for mood in [Mood::Happy, Mood::Munching, Mood::Waiting, Mood::Sleepy] {
            for fat in [0, 2, 4] {
                let px = sprite(fat, mood, 0);
                println!("{mood:?} fat {fat}\n{}safe:\n{}", ascii(&px), safe(&px));
            }
        }
    }
}
