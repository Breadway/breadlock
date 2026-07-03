//! Dev-only harness: exercises the exact PAM flow `breadlock` uses
//! (`auth::pam::check`) against a typed password, with no Wayland surface
//! involved at all. This is the safest way to validate the
//! `/etc/pam.d/breadlock` service file and the auth logic in isolation — a
//! bad PAM config here just prints an error, it can never lock a session.
//!
//! Not installed by the package; run from a build tree with
//! `cargo run --bin breadlock-auth-check`.

use std::io::Write;

#[path = "../auth/pam.rs"]
mod pam;

fn main() {
    let username = std::env::var("USER").unwrap_or_else(|_| {
        eprint!("Username: ");
        std::io::stdout().flush().ok();
        let mut buf = String::new();
        std::io::stdin().read_line(&mut buf).ok();
        buf.trim().to_string()
    });

    let password = rpassword_prompt();

    match pam::check(&username, &password) {
        Ok(()) => println!("OK: {username} authenticated"),
        Err(e) => {
            eprintln!("FAILED: {e}");
            std::process::exit(1);
        }
    }
}

/// Minimal no-echo password prompt so this harness doesn't need the `rpassword`
/// crate — good enough for a dev tool, never shipped.
fn rpassword_prompt() -> String {
    use std::io::BufRead;
    eprint!("Password: ");
    std::io::stderr().flush().ok();

    // Best-effort: disable echo via `stty` if a TTY is attached, restore after.
    let stty_available = std::process::Command::new("stty")
        .arg("-echo")
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).ok();

    if stty_available {
        let _ = std::process::Command::new("stty").arg("echo").status();
        eprintln!();
    }

    line.trim_end_matches(['\n', '\r']).to_string()
}
