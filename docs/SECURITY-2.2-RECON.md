# Piste 2.2 — Reconnaissance sécurité (étape 1)

**Date** : 2026-11-04 · **Branche** : `arena/890db17c-chirpstack-well-store`
**Référence** : ChirpStack `v4.20.0` (`8b5688584c60fca78ae756d90a54f681faa5bee6`)
**Objet** : reconnaissance de la chaîne de résolution d'un uplink data, modèle
de menace, mesures et propositions classées. **Aucun code du dépôt n'a été
modifié** pour cette étape (le harnais de mesure est un fichier autonome dans
`/tmp`, non versionné).

> **Limites du bac à sable** : ni PostgreSQL, ni Redis, ni Mosquitto, ni
> passerelle. `make test` (services Docker Compose) est indisponible. Les
> mesures ci-dessous sont donc **CPU locales** sur le vrai code `lrwn`,
> complétées par un **comptage statique** des opérations DB/Redis. Aucune
> latence bout-en-bout n'a été mesurée ; elle devra l'être sur un banc avec
> services avant toute implémentation.

---

## a. Chaîne de résolution d'un uplink data

| # | Étape | Fichier | Opérations |
| --- | --- | --- | --- |
| 1 | Déduplication multi-passerelles | `uplink/mod.rs:169-234` | clé `up:collect:{region_config_id}:{tx_info_hex}:{phy_hex}` + `:lock`. Pipeline Redis **atomique** `SADD` + `PEXPIRE` + `SET NX`. TTL = `max(2 × deduplication_delay, 200 ms)` = **400 ms** par défaut (`deduplication_delay=200 ms`). Puis `sleep(200 ms)`, `SMEMBERS`, `deduplication_id = Uuid::new_v4()`. |
| 2 | Normalisation | `uplink/mod.rs:311-364` | `PhyPayload::from_slice`, calcul `dr`/`ch`, `update_gateway_metadata()` (≥1 lookup par `rx_info`), `stream::frame::log_uplink_for_gateways()` (2 `XADD MAXLEN ~`). Dispatch par `FType`. |
| 3 | Résolution device | `uplink/data.rs:225-283` → `storage/device.rs:310-502` | `get_for_phypayload_and_incr_f_cnt_up(region_config_id, relayed, &mut phy, tx_dr, tx_ch)`. |

### 3.1 Requête des sessions candidates

```sql
SELECT * FROM device
WHERE (dev_addr = :dev_addr OR secondary_dev_addr = :dev_addr)
  AND is_disabled = false;
```
`storage/device.rs:333-341`. Index `idx_device_dev_addr` et
`idx_device_secondary_dev_addr` (migrations Postgres/SQLite). **Aucun `LIMIT`,
aucun plafond configurable** : le nombre `D` de devices candidats est celui des
collisions de DevAddr dans la flotte. `D = 0` → `Error::NotFound` → `Abort`.

Pour chaque device, `storage/device.rs:347-355` construit
`sessions = [device_session.clone()]` **+ `pending_rejoin_device_session` si
présent** (rejoin/context switch). Donc `S ∈ [D, 2D]` sessions, avec un
`clone()` du protobuf `DeviceSession` par device (coût mémoire/CPU proportionnel
à `D`).

Filtres avant MIC : `ds.region_config_id == region_config_id` **et**
`ds.dev_addr == dev_addr` (`storage/device.rs:374-376`). Le device « relay-only »
n'est rejeté qu'**après** un MIC valide (`:418-427`).

> Variante : `get_for_phypayload()` (`storage/device.rs:505-585`) est une version
> lecture seule utilisée ailleurs. Elle ne teste qu'**un** FCnt, ne fait pas le
> `UPDATE` atomique, et porte un `// TODO: Also validate region_config_id?`
> (`:527`) — divergence à cadrer séparément.

### 3.2 Résolution du FCnt 32 bits

`get_full_f_cnt_up(next_expected_full_fcnt, truncated_f_cnt)`
(`storage/device.rs:1067-1077`) reconstruit le compteur 32 bits à partir des
16 LSB transmis :

- retransmission : si `trunc == ((next % 2^16) as u16).wrapping_sub(1)` → `next - 1` ;
- sinon `gap = (trunc - next % 2^16) mod 2^16`, `full = next.wrapping_add(gap)`.

