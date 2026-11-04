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

**`crates.io` reste inaccessible**, mais ce n'est plus un bloqueur : un snapshot
`cargo vendor` des 608 crates de `Cargo.lock` est produit par un runner GitHub
Actions (qui a accès à crates.io), rapatrié via `git` et extrait dans
`~/.cache/vendor`. Les dépendances natives (`protoc`, `cmake`, `libpq`,
`pkg-config`) viennent de PyPI / npm / codeload. Voir
[`docs/OFFLINE-BUILD.md`](OFFLINE-BUILD.md) pour la procédure complète.

Conséquence : **`cargo check`, `cargo build` et `cargo clippy` tournent
réellement ici** en `--offline`, avec le workspace complet. Pour chaque patch :

- relecture + `diff` contre l'amont ;
- `rustfmt --edition 2024 --config skip_children=true --check` (vrai parseur Rust) ;
- `cargo check --offline -p chirpstack` (postgres **et** sqlite) — compilation
  typée de tout le noyau ;
- `cargo build --offline -p chirpstack` + exécution du binaire ;
- `cargo clippy --offline -p chirpstack` ;
- compilation par `rustc` d'un harnais autonome qui reproduit les fonctions
  patchées avec des types factices : **borrow checker**, inférence de types et
  équivalence comportementale avec l'algorithme amont
  (`scripts/verify_uplink_filters.rs`) ;
- analyse syntaxique `scripts/check_rust_syntax.py` en repli ;
- benchmarks `--release` (avec `CARGO_PROFILE_RELEASE_LTO=off` dans ce bac à
  sable, voir `docs/OFFLINE-BUILD.md`).

Seul `make test` reste à faire sur un poste avec les services Docker Compose
(PostgreSQL, Redis, Mosquitto…). L'UI est un placeholder `ui/build/index.html`
(voir `docs/OFFLINE-BUILD.md`).

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
| `cargo metadata --offline` | ✅ 609 packages résolus depuis `~/.cache/vendor` |
| `rustc --edition 2024` du harnais `scripts/verify_uplink_filters.rs` | ✅ compile (borrow checker + inférence OK) et **résultats identiques à l'algorithme amont** sur 6 jeux de passerelles + cas d'erreur |
| Analyse syntaxique tree-sitter (`scripts/check_rust_syntax.py`) | ✅ 0 erreur sur les 3 fichiers |
| `cargo check --offline -p chirpstack` (postgres, défaut) | ✅ 2 min 13, **0 erreur** |
| `cargo check --offline -p chirpstack --no-default-features --features sqlite` | ✅ 1 min 02, **0 erreur** |
| `cargo build --offline -p chirpstack` | ✅ 4 min 30 ; `./target/debug/chirpstack --version` → `chirpstack 4.20.0` |
| `cargo clippy --offline -p chirpstack` | ✅ 0 erreur ; warnings préexistants de l'amont uniquement, **aucun dans nos 3 fichiers** |
| Tests unitaires in-crate `uplink::tests` (3 tests) | ✅ 3/3 : équivalence avec l'algorithme amont (tenant + public) et non-modification du set en cas d'identifiant invalide |
| `make test` (215 autres tests : class_a, otaa, …) | ⏳ nécessite PostgreSQL / Redis / Mosquitto via Docker Compose, indisponible ici |

Commandes de vérification reproductibles (voir [`OFFLINE-BUILD.md`](OFFLINE-BUILD.md)) :

```bash
bash scripts/install-rust-toolchain.sh        # si le toolchain a disparu
bash scripts/install-build-deps.sh            # protoc, cmake, libpq, pkg-config, ui/build
bash scripts/fetch-vendor-crates.sh           # 608 crates -> ~/.cache/vendor
export CARGO_HOME="$HOME/.cache/cargo-home"

rustfmt --edition 2024 --config skip_children=true --check \
  chirpstack/src/uplink/mod.rs chirpstack/src/uplink/data.rs \
  chirpstack/src/api/application.rs
cargo check --offline -p chirpstack
cargo clippy --offline -p chirpstack
cargo test --offline -p chirpstack uplink::tests -- --test-threads=1
rustc --edition 2024 -o /tmp/verify scripts/verify_uplink_filters.rs && /tmp/verify
```

---

## Phase 2.1 — appliquée (2026-11-03) : pool de runtimes QuickJS

### Le problème, mesuré avant de coder

`chirpstack/src/codec/js/mod.rs::decode()` et `encode()` créaient **à chaque
trame** :

- un `rquickjs::Runtime::new()` + un `Context::full()` (moteur JS, classes
  standard, GC) ;
- un resolver/loader `base64-js` / `ieee754` / `buffer` ;
- un `Module::declare(ctx, "main", script)` + `eval()` + `promise.finish()`,
  c'est-à-dire la compilation du script du device **et** des ~30 Kio de modules
  vendored à chaque uplink.

Le handler d'interruption était armé *avant* `Runtime::new()` : le budget
`max_execution_time` (100 ms par défaut) incluait donc toute cette
initialisation. À froid, `codec::js::test::test_decode` échouait de façon
intermittente avec `Exception generated by QuickJS` **sur le code amont, avant
toute modification** (7 tests sur 8 seulement).

Benchmark de référence ajouté au dépôt (200 décodes séquentiels d'un codec
utilisant `Buffer`, binaire de test debug, 3 runs) :
**4,82 / 5,50 / 5,58 ms par appel**.

### La solution

Trois changements, tous dans `chirpstack/src/codec/js/mod.rs` :

1. **Un runtime par thread, plus un par trame.** `execute()` prend un permis sur
   un `tokio::sync::Semaphore` dimensionné par `std::thread::
   available_parallelism()` et envoie le travail CPU-bound dans
   `spawn_blocking()` (les workers Tokio ne sont plus bloqués). Un
   `thread_local!` conserve un `Worker { Runtime, Context, cache }` par thread
   de blocking : QuickJS n'est pas thread-safe, donc aucun contexte ne quitte
   son thread d'origine. Le sémaphore borne du même coup le nombre de runtimes
   vivants.
2. **Le script n'est compilé qu'une fois par (worker, script).** Le wrapper ne
   fait plus `export default decodeUplink(chirpstack_input)` au niveau racine ;
   il exporte une fonction `__chirpstack_codec_entry(input)`. Cette fonction est
   détachée de la closure `ctx.with(...)` via `rquickjs::Persistent<Function>`
   et stockée dans une `HashMap` indexée par hash du script (le texte complet
   est gardé dans l'entrée pour qu'une collision de hash soit inoffensive).
   Les modules `buffer` / `ieee754` / `base64-js` ne sont compilés qu'une fois
   par runtime par le loader QuickJS. Les noms de modules sont rendus uniques
   par un compteur, afin qu'une éviction du cache ne puisse pas faire
   re-déclarer un nom déjà enregistré.
