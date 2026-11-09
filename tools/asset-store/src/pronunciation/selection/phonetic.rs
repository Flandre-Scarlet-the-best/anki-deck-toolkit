//! Объяснимое сравнение целевого чтения с гипотезой распознавателя.
//!
//! Модуль не решает, «верна» ли запись: он только называет, **чем именно**
//! услышанная кана отличается от целевой, — по морам и с классом каждого
//! расхождения. Универсального правила «одна ошибка — совпадение» нет:
//! различие `し`/`ち` (`はし`/`はち`) или `え`/`へ` вне частицы — другая мора,
//! а не опечатка распознавателя. Решение о допустимости принимает политика
//! выбора ([`super::engine`]) вместе с остальными свидетельствами.
//!
//! Частицы `は`/`へ`, произносимые `わ`/`え`, распознаются **только** в море,
//! явно отмеченной вызывающей стороной как грамматическая частица
//! (`particle_morae`); глобальной замены нет.

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::pronunciation::kana;

/// Класс одного расхождения услышанного с целевым чтением.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DivergenceClass {
    /// Долгота гласной: лишняя или утраченная гласная (`ー`) после моры с той
    /// же гласной, либо орфографическая долгота `おう`/`おお`, `えい`/`ええ`.
    VowelLength,
    /// Удвоение согласного `っ`.
    Sokuon,
    /// Моросодержащее `ん`.
    MoraicNasal,
    /// Звонкость/оглушение согласного той же моры (`か`/`が`, `は`/`ぱ`).
    Voicing,
    /// Слияние или разделение `ゃゅょ` (`きょ`/`きよ`).
    Youon,
    /// `は`/`わ` или `へ`/`え` в море, явно отмеченной как частица.
    ParticleReading,
    /// Другая мора (`し`/`ち`, `え`/`へ` вне частицы).
    DistinctMora,
    /// Лишняя мора иного вида.
    Insertion,
    /// Утраченная мора иного вида.
    Deletion,
}

impl DivergenceClass {
    /// Класс, который **может** быть ошибкой распознавателя, а не диктора,
    /// если остальные свидетельства (точная гипотеза основной модели, отрыв
    /// от конкурентов) это поддерживают. Это не прощение: решение остаётся
    /// за политикой и помечается как терпимое расхождение, а не совпадение.
    pub fn may_be_recognizer_slip(self) -> bool {
        matches!(
            self,
            Self::VowelLength
                | Self::Sokuon
                | Self::MoraicNasal
                | Self::Voicing
                | Self::Youon
                | Self::ParticleReading
        )
    }
}

/// Одно расхождение: позиция — индекс моры целевого чтения.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Divergence {
    pub class: DivergenceClass,
    pub position: usize,
    pub expected: String,
    pub heard: String,
}

/// Итог сравнения целевого чтения с услышанной каной.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Comparison {
    Exact,
    Diverges {
        divergences: Vec<Divergence>,
    },
    /// Гипотеза не разбирается в каны (пусто, латиница, служебные токены).
    Unparseable,
}

impl Comparison {
    pub fn divergences(&self) -> &[Divergence] {
        match self {
            Self::Diverges { divergences } => divergences,
            _ => &[],
        }
    }

    /// Ровно одно расхождение класса, допускающего ошибку распознавателя.
    pub fn single_possible_slip(&self) -> Option<&Divergence> {
        match self.divergences() {
            [only] if only.class.may_be_recognizer_slip() => Some(only),
            _ => None,
        }
    }
}

/// Только кана и `ー`; прочее (пробелы, `|`, латиница) отбрасывается.
pub fn hiragana_only(text: &str) -> String {
    kana::to_hiragana(text)
        .chars()
        .filter(|c| kana::is_hiragana(*c) || *c == 'ー' || *c == 'ゔ')
        .collect()
}

