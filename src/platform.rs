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

/// Chiffre des données pour le compte Windows actuel (DPAPI) : la session iCloud
/// enregistrée sur le disque est illisible pour un autre utilisateur ou un autre PC.
pub fn protect(data: &[u8]) -> Vec<u8> {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Foundation::{LocalFree, HLOCAL};
        use windows::Win32::Security::Cryptography::{CryptProtectData, CRYPT_INTEGER_BLOB};
        let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let mut out = CRYPT_INTEGER_BLOB::default();
        if CryptProtectData(&input, None, None, None, None, 0, &mut out).is_ok() {
            let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
            let _ = LocalFree(Some(HLOCAL(out.pbData as _)));
            return v;
        }
    }
    data.to_vec()
}

pub fn unprotect(data: &[u8]) -> Option<Vec<u8>> {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Foundation::{LocalFree, HLOCAL};
        use windows::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};
        let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let mut out = CRYPT_INTEGER_BLOB::default();
        CryptUnprotectData(&input, None, None, None, None, 0, &mut out).ok()?;
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(out.pbData as _)));
        return Some(v);
    }
    #[cfg(not(windows))]
    Some(data.to_vec())
}
