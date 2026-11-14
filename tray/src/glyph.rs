//! The menu's icons. macOS has its own, SF Symbols, named by [`Glyph::symbol`] and set by `native`,
//! which follow the menu's light or dark and its highlight. Windows has none for these, so they are
//! drawn here: strokes on a 16 unit grid, as its own icons are, in the ink of its menus' text.
//! A root's status shows its light's colour, the tray badge's, on both.

use crate::icon::{self, Rgb};
use crate::model::Light;

/// What an item of the menu shows by its name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Glyph {
    Settings,
    Update,
    Recent,
    Deleted,
    Problems,
    Conflicts,
    /// "Open in the window…", at the foot of a list.
    Window,
    Sync,
    Pause,
    Resume,
    Folder,
    Browser,
    Quit,
}

impl Glyph {
    #[cfg(test)]
    pub const ALL: [Glyph; 13] = [
        Glyph::Settings,
        Glyph::Update,
        Glyph::Recent,
        Glyph::Deleted,
        Glyph::Problems,
        Glyph::Conflicts,
        Glyph::Window,
        Glyph::Sync,
        Glyph::Pause,
        Glyph::Resume,
        Glyph::Folder,
        Glyph::Browser,
        Glyph::Quit,
    ];

    /// An item's, by the id `gui::build_menu` gives it; none for a line of a list or of a root's status.
    pub fn of(id: &str) -> Option<Glyph> {
        Some(match id.split_once(':') {
            None => match id {
                "settings" => Glyph::Settings,
                "update" => Glyph::Update,
                "recent" => Glyph::Recent,
                "quit" => Glyph::Quit,
                _ => return None,
            },
            Some(("menu", "deleted")) => Glyph::Deleted,
            Some(("menu", "problems")) => Glyph::Problems,
            Some(("menu", "conflicts")) => Glyph::Conflicts,
            Some(("open", _)) => Glyph::Window,
            Some(("sync", _)) => Glyph::Sync,
            Some(("pause", _)) => Glyph::Pause,
            Some(("resume", _)) => Glyph::Resume,
            Some(("folder", _)) => Glyph::Folder,
            Some(("web", _)) => Glyph::Browser,
            _ => return None,
        })
    }

    /// Its SF Symbol, for macOS: each there since macOS 11, the oldest the tray runs on.
    pub fn symbol(self) -> &'static str {
        match self {
            Glyph::Settings => "gearshape",
            Glyph::Update => "arrow.down.circle",
            Glyph::Recent => "clock",
            Glyph::Deleted => "trash",
            Glyph::Problems => "exclamationmark.triangle",
            Glyph::Conflicts => "arrow.triangle.branch",
            Glyph::Window => "macwindow",
            Glyph::Sync => "arrow.triangle.2.circlepath",
            Glyph::Pause => "pause",
            Glyph::Resume => "play",
            Glyph::Folder => "folder",
            Glyph::Browser => "globe",
            Glyph::Quit => "power",
        }
    }
}

/// Windows draws a menu item's bitmap at 16 px square, whatever it is given (muda's), so it is drawn
/// at that.
pub const SIZE: u32 = 16;

/// Its menus' text: white on the dark ones, black on the light, a little softened as the system's own
/// icons are.
pub fn ink(dark: bool) -> Rgb {
    if dark {
        [0xf2, 0xf2, 0xf2]
    } else {
        [0x1c, 0x1c, 0x1c]
    }
}

/// RGBA, `size` square: `g` in `ink`. Windows greys it itself where its item cannot be chosen.
pub fn draw(g: Glyph, ink: Rgb, size: u32) -> Vec<u8> {
    paint(&marks(g), ink, size)
}

/// RGBA, `size` square: a disc in the light's colour, by a root's status.
pub fn dot(light: Light, size: u32) -> Vec<u8> {
    paint(&[Mark::Disc((8.0, 8.0), 3.5)], icon::colour(light), size)
}

type Pt = (f32, f32);

/// What a glyph is drawn of, in units of its 16 unit grid.
enum Mark {
    /// A line through the points, of the width given, round at its ends and corners.
    Path(Vec<Pt>, f32),
    Disc(Pt, f32),
}

/// Lucide's stroke, 2 on its 24 unit grid.
const W: f32 = 4.0 / 3.0;

