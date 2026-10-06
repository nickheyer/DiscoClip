//! Builds the SvelteKit app in `ui/` and embeds every file it produces, so the binary
//! serves the web app from memory with nothing to deploy beside it.

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use flate2::Compression;
use flate2::write::GzEncoder;
use sha2::{Digest, Sha256};

/// What Cargo watches for changes. Anything else under `ui/` is derived from these.
const WATCHED: [&str; 6] = [
    "src",
    "static",
    "package.json",
    "package-lock.json",
    "vite.config.ts",
    "tsconfig.json",
];

struct Asset {
    path: String,
    file: PathBuf,
    content_type: &'static str,
    etag: String,
    immutable: bool,
    /// The gzipped copy, written when text came out smaller that way
    gzip: Option<PathBuf>,
    etag_gzip: String,
}

/// Whether a content type is text worth gzipping, as fonts and pictures are compressed already
fn compressible(content_type: &str) -> bool {
    content_type.starts_with("text/")
        || content_type.starts_with("application/json")
        || content_type.starts_with("application/manifest+json")
        || content_type.starts_with("application/xml")
        || content_type == "image/svg+xml"
}

/// The bytes gzipped as small as the library makes them
fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
    encoder
        .write_all(bytes)
        .expect("writing into memory does not fail");
    encoder
        .finish()
        .expect("finishing an in-memory gzip stream does not fail")
}

fn npm() -> &'static str {
    if cfg!(windows) { "npm.cmd" } else { "npm" }
}

fn run(ui: &Path, args: &[&str], env: &[(&str, &Path)]) {
    let mut command = Command::new(npm());
    command.current_dir(ui).args(args);
    for (key, value) in env {
        command.env(key, value);
    }
    let status = command.status().unwrap_or_else(|e| {
        panic!(
            "could not run `{} {}` in {}: {e}",
            npm(),
            args.join(" "),
            ui.display()
        )
    });
    if !status.success() {
        panic!(
            "`{} {}` failed in {}: {status}",
            npm(),
            args.join(" "),
            ui.display()
        );
    }
}

/// Whether `node_modules` is missing or older than the lock file that describes it.
fn needs_install(ui: &Path) -> bool {
    let lock = ui.join("package-lock.json");
    let installed = ui.join("node_modules").join(".package-lock.json");
    let Ok(installed) = fs::metadata(&installed).and_then(|m| m.modified()) else {
        return true;
    };
    let Ok(lock) = fs::metadata(&lock).and_then(|m| m.modified()) else {
        return true;
    };
    lock > installed
}

fn content_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json; charset=utf-8",
        "webmanifest" => "application/manifest+json; charset=utf-8",
        "txt" => "text/plain; charset=utf-8",
        "xml" => "application/xml; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

