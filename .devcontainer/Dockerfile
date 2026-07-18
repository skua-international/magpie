FROM mcr.microsoft.com/devcontainers/javascript-node:22-bookworm

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:/usr/local/go/bin:${PATH}

RUN apt-get update \
    && apt-get install -y --no-install-recommends --no-install-suggests \
        ca-certificates \
        curl \
        git \
        gzip \
        gnupg \
        lsb-release \
        gh \
        protobuf-compiler \
        tar \
    && rm -rf /var/lib/apt/lists/*

RUN curl https://sh.rustup.rs -sSf | sh -s -- -y --profile minimal --default-toolchain stable \
    && rustup component add clippy rustfmt

RUN go_version="1.25.0" \
    && go_arch="$(dpkg --print-architecture)" \
    && case "$go_arch" in \
      amd64) go_arch=amd64 ;; \
      arm64) go_arch=arm64 ;; \
      *) echo "unsupported architecture: $go_arch" >&2; exit 1 ;; \
    esac \
    && curl -fsSLo /tmp/go.tgz "https://go.dev/dl/go${go_version}.linux-${go_arch}.tar.gz" \
    && rm -rf /usr/local/go \
    && tar -C /usr/local -xzf /tmp/go.tgz \
    && rm /tmp/go.tgz

RUN buf_arch="$(dpkg --print-architecture)" \
        && case "$buf_arch" in \
            amd64) buf_arch=x86_64 ;; \
            arm64) buf_arch=aarch64 ;; \
            *) echo "unsupported architecture: $buf_arch" >&2; exit 1 ;; \
        esac \
        && curl -fsSL "https://github.com/bufbuild/buf/releases/download/v1.71.0/buf-$(uname -s)-${buf_arch}.tar.gz" \
            | tar -xz -C /tmp \
        && install -m 0755 /tmp/buf/bin/buf /usr/local/bin/buf \
        && rm -rf /tmp/buf

RUN GOBIN=/usr/local/bin go install google.golang.org/protobuf/cmd/protoc-gen-go@latest \
        && GOBIN=/usr/local/bin go install connectrpc.com/connect/cmd/protoc-gen-connect-go@latest \
    && npm install --global @bufbuild/protoc-gen-es@latest

RUN curl -fsSL https://raw.githubusercontent.com/helm/helm/main/scripts/get-helm-3 | bash

RUN kubectl_version="$(curl -fsSL https://dl.k8s.io/release/stable.txt)" \
    && kubectl_arch="$(dpkg --print-architecture)" \
    && case "$kubectl_arch" in \
      amd64) kubectl_arch=amd64 ;; \
      arm64) kubectl_arch=arm64 ;; \
      *) echo "unsupported architecture: $kubectl_arch" >&2; exit 1 ;; \
    esac \
    && curl -fsSLo /usr/local/bin/kubectl "https://dl.k8s.io/release/${kubectl_version}/bin/linux/${kubectl_arch}/kubectl" \
    && chmod +x /usr/local/bin/kubectl

RUN mkdir -p /usr/local/cargo /usr/local/rustup \
    && chown -R vscode:vscode /usr/local/cargo /usr/local/rustup

USER vscode
WORKDIR /workspaces/skua-magpie
