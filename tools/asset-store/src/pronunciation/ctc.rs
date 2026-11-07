//! Алгоритмы над эмиссиями CTC: свободное декодирование и поиск вхождений
//! заданной последовательности.
//!
//! Все функции детерминированы и не знают о модели: на входе матрица
//! логарифмов вероятностей `кадры × токены` и классы фонем.
//!
//! Поиск вхождений — локальное выравнивание CTC с «наполнителем»: кадры вне
//! вхождения объясняются лучшим токеном кадра (свободный лучший путь), а
//! вхождение — топологией CTC последовательности (токены с необязательными
//! пробелами между ними). Оценка вхождения — сумма по его кадрам разности
//! `log p(путь) − log p(лучший токен кадра)`: она не больше нуля и равна нулю,
//! только если последовательность и есть свободная гипотеза модели на этих
//! кадрах. Поэтому подсказка не может «натянуть» чужую речь без штрафа:
//! каждый кадр, где модель слышит другое, уменьшает оценку.

/// Эмиссии: `frames × vocab` логарифмов вероятностей по строкам.
#[derive(Debug, Clone, PartialEq)]
pub struct Emissions {
    pub frames: usize,
    pub vocab: usize,
    pub log_probs: Vec<f32>,
}

impl Emissions {
    pub fn new(frames: usize, vocab: usize, log_probs: Vec<f32>) -> Option<Self> {
        (log_probs.len() == frames * vocab && log_probs.iter().all(|v| v.is_finite())).then_some(
            Self {
                frames,
                vocab,
                log_probs,
            },
        )
    }

    pub fn row(&self, t: usize) -> &[f32] {
        &self.log_probs[t * self.vocab..(t + 1) * self.vocab]
    }

    /// Логарифм вероятности лучшего токена кадра.
    pub fn best(&self, t: usize) -> f32 {
        self.row(t)
            .iter()
            .copied()
            .fold(f32::NEG_INFINITY, f32::max)
    }
}

/// Классы сравнения поверх словаря модели: несколько токенов могут давать
/// один класс (оглушённый `I` и `i`), служебные токены — не дают никакого.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classes {
    pub names: Vec<String>,
    /// Для каждого класса — токены словаря.
    pub tokens: Vec<Vec<usize>>,
}

impl Classes {
    pub fn id(&self, name: &str) -> Option<usize> {
        self.names.iter().position(|n| n == name)
    }

    /// `log Σ p(токен)` по токенам класса в кадре.
    pub fn log_prob(&self, emissions: &Emissions, t: usize, class: usize) -> f32 {
        let row = emissions.row(t);
        let tokens = &self.tokens[class];
        let max = tokens
            .iter()
            .map(|&k| row[k])
            .fold(f32::NEG_INFINITY, f32::max);
        if tokens.len() == 1 || max == f32::NEG_INFINITY {
            return max;
        }
        let sum: f64 = tokens.iter().map(|&k| f64::from(row[k] - max).exp()).sum();
        max + sum.ln() as f32
    }
}

/// Токен свободной гипотезы с его кадрами (включительно).
#[derive(Debug, Clone, PartialEq)]
pub struct TokenSpan {
    pub token: usize,
    pub start: usize,
    pub end: usize,
    /// Средний логарифм вероятности токена на его кадрах.
    pub mean_log_prob: f32,
}

/// Жадное свободное декодирование без какой-либо подсказки: argmax кадра,
/// слияние повторов, удаление пробела. Одинаковые токены, разделённые
/// пробелом, остаются двумя токенами.
pub fn greedy(emissions: &Emissions, blank: usize) -> Vec<TokenSpan> {
    let mut out: Vec<TokenSpan> = Vec::new();
    let mut previous = blank;
    let mut sum = 0.0f64;
    for t in 0..emissions.frames {
        let row = emissions.row(t);
        let (token, value) =
            row.iter()
                .copied()
                .enumerate()
                .fold((0, f32::NEG_INFINITY), |best, (k, v)| {
                    if v > best.1 { (k, v) } else { best }
                });
        if token != blank && token == previous {
            let last = out.last_mut().expect("повтор продолжает токен");
            last.end = t;
            sum += f64::from(value);
            last.mean_log_prob = (sum / (last.end - last.start + 1) as f64) as f32;
        } else if token != blank {
            sum = f64::from(value);
            out.push(TokenSpan {
                token,
                start: t,
                end: t,
                mean_log_prob: value,
            });
        }
        previous = token;
    }
    out
}

