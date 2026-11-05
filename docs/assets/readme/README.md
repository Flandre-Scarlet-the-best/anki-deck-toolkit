# Ресурсы корневых README

Каталог хранит собственные иллюстрации корневых `README.md`, `README.en.md`,
`README.zh-CN.md` и маленькую синтетическую демо-колоду для их быстрого старта.
Пользовательских колод, медиа и корпусов ресурсов здесь нет и быть не должно:
они остаются в игнорируемых Git `decks/` и `.asset-store/`.

| Файл | Что это |
|---|---|
| `demo-export/deck.json` | Синтетический CrowdAnki-экспорт: одна колода, одна модель, три заметки, без медиа. В значении `Meaning` заметки `demo01` намеренно оставлена опечатка `coincidance` — её исправляет пример `edit`. Файл в канонической форме, которую требует `edit`. |
| `demo-create.json` | Запрос `anki-repo create`, добавляющий в демо-колоду заметку `demo04`. |
| `visual-report-light.png`, `visual-report-dark.png` | Настоящие скриншоты `index.html` из `anki-repo visual-report` по этой демо-колоде в светлой и тёмной теме отчёта. |

## Как получены скриншоты

Отчёт построен штатными командами по копиям демо-колоды вне рабочей копии:

```bash
AR=./target/release/anki-repo
DEMO=$(mktemp -d)
cp -r docs/assets/readme/demo-export "$DEMO/before"
cp -r "$DEMO/before" "$DEMO/after"

$AR edit "$DEMO/after" --guid demo01 --field Meaning \
  --expect 'chance; coincidance' --set 'chance; coincidence' --apply &&
$AR create "$DEMO/after" --request docs/assets/readme/demo-create.json --apply &&
$AR retire "$DEMO/after" --guid demo03 --tag archived::demo --apply &&
$AR visual-report --before "$DEMO/before" --after "$DEMO/after" \
  --out "$DEMO/report" --retire-tag archived::demo
```

Команды связаны `&&`: если любая правка откажет, отчёт не строится, и неполный
скриншот не получится. `set -e` здесь не используется, потому что при вставке
блока в интерактивную оболочку он закрыл бы саму оболочку.

Страница `"$DEMO/report/index.html"` снята в Chromium при ширине окна 1100 CSS px
и масштабе 1,25. В кадр попадают сводка отчёта и раздел «Изменённые заметки»:
разделы «Ограничения этого отчёта», «Созданные заметки» и «Выведенные из
обращения», а также поясняющий абзац заголовка скрыты стилем только на время
съёмки. Содержимое кадра программа не меняла. Тёмный вариант снят после
нажатия штатного переключателя темы отчёта.

Интерфейс отчёта сейчас только на русском, поэтому скриншоты одинаковы для всех
трёх README. При изменении вида `visual-report` скриншоты пересоздаются тем же
способом; правка PNG вручную не допускается.