**Aucun `MAX_FCNT_GAP`** (la recommandation LoRaWAN 1.0.4 est 16 384) : le gap
accepté est 0…65 535. Le `wrapping_add` gère le rollover 32 bits mais sans garde
explicite (voir §b « Rejeu »).

### 3.3 MIC par candidat

`storage/device.rs:383-409` : pour chaque session, boucle sur
`[full_f_cnt, f_cnt_orig]` (le 16 LSB brut, nécessaire après un reset de
compteur). À chaque itération `pl.fhdr.f_cnt = f_cnt` puis
`phy.validate_uplink_data_mic(mac_version, conf_f_cnt, tx_dr, tx_ch,
f_nwk_s_int_key, s_nwk_s_int_key)` ; `break` au premier succès.

`lrwn/src/phy_payload.rs:585-606` :

```rust
mic == self.calculate_uplink_data_mic(...)?     // [u8; 4] == [u8; 4]
```

`calculate_uplink_data_mic` (`:919-993`) calcule **deux AES-CMAC** :
`cmac_s` (clé `s_nwk_s_int_key`, bloc `b1`) **et** `cmac_f` (clé
`f_nwk_s_int_key`, bloc `b0`), **même en LoRaWAN 1.0** où seul `cmac_f[0..4]`
est utilisé. La taille du bloc CMAC croît avec `FRMPayload` (jusqu'à ~255
octets).

**Bilan** : une trame à MIC invalide coûte `2 × S` validations MIC, soit jusqu'à
`4 × D`. Une trame valide coûte 1 à `2 × S` validations selon la position du
bon candidat.

### 3.4 Validation d'état et mise à jour du FCnt

`storage/device.rs:411-494`, après MIC valide :

| Cas | Résultat | SQL |
| --- | --- | --- |
| `full >= d.f_cnt_up` | `ValidationStatus::Ok` | **`UPDATE device SET f_cnt_up = full+1, scheduler_run_after = … WHERE dev_eui = … AND f_cnt_up <= full`** (ou `f_cnt_up > full` sur context switch). `updated_rows == 0` → `Error::FCntUpAlreadyUpdated`. |
| `ds.skip_f_cnt_check` | `ValidationStatus::Ok` | **aucun `UPDATE`**, `d.f_cnt_up = 0` en mémoire. |
| `full == d.f_cnt_up - 1` | `Retransmission` | aucun |
| sinon | `Reset` | aucun |

