# The image Athena's per-session sandboxes run (ATHENA_SANDBOX_IMAGE).
#
# OpenSandbox injects its own execd into every sandbox, so this image only
# carries the tools: Chromium and a pinned agent-browser for the browser
# tools, Python with a Jupyter server for run_code, and the usual CLI
# tools for shell. Build (x86_64 or arm64, matching the sandbox host):
#
#   docker build -t athena-sandbox:dev deploy/sandbox-image
#
# See "Sandbox and browser tools" in the README.

FROM debian:13-slim

ARG DEBIAN_FRONTEND=noninteractive

# agent-browser moves fast; bump the version and both digests together.
# Digests are the release assets' sha256 as GitHub lists them.
ARG AGENT_BROWSER_VERSION=v0.38.1
ARG AGENT_BROWSER_SHA256_AMD64=5100149a1903211c889de4e545bf36d90803740cea4f99aa22651649f9205ea1
ARG AGENT_BROWSER_SHA256_ARM64=937b315ee0761e8a62f7950ddcfef9b3d3d8e8d5eb9c9d2bf9e23e5725664511
ARG TARGETARCH

RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends \
      ca-certificates \
      chromium \
      curl \
      fonts-liberation \
      git \
      python3 \
      python3-pip \
      python3-venv \
      tini; \
    rm -rf /var/lib/apt/lists/*

RUN set -eux; \
    arch="${TARGETARCH:-$(dpkg --print-architecture)}"; \
    case "$arch" in \
      amd64) asset=agent-browser-linux-x64; sum="$AGENT_BROWSER_SHA256_AMD64" ;; \
      arm64) asset=agent-browser-linux-arm64; sum="$AGENT_BROWSER_SHA256_ARM64" ;; \
      *) echo "unsupported architecture: $arch" >&2; exit 1 ;; \
    esac; \
    curl -fsSL -o /usr/local/lib/agent-browser \
      "https://github.com/vercel-labs/agent-browser/releases/download/${AGENT_BROWSER_VERSION}/${asset}"; \
    echo "${sum}  /usr/local/lib/agent-browser" | sha256sum -c -; \
    chmod 0755 /usr/local/lib/agent-browser

# Jupyter in its own venv: execd's code interpreter (run_code) talks to it.
# The venv is first on PATH, so `pip install` in the shell works too (Debian
# refuses system-wide installs).
RUN set -eux; \
    python3 -m venv /opt/jupyter; \
    /opt/jupyter/bin/pip install --no-cache-dir jupyter-server==2.21.1 ipykernel==7.3.0
ENV PATH=/opt/jupyter/bin:$PATH

# agent-browser uses the system Chromium. Sandboxes run as root, which
# Chromium only allows with --no-sandbox; the OpenSandbox container is the
# isolation boundary. A wrapper, so the settings hold however execd starts
# the command.
RUN printf '%s\n' \
      '#!/bin/sh' \
      'export AGENT_BROWSER_EXECUTABLE_PATH=/usr/bin/chromium' \
      'export AGENT_BROWSER_ARGS="--no-sandbox,--disable-dev-shm-usage"' \
      'exec /usr/local/lib/agent-browser "$@"' \
      > /usr/local/bin/agent-browser; \
    chmod 0755 /usr/local/bin/agent-browser

COPY --chmod=0755 athena-sandbox-start /usr/local/bin/athena-sandbox-start

WORKDIR /workspace

# Athena creates sandboxes with its own entrypoint, which runs
# athena-sandbox-start when it exists; this default is for `docker run`.
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/athena-sandbox-start"]
