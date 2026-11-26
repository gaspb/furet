//! herdr's look, so the app and the herdr panel feel like one tool: its "paper" and "graphite"
//! palettes, the panel colours it derives from them, Geist and JetBrains Mono at its sizes.

use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::*;

struct Palette {
    background: u32,
    selection: u32,
    ansi: [u32; 8],
}

const PAPER: Palette = Palette {
    background: 0xffffff,
    selection: 0xcfdcf5,
    ansi: [
        0x25272c, 0xc62a2a, 0x1a7f37, 0x8f5d00, 0x1f62d0, 0x7a45c8, 0x0f6f74, 0x5f6068,
    ],
};

const GRAPHITE: Palette = Palette {
    background: 0x17191d,
    selection: 0x394454,
    ansi: [
        0x45484f, 0xdc8c95, 0xa6bd91, 0xd8be87, 0x96b2d7, 0xb5a2ce, 0x8cbdc3, 0xd2d4d9,
    ],
};

fn mix(a: u32, b: u32, t: f32) -> u32 {
    let ch = |x: u32, s: u32| ((x >> s) & 0xff) as f32;
    let one = |s: u32| ((ch(a, s) + (ch(b, s) - ch(a, s)) * t).round() as u32) << s;
    one(16) | one(8) | one(0)
}

fn c(hex: u32) -> Hsla {
    rgb(hex).into()
}

fn alpha(hex: u32, a: u8) -> Hsla {
    rgba((hex << 8) | a as u32).into()
}

/// The theme for the window's appearance, as herdr resolves it for its own panels.
pub fn apply(mode: ThemeMode, cx: &mut App) {
    Theme::change(mode, None, cx);
    let light = !mode.is_dark();
    let p = if light { &PAPER } else { &GRAPHITE };
    let bg = p.background;
    let fg = if light { 0x1b1b1e } else { 0xeeeeef };
    let ink = if light { 0x000000 } else { 0xffffff };
    let (hover, selected, border, input) = if light {
        (0x0c, 0x12, 0x17, 0x26)
    } else {
        (0x0e, 0x17, 0x18, 0x24)
    };
    let chrome = if light {
        mix(bg, 0xe8e8e2, 0.6)
    } else {
        mix(bg, 0, 0.32)
    };
    let raised = if light {
        mix(bg, 0, 0.035)
    } else {
        mix(bg, 0xffffff, 0.09)
    };
    let dim = mix(bg, fg, if light { 0.46 } else { 0.38 });
    let accent = p.ansi[4];
    Theme::update(cx, |t| {
        t.font_family = "Geist".into();
        t.font_size = px(13.);
        t.mono_font_family = "JetBrains Mono".into();
        t.mono_font_size = px(12.5);
        let k = &mut t.colors;
        k.background = c(bg);
        k.foreground = c(fg);
        k.caret = c(fg);
        k.selection = c(p.selection);
        for b in [
            &mut k.border,
            &mut k.sidebar_border,
            &mut k.title_bar_border,
            &mut k.status_bar_border,
            &mut k.table_row_border,
            &mut k.window_border,
        ] {
            *b = alpha(ink, border);
        }
        k.input = alpha(ink, input);
        k.ring = c(accent);
        k.sidebar = c(chrome);
        k.sidebar_foreground = c(fg);
        k.status_bar = c(chrome);
        k.title_bar = c(chrome);
        k.muted = c(raised);
        k.muted_foreground = c(dim);
        k.accent = alpha(ink, hover);
        k.accent_foreground = c(fg);
        k.list = c(bg);
        k.list_even = c(bg);
        k.list_head = c(bg);
        k.list_hover = alpha(ink, hover);
        k.list_active = alpha(ink, selected);
        k.list_active_border = alpha(ink, 0);
        k.table = c(bg);
        k.table_even = c(bg);
        k.table_head = c(bg);
        k.table_head_foreground = c(dim);
        k.table_hover = alpha(ink, hover);
        k.table_active = alpha(accent, 0x1c);
        k.table_active_border = c(accent);
        k.popover = c(if light { bg } else { mix(bg, 0xffffff, 0.06) });
        k.popover_foreground = c(fg);
        k.primary = c(fg);
        k.primary_hover = c(mix(fg, bg, 0.15));
        k.primary_active = c(mix(fg, bg, 0.25));
        k.primary_foreground = c(if light { 0xffffff } else { mix(bg, 0, 0.28) });
        k.secondary = c(raised);
        k.secondary_hover = alpha(ink, selected);
        k.secondary_foreground = c(fg);
        k.link = c(accent);
        k.blue = c(accent);
        k.info = c(accent);
        k.success = c(p.ansi[2]);
        k.green = c(p.ansi[2]);
        k.danger = c(p.ansi[1]);
        k.red = c(p.ansi[1]);
        k.warning = c(p.ansi[3]);
        k.yellow = c(p.ansi[3]);
        k.magenta = c(p.ansi[5]);
        k.cyan = c(p.ansi[6]);
    });
}

pub fn follow_system(window: &mut Window, cx: &mut App) {
    apply(window.appearance().into(), cx);
}

#[cfg(test)]
mod tests {
    #[test]
    fn mixes_like_herdr() {
        assert_eq!(super::mix(0xffffff, 0xe8e8e2, 0.6), 0xf1f1ee);
        assert_eq!(super::mix(0x000000, 0xffffff, 0.5), 0x808080);
    }
}
