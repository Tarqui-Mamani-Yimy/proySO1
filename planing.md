# so1Shell — Planificación

> Estado: borrador v2 (2026-10-05). Reemplaza la versión anterior, que era una
> lluvia de ideas sin alcance definido.

## 1. Visión

Construir, en Rust, un entorno de línea de comandos propio compuesto por dos
productos que comparten un contrato:

1. **Una shell interactiva** sobre el motor POSIX `epsh` (este repositorio),
   usable en cualquier terminal.
2. **Una terminal (emulador)** que aprovecha al máximo esa shell: bloques por
   comando, gráficos inline, teclado moderno y soporte nativo para agentes.

Una **capa de agentes** opcional se apoya en ambas.

## 2. Las tres capas (regla de diseño)

Cada funcionalidad pertenece a una sola capa. Si no está claro dónde vive, no se
implementa todavía.

| Capa | Responsabilidad | No hace |
|---|---|---|
| **Shell** | Interpretar comandos, editar la línea, historial, completado, job control, **emitir** marcas de integración (OSC 133 / OSC 7) | Dibujar, manejar fuentes, imágenes |
| **Terminal** | PTY, emulación VT, render, ventanas/paneles, **consumir** marcas para construir bloques | Interpretar comandos |
| **Agentes** | Orquestar agentes, permisos, estado por panel | Reimplementar shell o terminal |

El contrato entre shell y terminal son **secuencias de escape estándar**. Gracias
a eso la shell funciona en kitty, WezTerm, etc., y la terminal funciona con zsh
o fish.

## 3. Qué tomamos de cada proyecto

Cada idea se toma del diseño o de una especificación abierta. **No se copia código
GPL/AGPL** (ver §6).

### kitty — https://github.com/kovidgoyal/kitty (GPL-3.0)
| Idea | Capa |
|---|---|
| Shell integration: marcas de prompt/comando/salida y cwd | Shell emite / Terminal consume |
| Keyboard protocol (press/repeat/release desambiguados) | Terminal emite / Shell consume |
| Graphics protocol (imágenes inline) | Terminal |
| Remote control por socket | Terminal |
| "Kittens": utilidades pequeñas que extienden la terminal | Terminal |

### Warp — https://github.com/warpdotdev/warp (AGPL-3.0; `warpui*` MIT)
| Idea | Capa |
|---|---|
| **Bloques**: cada comando con su salida como unidad (hooks precmd/preexec) | Shell emite / Terminal modela |
| Lección clave: **una grilla VT por bloque**, no una sola grilla | Terminal |
| Editor de entrada real: selección, multilínea, historial visual | Shell |
| Completado basado en firmas de comandos | Shell |
| Render por primitivas en GPU (rect, glyph, image) | Terminal |

### herdr — https://github.com/herdrdev/herdr (Apache-2.0, Rust)
| Idea | Capa |
|---|---|
| Servidor persistente: detach/attach, los paneles sobreviven al cliente | Terminal |
| Estado por panel de agente (trabajando / bloqueado / ocioso) | Terminal + Agentes |
| CLI/socket para que los agentes creen paneles y se coordinen | Terminal + Agentes |
| Referencia técnica: VT parser + `portable-pty` | Terminal |

### Pi Agent — https://github.com/earendil-works/pi (MIT, TypeScript)
| Idea | Capa |
|---|---|
| Núcleo mínimo y extensible | Agentes |
| Modos interactivo / JSON / **RPC** / SDK, para integrarlo desde la terminal | Agentes |
| Extensiones, skills y temas como paquetes | Agentes |

### gentle-shell — https://github.com/Gentleman-Programming/gentle-shell (MIT)
No es una shell POSIX, sino un *harness* de agentes sobre Pi.
| Idea | Capa |
|---|---|
| Flujo de trabajo guiado (exploración, tareas, commits por unidad, TDD) | Agentes |
| Guardrails de revisión y memoria persistente | Agentes |

### Lo que ya aporta epsh (este repo)
- Intérprete POSIX completo y testeado (conformance mksh).
- Parse antes de ejecutar: el AST permite **chequear permisos** antes de correr un comando (clave para agentes).
- `ExternalHandler`, modo `interactive`, `ShellError::Stopped`: primitivas para job control.
- Sinks de stdout/stderr, cancelación y timeout.

## 4. Arquitectura objetivo

```
so1Shell/                  (workspace de Cargo)
├── crates/epsh/           motor POSIX (código actual)
├── crates/shell/          binario de la shell interactiva
│   ├── repl               loop, prompt, señales
│   ├── editor             reedline: historial, hints, completado
│   ├── jobs               fg/bg/jobs sobre ExternalHandler
│   └── integration        OSC 133 / OSC 7, detección de capacidades
├── crates/term-core/      modelo: grilla VT, bloques, paneles (sin UI)
├── crates/terminal/       binario: ventana, render GPU, input
└── crates/agent/          (fase tardía) estado de agentes, RPC, permisos
```

Bases open source verificadas (crates.io, 2026-10-04):

| Pieza | Crate | Licencia |
|---|---|---|
| Editor de línea | `reedline` 0.52 (nushell) | MIT |
| Emulación VT | `alacritty_terminal` 0.26 | Apache-2.0 |
| Parser de escapes | `vte` 0.15 | Apache-2.0 / MIT |
| PTY | `portable-pty` 0.9 (wezterm) | MIT |
| Monitoreo de procesos | `sysinfo` | MIT |
| Terminal de referencia | Rio (`raphamorim/rio`) | MIT |

