"""Общая модель локальных фильтров Sonar для валидации отчётов и тестов.

Учитывается поднабор Java properties из sonar-project.properties. Настройки
сервера и языковые фильтры не моделируются; это не запуск SonarScanner.
"""

import re


def read_properties(path):
    """Прочитать используемый поднабор Java properties, отказывая при неоднозначности."""
    properties = {}
    for raw in path.read_text(encoding="utf-8").splitlines():
        line = raw.strip()
        if not line or line.startswith(("#", "!")):
            continue
        if line.endswith("\\") or "=" not in line:
            raise ValueError(f"Неподдерживаемая строка properties: {raw!r}")
        key, value = (part.strip() for part in line.split("=", 1))
        if key in properties:
            raise ValueError(f"Повторное свойство: {key}")
        properties[key] = value
    return properties


def sonar_match(pattern, path):
    """Шаблоны Sonar: * и ? внутри компонента, ** для нуля и более каталогов."""
    parts = pattern.split("/")
    expressions = []
    for index, part in enumerate(parts):
        if part == "**":
            expressions.append(".*" if index == len(parts) - 1 else "(?:[^/]+/)*")
        else:
            if "**" in part:
                raise ValueError(f"Неподдерживаемый составной **: {pattern!r}")
            component = "".join(
                "[^/]*" if char == "*" else "[^/]" if char == "?" else re.escape(char)
                for char in part
            )
            expressions.append(component + ("/" if index < len(parts) - 1 else ""))
    return re.fullmatch("".join(expressions), path) is not None


def patterns(properties, key):
    return [part.strip() for part in properties.get(key, "").split(",") if part.strip()]


def in_roots(path, roots):
    return any(root == "." or path == root or path.startswith(root.rstrip("/") + "/")
               for root in roots)


def matches_any(path, globs):
    return any(sonar_match(pattern, path) for pattern in globs)


def scope(properties, path):
    """Рассчитать фильтры исходников и тестов как отдельные множества."""
    inclusions = patterns(properties, "sonar.test.inclusions")
    source = (in_roots(path, patterns(properties, "sonar.sources"))
              and not matches_any(path, patterns(properties, "sonar.exclusions"))
              and not matches_any(path, inclusions))
    test = (in_roots(path, patterns(properties, "sonar.tests"))
            and (not inclusions or matches_any(path, inclusions))
            and not matches_any(path, patterns(properties, "sonar.test.exclusions")))
    return source, test
