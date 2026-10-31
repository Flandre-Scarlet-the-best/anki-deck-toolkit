//! Отображение значений в базовых единицах контракта для человека.
//!
//! Контракт хранит только базовые единицы (секунда, байт, штука, ядро);
//! перевод в миллисекунды или МиБ — дело отображения и выполняется здесь.

use benchmark_contract::vocabulary::Unit;

/// Значение с единицей, подобранной по величине, для русского вывода.
pub fn quantity(value: f64, unit: Unit) -> String {
    quantity_in(value, unit, cores_ru)
}

/// Значение с единицей; `cores` — слово «ядро» на языке вывода для этого числа.
pub fn quantity_in(value: f64, unit: Unit, cores: fn(f64) -> &'static str) -> String {
    match unit {
        Unit::Second => seconds(value),
        Unit::Byte => bytes(value),
        Unit::Count => count(value),
        Unit::Core => format!("{} {}", trim(value, 2), cores(value)),
    }
}

/// Форма слова «ядро» после числа: дробное — «ядра», целое — по последним цифрам.
pub fn cores_ru(value: f64) -> &'static str {
    let shown = (value * 100.0).round() / 100.0;
    if shown.fract() != 0.0 {
        return "ядра";
    }
    let n = shown.abs() as u64;
    match (n % 10, n % 100) {
        (1, 11) => "ядер",
        (1, _) => "ядро",
        (2..=4, 12..=14) => "ядер",
        (2..=4, _) => "ядра",
        _ => "ядер",
    }
}

/// Разность с явным знаком.
pub fn signed(value: f64, unit: Unit) -> String {
    let sign = if value > 0.0 {
        "+"
    } else if value < 0.0 {
        "−"
    } else {
        "±"
    };
    format!("{sign}{}", quantity(value.abs(), unit))
}

pub fn percent(value: f64) -> String {
    let sign = if value > 0.0 {
        "+"
    } else if value < 0.0 {
        "−"
    } else {
        "±"
    };
    // Малое изменение не округляется до нуля: знак без величины вводил бы в заблуждение.
    let magnitude = (value * 100.0).abs();
    let digits = if magnitude < 1.0 { 2 } else { 1 };
    format!("{sign}{} %", trim(magnitude, digits))
}

fn seconds(value: f64) -> String {
    let abs = value.abs();
    if abs == 0.0 {
        "0 s".into()
    } else if abs < 1e-3 {
        format!("{} µs", trim(value * 1e6, 1))
    } else if abs < 1.0 {
        format!("{} ms", trim(value * 1e3, 2))
    } else {
        format!("{} s", trim(value, 3))
    }
}

fn bytes(value: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut scaled = value;
    let mut unit = 0;
    while scaled.abs() >= 1024.0 && unit < UNITS.len() - 1 {
        scaled /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} B", trim(value, 0))
    } else {
        format!("{} {}", trim(scaled, 2), UNITS[unit])
    }
}

fn count(value: f64) -> String {
    // Среднее счётчика дробное: целая часть группируется, дробь — один знак.
    let rounded = (value.abs() * 10.0).round() / 10.0;
    let whole = rounded.trunc();
    let digits = format!("{}", whole as u64);
    let mut grouped = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            grouped.push('\u{202f}');
        }
        grouped.push(c);
    }
    let fraction = ((rounded - whole) * 10.0).round() as u64;
    if fraction > 0 {
        grouped.push_str(&format!(".{fraction}"));
    }
    if value < 0.0 {
        format!("-{grouped}")
    } else {
        grouped
    }
}

/// Число с не более чем `digits` знаками после точки, без хвостовых нулей.
pub fn trim(value: f64, digits: usize) -> String {
    let text = format!("{value:.digits$}");
    if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').to_owned()
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units_scale_by_magnitude() {
        assert_eq!(quantity(0.0123, Unit::Second), "12.3 ms");
        assert_eq!(quantity(2.5, Unit::Second), "2.5 s");
        assert_eq!(quantity(0.000_25, Unit::Second), "250 µs");
        assert_eq!(quantity(1536.0, Unit::Byte), "1.5 KiB");
        assert_eq!(quantity(512.0, Unit::Byte), "512 B");
        assert_eq!(
            quantity(1_234_567.0, Unit::Count),
            "1\u{202f}234\u{202f}567"
        );
        assert_eq!(signed(-0.001, Unit::Second), "−1 ms");
        assert_eq!(percent(-0.0981), "−9.8 %");
        assert_eq!(percent(0.0), "±0 %");
        assert_eq!(percent(0.000_16), "+0.02 %");
        assert_eq!(
            quantity(416_279_827.9, Unit::Count),
            "416\u{202f}279\u{202f}827.9"
        );
        assert_eq!(quantity(0.94, Unit::Core), "0.94 ядра");
        assert_eq!(quantity(1.0, Unit::Core), "1 ядро");
        assert_eq!(quantity(3.0, Unit::Core), "3 ядра");
        assert_eq!(quantity(12.0, Unit::Core), "12 ядер");
        assert_eq!(quantity(21.0, Unit::Core), "21 ядро");
    }
}
