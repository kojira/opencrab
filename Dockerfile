# OpenCrab server — multi-stage build.
#   docker build -t opencrab:local .
# Runtime expects:
#   - ./config mounted at /app/config (default.toml)
#   - ./data   mounted at /app/data   (SQLite + agent workspaces)
#   - env: DISCORD_TOKEN / OPENROUTER_API_KEY / OPENAI_API_KEY... as used
# The REST gateway listens on 8080 (see config/default.toml).

# Rust は CI とバージョンを揃える（.github/workflows/ci.yml の dtolnay/rust-toolchain@1.98.0）。
# CI と Docker で toolchain がずれると、片方でだけ通る/落ちる状態になるため固定する。
FROM rust:1.98-bookworm AS build
WORKDIR /src

# ビルド依存は CI の "Install system dependencies" と同じ（.github/workflows/ci.yml）。
#   - reqwest(native-tls) → OpenSSL: libssl-dev, pkg-config
#   - songbird/audiopus(discord の音声) → opus: cmake, libopus-dev
# これらが無いと opencrab-server は既定 features でビルドできない。
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
       cmake pkg-config libopus-dev libssl-dev \
    && rm -rf /var/lib/apt/lists/*

COPY . .

# 既定 features（discord + nostr + web）でビルドする＝本番サーバと同じ全部入り。
# ここで `--features` を渡さないのは、opencrab-server の default が
# ["discord","nostr","web"] のため（crates/server/Cargo.toml）。個別に絞りたく
# なった場合は `--no-default-features --features <...>` を使うこと。
RUN cargo build --release -p opencrab-server

FROM debian:bookworm-slim
# 実行時共有ライブラリ:
#   - libssl3    : reqwest(native-tls)
#   - libopus0   : songbird/audiopus（システム libopus にリンクする場合）
# curl は HEALTHCHECK 用、ca-certificates は TLS 検証用。
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
       ca-certificates tzdata curl libssl3 libopus0 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -r -m -d /app crab
WORKDIR /app
COPY --from=build /src/target/release/opencrab-server /usr/local/bin/opencrab-server
USER crab
ENV TZ=Asia/Tokyo
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --retries=3 \
    CMD curl -fsS http://localhost:8080/health || exit 1
CMD ["opencrab-server"]
