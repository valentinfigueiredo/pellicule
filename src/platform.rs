//! Ce qui dépend du système : ouvrir un fichier, l'afficher dans l'Explorateur, la console.

use std::path::Path;
use std::process::Command;

/// Ouvre le fichier avec l'application par défaut (Photos, lecteur vidéo…).
pub fn open(path: &Path) {
    #[cfg(windows)]
    let _ = Command::new("explorer").arg(path).spawn();
    #[cfg(target_os = "macos")]
    let _ = Command::new("open").arg(path).spawn();
    #[cfg(not(any(windows, target_os = "macos")))]
    let _ = Command::new("xdg-open").arg(path).spawn();
}

/// Affiche le fichier sélectionné dans l'Explorateur / le Finder.
pub fn reveal(path: &Path) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = Command::new("explorer").raw_arg(format!("/select,\"{}\"", path.display())).spawn();
    }
    #[cfg(target_os = "macos")]
    let _ = Command::new("open").arg("-R").arg(path).spawn();
    #[cfg(not(any(windows, target_os = "macos")))]
    let _ = Command::new("xdg-open").arg(path.parent().unwrap_or(path)).spawn();
}

/// L'exe Windows est une application fenêtrée : en ligne de commande, on se
/// rattache à la console qui l'a lancé pour pouvoir y écrire.
pub fn attach_console() {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}
