FROM ubuntu:24.04
RUN apt-get update && apt-get install -y --no-install-recommends \
    git curl ca-certificates build-essential python3 \
    && rm -rf /var/lib/apt/lists/*