3. **Isolation et timeout conservés.**
   - L'objet `input` est reconstruit à chaque appel et le global historique
     `chirpstack_input` est réécrit à chaque appel (compatibilité avec les
     codecs qui le lisent directement) ; il est aussi positionné avant la
     première compilation, comme en amont.
   - La deadline est **rafraîchie hors de `ctx.with`** : `Context::with`
     verrouille le mutex du runtime, donc appeler `set_interrupt_handler`
     dedans provoquerait un deadlock. Deux phases : la compilation est
     protégée, puis l'appel du code utilisateur reçoit un budget
     `max_execution_time` complet. La création du runtime n'est plus imputée
     sur ce budget, ce qui supprime le test flaky.
   - Après **toute** erreur (exception JS ou timeout), le worker est jeté et
     reconstruit à l'appel suivant : un runtime empoisonné n'est jamais réutilisé
     pour un autre device.
   - `Drop for Worker` vide le cache de `Persistent` **avant** la destruction
     du runtime (rquickjs abort le processus dans le cas contraire).

### Configuration

> **État actuel** : ce tableau décrit l'état du commit `54efb7b`. Le
> durcissement décrit plus loin (« Durcissement sécurité de la 2.1 ») étend la
> configuration et remplace plusieurs de ces choix ; s'y référer pour la
> configuration en vigueur.

Deux options ajoutées à `[codec.js]` (`config.rs` + template `configfile.rs`),
avec `#[serde(default)]` donc sans impact sur les fichiers de config existants :

| Option | Défaut | Rôle |
| --- | --- | --- |
| `cache_scripts` | `true` | Réutiliser runtimes et scripts compilés. `false` = comportement amont (runtime neuf par appel), nécessaire pour les codecs qui comptent sur la remise à zéro de l'état de haut niveau. |
| `max_cached_scripts` | `64` | Nombre maximal de scripts compilés conservés par worker (borné à 1 minimum). |

`chirpstack --config <DIR> configfile` a été exécuté : le TOML généré est
valide et contient bien `cache_scripts=true` / `max_cached_scripts=64`.

### Résultat mesuré

Même benchmark, 3 runs après le patch : **0,113 / 0,149 / 0,108 ms par appel**
(200 appels en 22,5 / 29,8 / 21,6 ms contre ~1,05 s en amont), soit environ
**×40**. Le premier appel d'un script reste au coût d'une compilation ; les
suivants paient seulement la construction de l'objet d'entrée et l'appel JS.

### Vérification

| Contrôle | Résultat |
| --- | --- |
| `rustfmt --edition 2024 --config skip_children=true --check` | ✅ conforme |
| `cargo check --offline -p chirpstack` | ✅ 0 erreur |
| `cargo build --offline -p chirpstack` | ✅ binaire 4.20.0 reconstruit |
| `cargo clippy --offline -p chirpstack` | ✅ 0 erreur, **aucun warning dans `codec/js/mod.rs`** |
| `chirpstack --config <DIR> configfile` → `tomllib` | ✅ TOML valide, nouvelles options rendues |
| 8 tests amont `codec::js::test` | ✅ 8/8 (dont `test_decode` qui échouait par intermittence avant le patch) |
| 8 nouveaux tests `codec::js::pool_test` | ✅ 8/8 : entrée/global rafraîchis, scripts multiples en cache, reprise après erreur, timeout rafraîchi sur script **déjà en cache**, 16 décodes concurrents, réutilisation vs mode legacy, borne du cache |
| `cargo test --offline -p chirpstack codec::js::` en parallèle | ✅ 16 passés, 1 benchmark ignoré |
| `uplink::tests` (non-régression phase 1) | ✅ 3/3 |
| `make test` complet | ⏳ toujours conditionné aux services Docker Compose |

Reproduire la mesure :

```bash
cargo test --offline -p chirpstack codec::js::bench -- --ignored --nocapture
```

### Risques et limites

- **Différence sémantique** : avec `cache_scripts=true`, l'état de haut niveau
  d'un codec (`var counter = 0;` en dehors de la fonction) persiste entre
  appels, alors que l'amont repartait de zéro. Le comportement amont est
  restaurable par `cache_scripts=false`. **Le risque inter-tenants a été
  supprimé par le durcissement ci-dessous** (un `Context` et une clé de cache
  par tenant + profil) ; l'état ne persiste plus que pour un même triplet
  `(tenant, profil, script)`.
- **Empreinte mémoire** : jusqu'à `available_parallelism()` runtimes ×
  `max_cached_scripts` scripts compilés. Ajuster `max_cached_scripts` si le
  nombre de profils de devices est très inférieur à la valeur par défaut.
- **`max_execution_time`** ne s'applique plus à la création du runtime, mais
  toujours à la compilation du script et à l'exécution du code utilisateur
  (deux deadlines distinctes). C'est plus généreux qu'en amont, pas moins sûr.
- Mesures faites sur un binaire **debug** et un seul codec simple ; le gain en
  production dépend du nombre de scripts distincts et du taux de hits du cache.
- Un cache par worker signifie qu'un même script peut être compilé sur
  plusieurs threads : c'est voulu (pas de verrou global), mais à prendre en
  compte pour les codecs à état.

---

## Durcissement sécurité de la 2.1 — appliqué (2026-11-03)

Objectif : **corriger des failles**, pas seulement gagner en performance. Le
commit `54efb7b` (pool + cache de scripts) laissait trois problèmes
d'isolation / de ressources.

| # | Faille dans `54efb7b` | Correction |
| --- | --- | --- |
| 1 | Un seul `Context` par worker partagé par tous les scripts en cache : un codec modifiant `Object.prototype`, `JSON.parse`, `Buffer` ou `Array.prototype` contaminait les autres profils/tenants. | Un `Context::full()` **par entrée de cache** (même Runtime) ; l'entrée contient `(Context, Persistent<Function>)`. |
| 2 | Clé = hash du script seul : deux tenants avec le même texte partageaient la fonction compilée et son état global. | Clé = `(tenant_id, device_profile_id, hash(script))`, identité remontée depuis les appelants ; vérification du texte complet conservée. |
| 3 | Aucune limite mémoire/stack, ni GC, ni recyclage ; un OOM pouvait panic. | `set_memory_limit` / `set_max_stack_size`, GC périodique, recyclage après N appels ou seuil mémoire, `catch_unwind` + erreur propre + worker jeté. |
| 4 | `cache_scripts=true` par défaut avant validation. | Défaut repassé à `false` pendant les travaux, puis remis à `true` **uniquement après** les validations du §4. |

### 1. Isolation des contextes

`CachedScript { script, entry: Option<Persistent<Function>>, ctx: Context }`.
`Drop for CachedScript` libère le `Persistent` **avant** son `Context` ;
`Drop for Worker` vide le cache (donc tous les `Context`) **avant** le
`Runtime`. rquickjs `abort()` le processus si une valeur persistante survit à
son runtime : cet ordre est une propriété de sûreté, pas du nettoyage cosmétique.

Les modules vendored sont chargés **dans chaque Context** : le wrapper importe
`buffer` (qui importe `ieee754` et `base64-js`) et QuickJS tient son registre de
modules par `JSContext`, donc le loader est sollicité à nouveau pour chaque
nouveau realm.

> **Piège rquickjs 0.14 découvert en test** : `BuiltinLoader::load()` fait
> `self.modules.remove(path)` — c'est un loader **à usage unique**. Le loader
> étant installé sur le Runtime, le premier Context consomme la source
> `buffer` et le second échoue avec `Error loading module 'buffer'` (5 tests en
> échec lors de la première passe). Correction propre via le trait public
> `rquickjs::loader::Loader` : `VendorLoader` conserve ses sources et
> re-déclare le module dans le `Ctx` appelant (`get` + `clone` au lieu de
> `remove`). Aucun `unsafe`, aucun patch de dépendance, aucun renommage de
> modules.

