# New OpenCrab runtime binaries. Run the core by default; web-gate and nostr-gate
# are included so a deployment can launch each gate as its own process/container.
FROM rust:1.98-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release \
    -p opencrab-app --bin opencrab-social-runtime \
    -p opencrab-web-gate --bin web-gate \
    -p opencrab-nostr-gate --bin nostr-gate

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tzdata \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -r -m -d /app crab
WORKDIR /app
COPY --from=build /src/target/release/opencrab-social-runtime /usr/local/bin/
COPY --from=build /src/target/release/web-gate /usr/local/bin/
COPY --from=build /src/target/release/nostr-gate /usr/local/bin/
USER crab
ENV TZ=Asia/Tokyo
ENTRYPOINT ["opencrab-social-runtime"]
CMD ["/tmp/opencrab.sock", "/app/data/opencrab.db"]
