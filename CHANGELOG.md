# Changelog

All notable changes to this project will be documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added
- **Self-hosted backups only upload what changed.** Your server has always
  stored each file once and let versions share the bytes — but every backup still
  sent the whole folder and the server threw away the part it already had. A 3 GB
  save that changed 10 MB cost 3 GB of upload, every time. The client now tells
  the server what the version contains, the server answers which files it's
  missing, and only those travel. A second backup of the same game moves
  megabytes. It also stops a big save from arriving as one enormous request,
  which is what used to collide with `max_snapshot_size_mb` and with the body
  limit of any reverse proxy in front — nginx, a Synology's built-in one, a
  Cloudflare hostname. Nothing to configure: the server announces it and older
  clients keep working against the same server. (Hoard Cloud has worked this way
  since launch.)

- **Your own server now knows your machines.** The Eye panel used to show only
  the computer you were sitting at — the list of other devices was never wired
  up, on either deployment. Now it shows every machine on the account: which are
  on right now, what each is playing and for how long. Self-hosted included, and
  there it stays entirely between your machines and your server: the census
  lives in your own database and nothing about it is sent anywhere. Machines
  identify themselves by a stable fingerprint, so reinstalling doesn't duplicate
  them, and one that goes months without appearing is forgotten.

### Fixed
- **Save folders are no longer synced whole, junk and all.** A game's save
  folder rarely holds only saves: sitting next to your world files there are
  engine logs, crash telemetry, the analytics queue with the GUID that
  identifies *that* installation, shader information about *that* GPU, and
  settings files carrying *that* monitor's resolution. Hoard swept all of it
  into the snapshot and wrote all of it back on restore, which is how a save
  restored from one machine can crash the game on another. Now every file in
  the folder is sorted before it moves. Logs, crash dumps, temporary files, OS
  clutter and engine telemetry stay out of the backup entirely. Settings files
  are backed up as before — losing them is not an option — but a restore no
  longer writes them over the machine you're restoring onto unless you ask:
  there's a checkbox in the restore dialog, off by default, and `--allow-ini`
  on the CLI. Your save files themselves are unaffected either way. Two things
  fall out of it: a Unity game whose `Player.log` is rewritten at every launch
  used to cut a fresh cloud version on every single launch even when you hadn't
  played, and that stops; and the save catalog's own file patterns (`*.sav`,
  `*.plr` — 20,499 of them) are now read and used to protect anything the
  catalog says is real save data, so a game that genuinely saves into `.ini` or
  `.log` files is left alone.
- **Dropping from Pro to Free deleted your history without warning.** The
  grace window that was supposed to give you a month before a smaller plan takes
  effect never ran on the one downgrade that matters: the code worked out "how
  much room do you have today" using the plan you were moving *to*, so a Pro→Free
  drop looked like it changed nothing, the limit collapsed the same second, and
  the auto-purge started deleting old versions immediately. The window is now
  real — your old limit is frozen in place until the date, nothing is purged
  meanwhile, and the app counts down to it.
- **A full account failed one upload at a time, forever.** Hitting the storage
  limit surfaced as a raw server error per game (the JSON body, verbatim, in the
  activity panel) and every save kept retrying against a wall only you can move.
  It's now a state of its own: uploads park for an hour, the panel says what's
  happening in one line instead of once per game, and the row carries the button
  that opens "free up space" — which used to be buried in Account, three screens
  from wherever you were when it happened.
- **"Free up space" couldn't see space shared between two games.** When the same
  folder ends up tracked twice (it happens: the slug can change under you), both
  copies point at the same stored bytes, which belong exclusively to neither — so
  both reported "0 bytes to free" and archiving either one freed nothing. Those
  bytes are now counted and the pair is flagged as the duplicate it is. On the
  account that turned this up it was 1.25 GB: 60% of a Free plan, invisible.
- **"Free up space" picked your games for you.** It archived the heaviest ones
  until the numbers worked. Now it proposes that as a starting point and lets you
  tick what actually goes, with a live meter showing where your account lands —
  and it says so plainly when archiving everything still wouldn't be enough.

## [1.1.2] — 2026-08-07