`Context::as_raw()` permet au test `each_cache_entry_has_its_own_context` de
vérifier structurellement que deux entrées ont deux realms distincts.

### 2. Clé de cache et propagation des identifiants

`CodecIdentity { tenant_id: Option<Uuid>, device_profile_id: Option<Uuid> }`
est défini dans `codec/mod.rs`. Les trois appelants réels construisent
l'identité depuis le `DeviceProfile` (`dp.tenant_id`, `dp.id`) et la passent à
`binary_to_struct` / `struct_to_binary`, puis à `js::decode` / `js::encode` :

- `chirpstack/src/uplink/data.rs` (décodage d'uplink) ;
- `chirpstack/src/api/device.rs` (mise en file de downlink via gRPC) ;
- `chirpstack/src/integration/mod.rs` (downlink via intégration MQTT).

`CacheKey { tenant_id, device_profile_id, script_hash }`, plus la comparaison
du texte complet stocké dans l'entrée : une collision de hash ne peut pas
retourner le code compilé d'un autre script. Les 8 tests amont gardent leur
corps intact grâce à deux shims locaux utilisant `CodecIdentity::default()`.

### 3. Limites de ressources

Toutes les options ont `#[serde(default)]` sur `CodecJs` : les fichiers de
configuration existants restent valides.

| Option | Défaut | Rôle |
| --- | --- | --- |
| `cache_scripts` | `true` | `false` = comportement amont (Runtime + Context neufs par appel). |
| `max_cached_scripts` | `64` | Scripts compilés / realms conservés par worker (minimum 1). |
| `max_memory_bytes` | `33 554 432` (32 Mio) | Limite dure QuickJS par **Runtime** ; `0` = illimité (déconseillé). |
| `max_stack_bytes` | `524 288` (512 Kio) | Taille max de la pile JS. |
| `max_calls_per_worker` | `10 000` | Recyclage après N appels. |
| `gc_every_calls` | `100` | `1` = GC à chaque appel, `0` = désactivé. |

Le seuil de recyclage mémoire est dérivé : 75 % de `max_memory_bytes`
(`recycle_memory_bytes()`), via `Runtime::memory_usage().memory_used_size`.
Pas de knob supplémentaire ; `max_memory_bytes=0` le désactive.

**Mesure de l'impact du GC** — `JS_RunGC` parcourt *tous* les contextes du
runtime, il faut donc mesurer avec un cache plein (binaire debug,
`--test-threads=1`, benchmarks ignorés exécutés isolément) :

| Scénario | `gc=1` | `gc=100` | `gc=0` |
| --- | --- | --- | --- |
| 1 realm | 0,34 – 0,38 ms/appel | 0,20 – 0,25 ms/appel | 0,19 – 0,35 ms/appel |
| **64 realms (cache plein)** | **8,1 – 8,4 ms/appel** | 0,10 – 0,12 ms/appel | 0,018 ms/appel |

⇒ `gc_every_calls=100` : un GC à chaque appel est environ **75× plus lent**
avec un cache plein et casserait l'objectif « bien sous 1 ms ». `1` et `0`
restent configurables.

**Recyclage** : le worker entier est jeté et reconstruit à l'appel suivant
après `max_calls_per_worker` appels, si la mémoire atteint 75 % de la limite,
ou **après toute erreur** (exception JS, timeout, OOM, stack overflow, panic
Rust intercepté). Cela borne l'accumulation d'atomes, de shapes, de garbage
cyclique et d'intrinsèques modifiées, et garantit qu'un runtime empoisonné
n'est jamais réutilisé pour un autre device.

**Erreur propre** : `run_guarded()` enveloppe `Worker::run` dans
`catch_unwind(AssertUnwindSafe(...))`. Un échec d'allocation profond dans
rquickjs ou dans `codec::convert` devient
`JS codec engine aborted (worker discarded): ...` au lieu d'un panic remontant
jusqu'à Tokio. Les OOM levés par QuickJS deviennent
`JS codec memory limit exceeded: Error: out of memory`, les débordements de
pile `JS error: Error: Maximum call stack size exceeded`. Les deux tests
vérifient que l'appel suivant fonctionne.

### 4. Valeur par défaut : justification du passage à `true`

`cache_scripts` est resté à **`false` pendant toute la phase de durcissement**
afin qu'aucune modification non validée ne soit active par défaut. Il n'a été
repassé à `true` qu'après les validations suivantes :

1. 8 tests amont + 16 tests `pool_test` (dont les 8 nouveaux tests de
   sécurité) → ✅ **24/24** ;
2. `rustfmt`, `cargo clippy`, `cargo check` postgres **et** sqlite → ✅ 0
   erreur, aucun warning dans les fichiers modifiés ;
3. `chirpstack --config <DIR> configfile` → ✅ TOML valide, 6 options rendues ;
4. bench → ✅ **0,20 – 0,27 ms/appel** avec le défaut (bien sous 1 ms) ;
5. bench cache miss → ✅ ~8,5 – 11,6 ms, **une seule fois** par nouveau triplet ;
6. `a_full_cache_stays_within_the_memory_limit` → ✅ 64 realms =
   **13 994 717 octets**, sous la limite de 32 Mio et sous le seuil de
   recyclage de 24 Mio.

Pour un déploiement multi-tenant exigeant une équivalence stricte avec
l'amont, `cache_scripts=false` reste le choix prudent (Runtime + Context neufs
par appel), au prix de ~8,4 ms par décodage mesuré ici.

### 5. Tests ajoutés (en plus des 16 existants)

| Test | Propriété vérifiée |
| --- | --- |
| `prototype_and_json_pollution_is_confined_to_one_profile` | Un codec A qui pose `Object.prototype.x` et remplace `JSON.parse` n'affecte ni le profil B (même texte de script) ni un autre tenant. |
| `same_script_is_not_shared_between_tenants_or_profiles` | Texte identique sous 2 tenants/profils : compteur global non partagé (séquence 1,2,1,3,2,4). |
| `each_cache_entry_has_its_own_context` | Deux entrées ont deux `Context::as_raw()` distincts dans le même Runtime. |
| `resource_limits_are_applied_to_the_runtime` | `memory_usage().malloc_limit == max_memory_bytes`. |
| `memory_limit_returns_error_and_discards_worker` | Allocation en boucle ⇒ erreur OOM propre, worker jeté, appel suivant OK. |
| `stack_overflow_returns_error_without_crashing` | Récursion infinie ⇒ `Maximum call stack size exceeded`, pas de crash, appel suivant OK. |
| `worker_is_recycled_after_max_calls_per_worker` | Avec `max_calls_per_worker=3`, résultats identiques et état remis à zéro (1,2,3,1,2,3,…). |
| `a_full_cache_stays_within_the_memory_limit` | 64 realms tiennent dans les 32 Mio et sous le seuil de recyclage. |
| `bench_gc_with_a_full_cache` (ignoré) | Mesure le coût réel de `JS_RunGC` avec 64 realms. |

### 6. Validation et chiffres du bench

