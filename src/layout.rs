//! Split panes, tmux-style: a tree whose leaves are session ids and whose branches divide their
//! area side by side or stacked, at an adjustable ratio. Pure geometry — no terminals, no
//! sessions — so every rule about sizes lives, and is tested, here.
use ratatui::layout::Rect;
use serde_json::{json, Value};

/// The smallest pane worth having: a header line plus four terminal rows (vt100 panics below
/// one), twenty columns wide.
pub const MIN_W: u16 = 20;
pub const MIN_H: u16 = 5;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Dir {
    /// side by side, with a vertical divider
    Across,
    /// one above the other, with a horizontal divider
    Down,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    /// a pane showing the session with this id (ids stay put when sessions are reordered)
    Leaf(usize),
    Split { dir: Dir, ratio: f32, a: Box<Node>, b: Box<Node> },
}

/// A divider between two panes: where it is drawn, and what dragging it adjusts.
#[derive(Clone, Debug, PartialEq)]
pub struct Divider {
    /// which branch, from the root: false = first child, true = second
    pub path: Vec<bool>,
    pub dir: Dir,
    /// the one-cell line itself
    pub rect: Rect,
    /// the area the split divides, for turning a mouse position into a ratio
    pub parent: Rect,
}

impl Node {
    pub fn leaves(&self) -> Vec<usize> {
        match self {
            Node::Leaf(id) => vec![*id],
            Node::Split { a, b, .. } => [a.leaves(), b.leaves()].concat(),
        }
    }

    pub fn contains(&self, id: usize) -> bool {
        self.leaves().contains(&id)
    }

    /// Splits the pane showing `at`: it keeps the first half, `new` gets the second.
    pub fn split(&mut self, at: usize, dir: Dir, new: usize) -> bool {
        self.split_placed(at, dir, new, false)
    }

    /// The same, with `new` in the first half (left or top) when `new_first`.
    pub fn split_placed(&mut self, at: usize, dir: Dir, new: usize, new_first: bool) -> bool {
        match self {
            Node::Leaf(id) if *id == at => {
                let (a, b) = if new_first { (new, at) } else { (at, new) };
                *self = Node::Split { dir, ratio: 0.5, a: Box::new(Node::Leaf(a)), b: Box::new(Node::Leaf(b)) };
                true
            }
            Node::Leaf(_) => false,
            Node::Split { a, b, .. } => a.split_placed(at, dir, new, new_first) || b.split_placed(at, dir, new, new_first),
        }
    }

    /// Shows `new` where `old` was.
    pub fn replace(&mut self, old: usize, new: usize) -> bool {
        match self {
            Node::Leaf(id) if *id == old => {
                *id = new;
                true
            }
            Node::Leaf(_) => false,
            Node::Split { a, b, .. } => a.replace(old, new) || b.replace(old, new),
        }
    }

    /// Closes the pane showing `id`; its neighbour takes the space. The last pane cannot be
    /// closed (there must be somewhere to show a session), so that returns false.
    pub fn remove(&mut self, id: usize) -> bool {
        let Node::Split { a, b, .. } = self else { return false };
        let survivor = match (&**a, &**b) {
            (Node::Leaf(x), _) if *x == id => Some(b.clone()),
            (_, Node::Leaf(x)) if *x == id => Some(a.clone()),
            _ => None,
        };
        match survivor {
            Some(node) => {
                *self = *node;
                true
            }
            None => a.remove(id) || b.remove(id),
        }
    }

    /// Where each pane goes in `area`, in reading order.
    pub fn rects(&self, area: Rect) -> Vec<(usize, Rect)> {
        match self {
            Node::Leaf(id) => vec![(*id, area)],
            Node::Split { dir, ratio, a, b } => {
                let (ra, _, rb) = divide(area, *dir, *ratio);
                [a.rects(ra), b.rects(rb)].concat()
            }
        }
    }

    pub fn dividers(&self, area: Rect) -> Vec<Divider> {
        let mut out = vec![];
        self.collect_dividers(area, &mut vec![], &mut out);
        out
    }

