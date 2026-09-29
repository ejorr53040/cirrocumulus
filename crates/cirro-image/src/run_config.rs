//! What an image asks for when it runs, taken from its OCI config with
//! Docker's rules, so `cirro run nginx:alpine` does what `docker run` does.

use crate::Error;
use serde::{Deserialize, Serialize};

/// An image's `Entrypoint`, `Cmd`, `Env`, `WorkingDir` and `User`, with the
/// user already resolved to numbers against the image's own `/etc/passwd`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunConfig {
    pub entrypoint: Vec<String>,
    pub cmd: Vec<String>,
    pub env: Vec<String>,
    pub workdir: Option<String>,
    /// `(uid, gid)`; `None` runs as root.
    pub user: Option<(u32, u32)>,
}

impl RunConfig {
    /// The command to run: the entrypoint, followed by `args` when any are
    /// given and by the image's `Cmd` otherwise.
    pub fn command(&self, args: &[String]) -> Vec<String> {
        let rest = if args.is_empty() { &self.cmd } else { args };
        [self.entrypoint.as_slice(), rest].concat()
    }
}

/// `env` with each of `overrides` replacing the entry for the same key, or
/// added after them. glibc's `getenv` takes the first match, so a key must
/// appear once.
pub fn merge_env(env: &[String], overrides: &[String]) -> Vec<String> {
    let key = |entry: &str| entry.split_once('=').map_or(entry, |(k, _)| k).to_string();
    let mut merged: Vec<String> = env.to_vec();
    for entry in overrides {
        let k = key(entry);
        match merged.iter_mut().find(|e| key(e) == k) {
            Some(existing) => *existing = entry.clone(),
            None => merged.push(entry.clone()),
        }
    }
    merged
}

/// Resolves an image's `User` (`name`, `uid`, `name:group` or `uid:gid`)
/// the way Docker does: names come from the image's `passwd` and `group`,
/// a user's group defaults to its primary one, and a numeric uid the image
/// doesn't list gets gid 0.
pub fn resolve_user(
    user: &str,
    passwd: Option<&str>,
    group: Option<&str>,
) -> Result<Option<(u32, u32)>, Error> {
    if user.is_empty() {
        return Ok(None);
    }
    let (user_part, group_part) = match user.split_once(':') {
        Some((u, g)) => (u, Some(g)),
        None => (user, None),
    };
    // passwd lines: name:x:uid:gid:gecos:home:shell
    let passwd_entry = |matches: &dyn Fn(&[&str]) -> bool| {
        passwd?
            .lines()
            .map(|l| l.split(':').collect::<Vec<_>>())
            .find(|f| f.len() >= 4 && matches(f))
            .and_then(|f| Some((f[2].parse::<u32>().ok()?, f[3].parse::<u32>().ok()?)))
    };
    let (uid, primary_gid) = match user_part.parse::<u32>() {
        Ok(uid) => (
            uid,
            passwd_entry(&|f| f[2] == user_part).map_or(0, |(_, gid)| gid),
        ),
        Err(_) => passwd_entry(&|f| f[0] == user_part).ok_or_else(|| {
            Error(format!(
                "the image's user {user_part:?} isn't in its /etc/passwd"
            ))
        })?,
    };
    let gid = match group_part {
        None | Some("") => primary_gid,
        Some(g) => match g.parse::<u32>() {
            Ok(gid) => gid,
            // group lines: name:x:gid:members
            Err(_) => group
                .and_then(|text| {
                    text.lines()
                        .map(|l| l.split(':').collect::<Vec<_>>())
                        .find(|f| f.len() >= 3 && f[0] == g)
                        .and_then(|f| f[2].parse().ok())
                })
                .ok_or_else(|| Error(format!("the image's group {g:?} isn't in its /etc/group")))?,
        },
    };
    Ok(Some((uid, gid)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    const PASSWD: &str =
        "root:x:0:0:root:/root:/bin/sh\nnginx:x:101:101:nginx:/var/cache/nginx:/sbin/nologin\n";
    const GROUP: &str = "root:x:0:root\nnginx:x:101:nginx\nwww-data:x:82:\n";

    #[test]
    fn the_command_is_the_entrypoint_then_cmd() {
        let config = RunConfig {
            entrypoint: strings(&["/docker-entrypoint.sh"]),
            cmd: strings(&["nginx", "-g", "daemon off;"]),
            ..Default::default()
        };

        assert_eq!(
            config.command(&[]),
            strings(&["/docker-entrypoint.sh", "nginx", "-g", "daemon off;"])
        );
        assert_eq!(
            config.command(&strings(&["nginx", "-t"])),
            strings(&["/docker-entrypoint.sh", "nginx", "-t"])
        );
    }

    #[test]
    fn an_image_with_neither_has_no_command() {
        assert!(RunConfig::default().command(&[]).is_empty());
    }

    #[test]
    fn overrides_replace_env_entries_by_key() {
        let env = merge_env(
            &strings(&["PATH=/usr/bin", "NGINX_VERSION=1.27"]),
            &strings(&["PATH=/app", "EXTRA=1"]),
        );

        assert_eq!(
            env,
            strings(&["PATH=/app", "NGINX_VERSION=1.27", "EXTRA=1"])
        );
    }

    #[test]
    fn resolves_users_like_docker() {
        let resolve = |u| resolve_user(u, Some(PASSWD), Some(GROUP)).unwrap();

        assert_eq!(resolve(""), None);
        assert_eq!(resolve("nginx"), Some((101, 101)));
        assert_eq!(resolve("101"), Some((101, 101)));
        assert_eq!(resolve("nginx:www-data"), Some((101, 82)));
        assert_eq!(resolve("nginx:5"), Some((101, 5)));
        assert_eq!(resolve("1234"), Some((1234, 0)));
        assert_eq!(resolve("1234:1234"), Some((1234, 1234)));
    }

    #[test]
    fn unknown_names_are_errors() {
        let e = resolve_user("ghost", Some(PASSWD), Some(GROUP)).unwrap_err();
        assert!(e.0.contains("ghost"), "{e}");

        let e = resolve_user("nginx:ghosts", Some(PASSWD), Some(GROUP)).unwrap_err();
        assert!(e.0.contains("ghosts"), "{e}");

        assert!(resolve_user("nginx", None, None).is_err());
    }
}
