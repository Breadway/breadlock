//! Dev-only harness: renders the breadlock lock-screen motion system to a
//! folder of PNGs so the new animations can be eyeballed without locking a
//! session (or even touching Wayland). Every scene below pins concrete
//! progress values into `render::FrameInputs` — the same struct the real
//! locker feeds from live timestamps — so what you see here is exactly what
//! `state.rs` computes at runtime.
//!
//! Not installed by the package; run from a build tree with
//! `cargo run --bin breadlock-preview [out-dir]` (default `preview/`).
//! Scenes are written as `NN-<name>.png` in alphabetical-file order, so a
//! file manager or `for f in preview/*.png; do ...` steps through them as a
//! flipbook roughly in timeline order.

use breadlock_ui::painter::TextRenderer;
use breadlock_ui::theme;
use render::{compose, FrameInputs};

// Reuse the real renderer + background code via the same `#[path]` include
// trick as `breadlock-auth-check` (dev bins are separate crates and can't see
// `main.rs`'s modules otherwise). `render.rs` pulls `crate::background::Background`,
// which this crate root provides below. Only `compose`/`FrameInputs` are used
// here; the compositor-side helpers (blit_to_shm, the timing consts) stay
// included so this harness exercises the *real* renderer, so dead-code is
// expected and silenced.
#[allow(dead_code)]
#[path = "../background.rs"]
mod background;

#[allow(dead_code)]
#[path = "../render.rs"]
mod render;

const W: u32 = 960;
const H: u32 = 540;
const FONT: &str = "Varela Round";

struct Scene {
    name: &'static str,
    clock: &'static str,
    date: &'static str,
    clock_old: Option<(&'static str, f32)>,
    password_len: usize,
    failed: bool,
    failed_t: f32,
    dot_pop_t: f32,
    keystroke_age: Option<f32>,
    /// Idle caret blink phase driver (`t_secs` in FrameInputs). Only matters
    /// for scenes with no keystroke age: phase = (t × 1.8) % 1.0, caret is
    /// lit below 0.5.
    t_secs: f32,
    status: Option<&'static str>,
    appear_t: f32,
    unlock_t: f32,
    breathe_t: f32,
    status_t: f32,
}

impl Default for Scene {
    fn default() -> Self {
        Self {
            name: "",
            clock: "12:34",
            date: "Friday · Aug 21",
            clock_old: None,
            password_len: 0,
            failed: false,
            failed_t: 0.0,
            dot_pop_t: 1.0,
            keystroke_age: None,
            t_secs: 0.2,
            status: None,
            appear_t: 1.0,
            unlock_t: 0.0,
            breathe_t: 0.0,
            status_t: 1.0,
        }
    }
}

fn main() {
    let out_dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "preview".to_string());
    std::fs::create_dir_all(&out_dir).expect("failed to create preview output dir");

    let palette = theme::load_palette();
    let background = background::Background::load(
        &breadlock_ui::config::Background::default(),
        &palette,
    );

    let scenes = [
        // ---- Staggered entrance: clock leads, pill pops in last (overshoot).
        Scene { name: "01-appear-start", appear_t: 0.0, ..Scene::default() },
        Scene { name: "02-appear-clock", password_len: 4, appear_t: 0.25, ..Scene::default() },
        Scene { name: "03-appear-pill", password_len: 4, appear_t: 0.55, ..Scene::default() },
        // ---- Rest pose: empty pill showing the "Enter password" hint.
        Scene { name: "04-rest-pose", t_secs: 0.5, ..Scene::default() },
        // ---- Idle breath: glow peak on the pill (accent ring + deeper shadow).
        Scene { name: "05-breathe-peak", breathe_t: 1.0, ..Scene::default() },
        // ---- Typing: newest dot mid-pop, caret solid.
        Scene { name: "06-typing-pop", password_len: 6, dot_pop_t: 0.4, keystroke_age: Some(0.2), ..Scene::default() },
        // ---- Idle blink: two dots, caret lit (phase 0.36 → visible half-cycle).
        Scene { name: "07-idle-blink", password_len: 2, ..Scene::default() },
        // ---- Checking: status mid slide-in with the animated ellipsis.
        Scene { name: "08-checking", status: Some("Checking…"), status_t: 0.5, ..Scene::default() },
        // ---- Wrong password: mid-shake, red pill, red status (settled).
        Scene { name: "09-failed-shake", password_len: 6, failed: true, failed_t: 0.35, status: Some("Wrong password"), ..Scene::default() },
        // ---- Success: green flash ring, dots cascading accent → white.
        Scene { name: "10-success-flash", password_len: 6, unlock_t: 0.12, ..Scene::default() },
        // ---- Unlock fade-out: chrome faded, parallax drift (clock furthest).
        Scene { name: "11-unlock-fade", password_len: 6, unlock_t: 0.8, ..Scene::default() },
        // ---- Minute rollover: old clock fading out above, new fading in below.
        Scene { name: "12-clock-crossfade", clock: "12:35", clock_old: Some(("12:34", 0.5)), password_len: 4, ..Scene::default() },
    ];

    let mut text = TextRenderer::new();
    let mut count = 0;
    for scene in &scenes {
        let inputs = FrameInputs {
            width: W,
            height: H,
            background: &background,
            palette: &palette,
            font_family: FONT,
            clock_text: scene.clock,
            date_text: scene.date,
            clock_old: scene.clock_old,
            password_len: scene.password_len,
            failed: scene.failed,
            failed_t: scene.failed_t,
            dot_pop_t: scene.dot_pop_t,
            keystroke_age: scene.keystroke_age,
            t_secs: scene.t_secs,
            breathe_t: scene.breathe_t,
            status_t: scene.status_t,
            status_text: scene.status,
            appear_t: scene.appear_t,
            unlock_t: scene.unlock_t,
        };
        let Some(pixmap) = compose(&mut text, &inputs) else {
            eprintln!("compose returned None for scene {}", scene.name);
            std::process::exit(1);
        };
        let path = format!("{}/{}.png", out_dir, scene.name);
        pixmap
            .save_png(&path)
            .unwrap_or_else(|err| panic!("failed to write {path}: {err}"));
        count += 1;
        println!("wrote {path}");
    }
    println!("{count} frames → {out_dir}/");
}
