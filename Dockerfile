FROM rust:latest as builder

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Create a new empty shell project
WORKDIR /usr/src/redlib
COPY . .

# Build the application
RUN cargo build --release

# Runtime stage
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    wget \
    && rm -rf /var/lib/apt/lists/*

# Copy the binary from builder
COPY --from=builder /usr/src/redlib/target/release/redlib /usr/local/bin/

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
ENV REDLIB_DEFAULT_SHOW_NSFW=off
ENV REDLIB_DEFAULT_USE_HLS=off
ENV REDLIB_DEFAULT_HIDE_HLS_NOTIFICATION=off
ENV REDLIB_DEFAULT_AUTOPLAY_VIDEOS=off
ENV REDLIB_BANNER_TEXT=Redlib

# Tell Docker to expose port 8080
EXPOSE 8080

# Run a healthcheck every minute to make sure redlib is functional
HEALTHCHECK --interval=1m --timeout=3s CMD wget --spider -q http://localhost:8080/settings || exit 1

CMD ["redlib"]

