#!/bin/bash
# Графическая сессия Real-Site стенда. VNC и noVNC runner публикует только на
# 127.0.0.1 хоста; пароль VNC случайный и хранится в volume состояния (0600).
set -euo pipefail
PROFILE=/home/tester/.config/BraveSoftware/Brave-Browser
STATE=/home/tester/state
EXTENSION=/home/tester/state/data/anki-deck-toolkit/forvo-brave-adapter

# Все процессы Brave и отдельно главный (без --type=): сигнал получает только он,
# чтобы браузер завершился штатно и сохранил сессию.
brave_pids() { pgrep -f '^/opt/brave.com/brave/brave( |$)' || true; }
brave_main() {
  for p in $(brave_pids); do
    tr '\0' ' ' < "/proc/$p/cmdline" 2>/dev/null | grep -q -- '--type=' || echo "$p"
  done
}

case "${1:-session}" in
session)
  umask 077
  if [ ! -s "$STATE/vnc.pass" ]; then
    head -c 18 /dev/urandom | base64 | tr -d '/+=' | head -c 16 > "$STATE/vnc.txt"
    x11vnc -storepasswd "$(cat "$STATE/vnc.txt")" "$STATE/vnc.pass" >/dev/null 2>&1
  fi
  Xvfb :77 -screen 0 1440x900x24 -nolisten tcp >/tmp/xvfb.log 2>&1 &
  for _ in $(seq 100); do [ -e /tmp/.X11-unix/X77 ] && break; sleep 0.1; done
  eval "$(dbus-launch --sh-syntax)"
  echo "export DBUS_SESSION_BUS_ADDRESS='$DBUS_SESSION_BUS_ADDRESS'" > /tmp/dbus.env
  fluxbox >/tmp/fluxbox.log 2>&1 &
  # -skip_lockkeys: регистр приходит готовым в keysym клиента, повторное
  # применение Caps Lock на стороне X путало бы буквы.
  x11vnc -display :77 -rfbport 5900 -rfbauth "$STATE/vnc.pass" -forever -shared -quiet -skip_lockkeys >/tmp/x11vnc.log 2>&1 &
  websockify --web /usr/share/novnc 0.0.0.0:6080 127.0.0.1:5900 >/tmp/novnc.log 2>&1 &
  echo "real-site session ready"
  exec sleep infinity ;;
brave)
  [ -n "$(brave_pids)" ] && { echo "Brave уже запущен"; exit 0; }
  . /tmp/dbus.env
  args=(--user-data-dir="$PROFILE" --password-store=basic --no-first-run --no-default-browser-check)
  [ -n "${REAL_SITE_PROXY:-}" ] && args+=(--proxy-server="$REAL_SITE_PROXY")
  # Расширение адаптера из каталога setup; режим разработчика не нужен.
  [ -f "$EXTENSION/manifest.json" ] && args+=(--load-extension="$EXTENSION")
  nohup brave-browser "${args[@]}" >>"$STATE/brave.log" 2>&1 &
  # Падение zygote (например, без песочницы) проявляется в первые секунды.
  sleep 4
  [ -n "$(brave_main)" ] && { echo "Brave запущен"; exit 0; }
  echo "Brave не запустился, см. $STATE/brave.log" >&2; exit 1 ;;
sandbox-check)
  # Песочница включена, если renderer живёт в собственном user namespace.
  own=$(readlink /proc/1/ns/user); total=0; isolated=0
  for p in $(pgrep -f '^/opt/brave.com/brave/brave --type=renderer' || true); do
    ns=$(readlink "/proc/$p/ns/user" 2>/dev/null) || continue
    total=$((total + 1)); [ "$ns" != "$own" ] && isolated=$((isolated + 1))
  done
  echo "{\"renderers\":$total,\"isolated\":$isolated}" ;;
stop-brave)
  main=$(brave_main); [ -z "$main" ] && exit 0
  kill -TERM $main 2>/dev/null || true
  for _ in $(seq 200); do [ -z "$(brave_pids)" ] && exit 0; sleep 0.1; done
  echo "Brave не завершился за 20 с" >&2; exit 1 ;;
version)
  brave-browser --version ;;
*) echo "неизвестная команда: $1" >&2; exit 2 ;;
esac
