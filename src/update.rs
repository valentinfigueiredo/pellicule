//! Mise à jour depuis GitHub : on compare la version du `Cargo.toml` du dernier commit de `master`
//! avec la nôtre, puis on télécharge le `dist/pellicule.exe` de ce même commit avec le `curl`
//! fourni par Windows. Viser un commit précis évite le cache de GitHub (≈ 5 min), qui pourrait
//! sinon annoncer la nouvelle version tout en servant l'ancien exe.

use eframe::egui::Context;
use std::env;
use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::{self, Receiver};

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
pub const REPO: &str = "https://github.com/valentinfigueiredo/pellicule";
const RAW: &str = "https://raw.githubusercontent.com/valentinfigueiredo/pellicule";
const API: &str = "https://api.github.com/repos/valentinfigueiredo/pellicule/commits/master";

#[derive(Clone, PartialEq, Debug)]
pub enum State {
    /// Pas encore vérifié, ou pas de connexion.
    Unknown,
    UpToDate,
    Available { version: String, rev: String },
    Installing,
    Failed(String),
}

pub fn check_in_background(ctx: Context) -> Receiver<State> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let state = match latest() {
            Ok((version, rev)) if newer(&version, CURRENT) => State::Available { version, rev },
            Ok(_) => State::UpToDate,
            Err(_) => State::Unknown, // hors ligne : on ne dérange pas
        };
        let _ = tx.send(state);
        ctx.request_repaint();
    });
    rx
}

/// Télécharge la nouvelle version, remplace l'exe et relance Pellicule.
pub fn install_in_background(ctx: Context, rev: String) -> Receiver<State> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let err = install(&rev).err().unwrap_or_default();
        let _ = tx.send(State::Failed(err));
        ctx.request_repaint();
    });
    rx
}

/// Supprime l'ancienne version laissée par la dernière mise à jour.
pub fn cleanup() {
    if let Ok(exe) = env::current_exe() {
        let _ = fs::remove_file(exe.with_file_name("pellicule.old.exe"));
    }
}

/// La version publiée et le commit où la trouver.
fn latest() -> Result<(String, String), String> {
    let rev = last_commit().unwrap_or_else(|| "master".into());
    let out = curl().arg(format!("{RAW}/{rev}/Cargo.toml")).output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err("GitHub injoignable".into());
    }
    let version = parse_version(&String::from_utf8_lossy(&out.stdout)).ok_or("version introuvable")?;
    Ok((version, rev))
}

fn last_commit() -> Option<String> {
    let out = curl().args(["-H", "Accept: application/vnd.github.sha", API]).output().ok()?;
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && sha.len() == 40 && sha.bytes().all(|c| c.is_ascii_hexdigit())).then_some(sha)
}

fn install(rev: &str) -> Result<(), String> {
    let exe = env::current_exe().map_err(|e| e.to_string())?;
    let new = exe.with_file_name("pellicule.new.exe");
    let old = exe.with_file_name("pellicule.old.exe");

    let ok = curl()
        .arg("-o")
        .arg(&new)
        .arg(format!("{RAW}/{rev}/dist/pellicule.exe"))
        .status()
        .map_err(|e| format!("curl introuvable : {e}"))?
        .success();
    if !ok || !looks_like_exe(&new) {
        let _ = fs::remove_file(&new);
        return Err("téléchargement impossible, réessaie plus tard".into());
    }

    // Sous Windows on ne peut pas écraser un exe en cours d'exécution, mais on peut le renommer.
    let _ = fs::remove_file(&old);
    fs::rename(&exe, &old).map_err(|e| format!("remplacement impossible : {e}"))?;
    if let Err(e) = fs::rename(&new, &exe) {
        let _ = fs::rename(&old, &exe);
        return Err(format!("remplacement impossible : {e}"));
    }
    Command::new(&exe).spawn().map_err(|e| format!("relance impossible : {e}"))?;
    std::process::exit(0);
}

fn looks_like_exe(path: &PathBuf) -> bool {
    let mut head = [0u8; 2];
    let big_enough = fs::metadata(path).is_ok_and(|m| m.len() > 1_000_000);
    big_enough && fs::File::open(path).and_then(|mut f| f.read_exact(&mut head)).is_ok() && &head == b"MZ"
}

fn curl() -> Command {
    // Le curl de Windows (présent depuis Windows 10), sans passer par le PATH.
    let system = env::var_os("SystemRoot").map(|r| PathBuf::from(r).join("System32").join("curl.exe"));
    let mut cmd = Command::new(system.filter(|p| p.exists()).unwrap_or_else(|| "curl".into()));
    cmd.args(["-fsSL", "--max-time", "300", "-A", "pellicule"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW : pas de console qui clignote
    }
    cmd
}

/// La première ligne `version = "…"` du Cargo.toml (celle du paquet).
fn parse_version(toml: &str) -> Option<String> {
    toml.lines().find_map(|l| {
        let v = l.trim().strip_prefix("version")?.trim_start().strip_prefix('=')?;
        Some(v.trim().trim_matches('"').to_string())
    })
}

fn newer(candidate: &str, current: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> { v.split('.').map(|n| n.parse().unwrap_or(0)).collect() };
    parse(candidate) > parse(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(newer("0.3.0", "0.2.0"));
        assert!(newer("0.10.0", "0.9.9"));
        assert!(!newer("0.2.0", "0.2.0"));
        assert!(!newer("0.1.9", "0.2.0"));
        let toml = "[package]\nname = \"pellicule\"\nversion = \"0.4.1\"\n\n[dependencies]\nchrono = { version = \"0.4\" }\n";
        assert_eq!(parse_version(toml).as_deref(), Some("0.4.1"));
    }

    /// Interroge le vrai dépôt : `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn github() {
        let (version, rev) = latest().unwrap();
        println!("version {version} au commit {rev}");
        assert_eq!(rev.len(), 40);
    }
}
