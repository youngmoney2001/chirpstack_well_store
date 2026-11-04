# chirpstack_well_store

Fork de travail du **noyau ChirpStack** (LoRaWAN Network Server) destiné à
accueillir et expérimenter nos propres améliorations du cœur du serveur.

Le dépôt contient un snapshot complet et modifiable du code source officiel
open source, installé **à la racine** : les chemins sont donc identiques à ceux
de l'upstream (`chirpstack/src/...`), ce qui rend les diffs, la relecture et le
report des modifications vers une nouvelle version beaucoup plus simples.

## Base de travail

| Élément | Valeur |
| --- | --- |
| Projet amont | https://github.com/chirpstack/chirpstack |
| Version | **v4.20.0** (dernière version stable) |
| Commit | `8b5688584c60fca78ae756d90a54f681faa5bee6` |
| Licence | MIT (voir [`LICENSE`](LICENSE)) |
| Langage | Rust, édition 2024, toolchain `1.89.0` |
| Type | Workspace Cargo (monorepo) |

Voir [`UPSTREAM.md`](UPSTREAM.md) pour la provenance exacte, les rares fichiers
écartés du snapshot et la procédure de resynchronisation avec une nouvelle
version amont.

## Où se trouve quoi

| Chemin | Rôle |
| --- | --- |
| `chirpstack/` | **Le serveur** (binaire + toute la logique du noyau) |
| `chirpstack/src/uplink/` | Réception / déduplication / validation MIC / décodage des trames montantes (chemin chaud) |
| `chirpstack/src/downlink/` | Planification et construction des trames descendantes |
| `chirpstack/src/storage/` | Accès PostgreSQL / SQLite / Redis, migrations |
| `chirpstack/src/integration/` | Sorties MQTT, HTTP, Kafka, InfluxDB, AWS, … |
| `chirpstack/src/api/` | gRPC + authentification / validation des droits |
| `chirpstack/src/codec/` | Cayenne LPP et codec JavaScript (QuickJS) — pool de runtimes phase 2.1 dans `codec/js/mod.rs` |
| `lrwn/`, `lrwn-filters/`, `backend/` | Bibliothèques LoRaWAN, filtres, backend roaming |
| `api/` | Définitions protobuf + clients générés (Go, Python, JS, …) |
| `ui/` | Interface web React |
| `docs/OPTIMIZATIONS.md` | **Audit, modifications appliquées et feuille de route** |
| `docs/OFFLINE-BUILD.md` | Build hors ligne (vendor via GitHub Actions, dépendances natives, résultats) |
| `docs/upstream/` | README amont et CI amont archivés |
| `.github/workflows/vendor-crates.yml` | Génère le snapshot `cargo vendor` hors ligne |
| `scripts/` | Installation toolchain + dépendances natives, récupération du vendor, contrôles |

Le crate serveur représente à lui seul ~88 500 lignes de Rust.

## Construire et tester

L'upstream utilise Nix + Docker Compose pour les services (PostgreSQL, Redis,
Mosquitto, Kafka…). Les commandes habituelles, depuis la racine :

```bash
make docker-devshell      # environnement de développement (si Nix n'est pas installé)
make test                 # suite de tests (Postgres + Redis + Mosquitto requis)
make build-ui             # compilation de l'interface
cargo build -p chirpstack # binaire serveur
./chirpstack -c chirpstack/configuration/chirpstack.toml
```

Le README amont détaillant ces étapes est conservé dans
[`docs/upstream/README.md`](docs/upstream/README.md).

### Toolchain Rust dans ce bac à sable

Un toolchain autonome **Rust 1.99.0** (`rustc`, `cargo`, `rust-std`, `rustfmt`,
`clippy`) est installé et disponible sur le `PATH`. Comme `static.rust-lang.org`,
`sh.rustup.rs` et `deb.debian.org` sont bloqués ici, l'installation passe par les
composants officiels Rust repackagés sur `registry.npmjs.org` (hôte autorisé) :

```bash
bash scripts/install-rust-toolchain.sh     # réinstalle si besoin (~600 Mo, dans ~/.cache)
cargo --version && rustc --version
```

> `rust-toolchain.toml` épingle **1.89.0**, mais ce fichier n'est lu que par le
> proxy `rustup`. Ici on appelle les binaires réels directement, donc l'épingle
> est ignorée. Sur une machine normale, préférer `rustup toolchain install 1.89.0`.

`crates.io` reste **inaccessible** depuis le sandbox (filtrage plateforme, non
modifiable ici), mais la compilation complète fonctionne grâce à un snapshot
`cargo vendor` produit par GitHub Actions et rapatrié via `git` :

```bash
bash scripts/install-rust-toolchain.sh   # toolchain
bash scripts/install-build-deps.sh       # protoc, cmake, libpq, pkg-config, ui/build
bash scripts/fetch-vendor-crates.sh      # 608 crates -> ~/.cache/vendor

export CARGO_HOME="$HOME/.cache/cargo-home"
cargo check  --offline -p chirpstack     # ✅ 0 erreur
cargo build  --offline -p chirpstack     # ✅ binaire target/debug/chirpstack
cargo clippy --offline -p chirpstack     # ✅ 0 erreur
```

Voir [`docs/OFFLINE-BUILD.md`](docs/OFFLINE-BUILD.md) pour l'architecture
détaillée, le déclenchement du workflow, les limites (UI placeholder, tests
nécessitant Docker Compose) et les résultats mesurés.

