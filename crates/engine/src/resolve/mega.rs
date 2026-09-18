//! MEGA files and folders. Files are stored encrypted with a key that only the link
//! carries: the download link comes from the API, the file's name from its attributes
//! decrypted with that key, and the bytes are decrypted with AES-128 in counter mode as
//! they are fetched. A file of any kind resolves as what its name says it is. A folder
//! link lists its nodes, each node's key decrypted with the folder's, and its files become
//! a playlist whose entries name their node.

use std::sync::atomic::{AtomicU64, Ordering};

use aes::Aes128;
use aes::cipher::{
    BlockCipherDecrypt, BlockModeDecrypt, KeyInit, KeyIvInit, block_padding::NoPadding,
};
use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jiff::Timestamp;
use serde_json::{Value, json};
use url::Url;

use super::{
    Cipher, MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved,
    Resolver, SessionSupport, Tag, Variant, VariantKind, clean_title,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "mega";
const API: &str = "https://g.api.mega.co.nz/cs";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// A file, by its handle and the key the link carries.
    File { id: String, key: Vec<u8> },
    /// A folder, by its handle and key, and the node within it the link names.
    Folder {
        id: String,
        key: Vec<u8>,
        node: Option<String>,
        subfolder: Option<String>,
    },
}

fn decode_key(text: &str) -> Option<Vec<u8>> {
    let cleaned: String = text
        .trim()
        .trim_end_matches('=')
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .collect();
    let bytes = URL_SAFE_NO_PAD.decode(cleaned).ok()?;
    matches!(bytes.len(), 16 | 32).then_some(bytes)
}

fn is_handle(text: &str) -> bool {
    text.len() == 8
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if !matches!(
        host.as_str(),
        "mega.nz" | "www.mega.nz" | "mega.co.nz" | "www.mega.co.nz" | "mega.io"
    ) {
        return None;
    }
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    let fragment = url.fragment().unwrap_or_default();
    match segments.as_slice() {
        ["file" | "embed", id] if is_handle(id) => Some(Link::File {
            id: id.to_string(),
            key: decode_key(fragment).filter(|k| k.len() == 32)?,
        }),
        ["folder", id] if is_handle(id) => {
            // KEY, KEY/file/NODE or KEY/folder/SUB after the hash.
            let mut parts = fragment.split('/');
            let key = decode_key(parts.next()?).filter(|k| k.len() == 16)?;
            let (mut node, mut subfolder) = (None, None);
            match (parts.next(), parts.next()) {
                (Some("file"), Some(handle)) if is_handle(handle) => {
                    node = Some(handle.to_string())
                }
                (Some("folder"), Some(handle)) if is_handle(handle) => {
                    subfolder = Some(handle.to_string())
                }
                _ => {}
            }
            Some(Link::Folder {
                id: id.to_string(),
                key,
                node,
                subfolder,
            })
        }
        [] => {
            // The legacy shapes: #!ID!KEY for files, #F!ID!KEY for folders.
            if let Some(folder) = fragment.strip_prefix("F!") {
                let (id, key) = folder.split_once('!')?;
                let key = decode_key(key).filter(|k| k.len() == 16)?;
                return is_handle(id).then(|| Link::Folder {
                    id: id.to_string(),
                    key,
                    node: None,
                    subfolder: None,
                });
            }
            let (id, key) = fragment.strip_prefix('!')?.split_once('!')?;
            let key = decode_key(key).filter(|k| k.len() == 32)?;
            is_handle(id).then(|| Link::File {
                id: id.to_string(),
                key,
            })
        }
        _ => None,
    }
}

/// A node key as MEGA folds it: the 128-bit key that decrypts the data, and the 64-bit
/// nonce that starts the counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileKey {
    pub key: [u8; 16],
    pub nonce: [u8; 8],
}

pub fn fold_key(raw: &[u8]) -> Option<FileKey> {
    if raw.len() != 32 {
        return None;
    }
    let mut key = [0u8; 16];
    for (i, slot) in key.iter_mut().enumerate() {
        *slot = raw[i] ^ raw[i + 16];
    }
    let mut nonce = [0u8; 8];
    nonce.copy_from_slice(&raw[16..24]);
    Some(FileKey { key, nonce })
}

