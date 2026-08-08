# AGENTS.md — macos-proc-monitor

Index compact pour interactions IA. Détails produit/usage: voir `README.md`.

## Overview

Daemon macOS unique (un seul binaire) qui fait **deux choses dans le même process**:
1. **Collecte** les métriques par-process chaque seconde → fichiers Parquet partitionnés par heure.
2. **Sert** un dashboard web (Axum + DuckDB in-memory) qui lit ces Parquet, sur `http://127.0.0.1:9090`.

La boucle de collecte tourne sur un **thread bloquant dédié**; le serveur web sur le **runtime tokio**. `main()` est `#[tokio::main]`: il spawn le thread de collecte puis `serve_web(...).await`.

Historique: à l'origine 2 binaires (`macos-proc-monitor` collecteur + `macos-proc-analytics` web). Fusionnés en un seul daemon (commit `b9b1e92`). **Il n'y a plus de crate/binaire `analytics` ni `monitor`.**

## Layout (workspace Cargo)

```
crates/
  core/                      # package macos-proc-core, lib name `procmon`
    src/lib.rs               # réexporte collect + web
    src/collect.rs           # CollectConfig + collect_loop() : sysinfo, lsof, parquet, logging rotatif, purge rétention
    src/web.rs               # serve_web(bind, port, data_dir) : Router axum + handlers api_* + open_db (DuckDB)
    static/index.html        # dashboard (embarqué via include_str!("../static/index.html"))
  macos-proc-monitor/        # package + bin `macos-proc-monitor`
    src/main.rs              # CLI clap fusionné (flags collecte + --port/--bind), wire daemon
launchd/…plist               # service launchd (root, RunAtLoad+KeepAlive)
sudoers.d/…                  # règle NOPASSWD lsof
Makefile                     # install/uninstall/run/daemon-*
```

**Lib nommée `procmon`, pas `core`** (éviter de shadow le crate std `core`). Import: `use procmon::...`.

Un seul binaire produit: `macos-proc-monitor`.

## Commandes

| But | Commande |
|---|---|
| Build debug (workspace) | `cargo build` ou `make build` |
| Build release | `cargo build --release` ou `make release` |
| Lancer en dev | `make run ARGS='--help'` / `make run ARGS='--no-slow'` |
| Tests | `cargo test` ou `make test` |
| Install complet (binaire + sudoers + daemon launchd) | `make install` |
| Désinstaller (unload daemon + tout retirer) | `make uninstall` |
| Piloter le service | `make daemon-start` / `daemon-stop` / `daemon-status` |

**Ne jamais `sudo make install`.** Le `cargo build` doit tourner en user; les `sudo` sont déjà dans les recettes du Makefile (uniquement sur les étapes système: cp/chown/launchctl). `make install` dépend de `release` (build en user) puis fait les `sudo` ciblés.

## Conventions & gotchas

- **DuckDB bundled** (`features = ["bundled"]`): compilé depuis les sources, premier build long (~1 min release). Binaire ~37 Mo.
- **DuckDB par requête**: `web.rs::open_db` ouvre une connexion in-memory neuve à chaque appel API, avec un `CREATE VIEW ... read_parquet(glob)`. Coût scale avec le nombre de fichiers Parquet dans la fenêtre. Connu, pas une régression. Premier levier d'optim si le dashboard rame sur fenêtre 7j.
- **Chart.js + zoom Chrome**: `responsive:true` ne réagit pas au changement de zoom (change le devicePixelRatio, pas la taille du conteneur). Un `ResizeObserver` dans `static/index.html` force `chart.resize()` (commit `db1f91f`). Ne pas retirer.
- **Résolution des dirs**: `MACOS_PROC_MONITOR_FOLDER_DATA` / `_FOLDER_LOG`, sinon `~/.cache/macos-proc-monitor/{data,logs}`, sinon `/var/db/macos-proc-monitor/{data,logs}` (fallback launchd/root). Le plist force les chemins `/var/db/...`.
- **Flags web dans le plist**: `--port 9090 --bind 127.0.0.1` calés en dur dans `ProgramArguments`. Le web est **toujours** servi (pas de `--no-web`).
- **collecte = sync bloquant**: sysinfo, lsof (subprocess), écriture parquet sont bloquants → volontairement sur un thread std, pas sur tokio. Ne pas asyncifier sans raison.
- **édition 2021**, versions figées: sysinfo 0.39, arrow 53, parquet 53, axum 0.7, duckdb 1, tower-http 0.5. Vérifier compat avant bump.

## Vérifs après changement

- `cargo build --workspace` doit passer **zéro warning**.
- `cargo run -p macos-proc-monitor -- --help` liste flags collecte + `--port`/`--bind`.
- Si touche au dashboard: vérifier `http://127.0.0.1:9090` + `curl -s :9090/api/summary` → JSON.
- Si touche au Makefile: `grep -n 'sudo.*cargo\|sudo make' Makefile` doit être vide.
