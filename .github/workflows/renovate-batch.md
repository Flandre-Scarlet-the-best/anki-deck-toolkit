---
description: "Ручная проверка Codex через GitHub Copilot"
on:
  workflow_dispatch:
permissions:
  contents: read
engine:
  id: codex
  model: copilot/gpt-6-luna
  config: |
    model_reasoning_effort = "max"
tools:
  bash: ["*"]
  edit: false
safe-outputs:
  create-issue: false
timeout-minutes: 15
---

# Проверка подключения Codex

Это временный диагностический режим будущей автоматизации Renovate.

Прочитай файл `rust-toolchain.toml` из текущего репозитория.

Определи указанную в нём версию Rust toolchain.

Заверши работу вызовом инструмента `noop`. В сообщении укажи найденную версию Rust и факт успешного чтения файла.

Не изменяй файлы, не создавай ветки, коммиты, PR или Issues.

Не утверждай, что фактический уровень рассуждений проверен, если он не подтверждён runtime-диагностикой.