/// The attributes block decrypted: the JSON after the `MEGA` prefix.
pub fn decrypt_attributes(key: &[u8; 16], encoded: &str) -> Option<Value> {
    let data = URL_SAFE_NO_PAD.decode(encoded.trim_end_matches('=')).ok()?;
    if data.is_empty() || data.len() % 16 != 0 {
        return None;
    }
    let decryptor = cbc::Decryptor::<Aes128>::new_from_slices(key, &[0u8; 16]).ok()?;
    let plain = decryptor.decrypt_padded_vec::<NoPadding>(&data).ok()?;
    let text = String::from_utf8_lossy(&plain);
    let text = text.trim_end_matches('\0');
    serde_json::from_str(text.strip_prefix("MEGA")?).ok()
}

/// A node's key as the folder key encrypts it: AES-128 in ECB over each block.
pub fn decrypt_node_key(folder_key: &[u8], encoded: &str) -> Option<Vec<u8>> {
    let data = URL_SAFE_NO_PAD.decode(encoded.trim_end_matches('=')).ok()?;
    if data.is_empty() || data.len() % 16 != 0 {
        return None;
    }
    let cipher = Aes128::new_from_slice(folder_key).ok()?;
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks(16) {
        let mut block = aes::Block::default();
        block.copy_from_slice(chunk);
        cipher.decrypt_block(&mut block);
        out.extend_from_slice(&block);
    }
    Some(out)
}

fn api_error(code: i64, origin: &Url) -> ResolveError {
    match code {
        -9 => ResolveError::NotFound(origin.clone()),
        -3 | -4 | -6 => ResolveError::RateLimited(origin.clone()),
        -11 => ResolveError::unavailable(origin, "access to the file was denied"),
        -13 => ResolveError::unavailable(origin, "the file is still being uploaded"),
        -14 => ResolveError::unavailable(origin, "the key in the link does not fit the file"),
        -16 => ResolveError::unavailable(origin, "the file was taken down"),
        -17 => ResolveError::unavailable(origin, "the file's transfer quota is exceeded for now"),
        -18 => ResolveError::unavailable(origin, "the file is temporarily unavailable"),
        other => ResolveError::unavailable(origin, format!("the API answered with error {other}")),
    }
}

/// What a file is, by its name: MEGA knows nothing of a file beyond its name and size.
pub fn kind_of(name: &str) -> MediaKind {
    Container::from_name(name).map_or(MediaKind::File, |c| c.kind())
}

/// The codec an audio file's format implies.
fn audio_codec_of(container: Option<&Container>) -> Option<AudioCodec> {
    match container {
        Some(Container::Mp3) => Some(AudioCodec::Mp3),
        Some(Container::M4a) => Some(AudioCodec::Aac),
        Some(Container::Ogg) => Some(AudioCodec::Vorbis),
        Some(Container::Opus) => Some(AudioCodec::Opus),
        Some(Container::Flac) => Some(AudioCodec::Flac),
        _ => None,
    }
}

fn variant_for(url: Url, name: &str, size: Option<u64>, key: FileKey) -> Variant {
    let mut v = Variant::new(url, VariantKind::File);
    v.container = Container::from_name(name);
    match kind_of(name) {
        MediaKind::Video if v.container == Some(Container::Mp4) => {
            v.video = Some(VideoCodec::H264);
            v.audio = Some(AudioCodec::Aac);
        }
        MediaKind::Audio => {
            v.audio_only = true;
            v.audio = audio_codec_of(v.container.as_ref());
        }
        MediaKind::Video | MediaKind::Image | MediaKind::File => {}
    }
    v.size = size;
    v.cipher = Some(Cipher::Aes128Ctr {
        key: key.key,
        nonce: key.nonce,
    });
    v.format_id = Some("original".into());
    v
}

/// One node of a folder, its key and attributes decrypted.
#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub handle: String,
    pub parent: String,
    pub is_folder: bool,
    pub name: String,
    pub size: Option<u64>,
    pub key: Option<FileKey>,
    pub modified: Option<Timestamp>,
}

