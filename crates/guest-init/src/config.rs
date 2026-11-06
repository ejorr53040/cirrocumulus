//! The app config the Node agent sends guest-init over vsock (RESEARCH.md
//! M2): both ends use these types, so they can't disagree on its shape.

use serde::{Deserialize, Serialize};

/// The vsock port guest-init listens on for its config.
pub const CONFIG_PORT: u32 = 52;

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
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

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone, Copy)]
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
    fn a_config_the_node_sends_parses_back_the_same() {
        for config in [
            Config {
                exec: "/app".into(),
                args: vec!["--flag".into(), "value".into()],
                env: vec!["A=1".into()],
                workdir: Some("/srv".into()),
                user: Some(User { uid: 101, gid: 102 }),
            },
            Config {
                exec: "nginx".into(),
                args: Vec::new(),
                env: Vec::new(),
                workdir: None,
                user: None,
            },
        ] {
            let sent = serde_json::to_string(&config).unwrap();
            assert_eq!(parse_config(&sent).unwrap(), config);
        }
    }
}
