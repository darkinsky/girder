//! Content-addressed image bytes under `attachments/v1`.
//!
//! The id is `sha256:` plus the hex digest of the admitted bytes. The same image stored twice is
//! one object: a later upload cannot change what an earlier reference means, because the path is
//! the digest and an existing object is left as it was written.

use std::path::{Path, PathBuf};

use serde_json::json;

use super::{AttachmentError, DecodedImage, ImageRef, ImageShelf};

/// How many distinct objects the disk store will account for.
///
/// Far above one conversation. The cap still exists so a caller that keeps uploading unique images
/// is refused instead of filling the disk without a number in the error.
const DISK_CAPACITY: usize = 100_000;

/// A directory of immutable image objects.
#[derive(Clone, Debug)]
pub struct FileStore {
    root: PathBuf,
}

/// The directory `serve` and the live provider both read.
///
/// `GIRDER_HOME` when it is absolute, otherwise `~/.girder`, then `attachments/v1`. A relative
/// override cannot be resolved, so the store falls back to the system temporary directory rather
/// than a path that changes with the launch directory.
#[must_use]
pub fn environment_root() -> PathBuf {
    let home = std::env::var("HOME").ok();
    let override_home = std::env::var("GIRDER_HOME").ok();
    crate::util::home_paths::Paths::from_environment(home.as_deref(), override_home.as_deref())
        .map(|paths| paths.attachments())
        .unwrap_or_else(|_| {
            std::env::temp_dir()
                .join("girder")
                .join("attachments")
                .join("v1")
        })
}

impl FileStore {
    /// Opens the store at `root`.
    ///
    /// The directory is created on the first save. `root` has to be absolute: a relative root would
    /// follow whatever directory the process was started from.
    ///
    /// # Errors
    ///
    /// Returns [`AttachmentError::Store`] when `root` is relative or empty.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, AttachmentError> {
        let root = root.into();
        if root.as_os_str().is_empty() || !root.is_absolute() {
            return Err(AttachmentError::Store {
                reason: format!(
                    "the attachment directory must be absolute, got `{}`",
                    root.display()
                ),
            });
        }
        Ok(Self { root })
    }

    /// The directory objects are written under.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn object_path(&self, hex: &str) -> PathBuf {
        self.root.join("objects").join(&hex[..2]).join(hex)
    }

    fn meta_path(&self, hex: &str) -> PathBuf {
        self.root
            .join("objects")
            .join(&hex[..2])
            .join(format!("{hex}.meta"))
    }

    fn count(&self) -> usize {
        let objects = self.root.join("objects");
        let Ok(shards) = std::fs::read_dir(&objects) else {
            return 0;
        };
        let mut total = 0;
        for shard in shards.flatten() {
            let Ok(entries) = std::fs::read_dir(shard.path()) else {
                continue;
            };
            total += entries
                .flatten()
                .filter(|entry| {
                    entry
                        .path()
                        .extension()
                        .is_none_or(|extension| extension != "meta" && extension != "partial")
                })
                .count();
        }
        total
    }
}

impl ImageShelf for FileStore {
    fn insert(&self, _id: &str, image: DecodedImage) -> Result<ImageRef, AttachmentError> {
        let digest = sha256(&image.data);
        let hex = hex_encode(&digest);
        let id = format!("sha256:{hex}");
        let object = self.object_path(&hex);
        if !object.is_file() {
            crate::util::atomic_write::write_atomic(
                &object,
                &image.data,
                crate::util::atomic_write::Mode::PRIVATE,
            )
            .map_err(|error| AttachmentError::Store {
                reason: error.to_string(),
            })?;
        }
        let meta = self.meta_path(&hex);
        if !meta.is_file() {
            let body = json!({
                "mediaType": image.media_type,
                "name": display_name(image.name.as_deref()),
            });
            crate::util::atomic_write::write_text_atomic(
                &meta,
                &body.to_string(),
                crate::util::atomic_write::Mode::PRIVATE,
            )
            .map_err(|error| AttachmentError::Store {
                reason: error.to_string(),
            })?;
        }
        Ok(ImageRef {
            id,
            media_type: image.media_type,
            bytes: u64::try_from(image.data.len()).unwrap_or(u64::MAX),
            name: display_name(image.name.as_deref()),
        })
    }

