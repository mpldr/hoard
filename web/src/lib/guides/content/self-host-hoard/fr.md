---
title: "Comment auto-héberger Hoard avec Docker (self-hosted)"
description: "Lancez votre propre serveur Hoard en quelques minutes avec Docker Compose. Open source, gratuit, sur votre matériel : un cloud entièrement auto-hébergé pour vos sauvegardes de jeux, sans compte ni quota."
order: 0
featured: true
updated: 2026-06-29
---

Hoard est open source et auto-hébergeable. Au lieu d'utiliser Hoard Cloud, vous pouvez exécuter le même `hoard-server` sur votre propre machine et y connecter chaque appareil — sans compte, sans quota au-delà du disque que vous lui donnez. Ce guide met un serveur en route avec Docker en quelques minutes.

## Pourquoi auto-héberger Hoard

- **Maîtrise totale.** Vos sauvegardes vivent sur du matériel que vous contrôlez, pas sur le cloud d'un autre.
- **Aucun quota.** L'espace n'est limité que par votre propre disque.
- **Même app, mêmes fonctions.** L'historique versionné et la synchro en arrière-plan fonctionnent comme avec Hoard Cloud — seul le backend change.
- **Open source.** Vous pouvez lire, auditer et modifier le serveur.

C'est la différence clé avec des outils comme [Ludusavi](/guides/ludusavi-alternative) : Ludusavi est excellent pour les sauvegardes locales et le cloud « apportez le vôtre » via Rclone, mais c'est à vous de câbler la synchro. Hoard vous donne un serveur de synchro géré que vous lancez une fois et auquel chaque appareil se connecte.

## Ce qu'il vous faut

- Une machine qui reste allumée (serveur maison, NAS exécutant Docker ou petit VPS).
- Docker et Docker Compose installés.
- Éventuellement un nom de domaine et un reverse proxy pour le HTTPS (recommandé au-delà de votre réseau local).

## Installation avec Docker Compose

Clonez le dépôt, créez une configuration depuis l'exemple et démarrez la pile :

```sh
git clone https://github.com/rleeon/hoard.git && cd hoard
mkdir -p deploy/docker/config
cp deploy/config.toml.example deploy/docker/config/config.toml
$EDITOR deploy/docker/config/config.toml      # Use nano or vim or something lol

cd deploy/docker
docker compose up -d
docker compose logs -f                         # wait for "listening"
```

Attendez que les logs indiquent que le serveur écoute. Les données vivent dans un volume Docker nommé (`hoard-data`) — sauvegardez-le comme n'importe quel volume. Le conteneur écoute en interne sur le port `12421` ; choisissez un autre port hôte avec `HOARD_PORT=9000 docker compose up -d`.

## Créez votre utilisateur et un jeton d'appareil

Le serveur n'a pas d'écran d'inscription — vous créez les utilisateurs en ligne de commande :

```sh
docker compose exec server hoard-admin --config /etc/hoard/config.toml \
    user create alice --admin --password 'CHANGE_ME'
docker compose exec server hoard-admin --config /etc/hoard/config.toml \
    token create alice --device 'desktop'
```

Le jeton n'est affiché qu'une fois et **ne peut pas être récupéré ensuite**, copiez-le maintenant.

## Connectez l'application de bureau

Installez l'[app de bureau Hoard](/download) sur chaque machine. Dans l'assistant, choisissez **Autohost**, puis collez l'URL de votre serveur et le jeton que vous venez de créer. Ensuite, le comportement est identique à Hoard Cloud : détection des jeux, sauvegarde automatique et historique versionné. Voir [synchroniser ses sauvegardes entre PC](/guides/sync-game-saves-across-pcs) pour l'usage quotidien.

## En production

Pour tout ce qui dépasse votre réseau local, terminez le TLS sur un reverse proxy (Caddy, nginx ou Traefik). Plutôt bare metal ? Le dépôt fournit aussi un script d'installation `systemd` et une commande `hoard-server upgrade` qui remplace le binaire de façon atomique sans interrompre une synchro en cours.

## Auto-hébergement ou Hoard Cloud ?

L'auto-hébergement est idéal si vous avez déjà un serveur et voulez un contrôle total sans quota. Si vous préférez ne pas gérer d'infrastructure, [Hoard Cloud](/pricing) vous offre la même synchro gérée pour vous, avec une offre gratuite pour démarrer. Dans les deux cas, l'app et vos sauvegardes restent portables — vous pouvez changer plus tard.