    fn collect_dividers(&self, area: Rect, path: &mut Vec<bool>, out: &mut Vec<Divider>) {
        let Node::Split { dir, ratio, a, b } = self else { return };
        let (ra, line, rb) = divide(area, *dir, *ratio);
        out.push(Divider { path: path.clone(), dir: *dir, rect: line, parent: area });
        path.push(false);
        a.collect_dividers(ra, path, out);
        path.pop();
        path.push(true);
        b.collect_dividers(rb, path, out);
        path.pop();
    }

    /// Sets the ratio of the split at `path`. Callers clamp it (see `ratio_at`).
    pub fn set_ratio(&mut self, path: &[bool], to: f32) {
        match (self, path.split_first()) {
            (Node::Split { ratio, .. }, None) => *ratio = to,
            (Node::Split { a, b, .. }, Some((&second, rest))) => {
                if second { b.set_ratio(rest, to) } else { a.set_ratio(rest, to) }
            }
            _ => {}
        }
    }

    /// Leaves as their position in a saved list, splits as objects. `index_of` maps a session id
    /// to its position; a pane whose session is gone is simply left out.
    pub fn to_json(&self, index_of: &impl Fn(usize) -> Option<usize>) -> Value {
        match self {
            Node::Leaf(id) => index_of(*id).map_or(Value::Null, |i| json!(i)),
            Node::Split { dir, ratio, a, b } => json!({
                "dir": if *dir == Dir::Across { "across" } else { "down" },
                "ratio": ratio,
                "a": a.to_json(index_of),
                "b": b.to_json(index_of),
            }),
        }
    }

    /// The reverse, forgiving about what it finds: a pane whose session did not come back is
    /// dropped and its neighbour takes the space, and anything unreadable yields None.
    pub fn from_json(v: &Value, id_of: &impl Fn(usize) -> Option<usize>) -> Option<Node> {
        if let Some(i) = v.as_u64() {
            return id_of(i as usize).map(Node::Leaf);
        }
        let dir = match v["dir"].as_str()? {
            "across" => Dir::Across,
            "down" => Dir::Down,
            _ => return None,
        };
        let ratio = (v["ratio"].as_f64().unwrap_or(0.5) as f32).clamp(0.05, 0.95);
        match (Node::from_json(&v["a"], id_of), Node::from_json(&v["b"], id_of)) {
            (Some(a), Some(b)) => Some(Node::Split { dir, ratio, a: Box::new(a), b: Box::new(b) }),
            (one, other) => one.or(other),
        }
    }
}

/// Cuts `area` in two with a one-cell divider between; each side keeps at least the minimum
/// pane size when the area has room for two.
fn divide(area: Rect, dir: Dir, ratio: f32) -> (Rect, Rect, Rect) {
    match dir {
        Dir::Across => {
            let room = area.width.saturating_sub(1);
            let a = ((room as f32 * ratio).round() as u16).clamp(MIN_W.min(room / 2), room.saturating_sub(MIN_W.min(room / 2)));
            let line = Rect::new(area.x + a, area.y, area.width.min(1), area.height);
            (Rect { width: a, ..area }, line, Rect::new(area.x + a + 1, area.y, room - a, area.height))
        }
        Dir::Down => {
            let room = area.height.saturating_sub(1);
            let a = ((room as f32 * ratio).round() as u16).clamp(MIN_H.min(room / 2), room.saturating_sub(MIN_H.min(room / 2)));
            let line = Rect::new(area.x, area.y + a, area.width, area.height.min(1));
            (Rect { height: a, ..area }, line, Rect::new(area.x, area.y + a + 1, area.width, room - a))
        }
    }
}

/// Where a session dragged onto a pane lands: near an edge it opens on that side, in the middle
/// it replaces what the pane shows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Zone {
    Left,
    Right,
    Top,
    Bottom,
    Center,
}