| Mesure (200 appels, isolé, 3 runs) | Résultat |
| --- | --- |
| Pool durci, `gc=100` (défaut) | 39,4 / 50,0 / 45,8 / 54,9 ms → **0,197 / 0,250 / 0,229 / 0,274 ms par appel** |
| Pool durci, `gc=1` | 76,0 / 71,2 / 67,5 / 92,0 ms → 0,380 / 0,356 / 0,338 / 0,460 ms par appel |
| Pool durci, `gc=0` | 70,2 / 43,7 / 38,9 / 40,6 ms → 0,351 / 0,219 / 0,194 / 0,203 ms par appel |
| `cache_scripts=false` (amont) | 1,67 / 1,69 / 1,73 / 1,79 s → **8,36 / 8,45 / 8,63 / 8,95 ms par appel** |
| Cache miss (nouveau Context + modules vendored) | 346,9 / 339,6 / 464,1 ms pour 40 → **8,67 / 8,49 / 11,60 ms, une fois par clé** |
| Gain sur appels en cache | ≈ **×33 – ×42** vs le chemin « runtime neuf » du même binaire |

Reproduire :

```bash
cargo test --offline -p chirpstack codec::js:: -- --test-threads=1
cargo test --offline -p chirpstack codec::js::bench -- --ignored --nocapture --test-threads=1
cargo test --offline -p chirpstack codec::js::pool_test::bench_gc_with_a_full_cache \
  -- --ignored --nocapture --test-threads=1
```

> Note sandbox : le linker du binaire de test est tué par l'OOM (SIGKILL) avec
> la compilation incrémentale. `CARGO_INCREMENTAL=0 cargo test ... --no-run`
> contourne ce problème d'environnement (doc `OFFLINE-BUILD.md`).

### 7. Ce que rquickjs 0.14 ne permet pas — et alternatives

**a) Les limites sont par `JSRuntime`, pas par `JSContext`.**
`set_memory_limit`, `set_max_stack_size`, `run_gc` et `memory_usage` sont des
API C prenant un `JSRuntime*`. Avec un Runtime par worker et N Contexts, un
tenant peut consommer le budget partagé et provoquer une erreur OOM pour les
autres realms du même worker ; le GC est global. Ce n'est donc **pas** une
isolation mémoire forte par tenant.

- *Alternative recommandée si l'isolation forte est exigée* : **un Runtime par
  entrée de cache** (ou par tenant) avec une LRU bornée. Chaque limite devient
  alors réellement par realm et `Persistent::restore` devient sûr par
  construction. Coût : un `Runtime::new()` par script mis en cache (~8,5 ms
  mesurés à la création d'un realm) et une empreinte mémoire supérieure.
  Variante moins coûteuse : partitionner le pool par tenant (un semaphore et
  un ou plusieurs workers dédiés par tenant) — cela limite le bruit mais pas le
  budget mémoire partagé.
- *Mitigation en place* : recyclage de tout le worker à 75 % de la limite et
  discard immédiat sur OOM. Le rayon d'action d'un codec glouton est limité à
  un worker et à la trame courante ; les autres requêtes sont servies par les
  autres workers du sémaphore.

**b) `Persistent::restore` ne vérifie que le pointeur de Runtime**, pas le
`JSContext` d'origine (`rquickjs-core/src/persistent.rs`). Une valeur
persistante pourrait théoriquement être restaurée dans un autre Context du même
Runtime, et rquickjs ne l'empêcherait pas.

- *Alternative* : notre `CachedScript` associe **atomiquement**
  `(Context, Persistent)` et toute restauration passe par
  `cached.ctx.with(...)` : il n'existe aucun chemin de code qui restaure une
  valeur dans un autre contexte. Pour une garantie défense-en-profondeur, il
  faudrait soit qu'rquickjs expose `Ctx::as_ptr()` publiquement (demande
  upstream), soit adopter l'alternative (a) — un Runtime par entrée — auquel
  cas la vérification de Runtime devient équivalente à une vérification de
  Context.

**c) `set_memory_limit` est un no-op avec un allocator personnalisé.**
La documentation rquickjs précise : « Noop when a custom allocator is being
used, as is the case for the `rust-alloc` or `allocator` features ». Notre
workspace n'active ni l'une ni l'autre (`bindgen`, `loader`, `array-buffer`,
`chrono`), donc la limite est effective — le test
`resource_limits_are_applied_to_the_runtime` vérifie `malloc_limit`. Si ces
features sont activées ultérieurement, il faut basculer sur un quota
applicatif (compteur d'octets + rejet avant exécution) ou sur un processus
superviseur.

**d) La feature `multi-ctx` n'est pas nécessaire.** Elle ne fournit que le
trait `MultiWith`, pour utiliser plusieurs contextes *simultanément* dans une
même closure. Nous utilisons les realms séquentiellement (`Context::with` l'un
après l'autre, sur un seul thread), ce que QuickJS gère nativement.

**e) `BuiltinLoader` est à usage unique.** Traité au §1 par `VendorLoader`,
sans contournement ni `unsafe`.

### Risques résiduels

- Un cache miss coûte ~8,5 ms (Context + 3 modules vendored recompilés par
  realm) : c'est le prix de l'isolation. Sur un parc avec beaucoup de profils,
  surveiller le taux de hits et ajuster `max_cached_scripts`.
- `max_execution_time` (100 ms) couvre aussi la compilation d'un nouveau realm
  (~8,5 ms ici). Sur une machine très lente, augmenter cette valeur ou
  préchauffer les codecs au démarrage.
- L'état de module persiste pour un **même** triplet `(tenant, profil, script)`
  ; le recyclage à 10 000 appels le remet à zéro. Les codecs doivent rester
  des fonctions pures.
- `make test` complet (PostgreSQL / Redis / Mosquitto via Docker Compose)
  reste indisponible dans ce bac à sable.

## Revue A/B/C de `743c680` — appliquée (2026-11-03)

Revue demandée avant d'ouvrir la 2.2 : **deux corrections bloquantes (A, B)** et
**trois vérifications (C1, C2, C3)**. Aucune piste 2.2 n'a été démarrée. Ce
journal remplace, pour le codec JS, les paragraphes « Durcissement sécurité de
la 2.1 » qui décrivaient l'état intermédiaire `743c680`.

### A. Pool dédié de N threads OS — nombre de runtimes borné

