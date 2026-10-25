//! Mise à jour depuis GitHub : on compare la version du `Cargo.toml` de la branche `master`
//! avec la nôtre, puis on télécharge `dist/pellicule.exe` avec le `curl` fourni par Windows.

use eframe::egui::Context;
use std::env;
use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::{self, Receiver};
use std::time::{SystemTime, UNIX_EPOCH};

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
pub const REPO: &str = "https://github.com/valentinfigueiredo/pellicule";
const RAW: &str = "https://raw.githubusercontent.com/valentinfigueiredo/pellicule/master";

#[derive(Clone, PartialEq, Debug)]
pub enum State {
    /// Pas encore vérifié, ou pas de connexion.
    Unknown,
    UpToDate,
    Available(String),
    Installing,
    Failed(String),
}

pub fn check_in_background(ctx: Context) -> Receiver<State> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let state = match latest() {
            Ok(v) if newer(&v, CURRENT) => State::Available(v),
            Ok(_) => State::UpToDate,
            Err(_) => State::Unknown, // hors ligne : on ne dérange pas
        };
        let _ = tx.send(state);
        ctx.request_repaint();
    });
    rx
}

/// Télécharge la nouvelle version, remplace l'exe et relance Pellicule.
pub fn install_in_background(ctx: Context) -> Receiver<State> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let err = install().err().unwrap_or_default();
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

fn latest() -> Result<String, String> {
    let out = curl().arg(format!("{RAW}/Cargo.toml?t={}", now())).output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err("GitHub injoignable".into());
    }
    parse_version(&String::from_utf8_lossy(&out.stdout)).ok_or_else(|| "version introuvable".into())
}

fn install() -> Result<(), String> {
    let exe = env::current_exe().map_err(|e| e.to_string())?;
    let new = exe.with_file_name("pellicule.new.exe");
    let old = exe.with_file_name("pellicule.old.exe");

    let ok = curl()
        .arg("-o")
        .arg(&new)
        .arg(format!("{RAW}/dist/pellicule.exe?t={}", now()))
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
    cmd.args(["-fsSL", "--max-time", "300"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW : pas de console qui clignote
    }
    cmd
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
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
}