/// Сравнивает целевое чтение и услышанную кану по морам.
pub fn compare(target_reading: &str, heard: &str, particle_morae: &[usize]) -> Comparison {
    let target_text = hiragana_only(target_reading);
    let heard_text = hiragana_only(heard);
    if target_text.is_empty() || heard_text.is_empty() {
        return Comparison::Unparseable;
    }
    let (Ok(target), Ok(heard)) = (kana::morae(&target_text), kana::morae(&heard_text)) else {
        return Comparison::Unparseable;
    };
    if target == heard {
        return Comparison::Exact;
    }
    let ops = align(&target, &heard);
    let mut divergences = Vec::new();
    let mut index = 0;
    while index < ops.len() {
        // `きょ` ↔ `き` + `よ` (и наоборот) — одно расхождение `ゃゅょ`.
        if let Some((divergence, used)) = youon_pair(&ops, index, &target, &heard) {
            divergences.push(divergence);
            index += used;
            continue;
        }
        match ops[index] {
            Op::Match(..) => {}
            Op::Sub(t, h) => divergences.push(Divergence {
                class: substitution_class(&target, t, &heard[h], particle_morae),
                position: t,
                expected: target[t].clone(),
                heard: heard[h].clone(),
            }),
            Op::Ins(t, h) => divergences.push(Divergence {
                class: length_class(&heard[h], t.checked_sub(1).map(|p| target[p].as_str()))
                    .unwrap_or(DivergenceClass::Insertion),
                position: t,
                expected: String::new(),
                heard: heard[h].clone(),
            }),
            Op::Del(t) => divergences.push(Divergence {
                class: length_class(&target[t], t.checked_sub(1).map(|p| target[p].as_str()))
                    .unwrap_or(DivergenceClass::Deletion),
                position: t,
                expected: target[t].clone(),
                heard: String::new(),
            }),
        }
        index += 1;
    }
    Comparison::Diverges { divergences }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Match(usize, usize),
    Sub(usize, usize),
    /// Лишняя мора услышанного перед морой цели с этим индексом.
    Ins(usize, usize),
    Del(usize),
}

/// Выравнивание Левенштейна по морам с детерминированным обратным ходом.
fn align(target: &[String], heard: &[String]) -> Vec<Op> {
    let (n, m) = (target.len(), heard.len());
    let mut cost = vec![vec![0usize; m + 1]; n + 1];
    for (i, row) in cost.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in cost[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            let substitution = cost[i - 1][j - 1] + usize::from(target[i - 1] != heard[j - 1]);
            cost[i][j] = substitution.min(cost[i - 1][j] + 1).min(cost[i][j - 1] + 1);
        }
    }
    let mut ops = Vec::new();
    let (mut i, mut j) = (n, m);
    while i > 0 || j > 0 {
        if i > 0
            && j > 0
            && cost[i][j] == cost[i - 1][j - 1] + usize::from(target[i - 1] != heard[j - 1])
        {
            ops.push(if target[i - 1] == heard[j - 1] {
                Op::Match(i - 1, j - 1)
            } else {
                Op::Sub(i - 1, j - 1)
            });
            i -= 1;
            j -= 1;
        } else if i > 0 && cost[i][j] == cost[i - 1][j] + 1 {
            ops.push(Op::Del(i - 1));
            i -= 1;
        } else {
            ops.push(Op::Ins(i, j - 1));
            j -= 1;
        }
    }
    ops.reverse();
    ops
}

/// Кана без дакутэна/хандакутэна: `が` → `か`, `ぱ` → `は`, `ぎょ` → `きょ`.
fn without_voicing_marks(mora: &str) -> String {
    mora.nfd()
        .filter(|c| !matches!(*c, '\u{3099}' | '\u{309A}'))
        .nfc()
        .collect()
}

/// Гласная моры по её фонемной реализации; `ー` и `っ`/`ん` гласной не имеют.
fn vowel_of(mora: &str) -> Option<String> {
    let variants = kana::phoneme_variants(mora).ok()?;
    let last = variants.first()?.last()?.clone();
    kana::is_vowel(&last).then_some(last)
}

