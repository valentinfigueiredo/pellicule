# Pellicule

Un seul album pour toutes tes photos et vidéos, classé et renommé par date, sans doublons.
Un seul fichier `pellicule.exe` (~5 Mo, écrit en Rust), rien à installer.

Au lancement, tu as **ta pellicule** : toutes tes miniatures, de la plus récente à la plus ancienne,
groupées par mois (comme sur un téléphone). Clic sur une photo ou une vidéo pour l'ouvrir,
clic droit pour l'afficher dans son dossier.

```
Pellicule\
  2023\
    2023-08\
      2023-08-14_18-42-05.jpg
      2023-08-14_18-42-05_01.jpg    ← deux photos à la même seconde
      2023-08-14_19-03-11.mov
  À trier\                          ← dépose ici, c'est rangé tout seul
  _Doublons\                        ← doublons mis de côté (tu peux vider)
  .pellicule\                       ← index + journal (ne pas toucher)
```

## Utilisation sur Windows

1. Mets `pellicule.exe` où tu veux (par ex. `C:\Outils\Pellicule\`).
2. **Premier lancement** : double-clic → il te demande où créer l'album
   (par défaut `C:\Users\<toi>\Pictures\Pellicule`). Le choix est mémorisé dans `pellicule.ini`.
3. **Rassembler ce que tu as déjà** : glisse tes dossiers (téléphone, clé USB, disque externe,
   export Google Photos…) **dans la fenêtre**, ou clique sur « Ajouter des photos… ».
   Les fichiers sont **copiés**, les originaux ne bougent pas.
4. **Au quotidien** : tant que la fenêtre est ouverte, tout ce que tu déposes dans `À trier`
   est renommé, rangé et apparaît dans la pellicule.

Miniatures : Pellicule utilise celles de Windows (rapides, déjà en cache). Pour les photos iPhone
(HEIC) et les vidéos HEVC, il faut les extensions gratuites « HEIF Image Extensions » / « HEVC »
du Microsoft Store (souvent déjà installées sur Windows 11).

En ligne de commande (depuis un terminal) :

```
pellicule importer D:\DCIM E:\Photos      copie dans l'album
pellicule importer --essai D:\DCIM        montre ce qui serait fait, sans rien toucher
pellicule importer --deplacer D:\DCIM     déplace au lieu de copier
pellicule trier                           range « À trier » une fois
pellicule surveiller                      range « À trier » en continu
pellicule album D:\MonAlbum               change l'emplacement de l'album
```

## Comment la date est trouvée

1. **Métadonnées** : EXIF des photos (JPG, HEIC, PNG, WebP, RAW…), date QuickTime des vidéos
   (MP4/MOV ; pour l'iPhone, l'heure locale exacte de la prise).
2. **Nom du fichier** : `IMG-20240715-WA0001` (WhatsApp), `PXL_20240715_143208`,
   `Screenshot 2024-07-15 at 14.32.08`…
3. **Date de modification du fichier**, en dernier recours (signalée « à vérifier »).

## Doublons

Un fichier est considéré comme un doublon s'il est **identique octet par octet** à un fichier de l'album
(empreinte xxh3, calculée seulement quand deux fichiers ont la même taille, c'est pour ça que c'est rapide).
Une photo recompressée (ex. envoyée par WhatsApp) n'est pas identique à l'originale : elle sera gardée.

Rien n'est jamais supprimé : en mode copie les doublons sont ignorés, depuis `À trier` ils vont dans `_Doublons`.
Chaque opération est notée dans `.pellicule\journal.tsv` (ancien chemin → nouveau nom).

## Compiler

```bash
cargo build --release                                   # pour la machine actuelle
cargo build --release --target x86_64-pc-windows-gnu    # pellicule.exe (depuis macOS : brew install mingw-w64)
cargo test
PELLICULE_CAPTURE=ecran.png cargo run --release -- --album <album>   # capture de la fenêtre (hors Windows)
```

Code : `album.rs` (index, doublons, rangement), `date.rs` (EXIF, QuickTime, noms), `gui.rs` (fenêtre),
`thumbs.rs` (miniatures), `cli.rs` (commandes).
