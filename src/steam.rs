//! Steam: account (login, logout, status, owned games) and installing games.
//! - Built on steamroom (async). This module runs its own tokio runtime; the rest of megh stays sync.
//! - The refresh token lives in the macOS Keychain.
//! - <data_dir>/steam.yml holds the account name and Steam ID (no secrets).
//! - Installs download Windows depots into <data_dir>/games/<slug>, then share `install::register`.

use std::convert::Infallible;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use prost::Message;
use serde::{Deserialize, Serialize};
use steamroom::apps::{AppDetails, Depot, DepotManifestInfo};
use steamroom::cdn::server::CdnServer;
use steamroom::cdn::{CdnClient, CdnServerPool};
use steamroom::client::{LoggedIn, SteamClient};
use steamroom::depot::manifest::DepotManifest;
use steamroom::depot::{AppId, CellId};
use steamroom::generated::{CPlayerGetOwnedGamesRequest, CPlayerGetOwnedGamesResponse};
use steamroom::types::key_value::{KeyValue, KvValue};
use steamroom_client::depot_config::DepotConfig;
use steamroom_client::download::{CdnChunkFetcher, DepotJob};
use steamroom_client::event::DownloadEvent;
use steamroom_client::login::{
    AuthTokens, ConfirmationChallenge, CredentialsLoginFlow, GuardType, LoginBuilder, LoginError,
};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::library::{Game, Library};
use crate::{confirm, data_dir, install, runtime};

/// The only branch megh installs for now.
const BRANCH: &str = "public";

const KEYCHAIN_SERVICE: &str = "megh-steam";
/// Shown in the Steam account's list of devices.
const DEVICE_NAME: &str = "megh";
const GUARD_TRIES: usize = 3;

/// Who's logged in.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct Account {
    name: String,
    steam_id: u64,
}

fn account_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("steam.yml"))
}

fn load_account(path: &Path) -> Result<Option<Account>> {
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(path)?;
    let account = serde_saphyr::from_str(&text).with_context(|| format!("bad {}", path.display()))?;
    Ok(Some(account))
}

fn save_account(path: &Path, account: &Account) -> Result<()> {
    fs::create_dir_all(path.parent().unwrap())?;
    fs::write(path, serde_saphyr::to_string(account)?)?;
    Ok(())
}

fn keychain(account: &str) -> Result<keyring::Entry> {
    Ok(keyring::Entry::new(KEYCHAIN_SERVICE, account)?)
}

