---
title: "Cómo hacer copia y sincronizar partidas de emuladores (RetroArch, Dolphin, PCSX2)"
description: "Haz copia y sincroniza los archivos de guardado y los estados guardados de tus emuladores entre PC —RetroArch, Dolphin, PCSX2, DuckStation y más— automáticamente con Hoard."
order: 5
updated: 2026-06-28
---

Las partidas de emulador se pierden con facilidad: los archivos de guardado y los estados guardados viven en carpetas dispersas, y una reinstalación o un PC nuevo pueden borrar años de progreso. Hoard hace la copia automáticamente y los mantiene sincronizados entre equipos.

## Emuladores con los que funciona Hoard

Hoard gestiona los archivos de guardado estándar de emulador (`.srm`, `.sav`, memory cards) y los estados guardados de los emuladores populares, entre ellos:

- **RetroArch** — guardados y estados por núcleo
- **Dolphin** (GameCube / Wii) — memory cards y archivos GCI
- **PCSX2** (PS2) — memory cards
- **DuckStation / ePSXe** (PS1), **PPSSPP** (PSP), **mGBA** y más

Como Hoard localiza las carpetas de guardado con la misma base de datos comunitaria que utiliza Ludusavi, muchas rutas de emulador se detectan automáticamente. Para cualquier ruta personalizada, puedes apuntar Hoard a una carpeta a mano.

## Configura la copia de partidas de emulador

1. **Instala Hoard** para Windows, macOS o Linux e inicia sesión.
2. Abre la **Biblioteca** y añade tu emulador, o añade manualmente su carpeta de guardados/estados si has cambiado la ubicación por defecto.
3. Mantén el **modo automático** activado. Hoard hace la copia tras cada sesión y guarda un historial versionado.
4. Instala Hoard en tus otros PC con la misma cuenta para sincronizar esas partidas en todas partes; mira [cómo sincronizar partidas entre PC](/guides/sync-game-saves-across-pcs).

## ¿Ludusavi para emuladores?

Ludusavi también puede hacer copia de partidas de emulador en local, y es una gran opción gratuita para eso. Si además quieres que esas partidas de emulador se sincronicen automáticamente entre equipos y mantengan un historial de versiones en la nube sin configurar Rclone, ahí es donde ayuda Hoard; lee la [comparativa completa entre Ludusavi y Hoard](/guides/ludusavi-alternative).

## Consejo

Los estados guardados dependen de una versión concreta del emulador. Mantén tus emuladores actualizados de forma coherente entre PC para que un estado sincronizado cargue bien en todas partes.
