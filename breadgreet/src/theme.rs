//! breadgreet's app stylesheet.
//!
//! Layered on `bread_theme`'s shared `@define-color` palette (`@surface`,
//! `@overlay`, `@accent`, `@teal`, `@red`, …) via `apply_shared()` +
//! `apply_app_css()`. Visual target: `design/sketch.html`'s `.greetoverlay`
//! and the lock screen's motion vocabulary — a hero clock over an accent
//! rule, a floating surface card with a staggered pop-in, an entry with a
//! live accent focus glow, and per-state motion (spinner, shake).
//!
//! Entrance / idle motion is CSS `@keyframes` (GTK4 runs `animation` when a
//! widget's style is first computed, i.e. on show). Wallpaper Ken Burns
//! stays in Rust (`setup_wallpaper`) since it's a continuous frame-clock
//! pan, not a one-shot.

use bread_theme::gtk as bgtk;
use gtk4::prelude::*;
use gtk4::CssProvider;
use std::cell::RefCell;

thread_local! {
    static USER_PROVIDER: RefCell<Option<CssProvider>> = const { RefCell::new(None) };
}

fn css_font_family(family: &str) -> String {
    if family.is_empty() {
        return String::new();
    }
    let escaped = family.replace('\\', "\\\\").replace('"', "\\\"");
    format!("font-family: \"{escaped}\";")
}