/// Removes the account's token from the Keychain; a missing one is fine.
fn forget_token(account: &str) -> Result<()> {
    match keychain(account)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Runs async steamroom code to completion.
/// - Multi-threaded, so steamroom's background tasks keep going while we wait on stdin.
fn block_on<F: Future>(f: F) -> Result<F::Output> {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let out = rt.block_on(f);
    // Don't wait for a code prompt left unanswered (approved on the phone instead).
    rt.shutdown_background();
    Ok(out)
}

fn prompt(question: &str) -> Result<String> {
    print!("{question}");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

/// `megh steam login`
pub fn login() -> Result<()> {
    let name = prompt("Steam username: ")?;
    if name.is_empty() {
        bail!("no username given");
    }
    let password = rpassword::prompt_password("Password: ")?;
    let tokens = block_on(authenticate(&name, &password))??;

    let account = Account {
        steam_id: steam_id(&tokens.refresh_token)?,
        name: tokens.account_name.unwrap_or(name),
    };
    let path = account_path()?;
    // One account at a time: drop the previous one's token.
    if let Some(old) = load_account(&path)? {
        forget_token(&old.name)?;
    }
    keychain(&account.name)?.set_password(&tokens.refresh_token)?;
    save_account(&path, &account)?;
    println!("Logged in as {}.", account.name);
    Ok(())
}

/// Password, then Steam Guard if asked. Returns the tokens.
async fn authenticate(name: &str, password: &str) -> Result<AuthTokens> {
    let flow = LoginBuilder::new()
        .device_name(DEVICE_NAME)
        .with_credentials(name, password)
        .begin()
        .await?;
    match flow {
        CredentialsLoginFlow::Approved(approved) => Ok(approved.tokens().clone()),
        CredentialsLoginFlow::NeedsConfirmation(challenge) => steam_guard(&challenge).await,
        _ => bail!("Steam asked for a login step megh doesn't support"),
    }
}

/// Steam Guard: approve on the phone or enter a code, whichever comes first.
/// - Steam often offers both; approving makes a later code fail with DuplicateRequest.
async fn steam_guard(challenge: &ConfirmationChallenge) -> Result<AuthTokens> {
    let Some(&kind) = challenge
        .code_kinds()
        .iter()
        .find(|&&k| k == GuardType::DeviceCode)
        .or(challenge.code_kinds().first())
    else {
        println!("Approve the login in the Steam mobile app ...");
        return Ok(challenge.wait_for_tokens().await?);
    };
    let from = if kind == GuardType::DeviceCode { "your authenticator" } else { "your email" };
    let question = if challenge.accepts_confirmation() {
        format!("Approve on your phone, or enter the Steam Guard code from {from}: ")
    } else {
        format!("Steam Guard code from {from}: ")
    };

    // An accepted code also ends up in wait_for_tokens.
    tokio::select! {
        tokens = challenge.wait_for_tokens() => {
            println!();
            Ok(tokens?)
        }
        Err(e) = enter_codes(challenge, kind, &question) => Err(e),
    }
}

/// Prompts for codes until one is accepted, then waits forever (tokens come from polling).
async fn enter_codes(challenge: &ConfirmationChallenge, kind: GuardType, question: &str) -> Result<Infallible> {
    for attempt in 1..=GUARD_TRIES {
        let q = question.to_string();
        let code = tokio::task::spawn_blocking(move || prompt(&q)).await??;
        match challenge.submit_code(&code, kind).await {
            Ok(()) => std::future::pending::<()>().await,
            Err(LoginError::InvalidGuardCode) if attempt < GUARD_TRIES => eprintln!("Wrong code, try again."),
            Err(e) => return Err(e.into()),
        }
    }
    bail!("too many wrong codes")
}

/// The Steam ID inside a Steam token: a JWT whose `sub` claim is the ID.
fn steam_id(token: &str) -> Result<u64> {
    let payload = token.split('.').nth(1).context("Steam token isn't a JWT")?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('='))?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes)?;
    let sub = claims["sub"].as_str().context("Steam token has no Steam ID")?;
    sub.parse().with_context(|| format!("bad Steam ID '{sub}'"))
}

/// `megh steam logout`
/// - Only forgets the login on this Mac; the device stays in Steam's list until removed there.
pub fn logout() -> Result<()> {
    let path = account_path()?;
    let Some(account) = load_account(&path)? else {
        println!("Not logged in.");
        return Ok(());
    };
    forget_token(&account.name)?;
    fs::remove_file(&path)?;
    println!("Logged out {}.", account.name);
    Ok(())
}

/// The saved account and its token.
fn saved() -> Result<(Account, String)> {
    let account = load_account(&account_path()?)?.context("not logged in; run: megh steam login")?;
    let token = keychain(&account.name)?
        .get_password()
        .context("no saved Steam token; run: megh steam login")?;
    Ok((account, token))
}

/// Logs in with the saved token.
async fn connect(account: &Account, token: String) -> Result<SteamClient<LoggedIn>, LoginError> {
    LoginBuilder::new()
        .device_name(DEVICE_NAME)
        .with_refresh_token(&account.name, token)
        .login()
        .await
}

/// `megh steam status`
pub fn status() -> Result<()> {
    let (account, token) = saved()?;
    println!("Account: {} (Steam ID {})", account.name, account.steam_id);
    match block_on(connect(&account, token))? {
        Ok(_) => println!("Saved login works."),
        Err(e) => println!("Saved login failed ({e}); run: megh steam login"),
    }
    Ok(())
}

