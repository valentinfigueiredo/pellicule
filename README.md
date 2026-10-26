# Pellicule

Un seul album pour toutes tes photos et vidéos, classé et renommé par date, sans doublons.
Un seul fichier `pellicule.exe` (~6 Mo, écrit en Rust), rien à installer.

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

Miniatures : JPEG, PNG, WebP, GIF, BMP et TIFF sont décodés par Pellicule. Pour les photos iPhone
(HEIC), les RAW et les vidéos, Pellicule demande la miniature à Windows : il faut les extensions
gratuites « HEIF Image Extensions » et « HEVC Video Extensions » du Microsoft Store.
Si un aperçu manque, la raison est notée dans `.pellicule\apercus.log`, et
`pellicule apercu <fichier>` (dans un terminal) explique ce qui coince.

## Carte

Onglet « Carte » en haut : tes photos sur une carte, regroupées par lieu, comme dans l'app Photos.
Glisser pour se déplacer, molette (ou pincer) pour zoomer, double-clic pour zoomer sur un endroit.
Clic sur une bulle : ses photos s'affichent à droite. Bouton « Tout » : voir toutes les photos.

- La position vient du GPS enregistré dans la photo (EXIF) ou la vidéo (iPhone et Android).
  Les photos sans GPS (envoyées par WhatsApp, captures d'écran…) n'apparaissent pas sur la carte.
- Pellicule lit les positions en arrière-plan au premier lancement, puis les garde dans `.pellicule\lieux.tsv`.
- Fond de carte © les contributeurs d'OpenStreetMap. Les tuiles affichées sont gardées dans
  `.pellicule\carte` (consultables hors ligne). Afficher la carte demande à openstreetmap.org
  les zones regardées — jamais tes photos ni leurs positions.

## iCloud Photos

Bouton « Connecter iCloud » en haut de la fenêtre : identifiant Apple, mot de passe, puis le code à
6 chiffres reçu sur l'iPhone. Pellicule récupère alors **toute la photothèque iCloud** (originaux),
puis les nouveautés toutes les 30 minutes tant qu'elle est ouverte. Les fichiers arrivent dans
`À trier` et sont rangés comme le reste (doublons écartés).

- Connexion **non officielle** (la même que le site icloud.com, reprise d'icloudpd) : Apple peut la
  casser un jour ; la mise à jour de Pellicule la réparera.
- Sur l'iPhone : Réglages > ton nom > iCloud : **activer** « Accéder aux données iCloud sur le web »
  et **désactiver** « Protection avancée des données ».
- Le mot de passe n'est jamais enregistré. La session est gardée dans `.pellicule\icloud\session.bin`,
  chiffrée par Windows pour ton compte. Les photos déjà récupérées sont notées dans
  `deja-recuperees.txt` ; `journal.log` liste les échanges avec Apple (sans aucun secret) en cas de panne.
- Récupérées : les originaux (photos, vidéos). Pas les retouches, ni les vidéos des Live Photos,
  ni la photothèque partagée.

Mise à jour : quand une nouvelle version est publiée sur GitHub, un bouton « Mettre à jour » apparaît
en haut de la fenêtre. Un clic télécharge la nouvelle version et relance Pellicule
(ça ne marche que si le dépôt GitHub est public).

En ligne de commande (depuis un terminal) :

```
pellicule importer D:\DCIM E:\Photos      copie dans l'album
pellicule importer --essai D:\DCIM        montre ce qui serait fait, sans rien toucher
pellicule importer --deplacer D:\DCIM     déplace au lieu de copier
pellicule trier                           range « À trier » une fois
pellicule surveiller                      range « À trier » en continu
pellicule album D:\MonAlbum               change l'emplacement de l'album
pellicule apercu D:\photo.heic            explique pourquoi un aperçu ne s'affiche pas
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
# + PELLICULE_CARTE=1 (ou =selection) pour la carte, PELLICULE_ICLOUD=1 pour la fenêtre iCloud
```

Code : `album.rs` (index, doublons, rangement), `date.rs` (EXIF, QuickTime, noms), `gui.rs` (fenêtre),
`thumbs.rs` (miniatures), `map.rs` + `place.rs` (carte, GPS), `icloud/` (iCloud Photos),
`update.rs` (mise à jour), `cli.rs` (commandes).

Publier une version : augmenter `version` dans `Cargo.toml`, recompiler `dist/pellicule.exe`, pousser sur `master`.
