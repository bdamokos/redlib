# syntax=docker/dockerfile:1.7

FROM rust:1.94-bookworm as builder

ARG GIT_HASH=dev

# Install build dependencies
RUN apt-get update && apt-get install -y \
    cmake \
    clang \
    perl \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /usr/src/redlib

# Prime Cargo's dependency cache before copying the full source tree.
COPY Cargo.toml Cargo.lock build.rs ./
RUN mkdir src && printf 'fn main() {}\n' > src/main.rs
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git/db,sharing=locked \
    --mount=type=cache,target=/usr/src/redlib/target,sharing=locked \
    cargo build --release --locked --bin redlib
RUN rm src/main.rs && rmdir src

COPY src ./src
COPY static ./static
COPY templates ./templates

# Build the application
ENV GIT_HASH=${GIT_HASH}
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git/db,sharing=locked \
    --mount=type=cache,target=/usr/src/redlib/target,sharing=locked \
    cargo build --release --locked --bin redlib --bin subreddit_semantic_backfill --bin subreddit_content_semantic_backfill && \
    install -Dm755 target/release/redlib /tmp/redlib && \
    install -Dm755 target/release/subreddit_semantic_backfill /tmp/subreddit_semantic_backfill && \
    install -Dm755 target/release/subreddit_content_semantic_backfill /tmp/subreddit_content_semantic_backfill

# Runtime stage
FROM debian:bookworm-slim

ARG GIT_HASH=dev

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    ffmpeg \
    python3-venv \
    wget \
    && rm -rf /var/lib/apt/lists/*

RUN python3 -m venv /opt/redlib-promo && \
    /opt/redlib-promo/bin/pip install --no-cache-dir opencv-python-headless==5.0.0.93

# Copy the binary from builder
COPY --from=builder /tmp/redlib /usr/local/bin/redlib
COPY --from=builder /tmp/subreddit_semantic_backfill /usr/local/bin/subreddit_semantic_backfill
COPY --from=builder /tmp/subreddit_content_semantic_backfill /usr/local/bin/subreddit_content_semantic_backfill

# Copy the templates directory
COPY --from=builder /usr/src/redlib/templates /templates
COPY --from=builder /usr/src/redlib/static /static
COPY scripts/snap_promo_detector.py /usr/local/lib/redlib/snap_promo_detector.py

RUN mkdir -p /var/lib/redlib/cache /var/lib/redlib/state && \
    chown -R nobody:nogroup /var/lib/redlib

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
ENV REDLIB_CACHE_DIR=/var/lib/redlib/cache
ENV REDLIB_PREFS_DB=/var/lib/redlib/state/preferences.sqlite3
ENV REDLIB_PROMO_CLASSIFIER_PYTHON=/opt/redlib-promo/bin/python3
ENV REDLIB_PROMO_CLASSIFIER_SCRIPT=/usr/local/lib/redlib/snap_promo_detector.py
# Keep glibc from creating a large arena per Tokio/blocking worker. Runtime
# environments may still override this for allocator A/B tests.
ENV MALLOC_ARENA_MAX=2

LABEL org.opencontainers.image.title="Redlib" \
    org.opencontainers.image.revision="${GIT_HASH}"

# Tell Docker to expose port 8080
EXPOSE 8080

CMD ["redlib"]
