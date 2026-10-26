# syntax=docker/dockerfile:1
# check=skip=InvalidDefaultArgInFrom
#
# Образ настоящей Linux Anki Desktop выбранной версии для E2E-стенда anki-e2e.
# Версию, архив и его SHA-256 передаёт Rust runner после проверки по первичному
# источнику релизов; номер версии здесь не закреплён. Аддоны в образ НЕ входят:
# первоначальную установку AnkiConnect и CrowdAnki проверяют сами сценарии.
# Базовые образы задаёт только runner, поэтому у ARG нет значений по умолчанию.
ARG BASE_IMAGE
FROM ${BASE_IMAGE}

ARG ANKI_VERSION
ARG ANKI_SHA256
ARG ANKI_ARCHIVE

ENV DEBIAN_FRONTEND=noninteractive \
    LANG=C.UTF-8 \
    LC_ALL=C.UTF-8

# Зависимости официальной сборки (список DEBIAN_DEPS из её install.sh) и библиотеки,
# которых требуют модули Qt/QtWebEngine; Xvfb — виртуальный дисплей, dbus и
# at-spi2-core — шина доступности для пассивного наблюдения за окнами, x11vnc —
# только для явного диагностического просмотра.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates zstd xvfb dbus at-spi2-core x11vnc fonts-dejavu-core \
      libdbus-1-3 libfontconfig1 libfreetype6 libgl1 libnss3 \
      libxcb-icccm4 libxcb-image0 libxcb-keysyms1 libxcb-randr0 \
      libxcb-render-util0 libxcb-shape0 libxcb-xinerama0 libxcb-xkb1 \
      libxcomposite1 libxcursor1 libxi6 libxkbcommon0 libxkbcommon-x11-0 \
      libxrandr2 libxrender1 libxtst6 libglib2.0-0t64 \
      libegl1 libxcb-cursor0 libxdamage1 libgssapi-krb5-2 libpulse0 \
      libwayland-cursor0 libasound2t64 \
 && rm -rf /var/lib/apt/lists/*

# Архив повторно сверяется с SHA-256, проверенным runner по upstream digest.
# Распаковка повторяет копирование файлов официального install.sh в отдельный
# префикс, без xdg-mime интеграции рабочего стола, которой в контейнере нет.
COPY ${ANKI_ARCHIVE} /tmp/anki.tar.zst
RUN echo "${ANKI_SHA256}  /tmp/anki.tar.zst" | sha256sum -c - \
 && mkdir -p /opt/anki \
 && tar --zstd -xf /tmp/anki.tar.zst -C /opt/anki --strip-components=1 \
 && rm /tmp/anki.tar.zst \
 && ln -s /opt/anki/anki /usr/local/bin/anki

RUN useradd --create-home --uid 10001 --shell /usr/sbin/nologin anki

LABEL org.opencontainers.image.title="anki-e2e Anki ${ANKI_VERSION}" \
      io.github.anki-deck-toolkit.e2e.anki-version="${ANKI_VERSION}" \
      io.github.anki-deck-toolkit.e2e.anki-sha256="${ANKI_SHA256}"