fn marks(g: Glyph) -> Vec<Mark> {
    use Mark::{Disc, Path};
    let line = |pts: &[Pt]| Path(pts.to_vec(), W);
    let c = (8.0, 8.0);
    match g {
        Glyph::Settings => {
            // Eight teeth, each a little narrower at its tip than at its root.
            let (out, inn, tip, root) = (7.0, 5.1, 9.0, 15.0);
            let mut pts = Vec::new();
            for i in 0..8 {
                let a = i as f32 * 45.0;
                pts.push(polar(c, inn, a - root));
                pts.push(polar(c, out, a - tip));
                pts.push(polar(c, out, a + tip));
                pts.push(polar(c, inn, a + root));
            }
            vec![Path(closed(pts), W), Path(circle(c, 2.1), W)]
        }
        Glyph::Update => vec![
            Path(circle(c, 6.5), W),
            line(&[(8.0, 4.6), (8.0, 11.3)]),
            line(&[(5.2, 8.6), (8.0, 11.4), (10.8, 8.6)]),
        ],
        Glyph::Recent => vec![
            Path(circle(c, 6.5), W),
            line(&[(8.0, 4.4), (8.0, 8.0), (10.6, 9.5)]),
        ],
        Glyph::Deleted => vec![
            line(&[(2.0, 4.0), (14.0, 4.0)]),
            line(&[(6.0, 4.0), (6.0, 2.0), (10.0, 2.0), (10.0, 4.0)]),
            line(&[(3.6, 4.0), (4.4, 14.0), (11.6, 14.0), (12.4, 4.0)]),
            line(&[(6.7, 7.0), (6.7, 11.0)]),
            line(&[(9.3, 7.0), (9.3, 11.0)]),
        ],
        Glyph::Problems => vec![
            Path(closed(vec![(8.0, 1.8), (14.8, 13.8), (1.2, 13.8)]), W),
            line(&[(8.0, 6.0), (8.0, 9.4)]),
            Disc((8.0, 11.7), 0.85),
        ],
        // Lucide's git-branch: one line, and a second leaving it for a copy of its own.
        Glyph::Conflicts => vec![
            line(&[(4.0, 2.0), (4.0, 10.0)]),
            Path(circle((12.0, 4.0), 2.0), W),
            Path(circle((4.0, 12.0), 2.0), W),
            Path(arc((6.0, 6.0), 6.0, 0.0, 90.0), W),
        ],
        Glyph::Window => vec![
            Path(round_rect((1.5, 2.5), (14.5, 13.5), 1.6), W),
            line(&[(1.5, 5.8), (14.5, 5.8)]),
        ],
        // Lucide's refresh-cw: two arcs, each with its head.
        Glyph::Sync => {
            let mut up = arc(c, 6.0, 180.0, 315.0);
            up.push((14.0, 5.4));
            let mut down = arc(c, 6.0, 0.0, 135.0);
            down.push((2.0, 10.6));
            vec![
                Path(up, W),
                line(&[(14.0, 2.0), (14.0, 5.4), (10.6, 5.4)]),
                Path(down, W),
                line(&[(5.4, 10.6), (2.0, 10.6), (2.0, 14.0)]),
            ]
        }
        Glyph::Pause => vec![
            Path(vec![(5.5, 3.5), (5.5, 12.5)], 2.4),
            Path(vec![(10.5, 3.5), (10.5, 12.5)], 2.4),
        ],
        Glyph::Resume => vec![Path(closed(vec![(4.5, 2.5), (13.2, 8.0), (4.5, 13.5)]), W)],
        Glyph::Folder => vec![Path(
            closed(vec![
                (1.5, 2.5),
                (5.6, 2.5),
                (7.2, 4.5),
                (14.5, 4.5),
                (14.5, 13.5),
                (1.5, 13.5),
            ]),
            W,
        )],
        // Lucide's globe: the earth's edge, a meridian's two halves, the equator.
        Glyph::Browser => {
            let r: f32 = 6.6;
            // Through both poles, of the radius Lucide gives them: centred 7 units off the axis.
            let (big, off) = (9.6, 7.0);
            let half = (r / big).asin().to_degrees();
            vec![
                Path(circle(c, r), W),
                Path(arc((8.0 + off, 8.0), big, 180.0 - half, 180.0 + half), W),
                Path(arc((8.0 - off, 8.0), big, -half, half), W),
                line(&[(1.4, 8.0), (14.6, 8.0)]),
            ]
        }
        Glyph::Quit => vec![
            line(&[(8.0, 1.5), (8.0, 7.8)]),
            Path(arc((8.0, 8.6), 6.0, -45.0, 225.0), W),
        ],
    }
}

/// The point at `deg` round `c`, clockwise from east as the screen's y runs down.
fn polar(c: Pt, r: f32, deg: f32) -> Pt {
    let a = deg.to_radians();
    (c.0 + r * a.cos(), c.1 + r * a.sin())
}

fn arc(c: Pt, r: f32, from: f32, to: f32) -> Vec<Pt> {
    let n = ((to - from).abs() / 5.0).ceil().max(1.0) as usize;
    (0..=n)
        .map(|i| polar(c, r, from + (to - from) * i as f32 / n as f32))
        .collect()
}

fn circle(c: Pt, r: f32) -> Vec<Pt> {
    arc(c, r, 0.0, 360.0)
}

fn closed(mut pts: Vec<Pt>) -> Vec<Pt> {
    pts.push(pts[0]);
    pts
}

fn round_rect(a: Pt, b: Pt, r: f32) -> Vec<Pt> {
    let mut pts = Vec::new();
    pts.extend(arc((b.0 - r, a.1 + r), r, -90.0, 0.0));
    pts.extend(arc((b.0 - r, b.1 - r), r, 0.0, 90.0));
    pts.extend(arc((a.0 + r, b.1 - r), r, 90.0, 180.0));
    pts.extend(arc((a.0 + r, a.1 + r), r, 180.0, 270.0));
    closed(pts)
}

