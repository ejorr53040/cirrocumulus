//! Parses the app config guest-init reads before exec'ing it. Slice 2 reads
//! this from a fixed path baked into the rootfs (`/etc/cirro-init.json`);
//! a later slice replaces that source with vsock, keeping this same shape
//! since it's JSON either way (see RESEARCH.md M2).

use serde::Deserialize;

#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct Config {
    pub exec: String,
    #[serde(default)]
    pub args: Vec<String>,
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
    fn rejects_config_missing_exec() {
        assert!(parse_config(r#"{"args": []}"#).is_err());
    }
}
