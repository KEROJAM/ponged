# ponged-gateway — matchmaking + relay gateway (M6)
#
# Imagen mínima del gateway. Solo compila el binario `pong-gateway`
# (sin Bevy), así que el builder no necesita wayland/udev/vulkan.
#
# Build:    docker build -t pong-gateway .
# Run:      docker run -p 4001:4001 -v "$PWD/data:/data" pong-gateway
#           [--listen /ip4/0.0.0.0/tcp/4001 --public HOST_OR_IP]

FROM rust:1-bookworm AS builder
WORKDIR /app

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential pkg-config \
    && rm -rf /var/lib/apt/lists/*

# Cachear dependencias: copiar solo Cargo.toml / Cargo.lock primero.
COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release --bin pong-gateway

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/pong-gateway /usr/local/bin/pong-gateway

# El gateway escribe su identidad (gateway.key) y ratings (gateway.sqlite).
WORKDIR /data
VOLUME ["/data"]

EXPOSE 4001

ENTRYPOINT ["pong-gateway"]