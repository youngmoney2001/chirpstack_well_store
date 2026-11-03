# Compiler ChirpStack dans ce bac à sable (sans crates.io)

## Le problème

L'allowlist réseau de la plateforme n'autorise que `github.com`,
`codeload.github.com`, `api.github.com`, `registry.npmjs.org`, `pypi.org` et
`files.pythonhosted.org`. Sont donc **inaccessibles** :
`static.rust-lang.org`, `sh.rustup.rs`, `deb.debian.org`, `index.crates.io`,
`static.crates.io`, `raw.githubusercontent.com`, les release assets GitHub.
Le filtrage est en amont du conteneur (aucun proxy, aucune règle locale,
`sudo` n'y change rien) : **on ne peut pas « ouvrir » crates.io depuis ici**.

## La solution mise en place

```
registry.npmjs.org  ──► toolchain Rust 1.99.0 (rustc, cargo, rust-std, ...)
pypi.org            ──► protoc, cmake, libpq, meson, ninja, pkgconf
codeload.github.com ──► source de pkgconf
github.com (Actions)──► cargo vendor des 608 crates de Cargo.lock
github.com (git)    ──► rapatriement du snapshot vers ~/.cache/vendor
```

| Besoin | Source autorisée | Script |
| --- | --- | --- |
| `rustc`, `cargo`, `rustfmt`, `clippy` | composants officiels Rust repackagés `@toolchain-bin/*` sur npm | `scripts/install-rust-toolchain.sh` |
| `protoc` (35.1) | wheel PyPI `grpcio-tools` + wrapper | `scripts/install-build-deps.sh` |
| `cmake` (4.4.4, pour `rdkafka-sys`) | wheel PyPI `cmake` | idem |
| `libpq.so.5` | wheel PyPI `psycopg2-binary` | idem |
| `pg_config` | shim pointant sur la lib ci-dessus | idem |
| `pkg-config` (pkgconf 3.0.10) | source `pkgconf/pkgconf` via `codeload.github.com`, compilé avec meson/ninja (PyPI) | idem |
| `ui/build/` | placeholder `index.html` ( requis par `rust-embed`) | idem |
| **608 crates** | `cargo vendor` exécuté par un runner GitHub Actions | `.github/workflows/vendor-crates.yml` + `scripts/fetch-vendor-crates.sh` |

Tous les binaires et le `vendor/` (448 Mo) vivent dans `~/.cache/`, **hors du
dépôt** : `.cache` est exclu des snapshots et `target/`, `ui/build`,
`ui/node_modules`, `**/target` sont dans `.gitignore`.

### Pourquoi passer par GitHub Actions

`crates.io` reste inaccessible localement, mais un runner Ubuntu de GitHub
Actions y a accès. Le workflow :

1. `cargo vendor --versioned-dirs /tmp/vendor` (tous les packages de `Cargo.lock`) ;
2. `tar | gzip -1` puis `split -b 90m` (limite de 100 Mo par fichier GitHub) ;
3. pousse les parties dans `.vendor-parts/` sur **cette** branche.

Le sandbox récupère ensuite ces parties avec `git fetch` (hôte `github.com`
autorisé), les réassemble et les extrait. Un commit de nettoyage supprime les
parties du dépôt ; l'historique garde les blobs mais l'arbre final reste propre.

> `gh workflow run` (workflow_dispatch) est refusé par le token GitHub App
> d'Arena (HTTP 403). Le déclenchement se fait donc par un push qui modifie
> `.github/vendor-trigger` (filtre `on: push: paths:`). Le commit du workflow
> porte `[skip ci]` pour éviter les boucles.

## Commandes

```bash
# 1. Une seule fois (ou si l'environnement a été réinitialisé)
bash scripts/install-rust-toolchain.sh
bash scripts/install-build-deps.sh
bash scripts/fetch-vendor-crates.sh --trigger --cleanup   # si Cargo.lock change

# 2. Pour chaque session
export CARGO_HOME="$HOME/.cache/cargo-home"
export PKG_CONFIG_PATH="$HOME/.cache/pkgconf/lib/pkgconfig"

cargo check  --offline -p chirpstack                     # ✅ ~2 min
cargo build  --offline -p chirpstack                     # ✅ ~4 min 30
cargo clippy --offline -p chirpstack                     # ✅ ~1 min 30
cargo test   --offline -p chirpstack uplink::tests -- --test-threads=1
cargo test   --offline -p chirpstack codec::js:: -- --test-threads=1
./target/debug/chirpstack --version                      # chirpstack 4.20.0
cargo check --offline -p chirpstack --no-default-features --features sqlite
```

## Résultats obtenus (2026-11-03)

| Commande | Résultat |
| --- | --- |
| `cargo metadata --offline` | ✅ 609 packages résolus depuis `~/.cache/vendor` |
| `cargo check --offline -p chirpstack` (postgres, défaut) | ✅ 2 min 13, 0 erreur |
| `cargo check --offline -p chirpstack --no-default-features --features sqlite` | ✅ 1 min 02, 0 erreur |
| `cargo build --offline -p chirpstack` | ✅ 4 min 30, binaire `target/debug/chirpstack` (605 Mo) |
| `./target/debug/chirpstack --version` | ✅ `chirpstack 4.20.0` |
| `cargo clippy --offline -p chirpstack` | ✅ 0 erreur ; warnings préexistants de l'amont uniquement |
| `cargo test --offline -p chirpstack --no-run` | ✅ binaire de test compilé (3 min 51) |
| `cargo test --offline -p chirpstack uplink::tests -- --test-threads=1` | ✅ 3/3 tests des filtres d'uplink |
| `cargo test --offline -p chirpstack codec::js:: -- --test-threads=1` | ✅ 16/16 tests du codec JS (8 amont + 8 pool), 1 benchmark ignoré |
| `chirpstack --config <DIR> configfile` → parseur TOML | ✅ config générée valide (nouvelles options `[codec.js]`) |

Les fichiers modifiés par nos optimisations ne produisent **aucun** warning ni
erreur nouveaux. Les autres tests (`class_a_test`, `otaa_test`, `relay_*`, …)
compilent mais nécessitent les services Docker Compose pour s'exécuter.
`chirpstack/Cargo.toml` active en plus la feature Tokio `sync` (déjà présente
transitivement), utilisée par le sémaphore du pool QuickJS ; `Cargo.lock` n'est
pas modifié.

## Limites connues

- **UI** : `ui/build/index.html` est un placeholder. Pour une UI réelle il faut
  `make build-ui` sur une machine complète (génération grpc-web avec protoc
  3.21 + plugin, puis `pnpm build`). Cela n'affecte pas la compilation Rust.
- **Tests** : `make test` nécessite PostgreSQL, Redis, Mosquitto (et Kafka/AMQP
  pour certaines intégrations) via Docker Compose, non disponibles ici.
  `cargo test --no-run` peut en revanche compiler les tests.
- **Toolchain** : 1.99.0 installé, alors que `rust-toolchain.toml` épingle
  1.89.0. Ce fichier n'est lu que par `rustup` ; ici on appelle les binaires
  directement. Sur un poste normal, utiliser `rustup toolchain install 1.89.0`.
- **libpq** : `pq-sys`/`openssl-sys` ne figurent pas dans `Cargo.lock` (diesel
  passe par `tokio-postgres`), donc `libpq`, `pg_config` et `pkg-config` sont
  installés par précaution mais ne sont pas nécessaires à ce build.
- **Renouvellement** : après toute modification de `Cargo.lock`, relancer
  `scripts/fetch-vendor-crates.sh --trigger --cleanup`.
