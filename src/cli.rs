//! Les commandes en ligne de commande (`pellicule importer …`).

use crate::album::{self, Album, Mode, Outcome};
use crate::config;
use crate::date::Source;
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const HELP: &str = "\
Pellicule — un seul album pour toutes tes photos et vidéos, classé par date.

UTILISATION
  Double-clic sur pellicule.exe
      Ouvre ta pellicule (toutes tes photos, de la plus récente à la plus ancienne)
      et range automatiquement ce que tu déposes dans le dossier « À trier ».

  Glisser-déposer des fichiers ou dossiers sur pellicule.exe ou sa fenêtre
      Les COPIE dans l'album (les originaux ne sont pas touchés).

  pellicule importer <dossiers/fichiers…>   copie dans l'album
  pellicule surveiller                      surveille « À trier » en continu
  pellicule trier                           traite « À trier » une seule fois
  pellicule album [chemin]                  affiche ou change l'emplacement de l'album
  pellicule apercu <fichier>                explique pourquoi un aperçu ne s'affiche pas
  pellicule ranger                          relit les dates et range ce qui n'est pas à sa place

OPTIONS
  --deplacer        déplace au lieu de copier (les originaux disparaissent)
  --essai           montre ce qui serait fait, sans rien toucher
  --album <chemin>  utilise cet album pour cette fois

RANGEMENT
  Album\\2024\\2024-07\\2024-07-15_14-32-08.jpg
  Deux photos à la même seconde : 2024-07-15_14-32-08_01.jpg
";

pub fn is_command(a: &str) -> bool {
    matches!(a, "importer" | "surveiller" | "trier" | "ranger" | "album" | "apercu" | "aperçu" | "aide" | "-h" | "--help" | "/?" | "--version")
}

pub struct Opts {
    pub mode: Mode,
    pub dry: bool,
    pub album: Option<PathBuf>,
}

pub fn parse(args: impl IntoIterator<Item = OsString>) -> io::Result<(Opts, Vec<PathBuf>)> {
    let mut opts = Opts { mode: Mode::Copy, dry: false, album: None };
    let mut paths = Vec::new();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.to_str() {
            Some("--deplacer" | "--déplacer") => opts.mode = Mode::Move,
            Some("--essai") => opts.dry = true,
            Some("--album") => opts.album = it.next().map(PathBuf::from),
            Some(s) if s.starts_with("--") => return Err(io::Error::other(format!("option inconnue : {s}"))),
            _ => paths.push(PathBuf::from(a)),
        }
    }
    Ok((opts, paths))
}