`743c680` utilisait `tokio::task::spawn_blocking` + `thread_local!` : un
`Worker` (donc un `Runtime` QuickJS et jusqu'à 64 realms) était créé **par
thread de blocking Tokio**. Ce pool est partagé avec le reste de l'application
et peut monter à 512 threads : le pire cas théorique était
`512 × 64 × 32 Mio = 1 Tio`, sans borne exploitable.

| Étape | Nombre de threads | Borne mémoire JS |
| --- | --- | --- |
| `743c680` | jusqu'à 512 (Tokio, partagé) | `512 × 64 × 32 Mio` = **1 Tio** (non bornée) |
| A (borne demandée) | `N = workers` | `N × max_memory_bytes` = 2 × 32 Mio = **64 Mio** |
| B (une entrée = un Runtime, anciens défauts) | `N = workers` | `N × 64 × 32 Mio` = **4 Gio** |
| **B avec défauts ajustés (livré)** | `N = workers` | `N × 8 × 4 Mio` = **64 Mio** sur ce bac (N=2) |

Implémentation (`chirpstack/src/codec/js/mod.rs`) :

- `CODEC_POOL: LazyLock<CodecPool>` crée exactement **N threads OS dédiés**
  nommés `chirpstack-js-codec-{i}`, chacun propriétaire d'un unique `Worker`.
  Plus aucun `spawn_blocking`, plus aucun `thread_local!`, plus de sémaphore.
- `N = [codec.js] workers`, `0` = `std::thread::available_parallelism()`
  (**2** dans ce bac à sable). À fixer explicitement sur un hôte très multicœur.
- File **bornée** `std::sync::mpsc::sync_channel(queue_capacity)`,
  `queue_capacity = workers × 4` par défaut (**8** ici), partagée via
  `Arc<Mutex<Receiver>>` ; le verrou n'est tenu que pendant `recv()`, jamais
  pendant l'exécution JS (sinon les workers se sérialiseraient).
- Réponse par **`tokio::sync::oneshot`**. `submit()` utilise `try_send()` :
  file pleine ⇒ `codec_js_queue_full_total` + erreur
  `JS codec queue is full (...); request rejected`, **sans attente**. Test
  `a_full_queue_is_rejected_without_waiting`.
- Nouvelles métriques Prometheus : `codec_js_panics_total`,
  `codec_js_queue_full_total`.

### B. Un Runtime par entrée de cache — budgets réellement par script

`CachedScript` contient désormais **`Runtime + Context + Persistent`** (et le
texte du script, l'horloge LRU, le compteur d'appels). Le `Worker` ne possède
plus de `Runtime` : seulement une `HashMap<CacheKey, CachedScript>` et une
horloge.

| Propriété | `743c680` | Livré |
| --- | --- | --- |
| `set_memory_limit` / `set_max_stack_size` | 1 Runtime partagé par 64 realms | **par script** |
| `run_gc()` | parcourt les 64 Context | **1 seul realm** |
| `memory_usage()` et seuil de recyclage | global au worker | **par script** |
| Éviction de cache | vidait **tout** le cache | **LRU** (`last_used`, une entrée) |
| Erreur / saturation mémoire | jetait tout le worker | retire **uniquement** le realm fautif |

Conséquence mesurée sur le GC (debug, cache plein, 200 appels). Les deux
colonnes comparent des caches **pleins aux défauts de leur version** (64 realms
avant, 8 après) : l'écart inclut donc l'effet combiné de B et du nouveau
dimensionnement, ce qui est bien l'effet observé en production.

| `gc_every_calls` | `743c680` (64 realms, 1 Runtime) | Livré (8 realms, 8 Runtimes) |
| --- | --- | --- |
| `gc=1` | 8,14 – 8,37 **ms**/appel | **0,176 ms**/appel |
| `gc=100` | 0,097 – 0,118 ms/appel | 0,050 ms/appel |
| `gc=0` | 0,018 – 0,019 ms/appel | 0,060 ms/appel |

Le GC à chaque appel est redevenu bon marché (≈ ×47 moins cher qu'avant) :
**`gc_every_calls` repasse de `100` à `1`**. Il reste ~×3 plus lent que
`gc=0`, mais il supprime la dérive mémoire entre deux appels ; le surcoût
absolu en release est mesuré au §C3.

**Test d'isolation exigé** :
`memory_exhaustion_in_one_script_does_not_affect_another_on_the_same_thread`
charge le realm B, sature la mémoire du realm A sur le **même** worker
(`JS codec memory limit exceeded: Error: out of memory`), vérifie que le realm
B est **toujours en cache** et que l'appel suivant sur B renvoie le bon
résultat. Complété par `each_cache_entry_has_its_own_runtime_and_context`
(`Context::as_raw()` **et** `get_runtime_ptr()` distincts).

**Recalcul de la borne et ajustement des défauts.** B remplace la borne A par
`N × max_cached_scripts × max_memory_bytes`. Avec les défauts de `743c680`
(64 scripts, 32 Mio) et N=2, cette borne passe à **4 Gio** : inacceptable.
Pour conserver la borne de 64 Mio visée par A, les défauts sont abaissés :

| Option | `743c680` | Livré |
| --- | --- | --- |
| `max_cached_scripts` | 64 | **8** |
| `max_memory_bytes` | 32 Mio | **4 Mio** (4 194 304) |
| `max_calls_per_worker` | 10 000 | 10 000 |
| `gc_every_calls` | 100 | **1** |
| `workers` | — | **0** (= `available_parallelism`) |
| `queue_capacity` | — | **0** (= `workers × 4`) |
| `max_script_bytes` | — | **256 Kio** |

**Borne totale livrée** :
`workers × max_cached_scripts × max_memory_bytes`
= 2 × 8 × 4 194 304 = **67 108 864 octets = 64 Mio** sur ce bac, plus
`queue_capacity` messages en attente (ils ne contiennent aucun realm). Mesure
réelle d'un cache plein sur un thread : **2 298 720 octets** pour 8 realms,
soit 6,9 % de la borne. Sur un hôte à 32 cœurs, la même configuration donne
1 Gio : **fixer `[codec.js] workers` explicitement** en production.

### C1. `catch_unwind` en release et `mem::forget`

Vérification : aucun `panic = "abort"` / `panic = "unwind"` dans les fichiers
TOML du workspace ; `[profile.release]` ne contient que `opt-level = "z"`,
`lto = true`, `codegen-units = 1`. `rustc --print cfg` donne
`panic="unwind"`, et le test `catch_unwind_is_effective_in_this_build`
(`assert!(cfg!(panic = "unwind"))`) est exécuté **dans le binaire `--release`** :
`catch_unwind` y est donc **opérant**, contrairement à l'hypothèse de la revue.
Il deviendrait inopérant si un déploiement ajoutait `-C panic=abort` ; c'est
documenté comme risque résiduel, aucune modification du profil n'a été faite.

Après un panic attrapé, `run_slot` fait `slot.take()` puis
**`std::mem::forget(worker)`** : aucun `Drop` de `Runtime`, de `Context` ou de
`Persistent` n'est exécuté (un `abort()` rquickjs sur un état C corrompu est
ainsi évité), puis `error!(...)`, `CODEC_PANIC_COUNTER.inc()` et une erreur
propre `JS codec engine aborted (runtimes leaked, worker discarded)`. Le
`Drop` n'est utilisé que sur le chemin **normal** (recyclage après
`max_calls_per_worker`), où les runtimes sont connus sains. Test
`caught_panic_leaks_the_runtimes_and_discards_the_worker`.

> **Écart assumé** : la fuite est réelle et **cumulative**. Chaque panic laisse
> jusqu'à `max_cached_scripts × max_memory_bytes` (32 Mio ici) de realm
> QuickJS non libérés ; la borne de 64 Mio ne s'applique donc qu'au pool
> **vivant**. C'est la conséquence directe de la consigne « ne pas exécuter le
> Drop » : alerter sur `codec_js_panics_total` (toute valeur > 0 doit déclencher
> un redémarrage du processus). Aucune correction n'a été tentée ici, car elle
> contreviendrait à C1.

### C2. Ordre d'armement de la deadline et taille de script

`CachedScript::new()` crée le Runtime, applique les limites et **évalue le
module vendored** (`buffer` → `ieee754`, `base64-js`) **sans aucun handler
d'interruption** : c'est du code interne de taille fixe, son coût ne doit pas
être imputé au budget utilisateur. Ensuite seulement :