    fn fetch(&self, id: &str) -> Option<DecodedImage> {
        let hex = digest_hex(id)?;
        let bytes = std::fs::read(self.object_path(hex)).ok()?;
        if hex_encode(&sha256(&bytes)) != hex {
            return None;
        }
        let meta = std::fs::read_to_string(self.meta_path(hex)).ok();
        let parsed = meta.and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok());
        let media_type = parsed
            .as_ref()
            .and_then(|value| value.get("mediaType"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("application/octet-stream")
            .to_owned();
        let name = parsed
            .as_ref()
            .and_then(|value| value.get("name"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        Some(DecodedImage {
            data: bytes,
            media_type,
            name,
        })
    }

    fn len(&self) -> usize {
        self.count()
    }

    fn capacity(&self) -> usize {
        DISK_CAPACITY
    }
}

fn digest_hex(id: &str) -> Option<&str> {
    let hex = id.strip_prefix("sha256:")?;
    if hex.len() == 64
        && hex
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        Some(hex)
    } else {
        None
    }
}

fn display_name(value: Option<&str>) -> Option<String> {
    let value = value?;
    let split = value.rfind(['/', '\\']).map_or(0, |index| index + 1);
    let leaf: String = value[split..]
        .chars()
        .filter(|char| !char.is_control())
        .take(255)
        .collect();
    let leaf = leaf.trim();
    if leaf.is_empty() || leaf == "." || leaf == ".." {
        None
    } else {
        Some(leaf.to_owned())
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// SHA-256 of `message`, FIPS 180-4.
fn sha256(message: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut state: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_len = (message.len() as u64).saturating_mul(8);
    let mut padded = message.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in padded.chunks(64) {
        let mut words = [0u32; 64];
        for index in 0..16 {
            let start = index * 4;
            words[index] = u32::from_be_bytes(chunk[start..start + 4].try_into().expect("4 bytes"));
        }
        for index in 16..64 {
            let small = words[index - 15].rotate_right(7)
                ^ words[index - 15].rotate_right(18)
                ^ (words[index - 15] >> 3);
            let large = words[index - 2].rotate_right(17)
                ^ words[index - 2].rotate_right(19)
                ^ (words[index - 2] >> 10);
            words[index] = words[index - 16]
                .wrapping_add(small)
                .wrapping_add(words[index - 7])
                .wrapping_add(large);
        }
        let mut a = state[0];
        let mut b = state[1];
        let mut c = state[2];
        let mut d = state[3];
        let mut e = state[4];
        let mut f = state[5];
        let mut g = state[6];
        let mut h = state[7];
        for index in 0..64 {
            let sigma1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choose = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(sigma1)
                .wrapping_add(choose)
                .wrapping_add(K[index])
                .wrapping_add(words[index]);
            let sigma0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = sigma0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
        state[5] = state[5].wrapping_add(f);
        state[6] = state[6].wrapping_add(g);
        state[7] = state[7].wrapping_add(h);
    }
    let mut out = [0u8; 32];
    for (index, word) in state.iter().enumerate() {
        out[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("girder-attach-{}-{sequence}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    #[test]
    fn sha256_matches_the_empty_and_abc_vectors() {
        assert_eq!(
            hex_encode(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex_encode(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn the_same_bytes_share_one_object_and_a_new_store_reads_it() {
        let root = scratch();
        let store = FileStore::open(&root).expect("open");
        let image = DecodedImage {
            data: b"png-bytes".to_vec(),
            media_type: "image/png".to_owned(),
            name: Some(r"C:\shots\diagram.png".to_owned()),
        };
        let first = store.insert("ignored", image.clone()).expect("save");
        let second = store.insert("other", image).expect("save again");
        assert_eq!(first.id, second.id);
        assert!(first.id.starts_with("sha256:"));
        assert_eq!(first.name.as_deref(), Some("diagram.png"));
        assert_eq!(store.len(), 1);

        let reopened = FileStore::open(&root).expect("reopen");
        let loaded = reopened.fetch(&first.id).expect("loaded");
        assert_eq!(loaded.data, b"png-bytes");
        assert_eq!(loaded.media_type, "image/png");
        assert_eq!(loaded.name.as_deref(), Some("diagram.png"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_tampered_object_is_not_returned() {
        let root = scratch();
        let store = FileStore::open(&root).expect("open");
        let saved = store
            .insert(
                "ignored",
                DecodedImage {
                    data: b"intact".to_vec(),
                    media_type: "image/png".to_owned(),
                    name: None,
                },
            )
            .expect("save");
        let hex = saved.id.strip_prefix("sha256:").expect("prefix");
        std::fs::write(store.object_path(hex), b"changed").expect("tamper");
        assert!(store.fetch(&saved.id).is_none());
        assert!(store.fetch("../secrets").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_relative_root_is_refused() {
        let error = FileStore::open("attachments").expect_err("relative");
        assert!(matches!(error, AttachmentError::Store { .. }));
    }
}
