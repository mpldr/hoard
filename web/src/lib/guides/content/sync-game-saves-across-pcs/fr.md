---
title: "Comment synchroniser vos parties entre plusieurs PC"
description: "Jouez au même jeu sur votre fixe et votre portable sans perdre votre progression. Synchronisez vos parties entre PC automatiquement avec Hoard — une synchro cloud gérée, sans configurer Ludusavi et Rclone à la main."
order: 2
updated: 2026-06-28
---

Si vous jouez sur plus d'un ordinateur — un fixe à la maison et un portable en déplacement — Hoard garde vos sauvegardes synchronisées pour que vous repreniez toujours là où vous en étiez.

## Comment fonctionne la synchronisation

Hoard sauvegarde chaque partie vers votre cloud et récupère la dernière version sur vos autres machines. Quand vous finissez de jouer sur un PC, la sauvegarde la plus récente vous attend sur le suivant.

## Configurer la synchronisation

1. Installez **Hoard** sur chaque PC où vous jouez (Windows, macOS ou Linux).
2. Connectez-vous avec le **même compte** sur chaque machine, ou reliez-les au même serveur auto-hébergé.
3. Ajoutez les mêmes jeux à votre **Bibliothèque** sur chaque PC. Hoard les associe par jeu, donc une sauvegarde faite sur l'un apparaît sur les autres.
4. Gardez le **mode automatique** activé. Hoard envoie après que vous jouez et télécharge la dernière version avant que vous commenciez.

## Vous venez de Ludusavi ?

Ludusavi est un excellent outil open source pour sauvegarder et restaurer des parties en local, et il peut envoyer ces sauvegardes vers un cloud que vous configurez vous-même avec Rclone. Mais la synchro entre appareils, vous la montez à la main : planifier la sauvegarde, configurer le distant, puis restaurer sur l'autre PC avant de jouer.

Hoard transforme cela en synchro gérée. Il utilise les mêmes données communautaires d'emplacements que Ludusavi pour trouver vos sauvegardes, puis envoie après chaque session et télécharge la dernière version avant la suivante — sur chaque PC de votre compte, avec un historique versionné dans le cloud. Pas de distants Rclone, pas de scripts. Et comme Ludusavi, Hoard est open source et peut être auto-hébergé. Voir la [comparaison complète avec Ludusavi](/guides/ludusavi-alternative).

## Éviter les conflits

Hoard gère les conflits : il compare les dates de modification et conserve une copie locale de toute sauvegarde remplacée, donc une synchro ne détruit jamais la progression en silence. Si un jeu tourne encore ou qu'une sauvegarde a été modifiée il y a quelques minutes, Hoard attend.

## Astuce

Laissez chaque machine finir de synchroniser avant de lancer un jeu — le tableau de bord affiche l'état en direct, vous savez donc que la dernière sauvegarde est en place.
