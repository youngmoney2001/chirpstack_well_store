# Optimisations du noyau ChirpStack

Référence : snapshot upstream `v4.20.0` (`8b5688584c60fca78ae756d90a54f681faa5bee6`).
Ce document est le **journal de bord** de nos écarts par rapport à l'amont :
chaque modification du noyau doit y être décrite (motivation, impact, risque,
vérification) pour rester rebasable sur les futures versions.

## Contrainte de l'environnement de travail

Le bac à sable ne dispose pas de `rustc` / `cargo` et n'accède pas à `crates.io` :
**ni compilation ni `make test` ne sont possibles ici**. Pour chaque patch :

- relecture du code et de l'équivalence fonctionnelle avec l'amont ;
- analyse syntaxique des fichiers Rust modifiés avec `tree-sitter-rust`
  (comparaison du nombre d'erreurs avec les fichiers amont : 0 partout) ;
- reproductible avec `scripts/check_rust_syntax.py` (optionnel, voir plus bas).

La confirmation définitive (`cargo build -p chirpstack`, `make test`,
`cargo clippy`) doit être faite sur un poste avec la toolchain `1.89.0` et les
services Docker Compose.

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
| `diff -ru` contre l'amont, fichiers Rust | 3 fichiers modifiés, patches ciblés |
| Analyse syntaxique tree-sitter (`uplink/mod.rs`, `uplink/data.rs`, `api/application.rs`) | 0 erreur, identique à l'amont |
| `cargo build -p chirpstack` / `cargo clippy` | ⏳ à faire sur un poste avec toolchain |
| `make test` (Postgres + Redis + Mosquitto) | ⏳ à faire : la suite `class_a_test`, `class_a_pr_test`, `otaa_test`, `otaa_pr_test` couvre indirectement les deux filtres |
| Tests unitaires dédiés aux deux filtres | ⏳ à ajouter (ils nécessitent de construire un `UplinkFrameSet`, donc un `PhyPayload` factice) |

Script de contrôle statique (utile tant que `cargo` n'est pas disponible) :

```bash
python3 -m venv /tmp/tsvenv
/tmp/tsvenv/bin/pip install tree-sitter tree-sitter-rust
/tmp/tsvenv/bin/python scripts/check_rust_syntax.py \
    chirpstack/src/uplink/mod.rs chirpstack/src/uplink/data.rs \
    chirpstack/src/api/application.rs
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