pub fn nodes_of(listing: &Value, folder_key: &[u8]) -> Vec<Node> {
    listing["f"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|node| {
            let handle = node["h"].as_str()?.to_string();
            let is_folder = node["t"].as_i64() == Some(1);
            let encoded = node["k"].as_str()?.split_once(':').map(|(_, k)| k)?;
            let raw = decrypt_node_key(folder_key, encoded)?;
            let (attr_key, key) = if is_folder {
                let mut k = [0u8; 16];
                k.copy_from_slice(raw.get(..16)?);
                (k, None)
            } else {
                let folded = fold_key(&raw)?;
                (folded.key, Some(folded))
            };
            let attrs = decrypt_attributes(&attr_key, node["a"].as_str()?)?;
            Some(Node {
                handle,
                parent: node["p"].as_str().unwrap_or_default().to_string(),
                is_folder,
                name: attrs["n"].as_str().unwrap_or_default().to_string(),
                size: node["s"].as_u64(),
                key,
                modified: node["ts"]
                    .as_i64()
                    .and_then(|t| Timestamp::from_second(t).ok()),
            })
        })
        .collect()
}

/// The files beneath `root` (every node when `None`), depth first in listing order.
pub fn files_under<'a>(nodes: &'a [Node], root: Option<&str>) -> Vec<&'a Node> {
    let mut out = Vec::new();
    let mut stack: Vec<&str> = match root {
        Some(root) => vec![root],
        None => nodes
            .iter()
            .filter(|n| n.is_folder && !nodes.iter().any(|p| p.handle == n.parent))
            .map(|n| n.handle.as_str())
            .collect(),
    };
    let mut seen = std::collections::HashSet::new();
    while let Some(parent) = stack.pop() {
        if !seen.insert(parent.to_string()) {
            continue;
        }
        let mut children: Vec<&Node> = nodes.iter().filter(|n| n.parent == parent).collect();
        children.reverse();
        for child in children {
            if child.is_folder {
                stack.push(&child.handle);
            } else {
                out.push(child);
            }
        }
    }
    if root.is_none() && out.is_empty() {
        out.extend(nodes.iter().filter(|n| !n.is_folder));
    }
    out
}

pub struct MegaResolver {
    http: Http,
    sequence: AtomicU64,
}

impl MegaResolver {
    pub fn new(http: Http) -> Self {
        Self {
            http,
            sequence: AtomicU64::new(0),
        }
    }

    /// One request to the API, in the folder's context when `folder` is given.
    async fn call(
        &self,
        request: Value,
        folder: Option<&str>,
        origin: &Url,
    ) -> Result<Value, ResolveError> {
        let mut api = Url::parse(API).expect("valid");
        api.query_pairs_mut().append_pair(
            "id",
            &self.sequence.fetch_add(1, Ordering::Relaxed).to_string(),
        );
        if let Some(folder) = folder {
            api.query_pairs_mut().append_pair("n", folder);
        }
        let response = self
            .http
            .post(api)
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .json(&json!([request]))
            .send()
            .await?;
        if response.status.as_u16() == 429 {
            return Err(ResolveError::RateLimited(origin.clone()));
        }
        if !response.is_success() {
            return Err(ResolveError::unavailable(
                origin,
                format!("the API answered HTTP {}", response.status),
            ));
        }
        let answer: Value = response
            .json(MAX_PAGE)
            .await
            .map_err(|e| ResolveError::malformed(origin, e.to_string()))?;
        let first = match &answer {
            Value::Array(items) => items.first().cloned().unwrap_or(Value::Null),
            other => other.clone(),
        };
        match first {
            Value::Number(code) => Err(api_error(code.as_i64().unwrap_or(-1), origin)),
            Value::Object(_) => Ok(first),
            _ => Err(ResolveError::malformed(
                origin,
                "the API answered with neither a record nor an error",
            )),
        }
    }

