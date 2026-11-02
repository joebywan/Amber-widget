// Tray icon showing the live buy price as digits, because Windows trays can't show text beside an icon.
use std::sync::mpsc::Receiver;

use eframe::egui;
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

pub struct Status {
    pub buy: f64,
    pub sell: Option<f64>,
    pub spike: bool,
}

const SIZE: usize = 32;
const FONT: [(char, [u8; 5]); 11] = [
    ('0', [0b111, 0b101, 0b101, 0b101, 0b111]),
    ('1', [0b010, 0b110, 0b010, 0b010, 0b111]),
    ('2', [0b111, 0b001, 0b111, 0b100, 0b111]),
    ('3', [0b111, 0b001, 0b111, 0b001, 0b111]),
    ('4', [0b101, 0b101, 0b111, 0b001, 0b001]),
    ('5', [0b111, 0b100, 0b111, 0b001, 0b111]),
    ('6', [0b111, 0b100, 0b111, 0b101, 0b111]),
    ('7', [0b111, 0b001, 0b010, 0b010, 0b010]),
    ('8', [0b111, 0b101, 0b111, 0b101, 0b111]),
    ('9', [0b111, 0b101, 0b111, 0b001, 0b111]),
    ('-', [0b000, 0b000, 0b111, 0b000, 0b000]),
];

/// Whole cents, limited to what fits in the icon: at most three characters.
fn icon_text(cents: f64) -> String {
    (cents.round() as i64).clamp(-99, 999).to_string()
}

/// RGBA pixels: light digits on dark, or on the sell colour during a spike so it stands out in the panel.
fn icon_rgba(text: &str, spike: bool) -> Vec<u8> {
    let background = if spike { [0xc9, 0x63, 0x1a, 0xff] } else { [0x1a, 0x1a, 0x19, 0xff] };
    let mut pixels: Vec<u8> = background.iter().copied().cycle().take(SIZE * SIZE * 4).collect();
    let scale = if text.len() <= 2 { 4 } else { 3 };
    let width = text.len() * 3 * scale + (text.len() - 1) * scale / 2;
    let (left, top) = ((SIZE - width) / 2, (SIZE - 5 * scale) / 2);
    for (index, c) in text.chars().enumerate() {
        let Some((_, rows)) = FONT.iter().find(|(f, _)| *f == c) else { continue };
        let x0 = left + index * (3 * scale + scale / 2);
        for (row, bits) in rows.iter().enumerate() {
            for column in 0..3 {
                if bits & (0b100 >> column) == 0 {
                    continue;
                }
                for y in top + row * scale..top + (row + 1) * scale {
                    for x in x0 + column * scale..x0 + (column + 1) * scale {
                        pixels[(y * SIZE + x) * 4..][..4].copy_from_slice(&[0xff, 0xff, 0xff, 0xff]);
                    }
                }
            }
        }
    }
    pixels
}

fn make_icon(text: &str, spike: bool) -> Option<Icon> {
    Icon::from_rgba(icon_rgba(text, spike), SIZE as u32, SIZE as u32).ok()
}

fn build(ctx: egui::Context) -> Option<TrayIcon> {
    let open = MenuItem::new("Open Amber", true, None);
    let quit = MenuItem::new("Quit", true, None);
    let menu = Menu::new();
    menu.append(&open).ok()?;
    menu.append(&quit).ok()?;
    let (open_id, quit_id) = (open.id().clone(), quit.id().clone());
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if event.id == quit_id {
            std::process::exit(0);
        }
        if event.id == open_id {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            ctx.request_repaint();
        }
    }));
    TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_icon(make_icon("--", false)?)
        .with_tooltip("Amber")
        .build()
        .ok()
}

fn apply(tray: &TrayIcon, status: &Status) {
    let text = icon_text(status.buy);
    let sell = status.sell.map_or(String::new(), |s| format!(", sell {s:.1}c"));
    let _ = tray.set_icon(make_icon(&text, status.spike));
    let _ = tray.set_tooltip(Some(format!("Amber: buy {:.1}c{sell}", status.buy)));
    tray.set_title(Some(format!("{text}c")));
}

/// Updates the tray from the worker. Linux needs its own GTK thread for this; elsewhere the tray
/// belongs to the window's thread, so `Pump` gets drained from the UI loop.
pub struct Pump(#[cfg(not(target_os = "linux"))] Option<(TrayIcon, Receiver<Status>)>);

impl Pump {
    pub fn drain(&self) {
        #[cfg(not(target_os = "linux"))]
        if let Some((tray, updates)) = &self.0 {
            while let Ok(status) = updates.try_recv() {
                apply(tray, &status);
            }
        }
    }
}

#[cfg(target_os = "linux")]
pub fn start(ctx: egui::Context, updates: Receiver<Status>) -> Pump {
    // A failure here only costs the tray icon; the window carries on without it.
    std::thread::spawn(move || {
        if gtk::init().is_err() {
            return;
        }
        let Some(tray) = build(ctx) else { return };
        gtk::glib::timeout_add_local(std::time::Duration::from_secs(1), move || {
            while let Ok(status) = updates.try_recv() {
                apply(&tray, &status);
            }
            gtk::glib::ControlFlow::Continue
        });
        gtk::main();
    });
    Pump()
}

#[cfg(not(target_os = "linux"))]
pub fn start(ctx: egui::Context, updates: Receiver<Status>) -> Pump {
    Pump(build(ctx).map(|tray| (tray, updates)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_text_fits_three_characters() {
        assert_eq!(icon_text(39.3), "39");
        assert_eq!(icon_text(1500.0), "999");
        assert_eq!(icon_text(-250.0), "-99");
    }

    #[test]
    fn every_price_renders_digits_inside_the_icon() {
        for cents in -99..=999 {
            let pixels = icon_rgba(&cents.to_string(), false);
            assert_eq!(pixels.len(), SIZE * SIZE * 4);
            assert!(pixels.chunks(4).any(|p| p == [0xff, 0xff, 0xff, 0xff]), "{cents} drew nothing");
        }
    }
}
