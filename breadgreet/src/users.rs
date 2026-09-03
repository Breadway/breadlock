//! Enumerating the human users greetd could log in.
//!
//! On a typical single-user desktop this lets the greeter skip the "type your
//! username" step entirely: one human account → go straight to the password
//! prompt; several → offer a picker. The list comes from `/etc/passwd`
//! (world-readable, so this works as the unprivileged `greeter` user),
//! filtered to the login-user UID range from `/etc/login.defs`.

/// A local account the greeter can offer as a login target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    /// The login name (what greetd's `CreateSession` wants).
    pub name: String,
    /// The GECOS full name if the account has one, else `name` — this is what
    /// the picker shows.
    pub display: String,
    pub uid: u32,
}

/// Human users on this system, sorted by UID. Empty if `/etc/passwd` can't be
/// read or holds no login accounts (the greeter then falls back to a typed
/// username).
pub fn list() -> Vec<User> {
    let passwd = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
    let login_defs = std::fs::read_to_string("/etc/login.defs").ok();
    parse(&passwd, login_defs.as_deref())
}

/// `(UID_MIN, UID_MAX)` from `/etc/login.defs`, or shadow's defaults.
fn uid_bounds(login_defs: Option<&str>) -> (u32, u32) {
    let (mut min, mut max) = (1000u32, 60000u32);
    for line in login_defs.unwrap_or_default().lines() {
        let mut it = line.split_whitespace();
        match it.next() {
            Some("UID_MIN") => {
                if let Some(Ok(n)) = it.next().map(str::parse) {
                    min = n;
                }
            }
            Some("UID_MAX") => {
                if let Some(Ok(n)) = it.next().map(str::parse) {
                    max = n;
                }
            }
            _ => {}
        }
    }
    (min, max)
}

/// A shell that actually lets someone log in — excludes the `nologin` / `false`
/// placeholders system accounts use.
fn is_login_shell(shell: &str) -> bool {
    !shell.is_empty() && !shell.ends_with("nologin") && !shell.ends_with("/false")
}

fn parse(passwd: &str, login_defs: Option<&str>) -> Vec<User> {
    let (min, max) = uid_bounds(login_defs);
    let mut users: Vec<User> = passwd
        .lines()
        .filter_map(|line| {
            // name:passwd:uid:gid:gecos:home:shell
            let mut f = line.split(':');
            let name = f.next()?;
            let _passwd = f.next()?;
            let uid: u32 = f.next()?.parse().ok()?;
            let _gid = f.next()?;
            let gecos = f.next().unwrap_or("");
            let _home = f.next()?;
            let shell = f.next().unwrap_or("");
            if uid < min || uid > max || name == "nobody" || !is_login_shell(shell) {
                return None;
            }
            let full = gecos.split(',').next().unwrap_or("").trim();
            let display = if full.is_empty() { name } else { full }.to_string();
            Some(User {
                name: name.to_string(),
                display,
                uid,
            })
        })
        .collect();
    users.sort_by(|a, b| a.uid.cmp(&b.uid).then_with(|| a.name.cmp(&b.name)));
    users.dedup_by(|a, b| a.name == b.name);
    users
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWD: &str = "\
root:x:0:0:root:/root:/bin/bash
bin:x:1:1::/:/usr/bin/nologin
nobody:x:65534:65534:Nobody:/:/usr/bin/nologin
riley:x:1000:1000:Riley Horsham,,,:/home/riley:/bin/zsh
guest:x:1001:1001::/home/guest:/bin/bash
svc:x:850:850:some service:/var/lib/svc:/bin/bash
noshell:x:1002:1002::/home/noshell:/usr/sbin/nologin
falseshell:x:1003:1003::/home/f:/bin/false
";

    #[test]
    fn keeps_only_human_login_accounts() {
        let users = parse(PASSWD, Some("UID_MIN 1000\nUID_MAX 60000\n"));
        let names: Vec<&str> = users.iter().map(|u| u.name.as_str()).collect();
        assert_eq!(names, ["riley", "guest"]);
    }

    #[test]
    fn gecos_full_name_becomes_the_display_name() {
        let users = parse(PASSWD, None);
        let riley = users.iter().find(|u| u.name == "riley").unwrap();
        assert_eq!(riley.display, "Riley Horsham");
        let guest = users.iter().find(|u| u.name == "guest").unwrap();
        assert_eq!(guest.display, "guest"); // no GECOS -> falls back to name
    }

    #[test]
    fn respects_uid_min_from_login_defs() {
        // Lowering UID_MIN pulls the service account (uid 850) into range;
        // it still has a real shell so it now counts.
        let users = parse(PASSWD, Some("UID_MIN 500\nUID_MAX 60000\n"));
        let names: Vec<&str> = users.iter().map(|u| u.name.as_str()).collect();
        assert_eq!(names, ["svc", "riley", "guest"]);
    }

    #[test]
    fn sorted_by_uid() {
        let users = parse(PASSWD, Some("UID_MIN 500\n"));
        assert!(users.windows(2).all(|w| w[0].uid <= w[1].uid));
    }

    #[test]
    fn empty_passwd_yields_nothing() {
        assert!(parse("", None).is_empty());
    }

    #[test]
    fn defaults_when_login_defs_absent() {
        assert_eq!(uid_bounds(None), (1000, 60000));
    }
}
