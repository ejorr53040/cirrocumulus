//! Parses the app config the Node agent sends over vsock (RESEARCH.md M2).
//! Every field but `exec` is optional, so older senders still work.

use serde::Deserialize;

#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct Config {
    pub exec: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// `KEY=VALUE` entries; see `launch::app_env` for the defaults added.
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub workdir: Option<String>,
    #[serde(default)]
    pub user: Option<User>,
}

#[derive(Debug, Deserialize, PartialEq, Eq, Clone, Copy)]
pub struct User {
    pub uid: u32,
    pub gid: u32,
}

pub fn parse_config(json: &str) -> Result<Config, serde_json::Error> {
    serde_json::from_str(json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_exec_path_and_args() {
        let config = parse_config(r#"{"exec": "/app", "args": ["--flag", "value"]}"#).unwrap();

        assert_eq!(config.exec, "/app");
        assert_eq!(config.args, vec!["--flag", "value"]);
    }

    #[test]
    fn defaults_args_to_empty_when_omitted() {
        let config = parse_config(r#"{"exec": "/app"}"#).unwrap();

        assert_eq!(config.args, Vec::<String>::new());
    }

    #[test]
    fn parses_env_workdir_and_user() {
        let config = parse_config(
            r#"{"exec": "nginx", "env": ["A=1"], "workdir": "/srv", "user": {"uid": 101, "gid": 102}}"#,
        )
        .unwrap();

        assert_eq!(config.env, vec!["A=1"]);
        assert_eq!(config.workdir.as_deref(), Some("/srv"));
        assert_eq!(config.user, Some(User { uid: 101, gid: 102 }));
    }

    #[test]
    fn treats_null_workdir_and_user_as_unset() {
        let config = parse_config(r#"{"exec": "/app", "workdir": null, "user": null}"#).unwrap();

        assert_eq!(config.workdir, None);
        assert_eq!(config.user, None);
    }

    #[test]
    fn rejects_config_missing_exec() {
        assert!(parse_config(r#"{"args": []}"#).is_err());
    }
}
