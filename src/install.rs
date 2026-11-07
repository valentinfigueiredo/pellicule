//! Installation sous Windows, sans droits administrateur (comme VS Code ou Discord) :
//! l'exe est copié dans `%LOCALAPPDATA%\Programs\Pellicule`, avec une icône sur le Bureau,
//! une entrée dans le menu Démarrer et dans « Applications installées » (pour désinstaller).
//! L'album, les photos et les réglages ne sont jamais touchés, ni à l'installation ni à la
//! désinstallation.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

pub const EXE: &str = "pellicule.exe";

pub fn install_dir() -> Option<PathBuf> {
    env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Programs").join("Pellicule"))
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Pellicule tourne-t-elle depuis son dossier d'installation ?
pub fn running_installed() -> bool {
    match (env::current_exe(), install_dir()) {
        (Ok(exe), Some(dir)) => same_file(&exe, &dir.join(EXE)),
        _ => false,
    }
}

pub fn is_installed() -> bool {
    install_dir().is_some_and(|d| d.join(EXE).exists())
}

/// Installe la version en cours d'exécution et renvoie le chemin de l'exe installé.
pub fn install() -> Result<PathBuf, String> {
    if !cfg!(windows) {
        return Err("l'installation n'existe que sous Windows".into());
    }
    let src = env::current_exe().map_err(|e| e.to_string())?;
    let dir = install_dir().ok_or("dossier LOCALAPPDATA introuvable")?;
    fs::create_dir_all(&dir).map_err(|e| format!("impossible de créer {} : {e}", dir.display()))?;
    let dst = dir.join(EXE);
    if !same_file(&src, &dst) {
        // Une version installée peut être ouverte : on ne peut pas l'écraser, mais on peut la renommer.
        if dst.exists() {
            let old = dir.join("pellicule.old.exe");
            let _ = fs::remove_file(&old);
            fs::rename(&dst, &old).map_err(|e| format!("version installée impossible à remplacer : {e}"))?;
        }
        fs::copy(&src, &dst).map_err(|e| format!("copie impossible : {e}"))?;
    }
    win::shortcuts(&dst)?;
    win::register(&dst, &dir);
    Ok(dst)
}

/// Au lancement de la version installée : garde à jour la version affichée dans
/// « Applications installées » (elle change avec le bouton « Mettre à jour »).
pub fn refresh_registration() {
    if cfg!(windows) && running_installed() {
        if let (Ok(exe), Some(dir)) = (env::current_exe(), install_dir()) {
            win::register(&exe, &dir);
        }
    }
}

/// Enlève l'icône, l'entrée du menu Démarrer et du panneau de désinstallation, puis l'exe
/// une fois Pellicule fermée. L'album et les réglages restent.
pub fn uninstall() -> Result<(), String> {
    if !cfg!(windows) {
        return Err("la désinstallation n'existe que sous Windows".into());
    }
    win::remove_shortcuts();
    win::unregister();
    if let Some(dir) = install_dir().filter(|d| d.exists()) {
        win::delete_after_exit(&dir);
    }
    Ok(())
}

/// Lance l'exe installé et quitte celui-ci.
pub fn relaunch(exe: &Path) -> ! {
    let _ = std::process::Command::new(exe).spawn();
    std::process::exit(0);
}

#[cfg(windows)]
mod win {
    use std::os::windows::process::CommandExt;
    use std::path::{Path, PathBuf};
    use windows::core::{Interface, HSTRING, PWSTR};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, IPersistFile, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_WRITE,
        REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ,
    };
    use windows::Win32::UI::Shell::{
        FOLDERID_Desktop, FOLDERID_Programs, IShellLinkW, SHGetKnownFolderPath, ShellLink, KF_FLAG_DEFAULT,
    };

    const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Pellicule";
    const LINK: &str = "Pellicule.lnk";

    fn known_folder(id: &windows::core::GUID) -> Option<PathBuf> {
        unsafe {
            let p: PWSTR = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
            let s = p.to_string().ok();
            CoTaskMemFree(Some(p.0 as _));
            s.map(PathBuf::from)
        }
    }

    fn link_paths() -> Vec<PathBuf> {
        [known_folder(&FOLDERID_Desktop), known_folder(&FOLDERID_Programs)]
            .into_iter()
            .flatten()
            .map(|d| d.join(LINK))
            .collect()
    }

    pub fn shortcuts(exe: &Path) -> Result<(), String> {
        let links = link_paths();
        if links.is_empty() {
            return Err("Bureau introuvable".into());
        }
        for link in links {
            create_link(&link, exe).map_err(|e| format!("raccourci {} : {e}", link.display()))?;
        }
        Ok(())
    }

    fn create_link(link: &Path, exe: &Path) -> windows::core::Result<()> {
        unsafe {
            // Déjà fait par la fenêtre (glisser-déposer) : l'erreur « mode différent » est sans importance.
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let shell: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            shell.SetPath(&HSTRING::from(exe.as_os_str()))?;
            if let Some(dir) = exe.parent() {
                shell.SetWorkingDirectory(&HSTRING::from(dir.as_os_str()))?;
            }
            shell.SetDescription(&HSTRING::from("Toutes tes photos et vidéos, classées par date"))?;
            shell.SetIconLocation(&HSTRING::from(exe.as_os_str()), 0)?;
            let file: IPersistFile = shell.cast()?;
            file.Save(&HSTRING::from(link.as_os_str()), true)
        }
    }

    pub fn remove_shortcuts() {
        for link in link_paths() {
            let _ = std::fs::remove_file(link);
        }
    }

    fn wide(s: &str) -> Vec<u8> {
        s.encode_utf16().chain(Some(0)).flat_map(|c| c.to_le_bytes()).collect()
    }

    /// L'entrée « Pellicule » dans Paramètres > Applications installées.
    pub fn register(exe: &Path, dir: &Path) {
        unsafe {
            let mut key = HKEY::default();
            let created = RegCreateKeyExW(
                HKEY_CURRENT_USER,
                &HSTRING::from(UNINSTALL_KEY),
                None,
                None,
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                None,
                &mut key,
                None,
            );
            if created.is_err() {
                return;
            }
            let exe = exe.display().to_string();
            let size_kb = std::fs::metadata(&exe).map_or(0, |m| (m.len() / 1024) as u32);
            for (name, value) in [
                ("DisplayName", "Pellicule".to_string()),
                ("DisplayVersion", env!("CARGO_PKG_VERSION").to_string()),
                ("Publisher", "Pellicule".to_string()),
                ("DisplayIcon", exe.clone()),
                ("InstallLocation", dir.display().to_string()),
                ("UninstallString", format!("\"{exe}\" desinstaller")),
                ("URLInfoAbout", crate::update::REPO.to_string()),
            ] {
                let _ = RegSetValueExW(key, &HSTRING::from(name), None, REG_SZ, Some(&wide(&value)));
            }
            for (name, value) in [("NoModify", 1u32), ("NoRepair", 1), ("EstimatedSize", size_kb)] {
                let _ = RegSetValueExW(key, &HSTRING::from(name), None, REG_DWORD, Some(&value.to_le_bytes()));
            }
            let _ = RegCloseKey(key);
        }
    }

    pub fn unregister() {
        unsafe {
            let _ = RegDeleteTreeW(HKEY_CURRENT_USER, &HSTRING::from(UNINSTALL_KEY));
        }
    }

    /// Un exe ne peut pas se supprimer lui-même : on demande à `cmd` de le faire juste après.
    pub fn delete_after_exit(dir: &Path) {
        let script = format!("ping 127.0.0.1 -n 4 > nul & rmdir /s /q \"{}\"", dir.display());
        let _ = std::process::Command::new("cmd").args(["/C", &script]).creation_flags(0x0800_0000).spawn();
    }

    /// Une petite fenêtre Windows (pour la désinstallation, lancée sans Pellicule ouverte).
    pub fn ask(text: &str) -> bool {
        use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, IDYES, MB_ICONQUESTION, MB_YESNO};
        unsafe { MessageBoxW(None, &HSTRING::from(text), &HSTRING::from("Pellicule"), MB_YESNO | MB_ICONQUESTION) == IDYES }
    }

    pub fn tell(text: &str) {
        use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONINFORMATION, MB_OK};
        unsafe {
            MessageBoxW(None, &HSTRING::from(text), &HSTRING::from("Pellicule"), MB_OK | MB_ICONINFORMATION);
        }
    }
}

