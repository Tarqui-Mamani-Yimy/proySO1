# Phase 6 — Agent layer

Objective: expose a generic, agent-agnostic contract (block metadata, per-pane state, waiting) over the so1term control API, and a command-permission policy on epsh hooks. No Pi-specific integration.
Branch: `feat/phase-6-agent-layer`. Delivery strategy: `ask-on-risk`. Test runner: `cargo test --workspace` (focused: `cargo test -p so1-term-core`, `cargo test -p so1-terminal --test server`).

## Specs

- **S1** — planing.md Fase 6: "Chequeo de permisos de comandos de agentes vía AST de epsh."
  Design notes (from sugPerms.md, not user strings): enforce on expanded argv through epsh hooks, not on the AST alone; it is not a security boundary. Scope, policy format and who installs it are an open product decision (see Log).
- **S2** — planing.md Fase 6: "Estado por panel de agente (trabajando / bloqueado / ocioso)."
  Derived from OSC 133 (running between C and D, idle at a prompt). "bloqueado" has no reliable signal yet; assumption: report it only when a signal exists, never guessed.
- **S3** — planing.md Fase 6: "Contrato genérico para agentes vía `so1ctl` (metadatos de bloques, `wait`, `status`). Sin integración específica con Pi: cualquier agente, Pi incluido, es un cliente más."
  From sugPerms.md item 2: "Agregar comando, cwd, código de salida y duración"; "Si el bloque fue desalojado del scrollback, responder con un error explícito, igual que la regla actual de \"nunca texto parcial\"."
- **S4** — Documentation: AGENTS.md and resumen.md describe the new commands and fields; AGENTS.md stops calling `fg`/`bg` and job notifications "later Phase 2 tasks" (sugPerms.md item 1).

## Tasks

| ID | Specs | Route | Status | Commit |
|---|---|---|---|---|
| T1 — Block wall-clock times and a metadata accessor in term-core (command text, cwd, exit status, state, start/end, duration, explicit evicted result) | S3 | inline | pending | |
| T2 — Shared control helpers + new `block` control command (window and server) + `so1ctl block` | S3 | inline | pending | |
| T3 — Per-pane state in `list` (idle / running, last exit status) for window and server | S2, S3 | inline | pending | |
| T4 — `so1ctl wait` (client-side, bounded timeout, waits for the pane's running block to finish) | S3 | inline | pending | |
| T5 — Command-permission policy on epsh hooks | S1 | inline | blocked: product decision | |
| T6 — Docs (AGENTS.md, resumen.md, `so1ctl --help`) and phase verification | S4 | inline | pending | |

## Log

- **L1** (2026-11-21, user): "entonces arranquemos con la fase Seis, y ademas no hagamos la integracion con Pi, y respecto a tu ultima pregunta es un SI"
- **L2** (2026-11-21): planing.md Fase 6 item 3 changed from "Integración con Pi por su modo RPC" to the generic contract (S3); sugPerms.md records the decision.
- **L3** (2026-11-21, mapping): `Block` holds stream offsets only (`blocks.rs` `PhaseBoundaries`), no wall-clock times. Control dispatch is duplicated in `crates/terminal/src/main.rs:1791` and `crates/terminal/src/session.rs:343`, including the `list` JSON. Protocol v1 is strict (`allowed_fields`), so new capability is added as new commands. so1sh installs no `ExternalHandler`; `exec`, `kill` and `cd` bypass it; the redirect hook only sees writable redirections. `so1ctl` is hand-rolled and prints compact JSON.
- **L4**: forecast ~1250 authored lines (T1 250, T2 250, T3 120, T4 150, T5 400, T6 80); chain strategy to be asked before the commit that crosses ~400 lines.
- Next: T1.