## 5. Fases

Cada fase termina con algo **usable** y con tests en verde.

### Fase 0 — Fundaciones
- [ ] Inicializar git y hacer un commit base con el estado actual de epsh.
- [ ] Toolchain operativo (`rustup` en PATH, nightly fijado) y `cargo test` en verde.
- [ ] Convertir el repo en un workspace y mover epsh a `crates/epsh`.
- [ ] CI local: `cargo test`, `clippy`, conformance `check.pl`.

### Fase 1 — Shell MVP
- [ ] REPL con prompt, `exit`, Ctrl-C que cancela el comando y no la shell.
- [ ] `reedline`: historial persistente, Ctrl-R, edición multilínea.
- [ ] Ejecución de comandos externos con la terminal en primer plano (`tcsetpgrp`).
- [ ] Archivo de configuración de arranque (rc).

### Fase 2 — Shell moderna
- [ ] Job control: `jobs`, `fg`, `bg`, Ctrl-Z.
- [ ] Integración: emitir OSC 133 (prompt/comando/salida/exit code) y OSC 7 (cwd).
- [ ] Completado de comandos, rutas y variables, e *hints* (ghost text) desde el historial.
- [ ] Resaltado de sintaxis usando el lexer de epsh.
- [ ] Probarla dentro de kitty/WezTerm: los bloques deben verse.

### Fase 3 — Terminal MVP
- [ ] Ventana y render GPU básico (texto, colores, cursor).
- [ ] `portable-pty` lanzando la shell y `alacritty_terminal` como modelo VT.
- [ ] Scroll, selección, copiar/pegar, resize.

### Fase 4 — Terminal con bloques
- [ ] Construir bloques a partir de las marcas OSC 133 (una grilla por bloque).
- [ ] Navegar entre bloques, copiar la salida de un bloque, marcar errores por exit code.
- [ ] Paneles y pestañas.

### Fase 5 — Terminal avanzada
- [ ] Keyboard protocol de kitty (la shell lo consume).
- [ ] Graphics protocol de kitty.
- [ ] Socket de control remoto.
- [ ] Servidor persistente con detach/attach (estilo herdr).

### Fase 6 — Capa de agentes (opcional)
- [ ] Chequeo de permisos de comandos de agentes vía AST de epsh.
- [ ] Estado por panel de agente (trabajando / bloqueado / ocioso).
- [ ] Integración con Pi por su modo RPC.

## 6. Licencias

| Fuente | Licencia | Regla |
|---|---|---|
| epsh | MIT | Base del proyecto |
| kitty | GPL-3.0 | Solo implementar sus **protocolos** (especificación abierta); no copiar código |
| Warp | AGPL-3.0 (excepto `warpui*`, MIT) | Solo ideas de diseño; copiar código obliga a AGPL |
| herdr | Apache-2.0 | Se puede estudiar y reutilizar con atribución |
| Pi, gentle-shell | MIT | Reutilizable con atribución |

Licencia de so1Shell: **pendiente de decidir** (recomendado MIT o Apache-2.0, como epsh).

## 7. Riesgos

- **Alcance**: una terminal con GPU es un proyecto de meses. Por eso va después de la shell.
- **Bloques + programas TUI** (`vim`, `top`): necesitan pantalla alternativa; el modelo de bloques debe convivir con ella.
- **Job control**: el manejo de señales y grupos de procesos es la parte más delicada de la shell.
- **Toolchain nightly**: epsh fija una nightly; hay que confirmar que las crates nuevas compilan con ella.

## 8. Decisiones abiertas

1. Nombre definitivo del proyecto y de los binarios.
2. Licencia de so1Shell.
3. Librería de ventana y render para la terminal (`winit` + `wgpu`, o estudiar Rio).
   Motor VT: `alacritty_terminal` o la alternativa basada en `ghostty-vt` que usa herdr (sin evaluar).
4. Plataformas objetivo (¿solo Linux al inicio?).
5. ~~Roles del equipo de agentes~~ → resuelto en §10.

## 9. Pendiente de verificar

- Detalles exactos de OSC 133 / OSC 7 en la documentación de kitty.
- Paleta de comandos y *workflows* de Warp (vienen de conocimiento previo, no de la documentación).
- Herramientas internas exactas de Pi.

## 10. Roles del equipo (coordinado vía herdr, workspace `w3`)

| Agente herdr | Herramienta / modelo | Rol | Puede escribir en el repo |
|---|---|---|---|
| (orquestador, pane `w3:p3`) | Claude Code / Opus 5.5 | Orquestador: plan, tareas `odd/tasks/`, decisiones, asignación vía `herdr agent prompt`, integración | Solo planificación y documentos de tareas |
| `codex-impl` (pane `w3:p1`) | Codex / GPT-6.1-Sol (high) | Implementador único: código, tests, Cargo; TDD estricto; un commit convencional por unidad de trabajo en rama de feature | Sí (único escritor de código) |
| ~~`agy-research`~~ | ~~Antigravity / Gemini 3.8 Flash~~ | Sesión cerrada por el usuario el 2026-10-05. La verificación independiente pasa a subagentes de Claude (solo lectura) lanzados por el orquestador | No |

Reglas:
- Un solo escritor sobre el mismo árbol de trabajo, para evitar conflictos.
- Ningún agente toma trabajo por iniciativa propia; todo llega asignado por el orquestador.
- Push, PR y merge los decide siempre el usuario.
