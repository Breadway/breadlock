//! Dev-only harness: exercises the exact PAM flow `breadlock` uses
//! (`auth::pam::check`) against a typed password, with no Wayland surface
//! involved at all. This is the safest way to validate the
//! `/etc/pam.d/breadlock` service file and the auth logic in isolation — a
//! bad PAM config here just prints an error, it can never lock a session.
//!
//! Not installed by the package; run from a build tree with
//! `cargo run --bin breadlock-auth-check`.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use zeroize::{Zeroize, Zeroizing};

#[path = "../auth/pam.rs"]
mod pam;

fn main() {
    let username = pam::username_from_process().unwrap_or_else(|| {
        eprint!("Username: ");
        std::io::stdout().flush().ok();
        let mut buf = String::new();
        std::io::stdin().read_line(&mut buf).ok();
        let name = buf.trim().to_string();
        buf.zeroize();
        name
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

static mut SAVED_TERMIOS: libc::termios = unsafe { std::mem::zeroed() };
static ECHO_SAVED: AtomicBool = AtomicBool::new(false);

extern "C" fn restore_echo_on_signal(sig: libc::c_int) {
    unsafe {
        if ECHO_SAVED.load(Ordering::Relaxed) {
            libc::tcsetattr(
                libc::STDIN_FILENO,
                libc::TCSANOW,
                std::ptr::addr_of!(SAVED_TERMIOS),
            );
        }
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

/// Disable TTY echo; restore on drop (panic, return) and on SIGINT/SIGTERM
/// so Ctrl-C cannot leave the terminal silent.
struct EchoOff {
    fd: libc::c_int,
    orig: libc::termios,
}

impl EchoOff {
    fn new() -> Option<Self> {
        let fd = libc::STDIN_FILENO;
        if unsafe { libc::isatty(fd) } == 0 {
            return None;
        }
        let mut orig = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut orig) } != 0 {
            return None;
        }
        unsafe {
            SAVED_TERMIOS = orig;
            ECHO_SAVED.store(true, Ordering::Relaxed);
            libc::signal(
                libc::SIGINT,
                restore_echo_on_signal as *const () as libc::sighandler_t,
            );
            libc::signal(
                libc::SIGTERM,
                restore_echo_on_signal as *const () as libc::sighandler_t,
            );
        }
        let mut raw = orig;
        raw.c_lflag &= !libc::ECHO;
        if unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &raw) } != 0 {
            return None;
        }
        Some(Self { fd, orig })
    }
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSAFLUSH, &self.orig);
            ECHO_SAVED.store(false, Ordering::Relaxed);
        }
        eprintln!();
    }
}

/// Minimal no-echo password prompt so this harness doesn't need the `rpassword`
/// crate — good enough for a dev tool, never shipped.
fn rpassword_prompt() -> Zeroizing<String> {
    use std::io::BufRead;
    eprint!("Password: ");
    std::io::stderr().flush().ok();

    let _echo = EchoOff::new();

    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).ok();
    let trimmed = line.trim_end_matches(['\n', '\r']);
    let password = Zeroizing::new(trimmed.to_string());
    line.zeroize();
    password
}
