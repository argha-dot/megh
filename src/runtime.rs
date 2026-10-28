//! Wine runtimes: which ones exist (runtimes.yml) and installing them.
//! - The built-in list ships in the binary.
//! - Users can add or override entries in <data_dir>/runtimes.yml.
//! - Installed runtimes live in <data_dir>/runtimes/<id>/.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::data_dir;

const BUILTIN: &str = include_str!("../runtimes.yml");

#[derive(Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Arch {
    I386,
    X86_64,
}

impl Arch {
    fn name(self) -> &'static str {
        match self {
            Arch::I386 => "i386",
            Arch::X86_64 => "x86_64",
        }
    }
}

#[derive(Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Tested,
    Untested,
    Broken,
}

impl Status {
    fn name(self) -> &'static str {
        match self {
            Status::Tested => "tested",
            Status::Untested => "untested",
            Status::Broken => "broken",
        }
    }
}

/// One entry in runtimes.yml.
#[derive(Deserialize)]
pub struct Source {
    pub id: String,
    pub name: String,
    pub url: String,
    pub sha256: Option<String>,
    /// Path to the wine binary inside the extracted archive.
    pub wine: PathBuf,
    /// Exe types this runtime is auto-picked for.
    pub arch: Vec<Arch>,
    pub status: Status,
    #[serde(default)]
    pub notes: String,
}

#[derive(Deserialize)]
struct Config {
    runtimes: Vec<Source>,
}

/// An installed runtime.
pub struct Runtime {
    pub id: String,
    wine: PathBuf,
}

impl Runtime {
    pub fn wine(&self) -> PathBuf {
        self.wine.clone()
    }
}

fn user_config() -> Result<PathBuf> {
    Ok(data_dir()?.join("runtimes.yml"))
}

fn install_dir(id: &str) -> Result<PathBuf> {
    Ok(data_dir()?.join("runtimes").join(id))
}

/// Built-in sources, with the user's runtimes.yml merged on top by id.
fn sources() -> Result<Vec<Source>> {
    let mut all = serde_saphyr::from_str::<Config>(BUILTIN)?.runtimes;
    let path = user_config()?;
    if path.exists() {
        let text = fs::read_to_string(&path)?;
        let user: Config =
            serde_saphyr::from_str(&text).with_context(|| format!("bad {}", path.display()))?;
        for src in user.runtimes {
            match all.iter_mut().find(|s| s.id == src.id) {
                Some(existing) => *existing = src,
                None => all.push(src),
            }
        }
    }
    Ok(all)
}

/// The runtime for an exe: `id` if given, else auto-picked by bitness.
/// Installs it (after asking) if needed.
pub fn resolve(id: Option<&str>, exe: &Path) -> Result<Runtime> {
    let sources = sources()?;
    let src = match id {
        Some(id) => sources
            .iter()
            .find(|s| s.id == id)
            .with_context(|| format!("unknown runtime '{id}' (see: megh runtimes)"))?,
        None => {
            let arch = exe_arch(exe)?;
            let fits = |s: &&Source| s.arch.contains(&arch);
            sources
                .iter()
                .filter(fits)
                .find(|s| s.status == Status::Tested)
                .or_else(|| sources.iter().filter(fits).find(|s| s.status != Status::Broken))
                .with_context(|| format!("no runtime for {} exes (see: megh runtimes)", arch.name()))?
        }
    };
    ensure(src)
}

/// `megh runtimes`
pub fn list() -> Result<()> {
    for src in sources()? {
        let installed = install_dir(&src.id)?.join(&src.wine).is_file();
        let arch: Vec<_> = src.arch.iter().map(|a| a.name()).collect();
        println!(
            "{}  [{}] for {}{}",
            src.id,
            src.status.name(),
            arch.join(", "),
            if installed { "  (installed)" } else { "" }
        );
        println!("    {}", src.name);
        if !src.notes.is_empty() {
            println!("    {}", src.notes);
        }
    }
    println!("\nAdd or override runtimes in {}", user_config()?.display());
    Ok(())
}

pub fn check_rosetta() -> Result<()> {
    let ok = Command::new("/usr/bin/arch")
        .args(["-x86_64", "/usr/bin/true"])
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        bail!("Rosetta 2 is not installed. Run: softwareupdate --install-rosetta");
    }
    Ok(())
}