```
CachedScript::new()      -> vendor_loaded      (pas de deadline)
set_interrupt_handler()  -> deadline_armed    (max_execution_time)
compile_entry(script)    -> user_compile      (texte contrôlé par l'utilisateur)
set_interrupt_handler()  -> deadline_refreshed (avant chaque appel, hors ctx.with)
```

L'armement se fait **hors** `ctx.with()` (qui verrouille le Runtime, sinon
interblocage). Sur un cache hit, il n'y a pas de compilation : la deadline est
simplement réarmée avant l'exécution.

Tests : `deadline_order_is_vendor_then_deadline_then_user_script` (trace
ordonnée), `top_level_infinite_loop_is_interrupted` (une boucle `while(true){}`
**au niveau module** est bien interrompue : `Error: interrupted` à
`<anonymous>:1:1`), `oversized_script_is_rejected_before_compilation`.

**Nouvelle limite absente de `743c680`** : `[codec.js] max_script_bytes`
(défaut 256 Kio), appliquée au script généré (codec utilisateur + wrapper)
**avant** toute création de Runtime, donc avant même l'armement de la
deadline. Elle borne le travail du parseur/compilateur.

### C3. Bench en `--release`

Les chiffres de `743c680` étaient des chiffres **debug**, non représentatifs de
la production. Tous les benchmarks ont été relancés en `--release` (200 appels
par mesure, 3 runs isolés chacun, `--test-threads=1`) :

| Mesure | `--release` |
| --- | --- |
| Pool livré, `gc=1` (défaut) | 30,88 / 30,62 / 33,61 ms → **154,4 / 153,1 / 168,1 µs par appel** |
| Pool livré, `gc=1` (mesure répétée) | 30,58 / 24,01 / 32,05 ms → 152,9 / 120,1 / 160,2 µs par appel |
| Pool livré, `gc=0` | 26,50 / 19,05 / 25,21 ms → 132,5 / 95,3 / 126,0 µs par appel |
| `cache_scripts=false` (comportement amont) | 784,9 / 773,2 / 823,7 ms → **3,92 / 3,87 / 4,12 ms par appel** |
| Cache miss (Runtime + modules vendored + compilation) | 78,2 / 77,4 / 95,1 ms pour 20 → **3,91 / 3,87 / 4,76 ms, une fois par clé** |
| GC plein cache (8 realms) : `gc=1` / `gc=100` / `gc=0` | **49,1 – 49,7 / 15,8 – 16,4 / 15,8 – 16,0 µs** par appel |
| Gain sur appels en cache | ≈ **×23 – ×27** (≈ ×25) vs `cache_scripts=false` du même binaire |

`gc=default` et `gc=1` sont la même configuration (`gc_every_calls=1`),
mesurée deux fois pour donner l'ordre de grandeur du bruit. Le GC à chaque
appel coûte ~30 µs de plus que `gc=0` sur le banc interne (49,1 vs 16,0 µs),
soit un surcoût acceptable de ~25 % sur le chemin complet ; c'est ce qui
justifie le retour de `gc_every_calls` à `1`. Le bench imprime aussi
`[release] panic strategy: unwind=true abort=false`, ce qui confirme C1 dans le
binaire optimisé. Le cache miss est divisé par ~2,4 par rapport aux
8,5 – 11,6 ms mesurés en debug sur `743c680` (comparaison inter-profils
indicative).

Reproduire (dans ce bac à sable, voir `docs/OFFLINE-BUILD.md` pour la raison
des deux overrides de profil) :

```bash
export CARGO_PROFILE_RELEASE_LTO=off CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16
cargo test --release --offline -q -j 1 -p chirpstack codec::js::bench::bench_decode \
  -- --ignored --nocapture --test-threads=1
cargo test --release --offline -q -j 1 -p chirpstack codec::js::bench::bench_cache_miss \
  -- --ignored --nocapture --test-threads=1
cargo test --release --offline -q -j 1 -p chirpstack \
  codec::js::pool_test::bench_gc_with_a_full_cache -- --ignored --nocapture --test-threads=1
```

### Écarts avec les consignes A/B/C

| # | Consigne | Écart / réconciliation |
| --- | --- | --- |
| 1 | A demande de documenter `N × max_memory_bytes`. | B la remplace par `N × max_cached_scripts × max_memory_bytes` dès lors qu'un Runtime est créé par script. Les deux bornes sont documentées ci-dessus ; les défauts ont été abaissés (8 × 4 Mio) pour que la **borne finale reste 64 Mio**, identique à celle visée par A. Ce n'est pas un choix silencieux. |
| 2 | A : « canal borné ». | `std::sync::mpsc::sync_channel` + `Arc<Mutex<Receiver>>` (bibliothèque standard) au lieu d'un `crossbeam-channel`/`flume` : **aucune dépendance ajoutée**, `Cargo.lock` inchangé. Le verrou n'est tenu que pendant `recv()`. La capacité ne compte que les messages **en attente** ; le nombre total de travaux en vol est `N + queue_capacity`. |
| 3 | A : `workers` par défaut = `available_parallelism`. | Confirmé (2 ici). `queue_capacity` par défaut = `workers × 4`, `max_script_bytes` = 256 Kio : valeurs choisies ici, non imposées par la revue. |
| 4 | B : LRU bornée par `max_cached_scripts` par thread. | Implémentée par une horloge monotone `last_used` + `evict_lru()` (une seule entrée par insertion). `743c680` vidait tout le cache. |
| 5 | C1 : vérifier `panic="abort"`. | **Aucun `panic="abort"` n'est configuré** : release = `panic="unwind"`, `catch_unwind` reste opérant. La consigne « le documenter comme inopérant » ne s'applique donc pas ; le risque si un déploiement force `panic=abort` est documenté. `mem::forget` + log + métrique sont appliqués, avec la fuite cumulative signalée. |
| 6 | C2 : deadline après modules vendored, avant compilation. | Conforme. Ajout de `max_script_bytes`, mais la deadline ne couvre **pas** le chargement vendored (coût interne, volontairement exclu du budget utilisateur). |
| 7 | C3 : benchs en `--release`. | Conforme : benchs compilés et exécutés avec `--release` (`opt-level="z"`), **33/33 tests passent aussi dans le binaire release**. Écart d'environnement : le profil release exact du workspace (`lto=true`, `codegen-units=1`) fait dépasser 3,7 Gio au `rustc` final dans ce bac à ~3,9 Gio et il est tué par l'OOM (`signal: 9`). Les chiffres viennent donc de `CARGO_PROFILE_RELEASE_LTO=off` + `CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16` : optimisés, mais pas ceux du binaire LTO livré (un peu plus rapide). Aucun `Cargo.toml` modifié ; sur un poste avec plus de RAM, relancer sans ces deux variables. |
| 8 | Tests déterministes du panic. | Un champ `panic_probe: bool` (toujours `false` en production) a été ajouté à `Job` pour déclencher un panic de façon reproductible sans affecter les autres threads du pool. Crochet de test dans le type de production : à supprimer si l'on introduit un mécanisme d'injection dédié. |
| 9 | `VendorLoader` (`743c680`). | Conservé bien qu'un Runtime par script rende le loader `BuiltinLoader` à usage unique inoffensif : il évite de réintroduire le piège si un Runtime venait à héberger plusieurs Context. |

