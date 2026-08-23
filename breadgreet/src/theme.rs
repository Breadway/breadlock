use bread_theme::{gtk as bgtk, ink_on, load_palette};
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
    let p = load_palette();
    let font = css_font_family(font_family);
    format!(
        "window.breadgreet {{ background-color: {bg}; color: {on_bg}; {font} }}\
         .login-card {{ background: {surface}; color: {on_surface}; border-radius: 8px;\
             padding: 20px; min-width: 320px; }}\
         .login-clock {{ font-size: 48px; font-weight: bold; }}\
         .login-date {{ font-size: 18px; font-weight: 500; opacity: 0.8; }}\
         .login-entry {{ font-size: 14px; }}\
         .login-status {{ font-size: 12px; opacity: 0.75; margin-top: 8px; }}\
         .login-status.error {{ color: {red}; opacity: 1; }}\
         .login-session {{ font-size: 12px; opacity: 0.85; margin-top: 12px; }}\
         dropdown.login-session {{ min-height: 32px; }}\
         .login-veil {{ background-image: linear-gradient(to bottom, rgba(0,0,0,0.34) 0%, rgba(0,0,0,0.16) 100%); }}",
        bg = p.background,
        surface = p.color0,
        red = p.color1,
        on_bg = ink_on(&p.background),
        on_surface = ink_on(&p.color0),
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