    async fn file(&self, id: &str, key: &[u8], origin: &Url) -> Result<Resolved, ResolveError> {
        let folded = fold_key(key)
            .ok_or_else(|| ResolveError::unavailable(origin, "the link carries no usable key"))?;
        let record = self
            .call(json!({"a": "g", "g": 1, "p": id}), None, origin)
            .await?;
        let attrs = record["at"]
            .as_str()
            .and_then(|at| decrypt_attributes(&folded.key, at))
            .ok_or_else(|| {
                ResolveError::unavailable(
                    origin,
                    "the key in the link does not decrypt the file's name",
                )
            })?;
        let name = attrs["n"].as_str().unwrap_or_default().to_string();
        let download = record["g"]
            .as_str()
            .and_then(|u| Url::parse(u).ok())
            .ok_or_else(|| ResolveError::unavailable(origin, "the API gave no download link"))?;
        let mut resolved = Resolved::of(PLATFORM, kind_of(&name));
        resolved.id = Some(id.to_string());
        resolved.title = clean_title(name.rsplit_once('.').map_or(name.as_str(), |(s, _)| s));
        resolved.webpage_url = Some(origin.clone());
        resolved.variants = vec![variant_for(download, &name, record["s"].as_u64(), folded)];
        Ok(resolved)
    }

    async fn folder(
        &self,
        id: &str,
        key: &[u8],
        node: Option<&str>,
        subfolder: Option<&str>,
        origin: &Url,
    ) -> Result<Resolution, ResolveError> {
        let listing = self
            .call(json!({"a": "f", "c": 1, "r": 1}), Some(id), origin)
            .await?;
        let nodes = nodes_of(&listing, key);
        if nodes.is_empty() {
            return Err(ResolveError::unavailable(
                origin,
                "the key in the link does not decrypt the folder",
            ));
        }
        if let Some(handle) = node {
            let file = nodes
                .iter()
                .find(|n| n.handle == handle && !n.is_folder)
                .ok_or_else(|| ResolveError::NotFound(origin.clone()))?;
            let folded = file.key.ok_or_else(|| {
                ResolveError::unavailable(origin, "the node's key does not decrypt")
            })?;
            let record = self
                .call(json!({"a": "g", "g": 1, "n": handle}), Some(id), origin)
                .await?;
            let download = record["g"]
                .as_str()
                .and_then(|u| Url::parse(u).ok())
                .ok_or_else(|| {
                    ResolveError::unavailable(origin, "the API gave no download link")
                })?;
            let mut resolved = Resolved::of(PLATFORM, kind_of(&file.name));
            resolved.id = Some(handle.to_string());
            resolved.title = clean_title(
                file.name
                    .rsplit_once('.')
                    .map_or(file.name.as_str(), |(s, _)| s),
            );
            resolved.uploaded_at = file.modified;
            resolved.webpage_url = Some(origin.clone());
            resolved.variants = vec![variant_for(
                download,
                &file.name,
                record["s"].as_u64().or(file.size),
                folded,
            )];
            return Ok(Resolution::from(resolved));
        }
        let root = subfolder.map(String::from).or_else(|| {
            nodes
                .iter()
                .find(|n| n.is_folder && !nodes.iter().any(|p| p.handle == n.parent))
                .map(|n| n.handle.clone())
        });
        let title = root
            .as_deref()
            .and_then(|r| nodes.iter().find(|n| n.handle == r))
            .and_then(|n| clean_title(&n.name));
        let key_text = URL_SAFE_NO_PAD.encode(key);
        let entries: Vec<PlaylistEntry> = files_under(&nodes, root.as_deref())
            .into_iter()
            .map(|n| PlaylistEntry {
                url: Url::parse(&format!(
                    "https://mega.nz/folder/{id}#{key_text}/file/{}",
                    n.handle
                ))
                .expect("valid"),
                title: clean_title(n.name.rsplit_once('.').map_or(n.name.as_str(), |(s, _)| s)),
                duration: None,
            })
            .collect();
        if entries.is_empty() {
            return Err(ResolveError::unavailable(
                origin,
                "the folder holds no files",
            ));
        }
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.into(),
            id: Some(subfolder.unwrap_or(id).to_string()),
            title,
            total: Some(entries.len()),
            entries,
        }))
    }
}

