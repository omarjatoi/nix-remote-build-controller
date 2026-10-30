//! Client authentication against an OpenSSH authorized_keys list.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context as _, bail};
use russh::keys::{HashAlg, PublicKey};

/// Authenticates Nix clients against an authorized_keys list.
#[derive(Debug, Clone)]
pub struct ClientAuthorizer {
    /// Maps the wire-format public key to its authorized_keys comment.
    keys: HashMap<Vec<u8>, String>,
}

impl ClientAuthorizer {
    /// Reads an OpenSSH authorized_keys file.
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let data = std::fs::read(path)
            .with_context(|| format!("read authorized keys {}", path.display()))?;
        Self::parse(&data)
    }

    /// Parses authorized_keys content. Blank lines and comments are ignored;
    /// any unparsable line is an error so that a typo does not silently lock
    /// out (or fail to lock out) a client. Key options such as `from=` are
    /// accepted but not enforced.
    pub fn parse(data: &[u8]) -> anyhow::Result<Self> {
        let text = std::str::from_utf8(data).context("authorized keys are not valid UTF-8")?;
        let mut keys = HashMap::new();
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let key = parse_authorized_key(line)
                .map_err(|e| anyhow::anyhow!("authorized keys line {}: {e}", index + 1))?;
            keys.insert(wire_bytes(&key)?, key.comment().to_string());
        }
        if keys.is_empty() {
            bail!("authorized keys contain no keys");
        }
        Ok(Self { keys })
    }

    /// The number of authorized keys.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether the list is empty (never true for a parsed list).
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Whether `key` is in the list.
    pub fn is_authorized(&self, key: &PublicKey) -> bool {
        wire_bytes(key).is_ok_and(|bytes| self.keys.contains_key(&bytes))
    }
}

/// Parses one authorized_keys line: `[options] keytype base64 [comment]`.
///
/// The line is first tried as a bare key; otherwise the leading options field
/// (which may contain quoted strings with spaces and backslash escapes) is
/// skipped and the rest parsed as the key.
fn parse_authorized_key(line: &str) -> anyhow::Result<PublicKey> {
    if let Ok(key) = PublicKey::from_openssh(line) {
        return Ok(key);
    }
    let rest =
        skip_options(line).ok_or_else(|| anyhow::anyhow!("unterminated quote in key options"))?;
    PublicKey::from_openssh(rest.trim_start()).map_err(|e| anyhow::anyhow!("{e}"))
}

/// Returns what follows the options field: everything after the first
/// whitespace that is not inside double quotes. `None` if a quote is left
/// open.
fn skip_options(line: &str) -> Option<&str> {
    let mut in_quotes = false;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' if in_quotes => escaped = true,
            '"' => in_quotes = !in_quotes,
            c if c.is_whitespace() && !in_quotes => return Some(&line[i..]),
            _ => {}
        }
    }
    if in_quotes { None } else { Some("") }
}

fn wire_bytes(key: &PublicKey) -> anyhow::Result<Vec<u8>> {
    key.to_bytes().context("encode public key")
}

/// The OpenSSH SHA256 fingerprint of `key`, e.g. `SHA256:abc...`.
pub fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::keys::generate_host_key;

    #[test]
    fn parses_and_matches_keys() {
        let allowed = generate_host_key().unwrap();
        let other = generate_host_key().unwrap();
        let line = allowed.public_key().to_openssh().unwrap();
        let data = format!(
            "# comment\n\n{line} laptop\nno-pty,from=\"10.0.0.0/8\" {line}\ncommand=\"echo \\\"hi there\\\"\",no-pty {line} a comment\n"
        );
        let auth = ClientAuthorizer::parse(data.as_bytes()).unwrap();
        assert_eq!(auth.len(), 1);
        assert!(auth.is_authorized(allowed.public_key()));
        assert!(!auth.is_authorized(other.public_key()));
        assert!(fingerprint(allowed.public_key()).starts_with("SHA256:"));
    }

    #[test]
    fn rejects_garbage() {
        assert!(ClientAuthorizer::parse(b"ssh-ed25519 not-base64 x\n").is_err());
        assert!(ClientAuthorizer::parse(b"# only comments\n").is_err());
        assert!(ClientAuthorizer::parse(b"").is_err());
        assert!(ClientAuthorizer::parse(b"command=\"unterminated ssh-ed25519 AAAA\n").is_err());
    }
}
