//! Fetches statically linked FFmpeg and FFprobe builds for the compile target and embeds
//! them, zstd-compressed, into the crate so the final binary carries its own media tools.

use std::collections::HashMap;
use std::env;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

const ZSTD_LEVEL: i32 = 12;
const TOOLS: [&str; 2] = ["ffmpeg", "ffprobe"];

#[derive(Clone, Copy)]
enum Archive {
    TarXz,
    Zip,
}

impl Archive {
    fn extension(self) -> &'static str {
        match self {
            Archive::TarXz => "tar.xz",
            Archive::Zip => "zip",
        }
    }
}

struct Download {
    url: &'static str,
    archive: Archive,
}

struct Source {
    version: &'static str,
    exe_suffix: &'static str,
    downloads: Vec<Download>,
}

fn source_for(target: &str) -> Option<Source> {
    let jvs = |url: &'static str| Source {
        version: "7.0.2",
        exe_suffix: "",
        downloads: vec![Download {
            url,
            archive: Archive::TarXz,
        }],
    };
    let mac = |ffmpeg: &'static str, ffprobe: &'static str| Source {
        version: "release",
        exe_suffix: "",
        downloads: vec![
            Download {
                url: ffmpeg,
                archive: Archive::Zip,
            },
            Download {
                url: ffprobe,
                archive: Archive::Zip,
            },
        ],
    };
    let mut parts = target.splitn(4, '-');
    let arch = parts.next().unwrap_or("");
    let _vendor = parts.next().unwrap_or("");
    let os = parts.next().unwrap_or("");
    let env_abi = parts.next().unwrap_or("");
    match (arch, os, env_abi) {
        ("x86_64", "linux", _) => Some(jvs(
            "https://johnvansickle.com/ffmpeg/releases/ffmpeg-7.0.2-amd64-static.tar.xz",
        )),
        ("aarch64", "linux", _) => Some(jvs(
            "https://johnvansickle.com/ffmpeg/releases/ffmpeg-7.0.2-arm64-static.tar.xz",
        )),
        ("armv7" | "arm", "linux", abi) if abi.ends_with("hf") => Some(jvs(
            "https://johnvansickle.com/ffmpeg/releases/ffmpeg-7.0.2-armhf-static.tar.xz",
        )),
        ("i686" | "i586", "linux", _) => Some(jvs(
            "https://johnvansickle.com/ffmpeg/releases/ffmpeg-7.0.2-i686-static.tar.xz",
        )),
        ("x86_64", "windows", _) => Some(Source {
            version: "8.1",
            exe_suffix: ".exe",
            downloads: vec![Download {
                url: "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-n8.1-latest-win64-gpl-8.1.zip",
                archive: Archive::Zip,
            }],
        }),
        ("x86_64", "darwin", _) => Some(mac(
            "https://ffmpeg.martin-riedl.de/redirect/latest/macos/amd64/release/ffmpeg.zip",
            "https://ffmpeg.martin-riedl.de/redirect/latest/macos/amd64/release/ffprobe.zip",
        )),
        ("aarch64", "darwin", _) => Some(mac(
            "https://ffmpeg.martin-riedl.de/redirect/latest/macos/arm64/release/ffmpeg.zip",
            "https://ffmpeg.martin-riedl.de/redirect/latest/macos/arm64/release/ffprobe.zip",
        )),
        _ => None,
    }
}

fn download(url: &str, dest: &Path) -> io::Result<()> {
    let tmp = dest.with_extension("part");
    let mut response = ureq::get(url)
        .header("User-Agent", "discoclip-build")
        .call()
        .map_err(|e| io::Error::other(format!("GET {url}: {e}")))?;
    let mut reader = response.body_mut().as_reader();
    let mut file = BufWriter::new(File::create(&tmp)?);
    io::copy(&mut reader, &mut file)?;
    file.flush()?;
    drop(file);
    fs::rename(&tmp, dest)
}

fn base_name(name: &str) -> &str {
    name.rsplit(['/', '\\']).next().unwrap_or(name)
}