/// Collects every file under `dir`, writing a gzipped copy of compressible text into `gz_root`
fn walk(root: &Path, gz_root: &Path, dir: &Path, assets: &mut Vec<Asset>) {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        .map(|entry| entry.expect("directory entry").path())
        .collect();
    entries.sort();
    for entry in entries {
        if entry.is_dir() {
            walk(root, gz_root, &entry, assets);
            continue;
        }
        let under_root = entry.strip_prefix(root).expect("under the build root");
        let relative = under_root
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        let bytes = fs::read(&entry).unwrap_or_else(|e| panic!("reading {}: {e}", entry.display()));
        let digest = Sha256::digest(&bytes);
        let content_type = content_type(&entry);
        let mut gzipped = None;
        if compressible(content_type) {
            let compressed = gzip(&bytes);
            if compressed.len() < bytes.len() {
                let mut target = gz_root.join(under_root).into_os_string();
                target.push(".gz");
                let target = PathBuf::from(target);
                let parent = target.parent().expect("a gzip target has a directory");
                fs::create_dir_all(parent)
                    .unwrap_or_else(|e| panic!("creating {}: {e}", parent.display()));
                fs::write(&target, &compressed)
                    .unwrap_or_else(|e| panic!("writing {}: {e}", target.display()));
                gzipped = Some(target);
            }
        }
        assets.push(Asset {
            content_type,
            etag: format!("\"{}\"", hex(&digest[..16])),
            etag_gzip: format!("\"{}-gz\"", hex(&digest[..16])),
            immutable: relative.starts_with("_app/immutable/"),
            path: relative,
            file: entry,
            gzip: gzipped,
        });
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut word = [0u8; 3];
        word[..chunk.len()].copy_from_slice(chunk);
        let n = u32::from_be_bytes([0, word[0], word[1], word[2]]);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// The CSP hashes of the inline scripts in the app's page: the theme bootstrap in
/// `app.html` and the start script SvelteKit adds.
fn inline_script_hashes(html: &str) -> Vec<String> {
    let mut hashes = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find("<script") {
        let after_tag = &rest[start..];
        let Some(open_end) = after_tag.find('>') else {
            break;
        };
        let attributes = &after_tag[..open_end];
        let body_start = open_end + 1;
        let Some(close) = after_tag[body_start..].find("</script>") else {
            break;
        };
        let body = &after_tag[body_start..body_start + close];
        if !attributes.contains("src=") && !body.trim().is_empty() {
            hashes.push(format!(
                "sha256-{}",
                base64(&Sha256::digest(body.as_bytes()))
            ));
        }
        rest = &after_tag[body_start + close + "</script>".len()..];
    }
    hashes
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let manifest =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
    let ui = manifest.join("ui");
    for watched in WATCHED {
        println!("cargo:rerun-if-changed={}", ui.join(watched).display());
    }

    if needs_install(&ui) {
        run(
            &ui,
            &["ci", "--no-audit", "--no-fund", "--loglevel=error"],
            &[],
        );
    }
    let dist = out_dir.join("ui");
    if dist.exists() {
        fs::remove_dir_all(&dist).unwrap_or_else(|e| panic!("clearing {}: {e}", dist.display()));
    }
    run(
        &ui,
        &["run", "build", "--silent"],
        &[("DISCOCLIP_UI_OUT", &dist)],
    );

    let gz = out_dir.join("ui-gz");
    if gz.exists() {
        fs::remove_dir_all(&gz).unwrap_or_else(|e| panic!("clearing {}: {e}", gz.display()));
    }
    let mut assets = Vec::new();
    walk(&dist, &gz, &dist, &mut assets);
    let index = assets
        .iter()
        .find(|asset| asset.path == "index.html")
        .unwrap_or_else(|| panic!("the web app build in {} has no index.html", dist.display()));
    let html = fs::read_to_string(&index.file).expect("read index.html");
    let hashes = inline_script_hashes(&html);

    let mut code = String::new();
    code.push_str("pub static ASSETS: &[Asset] = &[\n");
    for asset in &assets {
        let gzip = match &asset.gzip {
            Some(file) => format!("Some(include_bytes!({:?}))", file.display().to_string()),
            None => "None".to_string(),
        };
        code.push_str(&format!(
            "    Asset {{ path: {path:?}, content_type: {content_type:?}, etag: {etag:?}, immutable: {immutable}, bytes: include_bytes!({file:?}), gzip: {gzip}, etag_gzip: {etag_gzip:?} }},\n",
            path = asset.path,
            content_type = asset.content_type,
            etag = asset.etag,
            immutable = asset.immutable,
            file = asset.file.display().to_string(),
            etag_gzip = asset.etag_gzip,
        ));
    }
    code.push_str("];\n");
    code.push_str("pub static INLINE_SCRIPT_HASHES: &[&str] = &[\n");
    for hash in &hashes {
        code.push_str(&format!("    {hash:?},\n"));
    }
    code.push_str("];\n");
    fs::write(out_dir.join("ui_embed.rs"), code).expect("write ui_embed.rs");
}