/// Returns the installed runtime, asking for a path or download if missing.
fn ensure(src: &Source) -> Result<Runtime> {
    let dir = install_dir(&src.id)?;
    let rt = Runtime { id: src.id.clone(), wine: dir.join(&src.wine) };
    if rt.wine.is_file() {
        return Ok(rt);
    }

    println!("Runtime '{}' ({}) is not installed.", src.id, src.name);
    loop {
        println!("Enter a path to its archive or an extracted folder (drag & drop works),");
        print!("or type 'download' to fetch it from {}: ", src.url);
        io::stdout().flush()?;

        let mut line = String::new();
        if io::stdin().read_line(&mut line)? == 0 {
            bail!("runtime '{}' is not installed", src.id);
        }
        let input = line.trim();
        if input.is_empty() {
            continue;
        }

        // Clear a broken earlier attempt. Doesn't follow symlinks.
        if dir.symlink_metadata().is_ok() {
            fs::remove_dir_all(&dir)?;
        }
        let result = if input == "download" {
            download(src, &dir)
        } else {
            add_local(src, &clean_path(input), &dir)
        };
        match result {
            Ok(()) if rt.wine.is_file() => return Ok(rt),
            Ok(()) => {
                fs::remove_dir_all(&dir)?;
                eprintln!("Error: no {} inside it\n", src.wine.display());
            }
            Err(e) => eprintln!("Error: {e:#}\n"),
        }
    }
}

/// Undoes shell quoting from drag & drop, expands ~.
fn clean_path(input: &str) -> PathBuf {
    let s = input.trim_matches(|c| c == '\'' || c == '"').replace("\\ ", " ");
    match s.strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(rest),
        None => PathBuf::from(s),
    }
}

/// An archive gets extracted; an extracted folder gets symlinked.
fn add_local(src: &Source, path: &Path, dir: &Path) -> Result<()> {
    let path = path
        .canonicalize()
        .with_context(|| format!("{} not found", path.display()))?;
    if path.is_file() {
        return extract(src, &path, dir);
    }
    if !path.join(&src.wine).is_file() {
        bail!("no {} in {}", src.wine.display(), path.display());
    }
    fs::create_dir_all(dir.parent().unwrap())?;
    std::os::unix::fs::symlink(&path, dir)?;
    Ok(())
}

fn download(src: &Source, dir: &Path) -> Result<()> {
    let archive = data_dir()?.join("downloads").join(&src.id);
    fs::create_dir_all(archive.parent().unwrap())?;
    let status = Command::new("curl")
        .args(["-L", "--fail", "-o"])
        .arg(&archive)
        .arg(&src.url)
        .status()?;
    if !status.success() {
        bail!("download failed");
    }
    let result = extract(src, &archive, dir);
    fs::remove_file(&archive)?;
    result
}

/// Verifies the sha256 (if the config has one) and unpacks into `dir`.
fn extract(src: &Source, archive: &Path, dir: &Path) -> Result<()> {
    match &src.sha256 {
        Some(want) => {
            let out = Command::new("shasum").args(["-a", "256"]).arg(archive).output()?;
            if !String::from_utf8_lossy(&out.stdout).starts_with(want.as_str()) {
                bail!("checksum mismatch: {} is not '{}'", archive.display(), src.id);
            }
        }
        None => eprintln!("Warning: no sha256 for '{}' in config; not verified", src.id),
    }

    fs::create_dir_all(dir)?;
    println!("Extracting {} ...", archive.display());
    // tar detects the compression itself.
    let status = Command::new("tar").arg("-xf").arg(archive).arg("-C").arg(dir).status()?;
    if !status.success() {
        fs::remove_dir_all(dir)?;
        bail!("failed to extract {}", archive.display());
    }
    Ok(())
}

/// Bitness of a Windows exe, from its PE header.
pub fn exe_arch(exe: &Path) -> Result<Arch> {
    let mut head = Vec::new();
    File::open(exe)
        .with_context(|| format!("{} not found", exe.display()))?
        .take(4096)
        .read_to_end(&mut head)?;
    let not_exe = || format!("{} is not a Windows exe", exe.display());
    let pe = head
        .get(0x3c..0x40)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
        .with_context(not_exe)?;
    match head.get(pe..pe + 6) {
        Some(&[b'P', b'E', 0, 0, lo, hi]) => match u16::from_le_bytes([lo, hi]) {
            0x014c => Ok(Arch::I386),
            0x8664 => Ok(Arch::X86_64),
            other => bail!("unsupported exe machine type {other:#x}"),
        },
        _ => bail!(not_exe()),
    }
}
