# Optimisations du noyau ChirpStack

Référence : snapshot upstream `v4.20.0` (`8b5688584c60fca78ae756d90a54f681faa5bee6`).
Ce document est le **journal de bord** de nos écarts par rapport à l'amont :
chaque modification du noyau doit y être décrite (motivation, impact, risque,
vérification) pour rester rebasable sur les futures versions.

## Environnement de travail

**Toolchain installé** : Rust 1.99.0 autonome (`rustc`, `cargo`, `rust-std`,
`rustfmt`, `clippy`), sur le `PATH`. Comme `static.rust-lang.org` / `sh.rustup.rs`
/ `deb.debian.org` sont bloqués, il provient des composants officiels Rust
repackagés sur `registry.npmjs.org` via `scripts/install-rust-toolchain.sh`.
Le `rust-toolchain.toml` (1.89.0) n'est lu que par `rustup`, donc ignoré ici.

**Limite restante** : `crates.io` (`index.crates.io`, `static.crates.io`) est
inaccessible → `cargo check` s'arrête sur `no matching package named 'aes' found`
en mode `--offline`, et part en timeouts réseau sinon. `cargo build` / `make test`
nécessitent donc un `vendor/` généré sur une machine connectée, ou l'ouverture
de l'accès à crates.io dans le bac à sable.

Pour chaque patch, les vérifications réalisables ici sont :

- relecture + `diff` contre l'amont ;
- `rustfmt --edition 2024 --config skip_children=true --check` (vrai parseur Rust) ;
- `cargo metadata --no-deps --offline` (workspace et manifestes valides) ;
- compilation par `rustc` d'un harnais autonome qui reproduit les fonctions
  patchées avec des types factices : valide le **borrow checker**, l'inférence
  de types et l'équivalence comportementale avec l'algorithme amont
  (`scripts/verify_uplink_filters.rs`) ;
- analyse syntaxique `scripts/check_rust_syntax.py` en repli.

La confirmation définitive (`cargo build -p chirpstack`, `cargo clippy`,
`make test`) reste à faire sur un poste avec crates.io + les services Docker
Compose.

---

## Phase 1 — appliquée (2026-11-03)

Trois patches volontairement petits, sans changement de comportement observable.

### 1.1 `chirpstack/src/uplink/mod.rs` — filtrage des `rx_info` sans clonage

**Fonctions** : `filter_rx_info_by_tenant_id` et `filter_rx_info_by_public_only`
(appelées par `uplink/data.rs`, `uplink/data_fns.rs`, `uplink/join.rs`,
`uplink/join_fns.rs`, donc sur **tous** les chemins d'uplink, y compris le
roaming passif).

**Avant** : pour chaque trame, parcours de `rx_info_set`, et pour chaque
passerelle conservée, `rx_info.clone()` dans un nouveau `Vec` — soit un clone
profond d'un message protobuf (`UplinkRxInfo`) par passerelle receptrice. En
présence de N passerelles, N clones + N allocations par uplink.

**Après** : les identifiants de passerelle (et les drapeaux nécessaires) sont
résolus une fois dans un vecteur, puis `Vec::retain` compacte le vecteur **sur
place**. Aucun clone de `UplinkRxInfo`, aucune allocation d'un second vecteur.
Les erreurs sont toujours résolues avant toute mutation, donc un identifiant
malformé laisse `rx_info_set` intact comme auparavant.

**Bonus** : `unwrap_or_else(Uuid::new_v4)` (un UUID aléatoire généré par
passerelle pour forcer une non-correspondance de tenant) est remplacé par un
test direct sur l'`Option`. Cela supprime un appel RNG par passerelle et rend le
résultat déterministe (l'ancien code avait une probabilité ~2⁻¹²² de conserver
à tort une passerelle sans tenant).

**Impact** : chemin le plus chaud du serveur ; gain proportionnel au nombre de
passerelles recevant la même trame (typiquement 1 à 10+ en réseau dense) et au
débit d'uplinks. Risque faible : même ordre de conservation, mêmes messages
d'erreur, même comportement si le jeu filtré devient vide.

### 1.2 `chirpstack/src/uplink/data.rs` — appel dupliqué supprimé

