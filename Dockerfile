# syntax=docker/dockerfile:1.7

FROM rust:1.94-bookworm as builder

ARG GIT_HASH=dev

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /usr/src/redlib

# Prime Cargo's dependency cache before copying the full source tree.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && printf 'fn main() {}\n' > src/main.rs
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git/db,sharing=locked \
    --mount=type=cache,target=/usr/src/redlib/target,sharing=locked \
    cargo build --release --locked --bin redlib
RUN rm src/main.rs && rmdir src

COPY . .

# Build the application
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git/db,sharing=locked \
    --mount=type=cache,target=/usr/src/redlib/target,sharing=locked \
    cargo build --release --locked --bin redlib && \
    cp target/release/redlib /tmp/redlib

# Runtime stage
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    ffmpeg \
    wget \
    && rm -rf /var/lib/apt/lists/*

# Copy the binary from builder
COPY --from=builder /tmp/redlib /usr/local/bin/redlib

# Copy the templates directory
COPY --from=builder /usr/src/redlib/templates /templates
COPY --from=builder /usr/src/redlib/static /static

RUN useradd -r -s /bin/false redlib
USER redlib

# Set environment variables
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

# Tell Docker to expose port 8080
EXPOSE 8080

CMD ["redlib"]
