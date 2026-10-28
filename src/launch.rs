//! Creates a per-game Wine prefix and launches the exe.

use std::fs::{self, File};
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::data_dir;
use crate::runtime::Runtime;

pub fn run(rt: &Runtime, exe: &Path, hud: bool, log: bool, args: &[String]) -> Result<()> {
    // Wine fails on bare relative names, so always pass an absolute path.
    let exe = exe
        .canonicalize()
        .with_context(|| format!("{} not found", exe.display()))?;
    let game_dir = exe.parent().context("exe has no parent folder")?;
    let game_name = game_dir
        .file_name()
        .and_then(|n| n.to_str())
        .context("bad game folder name")?;

    let prefix = data_dir()?.join("prefixes").join(game_name);
    if !prefix.join("drive_c").is_dir() {
        // Wine creates the prefix itself, but not its parent folders.
        fs::create_dir_all(prefix.parent().unwrap())?;
        println!("Creating Wine prefix at {} ...", prefix.display());
        let status = wine(rt, &prefix)
            .arg("wineboot")
            .arg("--init")
            .env("WINEDEBUG", "-all")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if !status.success() {
            bail!("wineboot failed");
        }
    }

    let mut cmd = wine(rt, &prefix);
    cmd.arg(&exe).args(args).current_dir(game_dir);
    if hud {
        cmd.env("MTL_HUD_ENABLED", "1");
    }
    if log {
        let logs = data_dir()?.join("logs");
        fs::create_dir_all(&logs)?;
        let path = logs.join(format!("{game_name}.log"));
        let file = File::create(&path)?;
        cmd.stdout(file.try_clone()?).stderr(file);
        println!("Logging to {}", path.display());
    } else {
        cmd.env("WINEDEBUG", "-all");
    }

    println!("Launching {} ...", exe.display());
    let status = cmd.status()?;
    if !status.success() {
        bail!("game exited with {status}");
    }
    Ok(())
}

/// Every wine call must share these env vars.
/// - A wineserver left running from wineboot with different sync
///   settings makes the game fail to start (esync_init error).
fn wine(rt: &Runtime, prefix: &Path) -> Command {
    let mut cmd = Command::new(rt.wine());
    cmd.env("WINEPREFIX", prefix)
        .env("WINEMSYNC", "1")
        .env("WINEESYNC", "1");
    cmd
}
