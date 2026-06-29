---
title: "Come self-hostare Hoard con Docker"
description: "Avvia il tuo server Hoard in pochi minuti con Docker Compose. Open source, gratuito, sul tuo hardware: un cloud completamente self-hosted per i salvataggi dei giochi, senza account né limiti di spazio."
order: 0
featured: true
updated: 2026-06-29
---

Hoard è open source e self-hostabile. Invece di usare Hoard Cloud, puoi eseguire lo stesso `hoard-server` sulla tua macchina e puntarci ogni dispositivo — senza account e senza limiti di spazio oltre al disco che gli dai. Questa guida mette in piedi un server con Docker in pochi minuti.

## Perché self-hostare Hoard

- **Controllo totale.** I tuoi salvataggi vivono su hardware che controlli tu, non sul cloud altrui.
- **Nessun limite.** Lo spazio è limitato solo dal tuo disco.
- **Stessa app, stesse funzioni.** Cronologia versionata e sync in background funzionano come con Hoard Cloud — cambia solo il backend.
- **Open source.** Puoi leggere, verificare e modificare il server.

È la differenza chiave rispetto a strumenti come [Ludusavi](/guides/ludusavi-alternative): Ludusavi è ottimo per i backup locali e per il cloud «porta il tuo» tramite Rclone, ma la sincronizzazione la configuri tu. Hoard ti dà un server di sync gestito che avvii una volta e a cui si collega ogni dispositivo.

## Cosa ti serve

- Una macchina sempre accesa (un server domestico, un NAS che esegue Docker o un piccolo VPS).
- Docker e Docker Compose installati.
- Facoltativamente un dominio e un reverse proxy per l'HTTPS (consigliato per tutto ciò che esce dalla rete locale).

## Installazione con Docker Compose

Clona il repository, crea una configurazione dall'esempio, imposta il tuo `public_url` e avvia lo stack:

```sh
git clone https://github.com/rleeon/hoard.git && cd hoard
mkdir -p deploy/docker/config
cp deploy/config.toml.example deploy/docker/config/config.toml
$EDITOR deploy/docker/config/config.toml      # set public_url at minimum

cd deploy/docker
docker compose up -d --build
docker compose logs -f                         # wait for "listening"
```

Attendi che i log mostrino che il server è in ascolto. I dati vivono in un volume Docker (`hoard-data`): eseguine il backup come per qualsiasi volume. Il container ascolta internamente sulla porta `8080`; usa un'altra porta host con `HOARD_PORT=9000 docker compose up -d`.

## Crea il tuo utente e un token dispositivo

Il server non ha una schermata di registrazione: gli utenti si creano da riga di comando:

```sh
docker compose exec server hoard-admin --config /etc/hoard/config.toml \
    user create alice --admin --password 'CHANGE_ME'
docker compose exec server hoard-admin --config /etc/hoard/config.toml \
    token create alice --device 'desktop'
```

Il token viene mostrato una sola volta e **non può essere recuperato in seguito**, quindi copialo ora.

## Collega l'app desktop

Installa l'[app desktop di Hoard](/download) su ogni macchina. Nella procedura iniziale scegli **Autohost**, poi incolla l'URL del server e il token appena creato. Da lì si comporta esattamente come Hoard Cloud: rileva i giochi, salva automaticamente e mantiene la cronologia versionata. Vedi [sincronizzare i salvataggi tra più PC](/guides/sync-game-saves-across-pcs) per l'uso quotidiano.

## In produzione

Per tutto ciò che è esposto oltre la rete locale, termina il TLS su un reverse proxy (Caddy, nginx o Traefik) e imposta `public_url` sul tuo vero indirizzo HTTPS. Preferisci il bare metal? Il repository include anche uno script di installazione `systemd` e un comando `hoard-server upgrade` che sostituisce il binario in modo atomico senza interrompere una sync in corso.

## Self-host o Hoard Cloud?

Il self-hosting è ideale se hai già un server e vuoi controllo totale senza limiti. Se preferisci non gestire infrastruttura, [Hoard Cloud](/pricing) ti dà la stessa sincronizzazione gestita da noi, con un piano gratuito per iniziare. In ogni caso app e salvataggi restano portabili: puoi cambiare in seguito.
