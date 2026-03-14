ARG RUST_BUILDER_VERSION=slim-bookworm
ARG DEBIAN_RUNTIME_VERSION=bookworm-slim

FROM rust:${RUST_BUILDER_VERSION} AS builder

WORKDIR /redlib

# Prime the dependency cache before copying the full source tree.
COPY Cargo.lock Cargo.toml ./
RUN mkdir src && echo "fn main() { panic!(\"why am i running?\") }" > src/main.rs
RUN cargo build --release --locked --bin redlib
RUN rm ./src/main.rs && rmdir ./src

# Copy the application and build the release binary.
COPY . ./
RUN cargo build --release --locked --bin redlib

FROM debian:${DEBIAN_RUNTIME_VERSION} AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    wget \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /redlib/target/release/redlib /usr/local/bin/redlib
COPY --from=builder /redlib/templates /templates
COPY --from=builder /redlib/static /static

RUN useradd \
    --no-create-home \
    --password "!" \
    --comment "user for running redlib" \
    redlib
USER redlib

ENV REDLIB_DEFAULT_THEME=system
ENV REDLIB_DEFAULT_FRONT_PAGE=default
ENV REDLIB_DEFAULT_LAYOUT=card
ENV REDLIB_DEFAULT_WIDE=off
ENV REDLIB_DEFAULT_POST_SORT=hot
ENV REDLIB_DEFAULT_COMMENT_SORT=confidence
ENV REDLIB_DEFAULT_SHOW_NSFW=on
ENV REDLIB_DEFAULT_USE_HLS=off
ENV REDLIB_DEFAULT_HIDE_HLS_NOTIFICATION=off
ENV REDLIB_DEFAULT_AUTOPLAY_VIDEOS=off
ENV REDLIB_BANNER_TEXT=Redlib

EXPOSE 8080

HEALTHCHECK --interval=1m --timeout=3s CMD wget --spider -q http://localhost:8080/settings || exit 1

LABEL org.opencontainers.image.authors="sigaloid"

CMD ["redlib"]
