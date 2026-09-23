# Despliegue en AWS — Pong 2D P2P (ponged-gateway)

Guía para correr el `ponged-gateway` en AWS como **relay + rendezvous +
matchmaking** para partidas WAN. El gateway es un solo binario headless
(`src/bin/gateway.rs`, M6) que:

- **Relay** (circuit-relay v2): NAT-traversal para los clientes.
- **Rendezvous server**: punto de descubrimiento público entre jugadores.
- **Kademlia server**: bootstrap de DHT (fallback de descubrimiento).
- **Matchmaking**: fila ELO + push `MatchFound` con direcciones `/p2p-circuit`.

El **cliente** se conecta con la variable `PONG_GATEWAY`:

```bash
PONG_GATEWAY=/ip4/<IP_PUBLICA>/tcp/4001 nix develop -c cargo run --bin ponged-cliente
```

---

## Requisitos previos

- Repo clonado con flake (`. # `) y `Cargo.lock`.
- Para el camino Docker: `docker` instalado localmente.
- Para el camino Nix/NixOS: `nix` con flake habilitado.

---

## Opción 1 — Docker (ECR + EC2)

### 1. Build y push de la imagen

```bash
docker build -t ponged-gateway .

# ECR
aws ecr create-repository --repository-name ponged-gateway
aws ecr get-login-password | docker login --username AWS --password-stdin \
  <aws_account>.dkr.ecr.<region>.amazonaws.com

docker tag ponged-gateway <aws_account>.dkr.ecr.<region>.amazonaws.com/ponged-gateway:latest
docker push <aws_account>.dkr.ecr.<region>.amazonaws.com/ponged-gateway:latest
```

### 2. EC2

- Instancia `t3.micro` (free-tier suficiente) con la imagen Amazon Linux/Ubuntu.
- **Security Group**: abrir entrante **TCP 4001** (y **TCP 22** para SSH).
- **Elastic IP**: asígnale una y úsala siempre en `--public`. Si cambia la IP,
  todos los clientes deben reconfigurar `PONG_GATEWAY`.

### 3. Ejecutar el contenedor

```bash
docker run -d --restart=unless-stopped \
  --name ponged-gateway \
  -p 4001:4001 \
  -v pong-data:/data \
  <aws_account>.dkr.ecr.<region>.amazonaws.com/ponged-gateway:latest \
  --listen /ip4/0.0.0.0/tcp/4001 --public <EIP_ELASTICA>
```

- `--public <host>` anuncia la IP/host pública en las direcciones de relay
  (necesario cuando el gateway no está en localhost).
- El volumen `/data` persiste `gateway.key` (identidad/PeerId) y
  `gateway.sqlite` (ratings). **No lo borres**: si se pierde `gateway.key`,
  cambia el PeerId y los ratings.

### 4. Clientes conectan

```bash
PONG_GATEWAY=/ip4/<EIP_ELASTICA>/tcp/4001 nix develop -c cargo run --bin ponged-cliente
```

---

## Opción 2 — NixOS en EC2 (módulo del flake)

El flake trae `nixosModules.ponged-gateway` (servicio systemd).

### Opciones del módulo

| Opción                        | Default                          | Descripción                    |
|-------------------------------|----------------------------------|--------------------------------|
| `services.ponged-gateway.enable` | `false`                          | Activa el servicio            |
| `... .package`                 | `pkgs.ponged-gateway`              | Paquete a usar                |
| `... .listen`                  | `/ip4/0.0.0.0/tcp/4001`          | Multiaddr de escucha          |
| `... .public`                  | `127.0.0.1`                      | Host/IP pública para relay    |
| `... .openFirewall`            | `false`                          | Abre TCP 4001 firewall        |

### Instrucciones

1. Lanza un EC2 con una **AMI de NixOS** (búscala en Community AMIs / consola de
   AWS). Asigna una **Elastic IP** para `--public`.
2. En la máquina local o en la instancia, una configuración de flake:

```nix
# flake.nix (deploy) — ej. host "prod"
{ inputs = { nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
             pong.url = "path:/ruta/a/Proyecto-Final"; # o tu repo git
           };
  outputs = { pong, ... }: {
    nixosConfigurations.prod = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        pong.nixosModules.ponged-gateway
        ({ config, ... }: {
          services.ponged-gateway = {
            enable = true;
            public = "54.xxx.xxx.xxx";   # Elastic IP
            openFirewall = true;
          };
          # usuarios/SSH, disco (EBS), etc.
        })
      ];
    };
  };
}
```

3. Desplegar remotamente:

