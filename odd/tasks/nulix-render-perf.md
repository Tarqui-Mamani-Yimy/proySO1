# nulix render performance — cut CPU under continuous output

Objective: bring nulix's CPU use under continuous PTY output closer to xfce4-terminal (VTE) without changing what is drawn.
Branch: `perf/nulix-redraw` (from `feat/nulix-up`). Delivery: ask-on-risk; stacked-to-main (cached from nulix-up). Test runner: `cargo test -p nulix -p nulix-core`; benchmark below.

## Specs

- **S1** — Goal (user, verbatim): "en este momento, nos encontramos en herdr (termianl de XFCE), quisiera que evalues cual de las terminales anda comiendo mas recursos, comparandola con mi proyecto" → benchmark → "ok" to profiling nulix and starting the optimizations: (1) limit redraws to about 60 per second, coalescing output into one frame; (2) redraw only panes that changed; (3) do not rebuild text for lines that did not change.
- **S2** — Benchmark (baseline 2026-11-23, release build, fresh instance, load = `seq -f "linea %g de una salida continua para comparar terminales" 1 40` every 0.1 s, CPU sampled for 15 s): nulix classic idle 0.3% / under load 29.3%; nulix up idle 0.3% / 34.6%; xfce4-terminal idle 0.0% / 2.9%. Acceptance: under the same load nulix CPU drops substantially (report the measured numbers; no fixed target is promised), idle stays ~0%, and nothing visible changes (screenshots before/after).
- **S3** — No behaviour change: all existing tests pass; keyboard echo latency is not made worse (a keypress still draws on the next frame).

## Tasks

| ID | Specs | Route | Status | Commit |
|---|---|---|---|---|
| P1 — measure: redraws/s and time per draw phase under the load | S1–S2 | delegated (explorer) | done (see L3) | — |
| P2 — glyph cache keyed by content (char, style), shared across cells and panes | S1–S3 | delegated (context backstop) | in progress | |
| P3 — cache up chrome/card meta text buffers; skip default-background rects | S1–S3 | after P2 | pending | |
| P4 — bound PTY apply work per event (floods) / frame pacing | S1–S3 | after P3, only if measured useful | pending | |

## Log

- **L1** (2026-11-23, user): "ok" (to: "¿Querés que perfile nulix y arranque con esas optimizaciones?").
- **L2**: `perf` is not installed and `perf_event_paranoid=3`, so P1 measures with temporary instrumentation that is never committed.
- **L3**: P1 result (instrumentation reverted): draws are already coalesced (~10 draws/s, one per PTY event), so frame pacing does not help this load. Re-shaping glyphs (`Buffer::set_text` + `shape_until_scroll`, ~0.14–0.21 ms each) takes 85–90% of draw time, because the per-pane `GlyphCache` is keyed by cell position (`main.rs:2969-3009`). In up mode, `prepare_overlay_texts` re-shapes the chrome every frame (~6 ms/draw). Other draw phases are under 2 ms. During floods, `apply_segments` takes up to 374 ms per event. Plan reordered: P2 content-keyed glyph cache, P3 chrome text cache, P4 floods/pacing.