### Validation

- `rustfmt --check`, `cargo clippy --offline -p chirpstack` : 0 erreur, 0
  warning nouveau.
- `cargo check --offline -p chirpstack` (postgres) et `--tests` : OK ;
  `cargo check --no-default-features --features sqlite` : `OK en 52 s, 0 erreur`.
- `cargo test --offline -p chirpstack codec::js:: -- --test-threads=1` :
  **33 passés / 0 échec / 3 ignorés** (8 tests amont + 25 tests pool/sécurité,
  3 benchmarks manuels ignorés), 1,07 s.
- `chirpstack --config <DIR> configfile` : TOML valide, **10 options
  `[codec.js]`** rendues.
- `uplink::tests` : 3/3.
- `make test` complet (PostgreSQL / Redis / Mosquitto) toujours indisponible
  dans ce bac à sable.

## Ajustements de la revue de `9732223` — appliqués (2026-11-04)

Trois ajustements demandés avant d'ouvrir la 2.2. **Aucun code de la piste 2.2
n'a été modifié** ; la reconnaissance 2.2 fait l'objet d'un document séparé
([`SECURITY-2.2-RECON.md`](SECURITY-2.2-RECON.md)).

### 1. File d'attente : attente bornée, et devenir réel d'un uplink en échec

#### 1.1 Que devient exactement un uplink dont le décodage échoue ?

Chaîne d'appels (fichiers de l'amont, non modifiés par ce commit) :

```
uplink/mod.rs::_deduplicate_uplink        (Redis SADD/PEXPIRE/SET NX, 200 ms, SMEMBERS)
  └─ uplink/mod.rs::handle_uplink        (meta gateway, frame log gateway)
       └─ uplink/data.rs::Data::_handle
            ├─ get_device_for_phy_payload  → device::get_for_phypayload_and_incr_f_cnt_up
            │    └─ UPDATE ... WHERE f_cnt_up <= full_f_cnt   ← FCnt DÉJÀ consommé
            ├─ … ADR / MAC commands / logs / uplink_history …
            └─ send_uplink_event()
                 └─ codec::binary_to_struct → js::decode → submit()
```

`send_uplink_event()` (`chirpstack/src/uplink/data.rs:906-997`) construit
d'abord un `integration_pb::UplinkEvent` contenant **`data` = octets
`FRMPayload` déchiffrés** et `object: None`. Si `binary_to_struct` renvoie une
erreur — y compris `JS codec queue saturated: …` — la branche `Err(e)` :

1. publie un **`integration::log_event`** de niveau `Error`, code
   `LogCode::UplinkCodec`, avec la chaîne d'erreur complète et le
   `deduplication_id` en contexte ;
2. **n'interrompt pas le flux** : `pl.object` reste `None` et l'exécution
   continue.

Conséquences précises :

| Question | Réponse |
| --- | --- |
| La trame est-elle perdue ? | **Non au niveau transport** : le `UplinkEvent` est bien publié (`integration::uplink_event`), avec les octets bruts `data` mais **sans objet décodé**. Les frame-logs (`log_uplink_frame_set`, `log_uplink_meta`, frame-log passerelles) ont déjà été écrits **avant** le décodage. |
| Les intégrations sont-elles notifiées ? | **Oui**, deux fois : un `log_event` d'erreur `UplinkCodec` **et** un `uplink_event` dégradé (`object: None`). |
| Les mesures sont-elles enregistrées ? | **Non.** `detect_and_save_measurements()` lit `up_event.object` ; avec `None`, `data_measurements` est vide : aucune écriture Redis de mesure, aucune auto-détection. |
| L'état du device avance-t-il ? | **Oui.** `sync_uplink_f_cnt()` puis `update_device()` (`partial_update`, sans condition) écrivent `f_cnt_up = full+1` ; `handle_uplink_ack`, `save_metrics` et le flux downlink s'exécutent quand même. Le FCnt avait déjà été réservé par l'`UPDATE` conditionnel de `get_for_phypayload_and_incr_f_cnt_up`. |
| Peut-on récupérer l'objet décodé ? | **Non.** Aucun retry, aucune file d'attente de reprise, aucun dead-letter. Rejouer la même trame dans les 400 ms du TTL de déduplication est absorbé par Redis ; après, le même FCnt est traité comme **retransmission** et `handle_retransmission_reset()` abandonne (sauf `skip_fcnt_check`). L'objet décodé est donc définitivement perdu. |
| Observabilité | `codec_js_queue_full_total` + le `log_event` `UplinkCodec`. Aucune alerte n'est configurée par défaut. |

Les autres appelants du codec ont un comportement différent : `api/device.rs:1148`
et `integration/mod.rs:503` (`struct_to_binary`, downlink) **propagent** l'erreur
à l'appelant (status gRPC / erreur d'intégration) et **ne créent pas** l'élément
de file downlink.

> C'est précisément pour éviter cette perte silencieuse qu'une rafale légitime ne
> doit pas être rejetée : le rejet immédiat de `9732223` transformait une
> saturation transitoire en **`object` absent + mesures absentes + FCnt
> consommé**, sans possibilité de rejouer.

#### 1.2 Attente bornée et capacité

- `submit()` n'utilise plus `try_send()` immédiat :
  `tokio::time::timeout(queue_wait_timeout, tx.send(message)).await`.
  L'attente porte sur une **tâche async**, aucun thread n'est bloqué.
- `[codec.js] queue_wait_timeout = "1s"` (nouveau). Le rejet n'intervient qu'en
  **vraie saturation** (le canal est resté plein pendant tout le délai), avec
  `codec_js_queue_full_total` et une erreur propre
  `JS codec queue saturated: waited …; request rejected`.
- `queue_capacity` passe d'un défaut `workers × 4` à **256 par thread** (0 =
  auto). Avec le routage par hash (§2) il y a un canal par thread, donc
  **N × 256 = 512** emplacements en attente pour N=2.
- Tests : `a_burst_of_200_concurrent_decodes_is_not_rejected` (200 décodes
  simultanés, workers=2, 2 clés → **0 rejet**, résultats corrects) et
  `real_queue_saturation_waits_then_rejects_cleanly` (capacité 1, aucun
  consommateur, attente 80 ms → le `submit` **attend réellement** ≥ 50 ms, puis
  erreur propre + compteur incrémenté).

### 2. Routage déterministe, budget réel et métriques

- **Un canal par thread** (`Vec<Sender<JobMessage>>`) ; plus de
  `Arc<Mutex<Receiver>>` partagé. Chaque job est routé par
  `hash(CacheKey) % N` (`route_worker`), donc **un script n'est compilé et mis
  en cache qu'une seule fois** dans le processus. Capacité réelle du cache :
  **N × `max_cached_scripts`** = 2 × 64 = **128 realms** ici.
- **Éviction sur budget réel** : `Worker::cache_bytes()` somme les
  `Runtime::memory_usage().memory_used_size` de tous les realms du thread ;
  `enforce_budget()` évince les LRU jusqu'à rentrer sous
  `[codec.js] max_cache_memory_bytes` (**32 Mio** par thread, 0 = désactivé).
  `max_memory_bytes` (4 Mio) reste le **plafond par script**. `max_cached_scripts`
  remonte de 8 à **64**, le budget réel devenant la vraie limite.
