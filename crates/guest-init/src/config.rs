//! The app config the Node agent sends guest-init over vsock: both ends use
//! these types, so they can't disagree on its shape.

use serde::{Deserialize, Serialize};
use std::io::Read;

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
    pub workdir: Option<String>,
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

/// The largest config accepted. A config is a command line, an environment
/// and a user: kilobytes in practice. The cap bounds what a bad sender can
/// make PID 1 hold in memory.
pub const MAX_CONFIG_BYTES: u64 = 1 << 20;

/// Reads a config to EOF, refusing one over [`MAX_CONFIG_BYTES`]. Reads at
/// most one byte past the cap, so an endless sender costs the cap and no
/// more.
pub fn read_config(reader: impl Read) -> Result<Config, String> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("read config: {e}"))?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err(format!("config is over {MAX_CONFIG_BYTES} bytes"));
    }
    let json = std::str::from_utf8(&bytes).map_err(|e| format!("config is not UTF-8: {e}"))?;
    parse_config(json).map_err(|e| format!("config is invalid: {e}"))
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

    fn padded_to(len: usize, json: &str) -> Vec<u8> {
        let mut bytes = json.as_bytes().to_vec();
        bytes.resize(len, b' ');
        bytes
    }

    #[test]
    fn a_config_of_exactly_the_cap_is_read() {
        let bytes = padded_to(MAX_CONFIG_BYTES as usize, r#"{"exec":"nginx"}"#);

        assert_eq!(read_config(&bytes[..]).unwrap().exec, "nginx");
    }

    #[test]
    fn a_config_one_byte_over_the_cap_is_refused_before_parsing() {
        let bytes = padded_to(MAX_CONFIG_BYTES as usize + 1, r#"{"exec":"nginx"}"#);

        assert_eq!(
            read_config(&bytes[..]).unwrap_err(),
            "config is over 1048576 bytes"
        );
    }

    #[test]
    fn an_endless_sender_is_stopped_at_the_cap() {
        assert_eq!(
            read_config(std::io::repeat(b' ')).unwrap_err(),
            "config is over 1048576 bytes"
        );
    }

    #[test]
    fn a_config_that_is_not_utf8_is_an_error_not_a_panic() {
        let err = read_config(&[0xff, 0xfe][..]).unwrap_err();

        assert!(err.starts_with("config is not UTF-8"), "{err}");
    }

    #[test]
    fn a_truncated_config_is_an_error_not_a_panic() {
        let err = read_config(&br#"{"exec":"ngi"#[..]).unwrap_err();

        assert!(err.starts_with("config is invalid"), "{err}");
    }

    #[test]
    fn a_config_without_exec_is_an_error_not_a_panic() {
        let err = read_config(&br#"{"args":[]}"#[..]).unwrap_err();

        assert!(err.starts_with("config is invalid"), "{err}");
    }
}
