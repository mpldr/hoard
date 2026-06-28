---
title: "Come sincronizzare i salvataggi tra più PC"
description: "Gioca allo stesso gioco su fisso e portatile senza perdere progressi. Sincronizza i tuoi salvataggi tra PC automaticamente con Hoard — sincronizzazione cloud gestita, senza configurare Ludusavi e Rclone a mano."
order: 2
updated: 2026-06-28
---

Se giochi su più di un computer — un fisso a casa e un portatile in giro — Hoard mantiene i salvataggi sincronizzati così riprendi sempre da dove avevi lasciato.

## Come funziona la sincronizzazione

Hoard fa il backup di ogni salvataggio sul tuo cloud e scarica l'ultima versione sulle altre macchine. Quando finisci di giocare su un PC, il salvataggio più recente ti aspetta sul successivo.

## Imposta la sincronizzazione

1. Installa **Hoard** su ogni PC su cui giochi (Windows, macOS o Linux).
2. Accedi con lo **stesso account** su ogni macchina, o collegale allo stesso server self-hosted.
3. Aggiungi gli stessi giochi alla **Libreria** su ogni PC. Hoard li abbina per gioco, così un salvataggio fatto su uno appare sugli altri.
4. Tieni attiva la **modalità automatica**. Hoard carica dopo che giochi e scarica l'ultima versione prima che inizi.

## Arrivi da Ludusavi?

Ludusavi è un ottimo strumento open source per fare backup e ripristinare salvataggi in locale, e può inviare quei backup a un cloud che configuri tu stesso con Rclone. Ma la sincronizzazione tra dispositivi la imposti a mano: programmare il backup, configurare il remoto, poi ripristinare sull'altro PC prima di giocare.

Hoard trasforma tutto questo in sincronizzazione gestita. Usa gli stessi dati comunitari di posizione di Ludusavi per trovare i tuoi salvataggi, poi carica dopo ogni sessione e scarica l'ultima versione prima della successiva — su ogni PC del tuo account, con cronologia versionata nel cloud. Niente remoti Rclone, niente script. E come Ludusavi, Hoard è open source e può essere self-hosted. Vedi il [confronto completo con Ludusavi](/guides/ludusavi-alternative).

## Evitare i conflitti

Hoard è consapevole dei conflitti: confronta le date di modifica e conserva una copia locale di ogni salvataggio sostituito, così una sincronizzazione non distrugge mai i progressi in silenzio. Se un gioco è ancora aperto o un salvataggio è stato toccato negli ultimi minuti, Hoard aspetta.

## Suggerimento

Lascia che ogni macchina finisca di sincronizzare prima di avviare un gioco — la dashboard mostra lo stato in tempo reale, così sai che l'ultimo salvataggio è al suo posto.