```bash
nixos-rebuild switch --flake .#prod --target-host root@<EIP>
```

El servicio queda con `Restart=on-failure`, `DynamicUser` y
`StateDirectory=/var/lib/ponged-gateway` (persisten `gateway.key` +
`gateway.sqlite`).

### Nota (bug actual del módulo)

`ExecStart` no pasa `--db`/`--key` ni systemd fija `WorkingDirectory`, por lo
que con `DynamicUser` el proceso intentaría escribir en `/` en vez de
`/var/lib/ponged-gateway`. Fix necesario en `flake.nix`:

```nix
serviceConfig = {
  ExecStart = ...;
  WorkingDirectory = "/var/lib/ponged-gateway";
  StateDirectory = "ponged-gateway";
  DynamicUser = true;
  Restart = "on-failure";
  RestartSec = 5;
};
```

---

## Opción 3 — Binario estático en cualquier instancia

El flake compila `ponged-gateway` **estático musl** (sin dependencias de sistema).

```bash
# máquina local
nix build .#ponged-gateway --no-link --print-out-paths
scp result/bin/ponged-gateway ec2-user@<ip>:/usr/local/bin/
```

En la instancia (sin NixOS, cualquier Linux con Nix no hace falta nada más),
un servicio systemd simple:

```ini
# /etc/systemd/system/ponged-gateway.service
[Unit]
Description=Pong P2P Matchmaking Gateway
After=network.target

[Service]
ExecStart=/usr/local/bin/ponged-gateway \
  --listen /ip4/0.0.0.0/tcp/4001 --public 54.xxx.xxx.xxx
WorkingDirectory=/var/lib/ponged-gateway
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
```

```bash
sudo mkdir -p /var/lib/ponged-gateway
sudo systemctl daemon-reload && sudo systemctl enable --now ponged-gateway
```

Abrir **TCP 4001** en el Security Group.

---

## Opción 4 — Nix genera el servicio systemd en cualquier distro

Corre sobre **Debian/Ubuntu/Fedora/Arch** (cualquier distro con systemd).
El flake construye el binario estático y **genera un `.service` listo**,
que un script instala y activa automáticamente.

### Prerequisitos en la instancia

```bash
# Debian/Ubuntu
curl -L https://nixos.org/nix/install | sh -s -- --daemon
# reinicia la shell o: source /etc/profile.d/nix.sh
```

### Instalar y habilitar el servicio

```bash
nix run .#installSystemd
```

Ese script:

1. Copia `ponged-gateway` (estático, sin dependencias) a `/usr/local/bin/`.
2. Genera el unit `/etc/systemd/system/ponged-gateway.service` con:
   - `EnvironmentFile=/etc/ponged-gateway.env` → lee la IP pública.
   - `StateDirectory=ponged-gateway` → crea `/var/lib/ponged-gateway`
     para persistir `gateway.key` + `gateway.sqlite`.
   - `Restart=on-failure`, `RestartSec=5`.
3. Crea `/etc/ponged-gateway.env` la primera vez.
4. `systemctl daemon-reload && systemctl enable --now ponged-gateway`.

### Configurar la IP pública

```bash
sudo -E vim /etc/ponged-gateway.env   # PONG_PUBLIC=<EIP_ELASTICA>
sudo systemctl restart ponged-gateway
```

Al quedar la IP en un archivo separado, si cambia la Elastic IP solo se
edita el `.env` y se reinicia — **no hay que regenerar nada**.

### Chequear

```bash
systemctl status ponged-gateway
journalctl -u ponged-gateway -f
```

Abrir **TCP 4001** en el Security Group y probar con:

```bash
PONG_GATEWAY=/ip4/<EIP_ELASTICA>/tcp/4001 nix develop -c cargo run --bin ponged-cliente
```

---

## Notas de WAN

- El descubrimiento es: **mDNS** (LAN, no aplica a AWS) + **rendezvous** (WAN)
  + fila del gateway (matchmaking). El cliente consulta el rendezvous cada 5 s
  mientras busca (`update_discovery`, Nota 3 en `PROGRESO.org`).
- El transporte solo es **TCP** (sin QUIC), así que DCUtR no hace hole-punch
  real: **todo el tráfico de partida fluye a través del relay**. Es el
  comportamiento esperado y suficiente para el proyecto.
- Para IP fija barata: **Elastic IP** (gratis estando asociada a una instancia
  corriendo) o un **dominio** con `/dns4/<dominio>/tcp/4001` en vez de `/ip4/`.
- `--public` siempre debe ser la dirección **externa** (Pública/EIP), no la
  interna de la VPC.