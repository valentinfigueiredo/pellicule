//! Sous Windows : l'icône et la description de `pellicule.exe` (Explorateur, Bureau, barre des tâches).

fn main() {
    println!("cargo:rerun-if-changed=assets/pellicule.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/pellicule.ico")
            .set("ProductName", "Pellicule")
            .set("FileDescription", "Pellicule")
            .set("LegalCopyright", "Pellicule");
        res.compile().expect("impossible d'ajouter l'icône à l'exe");
    }
}