impl Zone {
    /// The zone under (`col`, `row`) in `pane`: the nearest edge within a quarter of the pane,
    /// else the middle.
    pub fn at(pane: Rect, col: u16, row: u16) -> Zone {
        let fx = (col.saturating_sub(pane.x) as f32 + 0.5) / pane.width.max(1) as f32;
        let fy = (row.saturating_sub(pane.y) as f32 + 0.5) / pane.height.max(1) as f32;
        let edges = [(fx, Zone::Left), (1.0 - fx, Zone::Right), (fy, Zone::Top), (1.0 - fy, Zone::Bottom)];
        let (near, zone) = edges.into_iter().min_by(|a, b| a.0.total_cmp(&b.0)).unwrap_or((1.0, Zone::Center));
        if near > 0.25 { Zone::Center } else { zone }
    }

    /// How the pane splits for this zone: (direction, new session goes first), or None to replace.
    pub fn split(self) -> Option<(Dir, bool)> {
        match self {
            Zone::Left => Some((Dir::Across, true)),
            Zone::Right => Some((Dir::Across, false)),
            Zone::Top => Some((Dir::Down, true)),
            Zone::Bottom => Some((Dir::Down, false)),
            Zone::Center => None,
        }
    }

    /// The part of `pane` the session would take, for outlining while you drag.
    pub fn area(self, pane: Rect) -> Rect {
        let (hw, hh) = (pane.width / 2, pane.height / 2);
        match self {
            Zone::Left => Rect { width: hw, ..pane },
            Zone::Right => Rect { x: pane.x + hw, width: pane.width - hw, ..pane },
            Zone::Top => Rect { height: hh, ..pane },
            Zone::Bottom => Rect { y: pane.y + hh, height: pane.height - hh, ..pane },
            Zone::Center => pane,
        }
    }
}

/// Whether a pane this size can be split that way, leaving both halves usable.
pub fn fits(pane: Rect, dir: Dir) -> bool {
    match dir {
        Dir::Across => pane.width > 2 * MIN_W,
        Dir::Down => pane.height > 2 * MIN_H,
    }
}

