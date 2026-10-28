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
    let mut all = parse(BUILTIN)?;
    let path = user_config()?;
    if path.exists() {
        let text = fs::read_to_string(&path)?;
        let user = parse(&text).with_context(|| format!("bad {}", path.display()))?;
        merge(&mut all, user);
    }
    Ok(all)
}

fn parse(yaml: &str) -> Result<Vec<Source>> {
    Ok(serde_saphyr::from_str::<Config>(yaml)?.runtimes)
}

/// Same id replaces, new ids are appended.
fn merge(all: &mut Vec<Source>, user: Vec<Source>) {
    for src in user {
        match all.iter_mut().find(|s| s.id == src.id) {
            Some(existing) => *existing = src,
            None => all.push(src),
        }
    }
}

/// First tested runtime for `arch`, else first that isn't broken.
fn pick(sources: &[Source], arch: Arch) -> Option<&Source> {
    let fits = |s: &&Source| s.arch.contains(&arch);
    sources
        .iter()
        .filter(fits)
        .find(|s| s.status == Status::Tested)
        .or_else(|| sources.iter().filter(fits).find(|s| s.status != Status::Broken))
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
            pick(&sources, arch)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal PE header: "MZ", e_lfanew at 0x3c, then "PE\0\0" + machine.
    fn fake_exe(dir: &Path, machine: u16) -> PathBuf {
        let mut bytes = vec![0u8; 0x80];
        bytes[0..2].copy_from_slice(b"MZ");
        bytes[0x3c..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        bytes[0x40..0x44].copy_from_slice(b"PE\0\0");
        bytes[0x44..0x46].copy_from_slice(&machine.to_le_bytes());
        let path = dir.join(format!("{machine:x}.exe"));
        fs::write(&path, bytes).unwrap();
        path
    }

    fn source(id: &str, arch: Arch, status: Status) -> Source {
        Source {
            id: id.into(),
            name: id.into(),
            url: String::new(),
            sha256: None,
            wine: PathBuf::from("bin/wine"),
            arch: vec![arch],
            status,
            notes: String::new(),
        }
    }

    #[test]
    fn exe_arch_reads_pe_machine() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(exe_arch(&fake_exe(dir.path(), 0x014c)).unwrap(), Arch::I386));
        assert!(matches!(exe_arch(&fake_exe(dir.path(), 0x8664)).unwrap(), Arch::X86_64));
        assert!(exe_arch(&fake_exe(dir.path(), 0xaa64)).is_err()); // ARM64
    }

    #[test]
    fn exe_arch_rejects_non_exes() {
        let dir = tempfile::tempdir().unwrap();
        let text = dir.path().join("notes.txt");
        fs::write(&text, "hello").unwrap();
        assert!(exe_arch(&text).is_err());

        // Header points past the end of the file.
        let truncated = dir.path().join("short.exe");
        let mut bytes = vec![0u8; 0x40];
        bytes[0x3c..0x40].copy_from_slice(&0x1000u32.to_le_bytes());
        fs::write(&truncated, bytes).unwrap();
        assert!(exe_arch(&truncated).is_err());

        assert!(exe_arch(&dir.path().join("missing.exe")).is_err());
    }

    #[test]
    fn builtin_config_is_valid() {
        let all = parse(BUILTIN).unwrap();
        assert!(!all.is_empty());
        for src in &all {
            let dupes = all.iter().filter(|s| s.id == src.id).count();
            assert_eq!(dupes, 1, "duplicate id {}", src.id);
            let sha = src.sha256.as_deref().unwrap_or_default();
            assert_eq!(sha.len(), 64, "{} needs a sha256", src.id);
            assert!(src.url.starts_with("https://"), "{} url", src.id);
        }
        // Every arch has a runtime to auto-pick.
        assert!(pick(&all, Arch::I386).is_some());
        assert!(pick(&all, Arch::X86_64).is_some());
    }

    #[test]
    fn user_entries_replace_or_append() {
        let mut all = vec![source("a", Arch::I386, Status::Tested)];
        merge(
            &mut all,
            vec![source("a", Arch::X86_64, Status::Broken), source("b", Arch::I386, Status::Untested)],
        );
        let ids: Vec<_> = all.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
        assert!(all[0].status == Status::Broken);
    }

    #[test]
    fn pick_prefers_tested_and_skips_broken() {
        let all = vec![
            source("broken", Arch::I386, Status::Broken),
            source("untested", Arch::I386, Status::Untested),
            source("tested", Arch::I386, Status::Tested),
        ];
        assert_eq!(pick(&all, Arch::I386).unwrap().id, "tested");
        assert_eq!(pick(&all[..2], Arch::I386).unwrap().id, "untested");
        assert!(pick(&all[..1], Arch::I386).is_none());
        assert!(pick(&all, Arch::X86_64).is_none());
    }

    #[test]
    fn clean_path_undoes_drag_and_drop_quoting() {
        let home = PathBuf::from(std::env::var("HOME").unwrap());
        assert_eq!(clean_path("'/a b/c'"), PathBuf::from("/a b/c"));
        assert_eq!(clean_path("\"/a b/c\""), PathBuf::from("/a b/c"));
        assert_eq!(clean_path("/a\\ b/c"), PathBuf::from("/a b/c"));
        assert_eq!(clean_path("~/Games"), home.join("Games"));
    }
}