- **Nouvelle borne** : `N × max_cache_memory_bytes` = 2 × 32 Mio =
  **64 Mio**, inchangée par rapport à `9732223`. Les 128 realms ne sont pas
  additionnés à 4 Mio chacun : le budget réel plafonne la somme. Mesure d'un
  cache plein : ~0,29 Mio par realm, soit ~18 Mio pour 64 realms sous 32 Mio.
  > Limite : `memory_usage()` comptabilise le tas QuickJS, pas l'empreinte RSS
  > complète (pthread, allocations Rust, bytecode). La borne est donc celle du
  > tas JS, à majorer d'une marge process en production.
- **Métriques Prometheus** ajoutées :
  `codec_js_cache_hits_total`, `codec_js_cache_misses_total`,
  `codec_js_cache_evictions_total{reason="lru"|"budget"|"error"}` et
  `codec_js_jobs_by_worker_total{worker="0"|"1"|…}`. Test
  `cache_metrics_expose_hits_misses_and_eviction_reasons`.
- **Déséquilibre mesuré** (`bench_routing_imbalance`, ignoré, 200 appels, 2
  workers) : un profil très actif épingle tous ses jobs sur un seul thread.

| Profil | `--release` (3 runs) | debug |
| --- | --- | --- |
| 1 clé chaude | 86,6 / 128,2 / 145,0 µs par appel | 239,5 µs |
| 2 clés réparties | 36,7 / 52,1 / 59,5 µs par appel | 166,8 µs |
| Ratio hot / spread | **2,36× / 2,46× / 2,44×** | 1,44× |
| `jobs_per_worker` | **[300, 100]** (3:1) | [300, 100] |

Le routage par hash est déterministe et **non sensible à la charge** : c'est le
prix de « un script = un realm ». Correctifs possibles, non implémentés (à
arbitrer) : *least-loaded* avec duplication du realm (casse la garantie
« compilé une fois »), *consistent hashing* à nœuds virtuels (réduit le
déplacement de cache mais pas le déséquilibre d'une clé unique), ou
répartition intra-clé par lots avec plusieurs realms identiques. La métrique
`codec_js_jobs_by_worker_total` permet de détecter le cas en production.

### 3. Après un panic : le thread continue

`a_panic_does_not_kill_the_worker_thread` utilise un pool réel à 1 thread :
décodage OK → job `panic_probe` → erreur `engine aborted` +
`codec_js_panics_total` → **3 décodes suivants réussissent** avec un compteur
de module remis à 1, 2, 3 (preuve du `Worker` neuf) et le compteur de jobs du
thread a augmenté d'au moins 5 (preuve que le thread n'est pas mort).
`mem::forget` n'entraîne donc **aucune perte de capacité**. La fuite mémoire
cumulative des realms leakés reste documentée à la revue précédente :
alerter sur `codec_js_panics_total`.

### Validation

- `rustfmt --check` OK ; `cargo clippy --offline -p chirpstack` 0 erreur / 0
  warning nouveau ; `cargo check` postgres et sqlite OK.
- `cargo test --offline -p chirpstack codec::js:: -- --test-threads=1` :
  **38 passés / 0 échec / 4 ignorés** (8 amont + 30 pool/sécurité, 4 benchs
  manuels ignorés) — **identique en `--release`** (1,31 s).
- `chirpstack --config <DIR> configfile` : TOML valide, **12 options
  `[codec.js]`** (`queue_wait_timeout="1s"`, `max_cache_memory_bytes=33554432`,
  `max_cached_scripts=64`, `queue_capacity=0`).
- `uplink::tests` 3/3 ; `Cargo.lock` inchangé.
- Bench `--release` après routage : pool 171,7 µs/appel (`gc=1`), `gc=0`
  122,8 µs, `cache_scripts=false` 5,07 ms, cache miss 6,01 ms.

### Écarts avec les consignes

| # | Consigne | Écart / choix |
| --- | --- | --- |
| 1 | « capacité plus large (ex. 256) » | `queue_capacity` est devenue **la capacité de CHAQUE canal** (un par thread), donc 256 × N = 512 emplacements pour N=2, pas 256 au total. Choix imposé par le routage par hash ; documenté dans le template de config. |
| 2 | « attente bornée (ex. 1 s) » | Conforme (`queue_wait_timeout="1s"`). Le délai s'ajoute à la latence de l'uplink en cas de saturation et **n'est pas couvert** par `max_execution_time`, qui n'est armé que sur le thread worker. |
| 3 | Métrique de rejet | Le nom `codec_js_queue_full_total` de `9732223` est conservé, mais il compte désormais les **timeouts d'attente bornée**, pas les `try_send` immédiats. |
| 4 | « capacité réelle = N × max_cached_scripts » | Vraie pour le **nombre de realms** ; la mémoire, elle, est bornée par `N × max_cache_memory_bytes`. `max_cached_scripts=64` est donc un plafond de comptage, pas un budget mémoire. |
| 5 | Budget réel | `memory_usage()` = tas QuickJS uniquement, pas le RSS process (voir §2). Borne à majorer en production. |
| 6 | Déséquilibre | Le routage par hash crée un déséquilibre mesuré jusqu'à **×2,46** sur un profil chaud. Non corrigé : toute correction casserait « un script = un seul realm ». Observabilité ajoutée à la place. |
| 7 | Métriques | `jobs_by_worker` supplémentaire (non demandée) pour rendre le déséquilibre mesurable ; `evictions_total` utilise une `Family` labelisée (`reason`), `Cargo.lock` inchangé. |
| 8 | Panic | Confirmé sans modifier `run_slot` : le test passe par le vrai pool et le vrai thread, pas par un slot local. |


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
| 2.1 | ✅ **Appliquée + durcie + ajustée** — pool de runtimes QuickJS, cache de scripts compilés, isolation par `Context`/tenant/profil, limites mémoire/stack, GC et recyclage, revue A/B/C (pool dédié de N threads, un Runtime par script, `mem::forget`, `max_script_bytes`), puis attente bornée, routage par hash et budget mémoire réel (38 tests). Voir « Phase 2.1 », « Durcissement sécurité de la 2.1 » et « Revue A/B/C de 743c680 ». | `chirpstack/src/codec/js/mod.rs`, `codec/mod.rs`, `config.rs`, `cmd/configfile.rs`, 3 appelants | **Élevé**, ~×33–42 sur appels en cache | Fait |
| 2.2 | 🔎 **Étape 1 faite, rapport seul** — abordée sous l'angle **sécurité** : chaîne de résolution (candidats DevAddr / sessions secondaires / MIC / FCnt 32 bits), menaces DoS-Rejeu-Concurrence-MIC temps constant, mesures CPU du MIC par nombre de candidats, 8 propositions classées et règles anti-rejeu pour tout cache de sessions. Voir [`SECURITY-2.2-RECON.md`](SECURITY-2.2-RECON.md). **Aucune implémentation.** | `storage/device.rs`, `uplink/data.rs`, `uplink/mod.rs`, `lrwn/src/phy_payload.rs` | Sécurité : rollback FCnt + DoS non authentifié | Recon. faite |
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
