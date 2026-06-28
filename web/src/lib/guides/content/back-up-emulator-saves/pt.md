---
title: "Como fazer backup e sincronizar saves de emuladores (RetroArch, Dolphin, PCSX2)"
description: "Faz backup e sincroniza os ficheiros de save e os save states dos teus emuladores entre PCs — RetroArch, Dolphin, PCSX2, DuckStation e mais — automaticamente com o Hoard."
order: 5
updated: 2026-06-28
---

Os saves de emulador perdem-se com facilidade: ficheiros de save e save states vivem em pastas espalhadas, e uma reinstalação ou um PC novo podem apagar anos de progresso. O Hoard faz-lhes backup automaticamente e mantém-nos sincronizados entre máquinas.

## Emuladores com que o Hoard funciona

O Hoard trata os ficheiros de save padrão de emulador (`.srm`, `.sav`, memory cards) e os save states dos emuladores populares, incluindo:

- **RetroArch** — saves e estados por core
- **Dolphin** (GameCube / Wii) — memory cards e ficheiros GCI
- **PCSX2** (PS2) — memory cards
- **DuckStation / ePSXe** (PS1), **PPSSPP** (PSP), **mGBA** e mais

Como o Hoard localiza as pastas de save com a mesma base de dados comunitária que alimenta o Ludusavi, muitos caminhos de emulador são detetados automaticamente. Para qualquer caso personalizado, podes apontar o Hoard para uma pasta à mão.

## Configurar backups de saves de emulador

1. **Instala o Hoard** para Windows, macOS ou Linux e inicia sessão.
2. Abre a **Biblioteca** e adiciona o teu emulador, ou adiciona manualmente a sua pasta de saves/estados se mudaste a localização predefinida.
3. Mantém o **modo automático** ligado. O Hoard faz backup depois de cada sessão e guarda um histórico versionado.
4. Instala o Hoard nos teus outros PCs com a mesma conta para sincronizar esses saves em todo o lado — vê [sincronizar saves entre PCs](/guides/sync-game-saves-across-pcs).

## Ludusavi para emuladores?

O Ludusavi também pode fazer backup de saves de emulador localmente, e é uma excelente opção gratuita para isso. Se queres, além disso, que esses saves de emulador sincronizem automaticamente entre máquinas e mantenham um histórico de versões na nuvem sem configurar o Rclone, é aí que o Hoard ajuda — lê a [comparação completa Ludusavi vs Hoard](/guides/ludusavi-alternative).

## Dica

Os save states estão ligados a uma versão específica do emulador. Mantém os teus emuladores atualizados de forma coerente em todos os PCs para que um estado sincronizado carregue bem em todo o lado.
