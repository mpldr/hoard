---
title: "Alternativa ao Ludusavi: sincronização automática de saves na nuvem"
description: "Uma comparação justa entre o Ludusavi e o Hoard. O Ludusavi é uma excelente ferramenta open source de backup local; o Hoard acrescenta sincronização na nuvem gerida e histórico versionado em todos os teus PCs — usando os mesmos dados de localização."
order: 5
updated: 2026-06-28
---

Se procuras uma forma de fazer backup e sincronizar os teus saves, é provável que tenhas encontrado o **Ludusavi** — e é excelente. Este guia é uma comparação honesta para te ajudar a escolher a ferramenta certa, e explica onde o Hoard se encaixa se quiseres sincronização na nuvem automática entre máquinas.

## O que o Ludusavi faz bem

O Ludusavi é uma ferramenta gratuita e open source (criada por mtkennerly) para fazer backup e restaurar saves de PC em Windows, macOS e Linux. Tem uma GUI limpa e uma CLI, encontra automaticamente os saves de milhares de jogos, guarda backups locais versionados e pode enviar esses backups para uma nuvem tua configurando o **Rclone** (Google Drive, Dropbox e muitos outros). Se queres controlo total e uma configuração faz-tu-mesmo, o Ludusavi é uma escolha fantástica — e completamente gratuita.

O Hoard não vem substituir isso. Na verdade, **o Hoard usa a mesma base de dados comunitária de localizações em que o Ludusavi se apoia** para localizar onde cada jogo guarda os saves, por isso a qualidade da deteção está ao mesmo nível.

## Em que o Hoard é diferente

O ponto onde a maioria esbarra com qualquer ferramenta local é a **sincronização entre dispositivos**. Com o Ludusavi fá-lo tu: agendar um backup, configurar um remoto Rclone, e depois restaurar no outro PC antes de jogar. Funciona, mas é manual.

O Hoard transforma isso em **sincronização na nuvem gerida**:

- **Inicia sessão e pronto.** Sem remotos Rclone, sem scripts. O Hoard envia o teu save depois de jogares e descarrega a versão mais recente antes de começares, em cada PC da tua conta.
- **Histórico versionado na nuvem.** Cada backup é guardado, por isso podes voltar a qualquer save anterior — mesmo depois de uma falha de disco ou de uma instalação limpa.
- **Tem em conta os conflitos.** O Hoard compara os timestamps e guarda uma cópia local de tudo o que substitui, por isso uma sincronização nunca destrói progresso em silêncio.
- **Continua open source e self-hostable.** Como o Ludusavi, não há aprisionamento — usa o Hoard Cloud ou aloja o servidor tu mesmo.

## Qual deves escolher?

- Escolhe o **Ludusavi** se queres uma ferramenta de backup gratuita e local e não te importas de montar a tua própria nuvem com o Rclone.
- Escolhe o **Hoard** se queres que o backup *e* a sincronização entre PCs simplesmente funcionem, com um histórico na nuvem versionado, mantendo a opção de self-hosting.

Muita gente começa com o Ludusavi para backups locais e passa para o Hoard quando joga os mesmos jogos em mais de uma máquina. Se é o teu caso, vê [como sincronizar saves entre PCs](/guides/sync-game-saves-across-pcs) ou simplesmente [descarrega o Hoard](/download) e inicia sessão.
