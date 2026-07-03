use bread_theme::{gtk as bgtk, ink_on, load_palette};
use gtk4::CssProvider;
use std::cell::RefCell;

thread_local! {
    static USER_PROVIDER: RefCell<Option<CssProvider>> = const { RefCell::new(None) };
}

fn load_css() -> String {
    let p = load_palette();
    format!(
        "window.breadgreet {{ background-color: {bg}; color: {on_bg}; }}\
         .login-card {{ background: {surface}; color: {on_surface}; border-radius: 8px;\
             padding: 20px; min-width: 320px; }}\
         .login-clock {{ font-size: 48px; font-weight: bold; margin-bottom: 20px; }}\
         .login-entry {{ font-size: 14px; }}\
         .login-status {{ font-size: 12px; opacity: 0.75; margin-top: 8px; }}\
         .login-status.error {{ color: {red}; opacity: 1; }}\
         .login-session {{ font-size: 12px; opacity: 0.6; margin-top: 12px; }}",
        bg = p.background,
        surface = p.color0,
        red = p.color1,
        on_bg = ink_on(&p.background),
        on_surface = ink_on(&p.color0),
    )
}

pub fn apply() {
    bgtk::apply_shared();
    bgtk::apply_app_css(load_css);

    let home = std::env::var("HOME").unwrap_or_default();
    let user_path = std::path::PathBuf::from(format!("{home}/.config/breadgreet/style.css"));
    USER_PROVIDER.with(|cell| bgtk::apply_user_css(&user_path, cell));
}
