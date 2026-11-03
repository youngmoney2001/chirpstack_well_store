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
- analyse syntaxique `scripts/check_rust_syntax.py` en repli.

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
| 2.1 | ✅ **Appliquée + durcie** — pool de runtimes QuickJS, cache de scripts compilés, puis isolation par `Context`/tenant/profil, limites mémoire/stack, GC et recyclage (24 tests). Voir « Phase 2.1 » et « Durcissement sécurité de la 2.1 ». | `chirpstack/src/codec/js/mod.rs`, `codec/mod.rs`, `config.rs`, `cmd/configfile.rs`, 3 appelants | **Élevé**, ~×33–42 sur appels en cache | Fait |
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
