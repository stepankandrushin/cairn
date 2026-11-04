//! The tray icon, drawn here rather than shipped: a cairn of three stones, with a badge for the light.

use crate::model::Light;

pub const SIZE: u32 = 64;

type Rgb = [u8; 3];

/// RGBA, `SIZE` square.
pub fn draw(light: Light) -> Vec<u8> {
    let mut px = vec![0u8; (SIZE * SIZE * 4) as usize];
    // Mid grey, seen on light and dark bars alike.
    let stone: Rgb = [0x8a, 0x93, 0x99];
    for (cx, cy, rx, ry) in [
        (28.0, 51.0, 24.0, 9.5),
        (29.0, 33.5, 17.0, 8.0),
        (27.5, 17.5, 10.5, 7.0),
    ] {
        ellipse(&mut px, cx, cy, rx, ry, stone);
    }
    let (bx, by, r) = (50.0, 47.0, 13.0);
    // A ring of nothing round the badge, so it stands off the stones.
    clear_circle(&mut px, bx, by, r + 2.5);
    let colour: Rgb = match light {
        Light::Synced => [0x2e, 0x9d, 0x4f],
        Light::Syncing => [0x2f, 0x7f, 0xd8],
        Light::Waiting => [0xe0, 0xa0, 0x20],
        Light::Paused => [0x8e, 0x8e, 0x8e],
        Light::Error => [0xd6, 0x45, 0x41],
        Light::NotRunning => [0x5c, 0x5c, 0x5c],
    };
    ellipse(&mut px, bx, by, r, r, colour);
    let white = [0xff, 0xff, 0xff];
    match light {
        Light::Paused => {
            rect(&mut px, bx - 6.0, by - 6.5, 4.0, 13.0, white);
            rect(&mut px, bx + 2.0, by - 6.5, 4.0, 13.0, white);
        }
        Light::Error => {
            rect(&mut px, bx - 2.0, by - 8.0, 4.0, 10.0, white);
            rect(&mut px, bx - 2.0, by + 4.0, 4.0, 4.0, white);
        }
        Light::NotRunning => rect(&mut px, bx - 7.0, by - 2.0, 14.0, 4.0, white),
        Light::Syncing => ellipse(&mut px, bx, by, 4.5, 4.5, white),
        Light::Synced | Light::Waiting => {}
    }
    px
}

/// Coverage of a pixel by an ellipse, from its distance to the edge: smooth edges at any size.
fn ellipse(px: &mut [u8], cx: f32, cy: f32, rx: f32, ry: f32, c: Rgb) {
    each(
        px,
        |x, y| {
            let (dx, dy) = ((x - cx) / rx, (y - cy) / ry);
            let d = ((dx * dx + dy * dy).sqrt() - 1.0) * rx.min(ry);
            (0.5 - d).clamp(0.0, 1.0)
        },
        Some(c),
    );
}

fn clear_circle(px: &mut [u8], cx: f32, cy: f32, r: f32) {
    each(
        px,
        |x, y| {
            let d = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt() - r;
            (0.5 - d).clamp(0.0, 1.0)
        },
        None,
    );
}

fn rect(px: &mut [u8], x0: f32, y0: f32, w: f32, h: f32, c: Rgb) {
    each(
        px,
        |x, y| {
            let cover = |p: f32, lo: f32, len: f32| {
                ((p + 0.5).min(lo + len) - (p - 0.5).max(lo)).clamp(0.0, 1.0)
            };
            cover(x, x0, w) * cover(y, y0, h)
        },
        Some(c),
    );
}

/// Paints `c` over each pixel by its coverage, or with `None` erases by it.
fn each(px: &mut [u8], coverage: impl Fn(f32, f32) -> f32, c: Option<Rgb>) {
    for y in 0..SIZE {
        for x in 0..SIZE {
            let a = coverage(x as f32 + 0.5, y as f32 + 0.5);
            if a <= 0.0 {
                continue;
            }
            let i = ((y * SIZE + x) * 4) as usize;
            let p = &mut px[i..i + 4];
            match c {
                None => p[3] = (p[3] as f32 * (1.0 - a)) as u8,
                Some(c) => {
                    let below = p[3] as f32 / 255.0;
                    let out = a + below * (1.0 - a);
                    for k in 0..3 {
                        let mixed = c[k] as f32 * a + p[k] as f32 * below * (1.0 - a);
                        p[k] = (mixed / out.max(1e-6)).round() as u8;
                    }
                    p[3] = (out * 255.0).round() as u8;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes each light's icon as raw RGBA to `$CAIRN_TRAY_ICONS`, to look at.
    #[test]
    #[ignore]
    fn dump() {
        let dir = std::path::PathBuf::from(std::env::var_os("CAIRN_TRAY_ICONS").unwrap());
        for l in [
            Light::Synced,
            Light::Syncing,
            Light::Waiting,
            Light::Paused,
            Light::Error,
            Light::NotRunning,
        ] {
            std::fs::write(dir.join(format!("{}.rgba", l.word())), draw(l)).unwrap();
        }
    }

    #[test]
    fn each_light_draws_a_different_badge() {
        let all = [
            Light::Synced,
            Light::Syncing,
            Light::Waiting,
            Light::Paused,
            Light::Error,
            Light::NotRunning,
        ];
        let images: Vec<_> = all.iter().map(|l| draw(*l)).collect();
        for (i, a) in images.iter().enumerate() {
            assert_eq!(a.len(), (SIZE * SIZE * 4) as usize);
            for b in &images[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }
}
