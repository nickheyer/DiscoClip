//! Secrets at rest: sealed with AES-256-GCM under a key kept beside the database.
//!
//! A sealed value is `v1.` followed by URL-safe base64 of the nonce and ciphertext. The
//! context a value is sealed with, such as the row and column it belongs to, is bound into
//! the ciphertext, so a value moved to another place will not open.

use std::path::{Path, PathBuf};

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

pub const KEY_FILE: &str = "secret.key";
const VERSION: &str = "v1.";
const NONCE_LEN: usize = 12;

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("key file {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("key file {0} does not hold 32 bytes of hex")]
    Malformed(PathBuf),
    #[error("a sealed value cannot be opened; it was sealed under another key or moved")]
    Unsealable,
}

#[derive(Clone)]
pub struct Keyring {
    cipher: Aes256Gcm,
}

impl Keyring {
    pub fn from_key(key: [u8; 32]) -> Self {
        Self {
            cipher: Aes256Gcm::new(&Key::<Aes256Gcm>::from(key)),
        }
    }

    /// Reads the key at `path`, creating a random one readable only by its owner when the
    /// file does not exist yet.
    pub fn load_or_create(path: &Path) -> Result<Self, SecretError> {
        let io = |source| SecretError::Io {
            path: path.to_path_buf(),
            source,
        };
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let bytes = hex::decode(text.trim())
                    .map_err(|_| SecretError::Malformed(path.to_path_buf()))?;
                let key: [u8; 32] = bytes
                    .try_into()
                    .map_err(|_| SecretError::Malformed(path.to_path_buf()))?;
                Ok(Self::from_key(key))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(io)?;
                }
                let mut key = [0u8; 32];
                rand::fill(&mut key);
                let mut options = std::fs::OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                let mut file = options.open(path).map_err(io)?;
                use std::io::Write;
                writeln!(file, "{}", hex::encode(key)).map_err(io)?;
                Ok(Self::from_key(key))
            }
            Err(source) => Err(io(source)),
        }
    }

    /// Encrypts `plaintext`, bound to `context`.
    pub fn seal(&self, plaintext: &[u8], context: &str) -> String {
        let mut nonce = [0u8; NONCE_LEN];
        rand::fill(&mut nonce);
        let ciphertext = self
            .cipher
            .encrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: plaintext,
                    aad: context.as_bytes(),
                },
            )
            .expect("AES-GCM encryption does not fail on in-memory data");
        let mut bytes = nonce.to_vec();
        bytes.extend(ciphertext);
        format!("{VERSION}{}", URL_SAFE_NO_PAD.encode(bytes))
    }

    /// Decrypts what [`seal`](Self::seal) produced under the same `context`.
    pub fn open(&self, sealed: &str, context: &str) -> Result<Vec<u8>, SecretError> {
        let encoded = sealed
            .strip_prefix(VERSION)
            .ok_or(SecretError::Unsealable)?;
        let bytes = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| SecretError::Unsealable)?;
        if bytes.len() < NONCE_LEN {
            return Err(SecretError::Unsealable);
        }
        let (nonce, ciphertext) = bytes.split_at(NONCE_LEN);
        let nonce: [u8; NONCE_LEN] = nonce.try_into().expect("split at the nonce length");
        self.cipher
            .decrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: ciphertext,
                    aad: context.as_bytes(),
                },
            )
            .map_err(|_| SecretError::Unsealable)
    }

    pub fn seal_str(&self, text: &str, context: &str) -> String {
        self.seal(text.as_bytes(), context)
    }

    pub fn open_str(&self, sealed: &str, context: &str) -> Result<String, SecretError> {
        String::from_utf8(self.open(sealed, context)?).map_err(|_| SecretError::Unsealable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seals_and_opens_under_the_same_context() {
        let keyring = Keyring::from_key([7; 32]);
        let sealed = keyring.seal_str("hunter2", "row 1");
        assert!(sealed.starts_with("v1."));
        assert_ne!(sealed, keyring.seal_str("hunter2", "row 1"));
        assert_eq!(keyring.open_str(&sealed, "row 1").unwrap(), "hunter2");
        assert!(matches!(
            keyring.open_str(&sealed, "row 2"),
            Err(SecretError::Unsealable)
        ));
        assert!(matches!(
            Keyring::from_key([8; 32]).open_str(&sealed, "row 1"),
            Err(SecretError::Unsealable)
        ));
        assert!(matches!(
            keyring.open_str("v1.short", "row 1"),
            Err(SecretError::Unsealable)
        ));
        assert!(matches!(
            keyring.open_str("plain", "row 1"),
            Err(SecretError::Unsealable)
        ));
    }

    #[test]
    fn the_key_file_is_created_once_and_reused() {
        let dir = std::env::temp_dir().join(format!("discoclip-keyring-{}", uuid::Uuid::now_v7()));
        let path = dir.join("nested").join(KEY_FILE);
        let first = Keyring::load_or_create(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.trim().len(), 64);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let second = Keyring::load_or_create(&path).unwrap();
        let sealed = first.seal_str("same key", "ctx");
        assert_eq!(second.open_str(&sealed, "ctx").unwrap(), "same key");

        std::fs::write(&path, "not hex\n").unwrap();
        assert!(matches!(
            Keyring::load_or_create(&path),
            Err(SecretError::Malformed(_))
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
