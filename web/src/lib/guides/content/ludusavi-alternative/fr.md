---
title: "Alternative à Ludusavi : synchronisation cloud automatique de vos parties"
description: "Une comparaison équitable entre Ludusavi et Hoard. Ludusavi est un excellent outil open source de sauvegarde locale ; Hoard ajoute une synchro cloud gérée et un historique versionné sur tous vos PC — avec les mêmes données d'emplacement."
order: 5
updated: 2026-06-28
---

Si vous cherchez un moyen de sauvegarder et synchroniser vos parties, vous avez sans doute trouvé **Ludusavi** — et il est excellent. Ce guide est une comparaison honnête pour vous aider à choisir le bon outil, et explique où Hoard s'inscrit si vous voulez une synchro cloud automatique entre machines.

## Ce que Ludusavi fait bien

Ludusavi est un outil gratuit et open source (créé par mtkennerly) pour sauvegarder et restaurer les parties PC sous Windows, macOS et Linux. Il a une interface soignée et une CLI, trouve automatiquement les sauvegardes de milliers de jeux, conserve des sauvegardes locales versionnées, et peut envoyer ces sauvegardes vers un cloud qui vous appartient en configurant **Rclone** (Google Drive, Dropbox et bien d'autres). Si vous voulez un contrôle total et un montage fait main, Ludusavi est un choix fantastique — et entièrement gratuit.

Hoard n'est pas là pour le remplacer. En fait, **Hoard utilise la même base de données communautaire d'emplacements que celle sur laquelle s'appuie Ludusavi** pour localiser où chaque jeu range ses sauvegardes : la qualité de détection est donc équivalente.

## En quoi Hoard est différent

Le point où la plupart bloquent avec tout outil local, c'est la **synchronisation entre appareils**. Avec Ludusavi, vous la faites vous-même : planifier une sauvegarde, configurer un distant Rclone, puis restaurer sur l'autre PC avant de jouer. Ça marche, mais c'est manuel.

Hoard transforme cela en **synchro cloud gérée** :

- **Connectez-vous et c'est parti.** Pas de distants Rclone, pas de scripts. Hoard envoie votre sauvegarde après le jeu et télécharge la dernière version avant que vous commenciez, sur chaque PC de votre compte.
- **Historique versionné dans le cloud.** Chaque sauvegarde est conservée, vous pouvez donc revenir à n'importe quelle sauvegarde antérieure — même après une panne de disque ou une installation neuve.
- **Gestion des conflits.** Hoard compare les horodatages et conserve une copie locale de tout ce qu'il remplace, donc une synchro ne détruit jamais la progression en silence.
- **Toujours open source et auto-hébergeable.** Comme Ludusavi, pas de verrouillage — utilisez Hoard Cloud ou hébergez le serveur vous-même.

## Lequel choisir ?

- Choisissez **Ludusavi** si vous voulez un outil de sauvegarde gratuit et local et que configurer votre propre cloud avec Rclone ne vous dérange pas.
- Choisissez **Hoard** si vous voulez que la sauvegarde *et* la synchro entre PC fonctionnent toutes seules, avec un historique cloud versionné, tout en gardant l'option de l'auto-hébergement.

Beaucoup commencent avec Ludusavi pour les sauvegardes locales et passent à Hoard dès qu'ils jouent aux mêmes jeux sur plus d'une machine. Si c'est votre cas, voir [comment synchroniser vos parties entre PC](/guides/sync-game-saves-across-pcs) ou simplement [téléchargez Hoard](/download) et connectez-vous.