### Added
- **Every version now tells you which machine it came from.** The history of a
  save listed a date and a size and left you to guess whether that snapshot was
  the desktop's or the laptop's — which is the one thing you actually want to
  know before restoring one. Each version now carries the name of the machine
  that made it, and the history shows it.
- **A heads-up display over the game.** Alt+H brings up a panel on top of
  whatever you are playing, in the shape of the Steam overlay, showing what the
  sync engine is doing right now: what it backed up, when, and whether anything
  is waiting. It reads the engine and nothing more — there is no button on it
  that can touch your saves — and Alt+H puts it away again.
- **The Hoard Screen scope can be bound to a mouse or keyboard button.** The
  magnifier used to live in the overlay's own controls; you can now put it on a
  button and choose how it behaves — press to toggle, hold while you aim, or
  show it for a fixed moment. Extra mouse buttons work as bindings too.
- **Scanning a folder you point at yourself.** Telling Hoard "the saves are in
  here" now means exactly that: it looks inside the folder you chose and offers
  what it finds, without applying the size and name rules it uses when guessing
  on its own — those rules exist for scanning your whole disk unattended, and
  they were throwing away folders you had explicitly pointed at. The three
  slightly different ways of adding a save by hand are now one dialog.
- **One install, whatever your machine is.** Hoard now installs and updates as
  a set of components rather than as "the app" or "the CLI": the installer works
  out which pieces your machine wants and puts them all in at the same version,
  in one pass. A NAS or a server stops at the engine and the terminal; a desktop
  or a Steam Deck gets the app as well. Upgrades move everything together, so
  the pieces can't drift apart, and the app now ships the terminal command with
  it instead of leaving it as a separate download.
- **Saves sync in game mode on SteamOS, Bazzite and CachyOS.** The sync engine
  is now installed in its own right instead of riding inside the app bundle, so
  it can start with your session on systems where the app has to be an AppImage
  — which is every immutable/atomic image, the Steam Deck included. Nothing to
  keep open and nothing to launch: install once from the desktop and game mode
  syncs on its own.

### Changed
- **A pass over the app's surfaces.** Behaviour on hover and focus, the
  contrast of the greys, the depth of panels and cards, and how versions are
  laid out in history all got a revision, with a control for how pronounced the
  relief is. Covers and the Library frames stayed as they were.

### Fixed
- **A save that the game rotated mid-upload could be stored corrupt.** Many
  games write a new save by renaming the old one out of the way, and if that
  happened between Hoard reading a file and finishing sending it, what reached
  the server was half of one file and half of another — a version that looked
  fine in the list and could not be restored. Hoard now checks what it actually
  sent, byte for byte, and aborts the whole snapshot if a file moved underneath
  it, so a bad version is never committed. The next backup picks up the new
  contents normally. **If you have used Hoard on a game that rotates saves,
  this is the fix to update for.**
- **"Hoard already tracks this folder" on a folder it did not track.** A save's
  identity was tied to a name derived from the game's title, and that name is
  not stable — the same game could be `vrising` on one machine and `v-rising`
  on another, or gain a year in the catalogue. Two different folders could
  collide on it and Hoard would refuse to add the second, with the only way out
  being to untrack and re-add. Identity is now the folder itself, which is what
  it always meant.
- **Restoring into an empty folder could loop.** If the folder a save lives in
  was empty — a fresh machine, a game reinstalled — the restore bypassed the
  check that decides whether there is anything to do, and a restore that wrote
  nothing still reported success, so it started again immediately. One account
  moved 10.6 GB this way. Both halves are fixed: the empty folder no longer
  skips the check, and a restore that writes nothing is a failure.
- **Hoard could be pointed at a folder no backup should ever cover.** Nothing
  stopped you from tracking a Wine prefix root, a `Documents`, or a home
  directory — a mistake that turns the next backup into an attempt to upload
  everything you own. Those roots are now refused, the Windows rules apply
  inside Proton prefixes as well as outside, and the check sits on the path the
  backup actually takes rather than only in the dialog.
- **Self-hosted: rebuilding your server left invisible duplicate rows.** A
  rebuilt server hands out new identifiers, and re-adding a save created a
  second row while the old one stayed behind — not shown anywhere, still
  holding a claim on the folder, and answering 404 for every sync. The stale
  rows are now dropped when the library is listed.
