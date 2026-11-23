# nulix up — friendly UI mode for the jury demo

Objective: `nulix up` opens nulix in a friendlier UI mode (block cards, always-visible tab bar, shortcut-hint bar, F1 help, up theme); plain `nulix` is unchanged.
Branch: `feat/nulix-up`. Delivery: ask-on-risk (forecast ~1500–2000 authored lines > 400: chain strategy asked once before the second commit). Test runner: `cargo test -p nulix` / `cargo test -p shellux --test repl`; `make ci` at closure.

## Specs

- **S1** — Launch (user, verbatim): "como un comando que active la UI del proyecto, en donde para conocedores, utilicen solo los comandos y los que no puedan ejecuten un comando como por ejemplo \"nulix up\"" / "me parece bien el nombre". `nulix up` opens the standalone window in up mode; `nulix` and `nulix --session NAME` are unchanged. `nulix up` exports `NULIX_UI=up` to pane shells; other modes remove an inherited `NULIX_UI`. `nulix up --session` is out of scope (later). `--help` and docs describe it.
- **S2** — Look (user, verbatim): "quisiera que se vea mas o menos algo amigable y que la UI se vea algo mejor, pero sin perder mucho rendimiento" / "me parece bien el diseño". Approved mockup: https://claude.ai/artifact/VomRUtN7NKgzEMWc6xf7Mt. Up palette: bg `#14161C`, surface `#1B1E26`, chrome `#0F1116`, border `#2A2F3A`, text `#DCDCDC`, muted `#8A90A0`, accent `#69A5DC`, success `#50C878`, failure `#F06A6A`. Explicit `nulix.conf` colours still win. No animation; chrome redraws only on change.
- **S3** — Bars: tab bar always visible in up mode (`+` new tab, `×` close, click to select), session/name label at the right; bottom hint bar with focused-pane state (running/idle) and `⌃⇧D dividir · ⌃⇧T tab · ⌃⇧A bloque · ⌃⇧O copiar · F1 ayuda`. Both bars take whole cell rows from the PTY grid.
- **S4** — Spacing: with `NULIX_UI=up`, shellux prints one blank line before each primary prompt, so consecutive block cards do not touch.
- **S5** — Block cards (user: "si" to rounded borders): each primary-screen block is drawn as a rounded card (WGSL rounded-rect shader); border by state (default `#2A2F3A`, failure red-tinted, running accent 2px, selected accent); right of the prompt row `12ms ✓`, `0.4s ✗ 101`, `● 3.1s`; cancelled is muted. No cards on the alternate screen (bars stay).
- **S6** — F1 help overlay: centred panel grouping Paneles/Tabs/Bloques/Copiar/Sesión shortcuts; Esc or F1 closes. While the alternate screen is active, F1 goes to the application.
- **S7** — Demo guide `docs/demo.md` with the exact steps for the jury.

## Tasks

| ID | Specs | Route | Status | Commit |
|---|---|---|---|---|
| T1 — `nulix up` argument, `NULIX_UI` env, help, docs | S1 | inline | done (RED: 5 compile errors; GREEN: 30 passed) | see git log |
| T2 — up theme defaults | S2 | inline | pending | |
| T3 — always-visible tab bar + hint bar | S3 | inline | pending | |
| T4 — shellux blank line before prompt | S4 | inline | pending | |
| T5 — rounded block cards + status meta | S5 | inline | pending | |
| T6 — F1 help overlay | S6 | inline | pending | |
| T7 — demo guide | S7 | inline | pending | |

## Log

- **L1** (2026-11-23, user): "a un jurado en donde bueno, quisiera que se vea mas o menos algo amigable y que la UI se vea algo mejor, pero sin perder mucho rendimiento, como un comando que active la UI del proyecto, en donde para conocedores, utilicen solo los comandos y los que no puedan ejecuten un comando como por ejemplo \"nulix up\" o algo asi, seria algo dificil de implementar o no"
- **L2** (2026-11-23, user): "me parece bien el nombre" · rounded cards: "si" · design: "me parece bien el diseño" · plan T1–T7 approved: "si".
- **L3**: Routes are inline: no unit has a Writer-trigger reason (units are sequential and share `crates/terminal/src/main.rs`).
