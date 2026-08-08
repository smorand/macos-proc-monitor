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
    src/lib.rs               # réexporte collect + web + config + telemetry + error, VERSION, shutdown_signal
    src/collect.rs           # CollectConfig + collect_loop() : sysinfo, lsof, parquet, purge rétention (data + logs)
    src/config.rs            # Config + ConfigOverrides (figment: defaults<TOML<env<CLI) + resolve_dir (env/XDG//var/db)
    src/telemetry.rs         # init() : tracing registry, stderr + fichier rotatif quotidien (tracing-appender), TelemetryGuard
    src/error.rs             # CoreError (thiserror, variants boxées)
    src/web.rs               # router() + serve_web(bind, port, data_dir) : axum + handlers api_* + health + open_db (DuckDB)
    static/index.html        # dashboard (embarqué via include_str!("../static/index.html"))
    tests/                   # e2e: web_api.rs (axum réel), telemetry.rs, collect_loop.rs
  macos-proc-monitor/        # package + bin `macos-proc-monitor`
    src/main.rs              # main -> ExitCode (sync) + run (async), CLI clap -> ConfigOverrides, wire daemon
    tests/cli.rs             # e2e CLI (assert_cmd)
launchd/…plist               # service launchd (root, RunAtLoad+KeepAlive)
sudoers.d/…                  # règle NOPASSWD lsof
deny.toml / rust-toolchain.toml
Makefile                     # gate complet (check/lint/format/test-cov/security...) + install/daemon-*
```

**Lib nommée `procmon`, pas `core`** (éviter de shadow le crate std `core`). Import: `use procmon::...`.

Un seul binaire produit: `macos-proc-monitor`.

## Commandes

| But | Commande |
|---|---|
| Build debug (workspace) | `cargo build` ou `make build` |
| Build release | `cargo build --release` ou `make release` |
| Lancer en dev | `make run ARGS='--help'` / `make run ARGS='--no-slow'` |
| Tests (unit + integ + e2e) | `make test` |
| Gate complet (avant commit) | `make check` |
| Lint / format | `make lint` / `make format` |
| Sécurité (audit + deny) | `make security` |
| Couverture (>= 80% lignes) | `make test-cov` |
| Install complet (binaire + sudoers + daemon launchd) | `make install` |
| Désinstaller (unload daemon + tout retirer) | `make uninstall` |
| Piloter le service | `make daemon-start` / `daemon-stop` / `daemon-status` |

**Ne jamais `sudo make install`.** Le `cargo build` doit tourner en user; les `sudo` sont déjà dans les recettes du Makefile (uniquement sur les étapes système: cp/chown/launchctl). `make install` dépend de `release` (build en user) puis fait les `sudo` ciblés.

## Conventions & gotchas

- **DuckDB bundled** (`features = ["bundled"]`): compilé depuis les sources, premier build long (~6 min release). C'est le **moteur de requête** (lecture SQL sur les Parquet), distinct de la couche `parquet`/`arrow` qui écrit les fichiers. Les deux sont requis.
- **DuckDB par requête**: `web.rs::open_db` ouvre une connexion in-memory neuve à chaque appel API, avec un `CREATE VIEW ... read_parquet(glob)`. Coût scale avec le nombre de fichiers Parquet dans la fenêtre. Connu, pas une régression. Premier levier d'optim si le dashboard rame sur fenêtre 7j. Sur répertoire data vide, `open_db` échoue → endpoints data en 500, `/health/ready` en 503 (comportement attendu).
- **Config = figment** (`config.rs`): defaults < TOML (`~/.config/macos-proc-monitor/config.toml` ou `--config`) < env `MACOS_PROC_MONITOR_` < flags CLI. Les flags CLI sont des `Option` (`ConfigOverrides`), seuls ceux passés overrident. Ne pas lire `std::env::var` en direct pour la config.
- **Résolution des dirs** (`config.rs::resolve_dir`): `MACOS_PROC_MONITOR_FOLDER_DATA` / `_FOLDER_LOG` (contrat du plist root), sinon XDG cache (`~/.cache/macos-proc-monitor/{data,logs}` via etcetera), sinon `/var/db/macos-proc-monitor/{data,logs}`. **Ne pas** remplacer par un pur etcetera: `HOME=/var/root` (plist) donnerait `/var/root/.cache`, ce qui casserait le contrat `/var/db`. Le plist passe déjà les env vars.
- **Logging = tracing + tracing-appender** (`telemetry.rs`): deux layers (stderr pour launchd + fichier rotatif quotidien `monitor.<date>.log`). Le `TelemetryGuard` doit vivre tout le process (drop = flush du writer non-bloquant). La rétention des logs est faite dans `collect_loop` (purge `.log` par mtime, comme les `.parquet`), pas par tracing-appender.
- **Chart.js + zoom Chrome**: `responsive:true` ne réagit pas au changement de zoom. Un `ResizeObserver` dans `static/index.html` force `chart.resize()` (commit `db1f91f`). Ne pas retirer.
- **Flags web dans le plist**: `--port 9090 --bind 127.0.0.1` calés en dur dans `ProgramArguments`. Le web est **toujours** servi (pas de `--no-web`).
- **API**: handlers servis sous `/api/*` (contrat du dashboard embarqué) ET `/api/v1/*`. Health: `/health/live`, `/health/ready`, `/health`. 5xx sanitisés (`{"error":"internal server error"}`), erreur complète loggée côté serveur.
- **collecte = sync bloquant**: sysinfo, lsof (subprocess), écriture parquet sont bloquants → volontairement sur un thread std, pas sur tokio. `collect_loop` est une boucle infinie. Ne pas asyncifier sans raison.
- **édition 2024, MSRV 1.87** (`Cargo.toml` workspace + `rust-toolchain.toml`). thiserror (core) / anyhow (bin). `unsafe_code = "forbid"` workspace-wide: pas d'`unsafe`, même en test (donc interdit d'utiliser `std::env::set_var`, `unsafe` en 2024). Versions figées: sysinfo 0.39, arrow 53, parquet 53, axum 0.7, duckdb 1, tower-http 0.5. Vérifier compat avant bump.
- **cargo-deny / audit**: `paste` (unmaintained) et `rkyv` (advisory) sont transitifs sans upgrade sûr; gérés par `deny.toml` (`unmaintained = "workspace"`) et `.cargo/audit.toml` (ignore documenté). Le dep interne `macos-proc-core` a une `version` explicite (sinon `wildcards = deny` échoue).

## Vérifs après changement

- `cargo build --workspace` doit passer **zéro warning**.
- `cargo run -p macos-proc-monitor -- --help` liste flags collecte + `--port`/`--bind`.
- Si touche au dashboard: vérifier `http://127.0.0.1:9090` + `curl -s :9090/api/summary` → JSON.
- Si touche au Makefile: `grep -n 'sudo.*cargo\|sudo make' Makefile` doit être vide.