/// Выравнивание последовательности классов на кадрах `[start, end]`.
#[derive(Debug, Clone, PartialEq)]
pub struct Alignment {
    /// Сумма `log p(путь) − log p(лучший токен)` по кадрам вхождения (≤ 0).
    pub score: f32,
    pub start: usize,
    pub end: usize,
    /// Кадры каждого элемента последовательности (включительно).
    pub spans: Vec<(usize, usize)>,
}

impl Alignment {
    pub fn score_per_token(&self) -> f32 {
        self.score / self.spans.len().max(1) as f32
    }
}

/// Лучшее локальное выравнивание последовательности `sequence` (классы) в
/// окне `[lo, hi)` с запретом кадров `forbidden`. Пробел допустим только
/// между элементами: края вхождения — кадры первого и последнего элемента.
/// `None` — последовательность пуста или не помещается в окно.
pub fn best_alignment(
    emissions: &Emissions,
    classes: &Classes,
    blank: usize,
    sequence: &[usize],
    window: (usize, usize),
    forbidden: &[bool],
) -> Option<Alignment> {
    let n = sequence.len();
    let (lo, hi) = (window.0, window.1.min(emissions.frames));
    if n == 0 || hi <= lo || hi - lo < n {
        return None;
    }
    let states = 2 * n - 1;
    let neg = f32::NEG_INFINITY;
    let len = hi - lo;
    let frame_best: Vec<f32> = (lo..hi).map(|t| emissions.best(t)).collect();
    // Относительные оценки кадра: класс состояния минус лучший токен.
    let rel = |t: usize, state: usize| -> f32 {
        if forbidden.get(t).copied().unwrap_or(false) {
            return neg;
        }
        let best = frame_best[t - lo];
        if state.is_multiple_of(2) {
            classes.log_prob(emissions, t, sequence[state / 2]) - best
        } else {
            emissions.row(t)[blank] - best
        }
    };
    let mut score = vec![neg; states];
    // Обратные ссылки: предыдущее состояние (`usize::MAX` — начало вхождения).
    let mut back = vec![usize::MAX; len * states];
    let mut best: Option<(f32, usize)> = None;
    for (i, t) in (lo..hi).enumerate() {
        let mut next = vec![neg; states];
        for s in 0..states {
            let emit = rel(t, s);
            if emit == neg {
                continue;
            }
            let mut from = (neg, usize::MAX);
            let mut consider = |value: f32, origin: usize| {
                if value > from.0 {
                    from = (value, origin);
                }
            };
            consider(score[s], s);
            if s >= 1 {
                consider(score[s - 1], s - 1);
            }
            if s % 2 == 0 && s >= 2 && sequence[s / 2] != sequence[s / 2 - 1] {
                consider(score[s - 2], s - 2);
            }
            if s == 0 {
                // Начало вхождения в этом кадре: до него — наполнитель (0).
                consider(0.0, usize::MAX);
            }
            if from.0 == neg {
                continue;
            }
            next[s] = from.0 + emit;
            back[i * states + s] = from.1;
        }
        score = next;
        let end = score[states - 1];
        // При равной оценке предпочитается более поздний конец: кадры, где
        // последний элемент ещё лучший токен, принадлежат слову.
        if end > neg && best.is_none_or(|(b, _)| end >= b) {
            best = Some((end, i));
        }
    }
    let (total, end_i) = best?;
    let mut spans = vec![(usize::MAX, 0usize); n];
    let (mut i, mut s) = (end_i, states - 1);
    loop {
        if s % 2 == 0 {
            let span = &mut spans[s / 2];
            let t = lo + i;
            span.0 = span.0.min(t);
            span.1 = span.1.max(t);
        }
        let origin = back[i * states + s];
        if origin == usize::MAX {
            break;
        }
        s = origin;
        i = i.checked_sub(1)?;
    }
    Some(Alignment {
        score: total,
        start: spans[0].0,
        end: spans[n - 1].1,
        spans,
    })
}

