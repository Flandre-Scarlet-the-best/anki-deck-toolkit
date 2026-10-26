# syntax=docker/dockerfile:1@sha256:4edf897a3ffa55b89f906fc8cc78afdb3f1834cc9c7083565e611a8a7d5fe99e
# check=skip=InvalidDefaultArgInFrom
#
# Образ запуска: образ Anki выбранной версии плюс `anki-repo` и `anki-e2e`,
# собранные внутри Docker из снимка исходников, который подготовил runner.
# Сборка в контейнере связывает тестируемый бинарник с проверяемой ревизией и
# не зависит от glibc хоста. Версия Rust — MSRV workspace, её передаёт runner.
# Базовые образы задаёт только runner, поэтому у ARG нет значений по умолчанию.
ARG ANKI_IMAGE
ARG RUST_IMAGE

FROM ${RUST_IMAGE} AS build
ARG RUST_VERSION
ARG SOURCE_TREE_SHA256
ENV RUSTUP_TOOLCHAIN=${RUST_VERSION} \
    CARGO_TERM_COLOR=never
WORKDIR /src
COPY src/ /src/
RUN --mount=type=cache,id=anki-e2e-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=anki-e2e-cargo-target,target=/src/target,sharing=locked \
    find /src/target -mindepth 1 -maxdepth 1 -type d -name '????????????????????????????????????????????????????????????????' -print -exec rm -rf -- {} + \
 && cargo clean --locked -p anki-repo -p anki-e2e \
 && cargo build --locked -p anki-repo -p anki-e2e \
 && mkdir -p /out \
 && cp /src/target/debug/anki-repo /src/target/debug/anki-e2e /out/

FROM ${ANKI_IMAGE}
ARG SOURCE_COMMIT
ARG SOURCE_TREE_SHA256
COPY --from=build /out/anki-repo /out/anki-e2e /usr/local/bin/
ENV ANKI_E2E_SOURCE_COMMIT=${SOURCE_COMMIT} \
    ANKI_E2E_SOURCE_TREE_SHA256=${SOURCE_TREE_SHA256}
LABEL io.github.anki-deck-toolkit.e2e.source-commit="${SOURCE_COMMIT}" \
      io.github.anki-deck-toolkit.e2e.source-tree-sha256="${SOURCE_TREE_SHA256}"
# Единственная записываемая рабочая область сценариев и свидетельств.
RUN install -d -o anki -g anki -m 0700 /work
USER anki
WORKDIR /home/anki