fn load_css(font_family: &str) -> String {
    let font = css_font_family(font_family);
    format!(
        // ---- window + wallpaper veil ---------------------------------
        "window.breadgreet {{ background-color: @bg; color: @on-bg; {font} }}\
         .login-veil {{\
             background-image:\
                 radial-gradient(ellipse 90% 90% at 50% 45%, alpha(black, 0.0) 35%, alpha(black, 0.44) 100%),\
                 linear-gradient(to bottom, alpha(black, 0.40) 0%, alpha(black, 0.16) 42%, alpha(black, 0.36) 100%);\
         }}\
         \
         /* ---- clock cluster ---------------------------------------- */\
         .login-clock {{\
             font-size: 68px; font-weight: 300; color: white; letter-spacing: 1px;\
             text-shadow: 0 3px 22px alpha(black, 0.6);\
             animation: bg-rise 520ms cubic-bezier(0.16, 1, 0.3, 1) both;\
         }}\
         .login-date {{\
             font-size: 14px; font-weight: 600; color: alpha(white, 0.8);\
             letter-spacing: 1.5px;\
             text-shadow: 0 1px 10px alpha(black, 0.55);\
             animation: bg-rise 520ms cubic-bezier(0.16, 1, 0.3, 1) 80ms both;\
         }}\
         /* accent rule between the clock and the card — draws in */\
         .login-rule {{\
             min-height: 3px; min-width: 88px; margin: 15px 0 22px;\
             border-radius: 3px;\
             background-image: linear-gradient(90deg, alpha(@accent, 0.0), @accent 42%, @teal 100%);\
             box-shadow: 0 0 14px alpha(@accent, 0.5), 0 0 3px alpha(@teal, 0.4);\
             animation: bg-draw 620ms cubic-bezier(0.16, 1, 0.3, 1) 140ms both;\
         }}\
         \
         /* ---- the card ------------------------------------------- */\
         .login-card {{\
             background-image: linear-gradient(to bottom, shade(@surface, 1.06), @surface);\
             color: @on-surface;\
             border: 1px solid alpha(@overlay, 0.10);\
             border-top: 1px solid alpha(white, 0.06);\
             border-radius: 14px; padding: 24px 22px; min-width: 320px;\
             box-shadow: 0 20px 48px alpha(black, 0.55), 0 2px 8px alpha(black, 0.4);\
             animation: bg-pop 480ms cubic-bezier(0.34, 1.56, 0.64, 1) 170ms both,\
                        bg-breathe 5s ease-in-out 1400ms infinite;\
         }}\
         .login-card.shake {{ animation: bg-shake 400ms cubic-bezier(0.36, 0.07, 0.19, 0.97); }}\
         \
         /* ---- entry --------------------------------------------- */\
         .login-entry {{\
             background-image: linear-gradient(to bottom, shade(@surface, 1.42), shade(@surface, 1.58));\
             color: @on-surface;\
             border: 1px solid alpha(@overlay, 0.13);\
             border-radius: 9px; padding: 12px 14px; font-size: 15px;\
             caret-color: @accent;\
             box-shadow: inset 0 1px 2px alpha(black, 0.28);\
             transition: border-color 180ms ease, box-shadow 200ms ease, background-image 180ms ease;\
         }}\
         .login-entry:disabled {{ opacity: 0.5; }}\
         .login-entry > text {{ background: transparent; }}\
         .login-entry image {{ color: alpha(@on-surface, 0.55); margin-right: 6px; }}\
         .login-entry:focus-within {{\
             border-color: @accent;\
             background-image: linear-gradient(to bottom, shade(@surface, 1.5), shade(@surface, 1.66));\
             box-shadow: inset 0 1px 2px alpha(black, 0.2),\
                         0 0 0 3px alpha(@accent, 0.22),\
                         0 6px 22px alpha(@accent, 0.14);\
             outline: none;\
         }}\
         .login-entry:focus-within image {{ color: @accent; }}\
         \
         /* ---- status line -------------------------------------- */\
         .login-status {{\
             font-size: 12px; color: alpha(@on-surface, 0.68);\
             margin-top: 12px; min-height: 1em;\
             transition: color 160ms ease;\
         }}\
         /* errors must read as errors regardless of the wallpaper palette\
            (BOS's default `@red` slot is a warm ochre, not a warning red) */\
         .login-status.error {{ color: #ff6b6b; opacity: 1; font-weight: 700; }}\
         \
         /* ---- spinner ----------------------------------------- */\
         .login-spinner {{\
             min-width: 16px; min-height: 16px; margin-top: 14px;\
             border: 2px solid alpha(@overlay, 0.16);\
             border-top: 2px solid @accent;\
             border-radius: 999px; opacity: 0;\
             transition: opacity 160ms ease;\
         }}\
         .login-spinner.spinning {{ opacity: 1; animation: bg-spin 720ms linear infinite; }}\
         \
         /* ---- session picker (design sketch .srow) ------------ */\
         .login-session {{ margin-top: 16px; }}\
         .session-row {{\
             background-image: linear-gradient(to bottom, shade(@surface, 1.4), shade(@surface, 1.52));\
             border: 1px solid alpha(@overlay, 0.12);\
             border-radius: 9px; padding: 7px 10px;\
             transition: border-color 160ms ease;\
         }}\
         .session-row:focus-within {{ border-color: alpha(@accent, 0.6); }}\
         .session-icon {{\
             min-width: 22px; min-height: 22px; border-radius: 6px;\
             background-image: linear-gradient(135deg, @accent, @teal);\
             box-shadow: 0 1px 4px alpha(@accent, 0.35);\
         }}\
         .login-session dropdown {{ background: transparent; border: none; box-shadow: none; padding: 0; }}\
         .login-session dropdown > button {{\
             background: transparent; border: none; box-shadow: none; outline: none;\
             padding: 2px 4px; min-height: 22px; color: alpha(@on-surface, 0.92);\
             font-size: 13px;\
         }}\
         .login-session dropdown > button:hover {{ background: transparent; }}\
         .login-session dropdown arrow {{ color: alpha(@on-surface, 0.55); min-height: 14px; min-width: 14px; }}\
         .login-session popover > contents {{\
             background: @surface; border: 1px solid alpha(@overlay, 0.14);\
             border-radius: 10px; padding: 5px;\
             box-shadow: 0 12px 32px alpha(black, 0.5);\
         }}\
         .login-session popover row {{ border-radius: 7px; padding: 7px 11px; font-size: 13px; }}\
         .login-session popover row:selected {{ background: alpha(@accent, 0.22); color: @on-surface; }}\
         \
         /* ---- who's logging in (auto-user mode) -------------- */\
         .login-user {{ margin-bottom: 12px; }}\
         .user-row {{\
             background-image: linear-gradient(to bottom, shade(@surface, 1.4), shade(@surface, 1.52));\
             border: 1px solid alpha(@overlay, 0.12);\
             border-radius: 9px; padding: 7px 10px;\
             transition: border-color 160ms ease;\
         }}\
         .user-row:focus-within {{ border-color: alpha(@accent, 0.6); }}\
         .user-icon {{\
             min-width: 24px; min-height: 24px; border-radius: 999px;\
             background-image: linear-gradient(135deg, @accent, @teal);\
             box-shadow: 0 1px 5px alpha(@accent, 0.4), inset 0 1px 1px alpha(white, 0.2);\
         }}\
         .user-name {{ font-size: 14px; font-weight: 600; color: alpha(@on-surface, 0.95); }}\
         .login-user dropdown {{ background: transparent; border: none; box-shadow: none; padding: 0; }}\
         .login-user dropdown > button {{\
             background: transparent; border: none; box-shadow: none; outline: none;\
             padding: 2px 4px; min-height: 24px; color: alpha(@on-surface, 0.95);\
             font-size: 14px; font-weight: 600;\
         }}\
         .login-user dropdown > button:hover {{ background: transparent; }}\
         .login-user dropdown arrow {{ color: alpha(@on-surface, 0.55); min-height: 14px; min-width: 14px; }}\
         .login-user popover > contents {{\
             background: @surface; border: 1px solid alpha(@overlay, 0.14);\
             border-radius: 10px; padding: 5px;\
             box-shadow: 0 12px 32px alpha(black, 0.5);\
         }}\
         .login-user popover row {{ border-radius: 7px; padding: 7px 11px; font-size: 13px; }}\
         .login-user popover row:selected {{ background: alpha(@accent, 0.22); color: @on-surface; }}\
         \
         /* ---- keyframes -------------------------------------- */\
         @keyframes bg-rise {{ from {{ opacity: 0; transform: translateY(20px); }} to {{ opacity: 1; transform: none; }} }}\
         @keyframes bg-pop {{\
             from {{ opacity: 0; transform: scale(0.94) translateY(14px); }}\
             70% {{ transform: scale(1.015) translateY(0); }}\
             to {{ opacity: 1; transform: scale(1) translateY(0); }}\
         }}\
         @keyframes bg-draw {{ from {{ opacity: 0; transform: scaleX(0); }} to {{ opacity: 1; transform: scaleX(1); }} }}\
         @keyframes bg-spin {{ to {{ transform: rotate(360deg); }} }}\
         @keyframes bg-breathe {{\
             0%, 100% {{ box-shadow: 0 20px 48px alpha(black, 0.55), 0 2px 8px alpha(black, 0.4); }}\
             50% {{ box-shadow: 0 24px 60px alpha(black, 0.62), 0 0 0 1px alpha(@accent, 0.10), 0 2px 8px alpha(black, 0.4); }}\
         }}\
         @keyframes bg-shake {{\
             10%, 90% {{ transform: translateX(-2px); }}\
             20%, 80% {{ transform: translateX(4px); }}\
             30%, 50%, 70% {{ transform: translateX(-7px); }}\
             40%, 60% {{ transform: translateX(7px); }}\
         }}",
        font = font,
    )
}

pub fn apply(font_family: &str) {
    bgtk::apply_shared();
    let family = font_family.to_string();
    bgtk::apply_app_css(move || load_css(&family));

    let user_path = crate::config::xdg_config_dir()
        .join("breadgreet")
        .join("style.css");
    USER_PROVIDER.with(|cell| bgtk::apply_user_css(&user_path, cell));
}

/// Bind the greeter window to its output's wallpaper palette *and* re-apply
/// breadgreet's own sheet as a widget-tree provider.
///
/// [`apply`]'s `apply_app_css` loads at APPLICATION priority, but
/// `bind_window_auto` re-broadcasts the shared component sheet — including its
/// `* { font-size }` base rule — at `USER - 10`, which outranks APPLICATION
/// regardless of selector specificity. So the hero clock, the date, every
/// typographic override here would silently collapse back to the base size.
/// Riding our sheet at `USER - 9` (what `_with_app_css` does) puts it back on
/// top. `@accent`/`@surface`/… tokens are inlined against the same palette.
pub fn bind(window: &impl IsA<gtk4::Native>, font_family: &str) {
    let family = font_family.to_string();
    bgtk::bind_window_auto_with_app_css(window, move |_palette| load_css(&family));
}