- **Self-hosted: an update could leave the server unable to find its own
  database.** The Docker stack shipped a `config.toml` in the repository, so a
  `git pull` overwrote yours — including `data_dir`, which is where your saves
  and your database are. A server that starts against an empty database where
  a populated one is expected now refuses to run and says so, and the config
  file is no longer versioned. Copy `deploy/config.toml.example` once, as the
  self-host guide says, and updates stop touching it.
- **404s from guessing what kind of server was on the other end.** When the
  check that asks a server what it is could not reach it, the client assumed
  self-hosted and spoke the wrong dialect to a Hoard Cloud server, which
  answered 404 to everything. An unreachable server and a self-hosted one are
  now two different answers, and the client waits for a real one.
- **Windows: a black console window at every sign-in.** The sync service was
  built as a console program, so Windows opened a terminal for it when the
  scheduled task started it with your session. It is a windowless program now.
- **Detection got a broad overhaul.** Eighteen changes to how Hoard works out
  where a game keeps its saves — following a game's launch command through
  wrappers to the process that actually runs, resolving base-folder references,
  handling saves that are a single loose file rather than a folder, and more.
  The bundled catalogue also went from 7.3 MB to 1.7 MB.
- **Server: a stuck compression job retried every five minutes, forever.** Six
  stored objects had been failing to compress since July with no terminal
  state, so the sweep picked them up again on every pass. Attempts are now
  counted and capped.
- **Server: rate-limit responses are readable from a browser again.** The
  limiter sat outside the layer that adds the cross-origin headers, so its 429
  never carried them and the web only ever saw "network error" — precisely when
  knowing the real status matters. The order is now the other way round, and
  the preflight no longer counts against your quota.
- **Diagnostic reports were never actually being sent.** Hoard has had a
  diagnostics channel since 1.0, on by default, and it has never delivered a
  single line: it looked for your session in the wrong place, so on a Hoard
  Cloud machine it found nothing and gave up, every time. It works now, and the
  reports carry what makes a bug findable — including where detection got a
  game's save folder wrong and how you fixed it, which until now only reached us
  when somebody wrote in on Discord. Two things changed alongside it: paths are
  stripped of your username before they leave your machine (`C:\Users\<user>\…`
  is what arrives), and the Settings toggle now says what is actually sent
  instead of promising "anonymous pings" that never leave out paths or game
  names. It is still one switch, still on by default, and turning it off still
  stops the stream within seconds.
- **Self-hosting on OneDrive, Mega, Google Drive or Dropbox actually works
  now.** The guide has pointed at `rclone serve s3` as the way to keep saves on
  a cloud drive you already pay for, without ever explaining how to set it up —
  and worse, going that route quietly stored every save wrong. The uploads were
  being framed in a way that AWS, R2 and MinIO unwrap and the rclone bridge does
  not, so what landed in your drive was not what was sent, and you'd only find
  out the day you needed a restore. The server now speaks the plainest version
  of the protocol, checks at startup that the storage it was given returns
  exactly the bytes it wrote (and refuses to start if not), and the self-host
  guide has a step-by-step section for each provider, including what the
  trade-offs are.
- **A backup to remote storage no longer holds up everyone else's.** While one
  save was uploading to an S3 bucket or a cloud drive, the rest of the server's
  writes queued behind it and started failing outright if it took more than a
  few seconds — which, on a consumer drive, it does. Uploads now happen outside
  the database lock.
- **Restoring from remote storage stopped needing a copy of the whole save on
  the server's disk.** A restore staged every file of the snapshot locally
  before sending any of it, so a 10 GB library needed 10 GB free on the server.
  It now streams a piece at a time (a few MB), and a download that gets cut
  short is reported as an error instead of quietly producing a short file.
- **The terminal install could not sync at all.** Since 1.1.0 `hoard` has been a
  thin client of the sync service, but the published tarball only ever contained
  `hoard` — never the `hoardd` engine it talks to. Installing from the terminal
  produced a command that could not start or reach a service, which is exactly
  what the headless install is for. The tarball now carries both halves, and CI
  refuses to publish one without the other.
