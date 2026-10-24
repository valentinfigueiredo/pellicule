//! Pellicule : rassemble toutes tes photos et vidéos dans un seul album,
//! classé par date (`2024/2024-07/2024-07-15_14-32-08.jpg`), sans doublons.

// Sous Windows : application fenêtrée, sans console noire au double-clic.
#![windows_subsystem = "windows"]

mod album;
mod cli;
mod config;
mod date;
mod gui;
mod platform;
mod thumbs;

use std::env;
use std::ffi::OsString;

fn main() {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    let command = args.first().and_then(|a| a.to_str()).filter(|a| cli::is_command(a)).map(str::to_string);
    let rest = args.into_iter().skip(command.is_some() as usize);

    let result = cli::parse(rest).and_then(|(opts, paths)| match &command {
        Some(cmd) => {
            platform::attach_console();
            cli::run(cmd, opts, paths)
        }
        // Double-clic, ou fichiers glissés sur l'exe : la galerie.
        None => gui::run(opts.album, paths),
    });
    if let Err(e) = result {
        eprintln!("\nErreur : {e}");
        std::process::exit(1);
    }
}
