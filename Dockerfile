# OpenCrab server — multi-stage build.
#   docker build -t opencrab:local .
# Runtime expects:
#   - ./config mounted at /app/config (default.toml)
#   - ./data   mounted at /app/data   (SQLite + agent workspaces)
#   - env: DISCORD_TOKEN / OPENROUTER_API_KEY / OPENAI_API_KEY... as used
# The REST gateway listens on 8080 (see config/default.toml).

FROM rust:1.89-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --features discord -p opencrab-server

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tzdata curl \
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