#[cfg(not(windows))]
mod win {
    use std::path::Path;
    pub fn shortcuts(_exe: &Path) -> Result<(), String> {
        Ok(())
    }
    pub fn register(_exe: &Path, _dir: &Path) {}
    pub fn remove_shortcuts() {}
    pub fn unregister() {}
    pub fn delete_after_exit(_dir: &Path) {}
    pub fn ask(text: &str) -> bool {
        println!("{text} (oui)");
        true
    }
    pub fn tell(text: &str) {
        println!("{text}");
    }
}

/// `pellicule desinstaller` : appelé par Paramètres > Applications installées.
pub fn uninstall_interactive() -> Result<(), String> {
    if !win::ask("Désinstaller Pellicule ?\n\nTon album, tes photos et tes réglages ne seront pas touchés.") {
        return Ok(());
    }
    uninstall()?;
    win::tell("Pellicule est désinstallée.\n\nTon album est toujours là, avec toutes tes photos.");
    Ok(())
}

/// `pellicule installer` : installe sans passer par la fenêtre.
pub fn install_interactive() -> Result<(), String> {
    let exe = install()?;
    win::tell(&format!("Pellicule est installée : icône sur le Bureau et dans le menu Démarrer.\n\n{}", exe.display()));
    Ok(())
}
