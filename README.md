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
  Un pool de runtimes par thread de blocking Tokio + un cache de scripts
  compilés fait passer le micro-benchmark de ~5,3 ms à ~0,12 ms par décodage
  (≈ ×40). Timeout par appel, isolation des entrées et reprise après erreur sont
  conservés et testés.

Nouvelles options `[codec.js]` (facultatives, valeurs par défaut indiquées) :

```toml
[codec.js]
max_execution_time="100ms"
cache_scripts=true        # false = comportement amont (runtime neuf par appel)
max_cached_scripts=64     # scripts compilés conservés par worker
```

`cache_scripts=false` est à utiliser si un codec s'appuie sur la remise à zéro
de variables déclarées hors de `decodeUplink` / `encodeDownlink`.

Tests rapides :

```bash
cargo test --offline -p chirpstack codec::js:: -- --test-threads=1
cargo test --offline -p chirpstack codec::js::bench -- --ignored --nocapture
```

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
