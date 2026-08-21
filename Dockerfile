# OpenCrab server — multi-stage build.
#   docker build -t opencrab:local .
#
# ── 起動に必要なマウント / env ─────────────────────────────────────────────
#   - ./config を /app/config にマウント（default.toml）
#   - ./data   を /app/data   にマウント（SQLite + agent workspaces）
#   - env: DISCORD_TOKEN / OPENROUTER_API_KEY / OPENAI_API_KEY... を必要に応じて
#   REST ゲートウェイは 8080 で待ち受ける（config/default.toml）。
#
# ── 運用上の注意（コンテナで動かす人向け・事故りやすい3点）─────────────────
#   1. ./data は永続ボリューム必須。database.path = "data/opencrab.db" は cwd 相対
#      （WORKDIR /app からの相対）なので、ボリュームをマウントしないと DB は
#      コンテナ寿命で揮発する。必ず -v で ./data を永続化すること。
#
#   2. ./config 未マウントは起動時に即落ちる。main.rs の load_config(...)? が
#      config/default.toml を読めないと Err で終了する。これは fail-loud で
#      正しい挙動（黙って動かない状態を避ける）。config は必ずマウントする。
#
#   3. 必須 env が無くても "healthy" のまま黙って無効化される点に注意。
#      config の ${VAR} 展開は未定義時に空文字になる（unwrap_or_default）ため、
#      例えば DISCORD_TOKEN を渡し忘れても、warn ログを出すだけでプロセスは起動し、
#      空キーのプロバイダ/ゲートウェイは登録がスキップされるだけになる。しかも
#      /health は静的に "ok" を返すので HEALTHCHECK は緑のまま。
#      → 「healthy と表示されているのに何も喋らない」状態が起こりうる。必須 env の
#        渡し忘れは HEALTHCHECK では検出できないので、投入前に env を確認すること。
#      （この挙動自体は既存サーバの仕様であり本 Dockerfile の欠陥ではない。別 issue で追跡。）

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
