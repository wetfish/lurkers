# Builds lurker, then copies it into a small image of its own. See
# docs/deploying.md.

FROM rust:1.99.0-slim-trixie AS build
WORKDIR /src

# Build the dependencies on their own first, so a change to lurker's code
# reuses them from Docker's cache
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY messages.toml lurker.example.toml ./
COPY src src
# Newer than the placeholder built above, so cargo rebuilds lurker itself
RUN touch src/main.rs && cargo build --release --locked

FROM debian:trixie-20261005-slim
COPY --from=build /src/target/release/lurker /usr/local/bin/lurker

# The database volume, so a relative database.path lands in it
WORKDIR /data
ENTRYPOINT ["lurker"]
CMD ["/settings/lurker.toml"]