/// The ratio a divider dragged to (`col`, `row`) asks for, clamped so neither side shrinks
/// below the minimum pane.
pub fn ratio_at(d: &Divider, col: u16, row: u16) -> f32 {
    let (pos, start, len, min) = match d.dir {
        Dir::Across => (col, d.parent.x, d.parent.width, MIN_W),
        Dir::Down => (row, d.parent.y, d.parent.height, MIN_H),
    };
    let room = len.saturating_sub(1).max(1) as f32;
    let lo = (min as f32 / room).min(0.5);
    ((pos.saturating_sub(start)) as f32 / room).clamp(lo, 1.0 - lo)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect::new(38, 1, 160, 48)
    }

    /// root: [1 | [2 / 3]] — one pane on the left, two stacked on the right
    fn three() -> Node {
        let mut n = Node::Leaf(1);
        assert!(n.split(1, Dir::Across, 2));
        assert!(n.split(2, Dir::Down, 3));
        n
    }

    #[test]
    fn splits_and_closes_like_tmux() {
        let mut n = three();
        assert_eq!(n.leaves(), [1, 2, 3], "reading order");
        assert!(!n.split(9, Dir::Down, 4), "no pane shows session 9");

        assert!(n.replace(3, 7));
        assert_eq!(n.leaves(), [1, 2, 7]);

        assert!(n.remove(2));
        assert_eq!(n, Node::Split { dir: Dir::Across, ratio: 0.5, a: Box::new(Node::Leaf(1)), b: Box::new(Node::Leaf(7)) });
        assert!(n.remove(1));
        assert_eq!(n, Node::Leaf(7), "the neighbour takes the space");
        assert!(!n.remove(7), "the last pane stays");
    }

    #[test]
    fn panes_and_dividers_tile_the_area_exactly() {
        for n in [Node::Leaf(1), three(), {
            let mut t = three();
            t.split(1, Dir::Down, 4);
            t.split(4, Dir::Across, 5);
            t
        }] {
            let (rects, lines) = (n.rects(area()), n.dividers(area()));
            let cells: u32 = rects.iter().map(|(_, r)| r.area()).chain(lines.iter().map(|d| d.rect.area())).sum();
            assert_eq!(cells, area().area(), "every cell belongs to exactly one pane or divider");
            let all: Vec<Rect> = rects.iter().map(|(_, r)| *r).chain(lines.iter().map(|d| d.rect)).collect();
            for (i, x) in all.iter().enumerate() {
                assert!(area().contains(x.as_position()) && x.right() <= area().right() && x.bottom() <= area().bottom());
                for y in &all[i + 1..] {
                    assert!(!x.intersects(*y), "{x:?} overlaps {y:?}");
                }
            }
            for (id, r) in rects {
                assert!(r.width >= MIN_W && r.height >= MIN_H, "pane {id} is usable: {r:?}");
            }
        }
    }

    #[test]
    fn dragging_a_divider_never_squeezes_a_pane_away() {
        let mut n = three();
        let d = n.dividers(area())[0].clone();
        assert_eq!((d.path.clone(), d.dir), (vec![], Dir::Across));
        for col in [0, 38, 40, 120, 197, 400] {
            n.set_ratio(&d.path, ratio_at(&d, col, 0));
            for (_, r) in n.rects(area()) {
                assert!(r.width >= MIN_W, "dragged to column {col}: {r:?}");
            }
        }
        let inner = n.dividers(area())[1].clone();
        assert_eq!((inner.path.clone(), inner.dir), (vec![true], Dir::Down), "the right-hand stack");
        n.set_ratio(&inner.path, 0.25);
        let heights: Vec<u16> = n.rects(area()).iter().skip(1).map(|(_, r)| r.height).collect();
        assert!(heights[0] < heights[1], "the upper pane shrank: {heights:?}");
    }

    #[test]
    fn drop_zones() {
        let p = Rect::new(40, 2, 100, 30);
        assert_eq!(Zone::at(p, 138, 16), Zone::Right);
        assert_eq!(Zone::at(p, 41, 16), Zone::Left);
        assert_eq!(Zone::at(p, 90, 2), Zone::Top);
        assert_eq!(Zone::at(p, 90, 31), Zone::Bottom);
        assert_eq!(Zone::at(p, 90, 16), Zone::Center);
        assert_eq!(Zone::Right.area(p), Rect::new(90, 2, 50, 30));

        let mut n = Node::Leaf(1);
        assert!(n.split_placed(1, Dir::Across, 2, true));
        assert_eq!(n.leaves(), [2, 1], "dropped on the left edge: new session first");
    }

    #[test]
    fn only_splits_what_has_room() {
        assert!(fits(Rect::new(0, 0, 41, 11), Dir::Across));
        assert!(!fits(Rect::new(0, 0, 40, 11), Dir::Across));
        assert!(fits(Rect::new(0, 0, 40, 11), Dir::Down));
        assert!(!fits(Rect::new(0, 0, 40, 10), Dir::Down));
    }

    #[test]
    fn saves_and_restores_forgivingly() {
        let n = three();
        // sessions saved in the order 3, 1, 2
        let saved = [3, 1, 2];
        let v = n.to_json(&|id| saved.iter().position(|&s| s == id));
        // restored with new ids 30, 10, 20
        let fresh = [30, 10, 20];
        let back = Node::from_json(&v, &|i| fresh.get(i).copied()).unwrap();
        assert_eq!(back.leaves(), [10, 20, 30], "same shape, new ids");

        let lost = Node::from_json(&v, &|i| if i == 1 { None } else { fresh.get(i).copied() }).unwrap();
        assert_eq!(lost.leaves(), [20, 30], "a session that didn't come back leaves no hole");
        assert_eq!(Node::from_json(&json!({"dir": "sideways"}), &|i| Some(i)), None);
        assert_eq!(Node::from_json(&Value::Null, &|i| Some(i)), None);
    }
}