fn substitution_class(
    target: &[String],
    position: usize,
    heard: &str,
    particle_morae: &[usize],
) -> DivergenceClass {
    let expected = target[position].as_str();
    if particle_morae.contains(&position)
        && matches!((expected, heard), ("は", "わ") | ("へ", "え"))
    {
        return DivergenceClass::ParticleReading;
    }
    if without_voicing_marks(expected) == without_voicing_marks(heard) {
        return DivergenceClass::Voicing;
    }
    // Орфографическая долгота: `こう` услышано как `こお`, `せい` как `せえ`.
    let previous_vowel = position.checked_sub(1).and_then(|p| vowel_of(&target[p]));
    if let Some(vowel) = previous_vowel.as_deref()
        && matches!(
            (vowel, expected, heard),
            ("o", "う", "お") | ("o", "お", "う") | ("e", "い", "え") | ("e", "え", "い")
        )
    {
        return DivergenceClass::VowelLength;
    }
    DivergenceClass::DistinctMora
}

/// Вставка/удаление `っ`, `ん` или продлевающей гласной.
fn length_class(mora: &str, previous: Option<&str>) -> Option<DivergenceClass> {
    match mora {
        "っ" => Some(DivergenceClass::Sokuon),
        "ん" => Some(DivergenceClass::MoraicNasal),
        "ー" => Some(DivergenceClass::VowelLength),
        _ => {
            let vowel = vowel_of(mora)?;
            // Сама мора — чистая гласная, продлевающая гласную предыдущей.
            (kana::phoneme_variants(mora).ok()?.first()?.len() == 1
                && previous.and_then(vowel_of).as_deref() == Some(vowel.as_str()))
            .then_some(DivergenceClass::VowelLength)
        }
    }
}

fn small_to_large(c: char) -> Option<char> {
    match c {
        'ゃ' => Some('や'),
        'ゅ' => Some('ゆ'),
        'ょ' => Some('よ'),
        _ => None,
    }
}