/// `megh steam games`
pub fn games() -> Result<()> {
    let (account, token) = saved()?;
    let request = CPlayerGetOwnedGamesRequest {
        steamid: Some(account.steam_id),
        include_appinfo: Some(true),
        include_played_free_games: Some(true),
        ..Default::default()
    };
    let response: CPlayerGetOwnedGamesResponse = block_on(async {
        let client = connect(&account, token)
            .await
            .context("saved login failed; run: megh steam login")?;
        let reply = client
            .call_service_method("Player.GetOwnedGames#1", &request.encode_to_vec())
            .await?;
        anyhow::Ok(reply.decode()?)
    })??;

    let lines = game_lines(response);
    if lines.is_empty() {
        println!("No games on this account.");
    }
    for line in &lines {
        println!("{line}");
    }
    Ok(())
}

/// "<appid>  <name>" lines, sorted by name.
fn game_lines(response: CPlayerGetOwnedGamesResponse) -> Vec<String> {
    let mut games: Vec<_> = response
        .games
        .into_iter()
        .map(|g| (g.appid(), g.name.unwrap_or_default()))
        .collect();
    games.sort_by_key(|(_, name)| name.to_lowercase());
    let width = games.iter().map(|(id, _)| id.to_string().len()).max().unwrap_or(0);
    games.iter().map(|(id, name)| format!("{id:>width$}  {name}")).collect()
}

/// The app id in `megh install steam:<appid>`; None for anything else (a GOG installer path).
pub fn parse_target(setup: &Path) -> Result<Option<u32>> {
    let Some(id) = setup.to_str().and_then(|s| s.strip_prefix("steam:")) else {
        return Ok(None);
    };
    let id = id.parse().with_context(|| format!("bad Steam app id '{id}' (see: megh steam games)"))?;
    Ok(Some(id))
}

/// `megh steam install <appid>` / `megh install steam:<appid>`
/// - Downloads into games/.installing-<slug>; rerunning after an interruption resumes there.
pub fn install(app_id: u32, runtime_id: Option<&str>) -> Result<()> {
    let (account, token) = saved()?;
    let games = data_dir()?.join("games");
    let Some((slug, game)) = block_on(download(&account, token, AppId(app_id), &games))?? else {
        return Ok(());
    };
    let rt = runtime::resolve(runtime_id, &game.install_dir.join(&game.exe))?;
    install::register(&slug, game, &rt)
}

/// Downloads the game's Windows depots. None if the user said no.
async fn download(account: &Account, token: String, app_id: AppId, games: &Path) -> Result<Option<(String, Game)>> {
    let client = connect(account, token)
        .await
        .context("saved login failed; run: megh steam login")?;
    let details = client.app_details(app_id).await?;
    let name = details
        .name
        .clone()
        .with_context(|| format!("Steam has no app {app_id} (see: megh steam games)"))?;
    let slug = install::slugify(&name);
    let dest = games.join(&slug);
    if dest.exists() || Library::load()?.games.contains_key(&slug) {
        bail!("'{slug}' is already in the library");
    }
    let depots = pick_depots(&details);
    if depots.is_empty() {
        bail!("{name} has no Windows depots to download");
    }
    let (exe, args) = launch_option(&details.key_values).context("no Windows launch option in the app info")?;

    let total = |size: fn(&DepotManifestInfo) -> Option<u64>| depots.iter().filter_map(|(_, m)| size(m)).sum();
    println!(
        "{name} ({app_id}): {} depot(s), {} download, {} on disk",
        depots.len(),
        fmt_size(total(|m| m.download_size)),
        fmt_size(total(|m| m.size))
    );
    let staging = games.join(format!(".installing-{slug}"));
    if staging.exists() {
        println!("Resuming an earlier download.");
    } else if !confirm(&format!("Install to {}?", dest.display()), true)? {
        println!("Cancelled.");
        return Ok(None);
    }
    fs::create_dir_all(&staging)?;

    let servers = client.get_cdn_servers(CellId(0), Some(20)).await?;
    if servers.is_empty() {
        bail!("Steam returned no download servers");
    }
    for (i, (depot, manifest)) in depots.iter().enumerate() {
        let label = format!("Depot {}/{} ({})", i + 1, depots.len(), depot.id.0);
        download_depot(&client, app_id, depot, manifest, &servers, &staging, &label)
            .await
            .context("download stopped; run the same command again to resume")?;
    }
    fs::rename(&staging, &dest)?;

    let game = Game {
        name,
        gog_id: None,
        steam_app_id: Some(app_id.0),
        install_dir: dest,
        exe,
        args,
        runtime: None,
        graphics: None,
        env: Default::default(),
        saves: Vec::new(),
    };
    Ok(Some((slug, game)))
}