/// Принудительное выравнивание последовательности ровно на кадрах
/// `[start, end]` (пробел допустим и по краям): сумма
/// `log p(путь) − log p(лучший токен)`. Сравнивает цель и похожие чтения на
/// одних и тех же кадрах, чтобы более короткий конкурент не мог отдать
/// «лишние» звуки наполнителю бесплатно. `None` — не помещается.
pub fn forced_score(
    emissions: &Emissions,
    classes: &Classes,
    blank: usize,
    sequence: &[usize],
    span: (usize, usize),
) -> Option<f32> {
    let n = sequence.len();
    let (start, end) = span;
    if n == 0 || end >= emissions.frames || end < start || end - start + 1 < n {
        return None;
    }
    let states = 2 * n + 1;
    let neg = f32::NEG_INFINITY;
    let emit = |t: usize, s: usize| -> f32 {
        let best = emissions.best(t);
        if s % 2 == 1 {
            classes.log_prob(emissions, t, sequence[s / 2]) - best
        } else {
            emissions.row(t)[blank] - best
        }
    };
    let mut score = vec![neg; states];
    score[0] = emit(start, 0);
    score[1] = emit(start, 1);
    for t in start + 1..=end {
        let mut next = vec![neg; states];
        for s in 0..states {
            let mut from = score[s];
            if s >= 1 {
                from = from.max(score[s - 1]);
            }
            if s % 2 == 1 && s >= 3 && sequence[s / 2] != sequence[s / 2 - 1] {
                from = from.max(score[s - 2]);
            }
            if from > neg {
                next[s] = from + emit(t, s);
            }
        }
        score = next;
    }
    let total = score[states - 1].max(score[states - 2]);
    (total > neg).then_some(total)
}