Retour dans `uplink/data.rs` : `handle_retransmission_reset()` (`:491-546`)
**abandonne** sur `Retransmission`/`Reset` … **sauf si `skip_f_cnt_check`**, où
il renvoie `Ok(())` immédiatement (aucun log d'alerte). Puis le flux continue
(`MAC commands`, `append_meta_data_to_uplink_history`, `send_uplink_event`,
`detect_and_save_measurements`) et **à la fin** :

- `sync_uplink_f_cnt()` (`:1111-1115`) pose `device_changeset.f_cnt_up = full+1` ;
- `update_device()` (`:1117-1125`) appelle `device::partial_update(dev_eui, …)`
  (`storage/device.rs:611-619`) : **`UPDATE … SET changeset WHERE dev_eui = …`
  sans condition sur `f_cnt_up`**.

Le chemin `InvalidMIC` (`uplink/data.rs:264-273`) : `info!`, puis
`stream::frame::log_uplink_for_device()` avec `dev_eui = "0000000000000000"`
(2 `XADD`), puis `Error::Abort` — absorbé silencieusement par `Data::handle`
(`:55-65`).

---

## b. Modèle de menace

### b.1 DoS — coût d'une trame à MIC invalide

**Aucune limite, aucun rate-limit.** `grep -rni 'rate.limit|ratelimit|throttl'
chirpstack/src/` ne renvoie rien ; `config.rs` n'expose aucun quota ; la requête
candidate n'a pas de `LIMIT` ; il n'y a ni backoff, ni cache négatif de DevAddr,
ni pénalité par `dev_addr`.

Coût **par trame à MIC invalide** (comptage statique, ordre d'exécution) :

| Ressource | Opérations |
| --- | --- |
| Redis dédup | 1 pipeline atomique (`SADD`/`PEXPIRE`/`SET NX`) + `sleep(200 ms)` + `SMEMBERS` |
| Redis passerelles | ≥1 lookup de meta par `rx_info` + 2 `XADD` de frame-log |
| Redis frame-log « MIC faux » | 2 `XADD` supplémentaires (`dev_eui` nul) |
| PostgreSQL | **1 `SELECT` ramenant `D` lignes** (aucune écriture) |
| CPU | `D` clones de `DeviceSession`, `2 × S` validations MIC = `4 × D` AES-CMAC au pire, `S` reconstructions de FCnt |
| Intégrations | aucune (le flux s'arrête à `Abort`) |

Points d'amplification :

- **La déduplication n'est pas une défense** : la clé contient le `hex` complet
  du PHY payload, donc changer **un seul octet de MIC** crée une nouvelle clé.
  Le TTL de 400 ms ne coalesce que des trames **identiques**. Une rafale de
  trames distinctes vers un DevAddr valide contourne totalement ce mécanisme.
- `D` est choisi par l'opérateur (collisions de DevAddr), pas par l'attaquant,
  mais il est **observable à 32 bits sur l'air** et peut être très grand sur un
  réseau dense ; l'attaquant n'a pas besoin de clés, seulement d'un DevAddr
  existant et d'un MIC faux.
- La taille du `FRMPayload` (jusqu'à ~255 o) augmente le coût de chaque CMAC :
  l'attaquant peut maximiser le travail CPU par trame.
- Chaque trame écrit dans des Redis Streams (`XADD`, `MAXLEN ~` borné mais
  réel) → amplification I/O/mémoire en plus du CPU.

**Nombre max de candidats** : illimité (pas de `LIMIT` ni de config). À
recommander : un plafond explicite + métrique `candidates_per_dev_addr`.

### b.2 Rejeu — FCnt, `skip_f_cnt_check`, déduplication

| Mécanisme | Comportement | Risque |
| --- | --- | --- |
| Gap max | **Absent** : tout gap 0…65 535 est accepté si le MIC passe. | Un compteur 16 LSB très en avance est interprété comme `full = next + gap` ; la reconstruction 32 bits est ambiguë. Le code compense en testant aussi `f_cnt_orig`. À cadrer avec un `MAX_FCNT_GAP`. |
| Rollover 16→32 | `wrapping_add` sur `u32` ; `f_cnt_up` stocké en `i64`. | Au passage de `0xFFFF_FFFF` → 0, `get_full_f_cnt_up(1, 0)` retombe sur le cas « retransmission » (`0`) au lieu de `2^32`. À **vérifier par un test dédié** avant d'affirmer un bug ; aucun test de rollover 32 bits n'a été trouvé. |
| MIC valide + FCnt ancien (chemin normal) | `full < f_cnt_up` → `Retransmission` ou `Reset` → `handle_retransmission_reset()` **abandonne**. | Protection correcte, avec log d'alerte `UplinkFCntRetransmission` / `UplinkFCntReset`. |
| **`skip_f_cnt_check = true`** | `ValidationStatus::Ok` **sans `UPDATE` conditionnel**, `d.f_cnt_up = 0`, `handle_retransmission_reset()` renvoie `Ok` sans log. Puis `sync_uplink_f_cnt()` + `update_device()` écrivent **`f_cnt_up = full+1` sans condition**. | **Fenêtre de rejeu + rollback de FCnt** : une trame ancienne capturée (MIC valide) est acceptée, silencieusement, et **ramène le compteur en arrière**. Après désactivation du flag, toutes les trames capturées dans l'intervalle redeviennent « fraîches ». |
| Déduplication multi-passerelles | Coalesce les copies **identiques** pendant 400 ms, clé = PHY payload. | Ce n'est **pas** une protection anti-rejeu : après le TTL, la même trame reçoit un nouveau `deduplication_id`. Avec `skip_f_cnt_check`, elle est acceptée ; sinon elle est rejetée comme retransmission. |

### b.3 Concurrence — deux uplinks simultanés du même device

- **Chemin normal : protégé.** Le `SELECT` lit `f_cnt_up`, puis l'`UPDATE`
  **conditionnel** `WHERE f_cnt_up <= full` (compare-and-swap) réserve le
  compteur. Deux flux concurrents avec le même `full` : un seul `UPDATE`
  modifie une ligne, l'autre obtient `updated_rows == 0` →
  `Error::FCntUpAlreadyUpdated` (`storage/device.rs:470-474`). C'est une
  atomicité correcte au niveau DB, pas un verrou applicatif.
- **Mais l'atomicité n'est pas bout-en-bout** :
  - `skip_f_cnt_check` **court-circuite le CAS** : les deux flux passent, puis
    deux `partial_update` inconditionnels → « dernier écrit gagne », FCnt peut
    reculer.
  - `filter_rx_info_by_tenant()` (`uplink/data.rs:551-570`) restaure
    `f_cnt_up = d.f_cnt_up` **sans condition** en cas d'erreur de filtrage :
    si un flux plus récent a déjà avancé le compteur, cette écriture peut le
    faire reculer.
  - `update_device()` / `partial_update()` en fin de flux n'ont **aucune
    condition sur `f_cnt_up`**.
- Le verrou de déduplication ne protège que les copies **du même** PHY payload ;
  deux trames différentes du même device ont des clés distinctes → pas de
  sérialisation.

### b.4 Comparaison du MIC en temps constant ?

**Non.**

- `lrwn/src/phy_payload.rs:595` : `mic == calculated` sur `[u8; 4]`, simple
  `PartialEq`. LLVM abaisse souvent une comparaison de 4 octets en un seul
  `cmpl` 32 bits (pratiquement constant), mais ce n'est **ni garanti, ni
  documenté, ni testé**. Aucun `subtle`/`ct_eq`/`constant_time_eq` dans
  `lrwn/src` ou `chirpstack/src` (`grep` → vide). `subtle 2.6.1` est déjà
  présent dans `Cargo.lock` (transitif via `aes`/`cmac`).
- `validate_uplink_data_micf()` (`lrwn/src/phy_payload.rs:642-660`), utilisée
  par `storage/passive_roaming.rs:150`, compare des **tranches**
  `v[2..] == mic[2..]` → sortie anticipée garantie, temps variable.
- Fuite secondaire : le `break` au premier MIC valide fait dépendre la durée de
  la position du bon candidat ; le nombre de candidats testés est donc
  observable.

**Gravité pratique faible** (MIC 32 bits, brute-force hors ligne irréaliste,
gigue réseau LoRaWAN/passerelles bien supérieure au signal), mais c'est un écart
d'hygiène à corriger à peu de frais.

---

## c. Mesures

### c.1 Dispositif

Harnais autonome `/tmp/bench_mic.rs`, compilé avec `rustc -O` contre
`target/release/deps/liblrwn-40fd48584065d172.rlib` (`-L dependency=...`). Il
reproduit exactement la boucle `[full_f_cnt, f_cnt_orig]` de
`get_for_phypayload_and_incr_f_cnt_up` sur la trame LoRaWAN 1.0 confirmée du
doc-test `lrwn`, avec `k` clés candidate (la bonne en position 0, en position
`k-1`, ou absente). 2 000 répétitions par point, `black_box` sur les résultats.
**Aucun fichier du dépôt n'a été ajouté ou modifié.**

### c.2 Coût CPU d'une validation MIC

```
one validate_uplink_data_mic: 2.24 µs   (2 AES-CMAC, même en LoRaWAN 1.0)
```

| `k` candidats (sessions) | Checks MIC : invalide / valide@1er / valide@dernier | MIC invalide | Valide, bon candidat 1er | Valide, bon candidat dernier |
| --- | --- | --- | --- | --- |
| 1 | 2 / 2 / 2 | 4,42 µs | 4,44 µs | 4,46 µs |
| 2 | 4 / 2 / 4 | 8,79 µs | 4,49 µs | 8,82 µs |
| 4 | 8 / 2 / 8 | 17,48 µs | 4,47 µs | 17,58 µs |
| 8 | 16 / 2 / 16 | 34,92 µs | 4,46 µs | 35,01 µs |
| 16 | 32 / 2 / 32 | 69,75 µs | 4,46 µs | 70,31 µs |
| 32 | 64 / 2 / 64 | 141,13 µs | 4,63 µs | 143,17 µs |
| 64 | 128 / 2 / 128 | 289,03 µs | 4,48 µs | 280,33 µs |
| 128 | 256 / 2 / 256 | 569,26 µs | 4,44 µs | 569,39 µs |

Lecture : le coût est **strictement linéaire** dans le nombre de candidats pour
une trame invalide (`≈ 4,4 µs × k`), tandis qu'une trame valide dont le device
est le premier candidat reste à **~4,5 µs** quel que soit `k`. Autrement dit,
**une trame non authentifiée coûte jusqu'à ×128 de plus qu'une trame valide**
sur un DevAddr à 128 candidats. À 128 candidats, le seul CPU MIC sature un cœur
à ≈ 1 750 trames/s (borne haute : la DB et les 200 ms de déduplication
dominent en pratique).

### c.3 Comparaison valide / MIC invalide (opérations, non mesurées en latence)

| | Trame valide (1 candidat) | Trame à MIC invalide (`k` candidats) |
| --- | --- | --- |
| Redis dédup | pipeline + 200 ms + `SMEMBERS` | identique |
| Meta / frame-logs passerelles | ≥1 lookup + 2 `XADD` | identique |
| Frame-log `dev_eui` nul | non | **+2 `XADD`** |
| PostgreSQL | 1 `SELECT` (1 ligne) + **1 `UPDATE` CAS** | 1 `SELECT` (`k` lignes), **0 écriture** |
| CPU MIC | 1–2 checks (~4,5 µs) | `2k` checks (voir tableau) |
| Clones `DeviceSession` | 1 | `k` |
| Fin de flux | `partial_update`, intégrations, mesures, downlink | **aucun** (Abort) |

La trame invalide est **moins chère en écritures** mais déclenche **tout le
chemin de résolution non authentifié** (dédup, meta, frame-logs, SELECT,
clones, MIC) sans aucune pénalité ni plafond. Il n'existe pas de mesure de
latence DB/Redis dans ce bac : à refaire sur un banc avec services, en
balayant `k = 1…128` et en relevant `p50/p99`.

---

## d. Propositions classées (gravité / coût)

| # | Gravité | Constat | Proposition | Coût |
| --- | --- | --- | --- | --- |
| 1 | **Élevée** | `skip_f_cnt_check` + `partial_update` inconditionnel ⇒ **rollback de FCnt et fenêtre de rejeu** ; le CAS est contourné, aucun log d'alerte. | Ne jamais écrire un FCnt **inférieur** : `partial_update`/`update_device` avec `WHERE f_cnt_up <= :full` ou `GREATEST(f_cnt_up, full+1)`. Conserver le `UPDATE` conditionnel **même quand le contrôle FCnt est désactivé** (ne désactiver que le rejet, pas la réservation atomique). Toujours émettre un log `UplinkFCntRetransmission/Reset`, même avec le flag. Ajouter un test « trame ancienne + flag → FCnt ne recule pas ». | Faible : 1–2 requêtes SQL + tests |
| 2 | **Élevée** | DoS sur DevAddr valide : pas de `LIMIT`, pas de rate-limit, `4 × D` AES-CMAC, 2 `XADD` par trame, dédup contournable en changeant 1 octet. | (a) `.limit(max_candidates)` + métrique + rejet propre au-delà ; (b) **token bucket par `dev_addr`** dans Redis (`INCR` + `PEXPIRE`), appliqué **après dédup mais avant le `SELECT`**, clé = DevAddr (pas le PHY payload) pour survivre à la variation du MIC ; (c) cache négatif court des DevAddr sans session ; (d) ne pas écrire le frame-log `dev_eui` nul sous forte charge. | Moyen : config + 1 aller-retour Redis + métriques + tests |
| 3 | Moyenne | `calculate_uplink_data_mic` calcule **2 AES-CMAC même en LoRaWAN 1.0** ; `ds.clone()` par candidat. | Ne calculer `cmac_s` que pour 1.1 ; pré-factoriser `b0`/`b1` invariants ; emprunter `&DeviceSession` au lieu de cloner. Divise par ~2 le travail non authentifié. | Faible–moyen (patch `lrwn` + tests) |
| 4 | Moyenne | Écritures FCnt non conditionnelles en fin de flux et dans `filter_rx_info_by_tenant()`. | Rendre toute écriture de `f_cnt_up` monotone/conditionnelle ; le rollback d'erreur de filtrage doit être `WHERE f_cnt_up <= :old_value`. | Faible |
| 5 | Moyenne | Pas de `MAX_FCNT_GAP`, rollover 32 bits non testé. | Introduire `MAX_FCNT_GAP` configurable (défaut 16 384), rejeter au-delà **après** MIC valide ; écrire des tests de rollover 16→32 et de reset. | Moyen (sémantique LoRaWAN à valider) |
| 6 | Faible | Comparaison MIC non constante (`==` sur `[u8;4]`, `==` sur tranches en roaming passif), `subtle` déjà dans `Cargo.lock`. | Comparaison à temps constant (`ct_eq` via `subtle`, ou accumulation XOR sur `u32` sans nouvelle dépendance) dans `validate_uplink_data_mic` **et** `validate_uplink_data_micf`. | Faible |
| 7 | Faible (perf) | Piste 2.2 initiale = éviter les clones / mettre les sessions en cache. | Voir §d.1 ci-dessous : **un cache de sessions ne doit jamais devenir une source de vérité pour le FCnt**. | Élevé |
| 8 | Faible | Observabilité absente sur ces chemins. | Compteurs/histogrammes : `invalid_mic_total{dev_addr}`, `candidates_per_dev_addr`, `fcnt_cas_conflicts_total`, `dedup_bypass_total`, latence de résolution p50/p99. | Faible |

### d.1 Si un cache de sessions est envisagé : comment **ne pas** ouvrir de fenêtre de rejeu

Le risque est précis : une entrée en cache contient un `f_cnt_up` **périmé**. Si
l'acceptation d'une trame se base dessus, une trame ancienne capturée (MIC
valide) est jugée « fraîche » et rejouée ; pire, l'écriture du FCnt à partir de
cette valeur stale fait **reculer** le compteur et réouvre la fenêtre pour
toutes les trames suivantes.

Règles à respecter (à appliquer ensemble) :

1. **Le cache ne sert qu'à présélectionner les candidats MIC** (clés,
   `mac_version`, `conf_f_cnt`, `dev_addr`), **jamais à décider de l'acceptation
   ni à fournir le FCnt de référence**.
2. **La source de vérité reste la DB** : après le MIC, exécuter **systématiquement**
   l'`UPDATE ... WHERE dev_eui = … AND f_cnt_up <= :full` (CAS). C'est lui qui
   décide. « Cache hit » ne doit jamais raccourcir ce chemin.
3. **Contrôle de concurrence optimiste** : chaque entrée porte une `version`
   (ou un hash de la session). Toute `partial_update` incrémente `version` ;
   les écritures sont `WHERE version = :cached_version`. Si 0 ligne →
   **invalider l'entrée, relire la DB, revalider le MIC**, puis seulement alors
   continuer. Pas de « last write wins ».
4. **Invalidation explicite** sur activation OTAA/ABP, rejoin, context switch,
   changement de `dev_addr`/`secondary_dev_addr`, mise à jour de
   `skip_f_cnt_check`, et sur toute erreur `FCntUpAlreadyUpdated`.
5. **TTL borné et court** (≤ fenêtre de déduplication, 400 ms par défaut), avec
   limite de taille par DevAddr ; ne **jamais** mettre en cache un device avec
   `skip_f_cnt_check = true`.
6. **Monotonicité** : même avec cache, `f_cnt_up` ne doit jamais être écrit à la
   baisse (`GREATEST`), y compris sur le chemin d'erreur de
   `filter_rx_info_by_tenant()`.
7. **Tests de non-régression rejeu** : (i) trame capturée rejouée après TTL avec
   cache chaud → rejetée ; (ii) deux flux concurrents → un seul `Ok`, `version`
   cohérente ; (iii) entrée stale (`version` ancienne) → invalidation et re-lecture ;
   (iv) `skip_f_cnt_check` → aucune entrée en cache.

### d.2 Ordre d'implémentation recommandé

1. #1 (rollback FCnt) — corrige la faille la plus grave, faible coût.
2. #4 (écritures monotones) — même famille, verrouille la concurrence.
3. #2 (plafond + rate-limit par DevAddr) — coupe le DoS non authentifié.
4. #3 + #6 — réduit le travail et l'hygiène cryptographique.
5. #5 — sémantique FCnt à valider avec des tests LoRaWAN.
6. #7 seulement après 1–4, avec les sept règles de §d.1.
7. #8 à ajouter dès l'étape 1 pour mesurer l'effet de chaque patch.

**Aucune de ces propositions n'est implémentée ici.** Prochaine étape à valider
avec toi avant d'écrire du code.
