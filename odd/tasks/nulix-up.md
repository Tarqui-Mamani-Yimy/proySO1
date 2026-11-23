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
| T1 — `nulix up` argument, `NULIX_UI` env, help, docs | S1 | inline | done (RED: 5 compile errors; GREEN: 30 passed) | 85ca1d0 |
| T2 — up theme defaults | S2 | folded into T3 (see L5) | dropped | |
| T3 — up chrome colours + always-visible tab bar + hint bar | S2–S3 | delegated (context backstop) | done (RED: 8 failed + 6 compile errors; GREEN: 41 bin + 81 lib) | next commit |
| T4 — shellux blank line before prompt | S4 | inline | done (RED: PTY test failed; GREEN: shellux 87+8+116) | 3bdaea7 |
| T5 — rounded block cards + status meta | S5 | inline | pending | |
| T6 — F1 help overlay | S6 | inline | pending | |
| T7 — demo guide | S7 | inline | pending | |

## Log

- **L1** (2026-11-23, user): "a un jurado en donde bueno, quisiera que se vea mas o menos algo amigable y que la UI se vea algo mejor, pero sin perder mucho rendimiento, como un comando que active la UI del proyecto, en donde para conocedores, utilicen solo los comandos y los que no puedan ejecuten un comando como por ejemplo \"nulix up\" o algo asi, seria algo dificil de implementar o no"
- **L2** (2026-11-23, user): "me parece bien el nombre" · rounded cards: "si" · design: "me parece bien el diseño" · plan T1–T7 approved: "si".
- **L3**: Routes are inline: no unit has a Writer-trigger reason (units are sequential and share `crates/terminal/src/main.rs`).
- **L4** (2026-11-23, user): chain strategy "stacked-to-main (Recommended)". T1 85ca1d0; RDD assess medium, under_budget.
- **L5**: T2 folded into T3: nulix's default background `#14161C` and foreground `#DCDCDC` already match S2; the other S2 colours are chrome/card colours (not the ANSI palette), so `nulix.conf` colours are untouched by them. T3 delegated to one writer for the parent-context backstop.
- **L6**: T3 writer returned done. Parent spot check: `cargo test -p nulix` all ok. Visual check (risk item 6: no test renders the bars): screenshot of `nulix up` under X11 showed both bars and the T4 blank row; `⇧` rendered as a wrong glyph, so hints were spelled `Ctrl+Shift+X` and `F1 ayuda` moved first so it is dropped last (tests updated first, RED 2 failed, then GREEN). Closing an inactive tab = select it, close the active one, reselect the previous tab (TabSet only has `close_active`).