Le détail des modifications du noyau est dans
[`docs/OPTIMIZATIONS.md`](docs/OPTIMIZATIONS.md).

## Optimisations déjà appliquées

- **Phase 1** : filtrage des `rx_info` sans clone de messages protobuf,
  suppression d'un appel dupliqué à `set_region_config()`, regex IFTTT compilée
  une seule fois.
- **Phase 2.1** : le codec JavaScript ne crée plus un runtime QuickJS par trame.
  Un **pool dédié de N threads OS** + un cache de scripts compilés (un
  `Runtime + Context` par script, LRU bornée) fait passer le micro-benchmark
  `--release` d'environ 3,9 ms (realm neuf par appel) à ~0,15 ms par décodage,
  soit ≈ ×25 (voir les chiffres dans
  [`docs/OPTIMIZATIONS.md`](docs/OPTIMIZATIONS.md)).
- **Durcissement 2.1 (sécurité)** : clé de cache = tenant + device profile +
  hash du script (avec comparaison du texte complet), isolation des realms,
  limites mémoire/stack, GC, recyclage des workers, erreurs propres sur OOM /
  débordement de pile. Un codec qui modifie `Object.prototype` ou `JSON.parse`
  ne peut plus affecter un autre profil ou un autre tenant.
- **Revue A/B/C de la 2.1 (sécurité)** : fin du `spawn_blocking` +
  `thread_local!` (qui pouvait créer jusqu'à 512 workers) au profit de N threads
  dédiés alimentés par une **file bornée** (attente bornée, rejet seulement en
  vraie saturation) ; les limites mémoire/stack/GC/`memory_usage`
  deviennent **réellement par script** ; après un panic attrapé, `mem::forget`
  + log + métrique `codec_js_panics_total` au lieu du `Drop` d'un runtime
  potentiellement corrompu ; deadline armée **après** les modules vendored et
  **avant** la compilation du script utilisateur ; ajout de `max_script_bytes`;
  benchmarks refaits en `--release`.

- **Ajustements de la revue (sécurité)** : routage `hash(clé) % N` (un canal
  par thread, un script compilé **une seule fois**), **éviction sur budget
  mémoire réel** par thread, `max_cached_scripts` relevé à 64, métriques
  hits/misses/évictions, et **attente bornée** (`queue_wait_timeout="1s"`)
  au lieu du rejet immédiat. Un panic attrapé ne tue pas le thread. Voir
  [`docs/OPTIMIZATIONS.md`](docs/OPTIMIZATIONS.md) pour le devenir exact d'un
  uplink dont le décodage échoue.

**Borne mémoire totale du codec JS** :
`workers × max_cache_memory_bytes` = 2 × 32 Mio = **64 Mio** avec les valeurs
par défaut sur ce bac à sable (2 cœurs). Chaque script reste plafonné à
`max_memory_bytes` (4 Mio) et chaque thread à 64 realms. Sur un hôte très
multicœur, **fixer `workers` explicitement**.

Options `[codec.js]` (facultatives, valeurs par défaut indiquées) :

```toml
[codec.js]
max_execution_time="100ms"
cache_scripts=true        # false = comportement amont (realm neuf par appel)
max_cached_scripts=64       # realms max par thread (LRU)
workers=0                   # 0 = available_parallelism()
queue_capacity=0            # 0 = 256 PAR thread (total = N * 256)
queue_wait_timeout="1s"     # attente bornée ; rejet seulement en saturation réelle
max_script_bytes=262144     # taille max du script généré (wrapper inclus)
max_cache_memory_bytes=33554432  # budget réel PAR thread (somme memory_usage)
max_memory_bytes=4194304    # limite QuickJS PAR SCRIPT (0 = illimité)
max_stack_bytes=524288
max_calls_per_worker=10000
gc_every_calls=1            # 1 realm par Runtime : le GC à chaque appel est bon marché
```

`cache_scripts=false` est à utiliser si un codec s'appuie sur la remise à zéro
de variables déclarées hors de `decodeUplink` / `encodeDownlink`.

Tests rapides :

```bash
cargo test --offline -p chirpstack codec::js:: -- --test-threads=1
# benchmarks (ajouter --release pour des chiffres de production) :
cargo test --offline -p chirpstack codec::js::bench -- --ignored --nocapture --test-threads=1
```

## Piste 2.2 — reconnaissance sécurité (étape 1)

Le rapport de reconnaissance, **sans aucune modification de code**, est dans
[`docs/SECURITY-2.2-RECON.md`](docs/SECURITY-2.2-RECON.md) : chaîne de
résolution d'un uplink, modèle de menace (DoS sur DevAddr, rejeu / FCnt,
concurrence, MIC en temps constant), mesures du coût MIC par nombre de
candidats et propositions classées. L'implémentation reste à valider.

## Méthode de travail

1. Toute modification du noyau est consignée dans `docs/OPTIMIZATIONS.md`
   (fichier, motivation, impact attendu, risque, vérification).
2. On garde les patches petits et ciblés pour rester « rebasables » sur les
   futures versions amont.
3. Aucun artefact de build, aucun jeu de données volumineux n'est versionné.

## Licence

Code amont sous licence MIT — Copyright (c) 2022 Orne Brocaar.
La licence est conservée telle quelle dans [`LICENSE`](LICENSE) ; les
modifications apportées dans ce dépôt restent soumises aux mêmes conditions.