/// Pulls every executable named in `wanted` out of the archive into `found`.
fn extract(
    archive: Archive,
    path: &Path,
    wanted: &[String],
    found: &mut HashMap<String, Vec<u8>>,
) -> io::Result<()> {
    let is_wanted = |name: &str, found: &HashMap<String, Vec<u8>>| -> Option<String> {
        let base = base_name(name);
        wanted
            .iter()
            .find(|w| *w == base && !found.contains_key(base))
            .cloned()
    };
    match archive {
        Archive::TarXz => {
            let file = BufReader::new(File::open(path)?);
            let decoder = liblzma::read::XzDecoder::new(file);
            let mut tar = tar::Archive::new(decoder);
            for entry in tar.entries()? {
                let mut entry = entry?;
                let name = entry.path()?.to_string_lossy().into_owned();
                if entry.header().entry_type().is_file()
                    && let Some(exe) = is_wanted(&name, found)
                {
                    let mut buf = Vec::with_capacity(entry.size() as usize);
                    entry.read_to_end(&mut buf)?;
                    found.insert(exe, buf);
                }
            }
        }
        Archive::Zip => {
            let file = BufReader::new(File::open(path)?);
            let mut zip = zip::ZipArchive::new(file)
                .map_err(|e| io::Error::other(format!("open zip: {e}")))?;
            for i in 0..zip.len() {
                let mut entry = zip
                    .by_index(i)
                    .map_err(|e| io::Error::other(format!("zip entry {i}: {e}")))?;
                if entry.is_file()
                    && let Some(exe) = is_wanted(entry.name(), found)
                {
                    let mut buf = Vec::with_capacity(entry.size() as usize);
                    entry.read_to_end(&mut buf)?;
                    found.insert(exe, buf);
                }
            }
        }
    }
    Ok(())
}

fn compress(raw: &[u8], dest: &Path) -> io::Result<()> {
    let tmp = dest.with_extension("part");
    let file = BufWriter::new(File::create(&tmp)?);
    let mut encoder = zstd::Encoder::new(file, ZSTD_LEVEL)?;
    encoder.include_checksum(true)?;
    encoder.write_all(raw)?;
    let mut file = encoder.finish()?;
    file.flush()?;
    drop(file);
    fs::rename(&tmp, dest)
}

fn digest_of(paths: &[PathBuf]) -> io::Result<String> {
    let mut hasher = Sha256::new();
    for path in paths {
        hasher.update(fs::read(path)?);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let target = env::var("TARGET").expect("cargo sets TARGET");
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
    let Some(source) = source_for(&target) else {
        panic!("no portable ffmpeg build is available for target {target}");
    };
    let exes: Vec<String> = TOOLS
        .iter()
        .map(|tool| format!("{tool}{}", source.exe_suffix))
        .collect();
    let payloads: Vec<PathBuf> = TOOLS
        .iter()
        .map(|tool| out_dir.join(format!("{tool}.zst")))
        .collect();
    let urls: Vec<&str> = source.downloads.iter().map(|d| d.url).collect();
    let stamp_value = urls.join("\n");
    let stamp = out_dir.join("tools.source");
    let fresh = fs::read_to_string(&stamp).is_ok_and(|s| s == stamp_value)
        && payloads.iter().all(|p| p.exists());

    if !fresh {
        let mut found = HashMap::new();
        for (index, item) in source.downloads.iter().enumerate() {
            let archive_path = out_dir.join(format!("tools-{index}.{}", item.archive.extension()));
            download(item.url, &archive_path)
                .unwrap_or_else(|e| panic!("downloading {}: {e}", item.url));
            extract(item.archive, &archive_path, &exes, &mut found)
                .unwrap_or_else(|e| panic!("extracting {}: {e}", archive_path.display()));
            let _ = fs::remove_file(&archive_path);
        }
        for (exe, payload) in exes.iter().zip(&payloads) {
            let raw = found
                .remove(exe)
                .unwrap_or_else(|| panic!("{exe} not present in any downloaded archive"));
            compress(&raw, payload).unwrap_or_else(|e| panic!("compressing {exe}: {e}"));
        }
        fs::write(&stamp, &stamp_value).expect("write source stamp");
    }

    let digest = digest_of(&payloads).expect("hash tool payloads");
    let embed = format!(
        "pub const TOOLS_VERSION: &str = {version:?};\n\
         pub const TOOLS_SOURCE: &str = {source:?};\n\
         pub const TOOLS_DIGEST: &str = {digest:?};\n\
         pub const FFMPEG_EXE: &str = {ffmpeg_exe:?};\n\
         pub const FFPROBE_EXE: &str = {ffprobe_exe:?};\n\
         pub static FFMPEG_ZST: &[u8] = include_bytes!({ffmpeg_zst:?});\n\
         pub static FFPROBE_ZST: &[u8] = include_bytes!({ffprobe_zst:?});\n",
        version = source.version,
        source = stamp_value,
        ffmpeg_exe = exes[0],
        ffprobe_exe = exes[1],
        ffmpeg_zst = payloads[0].display().to_string(),
        ffprobe_zst = payloads[1].display().to_string(),
    );
    fs::write(out_dir.join("ffmpeg_embed.rs"), embed).expect("write ffmpeg_embed.rs");
}