/// `きょ` (одна мора цели) против `き`+`よ` услышанного или наоборот.
fn youon_pair(
    ops: &[Op],
    index: usize,
    target: &[String],
    heard: &[String],
) -> Option<(Divergence, usize)> {
    let split = |mora: &str| -> Option<(String, String)> {
        let mut chars = mora.chars();
        let base = chars.next()?;
        let small = small_to_large(chars.next()?)?;
        chars
            .next()
            .is_none()
            .then(|| (base.to_string(), small.to_string()))
    };
    let pair = (ops.get(index)?, ops.get(index + 1));
    // Порядок пары в выравнивании произволен: `Ins`+`Sub` равен `Sub`+`Ins`.
    let pair = match pair {
        (Op::Ins(t, h), Some(Op::Sub(t2, h2))) if *t == *t2 => {
            (&Op::Sub(*t2, *h), Some(&Op::Ins(*t, *h2)))
        }
        (Op::Del(t), Some(Op::Sub(t2, h))) => (&Op::Sub(*t, *h), Some(&Op::Del(*t2))),
        other => other,
    };
    let (first, second) = pair;
    match (first, second) {
        // Цель `きょ`, услышано `き` `よ`.
        (Op::Sub(t, h), Some(Op::Ins(_, h2))) => {
            let (base, large) = split(&target[*t])?;
            (heard[*h] == base && heard[*h2] == large).then(|| {
                (
                    Divergence {
                        class: DivergenceClass::Youon,
                        position: *t,
                        expected: target[*t].clone(),
                        heard: format!("{}{}", heard[*h], heard[*h2]),
                    },
                    2,
                )
            })
        }
        // Цель `き` `よ`, услышано `きょ`.
        (Op::Sub(t, h), Some(Op::Del(t2))) => {
            let (base, large) = split(&heard[*h])?;
            (target[*t] == base && target[*t2] == large).then(|| {
                (
                    Divergence {
                        class: DivergenceClass::Youon,
                        position: *t,
                        expected: format!("{}{}", target[*t], target[*t2]),
                        heard: heard[*h].clone(),
                    },
                    2,
                )
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classes(target: &str, heard: &str) -> Vec<DivergenceClass> {
        compare(target, heard, &[])
            .divergences()
            .iter()
            .map(|d| d.class)
            .collect()
    }

    #[test]
    fn exact_and_katakana_are_equal() {
        assert_eq!(compare("なま", "なま", &[]), Comparison::Exact);
        assert_eq!(compare("ビール", "びーる", &[]), Comparison::Exact);
    }

    #[test]
    fn hashi_hachi_is_a_distinct_mora_not_a_slip() {
        let comparison = compare("はし", "はち", &[]);
        assert_eq!(classes("はし", "はち"), vec![DivergenceClass::DistinctMora]);
        assert!(comparison.single_possible_slip().is_none());
    }

    #[test]
    fn particle_reading_requires_explicit_particle_position() {
        // Вне частицы `え`/`へ` — другая мора.
        assert_eq!(classes("へ", "え"), vec![DivergenceClass::DistinctMora]);
        assert_eq!(classes("え", "へ"), vec![DivergenceClass::DistinctMora]);
        assert_eq!(classes("はな", "わな"), vec![DivergenceClass::DistinctMora]);
        // `こんにちは`: последняя мора отмечена как частица.
        let comparison = compare("こんにちは", "こんにちわ", &[4]);
        assert_eq!(
            comparison.divergences()[0].class,
            DivergenceClass::ParticleReading
        );
        // Та же замена в другой позиции не прощается.
        let comparison = compare("はな", "わな", &[1]);
        assert_eq!(
            comparison.divergences()[0].class,
            DivergenceClass::DistinctMora
        );
        // Обратное направление (`わ` вместо `は` в цели) частицей не считается.
        assert_eq!(
            compare("わ", "は", &[0]).divergences()[0].class,
            DivergenceClass::DistinctMora
        );
    }

    #[test]
    fn minimal_pair_classes_are_named() {
        assert_eq!(classes("きって", "きて"), vec![DivergenceClass::Sokuon]);
        assert_eq!(
            classes("かんい", "かい"),
            vec![DivergenceClass::MoraicNasal]
        );
        assert_eq!(
            classes("おばあさん", "おばさん"),
            vec![DivergenceClass::VowelLength]
        );
        assert_eq!(
            classes("ビール", "ビル"),
            vec![DivergenceClass::VowelLength]
        );
        assert_eq!(
            classes("がっこう", "かっこう"),
            vec![DivergenceClass::Voicing]
        );
        assert_eq!(classes("しょう", "じょう"), vec![DivergenceClass::Voicing]);
        assert_eq!(
            classes("びょういん", "びよういん"),
            vec![DivergenceClass::Youon]
        );
        assert_eq!(
            classes("びよういん", "びょういん"),
            vec![DivergenceClass::Youon]
        );
        assert_eq!(classes("こう", "こお"), vec![DivergenceClass::VowelLength]);
        assert_eq!(classes("せい", "せえ"), vec![DivergenceClass::VowelLength]);
    }

    #[test]
    fn several_divergences_are_never_a_single_slip() {
        let comparison = compare("きて", "らいて", &[]);
        assert!(comparison.divergences().len() >= 2);
        assert!(comparison.single_possible_slip().is_none());
        // Разные чтения одного кандзи — не ошибки распознавателя.
        let comparison = compare("なま", "せい", &[]);
        assert!(comparison.single_possible_slip().is_none());
    }

    #[test]
    fn unparseable_hypothesis_is_not_a_match() {
        assert_eq!(compare("なま", "", &[]), Comparison::Unparseable);
        assert_eq!(compare("なま", "abc", &[]), Comparison::Unparseable);
    }
}