/// One depot into `dir`, following steamroom's own CLI.
/// - steamroom records progress in <dir>/.DepotDownloader, which is what makes resuming work.
async fn download_depot(
    client: &SteamClient<LoggedIn>,
    app_id: AppId,
    depot: &Depot,
    manifest: &DepotManifestInfo,
    servers: &[CdnServer],
    dir: &Path,
    label: &str,
) -> Result<()> {
    let (depot_id, manifest_id) = (depot.id, manifest.manifest_id);
    let key = client.get_depot_decryption_key(depot_id, app_id).await?;
    // Both are optional: old manifests have no request code, public depots need no CDN token.
    let request_code = client
        .get_manifest_request_code(app_id, depot_id, manifest_id, Some(BRANCH), None)
        .await
        .ok()
        .flatten()
        .unwrap_or(0);
    let cdn_token = client
        .get_cdn_auth_token(app_id, depot_id, &servers[0].host)
        .await
        .ok()
        .and_then(|t| t.token);

    let pool = CdnServerPool::new(servers.to_vec());
    let cdn = CdnClient::new()?;
    let raw = cdn
        .download_manifest_pooled(&pool, depot_id, manifest_id, request_code, cdn_token.as_deref())
        .await?;
    let bytes = unzip_manifest(&raw)?;
    let mut files = DepotManifest::parse(&bytes)?;
    if files.filenames_encrypted {
        files.decrypt_filenames(&key)?;
    }

    let mut config = DepotConfig::load(dir);
    config.set_installing(depot_id, manifest_id, &key);
    config.save(dir)?;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let job = DepotJob::builder()
        .depot_id(depot_id)
        .depot_key(key.clone())
        .install_dir(dir.to_path_buf())
        .verify(true)
        .event_sender(tx)
        .build()?;
    let progress = tokio::spawn(show_progress(rx, label.to_string()));
    let result = job.download(&files, Arc::new(CdnChunkFetcher::new(cdn, pool, cdn_token))).await;
    // The job holds the sender; dropping it ends the progress task.
    drop(job);
    let _ = progress.await;
    result.map_err(|e| anyhow!("{e}"))?;

    DepotConfig::save_manifest_decompressed(dir, depot_id, manifest_id, &bytes)?;
    config.set_installed(depot_id, manifest_id, &key);
    config.save(dir)?;
    Ok(())
}

/// "<label>: 45% (1.2 GB / 2.7 GB)" on one line, redrawn as chunks arrive.
async fn show_progress(mut events: UnboundedReceiver<DownloadEvent>, label: String) {
    let (mut done, mut total) = (0u64, 0u64);
    while let Some(event) = events.recv().await {
        match event {
            DownloadEvent::DownloadStarted { total_bytes, .. } => total = total_bytes,
            DownloadEvent::ChunkCompleted { bytes } => done += bytes,
            _ => continue,
        }
        let percent = (done * 100).checked_div(total).unwrap_or(0);
        print!("\r{label}: {percent}% ({} / {})   ", fmt_size(done), fmt_size(total));
        let _ = io::stdout().flush();
    }
    println!();
}

