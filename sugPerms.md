# Sugerencias a implementar

Selección hecha a partir de `sugerencias.md` (reseñas de Gemini y ChatGPT), contrastada
con el código el 2026-11-21. Cada punto indica su evidencia y por qué entra o queda
afuera.

## Calificación

| Área | Nota | Motivo |
|---|---|---|
| Arquitectura | 9 | Capas separadas (motor, shell, núcleo sin UI, binarios) unidas por escapes estándar |
| Motor epsh | 8 | Conformance mksh, paridad con dash en comandos externos; traps durante esperas y aritmética lenta pendientes |
| Terminal so1term | 8 | Paneles, bloques, protocolos kitty, sesiones persistentes; falta validar contra apps TUI reales |
| Shell so1sh | 8 | Edición, prompt, rc, `jobs`/`fg`/`bg`, avisos, completado, hints y resaltado; faltan alias |
| Producto / uso diario | 5.5 | Sin archivo de configuración, sin empaquetado, licencia sin decidir, Windows recortado |
| Documentación | 6 | `AGENTS.md` está desactualizado respecto del código (ver punto 1) |
| **Total** | **8** | Los cimientos están bien; lo que falta es producto |

## A implementar (ordenado por valor sobre esfuerzo)

1. **Actualizar la documentación con lo que ya existe.** Esfuerzo: mínimo.
   - `AGENTS.md` dice que `fg`/`bg` y los avisos son "later Phase 2 tasks", pero ya
     existen (`crates/shell/src/jobs.rs:278`, `:287`; Fase 2 T3 y T4 hechas).
   - El árbol de arquitectura no menciona `completion.rs`, `highlighter.rs` ni
     `history_hint.rs`.
   - `resumen.md` debe reflejar lo mismo.
   - Por qué: las dos reseñas externas evaluaron mal por culpa de esta brecha.

2. **Metadatos de bloques en `so1ctl`.** Esfuerzo: bajo a medio.
   - Hoy `get-text` solo ofrece la pantalla y la salida del último bloque.
   - Agregar comando, cwd, código de salida y duración (por ejemplo
     `so1ctl block --last --json`).
   - El código de salida ya llega por OSC 133 `D;<status>`; la duración necesita
     marcas de tiempo en `blocks.rs`.
   - Si el bloque fue desalojado del scrollback, responder con un error explícito,
     igual que la regla actual de "nunca texto parcial".
   - Por qué: es la base de la Fase 6 (estado por panel) y el mejor diferencial para
     agentes.

3. **Alias solo en el REPL interactivo de so1sh.** Esfuerzo: bajo a medio.
   - epsh sigue sin alias por diseño; la expansión vive en la capa de la shell.
   - Tests para recursión, comillas y posición de palabra. Scripts, `-c` y entrada
     por pipe no expanden alias.

4. **Matriz de pruebas TUI para so1term.** Esfuerzo: bajo.
   - nvim, tmux, htop, fzf y lazygit: checklist manual y algunos tests sobre PTY.

5. **Traps durante esperas bloqueantes** (`TODO.md` §4). Esfuerzo: medio, código de
   señales delicado.
   - Usar `sigaction` sin `SA_RESTART` o un self-pipe, sin sumar `signal-hook`
     (`signal.rs:8-10` explica por qué se evita el trampolín de rustix).
   - Tests primero.

6. **Archivo de configuración de so1term.** Esfuerzo: medio.
   - Primero carga al arrancar; la recarga en caliente después, aplicada por cliente
     y no al servidor de sesión.

7. **`VarValue::Int` en epsh.** Esfuerzo: medio.
   - `var.rs` ya cachea enteros; el costo restante es crear un `String` en cada
     escritura.
   - Medir antes y comparar contra el comportamiento anterior.

8. **Empaquetado** (binarios, AUR, Homebrew). Depende de decidir la licencia
   (`planing.md` §6).

## Van a la Fase 6 (capa de agentes)

- **Chequeo de permisos de comandos** (Fase 6, ítem 1). Es la sugerencia `epsh-policy`,
  pero como crate propio de la capa de agentes, no dentro de epsh.
- **Estado por panel de agente** (Fase 6, ítem 2). Se construye sobre el punto 2.
- **`so1ctl wait` / `status` / eventos.** Necesita un stream de eventos; el timeout de
  30 segundos por petición no sirve para esperar comandos largos.
- **Contrato genérico para agentes** (Fase 6, ítem 3). Reemplaza la integración con Pi
  por su modo RPC (decisión del 2026-11-21): so1term expone `so1ctl`, los metadatos de
  bloques y el estado por panel, y cada agente los consume desde su lado.

## Descartadas o postergadas

| Sugerencia | Motivo |
|---|---|
| Sistema de plugins | Rompe la regla de `planing.md` §2 (capa no clara) y agranda la superficie de ataque; las funciones del rc ya cubren parte |
| Carapace para completado | Dependencia de un binario externo; el completado básico ya existe |
| `waitpid(-1, WNOHANG)` para avisos | Robaría hijos del embebedor; so1sh consulta solo los pids de sus trabajos |
| `signal-hook` para traps | Dependencia innecesaria; se resuelve con `sigaction` |
| Presentar `epsh-policy` como sandbox | El AST no ve el comando real (`$cmd`, `$(…)`); no es una barrera de seguridad |
| Integración específica con Pi por RPC | Ata el proyecto a un agente de terceros y su RPC no está verificado (`planing.md` §9); se reemplaza por el contrato genérico |
| Autosugerencias / menús | Ya existen (`history_hint.rs`, `completion.rs`) |