#[async_trait]
impl Resolver for MegaResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "MEGA",
            hosts: &["mega.nz", "mega.co.nz"],
            features: &[
                "files",
                "folders",
                "files within folders",
                "legacy links",
                "embeds",
                "audio",
                "images",
                "documents",
            ],
            formats: &[
                "mp4", "mkv", "webm", "mov", "mp3", "m4a", "flac", "jpg", "png", "pdf", "zip",
                "any file",
            ],
            media: &[
                MediaKind::Video,
                MediaKind::Audio,
                MediaKind::Image,
                MediaKind::File,
            ],
            tags: &[Tag::Files],
            session: SessionSupport::None,
            examples: &[
                "https://mega.nz/file/MR5F3QCI#Vj2aut_FqBVciXFJ9eck22uczDouiElMTBdwmGnk9-g",
                "https://mega.nz/file/IN4hGQwB#YrQ7z_bFiznfD8tB1RYut7V158j0SSKGriMM6fcJ6yw",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::File { id, key } => Ok(Resolution::from(self.file(&id, &key, url).await?)),
            Link::Folder {
                id,
                key,
                node,
                subfolder,
            } => {
                self.folder(&id, &key, node.as_deref(), subfolder.as_deref(), url)
                    .await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };

    fn post(url: &str, body: &str) -> Exchange {
        Exchange {
            request: RecordedRequest {
                method: "POST".into(),
                url: url.into(),
                headers: Vec::new(),
                body: None,
            },
            response: RecordedResponse {
                status: 200,
                url: url.into(),
                headers: vec![("content-type".into(), "application/json".into())],
                body: RecordedBody::Text(body.into()),
                truncated: false,
            },
        }
    }

    const FILE_LINK: &str =
        "https://mega.nz/file/MR5F3QCI#Vj2aut_FqBVciXFJ9eck22uczDouiElMTBdwmGnk9-g";
    const FILE_RECORD: &str = r#"[{"s":373612482,"at":"VeW4iFVfMp8-_iXmtyqIr_1rO1yXC17RN6v52jIyJuV4tdYJru1-HiiLgj8jCL8CYlPAj5xiZpFlILcMoRA85dGk9NNI9N42__HPC_923ks","msd":1,"fa":"251:8*0ZchrMcp-NQ","g":"http://gfs204n338.userstorage.mega.co.nz/dl/A2qfpogNUsOB_QcmljQ5cNABdK9HrKRhv8t9DwZN0z","ip":["1.2.3.4"],"fh":"x"}]"#;
    const FOLDER_LINK: &str = "https://mega.nz/folder/qQVUTAyZ#YJSPh-G_gZGDkg14ck-NLA";

    /// The folder above as the API listed it, one folder node holding two zip files, with
    /// a video node encrypted under the same folder key beside them.
    fn folder_listing() -> String {
        include_str!("mega_folder_fixture.json").to_string()
    }

    #[test]
    fn links_are_read_in_every_shape() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert!(
            matches!(link(FILE_LINK), Some(Link::File { id, key }) if id == "MR5F3QCI" && key.len() == 32)
        );
        assert!(
            matches!(link("https://mega.nz/#!MR5F3QCI!Vj2aut_FqBVciXFJ9eck22uczDouiElMTBdwmGnk9-g"), Some(Link::File { id, .. }) if id == "MR5F3QCI")
        );
        assert!(matches!(
            link("https://mega.nz/embed/MR5F3QCI#Vj2aut_FqBVciXFJ9eck22uczDouiElMTBdwmGnk9-g"),
            Some(Link::File { .. })
        ));
        assert!(
            matches!(link(FOLDER_LINK), Some(Link::Folder { id, key, node: None, subfolder: None }) if id == "qQVUTAyZ" && key.len() == 16)
        );
        assert!(
            matches!(link(&format!("{FOLDER_LINK}/file/XJtTFSAA")), Some(Link::Folder { node: Some(n), .. }) if n == "XJtTFSAA")
        );
        assert!(
            matches!(link(&format!("{FOLDER_LINK}/folder/OV0B1KIL")), Some(Link::Folder { subfolder: Some(s), .. }) if s == "OV0B1KIL")
        );
        assert!(matches!(
            link("https://mega.nz/#F!qQVUTAyZ!YJSPh-G_gZGDkg14ck-NLA"),
            Some(Link::Folder { .. })
        ));
        assert_eq!(link("https://mega.nz/file/MR5F3QCI"), None);
        assert_eq!(link("https://mega.nz/file/MR5F3QCI#tooshort"), None);
        assert_eq!(link("https://mega.nz/pro"), None);
    }

    #[test]
    fn keys_fold_and_attributes_decrypt() {
        let raw = decode_key("Vj2aut_FqBVciXFJ9eck22uczDouiElMTBdwmGnk9-g").unwrap();
        let folded = fold_key(&raw).unwrap();
        let attrs = decrypt_attributes(&folded.key, "VeW4iFVfMp8-_iXmtyqIr_1rO1yXC17RN6v52jIyJuV4tdYJru1-HiiLgj8jCL8CYlPAj5xiZpFlILcMoRA85dGk9NNI9N42__HPC_923ks").unwrap();
        assert_eq!(attrs["n"], "All To Ourselves Deluxe 1080p.mp4");
        assert!(
            decrypt_attributes(&[0u8; 16], "VeW4iFVfMp8-_iXmtyqIr_1rO1yXC17RN6v52jIyJuU").is_none()
        );
    }

    #[tokio::test]
    async fn files_resolve_with_their_cipher() {
        let mut fixture = Fixture::new("mega", None);
        fixture
            .exchanges
            .push(post("https://g.api.mega.co.nz/cs?id=0", FILE_RECORD));
        fixture
            .exchanges
            .push(post("https://g.api.mega.co.nz/cs?id=1", "[-9]"));
        fixture
            .exchanges
            .push(post("https://g.api.mega.co.nz/cs?id=2", "[-17]"));
        let resolver = MegaResolver::new(Http::replay(fixture));
        let url = Url::parse(FILE_LINK).unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(
            resolved.title.as_deref(),
            Some("All To Ourselves Deluxe 1080p")
        );
        assert_eq!(resolved.variants.len(), 1);
        let v = &resolved.variants[0];
        assert!(
            v.url
                .as_str()
                .starts_with("http://gfs204n338.userstorage.mega.co.nz/dl/")
        );
        assert_eq!(v.size, Some(373612482));
        assert_eq!(v.container, Some(Container::Mp4));
        let Some(Cipher::Aes128Ctr { key, nonce }) = &v.cipher else {
            panic!("no cipher");
        };
        let expected =
            fold_key(&decode_key("Vj2aut_FqBVciXFJ9eck22uczDouiElMTBdwmGnk9-g").unwrap()).unwrap();
        assert_eq!(key, &expected.key);
        assert_eq!(nonce, &expected.nonce);
        assert_eq!(hex::encode(key), "3da15680f14de159109e01d19c03d333");
        assert_eq!(hex::encode(nonce), "6b9ccc3a2e88494c");
        assert!(matches!(
            resolver
                .resolve(
                    &Url::parse(
                        "https://mega.nz/file/gone1234#Vj2aut_FqBVciXFJ9eck22uczDouiElMTBdwmGnk9-g"
                    )
                    .unwrap()
                )
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let error = resolver
            .resolve(
                &Url::parse(
                    "https://mega.nz/file/quota123#Vj2aut_FqBVciXFJ9eck22uczDouiElMTBdwmGnk9-g",
                )
                .unwrap(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("quota")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn folders_list_their_files_and_entries_resolve_within_them() {
        let mut fixture = Fixture::new("mega", None);
        fixture.exchanges.push(post(
            "https://g.api.mega.co.nz/cs?id=0&n=qQVUTAyZ",
            &folder_listing(),
        ));
        fixture.exchanges.push(post(
            "https://g.api.mega.co.nz/cs?id=1&n=qQVUTAyZ",
            &folder_listing(),
        ));
        fixture.exchanges.push(post(
            "https://g.api.mega.co.nz/cs?id=2&n=qQVUTAyZ",
            r#"[{"s":39859166,"g":"http://gfs.userstorage.mega.co.nz/dl/token","at":"x"}]"#,
        ));
        fixture.exchanges.push(post(
            "https://g.api.mega.co.nz/cs?id=3&n=qQVUTAyZ",
            &folder_listing(),
        ));
        fixture.exchanges.push(post(
            "https://g.api.mega.co.nz/cs?id=4&n=qQVUTAyZ",
            r#"[{"s":1234,"g":"http://gfs.userstorage.mega.co.nz/dl/ziptoken","at":"x"}]"#,
        ));
        let resolver = MegaResolver::new(Http::replay(fixture));
        let playlist = match resolver
            .resolve(&Url::parse(FOLDER_LINK).unwrap())
            .await
            .unwrap()
        {
            Resolution::Playlist(p) => p,
            other => panic!("expected a playlist, got {other:?}"),
        };
        assert_eq!(playlist.title.as_deref(), Some("Eldritch_Dimensions"));
        // The video and the two zip files beside it.
        assert_eq!(playlist.entries.len(), 3);
        let entry = playlist
            .entries
            .iter()
            .find(|e| e.title.as_deref() == Some("trailer"))
            .unwrap();
        assert!(
            entry
                .url
                .as_str()
                .starts_with("https://mega.nz/folder/qQVUTAyZ#YJSPh-G_gZGDkg14ck-NLA/file/")
        );
        let resolved = resolver.resolve(&entry.url).await.unwrap().media().unwrap();
        assert_eq!(resolved.title.as_deref(), Some("trailer"));
        assert_eq!(resolved.media, MediaKind::Video);
        assert_eq!(resolved.variants[0].size, Some(39859166));
        assert!(resolved.variants[0].cipher.is_some());
        assert!(resolved.uploaded_at.is_some());
        let zip = playlist
            .entries
            .iter()
            .find(|e| e.title.as_deref() != Some("trailer"))
            .unwrap();
        let resolved = resolver.resolve(&zip.url).await.unwrap().media().unwrap();
        assert_eq!(resolved.media, MediaKind::File);
        assert_eq!(
            resolved.variants[0].container,
            Some(Container::Other("zip".into()))
        );
        assert_eq!(resolved.variants[0].size, Some(1234));
        assert!(resolved.variants[0].cipher.is_some());
        assert!(!resolved.variants[0].audio_only);
    }

    #[tokio::test]
    async fn images_and_audio_files_resolve_as_what_their_names_say() {
        // The file record of the video above, its attributes re-encrypted under the same
        // key for a photo and for a song, so the names decrypt to those.
        let raw = decode_key("Vj2aut_FqBVciXFJ9eck22uczDouiElMTBdwmGnk9-g").unwrap();
        let folded = fold_key(&raw).unwrap();
        let attributes = |name: &str| {
            use aes::cipher::BlockModeEncrypt;
            let mut plain = format!("MEGA{{\"n\":\"{name}\"}}").into_bytes();
            while plain.len() % 16 != 0 {
                plain.push(0);
            }
            let encryptor =
                cbc::Encryptor::<Aes128>::new_from_slices(&folded.key, &[0u8; 16]).unwrap();
            URL_SAFE_NO_PAD.encode(encryptor.encrypt_padded_vec::<NoPadding>(&plain))
        };
        let record = |name: &str, size: u64| {
            format!(
                r#"[{{"s":{size},"at":"{}","g":"http://gfs.userstorage.mega.co.nz/dl/{size}"}}]"#,
                attributes(name)
            )
        };
        let mut fixture = Fixture::new("mega", None);
        fixture.exchanges.push(post(
            "https://g.api.mega.co.nz/cs?id=0",
            &record("Holiday photo.JPG", 3_500_000),
        ));
        fixture.exchanges.push(post(
            "https://g.api.mega.co.nz/cs?id=1",
            &record("Demo take 3.flac", 25_000_000),
        ));
        let resolver = MegaResolver::new(Http::replay(fixture));
        let image = resolver
            .resolve(&Url::parse(FILE_LINK).unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(image.media, MediaKind::Image);
        assert_eq!(image.title.as_deref(), Some("Holiday photo"));
        assert_eq!(image.variants[0].container, Some(Container::Jpeg));
        assert_eq!(image.variants[0].size, Some(3_500_000));
        assert!(image.variants[0].cipher.is_some());
        let audio = resolver
            .resolve(&Url::parse(FILE_LINK).unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(audio.media, MediaKind::Audio);
        assert_eq!(audio.title.as_deref(), Some("Demo take 3"));
        let v = &audio.variants[0];
        assert!(v.audio_only);
        assert_eq!(v.container, Some(Container::Flac));
        assert_eq!(v.audio, Some(AudioCodec::Flac));
        assert_eq!(v.size, Some(25_000_000));
    }
}