/// CDN manifests come zipped (one entry); older ones may be raw.
fn unzip_manifest(raw: &[u8]) -> Result<Vec<u8>> {
    if !raw.starts_with(b"PK") {
        return Ok(raw.to_vec());
    }
    let mut archive = zip::ZipArchive::new(io::Cursor::new(raw))?;
    let mut bytes = Vec::new();
    archive.by_index(0)?.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// The Windows depots to download, with their public-branch manifests.
/// - Skips other OSes and languages, redistributables shared from other apps, and DLC.
/// - Prefers 64-bit when the game ships both.
fn pick_depots(details: &AppDetails) -> Vec<(&Depot, &DepotManifestInfo)> {
    // DLC depots are marked in the raw app info only.
    let raw = details.key_values.get("depots");
    let is_dlc = |d: &Depot| {
        raw.and_then(|r| r.get(&d.id.0.to_string()))
            .and_then(|r| r.get("dlcappid"))
            .is_some()
    };
    let fits = |d: &&Depot| {
        let c = &d.config;
        (c.os_list.is_empty() || c.os_list.iter().any(|os| os == "windows"))
            && c.language.as_deref().is_none_or(|l| l.is_empty() || l == "english")
            && !c.low_violence
            && !d.is_shared()
            && !is_dlc(d)
    };
    let depots: Vec<_> = details
        .depots
        .iter()
        .filter(fits)
        .filter_map(|d| Some((d, d.manifest(BRANCH)?)))
        .collect();
    let arch = |d: &Depot| d.config.os_arch.clone();
    let has_64 = depots.iter().any(|(d, _)| arch(d).as_deref() == Some("64"));
    depots
        .into_iter()
        .filter(|(d, _)| !has_64 || arch(d).as_deref() != Some("32"))
        .collect()
}

/// The exe (relative to the install dir) and arguments from the app info's launch list.
/// - The first Windows .exe entry, preferring the default launch type; beta-only entries are skipped.
fn launch_option(app: &KeyValue) -> Option<(PathBuf, Vec<String>)> {
    let KvValue::Children(entries) = &app.get("config")?.get("launch")?.value else {
        return None;
    };
    // Keys are "0", "1", ...; BTreeMap orders them as strings ("10" < "2").
    let mut entries: Vec<_> = entries
        .iter()
        .filter_map(|(k, v)| Some((k.parse::<u32>().ok()?, v)))
        .collect();
    entries.sort_by_key(|(k, _)| *k);

    let text = |kv: &KeyValue, key: &str| kv.get(key).and_then(|v| v.as_str()).map(str::to_string);
    let windows = |e: &&KeyValue| {
        let config = e.get("config");
        let os = config.and_then(|c| text(c, "oslist"));
        os.is_none_or(|os| os.split(',').any(|o| o.trim() == "windows"))
            && config.and_then(|c| c.get("betakey")).is_none()
            && text(e, "executable").is_some_and(|x| x.to_lowercase().ends_with(".exe"))
    };
    let usable: Vec<&KeyValue> = entries.into_iter().map(|(_, e)| e).filter(windows).collect();
    let entry = usable
        .iter()
        .find(|e| text(e, "type").is_none_or(|t| t == "default"))
        .or(usable.first())?;

    let exe = PathBuf::from(text(entry, "executable")?.replace('\\', "/"));
    let args = text(entry, "arguments")
        .map(|a| a.split_whitespace().map(String::from).collect())
        .unwrap_or_default();
    Some((exe, args))
}

fn fmt_size(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{:.1} GB", bytes as f64 / 1e9)
    } else {
        format!("{:.0} MB", bytes as f64 / 1e6)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use steamroom::generated::c_player_get_owned_games_response::Game;

    #[test]
    fn account_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("megh/steam.yml");
        assert_eq!(load_account(&path).unwrap(), None);
        let account = Account { name: "gaben".into(), steam_id: 76561197960287930 };
        save_account(&path, &account).unwrap();
        assert_eq!(load_account(&path).unwrap(), Some(account));
    }

    #[test]
    fn steam_id_comes_from_the_token() {
        let b64 = |s: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s);
        let token = format!("{}.{}.sig", b64(r#"{"typ":"JWT"}"#), b64(r#"{"sub":"76561197960287930","aud":["client"]}"#));
        assert_eq!(steam_id(&token).unwrap(), 76561197960287930);

        assert!(steam_id("not-a-jwt").is_err());
        let no_sub = format!("x.{}.sig", b64(r#"{"aud":["client"]}"#));
        assert!(steam_id(&no_sub).is_err());
    }

    #[test]
    fn games_are_sorted_by_name_and_aligned() {
        let game = |id: i32, name: &str| Game { appid: Some(id), name: Some(name.into()), ..Default::default() };
        let response = CPlayerGetOwnedGamesResponse {
            game_count: Some(3),
            games: vec![game(1145360, "Hades"), game(620, "Portal 2"), game(1794680, "Vampire Survivors")],
        };
        assert_eq!(game_lines(response), ["1145360  Hades", "    620  Portal 2", "1794680  Vampire Survivors"]);
        assert!(game_lines(CPlayerGetOwnedGamesResponse::default()).is_empty());
    }

    /// App info in Steam's text KeyValues format, like PICS returns.
    const APP: &str = r#"
"1000"
{
    "common" { "name" "Test Game" "type" "game" }
    "config"
    {
        "launch"
        {
            "0" { "executable" "TestGame.app" "config" { "oslist" "macos" } }
            "1" { "executable" "bin\\Beta.exe" "config" { "oslist" "windows" "betakey" "beta" } }
            "2" { "executable" "bin\\Tools.exe" "type" "option1" "config" { "oslist" "windows" } }
            "10" { "executable" "bin\\Game.exe" "arguments" "-windowed -nointro" "type" "default" }
        }
    }
    "depots"
    {
        "branches" { "public" { "buildid" "1" } }
        "1001" { "config" { "oslist" "windows" "osarch" "32" } "manifests" { "public" { "gid" "11" "size" "100" "download" "50" } } }
        "1002" { "config" { "oslist" "windows" "osarch" "64" } "manifests" { "public" { "gid" "12" } } }
        "1003" { "config" { "oslist" "windows" } "manifests" { "public" { "gid" "13" } } }
        "1004" { "config" { "oslist" "macos" } "manifests" { "public" { "gid" "14" } } }
        "1005" { "config" { "language" "german" } "manifests" { "public" { "gid" "15" } } }
        "1006" { "dlcappid" "2000" "manifests" { "public" { "gid" "16" } } }
        "1007" { "config" { "oslist" "windows" } "manifests" { "beta" { "gid" "17" } } }
        "228988" { "depotfromapp" "228980" "manifests" { "public" "18" } }
    }
}
"#;

    fn details(text: &str) -> AppDetails {
        AppDetails::from_key_values(AppId(1000), KeyValue::from_text(text).unwrap())
    }

    fn picked(details: &AppDetails) -> Vec<u32> {
        pick_depots(details).iter().map(|(d, _)| d.id.0).collect()
    }

    #[test]
    fn picks_windows_depots_preferring_64_bit() {
        // Not: 32-bit (64 exists), mac, german, DLC, beta-only, redistributable.
        let app = details(APP);
        assert_eq!(picked(&app), [1002, 1003]);
        assert_eq!(pick_depots(&app)[1].1.manifest_id.0, 13);
    }

    #[test]
    fn keeps_32_bit_depots_when_there_is_no_64_bit_one() {
        let app = r#""1" { "depots" {
            "1" { "config" { "oslist" "windows" "osarch" "32" } "manifests" { "public" { "gid" "1" } } }
            "2" { "manifests" { "public" { "gid" "2" } } }
        } }"#;
        assert_eq!(picked(&details(app)), [1, 2]);
    }

    #[test]
    fn launch_option_is_the_default_windows_exe() {
        let (exe, args) = launch_option(&details(APP).key_values).unwrap();
        // "10" sorts after "2" numerically, and wins as the default type.
        assert_eq!(exe, PathBuf::from("bin/Game.exe"));
        assert_eq!(args, ["-windowed", "-nointro"]);
    }

    #[test]
    fn launch_option_falls_back_to_the_first_windows_exe() {
        let app = r#""1" { "config" { "launch" {
            "0" { "executable" "start.bat" }
            "1" { "executable" "Game.EXE" "type" "option1" }
        } } }"#;
        let (exe, args) = launch_option(&details(app).key_values).unwrap();
        assert_eq!(exe, PathBuf::from("Game.EXE"));
        assert!(args.is_empty());
        assert!(launch_option(&details(r#""1" { "common" { "name" "x" } }"#).key_values).is_none());
    }

    #[test]
    fn parse_target_reads_steam_app_ids() {
        assert_eq!(parse_target(Path::new("steam:1145360")).unwrap(), Some(1145360));
        assert!(parse_target(Path::new("steam:abc")).is_err());
        assert_eq!(parse_target(Path::new("~/Downloads/setup_game.exe")).unwrap(), None);
    }
}
