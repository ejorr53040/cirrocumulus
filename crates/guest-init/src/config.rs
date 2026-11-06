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
    fn a_config_with_only_exec_takes_the_defaults() {
        assert_eq!(
            parse_config(r#"{"exec":"nginx"}"#).unwrap(),
            Config {
                exec: "nginx".into(),
                args: Vec::new(),
                env: Vec::new(),
                workdir: None,
                user: None,
            }
        );
    }
}
