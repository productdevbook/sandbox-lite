FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY assets ./assets
RUN cargo build --release

FROM debian:bookworm-slim
RUN useradd --system --home /data sandbox && mkdir -p /data /bases && chown sandbox:sandbox /data
COPY --from=build /src/target/release/sandbox-lite /usr/local/bin/sandbox-lite
COPY examples /bases
USER sandbox
EXPOSE 4321
VOLUME ["/data"]
ENTRYPOINT ["sandbox-lite", "--listen", "0.0.0.0:4321", "--bases", "/bases", "--data-dir", "/data"]