/// Все неперекрывающиеся вхождения: лучшее выравнивание, затем его кадры
/// (с полем `separation` по обе стороны) запрещаются и поиск повторяется,
/// пока оценка на элемент не ниже `min_score_per_token` и вхождений меньше
/// `max_occurrences`. Порядок результата — по времени.
pub fn find_occurrences(
    emissions: &Emissions,
    classes: &Classes,
    blank: usize,
    sequence: &[usize],
    min_score_per_token: f32,
    max_occurrences: usize,
    separation: usize,
) -> Vec<Alignment> {
    let mut forbidden = vec![false; emissions.frames];
    let mut found = Vec::new();
    while found.len() < max_occurrences {
        let Some(alignment) = best_alignment(
            emissions,
            classes,
            blank,
            sequence,
            (0, emissions.frames),
            &forbidden,
        ) else {
            break;
        };
        if alignment.score_per_token() < min_score_per_token {
            break;
        }
        let from = alignment.start.saturating_sub(separation);
        let to = (alignment.end + separation + 1).min(emissions.frames);
        forbidden[from..to].iter_mut().for_each(|f| *f = true);
        found.push(alignment);
    }
    found.sort_by_key(|a| a.start);
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Синтетические эмиссии: каждому кадру — «услышанный» токен с
    /// вероятностью `p`, остаток равномерно.
    fn synthetic(frames: &[usize], vocab: usize, p: f32) -> Emissions {
        let rest = ((1.0 - p) / (vocab - 1) as f32).ln();
        let mut log_probs = Vec::new();
        for &token in frames {
            for k in 0..vocab {
                log_probs.push(if k == token { p.ln() } else { rest });
            }
        }
        Emissions::new(frames.len(), vocab, log_probs).unwrap()
    }

    fn classes(vocab: usize) -> Classes {
        Classes {
            names: (0..vocab).map(|k| k.to_string()).collect(),
            tokens: (0..vocab).map(|k| vec![k]).collect(),
        }
    }

    const B: usize = 0;

    #[test]
    fn greedy_collapses_repeats_and_keeps_blank_separated_doubles() {
        let e = synthetic(&[B, 1, 1, B, 2, B, 2, 2, B], 4, 0.9);
        let tokens: Vec<(usize, usize, usize)> = greedy(&e, B)
            .iter()
            .map(|t| (t.token, t.start, t.end))
            .collect();
        assert_eq!(tokens, [(1, 1, 2), (2, 4, 4), (2, 6, 7)]);
    }

    #[test]
    fn exact_free_hypothesis_scores_zero_and_mismatch_is_penalised() {
        let e = synthetic(&[B, B, 1, 2, B, 3, B, B], 5, 0.9);
        let c = classes(5);
        let none = vec![false; e.frames];
        let exact = best_alignment(&e, &c, B, &[1, 2, 3], (0, 8), &none).unwrap();
        assert!(exact.score.abs() < 1e-5, "{}", exact.score);
        assert_eq!((exact.start, exact.end), (2, 5));
        assert_eq!(exact.spans, [(2, 2), (3, 3), (5, 5)]);
        // Подсказка с другим токеном не натягивается бесплатно.
        let wrong = best_alignment(&e, &c, B, &[1, 4, 3], (0, 8), &none).unwrap();
        assert!(wrong.score < -3.0, "{}", wrong.score);
        // Лишний элемент тоже требует кадров, которых модель не слышала.
        let longer = best_alignment(&e, &c, B, &[1, 2, 3, 3], (0, 8), &none).unwrap();
        assert!(longer.score < -3.0);
    }

    #[test]
    fn identical_neighbours_need_a_blank_between_them() {
        // «っ»-подобный случай: «1 1» без пробела — один токен.
        let merged = synthetic(&[B, 1, 1, 1, B], 3, 0.95);
        let split = synthetic(&[B, 1, B, 1, B], 3, 0.95);
        let c = classes(3);
        let none = vec![false; 5];
        let on_merged = best_alignment(&merged, &c, B, &[1, 1], (0, 5), &none).unwrap();
        let on_split = best_alignment(&split, &c, B, &[1, 1], (0, 5), &none).unwrap();
        assert!(on_split.score.abs() < 1e-5);
        assert!(on_merged.score < -2.0);
    }

    #[test]
    fn repeated_word_occurrences_are_all_found_in_time_order() {
        // 1 2 | 3 | 1 2 : целевая «1 2» встречается дважды, между ними «3».
        let e = synthetic(&[B, 1, 2, B, B, 3, B, B, 1, 1, 2, B], 4, 0.9);
        let found = find_occurrences(&e, &classes(4), B, &[1, 2], -1.0, 8, 1);
        let spans: Vec<(usize, usize)> = found.iter().map(|a| (a.start, a.end)).collect();
        assert_eq!(spans, [(1, 2), (8, 10)]);
        assert!(find_occurrences(&e, &classes(4), B, &[3, 3], -1.0, 8, 1).is_empty());
    }

    #[test]
    fn merged_classes_accept_either_token() {
        // Класс 1 = токены {1, 2} (как i и оглушённое I).
        let e = synthetic(&[B, 2, B], 3, 0.9);
        let c = Classes {
            names: vec!["blank".into(), "i".into()],
            tokens: vec![vec![0], vec![1, 2]],
        };
        let a = best_alignment(&e, &c, B, &[1], (0, 3), &[false; 3]).unwrap();
        assert!(a.score > -0.2, "{}", a.score);
    }

    #[test]
    fn forced_score_charges_shorter_competitors_for_unexplained_frames() {
        // Слышно «1 2 2»: цель «1 2 2» объясняет все кадры, «1 2» — нет.
        let e = synthetic(&[B, 1, 1, B, 2, 2, B, 2, 2, B], 4, 0.9);
        let c = classes(4);
        let full = forced_score(&e, &c, B, &[1, 2, 2], (1, 8)).unwrap();
        let short = forced_score(&e, &c, B, &[1, 2], (1, 8)).unwrap();
        assert!(full.abs() < 1e-5, "{full}");
        assert!(short < full - 2.0, "{short}");
        assert!(forced_score(&e, &c, B, &[1, 2, 2], (1, 2)).is_none());
    }

    #[test]
    fn forbidden_frames_and_tiny_windows() {
        let e = synthetic(&[B, 1, B], 2, 0.9);
        let c = classes(2);
        // Запрещённый кадр не используется: выравнивание уходит на соседние
        // кадры пробела и получает штраф, а не «бесплатное» совпадение.
        let avoided = best_alignment(&e, &c, B, &[1], (0, 3), &[false, true, false]).unwrap();
        assert_ne!(avoided.start, 1);
        assert!(avoided.score < -2.0, "{}", avoided.score);
        assert!(best_alignment(&e, &c, B, &[1], (0, 3), &[true; 3]).is_none());
        assert!(best_alignment(&e, &c, B, &[1, 1], (1, 2), &[false; 3]).is_none());
        assert!(best_alignment(&e, &c, B, &[], (0, 3), &[false; 3]).is_none());
    }
}
