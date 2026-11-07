use gpui::{Hsla, Rgba};

fn linear_to_srgb(v: f32) -> f32 {
    let v = v.clamp(0.0, 1.0);
    if v <= 0.003_130_8 { 12.92 * v } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 }
}

fn oklch_linear(l: f32, c: f32, h: f32) -> [f32; 3] {
    let (a, b) = (c * h.to_radians().cos(), c * h.to_radians().sin());
    let (x, y, z) = (l + 0.396_337_78 * a + 0.215_803_76 * b, l - 0.105_561_346 * a - 0.063_854_17 * b, l - 0.089_484_18 * a - 1.291_485_5 * b);
    let (x, y, z) = (x * x * x, y * y * y, z * z * z);
    [4.076_741_7 * x - 3.307_711_6 * y + 0.230_969_94 * z, -1.268_438 * x + 2.609_757_4 * y - 0.341_319_4 * z, -0.004_196_086 * x - 0.703_418_6 * y + 1.707_614_7 * z]
}

fn oklch(l: f32, c: f32, h: f32) -> Hsla {
    let [r, g, b] = oklch_linear(l, c, h);
    Rgba { r: linear_to_srgb(r), g: linear_to_srgb(g), b: linear_to_srgb(b), a: 1.0 }.into()
}

const NEUTRAL: f32 = 265.0;

macro_rules! tokens {
    ($($(#[$doc:meta])* $name:ident = ($l:expr, $c:expr, $h:expr);)*) => {
        $($(#[$doc])* pub fn $name() -> Hsla { oklch($l, $c, $h) })*
        const ALL: &[(&str, (f32, f32, f32))] = &[$((stringify!($name), ($l, $c, $h))),*];
    };
}

tokens! {
    /// The window background: true black, like a Windows console.
    bg = (0.0, 0.0, NEUTRAL);
    /// Side panels and title bar.
    surface = (0.13, 0.0, NEUTRAL);
    /// Composer, code blocks and hovered rows.
    raised = (0.2, 0.0, NEUTRAL);
    /// Palettes and popovers.
    overlay = (0.24, 0.0, NEUTRAL);
    /// Hairlines and resting borders.
    line = (0.34, 0.0, NEUTRAL);
    /// Body text.
    text = (0.96, 0.0, NEUTRAL);
    /// Secondary text.
    dim = (0.74, 0.0, NEUTRAL);
    /// Emphasis, keyboard focus and the one filled control.
    accent = (1.0, 0.0, NEUTRAL);
    /// Tool names and links.
    bright = (0.86, 0.0, NEUTRAL);
    /// Only for finished work and added lines.
    green = (0.8, 0.15, 150.0);
    /// Only for failures, limits, removed lines and the permissions warning.
    red = (0.74, 0.16, 25.0);
    /// Only for the Claude mark.
    claude = (0.676, 0.13, 39.0);
}

/// The same tokens as CSS variables, so the phone page can't drift from the app.
pub fn css() -> String {
    let tokens: String = ALL.iter().map(|(name, (l, c, h))| format!("--{name}:oklch({l} {c} {h});")).collect();
    format!(":root{{{tokens}}}")
}

/// A translucent version of a token, for row tints.
pub fn tint(color: Hsla, alpha: f32) -> Hsla {
    Hsla { a: alpha, ..color }
}

/// The 16 terminal colors, tuned to sit on the window background.
pub fn ansi(index: u8) -> Hsla {
    const TABLE: [(f32, f32, f32); 16] = [
        (0.3, 0.0, NEUTRAL),
        (0.68, 0.17, 25.0),
        (0.74, 0.15, 150.0),
        (0.8, 0.13, 85.0),
        (0.7, 0.12, 250.0),
        (0.7, 0.13, 320.0),
        (0.76, 0.1, 200.0),
        (0.85, 0.0, NEUTRAL),
        (0.5, 0.0, NEUTRAL),
        (0.76, 0.17, 25.0),
        (0.84, 0.16, 150.0),
        (0.88, 0.13, 85.0),
        (0.8, 0.12, 250.0),
        (0.8, 0.13, 320.0),
        (0.86, 0.1, 200.0),
        (0.97, 0.0, NEUTRAL),
    ];
    let (l, c, h) = TABLE[usize::from(index & 15)];
    oklch(l, c, h)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn luminance(token: (f32, f32, f32)) -> f32 {
        let [r, g, b] = oklch_linear(token.0, token.1, token.2);
        0.2126 * r.clamp(0.0, 1.0) + 0.7152 * g.clamp(0.0, 1.0) + 0.0722 * b.clamp(0.0, 1.0)
    }

    fn contrast(a: (f32, f32, f32), b: (f32, f32, f32)) -> f32 {
        let (x, y) = (luminance(a), luminance(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    fn token(name: &str) -> (f32, f32, f32) {
        ALL.iter().find(|t| t.0 == name).expect("token").1
    }

    #[test]
    fn every_text_color_clears_the_contrast_floor_on_every_surface() {
        for fg in ["text", "dim", "accent", "bright", "green", "red"] {
            for bg in ["bg", "surface", "raised", "overlay"] {
                let ratio = contrast(token(fg), token(bg));
                assert!(ratio >= 4.5, "{fg} on {bg} is {ratio}");
            }
        }
        assert!(contrast(token("bg"), token("accent")) >= 4.5, "text on the filled control");
    }

    #[test]
    fn the_interface_is_black_and_white_apart_from_status_and_provider_colors() {
        for (name, (_, chroma, _)) in ALL {
            assert_eq!(*chroma == 0.0, !matches!(*name, "green" | "red" | "claude"), "{name}");
        }
    }

    #[test]
    fn oklch_white_and_black_are_exact() {
        assert!(oklch_linear(1.0, 0.0, 0.0).iter().all(|v| (v - 1.0).abs() < 0.001));
        assert!(oklch_linear(0.0, 0.0, 0.0).iter().all(|v| v.abs() < 0.001));
    }
}
