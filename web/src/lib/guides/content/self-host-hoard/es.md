---
title: "Cómo autoalojar Hoard con Docker (self-hosted)"
description: "Monta tu propio servidor de Hoard con Docker Compose en minutos. Código abierto, gratis y en tu hardware: una nube totalmente self-hosted para tus partidas guardadas, sin cuenta ni límite de espacio."
order: 0
featured: true
updated: 2026-06-29
---

Hoard es de código abierto y se puede autoalojar. En lugar de usar Hoard Cloud, puedes ejecutar el mismo `hoard-server` en tu propia máquina y apuntar todos tus dispositivos a él: sin cuenta y sin más límite de espacio que el disco que le des. Esta guía deja un servidor funcionando con Docker en pocos minutos.

## Por qué autoalojar Hoard

- **Control total.** Tus partidas viven en hardware que tú controlas, no en la nube de otro.
- **Sin cuota.** El espacio solo lo limita tu propio disco.
- **La misma app, las mismas funciones.** El historial versionado y la sincronización en segundo plano funcionan igual que con Hoard Cloud; solo cambia el backend.
- **Código abierto.** Puedes leer, auditar y modificar el servidor.

Esta es la diferencia clave frente a herramientas como [Ludusavi](/guides/ludusavi-alternative): Ludusavi es excelente para copias locales y para usar tu propia nube vía Rclone, pero la sincronización la montas tú. Hoard te da un servidor de sincronización gestionado que arrancas una vez y al que se conectan todos los dispositivos.

## Qué necesitas

- Una máquina que esté siempre encendida (un servidor casero, un NAS que ejecute Docker o un VPS pequeño).
- Docker y Docker Compose instalados.
- Opcionalmente un dominio y un proxy inverso para HTTPS (recomendado para cualquier cosa fuera de tu red local).

## Instalación con Docker Compose

Clona el repositorio, crea una configuración a partir del ejemplo y arranca el stack:

```sh
git clone https://github.com/rleeon/hoard.git && cd hoard
mkdir -p deploy/docker/config
cp deploy/config.toml.example deploy/docker/config/config.toml
$EDITOR deploy/docker/config/config.toml      # Use nano or vim or something lol

cd deploy/docker
docker compose up -d
docker compose logs -f                         # wait for "listening"
```

Espera a que los logs muestren que el servidor está escuchando. Los datos se guardan en un volumen de Docker (`hoard-data`); haz copia de seguridad como con cualquier otro volumen. El contenedor escucha internamente en el puerto `12421`; usa otro puerto del host con `HOARD_PORT=9000 docker compose up -d`.

## Crea tu usuario y un token de dispositivo

El servidor no tiene pantalla de registro: los usuarios se crean por línea de comandos:

```sh
docker compose exec server hoard-admin --config /etc/hoard/config.toml \
    user create alice --admin --password 'CHANGE_ME'
docker compose exec server hoard-admin --config /etc/hoard/config.toml \
    token create alice --device 'desktop'
```

El token se muestra una sola vez y **no se puede recuperar después**, así que cópialo ahora.

## Conecta la aplicación de escritorio

Instala la [app de escritorio de Hoard](/download) en cada equipo. En el asistente inicial elige **Self-Host**, y pega la URL de tu servidor y el token que acabas de crear. A partir de ahí se comporta igual que Hoard Cloud: detecta tus juegos, copia las partidas automáticamente y mantiene el historial versionado. Consulta [sincronizar partidas entre varios PC](/guides/sync-game-saves-across-pcs) para el día a día.

## Llevarlo a producción

Para cualquier cosa expuesta fuera de tu red local, termina el TLS en un proxy inverso (Caddy, nginx o Traefik). ¿Prefieres bare metal? El repositorio también incluye un script de instalación con `systemd` y un comando `hoard-server upgrade` que cambia el binario de forma atómica sin cortar una sincronización en curso.

## ¿Self-hosted o Hoard Cloud?

Autoalojar es ideal si ya tienes un servidor y quieres control total sin límites. Si prefieres no mantener infraestructura, [Hoard Cloud](/pricing) te da la misma sincronización gestionada por nosotros, con un plan gratuito para empezar. En cualquier caso, la app y tus partidas siguen siendo portables: puedes cambiar más adelante.
