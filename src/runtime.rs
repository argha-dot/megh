//! Locates the GPTK runtime (Gcenx build: Wine + D3DMetal).
//! - Path is remembered in <data_dir>/runtime.
//! - If missing, asks the user for a path or offers to download.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::data_dir;

const GPTK_URL: &str = "https://github.com/Gcenx/game-porting-toolkit/releases/download/Game-Porting-Toolkit-3.0-3/game-porting-toolkit-3.0-3.tar.xz";
const GPTK_SHA256: &str = "d377683937340f914823dbb2e1252b329cbf834ff58907d0293db8cebf0e392e";
const APP_NAME: &str = "Game Porting Toolkit.app";

/// A usable "Game Porting Toolkit.app".
pub struct Runtime {
    app: PathBuf,
}

impl Runtime {
    pub fn wine(&self) -> PathBuf {
        self.app.join("Contents/Resources/wine/bin/wine64")
    }

    fn from_app(app: &Path) -> Result<Self> {
        let rt = Runtime { app: app.to_path_buf() };
        if !rt.wine().is_file() {
            bail!("no wine64 in {}", app.display());
        }
        if !app.join("Contents/Resources/wine/lib/external/D3DMetal.framework").is_dir() {
            bail!("no D3DMetal.framework in {}", app.display());
        }
        Ok(rt)
    }
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

/// Returns the configured runtime, prompting the user if there is none.
pub fn ensure() -> Result<Runtime> {
    let config = data_dir()?.join("runtime");
    if let Ok(saved) = fs::read_to_string(&config) {
        match Runtime::from_app(Path::new(saved.trim())) {
            Ok(rt) => return Ok(rt),
            Err(e) => eprintln!("Saved runtime is unusable: {e}"),
        }
    }

    let rt = prompt()?;
    fs::create_dir_all(data_dir()?)?;
    fs::write(&config, rt.app.to_string_lossy().as_bytes())?;
    println!("Using runtime: {}", rt.app.display());
    Ok(rt)
}

fn prompt() -> Result<Runtime> {
    println!("GPTK runtime (Wine + D3DMetal) not found.");
    loop {
        println!("Enter a path to \"{APP_NAME}\" or a .tar.xz (drag & drop works),");
        print!("or type 'download' to fetch Gcenx's GPTK 3.0-3 (~228MB): ");
        io::stdout().flush()?;

        let mut line = String::new();
        if io::stdin().read_line(&mut line)? == 0 {
            bail!("no runtime selected");
        }
        let input = line.trim();
        if input.is_empty() {
            continue;
        }

        let result = if input == "download" {
            download()
        } else {
            from_user_path(&clean_path(input))
        };
        match result {
            Ok(rt) => return Ok(rt),
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

/// Accepts the .app, a folder containing it, or a .tar.xz.
fn from_user_path(path: &Path) -> Result<Runtime> {
    if path.is_file() {
        return extract(path);
    }
    if path.join(APP_NAME).is_dir() {
        return Runtime::from_app(&path.join(APP_NAME));
    }
    Runtime::from_app(path)
}

fn download() -> Result<Runtime> {
    let dir = data_dir()?.join("downloads");
    fs::create_dir_all(&dir)?;
    let tarball = dir.join("game-porting-toolkit-3.0-3.tar.xz");

    let status = Command::new("curl")
        .args(["-L", "--fail", "-o"])
        .arg(&tarball)
        .arg(GPTK_URL)
        .status()?;
    if !status.success() {
        bail!("download failed");
    }

    let out = Command::new("shasum").args(["-a", "256"]).arg(&tarball).output()?;
    let sum = String::from_utf8_lossy(&out.stdout);
    if !sum.starts_with(GPTK_SHA256) {
        fs::remove_file(&tarball)?;
        bail!("checksum mismatch for downloaded runtime");
    }

    let rt = extract(&tarball)?;
    fs::remove_file(&tarball)?;
    Ok(rt)
}

/// Unpacks a GPTK tarball into <data_dir>/runtimes/<tarball name>/.
fn extract(tarball: &Path) -> Result<Runtime> {
    let name = tarball
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix(".tar.xz"))
        .context("expected a .tar.xz file")?;
    let dest = data_dir()?.join("runtimes").join(name);
    fs::create_dir_all(&dest)?;

    println!("Extracting {} ...", tarball.display());
    let status = Command::new("tar").arg("-xf").arg(tarball).arg("-C").arg(&dest).status()?;
    if !status.success() {
        bail!("failed to extract {}", tarball.display());
    }
    Runtime::from_app(&dest.join(APP_NAME))
}
