//! Seat key files. A seat's private key lives in one file, mode 0600, in one of two
//! formats holding the same 32-byte Ed25519 seed: 64 hex characters, or an unencrypted
//! PKCS8 PEM (`-----BEGIN PRIVATE KEY-----`, RFC 8410).

use ed25519_dalek::SigningKey;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum KeyFileError {
    #[error("cannot read key file {0}: {1}")]
    Io(String, std::io::Error),
    #[error("key file {0} is readable by group or others; run: chmod 600 {0}")]
    Permissions(String),
    #[error("key file is neither a 64-char hex seed nor an unencrypted Ed25519 PKCS8 PEM")]
    Format,
}

/// RFC 8410 PKCS8 wrapper for an Ed25519 seed: a fixed 16-byte header, then the seed.
const PKCS8_HEADER: [u8; 16] = [
    0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];

pub fn parse(text: &str) -> Result<SigningKey, KeyFileError> {
    let text = text.trim();
    let seed: Vec<u8> = if text.starts_with("-----BEGIN") {
        if !text.contains("-----BEGIN PRIVATE KEY-----") {
            return Err(KeyFileError::Format);
        }
        let der = base64_decode(text.lines().filter(|l| !l.starts_with("-----")))?;
        if der.len() != 48 || der[..16] != PKCS8_HEADER {
            return Err(KeyFileError::Format);
        }
        der[16..].to_vec()
    } else {
        hex::decode(text).map_err(|_| KeyFileError::Format)?
    };
    let seed: [u8; 32] = seed.try_into().map_err(|_| KeyFileError::Format)?;
    Ok(SigningKey::from_bytes(&seed))
}

pub fn load(path: &Path) -> Result<SigningKey, KeyFileError> {
    let shown = path.display().to_string();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(path).map_err(|e| KeyFileError::Io(shown.clone(), e))?;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(KeyFileError::Permissions(shown));
        }
    }
    let text = std::fs::read_to_string(path).map_err(|e| KeyFileError::Io(shown, e))?;
    parse(&text)
}

/// Write a new hex seed file, mode 0600, refusing to overwrite an existing key.
pub fn create(path: &Path, sk: &SigningKey) -> Result<(), KeyFileError> {
    use std::io::Write;
    let shown = path.display().to_string();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| KeyFileError::Io(shown.clone(), e))?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(path)
        .map_err(|e| KeyFileError::Io(shown.clone(), e))?;
    writeln!(f, "{}", hex::encode(sk.to_bytes())).map_err(|e| KeyFileError::Io(shown, e))
}

fn base64_decode<'a>(lines: impl Iterator<Item = &'a str>) -> Result<Vec<u8>, KeyFileError> {
    let mut out = Vec::with_capacity(48);
    let (mut acc, mut bits) = (0u32, 0u8);
    for c in lines.flat_map(|l| l.bytes()) {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\r' | b' ' => continue,
            _ => return Err(KeyFileError::Format),
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 8410 §10.3 example key.
    const PEM: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEINTuctv5E1hK1bbY8fdp+K06/nwoy/HU++CXqI9EdVhC\n-----END PRIVATE KEY-----\n";
    const SEED: &str = "d4ee72dbf913584ad5b6d8f1f769f8ad3afe7c28cbf1d4fbe097a88f44755842";
    const PUBKEY: &str = "19bf44096984cdfe8541bac167dc3b96c85086aa30b6b6cb0c5c38ad703166e1";

    #[test]
    fn pem_and_hex_load_the_same_key() {
        for text in [PEM, SEED] {
            assert_eq!(crate::pubkey_hex(&parse(text).unwrap()), PUBKEY);
        }
    }

    #[test]
    fn garbage_is_rejected() {
        for text in [
            "",
            "zz",
            "abcd",
            "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----",
            "-----BEGIN ENCRYPTED PRIVATE KEY-----\nAAAA\n-----END ENCRYPTED PRIVATE KEY-----",
        ] {
            assert!(parse(text).is_err(), "{text:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn create_is_0600_and_never_overwrites_and_load_checks_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("oracle-keyfile-{}", crate::random_hex(6)));
        let path = dir.join("seat.key");
        let sk = crate::generate_key();
        create(&path, &sk).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(create(&path, &sk).is_err(), "must not overwrite");
        assert_eq!(load(&path).unwrap().to_bytes(), sk.to_bytes());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(load(&path), Err(KeyFileError::Permissions(_))));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
