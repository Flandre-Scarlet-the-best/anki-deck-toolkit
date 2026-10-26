//! Монитор окон: жизненный цикл, классификация, свидетельства и политика шага.
//!
//! Каждое новое или изменившееся окно сразу фиксируется скриншотом реального
//! дисплея и машиночитаемой записью. Действие выполняется только если окно
//! совпало с правилом каталога И это правило ожидается текущим шагом. Всё
//! остальное — остановка сценария с причиной; неизвестное окно не закрывается.
//!
//! Отказ опроса AT-SPI или X11 не обновляет снимок окон и не выдаёт прежний
//! снимок за свежий: краткий сбой — неопределённость, устойчивый — `ERROR`.
//! Скриншоты событий и действий — обязательные свидетельства: их потеря не даёт
//! сценарию `PASS`.

use super::Fail;
use super::atspi::{Atspi, Rect, TopLevel};
use super::catalog::{self, Class, Rule};
use super::display::{Display, X11Window};
use crate::util;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Непрерывный отказ наблюдения дольше этого срока — потеря наблюдаемости.
pub const OBSERVATION_GRACE: Duration = Duration::from_secs(10);

/// Состояние свежести снимка окон между опросами.
#[derive(Debug, Default)]
struct Freshness {
    /// Был ли успешный опрос в текущем сценарии.
    observed: bool,
    failing_since: Option<Instant>,
    failures: u32,
}

enum Failure {
    /// Первый отказ после успешного опроса: начало неопределённости.
    Started,
    Continuing,
    /// Отказ длится дольше [`OBSERVATION_GRACE`].
    Lost(Duration),
}

impl Freshness {
    fn fresh(&self) -> bool {
        self.observed && self.failing_since.is_none()
    }

    fn failed(&mut self, now: Instant) -> Failure {
        self.failures += 1;
        let Some(since) = self.failing_since else {
            self.failing_since = Some(now);
            return Failure::Started;
        };
        let elapsed = now.saturating_duration_since(since);
        if elapsed >= OBSERVATION_GRACE {
            Failure::Lost(elapsed)
        } else {
            Failure::Continuing
        }
    }

