# The bot as a container: `trade-bot run --paper` by default.
#
#   docker build -t trade-bot .
#   docker run --rm --env-file .env -v "$PWD/config/config.toml:/app/config/config.toml:ro" \
#     -v trade-bot-data:/app/data trade-bot
#
# The books (order journal, fills, holdings, strategy state) live in
# /app/data: mount a persistent volume there, or every restart forgets
# which orders on Binance are the bot's. Live trading is never the
# default: it needs the config locks and `run --live --i-understand`
# typed as the container command (CLAUDE.md invariant 7).

# Debian base + rustup rather than a rust:<version> image: the toolchain
# comes from rust-toolchain.toml, the same exact pin as local and CI.
FROM debian:trixie-slim AS build
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl build-essential pkg-config libssl-dev \
 && rm -rf /var/lib/apt/lists/*
ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo PATH=/usr/local/cargo/bin:$PATH
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p cli \
 && cp target/release/cli /trade-bot

FROM debian:trixie-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates libssl3t64 \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home-dir /app bot \
 && mkdir -p /app/data /app/config \
 && chown bot /app/data
COPY --from=build /trade-bot /usr/local/bin/trade-bot
COPY config/config.example.toml /app/config/
COPY config/examples /app/config/examples
WORKDIR /app
USER bot
VOLUME /app/data
# Binary is PID 1 and handles SIGTERM itself: stop trading, cancel its
# open orders, record their last fills. Give `docker stop` time for that
# (`-t 120`); a second signal abandons the close-out.
ENTRYPOINT ["trade-bot"]
CMD ["run", "--paper"]
