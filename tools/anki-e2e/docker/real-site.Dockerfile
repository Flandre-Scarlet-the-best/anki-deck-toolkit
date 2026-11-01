# syntax=docker/dockerfile:1@sha256:4edf897a3ffa55b89f906fc8cc78afdb3f1834cc9c7083565e611a8a7d5fe99e
# check=skip=InvalidDefaultArgInFrom
#
# Real-Site стенд Forvo-Brave: настоящий Brave Stable для Linux, графическая
# сессия с noVNC и `pronunciation-assets`, собранный из снимка исходников.
# Секретов в слоях нет: профиль Brave, сопряжение, пароль VNC и загрузки живут
# только в именованных volumes стенда. Базовые образы задаёт runner.
ARG BASE_IMAGE
ARG RUST_IMAGE

FROM ${RUST_IMAGE} AS build
ARG RUST_VERSION
ENV RUSTUP_TOOLCHAIN=${RUST_VERSION} \
    CARGO_TERM_COLOR=never
RUN apt-get update \
 && apt-get install -y --no-install-recommends cmake clang \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY src/ /src/
RUN --mount=type=cache,id=anki-e2e-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=anki-e2e-real-site-target,target=/src/target,sharing=locked \
    cargo build --locked --release -p asset-store --bin pronunciation-assets \
 && install -D -m 0755 /src/target/release/pronunciation-assets /out/pronunciation-assets

FROM ${BASE_IMAGE}
ARG SOURCE_COMMIT
ARG SOURCE_TREE_SHA256
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates curl gnupg xvfb x11vnc fluxbox novnc websockify dbus-x11 \
      ffmpeg fonts-noto-cjk procps jq \
 && rm -rf /var/lib/apt/lists/*
# Официальная установка Brave Stable: https://brave.com/linux/
RUN curl -fsSLo /usr/share/keyrings/brave-browser-archive-keyring.gpg \
      https://brave-browser-apt-release.s3.brave.com/brave-browser-archive-keyring.gpg \
 && curl -fsSLo /etc/apt/sources.list.d/brave-browser-release.sources \
      https://brave-browser-apt-release.s3.brave.com/brave-browser.sources \
 && apt-get update \
 && apt-get install -y --no-install-recommends brave-browser \
 && rm -rf /var/lib/apt/lists/*
# Политика только этого образа: без менеджера паролей (в контейнере нет связки
# ключей) и без запроса места сохранения. Пользовательский Brave не меняется.
COPY real-site-policy.json /etc/brave/policies/managed/anki-e2e-real-site.json
COPY --from=build /out/pronunciation-assets /usr/local/bin/
COPY real-site-entrypoint.sh /usr/local/bin/real-site-entrypoint
RUN chmod 0755 /usr/local/bin/real-site-entrypoint \
 && userdel -r ubuntu 2>/dev/null || true \
 && useradd -m -u 1000 -s /bin/bash tester \
 && install -d -o tester -g tester -m 0700 /home/tester/.config/BraveSoftware /home/tester/state /home/tester/Downloads
LABEL io.github.anki-deck-toolkit.e2e.source-commit="${SOURCE_COMMIT}" \
      io.github.anki-deck-toolkit.e2e.source-tree-sha256="${SOURCE_TREE_SHA256}"
USER tester
ENV HOME=/home/tester \
    XDG_CONFIG_HOME=/home/tester/state/config \
    XDG_DATA_HOME=/home/tester/state/data \
    DISPLAY=:77
ENTRYPOINT ["/usr/local/bin/real-site-entrypoint"]
CMD ["session"]
