//! How the app is launched: its environment and which file `exec` names.
//! Pure, so the rules are unit-tested here rather than only in a real boot.

use std::path::{Path, PathBuf};

/// Docker's `PATH` for images that don't set one.
const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// The app's environment: `env` as given, plus `PATH` and `HOME=/` when it
/// doesn't set them, as Docker does.
pub(crate) fn app_env(env: &[String]) -> Vec<String> {
    let has = |key: &str| {
        env.iter()
            .any(|e| e.split_once('=').is_some_and(|(k, _)| k == key))
    };
    let mut out = env.to_vec();
    if !has("PATH") {
        out.push(format!("PATH={DEFAULT_PATH}"));
    }
    if !has("HOME") {
        out.push("HOME=/".to_string());
    }
    out
}

/// The value of `key` in `env`; the last entry wins, as in `execve`'s callers.
pub(crate) fn lookup<'a>(env: &'a [String], key: &str) -> Option<&'a str> {
    env.iter()
        .rev()
        .find_map(|e| e.split_once('=').filter(|(k, _)| *k == key).map(|(_, v)| v))
}

/// The file to run for `exec`: `exec` itself when it contains a `/`,
/// otherwise the first `PATH` directory holding an executable of that name.
pub(crate) fn resolve(
    exec: &str,
    path: &str,
    is_executable: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    if exec.contains('/') {
        return Some(PathBuf::from(exec));
    }
    path.split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join(exec))
        .find(|candidate| is_executable(candidate))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn adds_path_and_home_when_missing() {
        let env = app_env(&strings(&["A=1"]));

        assert_eq!(
            env,
            strings(&["A=1", &format!("PATH={DEFAULT_PATH}"), "HOME=/"])
        );
    }

    #[test]
    fn keeps_path_and_home_the_config_sets() {
        let env = app_env(&strings(&["PATH=/app", "HOME=/root"]));

        assert_eq!(env, strings(&["PATH=/app", "HOME=/root"]));
    }

    #[test]
    fn a_key_that_only_starts_with_path_is_not_path() {
        let env = app_env(&strings(&["PATHS=/x"]));

        assert_eq!(lookup(&env, "PATH"), Some(DEFAULT_PATH));
    }

    #[test]
    fn lookup_takes_the_last_entry() {
        assert_eq!(lookup(&strings(&["A=1", "A=2"]), "A"), Some("2"));
    }

    #[test]
    fn an_exec_with_a_slash_is_used_as_is() {
        let found = resolve("./run", "/bin", |_| false);

        assert_eq!(found, Some(PathBuf::from("./run")));
    }

    #[test]
    fn a_bare_exec_is_the_first_executable_match_on_path() {
        let found = resolve("nginx", "/usr/local/bin::/usr/sbin:/usr/bin", |p| {
            p == Path::new("/usr/sbin/nginx") || p == Path::new("/usr/bin/nginx")
        });

        assert_eq!(found, Some(PathBuf::from("/usr/sbin/nginx")));
    }

    #[test]
    fn a_bare_exec_not_on_path_is_not_found() {
        assert_eq!(resolve("nginx", "/bin", |_| false), None);
    }
}