fn to_segment(p: Pt, a: Pt, b: Pt) -> f32 {
    let (px, py) = (p.0 - a.0, p.1 - a.1);
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len = dx * dx + dy * dy;
    let t = if len > 0.0 {
        ((px * dx + py * dy) / len).clamp(0.0, 1.0)
    } else {
        0.0
    };
    ((px - t * dx).powi(2) + (py - t * dy).powi(2)).sqrt()
}

impl Mark {
    /// From `p` to its edge, in grid units: below 0 inside.
    fn edge(&self, p: Pt) -> f32 {
        match self {
            Mark::Path(pts, w) => {
                let near = pts
                    .windows(2)
                    .map(|s| to_segment(p, s[0], s[1]))
                    .fold(f32::MAX, f32::min);
                near - w / 2.0
            }
            Mark::Disc(c, r) => ((p.0 - c.0).powi(2) + (p.1 - c.1).powi(2)).sqrt() - r,
        }
    }
}

/// Each pixel covered by how far it is from the nearest edge: smooth edges at any size, as the
/// tray's icon is drawn.
fn paint(marks: &[Mark], ink: Rgb, size: u32) -> Vec<u8> {
    let unit = size as f32 / 16.0;
    let mut px = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let p = ((x as f32 + 0.5) / unit, (y as f32 + 0.5) / unit);
            let a = marks
                .iter()
                .map(|m| (0.5 - m.edge(p) * unit).clamp(0.0, 1.0))
                .fold(0.0, f32::max);
            if a > 0.0 {
                let i = ((y * size + x) * 4) as usize;
                px[i..i + 3].copy_from_slice(&ink);
                px[i + 3] = (a * 255.0).round() as u8;
            }
        }
    }
    px
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes each glyph as raw RGBA, 16 and 64 px square, to `$CAIRN_TRAY_GLYPHS`, to look at.
    #[test]
    #[ignore]
    fn dump() {
        let dir = std::path::PathBuf::from(std::env::var_os("CAIRN_TRAY_GLYPHS").unwrap());
        for g in Glyph::ALL {
            for size in [SIZE, 64] {
                let name = format!("{}-{size}.rgba", g.symbol());
                std::fs::write(dir.join(name), draw(g, ink(false), size)).unwrap();
            }
        }
        std::fs::write(dir.join("dot-16.rgba"), dot(Light::Synced, SIZE)).unwrap();
    }

    #[test]
    fn each_item_has_its_own_glyph_and_a_line_none() {
        let items = [
            ("settings", Glyph::Settings),
            ("update", Glyph::Update),
            ("recent", Glyph::Recent),
            ("menu:deleted", Glyph::Deleted),
            ("menu:problems", Glyph::Problems),
            ("menu:conflicts", Glyph::Conflicts),
            ("open:deleted", Glyph::Window),
            ("sync:r1", Glyph::Sync),
            ("pause:r1", Glyph::Pause),
            ("resume:r1", Glyph::Resume),
            ("folder:r1", Glyph::Folder),
            ("web:r1", Glyph::Browser),
            ("quit", Glyph::Quit),
        ];
        assert_eq!(items.len(), Glyph::ALL.len());
        for (id, g) in items {
            assert_eq!(Glyph::of(id), Some(g), "{id}");
        }
        // A list's lines and notes, and a root's status and its submenu, by their ids.
        for id in [
            "recent:3:0",
            "recent:none",
            "deleted",
            "problems",
            "conflict:r1:7",
            "note",
            "line:r1",
            "line:registry",
            "root:r1",
        ] {
            assert_eq!(Glyph::of(id), None, "{id}");
        }
    }

    #[test]
    fn each_glyph_draws_its_own_shape() {
        let all: Vec<Vec<u8>> = Glyph::ALL
            .iter()
            .map(|g| draw(*g, ink(false), SIZE))
            .collect();
        for (i, a) in all.iter().enumerate() {
            let alpha = |x: u32, y: u32| a[((y * SIZE + x) * 4 + 3) as usize];
            let inked = (0..SIZE * SIZE).filter(|p| alpha(p % SIZE, p / SIZE) > 128);
            assert!(
                inked.count() >= 12,
                "{:?} draws almost nothing",
                Glyph::ALL[i]
            );
            for b in &all[i + 1..] {
                assert_ne!(a, b);
            }
        }
        // In the ink asked for, and a root's light in its own colour.
        let inked = |px: &[u8], c: Rgb| px.chunks(4).filter(|p| p[3] > 0).all(|p| p[..3] == c);
        assert!(inked(&draw(Glyph::Sync, ink(true), SIZE), ink(true)));
        let waiting = Light::Waiting;
        assert!(inked(&dot(waiting, SIZE), icon::colour(waiting)));
    }
}