- **Which engine ran no longer depends on who started it.** With the app and the
  terminal install both present, the running engine could be either copy
  depending on `PATH` order and which client woke it. The installed service is
  now the single authority on that, and clients follow it.
- **In-app updates on SteamOS, Bazzite and other atomic systems.** The updater
  offered an `.rpm` on any machine with `rpm` present, including images whose
  `/usr` is read-only — so the download succeeded and the install could not
  possibly apply. It now picks the format the machine can actually install, the
  same way the terminal installer does.

## [1.1.1] — 2026-08-02

### Added
- **A shareable card at the bottom of Hoard-Wrapped.** The recap now ends in a
  wide camera button that opens your card: photo, name, a random line that
  riffs on your most-played game (22 games, eight languages — play a Fallout
  and it says "war never changes"), your stats for the last week, month or
  year, and a row of cubes for that range (a week is seven big ones). Photo,
  name, line and range are editable and stay **on this device only** — nothing
  is uploaded or synced. A separate camera button takes the shot and drops the
  PNG in your gallery (`Pictures/Hoard/`), branded with hoard.services both on
  the image and in its PNG metadata.
- **Link a cloud save by picking the game, not the folder.** "Link to this
  machine" now lists the games detection already found here, best name match
  first, so a save synced from another device can be bound in one click.
  Games whose folder another save already tracks are left out, and the folder
  picker stays as the fallback for what detection genuinely missed.

### Fixed
- **Self-hosted sync was dead in 1.1.0 if you only ever signed in through the
  app.** Moving the engine into the background service also moved where it
  looks for your session: it read only `config.toml`, which just the CLI
  (`hoard login --token`) writes, while the app keeps its own. So the service
  started with no session, no save was ever backed up, and all the window could
  say was "the sync service is offline". The service now uses the app's session
  first and keeps `config.toml` as the headless fallback — nothing to redo, it
  picks up the session you already have on the next start.
- **"The sync service is offline" now says why, and offers the fix.** The
  reason existed inside the service and was dropped on the way to the window.
  It travels now: no session, a keyring that won't hand it over, an expired
  session — each with its own sentence, the raw error underneath for a bug
  report, and a "Sign in again" button on the cases that actually fixes.
- **The service takes ownership of the saved session.** When it starts from a
  session that was left in the file (a client that had no service to hand it
  to, or a keyring that was locked at the time), it now stores it in the
  keyring itself. On macOS that's what stops the password prompt on every
  engine start, since a keychain item only authorises the binary that created
  it.
- **The app re-hands its session to a service that has none.** If the service
  reports "no session" while the app has one, it hands it over instead of
  waiting out a backoff that can't fix anything on its own.
- **"Link to this machine" opened the file manager again.** The 1.0.4 UI
  rewrite dropped the wiring for the link dialog added in 1.0.3, so the button
  went straight to the OS folder picker — making you hand-find a save folder
  Hoard had already detected. The dialog is back.

## [1.1.0] — 2026-07-28

### Added
- **Hoard keeps syncing with the app closed.** The sync engine moved out of
  the window and into a local service (`hoardd`) that starts with your session
  and stays resident: the desktop app and the `hoard` CLI are now thin clients
  that talk to it over a local socket. Close the app mid-game and your saves
  still get backed up; open it again and it just attaches to the service
  that was already running. On Linux the service also sends the native
  notifications, so a finished backup tells you even with no window open
  (Windows and macOS still notify from the app).