pub fn run(command: &str, opts: Opts, paths: Vec<PathBuf>) -> io::Result<()> {
    match command {
        "aide" | "-h" | "--help" | "/?" => {
            print!("{HELP}");
            Ok(())
        }
        "--version" => {
            println!("pellicule {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "album" => match paths.first() {
            Some(p) => {
                let p = config::absolute(p);
                config::save_album(&p)?;
                println!("Album : {}", p.display());
                Ok(())
            }
            None => {
                let p = config::album(opts.album.as_deref()).unwrap_or_else(config::default_album);
                println!("Album : {}", p.display());
                Ok(())
            }
        },
        "importer" => import(&opts, &paths),
        "apercu" | "aperçu" => {
            for p in &paths {
                println!("{}", p.display());
                for (method, result) in crate::thumbs::diagnose(p) {
                    match result {
                        Ok([w, h]) => println!("  {method} : OK ({w}×{h})"),
                        Err(e) => println!("  {method} : échec — {e}"),
                    }
                }
            }
            Ok(())
        }
        "trier" => {
            let mut album = open_album(&opts)?;
            let files = album::collect(&album.inbox());
            sort_inbox(&mut album, &files)
        }
        "surveiller" => watch(&opts),
        "ranger" => {
            let mut album = open_album(&opts)?;
            println!("Vérification des dates de {} fichiers…", album.len());
            let moved = album.reorganize(&mut |_, _| {})?;
            for (from, to) in &moved {
                println!("  {}  ->  {}", display(from), display(to));
            }
            match (moved.len(), album.dry) {
                (0, _) => println!("Tout est déjà bien rangé."),
                (1, true) => println!("\n1 fichier à ranger."),
                (1, false) => println!("\n1 fichier rangé."),
                (n, true) => println!("\n{n} fichiers à ranger."),
                (n, false) => println!("\n{n} fichiers rangés."),
            }
            Ok(())
        }
        _ => unreachable!(),
    }
}

fn album_root(opts: &Opts) -> io::Result<PathBuf> {
    if let Some(p) = config::album(opts.album.as_deref()) {
        return Ok(p);
    }
    // Premier lancement : on demande où mettre l'album.
    let default = config::default_album();
    println!("Bienvenue dans Pellicule !\n");
    println!("Où veux-tu créer ton album ?");
    print!("(Entrée = {}) : ", default.display());
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let line = line.trim().trim_matches('"');
    let chosen = if line.is_empty() { default } else { config::absolute(Path::new(line)) };
    if let Err(e) = config::save_album(&chosen) {
        eprintln!("(impossible d'enregistrer le choix : {e})");
    }
    println!();
    Ok(chosen)
}

fn open_album(opts: &Opts) -> io::Result<Album> {
    let album = Album::open(album_root(opts)?, opts.dry)?;
    println!("Album : {}  ({} fichiers)", album.root.display(), album.len());
    if opts.dry {
        println!("Mode essai : rien ne sera copié ni déplacé.");
    }
    Ok(album)
}

fn import(opts: &Opts, paths: &[PathBuf]) -> io::Result<()> {
    let mut album = open_album(opts)?;
    let sources = album::gather(&album.root, paths);
    for w in &sources.warnings {
        eprintln!("{w}");
    }
    if !sources.inbox.is_empty() {
        sort_inbox(&mut album, &sources.inbox)?;
    }
    println!("{} photos/vidéos trouvées", sources.outside.len());
    process(&mut album, &sources.outside, opts.mode)
}

fn sort_inbox(album: &mut Album, files: &[PathBuf]) -> io::Result<()> {
    let r = process(album, files, Mode::Move);
    if !album.dry {
        album::prune_empty_dirs(&album.inbox());
    }
    r
}

#[derive(Default)]
struct Summary {
    added: usize,
    from_name: usize,
    from_file: usize,
    duplicates: usize,
    errors: usize,
}

fn process(album: &mut Album, files: &[PathBuf], mode: Mode) -> io::Result<()> {
    if files.is_empty() {
        println!("Rien à faire.");
        return Ok(());
    }
    println!();
    let mut s = Summary::default();
    let width = files.len().to_string().len();
    for (i, f) in files.iter().enumerate() {
        let name = f.file_name().unwrap_or_default().to_string_lossy();
        let n = format!("[{:>width$}/{}]", i + 1, files.len());
        match album.import(f, mode) {
            Ok(Outcome::Added { rel, source }) => {
                s.added += 1;
                let note = match source {
                    Source::Exif | Source::Video => "",
                    Source::Nom => {
                        s.from_name += 1;
                        "  (date du nom)"
                    }
                    Source::Fichier => {
                        s.from_file += 1;
                        "  (date du fichier, à vérifier)"
                    }
                };
                println!("{n} + {name}  ->  {}{note}", display(&rel));
            }
            Ok(Outcome::Duplicate { of }) => {
                s.duplicates += 1;
                println!("{n} = {name}  déjà présent : {}", display(&of));
            }
            Ok(Outcome::Ignored) => {}
            Err(e) => {
                s.errors += 1;
                eprintln!("{n} ! {name}  erreur : {e}");
            }
        }
        // Sauvegarde régulière : un gros import interrompu ne perd rien.
        if i % 500 == 499 {
            album.save()?;
        }
    }
    album.save()?;

    let verb = match (album.dry, mode) {
        (true, _) => "à ajouter",
        (false, Mode::Copy) => "copiés",
        (false, Mode::Move) => "rangés",
    };
    println!("\n{} {verb}, {} doublons ignorés, {} erreurs.", s.added, s.duplicates, s.errors);
    if s.from_name + s.from_file > 0 {
        println!(
            "Sans date dans les métadonnées : {} datés par le nom, {} par la date du fichier.",
            s.from_name, s.from_file
        );
    }
    if mode == Mode::Move && s.duplicates > 0 && !album.dry {
        println!("Les doublons sont dans « {} » : tu peux vider ce dossier.", album::DUPLICATES);
    }
    Ok(())
}

fn display(rel: &str) -> String {
    rel.replace('/', std::path::MAIN_SEPARATOR_STR)
}

fn watch(opts: &Opts) -> io::Result<()> {
    let mut album = open_album(opts)?;
    let inbox = album.inbox();
    println!("\nDépose tes photos et vidéos dans :\n  {}\nElles seront renommées et rangées automatiquement. (Ctrl+C pour quitter)", inbox.display());
    let mut watcher = album::InboxWatcher::default();
    loop {
        let ready = watcher.poll(&inbox);
        if !ready.is_empty() {
            sort_inbox(&mut album, &ready)?;
            watcher.done(ready);
            println!("\nEn attente de nouveaux fichiers…");
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}
