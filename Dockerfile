FROM node:22-trixie-slim AS web
WORKDIR /src
RUN corepack enable
COPY package.json pnpm-lock.yaml pnpm-workspace.yaml ./
COPY apps/client apps/client
# 只要清单：frozen-lockfile 要求 workspace 成员齐全，但 --filter 不装它的依赖（electron）。
COPY apps/desktop/package.json apps/desktop/package.json
COPY packages packages
RUN pnpm install --frozen-lockfile --filter "@komo/client..." \
    && pnpm --filter @komo/client build

FROM rust:1-trixie AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates crates
RUN cargo build --release --locked -p komo

FROM debian:trixie-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git procps \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/komo /usr/local/bin/komo
# 网页壳按「二进制旁的 web/」找（komo-gateway/src/web.rs）。
COPY --from=web /src/apps/client/dist/client /usr/local/bin/web
# 默认只听回环，容器外连不进来。
ENV KOMO_HOME=/data KOMO_LISTEN=0.0.0.0:7777
# 裸 `komo` 把 Gateway 拉到后台就退出，容器会跟着停，所以要前台模式。
CMD ["komo", "gateway", "--foreground"]