`_handle` appelait deux fois `set_region_config()` (lignes 109 et 122 de
l'amont), sans aucune modification de `region_config_id` entre les deux —
`filter_rx_info_by_tenant()` ne touche que `rx_info_set` et `set_device_info()`
est en lecture seule. Le second appel recopiait une `String`, prenait un verrou
`RwLock` sur `REGIONS` et clonait un `Arc` pour rien, à chaque uplink.

Le premier appel est conservé (et commenté), le second supprimé. Aucun effet
fonctionnel : la fonction est idempotente.

### 1.3 `chirpstack/src/api/application.rs` — regex compilée une seule fois

`create_ifttt_integration` compilait `^[a-zA-Z0-9]+$` à **chaque requête** via
`regex::Regex::new(...).unwrap()`. Remplacé par un `LazyLock<Regex>` statique,
en suivant le motif déjà utilisé dans `storage/search.rs`
(`SEARCH_TAG_RE`). Comportement strictement identique, coût de compilation
amorti.

---

## Vérification de la phase 1

| Contrôle | Résultat |
| --- | --- |
| `diff -ru` contre l'amont, fichiers Rust | ✅ 3 fichiers modifiés, patches ciblés (cf. §1.1–1.3) |
| `rustfmt --edition 2024 --check` sur les 3 fichiers | ✅ conformes (identique à l'amont) |
| `cargo metadata --no-deps --offline` | ✅ workspace valide, 6 crates : `chirpstack`, `backend`, `chirpstack_api`, `lrwn`, `chirpstack_integration`, `lrwn_filters` |
| `rustc --edition 2024` du harnais `scripts/verify_uplink_filters.rs` | ✅ compile (borrow checker + inférence OK) et **résultats identiques à l'algorithme amont** sur 6 jeux de passerelles + cas d'erreur |
| Analyse syntaxique tree-sitter (`scripts/check_rust_syntax.py`) | ✅ 0 erreur sur les 3 fichiers |
| `cargo check -p chirpstack` | ❌ bloqué : `no matching package named 'aes' found` (`--offline`) / timeouts TLS vers `index.crates.io` |
| `cargo build`, `cargo clippy`, `make test` | ⏳ à faire sur un poste avec crates.io + Docker Compose ; `class_a_test`, `class_a_pr_test`, `otaa_test`, `otaa_pr_test` couvrent indirectement les deux filtres |
| Tests unitaires in-crate dédiés aux deux filtres | ⏳ à ajouter dès que les dépendances sont disponibles (ils nécessitent un `UplinkFrameSet`, donc un `PhyPayload` factice) |

Commandes de vérification reproductibles :

```bash
bash scripts/install-rust-toolchain.sh        # si le toolchain a disparu
cargo metadata --no-deps --offline
rustfmt --edition 2024 --config skip_children=true --check \
  chirpstack/src/uplink/mod.rs chirpstack/src/uplink/data.rs \
  chirpstack/src/api/application.rs
rustc --edition 2024 -o /tmp/verify scripts/verify_uplink_filters.rs && /tmp/verify
python3 -m venv /tmp/tsvenv && /tmp/tsvenv/bin/pip install -q tree-sitter tree-sitter-rust
/tmp/tsvenv/bin/python scripts/check_rust_syntax.py chirpstack/src/uplink/mod.rs
```

---

## Cartographie du chemin chaud

```
gateway backend (MQTT / UDP)
  └─ uplink::handle_uplink_frame            (uplink/mod.rs)
       ├─ déduplication Redis               (uplink/mod.rs)
       ├─ get_for_phypayload_and_incr_f_cnt_up / validation MIC  (storage/device.rs)
       ├─ Data::_handle                    (uplink/data.rs)  ← phase 1
       │    ├─ filter_rx_info_by_tenant   (uplink/mod.rs)   ← phase 1
       │    ├─ codec JS / Cayenne LPP     (codec/)
       │    ├─ intégrations (join_all)    (integration/mod.rs)
       │    ├─ metrics (pipeline Redis)   (storage/metrics.rs)
       │    └─ downlink::data             (downlink/data.rs)
       └─ stream / frame logs
```

**Déjà bien optimisé en amont** (ne pas refaire) :

- `integration/mod.rs` : les intégrations sont lancées en parallèle avec
  `futures::join_all`, pas en `await` séquentiels.
- `storage/metrics.rs::save` : toutes les écritures Redis sont regroupées dans
  un `redis::pipe().atomic()`.
- `region.rs` : les configs de région sont mises en cache dans des `Arc`.
- `storage/search.rs` : regex déjà en `LazyLock`.
- `monitoring/prometheus.rs` : compteurs en `LazyLock`.

---

## Backlog (à prioriser avec toi)

| # | Piste | Fichier(s) | Impact attendu | Effort / risque |
| --- | --- | --- | --- | --- |
| 2.1 | **Pool de runtimes QuickJS / cache de modules compilés** : `decode()` et `encode()` créent un `Runtime::new()` + `Context::full()` et recompilent le script du codec à **chaque trame**. Un cache indexé par hash du script (avec pool de contexts et reset de l'interrupt handler) éviterait cette initialisation répétée. | `chirpstack/src/codec/js/mod.rs` | **Élevé** pour les déploiements avec codec JS (cas typique d'un projet de capteurs) | Élevé / attention à l'isolation entre devices et au timeout d'exécution |
| 2.2 | Éviter `sessions.push(ds.clone())` pour chaque device candidat lors de la validation MIC : travailler par référence et ne cloner la session que sur succès. | `chirpstack/src/storage/device.rs` (~l. 347) | Moyen, proportionnel au nombre de devices partageant un DevAddr | Moyen |
| 2.3 | Réduire les allers-retours Redis de la déduplication / du verrou Class-A (regrouper en pipeline ou Lua). | `chirpstack/src/uplink/mod.rs`, `chirpstack/src/downlink/scheduler.rs` | Moyen sur réseau dense | Moyen / risque de régression sur la concurrence |
| 2.4 | Ajouter des métriques de latence par étape du pipeline d'uplink (histogrammes Prometheus) pour mesurer avant/après au lieu d'estimer. | `chirpstack/src/monitoring/` | Indispensable pour valider 2.1–2.3 | Faible |
| 2.5 | **Améliorations métier** (à définir) : codec / décodage spécifique aux puits, mesures de niveau & pompe, alertes, stockage long terme, intégration HTTP/MQTT dédiée, multi-tenant par site. | `chirpstack/src/codec/`, `integration/`, `api/`, migrations | Fonctionnel | À cadrer avec toi |
| 2.6 | Mise à jour vers une version amont plus récente en réappliquant ce journal. | `UPSTREAM.md` | Maintenance | Faible |

## Méthode de validation des prochaines phases

1. Poser une mesure de référence (`uplink_count`, latence de traitement, CPU,
   commandes Redis/s) sur un jeu de trames rejoué.
2. Un patch à la fois, avec son entrée dans ce journal.
3. `make test` complet + benchmark comparatif avant de passer au suivant.

> **Point de cadrage** : les pistes 2.1–2.4 sont des optimisations de
> performance génériques. Si tes « améliorations » sont fonctionnelles (le nom
> du dépôt suggère un cas d'usage « puits / stockage de mesures »), dis-moi
> lesquelles et je pars directement sur 2.5.