    /// Возвращает число отказов, если до успеха наблюдение было деградировано.
    fn succeeded(&mut self) -> Option<u32> {
        self.observed = true;
        self.failing_since.take()?;
        Some(std::mem::take(&mut self.failures))
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Act {
    Click(&'static str),
    Leave,
}

#[derive(Debug, Clone, Copy)]
pub struct Expect {
    pub rule: &'static str,
    pub act: Act,
    pub max: usize,
}

/// Ожидания текущего шага: какие окна допустимы и что с ними делать.
#[derive(Debug, Clone, Default)]
pub struct Policy {
    pub expect: Vec<Expect>,
}

impl Policy {
    pub fn none() -> Policy {
        Policy::default()
    }
    pub fn click(mut self, rule: &'static str, button: &'static str, max: usize) -> Policy {
        self.expect.push(Expect {
            rule,
            act: Act::Click(button),
            max,
        });
        self
    }
    pub fn leave(mut self, rule: &'static str, max: usize) -> Policy {
        self.expect.push(Expect {
            rule,
            act: Act::Leave,
            max,
        });
        self
    }
}

#[derive(Debug, Clone)]
pub struct Button {
    pub name: String,
    pub enabled: bool,
    pub focused: bool,
    pub rect: Option<Rect>,
}

#[derive(Debug, Clone)]
pub struct Obs {
    pub key: String,
    pub role: String,
    pub title: String,
    pub text: String,
    pub buttons: Vec<Button>,
    pub selected: Vec<String>,
    pub rect: Option<Rect>,
    pub states: Vec<&'static str>,
    pub x11: Option<X11Window>,
    pub main_window: bool,
}

impl Obs {
    fn from_top(top: &TopLevel, x11: &[X11Window]) -> Obs {
        let node = &top.node;
        let mut all = Vec::new();
        node.walk(&mut all);
        let mut texts = Vec::new();
        let mut buttons = Vec::new();
        let mut selected = Vec::new();
        for n in all.iter().skip(1) {
            match n.role.as_str() {
                "push button" | "toggle button" => {
                    if n.has("showing") {
                        buttons.push(Button {
                            name: n.name.clone(),
                            enabled: n.has("enabled") && n.has("sensitive"),
                            focused: n.has("focused"),
                            rect: n.rect,
                        });
                    }
                }
                "list item" | "table cell" | "tree item" => {
                    if n.has("selected") || n.has("focused") {
                        selected.push(n.name.clone());
                    }
                }
                _ => {
                    for t in [Some(&n.name), n.text.as_ref()].into_iter().flatten() {
                        let t = visible_text(t);
                        if !t.trim().is_empty() && n.has("showing") && !texts.contains(&t) {
                            texts.push(t);
                        }
                    }
                }
            }
        }
        let x11 = node.rect.and_then(|r| {
            x11.iter()
                .find(|w| {
                    w.x == r.x && w.y == r.y && w.width as i32 == r.w && w.height as i32 == r.h
                })
                .or_else(|| {
                    x11.iter()
                        .find(|w| !node.name.is_empty() && w.title == node.name)
                })
                .cloned()
        });
        Obs {
            key: top.key.clone(),
            role: node.role.clone(),
            title: visible_text(&node.name),
            text: texts.join("\n"),
            buttons,
            selected,
            rect: node.rect,
            states: node.states.clone(),
            x11,
            main_window: node.role == "frame" && node.name.ends_with(" - Anki"),
        }
    }

    pub fn button_names(&self) -> Vec<String> {
        self.buttons.iter().map(|b| b.name.clone()).collect()
    }

    pub fn modal(&self) -> bool {
        self.states.contains(&"modal") || self.x11.as_ref().is_some_and(|x| x.modal)
    }

    fn signature(&self) -> String {
        format!(
            "{}|{}|{}|{:?}|{:?}",
            self.role,
            self.title,
            self.text,
            self.button_names(),
            self.selected
        )
    }

    fn json(&self) -> Value {
        json!({
            "key": self.key, "role": self.role, "title": self.title, "text": self.text,
            "buttons": self.buttons.iter().map(|b| json!({"name":b.name,"enabled":b.enabled,"focused":b.focused})).collect::<Vec<_>>(),
            "selected_items": self.selected, "atspi_states": self.states, "rect": self.rect,
            "modal": self.modal(), "x11": self.x11,
        })
    }
}

/// Видимый текст без невидимых символов изоляции направления (U+2066–U+2069),
/// которыми Anki обрамляет подставляемые значения переводов.
pub fn visible_text(text: &str) -> String {
    text.chars()
        .filter(|c| !matches!(c, '\u{2066}'..='\u{2069}'))
        .collect()
}

struct Tracked {
    signature: String,
    first_seen: Instant,
    event: usize,
    appeared: usize,
    acted: bool,
}

impl Tracked {
    fn observed(previous: Option<&Self>, signature: String, event: usize, now: Instant) -> Self {
        Self {
            signature,
            first_seen: previous.map_or(now, |t| t.first_seen),
            event,
            appeared: previous.map_or(event, |t| t.appeared),
            acted: previous.is_some_and(|t| t.acted),
        }
    }

    fn record_closed(&self, events: &mut [Value], visible_ms: u128, closed_at_ms: u128) {
        for index in [self.appeared, self.event] {
            events[index]["visible_ms"] = json!(visible_ms);
            events[index]["closed_at_ms"] = json!(closed_at_ms);
        }
    }
}

pub struct Monitor {
    atspi: Atspi,
    pub display: Display,
    tracked: BTreeMap<String, Tracked>,
    pub events: Vec<Value>,
    current: Vec<Obs>,
    out_dir: PathBuf,
    shot_seq: u32,
    scenario: String,
    started: Instant,
    pub step: String,
    step_counts: BTreeMap<&'static str, usize>,
    pub last_error: Option<String>,
    freshness: Freshness,
    /// Потерянные обязательные свидетельства сценария.
    pub evidence_errors: Vec<String>,
}

impl Monitor {
    pub fn new(atspi: Atspi, display: Display) -> Monitor {
        Monitor {
            atspi,
            display,
            tracked: BTreeMap::new(),
            events: Vec::new(),
            current: Vec::new(),
            out_dir: PathBuf::from("/tmp"),
            shot_seq: 0,
            scenario: String::new(),
            started: Instant::now(),
            step: String::new(),
            step_counts: BTreeMap::new(),
            last_error: None,
            freshness: Freshness::default(),
            evidence_errors: Vec::new(),
        }
    }

    /// Начало сценария: собственный каталог свидетельств и пустой журнал.
    pub fn begin(&mut self, scenario: &str, out_dir: PathBuf) {
        self.scenario = scenario.into();
        self.out_dir = out_dir;
        self.events.clear();
        self.tracked.clear();
        self.current.clear();
        self.shot_seq = 0;
        self.started = Instant::now();
        self.freshness = Freshness::default();
        self.evidence_errors.clear();
        self.last_error = None;
    }

    /// Последний снимок окон получен успешным опросом и с тех пор опрос не отказывал.
    pub fn fresh(&self) -> bool {
        self.freshness.fresh()
    }

    pub fn set_step(&mut self, step: &str) {
        self.step = step.into();
        self.step_counts.clear();
    }

    /// Обязательный скриншот: неудача записывается в потерянные свидетельства.
    pub fn screenshot(&mut self, label: &str) -> Option<String> {
        self.capture(label).ok()
    }

    fn capture(&mut self, label: &str) -> Result<String, String> {
        self.shot_seq += 1;
        let safe: String = label
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .take(60)
            .collect();
        let rel = format!("screenshots/{:03}-{safe}.png", self.shot_seq);
        self.display
            .screenshot(&self.out_dir.join(&rel))
            .map(|()| rel.clone())
            .map_err(|e| {
                let message = format!("скриншот {rel} (шаг «{}»): {e}", self.step);
                self.evidence_errors.push(message.clone());
                message
            })
    }

    fn t_ms(&self) -> u128 {
        self.started.elapsed().as_millis()
    }

    pub fn dialogs(&self) -> Vec<&Obs> {
        self.current.iter().filter(|o| !o.main_window).collect()
    }

    pub fn main_window(&self) -> Option<&Obs> {
        self.current.iter().find(|o| o.main_window)
    }

    /// Сколько раз правило встречалось в сценарии (по событиям появления).
    pub fn seen(&self, rule: &str) -> usize {
        self.events
            .iter()
            .filter(|e| e["event"] == "appeared" && e["rule"] == rule)
            .count()
    }

    pub fn seen_in_step(&self, rule: &str, step: &str) -> usize {
        self.events
            .iter()
            .filter(|e| e["event"] == "appeared" && e["rule"] == rule && e["step"] == step)
            .count()
    }

    /// Опрашивать до свежего снимка: хотя бы один опрос, затем до успеха или
    /// потери наблюдаемости. Прежний снимок не принимается за итоговый.
    pub fn settle(&mut self, policy: &Policy) -> Result<(), Fail> {
        loop {
            self.poll(policy)?;
            if self.fresh() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(150));
        }
    }

    /// Отказ опроса: снимок не обновляется (неполный опрос не доказывает
    /// закрытия окон), а устойчивый отказ становится ошибкой сценария.
    fn observation_failed(&mut self, error: String) -> Result<(), Fail> {
        let now = Instant::now();
        let verdict = self.freshness.failed(now);
        if matches!(verdict, Failure::Started) {
            self.events
                .push(json!({"event":"observation_degraded","step":self.step,
                "t_ms":self.t_ms(),"error":error}));
        }
        self.last_error = Some(error.clone());
        match verdict {
            Failure::Lost(elapsed) => Err(Fail::error(
                "ui_observation_lost",
                format!(
                    "Шаг «{}»: окна Anki не наблюдаются {elapsed:?} ({} отказов подряд); отсутствие неожиданных окон не подтверждено: {error}",
                    self.step, self.freshness.failures
                ),
            )),
            Failure::Started | Failure::Continuing => Ok(()),
        }
    }

    /// Один цикл наблюдения с применением политики текущего шага.
    pub fn poll(&mut self, policy: &Policy) -> Result<(), Fail> {
        let tops = match self.atspi.top_levels("Anki") {
            Ok(t) => t,
            Err(e) => return self.observation_failed(format!("AT-SPI: {e}")),
        };
        // Без свойств X11 нельзя ни отличить веб-панели от окон, ни определить модальность.
        let x11 = match self.display.windows() {
            Ok(windows) => windows,
            Err(e) => return self.observation_failed(format!("X11: {e}")),
        };
        if let Some(failures) = self.freshness.succeeded() {
            self.events
                .push(json!({"event":"observation_restored","step":self.step,
                "t_ms":self.t_ms(),"failed_polls":failures}));
        }
        // QtWebEngine публикует веб-панели главного окна («top toolbar», «main webview»)
        // отдельными объектами верхнего уровня; без собственного окна X11 это не окно.
        let observed: Vec<Obs> = tops
            .iter()
            .map(|t| Obs::from_top(t, &x11))
            .filter(|o| !(o.x11.is_none() && matches!(o.role.as_str(), "filler" | "panel")))
            .collect();
        let now = Instant::now();
        let present: Vec<String> = observed.iter().map(|o| o.key.clone()).collect();
        let gone: Vec<String> = self
            .tracked
            .keys()
            .filter(|k| !present.contains(k))
            .cloned()
            .collect();
        for key in gone {
            if let Some(t) = self.tracked.remove(&key) {
                let visible = now.duration_since(t.first_seen).as_millis();
                let closed_at = self.t_ms();
                t.record_closed(&mut self.events, visible, closed_at);
                let title = self.events[t.event]["title"].clone();
                self.events.push(json!({"event":"closed","step":self.step,"t_ms":self.t_ms(),"key":key,"title":title,"visible_ms":visible}));
            }
        }
        self.current = observed.clone();
        let mut failure = None;
        for obs in observed {
            let signature = obs.signature();
            let changed = match self.tracked.get(&obs.key) {
                None => true,
                Some(t) => t.signature != signature,
            };
            if !changed {
                continue;
            }
            let rule = if obs.main_window {
                None
            } else {
                catalog::find(&obs.role, &obs.title, &obs.text, &obs.button_names())
            };
            let label = format!(
                "{}-{}",
                if self.tracked.contains_key(&obs.key) {
                    "changed"
                } else {
                    "appeared"
                },
                rule.map_or(obs.role.as_str(), |r| r.id)
            );
            let shot = self.capture(&label);
            let is_new = !self.tracked.contains_key(&obs.key);
            let decision = self.decide(&obs, rule, policy, is_new);
            let mut record = obs.json();
            let fields = record.as_object_mut().expect("object");
            fields.insert(
                "event".into(),
                json!(if is_new { "appeared" } else { "changed" }),
            );
            fields.insert("scenario".into(), json!(self.scenario));
            fields.insert("step".into(), json!(self.step));
            fields.insert("t_ms".into(), json!(self.t_ms()));
            fields.insert("at".into(), json!(util::utc_rfc3339(util::unix_now())));
            fields.insert("screenshot".into(), json!(shot.as_ref().ok()));
            if let Err(e) = &shot {
                fields.insert("screenshot_error".into(), json!(e));
            }
            fields.insert("rule".into(), json!(rule.map(|r| r.id)));
            fields.insert("origin".into(), json!(rule.map(|r| r.origin)));
            fields.insert("classification".into(), json!(decision.class));
            fields.insert(
                "needs_user_action".into(),
                json!(rule.map_or(obs.modal(), |r| r.needs_user_action)),
            );
            fields.insert("test_action".into(), json!("none"));
            fields.insert("visible_ms".into(), Value::Null);
            fields.insert("verdict".into(), json!(decision.verdict));
            self.events.push(record);
            let index = self.events.len() - 1;
            let tracked = Tracked::observed(self.tracked.get(&obs.key), signature, index, now);
            let acted = tracked.acted;
            self.tracked.insert(obs.key.clone(), tracked);
            if let Some(f) = decision.failure {
                failure.get_or_insert(f);
                continue;
            }
            if let (Some(Act::Click(button)), false) = (decision.act, acted) {
                if let Err(f) = self.click(&obs, button, index) {
                    failure.get_or_insert(f);
                }
            } else if decision.act.is_some() && acted {
                self.events[index]["test_action"] = json!("already_acted");
            }
        }
        failure.map_or(Ok(()), Err)
    }

    fn decide(
        &mut self,
        obs: &Obs,
        rule: Option<&'static Rule>,
        policy: &Policy,
        is_new: bool,
    ) -> Decision {
        if obs.main_window {
            return Decision {
                class: json!("main_window"),
                verdict: "main_window",
                act: None,
                failure: None,
            };
        }
        let describe = || {
            format!(
                "role={} title={:?} text={:?} buttons={:?} modal={}",
                obs.role,
                obs.title,
                obs.text.chars().take(400).collect::<String>(),
                obs.button_names(),
                obs.modal()
            )
        };
        let Some(rule) = rule else {
            let blocking =
                obs.modal() || matches!(obs.role.as_str(), "dialog" | "alert" | "file chooser");
            let class = if blocking {
                Class::UnknownBlocking
            } else {
                Class::UnexpectedNotice
            };
            return Decision {
                class: json!(class),
                verdict: "unknown_window",
                act: None,
                failure: Some(Fail::failed(
                    if blocking {
                        "unknown_blocking_dialog"
                    } else {
                        "unknown_notice"
                    },
                    format!(
                        "Шаг «{}»: неизвестное окно не нажимается и не скрывается — {}",
                        self.step,
                        describe()
                    ),
                )),
            };
        };
        let Some(expect) = policy.expect.iter().find(|e| e.rule == rule.id) else {
            // Известное окно в чужом шаге: например, «Installed» при no-op setup вводит в заблуждение.
            let class = match rule.class {
                Class::ExpectedInformational => Class::DuplicateOrMisleading,
                c => c,
            };
            return Decision {
                class: json!(class),
                verdict: "not_expected_in_step",
                act: None,
                failure: Some(Fail::failed(
                    "dialog_not_expected_in_step",
                    format!(
                        "Шаг «{}» не ожидает окна {}: {}",
                        self.step,
                        rule.id,
                        describe()
                    ),
                )),
            };
        };
        let count = self.step_counts.entry(rule.id).or_insert(0);
        // Повтором считается новое окно, а не изменение уже показанного.
        if is_new {
            *count += 1;
        }
        if *count > expect.max {
            return Decision {
                class: json!(Class::DuplicateOrMisleading),
                verdict: "repeated_beyond_expectation",
                act: None,
                failure: Some(Fail::failed(
                    "duplicate_notification",
                    format!(
                        "Шаг «{}»: окно {} показано {} раз(а), ожидалось не более {}",
                        self.step, rule.id, count, expect.max
                    ),
                )),
            };
        }
        Decision {
            class: json!(rule.class),
            verdict: "expected",
            act: Some(expect.act),
            failure: None,
        }
    }

    fn click(&mut self, obs: &Obs, name: &str, index: usize) -> Result<(), Fail> {
        let Some(button) = obs.buttons.iter().find(|b| b.name == name) else {
            return Err(Fail::failed(
                "expected_button_missing",
                format!("Нет кнопки {name:?} в {}", obs.title),
            ));
        };
        let (Some(rect), true) = (button.rect, button.enabled) else {
            return Err(Fail::failed(
                "button_not_clickable",
                format!("Кнопка {name:?} недоступна"),
            ));
        };
        let shot = self.screenshot(&format!("before-click-{name}"));
        let (x, y) = rect.center();
        self.display
            .click(x, y)
            .map_err(|e| Fail::error("gui_input_failed", e))?;
        if let Some(t) = self.tracked.get_mut(&obs.key) {
            t.acted = true;
        }
        self.events[index]["test_action"] = json!(format!("click:{name}"));
        self.events.push(json!({"event":"action","step":self.step,"t_ms":self.t_ms(),"key":obs.key,"title":obs.title,
            "action":format!("click:{name}"),"at":[x,y],"screenshot":shot}));
        Ok(())
    }

    /// Окна, оставшиеся к концу сценария, кроме главного окна Anki.
    pub fn leftover(&self) -> Vec<Value> {
        self.current
            .iter()
            .filter(|o| !o.main_window)
            .map(Obs::json)
            .collect()
    }
}

struct Decision {
    class: Value,
    verdict: &'static str,
    act: Option<Act>,
    failure: Option<Fail>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_window_keeps_duration_on_the_appearance_used_by_reports() {
        let start = Instant::now();
        for changes in 0..=2 {
            let mut events = vec![json!({"event":"appeared","visible_ms":null})];
            let mut tracked = Tracked::observed(None, "initial".into(), 0, start);
            tracked.acted = true;
            for index in 1..=changes {
                events.push(json!({"event":"changed","visible_ms":null}));
                tracked = Tracked::observed(
                    Some(&tracked),
                    format!("changed-{index}"),
                    index,
                    start + Duration::from_millis(100 * index as u64),
                );
            }
            let visible = (start + Duration::from_millis(500))
                .duration_since(tracked.first_seen)
                .as_millis();
            tracked.record_closed(&mut events, visible, 700);
            let appeared = events.iter().find(|e| e["event"] == "appeared").unwrap();
            assert_eq!(appeared["visible_ms"], 500);
            assert_eq!(appeared["closed_at_ms"], 700);
            assert_eq!(events[tracked.event]["visible_ms"], 500);
            assert_eq!(tracked.event, changes);
            assert!(tracked.acted);
        }
    }

    #[test]
    fn brief_observation_failure_is_uncertainty_not_closure_or_fresh_state() {
        let start = Instant::now();
        let mut freshness = Freshness::default();
        assert!(!freshness.fresh(), "до первого опроса снимка нет");
        assert_eq!(freshness.succeeded(), None);
        assert!(freshness.fresh());

        assert!(matches!(freshness.failed(start), Failure::Started));
        assert!(!freshness.fresh(), "прежний снимок не выдаётся за свежий");
        assert!(matches!(
            freshness.failed(start + Duration::from_secs(3)),
            Failure::Continuing
        ));
        assert_eq!(freshness.succeeded(), Some(2));
        assert!(freshness.fresh());
    }

    #[test]
    fn sustained_observation_failure_is_lost_observability() {
        let start = Instant::now();
        let mut freshness = Freshness::default();
        freshness.succeeded();
        freshness.failed(start);
        assert!(matches!(
            freshness.failed(start + OBSERVATION_GRACE - Duration::from_millis(1)),
            Failure::Continuing
        ));
        let Failure::Lost(elapsed) = freshness.failed(start + OBSERVATION_GRACE) else {
            panic!("устойчивый отказ должен стать потерей наблюдаемости");
        };
        assert_eq!(elapsed, OBSERVATION_GRACE);
        assert!(!freshness.fresh());
    }

    #[test]
    fn failure_before_first_observation_never_counts_as_fresh() {
        let start = Instant::now();
        let mut freshness = Freshness::default();
        freshness.failed(start);
        assert!(!freshness.fresh());
        assert!(matches!(
            freshness.failed(start + OBSERVATION_GRACE),
            Failure::Lost(_)
        ));
    }
}
