//! breadgreet's app stylesheet.
//!
//! Layered on top of `bread_theme`'s shared `@define-color` palette
//! (`@surface`, `@overlay`, `@accent`, `@red`, …) via `apply_shared()` +
//! `apply_app_css()`. The visual target is `design/sketch.html`'s
//! `.greetoverlay` section — the same design language as the lock screen:
//! a floating surface card over a cover-fit / Ken-Burns wallpaper, an
//! accent focus ring on the entry, a spinner while a request is in flight,
//! and a shake on a failed attempt.

use bread_theme::gtk as bgtk;
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
        // ---- window + wallpaper ----------------------------------------
        "window.breadgreet {{ background-color: @bg; color: @on-bg; {font} }}\
         .login-veil {{\
             background-image: linear-gradient(to bottom,\
                 alpha(black, 0.42) 0%, alpha(black, 0.20) 45%, alpha(black, 0.34) 100%);\
         }}\
         \
         /* ---- clock cluster (over the wallpaper) --------------------- */\
         .login-clock {{\
             font-size: 46px; font-weight: 700; color: white;\
             text-shadow: 0 2px 12px alpha(black, 0.55);\
         }}\
         .login-date {{\
             font-size: 14px; font-weight: 500; color: alpha(white, 0.82);\
             text-shadow: 0 1px 8px alpha(black, 0.5);\
         }}\
         \
         /* ---- the card --------------------------------------------- */\
         .login-card {{\
             background: @surface; color: @on-surface;\
             border: 1px solid alpha(@overlay, 0.09);\
             border-radius: 10px; padding: 22px;\
             min-width: 300px;\
             box-shadow: 0 6px 28px alpha(black, 0.5);\
         }}\
         .login-card.shake {{ animation: breadgreet-shake 380ms cubic-bezier(0.36, 0.07, 0.19, 0.97); }}\
         \
         /* ---- entry ----------------------------------------------- */\
         .login-entry {{\
             background: shade(@surface, 1.5); color: @on-surface;\
             border: 1px solid alpha(@overlay, 0.14);\
             border-radius: 7px; padding: 11px 14px; font-size: 14px;\
             caret-color: @accent;\
             transition: border-color 180ms ease, box-shadow 180ms ease;\
         }}\
         .login-entry:disabled {{ opacity: 0.55; }}\
         .login-entry > text {{ background: transparent; }}\
         .login-entry:focus-within {{\
             border-color: @accent;\
             box-shadow: 0 0 0 2px alpha(@accent, 0.28);\
             outline: none;\
         }}\
         \
         /* ---- status line --------------------------------------- */\
         .login-status {{\
             font-size: 12px; color: alpha(@on-surface, 0.72);\
             margin-top: 10px; min-height: 1em;\
             transition: color 160ms ease;\
         }}\
         .login-status.error {{ color: @red; opacity: 1; font-weight: 700; }}\
         \
         /* ---- spinner (shown while a request is in flight) ------ */\
         .login-spinner {{\
             min-width: 16px; min-height: 16px;\
             margin-top: 12px;\
             border: 2px solid alpha(@overlay, 0.15);\
             border-top: 2px solid @accent;\
             border-radius: 999px;\
             opacity: 0;\
         }}\
         .login-spinner.spinning {{ opacity: 1; animation: breadgreet-spin 800ms linear infinite; }}\
         \
         /* ---- session picker (styled as a surface pill) -------- */\
         .login-session {{\
             margin-top: 14px; font-size: 12px;\
         }}\
         .login-session > button {{\
             background: shade(@surface, 1.5); color: alpha(@on-surface, 0.9);\
             border: 1px solid alpha(@overlay, 0.14);\
             border-radius: 7px; padding: 9px 12px; min-height: 20px;\
         }}\
         .login-session > button:hover {{ border-color: alpha(@accent, 0.5); }}\
         .login-session > button:focus-within {{\
             border-color: @accent; box-shadow: 0 0 0 2px alpha(@accent, 0.28); outline: none;\
         }}\
         .login-session arrow {{ color: alpha(@on-surface, 0.6); min-height: 16px; min-width: 16px; }}\
         .login-session popover > contents {{\
             background: @surface; border: 1px solid alpha(@overlay, 0.12);\
             border-radius: 8px; padding: 4px;\
         }}\
         .login-session popover row {{ border-radius: 6px; padding: 6px 10px; }}\
         .login-session popover row:selected {{ background: alpha(@accent, 0.22); color: @on-surface; }}\
         \
         /* ---- keyframes --------------------------------------- */\
         @keyframes breadgreet-spin {{ to {{ transform: rotate(360deg); }} }}\
         @keyframes breadgreet-shake {{\
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
