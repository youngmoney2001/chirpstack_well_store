# Provenance du code amont

Ce dépôt est un **snapshot source** (pas un sous-module, pas un sous-arbre avec
historique) du projet officiel :

- Dépôt : <https://github.com/chirpstack/chirpstack>
- Tag : `v4.20.0` (publié le 2026-10-21)
- Commit : `8b5688584c60fca78ae756d90a54f681faa5bee6`
- Licence : **MIT**, Copyright (c) 2022 Orne Brocaar — voir [`LICENSE`](LICENSE)
- Date de l'import : 2026-11-03

Import réalisé avec :

```bash
git clone --depth 1 --branch v4.20.0 \
  https://github.com/chirpstack/chirpstack.git /tmp/cs-upstream
```

Le contenu a été copié à la racine de ce dépôt (hors répertoire `.git` de
l'amont), afin que ce dépôt soit un fork direct et que les chemins du noyau
restent `chirpstack/src/...`.

## Écarts volontaires par rapport au snapshot amont

Seuls quatre fichiers diffèrent de l'arbre du tag :

| Fichier amont | Traitement | Raison |
| --- | --- | --- |
| `.gitignore` | remplacé | La règle `.*` de l'amont ignore **tous** les fichiers cachés ; or `.cargo/config.toml`, `.env`, `.docker-compose/` et `.github/` sont nécessaires au développement et doivent être versionnés ici. |
| `README.md` | remplacé | README de ce fork (voir `docs/upstream/README.md` pour l'original). |
| `.github/workflows/main.yml` | déplacé vers `docs/upstream/ci-main.yml` | La CI amont est liée à Nix et publie des binaires sur le bucket S3 de l'amont avec *leurs* secrets ; elle échouerait et serait dangereuse sur un fork. Elle est conservée comme référence, désactivée. |
| `LICENSE` | conservé tel quel | Obligation de la licence MIT. |

Aucun autre fichier source n'a été modifié lors de l'import. Les modifications
ultérieures du noyau sont listées dans [`docs/OPTIMIZATIONS.md`](docs/OPTIMIZATIONS.md).

## Comparer avec l'amont / se resynchroniser

Comme l'historique git de l'amont n'a pas été importé, la comparaison se fait
arbre à arbre :

```bash
# 1. récupérer la version de référence (ou un nouveau tag)
git clone --depth 1 --branch v4.20.0 \
  https://github.com/chirpstack/chirpstack.git /tmp/cs-upstream

# 2. voir uniquement NOS modifications (en excluant les 4 écarts ci-dessus)
diff -ru --exclude=.git --exclude=.gitignore --exclude=README.md \
  --exclude=LICENSE --exclude=target --exclude=node_modules \
  /tmp/cs-upstream . > /tmp/local.patch
```

Pour passer à une version amont plus récente :

1. cloner le nouveau tag dans `/tmp/cs-upstream` ;
2. générer le patch de nos modifications avec la commande ci-dessus ;
3. lister chaque entrée de `docs/OPTIMIZATIONS.md` et la réappliquer
   (ou la laisser tomber si l'amont l'a déjà corrigée) ;
4. mettre à jour ce fichier (tag + commit) et `docs/OPTIMIZATIONS.md`.

## Remarque sur la licence

Le code amont est distribué sous MIT. La copie de la notice de licence et
l'attribution de l'auteur sont conservées. Toute redistribution de ce dépôt
doit conserver [`LICENSE`](LICENSE).
