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
    caps_lock: bool,
    layout_index: u32,
    reveal: bool,
    idle_dim: f32,
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
            caps_lock: false,
            layout_index: 0,
            reveal: false,
            idle_dim: 0.0,
        }
    }
}

/// `--time [WxH] [frames] [wallpaper.png]` — renders the real compose() path
/// (image background + Ken Burns, full chrome) in a loop and prints per-frame
/// timings, so the software renderer's cost can be measured without Wayland.
fn bench(args: &[String]) {
    let parse = |s: &str, d: &str| -> String { args.iter().find(|a| a.starts_with(s)).map(|a| a[s.len()..].to_string()).unwrap_or_else(|| d.to_string()) };
    let size: (u32, u32) = {
        let v: Vec<u32> = parse("--size=", "1920x1200").split('x').filter_map(|s| s.parse().ok()).collect();
        (v[0], v[1])
    };
    let frames: u32 = parse("--frames=", "120").parse().unwrap_or(120);
    let path = parse("--wallpaper=", "/home/breadway/.config/breadlock/wallpaper.png");

    let palette = theme::load_palette();
    let bg_cfg = breadlock_ui::config::Background {
        mode: breadlock_ui::config::BackgroundMode::Image,
        path,
        blur: false,
        ken_burns: true,
    };
    let background = background::Background::load(&bg_cfg, &palette);

    let mut text = TextRenderer::new();
    // Warm up once: the first frame builds the scaled-wallpaper cache and
    // shapes the glyphs. Steady-state frames are what the timer loop sees.
    let warm = FrameInputs {
        width: size.0,
        height: size.1,
        background: &background,
        palette: &palette,
        font_family: FONT,
        clock_text: "12:34",
        date_text: "Friday · Aug 21",
        clock_old: None,            password_len: 6,
            password: "hunter2",
            reveal: false,
            caps_lock: false,
            layout_index: 0,
            idle_dim: 0.0,
            failed: false,
            failed_t: 0.0,
            dot_pop_t: 1.0,
            keystroke_age: None,
            t_secs: 0.0,
            breathe_t: 0.0,
            status_t: 1.0,
            status_text: None,
            appear_t: 1.0,
            unlock_t: 0.0,
            smooth_pan: true,
        };
    compose(&mut text, &warm).expect("warm-up compose failed");

    // Isolate the background pass cost (wallpaper blit + fills) alone.
    let mut bg_times = Vec::new();
    {
        let mut dummy = tiny_skia::Pixmap::new(size.0, size.1).expect("pixmap");
        for i in 0..60 {
            let t = std::time::Instant::now();
            background.paint(&mut dummy, (i as f32 / 60.0) * 90.0, true);
            bg_times.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        bg_times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let avg: f64 = bg_times.iter().sum::<f64>() / bg_times.len() as f64;
        println!("background.paint only: avg {avg:.2} ms  max {:.2} ms", bg_times[bg_times.len() - 1]);
    }

    let mut times = Vec::with_capacity(frames as usize);
    let start = std::time::Instant::now();
    for i in 0..frames {
        let t = std::time::Instant::now();
        let inputs = FrameInputs {
            width: size.0,
            height: size.1,
            background: &background,
            palette: &palette,
            font_family: FONT,
            clock_text: "12:34",
            date_text: "Friday · Aug 21",
            clock_old: None,
            password_len: 6,
            password: "hunter2",
            reveal: false,
            caps_lock: false,
            layout_index: 0,
            idle_dim: 0.0,
            failed: false,
            failed_t: 0.0,
            dot_pop_t: 1.0,
            keystroke_age: None,
            // Walk t_secs through a Ken Burns cycle so every frame differs.
            t_secs: (i as f32 / frames as f32) * 90.0,
            breathe_t: (i % 10) as f32 / 10.0,
            status_t: 1.0,
            status_text: None,
            appear_t: 1.0,
            unlock_t: 0.0,
            smooth_pan: true,
        };
        if compose(&mut text, &inputs).is_none() {
            eprintln!("compose returned None at frame {i}");
            std::process::exit(1);
        }
        times.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    let total = start.elapsed().as_secs_f64() * 1000.0;
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let avg: f64 = times.iter().sum::<f64>() / times.len() as f64;
    let p95 = times[(times.len() as f64 * 0.95) as usize];
    println!(
        "{frames} frames @ {}x{}: avg {avg:.2} ms  p95 {p95:.2} ms  max {:.2} ms  total {total:.0} ms (first frame excluded from avg? no)",
        size.0, size.1, times[times.len() - 1]
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--time") {
        bench(&args);
        return;
    }
    let out_dir = args
        .first()
        .cloned()
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
        // ---- Caps Lock on: chip above the pill.
        Scene { name: "13-caps-lock", password_len: 4, caps_lock: true, ..Scene::default() },
        // ---- Non-default layout: layout chip instead of caps.
        Scene { name: "14-layout-2", password_len: 4, layout_index: 1, ..Scene::default() },
        // ---- Hold-to-reveal: plain password characters instead of dots.
        Scene { name: "15-reveal", password_len: 8, reveal: true, ..Scene::default() },
        // ---- Idle auto-dim: deepened veil (rest pose + full idle dim).
        Scene { name: "16-idle-dim", idle_dim: 1.0, ..Scene::default() },
        // ---- Repeat failure: attempt counter in the status line.
        Scene { name: "17-failed-3x", password_len: 6, failed: true, failed_t: 0.8, status: Some("Wrong password — 3 failed attempts"), ..Scene::default() },
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
            password: "hunter2",
            reveal: scene.reveal,
            caps_lock: scene.caps_lock,
            layout_index: scene.layout_index,
            idle_dim: scene.idle_dim,
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
            smooth_pan: false,
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