- **Real game covers, in the shape covers are.** The panel now asks Steam for
  each game's vertical 2:3 art instead of the 460×215 store banner, so a card
  shows the actual cover instead of a center-cropped strip of one. Games with
  no vertical art keep their banner, letterboxed over a blurred blow-up of
  itself rather than cropped to a third of the image. You can frame the whole
  grid as 2:3 posters or as squares (toolbar, top right — the square is there
  for custom art that isn't a poster), and your own image still beats both:
  hover a cover and click the pencil in its corner.
- **Redesigned dashboard.** The list of rows is now a grid of cover cards, each
  one carrying what the row had no room for: last save, total size across
  versions, stored-version count, a per-game menu (rename, pause, history) and
  a status pill that always speaks for *this* device — the cloud's version
  rides in a separate chip over the cover. A summary bar at the bottom totals
  games, versions, size and last backup.
- **Sniper scope (magnifier) in Hoard Screen.** A lens — circle or square —
  that shows whatever is under it magnified (×1–×4), sniper-style. Drag and
  resize it anywhere; clicks pass through to the game, and a crosshair draws
  on top of it unmagnified. Windows-only capture for now; while a scope is
  active the overlay is excluded from recordings/OBS (it has to be, or the
  lens would magnify itself).
- **Layers panel in Hoard Screen.** An ordered list of everything on the
  overlay: click to select, arrows to decide what draws over what. New
  crosshairs start above everything; widgets always float over placed apps.
- **Crosshair widget in Hoard Screen.** The overlay grows its first
  non-capture widget: a procedural crosshair (cross, ×, dot or circle) with
  color, opacity, size, thickness, center gap, center dot and outline — all
  editable live from the Screen panel, per monitor or mirrored. It renders
  through the same compositing path on every OS, is always click-through,
  and stays pixel-crisp at any size.

### Changed
- **Restores skip what your disk already has.** Before downloading a version,
  the client indexes the live folder by content: any file whose contents
  already sit there is copied locally instead of fetched. Restoring a 400 MB
  Factorio save after a small change now moves single-digit megabytes over the
  network.

### Fixed
- **Dismissing a message from the bell now sticks.** Dismissing only removed
  it from that window: the next time the app checked in — a restart, or a
  minute later — the server sent it back and it reappeared, forever. The
  dismissal is now recorded on the server, so a message you close stays closed
  on every machine you sign in from and after a reinstall. (Operator
  broadcasts also reach the bell again at all, which they hadn't since 1.0.4.)
- **Updating on Windows no longer trips over the sync service.** With the
  service now outliving the window, the installer had to overwrite a file the
  daemon was holding open, and the update failed — leaving the app running
  without its service. The installer stops the service (and the overlay)
  before replacing anything, and the in-app updater downloads the installer
  that does so.
- **Saves no longer get tracked under an app's name.** A background app that
  happened to be busy while a save folder changed could be credited with it,
  so the panel grew entries called "ChatGPT", "opencode" or "Codex … Setup"
  pointing at another game's folder — and since each wrong name made a new
  entry, they piled up. AI/desktop apps, capture tools (OBS, Streamlabs) and
  file-sync clients (Dropbox, Nextcloud, Syncthing, …) are no longer taken for
  games, the same folder can't be tracked twice under different names, and
  entries already poisoned are dropped when a real game covers that folder.
- **The cloud panel no longer goes stale in silence.** A background task that
  died could leave the app showing versions that no longer matched the cloud,
  with nothing on screen saying so; the engine now watches the cloud itself,
  restarts the task that died, and says out loud when its view is stale.
- **A locked keyring can't freeze the app any more.** If the system keyring
  never answered (locked wallet, no unlock prompt), the engine hung and the
  service refused to stop. Keyring reads now give up after 5 seconds with a
  reason you can read.
- **Hoard Screen editor can no longer lose track of the overlay.** The editor
  now re-syncs with the overlay process every few seconds (and on open), so a
  panel that is really on screen — e.g. a TikTok capture while gaming — can
  always be moved or removed even if the app's own copy of the layout went
  stale (reload, missed event). The overlay also shuts itself down if the app
  dies instead of lingering as an unremovable ghost.

## [1.0.4] — 2026-07-18

### Added
- **Sort the panel.** Order the dashboard's games by last backup (new
  default) or by cloud size. Cloud saves now carry their real "last backup"
  time, so the recency sort works on Hoard Cloud too.
- **Cloud size at a glance.** Every game row in the panel shows the space it
  occupies in the cloud (and only in the cloud — local footprints live in
  the Library, clearly labelled as such).
- **Bulk-delete versions.** History grew a checkbox per version plus
  select-all: tick as many as you want and delete them in one confirmed go
  instead of one dialog per version.
- **Max versions per game.** A per-account cap on stored versions, set right
  in the panel (empty = unlimited, like before). The server enforces it after
  every backup and prunes immediately when you lower it — oldest versions go
  first; pinned versions and the newest one are never touched. If the new cap
  would delete anything, a confirmation dialog first tells you exactly how
  many versions are about to go (server-side dry-run, so the number is real).
  Works on Cloud and self-hosted (`hoard snapshots max-versions` in the CLI,
  same preview + `[y/N]` prompt, `--yes` to skip).

### Fixed
- **Leaner startup sync.** When several games need restoring at once (e.g.
  first launch of the day), the app now fetches the cloud save list once for
  the whole batch instead of once per game — faster startup and fewer
  requests.

### Changed
- **Faster cloud sync.** Backups now hash and upload several files at a time,
  and restores download several at a time, instead of strictly one by one.
  Saves made of many small files — the common case — sync noticeably faster
  in both directions.
- **Local vs. server sizes, labelled.** The Library's tracked-games header
  (local, this machine) and each card's size pill (server-side) now carry
  icons and tooltips saying which is which, so the two totals can no longer
  be confused.
- **Cloud poll cadence is now fixed (60 s).** The `/v1/cloud/sync` fallback
  poll is no longer a preference — Realtime push already delivers changes
  instantly, so a faster poll bought nothing and a hand-edited `prefs.json`
  could hammer the server. Existing prefs files keep loading; the old key is
  simply ignored.
- **Server: internal storage maintenance.** Background housekeeping of how
  the cloud tier stores snapshot data internally. No user-facing changes:
  quotas, sizes shown in the app and download behavior are identical.
- **Server: per-device rate limit on polling endpoints.** `/v1/cloud/sync`,
  `/v1/devices`, `/v1/notifications` and `/v1/presence/heartbeat` are now
  capped per (user, device, endpoint) — 10 requests/minute by default
  (`[server.rate_limit] poll_per_minute`, cloud mode). The official client
  polls each at most twice a minute, so only modified or misconfigured
  clients ever see the 429 (which carries `Retry-After`). The client now
  sends its device fingerprint on sync/notifications so the cap is truly
  per machine, and the devices-feed refresh floor went from 2 s to 10 s so
  many-device accounts stay well under the cap.

## [1.0.3] — 2026-07-15

Sync you can trust, and an app that feels alive. Three deep fixes end the
"reload Steam on both devices" dance and the download-timeout loop; on top of
that, see every machine on your account live, hear from the dev through an
in-app bell, and pick a theme.

### Added
- **The Eye: your devices, live.** (Cloud) A header panel listing every
  machine on the account — online dot, which games each one is running right
  now and for how long. Agents heartbeat every 30 s and beat instantly when a
  game starts or stops, so launching a game on the Deck shows on the desktop
  in a second or two; a crashed machine simply ages out of the window instead
  of staying green. Desktop and CLI daemon both report.
- **The bell: announcements from the dev.** (Cloud) Operator broadcasts land
  in seconds over Realtime push (cursor-based polling as fallback, so nothing
  is ever re-delivered), render a mini-markdown subset, can carry an action
  button and expire on their own. Only the operator can send one — rows are
  inserted via direct service-role SQL, there is no HTTP write path.
  Dismissals sync server-side: dismissed on one device, gone on all of them.
- **Themes.** Obsidian (the classic dark), Quartz (light) or Auto to follow
  the OS scheme, plus an accent-colour picker — all in Settings. A pure
  CSS-variable re-skin that persists locally.
- **Link a cloud save without hunting for the folder.** When a save lives in
  the cloud but isn't linked on this machine, the link dialog now leads with
  the folders detection already found here — one click and done. The folder
  picker stays as the fallback, and a never-scanned machine is offered the
  scan instead of a false "nothing found".
- **Rename works on Hoard Cloud saves.** The cloud grew the rename endpoint
  the self-hosted server already had; duplicate labels are rejected cleanly.
- **Wrapped: browse any year.** The playtime recap grew a year picker —
  every year with playtime, latest first.
- **Operator tools** in `tools/`: the broadcast sender
  (`send-notification.sh`) and a single-file metrics dashboard.

### Fixed
- **Saves from another device now arrive without reloading Steam.** On the
  Steam Deck, Proton often leaves zombie processes behind after a game
  closes, so the engine kept believing the game was still running and held
  the cross-device restore forever — the hold itself is deliberate (never
  swap saves under a live game), but it had no way out. Zombie processes no
  longer count as running, a held restore is delivered the moment the game
  actually stops, and while it waits the app says so ("update ready — waiting
  for the game to close") instead of staying silent. Failed backups also
  retry on a 10-minute backoff instead of wedging restores until the next
  file event.
- **A Cloud session can no longer die permanently.** Two internal refresh
  paths could race over the same refresh token, and losing that race revoked
  the whole token family — sync stopped for good until re-login plus a
  restart. Every refresh now goes through one serialized path that re-reads
  the token from disk and collapses bursts into a single request. If a
  session does expire, the daemon announces it once, re-checks quietly, and
  everything — refresher and realtime push — reconnects on its own after
  `hoard login`, no restart needed. Daemon boot also survives starting before
  the network is up instead of exiting.
- **Big saves no longer die with "operation timed out".** Snapshot transfers
  ran on an HTTP client whose 60-second total timeout covered the response
  body too, so any download longer than a minute (Paradox-sized saves) was
  killed mid-stream and retried in a loop — and slow uploads could hang the
  "Uploading…" pill the same way. Transfers now use dedicated streaming
  clients: no total cap, a stall detector on downloads, TCP keepalive on
  uploads.

## [1.0.2] — 2026-07-12

The open-source release. The whole app — including the Pro layer — now lives in
one AGPL repo, the CLI grows into a first-class frontend, and Hoard Wrapped is
free for everyone. Plus an official Docker image, packaging for more distros,
and a round of detection and reliability fixes.

### Added
- **The Pro layer is now open source, in this repo.** Hoard Screen (the in-game
  overlay) and Hoard Wrapped (the year-in-games recap) ship as regular AGPL
  crates. The paywall isn't the code — the Hoard Screen entitlement is signed
  server-side, so anyone can build it but only Cloud unlocks it. There's nothing
  to patch out locally.
- **Hoard Wrapped is free for everyone.** The playtime recap renders for Cloud
  and self-hosted alike, with no gate — a two-mode engine that generates the
  recap server-side on Cloud and locally when self-hosted.
- **The CLI is now a full frontend of the shared engine.** `hoard` and the
  desktop app run the exact same `hoard-agent` core, so every feature lands in
  both. New: an interactive `hoard login` flow that no longer needs a
  hand-pasted token.
- **Sign in the CLI by pairing a device.** Cloud login on a headless box can now
  be approved from an already-signed-in device instead of copying credentials
  around, with a `/link` page to complete the pairing.
- **More install options.** An official multi-arch Docker image on GHCR
  (`ghcr.io/rleeon/hoard`, amd64 + arm64) — `docker compose pull && docker
  compose up -d` to update instead of building on your box — plus `.rpm` and
  Snap packages for the desktop app.
- **Reclaim archived games from the app.** Games you archived to free quota now
  show up in Library and History with a **Reactivar** action, so bringing one
  back no longer means digging through the CLI.

### Fixed
- **AppImage on SteamOS / Bazzite and other newer distros.** The bundle no
  longer ships its own `libwayland-client`/`libEGL`/`libGL`/`libgbm` — those
  now resolve from the host, fixing the solid-white window and
  `could not create default EGL display: EGL_BAD_PARAMETER` that forced users
  to launch with `LD_PRELOAD`.
- **Sign-in did nothing under the AppImage.** Outward links (OAuth sign-in,
  upgrade/billing, terms) now open through a Rust `open_external` command that
  strips the AppImage-injected loader env, so the browser starts against the
  host's libraries instead of Hoard's bundled (mismatched) ones and actually
  appears.
- **Detection sweep.** Several fixes to game/save detection and the backup
  queue, so more games are found automatically and fewer get stuck.
- **No more phantom "game started" flaps.** A brief CPU dip on a correlation
  match is now debounced instead of flapping the running-game state.
- **One agent per machine.** A single-instance lock stops two daemons from
  rotating the same token and 401-ing each other's syncs.
- **Safer self-hosted upgrades.** `hoard-server upgrade` refuses to run inside a
  container and points you at rebuilding the image instead of swapping a binary
  that a `docker compose pull` will overwrite.

### Changed
- **Failed syncs are now visible.** Bandwidth-window rejections are recorded in
  `sync_log` alongside quota rejections, so the sync failure rate is no longer
  invisible.
- **Storage downgrade grace widened to 30 days** (was 14) — more room before a
  plan change trims your ceiling.
- **Community docs in the repo.** Added CONTRIBUTING, a self-hosting guide, a
  funding breakdown, and a GitHub Sponsor button.
- CI now runs only on version tags, pull requests, and manual dispatch —
  routine branch pushes (including docs-only edits) no longer spend Actions
  minutes. Validate locally with `cargo check` + `pnpm check` before pushing.

## [1.0.1] — 2026-07-09

The reliability release. A single-PC data-loss window in Global Sync is closed
for good, cloud limits get roomier across the board, and running out of quota
is no longer a dead end — you can now buy your way *down* by archiving the
whales instead of deleting anything.

### Added
- **Reclaim quota without deleting a single byte.** When your live saves push
  past the plan ceiling, a new dialog ranks your games by footprint and lets
  you archive the heaviest ones. Archiving frees the quota **instantly**
  (refcount drops, `/v1/me` reflects it on the next poll) while the cloud copy
  is frozen and stays downloadable for a 7-day grace window before a cron
  purges it. Your local save is never touched, and the whole thing is
  reversible the moment you upgrade — it's an escape hatch, not a guillotine.
- **Wrapped credits playtime for *any* Steam game you actually run** — even
  ones with no local save to capture and no catalog entry (online-only titles,
  private servers, War Selection, and friends). When the agent sees a process
  launch from its Steam install dir, it attributes the time. Nothing gets
  enrolled and the "Played, not backed up" list stays clean; Proton, runtimes
  and SteamVR are filtered out so they never book phantom hours.

### Changed
- **Cloud limits, meaningfully bigger.** Storage: Free **1 → 2 GB**, Pro
  **25 → 100 GB**. Per-save ceiling: Free **200 MB → 1 GB**, Pro **2 → 10 GB**.
  Rolling 15-minute bandwidth window: Free **→ 3 GB**, Pro **→ 15 GB** (kept
  above the max single-save size so a first upload can never wedge itself
  behind its own window). The Pro base tier no longer pins a per-user storage
  override, so raising the plan default now actually reaches existing
  subscribers on renewal instead of being shadowed by a stale `storage_gb`.
- **Account screen: dropped the redundant "Compare plans" button** and its
  modal — one fewer detour between you and the upgrade CTA.

### Fixed
- **Global Sync can no longer clobber an in-progress save (real data loss).**
  With Sync on, three independent code paths — the SSE/poller instant pull, the
  reconciliation sweep, and the pre-launch barrier — bypassed the live-session
  guards. On a *single* PC that meant an automatic pull could re-apply the last
  uploaded version on top of progress the autosave hadn't captured yet, and
  those intermediate saves were never versioned at all (reproducible loss with
  R.E.P.O.). Every automatic pull now waits for the game to close and the save
  to settle. The legitimate multi-device path is untouched: an idle machine
  still pulls the new version immediately, and genuine divergence is resolved by
  upload-conflict reconciliation rather than a silent overwrite.
- **In-progress work is versioned in seconds, not left in a queue.** When a
  pull is deferred for a live session and there are un-uploaded local changes,
  the agent now pushes them immediately — skipping the data-saving interval —
  instead of parking them in the backup queue. What you played exists as a cloud
  version within seconds even if it isn't the version you ultimately keep; if
  the cloud was ahead, upload-conflict reconciliation versions both sides.
- **"Export all data" can't hang forever anymore.** An export job that died
  mid-build (worker restart) left a phantom `running` row that blocked every
  subsequent attempt. A reaper now marks jobs stale after 1h so you can retry,
  and the button stays responsive even when the delivery email never lands.
- **The reclaim-storage dialog shows real game names** instead of a wall of
  "main", and a failed load surfaces a clear message with a retry button
  instead of a raw error string.
