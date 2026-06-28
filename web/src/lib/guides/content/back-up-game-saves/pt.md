---
title: "Como fazer backup dos teus saves automaticamente"
description: "Configura backups na nuvem automáticos e versionados dos teus saves de PC com o Hoard — para que uma falha, uma reinstalação ou um mod com problemas nunca apaguem o teu progresso."
order: 1
updated: 2026-06-28
---

Perder um save significa perder horas de progresso. O Hoard faz backup dos teus saves de PC automaticamente e guarda um histórico completo de versões, para que possas sempre voltar atrás.

## O que o Hoard guarda

O Hoard deteta as pastas de save dos jogos a que jogas e copia-as para a tua própria nuvem — Hoard Cloud ou um servidor que alojes tu mesmo. Cada backup é versionado, por isso as cópias antigas nunca são sobrescritas.

Para encontrar onde cada jogo guarda os saves, o Hoard usa a mesma base de dados comunitária de localizações que alimenta o Ludusavi, por isso a deteção funciona logo para milhares de títulos. A diferença está no que acontece a seguir: em vez de deixar o backup no teu disco, o Hoard versiona-o automaticamente na nuvem.

## Configurar backups automáticos

1. **Descarrega e instala o Hoard** para Windows, macOS ou Linux a partir da página de download.
2. Inicia sessão, ou aponta a app para o teu servidor self-hosted.
3. Abre a **Biblioteca**. O Hoard procura jogos instalados e lista os saves que encontra.
4. Adiciona os jogos que queres proteger. O Hoard localiza cada pasta de save automaticamente; podes adicionar um caminho à mão se um jogo não for detetado.
5. Deixa o **modo automático** ligado. O Hoard vigia as pastas de save e faz backup quando paras de jogar.

A partir daí cada sessão é capturada sem que faças nada.

## Dica: verifica o teu histórico

Abre o separador **Histórico** de um jogo para ver cada backup com data e tamanho. A partir daí podes restaurar qualquer versão anterior com um clique. Os teus saves viajam cifrados, são guardados na UE, e podes exportá-los ou apagá-los quando quiseres.

Já usas uma ferramenta de backup local como o Ludusavi? Podes mantê-la — mas se queres que esses backups cheguem à nuvem e sincronizem entre máquinas sem configurares o Rclone tu mesmo, é exatamente isso que o Hoard automatiza. Vê [Ludusavi vs Hoard](/guides/ludusavi-alternative) para uma comparação justa.
