//! Connexion à iCloud Photos (non officielle) : même méthode que le site icloud.com,
//! reprise d'icloudpd (https://github.com/icloud-photos-downloader/icloud_photos_downloader).
//!
//! Tout tourne dans un fil d'exécution à part ; la fenêtre envoie des commandes et lit l'état.

mod client;
mod srp;
mod sync;

use client::{Error, Login, Session};
use eframe::egui::Context;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Sans action de ta part, on regarde s'il y a du nouveau sur iCloud toutes les 30 minutes.
const AUTO_SYNC: Duration = Duration::from_secs(30 * 60);

pub enum Cmd {
    Login { apple_id: String, password: String },
    Code(String),
    Sync,
    Logout,
}

#[derive(Clone)]
pub enum State {
    Busy(String),
    LoggedOut { apple_id: String, error: Option<String> },
    NeedCode { error: Option<String> },
    Ready { apple_id: String, last: Option<String> },
    Syncing { seen: u64, total: u64, downloaded: u64 },
}

pub struct Worker {
    cmds: Sender<Cmd>,
    state: Arc<Mutex<State>>,
    pub stop: Arc<AtomicBool>,
}

impl Worker {
    pub fn start(album_root: &Path, ctx: Context) -> Worker {
        let (cmds, rx) = mpsc::channel();
        let state = Arc::new(Mutex::new(State::Busy("Ouverture…".into())));
        let stop = Arc::new(AtomicBool::new(false));
        let (st, sp) = (state.clone(), stop.clone());
        let dir = album_root.join(".pellicule").join("icloud");
        let inbox = album_root.join(crate::album::INBOX);
        std::thread::spawn(move || {
            let set = |s: State| {
                *st.lock().unwrap() = s;
                ctx.request_repaint();
            };
            let mut session = Session::load(&dir);
            let mut ready = false;
            if !session.apple_id.is_empty() {
                set(State::Busy("Reconnexion à iCloud…".into()));
                ready = match session.resume() {
                    Ok(ok) => ok,
                    Err(e) => {
                        set(State::LoggedOut { apple_id: session.apple_id.clone(), error: Some(e.to_string()) });
                        false
                    }
                };
            }
            if !ready && !matches!(*st.lock().unwrap(), State::LoggedOut { .. }) {
                set(State::LoggedOut { apple_id: session.apple_id.clone(), error: None });
            }
            let mut pending_sync = ready;

            loop {
                let cmd = if pending_sync {
                    pending_sync = false;
                    Ok(Cmd::Sync)
                } else {
                    rx.recv_timeout(AUTO_SYNC)
                };
                match cmd {
                    Ok(Cmd::Login { apple_id, password }) => {
                        set(State::Busy("Connexion à iCloud…".into()));
                        match session.login(&apple_id, &password) {
                            Ok(Login::Ready) => {
                                ready = true;
                                pending_sync = true;
                            }
                            Ok(Login::NeedCode) => set(State::NeedCode { error: None }),
                            Err(e) => set(State::LoggedOut { apple_id, error: Some(e.to_string()) }),
                        }
                    }
                    Ok(Cmd::Code(code)) => {
                        set(State::Busy("Vérification du code…".into()));
                        match session.submit_code(&code) {
                            Ok(()) => {
                                ready = true;
                                pending_sync = true;
                            }
                            Err(Error::NeedLogin(m)) => {
                                set(State::LoggedOut { apple_id: session.apple_id.clone(), error: Some(m) })
                            }
                            Err(e) => set(State::NeedCode { error: Some(e.to_string()) }),
                        }
                    }
                    Ok(Cmd::Logout) => {
                        session.forget();
                        ready = false;
                        set(State::LoggedOut { apple_id: session.apple_id.clone(), error: None });
                    }
                    Ok(Cmd::Sync) | Err(RecvTimeoutError::Timeout) if ready => {
                        sp.store(false, Ordering::Relaxed);
                        set(State::Syncing { seen: 0, total: 0, downloaded: 0 });
                        let result = sync::run(&mut session, &dir, &inbox, &sp, &mut |p| {
                            set(State::Syncing { seen: p.seen, total: p.total, downloaded: p.downloaded })
                        });
                        let apple_id = session.apple_id.clone();
                        match result {
                            Ok(s) => {
                                let mut msg = match s.downloaded {
                                    0 => "Tout est déjà dans ta pellicule.".to_string(),
                                    1 => "1 nouvelle photo/vidéo récupérée.".to_string(),
                                    n => format!("{n} nouvelles photos/vidéos récupérées."),
                                };
                                if s.errors > 0 {
                                    msg += &format!(" {} n'ont pas pu être téléchargées (nouvel essai plus tard).", s.errors);
                                }
                                if sp.load(Ordering::Relaxed) {
                                    msg = format!("Arrêté. {msg}");
                                }
                                set(State::Ready { apple_id, last: Some(msg) });
                            }
                            Err(Error::NeedLogin(m)) => {
                                ready = false;
                                set(State::LoggedOut { apple_id, error: Some(m) });
                            }
                            Err(e) => set(State::Ready { apple_id, last: Some(e.to_string()) }),
                        }
                    }
                    Ok(Cmd::Sync) | Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return,
                }
                if ready && !pending_sync && matches!(*st.lock().unwrap(), State::Busy(_) | State::NeedCode { .. }) {
                    set(State::Ready { apple_id: session.apple_id.clone(), last: None });
                }
            }
        });
        Worker { cmds, state, stop }
    }

    pub fn state(&self) -> State {
        self.state.lock().unwrap().clone()
    }

    pub fn send(&self, cmd: Cmd) {
        let _ = self.cmds.send(cmd);
    }
}
