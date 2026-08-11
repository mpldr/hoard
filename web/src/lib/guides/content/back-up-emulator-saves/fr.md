---
title: "Comment sauvegarder et synchroniser les sauvegardes d'émulateur (RetroArch, Dolphin, PCSX2)"
description: "Sauvegardez et synchronisez vos fichiers de sauvegarde et vos save states d'émulateur entre PC — RetroArch, Dolphin, PCSX2, DuckStation et plus — automatiquement avec Hoard."
order: 6
updated: 2026-06-28
---

Les sauvegardes d'émulateur se perdent facilement : fichiers de sauvegarde et save states vivent dans des dossiers éparpillés, et une réinstallation ou un nouveau PC peut effacer des années de progression. Hoard les sauvegarde automatiquement et les garde synchronisées entre machines.

## Émulateurs pris en charge par Hoard

Hoard gère les fichiers de sauvegarde d'émulateur courants (`.srm`, `.sav`, cartes mémoire) et les save states des émulateurs populaires, dont :

- **RetroArch** — sauvegardes et états par cœur
- **Dolphin** (GameCube / Wii) — cartes mémoire et fichiers GCI
- **PCSX2** (PS2) — cartes mémoire
- **DuckStation / ePSXe** (PS1), **PPSSPP** (PSP), **mGBA**, et plus

Comme Hoard localise les dossiers de sauvegarde avec la même base communautaire que celle qui alimente Ludusavi, de nombreux chemins d'émulateur sont détectés automatiquement. Pour tout cas particulier, vous pouvez pointer Hoard vers un dossier à la main.

## Configurer les sauvegardes d'émulateur

1. **Installez Hoard** pour Windows, macOS ou Linux et connectez-vous.
2. Ouvrez la **Bibliothèque** et ajoutez votre émulateur, ou ajoutez son dossier de sauvegardes/états manuellement si vous avez changé l'emplacement par défaut.
3. Gardez le **mode automatique** activé. Hoard sauvegarde après chaque session et conserve un historique versionné.
4. Installez Hoard sur vos autres PC avec le même compte pour synchroniser ces sauvegardes partout — voir [synchroniser vos parties entre PC](/guides/sync-game-saves-across-pcs).

## Ludusavi pour les émulateurs ?

Ludusavi peut aussi sauvegarder les parties d'émulateur en local, et c'est une excellente option gratuite pour cela. Si vous voulez en plus que ces sauvegardes d'émulateur se synchronisent automatiquement entre machines et conservent un historique de versions cloud sans configurer Rclone, c'est là que Hoard aide — lisez la [comparaison complète Ludusavi vs Hoard](/guides/ludusavi-alternative).

## Astuce

Les save states sont liés à une version précise de l'émulateur. Gardez vos émulateurs à jour de façon cohérente sur tous vos PC pour qu'un état synchronisé se charge correctement partout.
