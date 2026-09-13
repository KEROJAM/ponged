# ponged

Un Pong 2D multijugador **P2P** en Rust. El host simula el juego en un hilo
headless (64 Hz, no depende de la ventana) y el guest recibe snapshots a 30 Hz
que interpola y predice para compensar la latencia. Un **gateway** separado
hace de matchmaker (fila por ELO + rangos) y de relay para atravesar NAT.

Ecosistema: **Bevy** (render + UI) · **libp2p** (red: TCP, noise, yamux, mDNS,
rendezvous, Kademlia, relay, DCUtR) · **Nix flakes** (entorno reproducible) ·
**rusqlite** (historial local y ratings).

## Arquitectura

```
┌─────────────────────────────────────────────┐
│  pong-gateway — servidor headless            │
│  ├─ relay (circuit-relay v2: NAT)            │
│  ├─ rendezvous server (peer discovery)       │
│  ├─ Kademlia DHT server (bootstrap)          │
│  ├─ matchmaking: fila ELO + rangos, SQLite   │
│  └─ identify (detección de agente)           │
└──────────────────────┬──────────────────────┘
                       │ libp2p TCP+noise
          ┌────────────┴────────────┐
          ▼                         ▼
┌──────────────────┐    ┌──────────────────┐
│  Cliente A       │    │  Cliente B       │
│  ├─ Bevy ECS     │    │  ├─ Bevy ECS     │
│  ├─ sim hilo     │◄──►│  ├─ interpolación│
│  │  (64Hz, host) │    │  │  + predicción │
│  ├─ historial    │    │  ├─ historial    │
│  └─ menú/lobby   │    │  └─ menú/lobby   │
└──────────────────┘    └──────────────────┘
```

- El **host** (PeerId menor) corre la simulación autoritativa en un hilo
  dedicado y publica snapshots. El **guest** solo renderiza, interpola y predice.
- **Migración de host** (M7): si el host sale, el guest toma el mando sin volver
  al menú (elegante con ESC, o abrupta si se desconecta).
- **Victoria**: el primero en llegar a **5 puntos** gana. Perder 5-0 muestra
  **"YOU JUST GOT PONGED!"**.
- **Aceleración por rally**: cada toque de paleta devuelve la pelota un poco
  más rápido (hasta ~2.5×), así los intercambios largos exigen reflejos.

## Rangos

El ELO interno se muestra como rango (empezando en **Brick** para jugadores
nuevos):

| Rango          | ELO mínimo |
|----------------|-----------:|
| Brick          |         0  |
| Bronze         |     1,000  |
| Silver         |     1,200  |
| Gold           |     1,400  |
| Platinum       |     1,600  |
| Diamond        |     1,800  |
| Obsidian       |     2,000  |
| Pong Master    |     2,200  |
| Pong Legend    |     2,400  |

## Requisitos

- [Nix](https://nixos.org/) (flakes) — gestiona Rust + las bibliotecas del
  systema (wayland, x11, udev). No instales Rust por separado.

## Compilar / correr (dev)

```bash
# Todos los bins (cliente + gateway)
nix develop -c cargo build --bins

# Prueba de humo: lanza el gateway en localhost
nix develop -c cargo run --bin pong-gateway -- --listen /ip4/127.0.0.1/tcp/4001

# Cliente (otra terminal)
nix develop -c cargo run --bin ponged
```

El cliente Bevy soporta **Wayland y X11** (winit elige el backend disponible:
X11/XWayland entra también cuando `XDG_SESSION_TYPE=wayland`). Para forzar uno
en concreto: `WINIT_UNIX_BACKEND=x11 nix develop -c cargo run --bin ponged`.

En el menú: **Connect to gateway** → regístrate solo → **Join WAN queue**
(o challa peers de la LAN que aparece en la lista). El PeerId del gateway se
imprime al arrancarlo; si no es localhost, pasa la dirección con
`PONG_GATEWAY`:

```bash
PONG_GATEWAY=/ip4/<ip-del-gateway>/tcp/4001 nix develop -c cargo run --bin ponged
```

## Build de release / binarios estáticos

```bash
# Binarios musl 100% estáticos (lo usa el release.yml de GitHub Actions)
nix build .#pong-gateway --no-link --print-out-paths
nix build .#pong-client --no-link --print-out-paths
```

## Docker (gateway)

```bash
docker build -t pong-gateway .
mkdir -p data
docker run -p 4001:4001 -v "$PWD/data:/data" pong-gateway \
  --listen /ip4/0.0.0.0/tcp/4001 --public <HOST_OR_IP>
```

Persiste la identidad del gateway (`gateway.key`) y los ratings
(`gateway.sqlite`) en el volumen `/data`.

> En WAN el gateway debe ser alcanzable; `--public` anuncia la IP/host que los
> clientes deben dialear para el relay. La imagen OCI también se puede producir
> desde el flake: `nix build .#dockerImage`.

## Sala de partidas / despliegue en WAN

1. Arranca el gateway con `--public <IP pública>`.
2. Cada cliente configura `PONG_GATEWAY` hacia esa IP.
3. Ambos se registran → la fila los empareja por ELO → `MatchFound` con
   direcciones `/p2p-circuit` a través del gateway → DCUtR intenta abrir
   conexión directa (P2P). Si no, todo el tráfico fluye por el relay.

## Tests

```bash
nix develop -c cargo test
```

18 tests: rangos (Brick→Pong Legend), ELO del gateway, roundtrip CBOR del
protocolo, retrocompatibilidad del snapshot y condición de victoria del sim.

## Estructura

```
src/lib.rs           Tipos compartidos (solo protocolo, sin Bevy)
src/protocol.rs      Protocolo de red (snapshots + RPC gateway)
src/main.rs          Cliente Bevy: ECS, HUD, overlay de victoria, glue net
src/sim.rs           Simulación headless del host (64 Hz) + condición de victoria
src/networking.rs    Capa libp2p (swarm, behaviours, eventos)
src/menu.rs          Lobby: UI, gateway flow, migración de host
src/history.rs       Historial local SQLite + reporte al gateway
src/bin/gateway.rs   Servidor: relay, rendezvous, kad, matchmaking por ELO
```