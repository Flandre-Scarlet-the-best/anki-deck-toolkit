use super::{ForvoSession, error};
use crate::{AssetError, pronunciation::Pronunciation};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Gender {
    Female,
    Male,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Candidate {
    pub record_id: String,
    pub username: Option<String>,
    pub votes: Option<i64>,
    pub gender: Option<Gender>,
    pub country: Option<String>,
    pub download_available: bool,
}

pub const RANKING: &str =
    "votes_desc_known_first;equal_votes_female_first;japan_first;record_id_asc_v1";

impl Candidate {
    pub fn validate(&self) -> Result<(), AssetError> {
        if self.record_id.is_empty()
            || self.record_id.len() > 20
            || self.record_id.starts_with('0')
            || !self.record_id.bytes().all(|b| b.is_ascii_digit())
            || [&self.username, &self.country]
                .into_iter()
                .flatten()
                .any(|s| s.is_empty() || s.len() > 128 || s.chars().any(char::is_control))
        {
            return Err(error(
                "dom_changed",
                "Неподдерживаемые сведения о дикторе Forvo",
            ));
        }
        Ok(())
    }
}

pub fn rank(candidates: &mut [Candidate]) {
    candidates.sort_by(|a, b| {
        b.votes
            .cmp(&a.votes)
            .then_with(|| {
                (b.gender == Some(Gender::Female)).cmp(&(a.gender == Some(Gender::Female)))
            })
            .then_with(|| {
                (b.country.as_deref() == Some("Japan"))
                    .cmp(&(a.country.as_deref() == Some("Japan")))
            })
            .then_with(|| a.record_id.len().cmp(&b.record_id.len()))
            .then_with(|| a.record_id.cmp(&b.record_id))
    });
}

pub fn word_url(surface: &str) -> String {
    let mut url = url::Url::parse("https://forvo.com/").expect("URL");
    url.path_segments_mut()
        .expect("base")
        .extend(["word", surface, ""]);
    url.into()
}

#[derive(Deserialize)]
pub(crate) struct DomResult {
    pub code: String,
    pub candidates: Vec<Candidate>,
    #[serde(default)]
    pub point: Option<DomPoint>,
}
#[derive(Deserialize)]
pub(crate) struct DomPoint {
    pub x: f64,
    pub y: f64,
}

impl ForvoSession {
    pub(crate) async fn dom(
        &self,
        surface: &str,
        action: &str,
        id: &str,
    ) -> Result<DomResult, AssetError> {
        let expression = format!(
            "{}({},{},{})",
            include_str!("dom.js"),
            serde_json::json!(surface),
            serde_json::json!(action),
            serde_json::json!(id)
        );
        self.browser()
            .page()
            .evaluate_expression(
                chromiumoxide::cdp::js_protocol::runtime::EvaluateParams::builder()
                    .expression(expression)
                    .user_gesture(action == "click")
                    .build()
                    .expect("expression"),
            )
            .await
            .map_err(|_| {
                error(
                    "dom_changed",
                    "Не удалось прочитать/нажать строку Forvo; повторите проверку страницы",
                )
            })?
            .into_value()
            .map_err(|_| error("dom_changed", "Неизвестная структура кандидатов Forvo"))
    }
    pub async fn candidates(
        &mut self,
        request: &Pronunciation,
    ) -> Result<Vec<Candidate>, AssetError> {
        self.navigate(&word_url(&request.surface)).await?;
        self.require_authenticated().await?;
        // DOM слова может появиться до выполнения отложенных frontend-скриптов.
        // Ждём DOMContentLoaded, который нужен обычной кнопке сайта, а не рекламы/network idle.
        let ready=self.browser().page().evaluate_expression(r#"new Promise(resolve=>{
            if(document.readyState==='complete'||performance.getEntriesByType('navigation')[0]?.domContentLoadedEventEnd>0){resolve(true);return;}
            const done=()=>{clearTimeout(timer);document.removeEventListener('DOMContentLoaded',done);resolve(true);};
            const timer=setTimeout(()=>{document.removeEventListener('DOMContentLoaded',done);resolve(false);},12000);
            document.addEventListener('DOMContentLoaded',done,{once:true});
        })"#).await.map_err(|_|error("browser_recovery_required","Не удалось дождаться готовности интерфейса Forvo"))?
            .into_value::<bool>().map_err(|_|error("dom_changed","Неизвестная готовность интерфейса Forvo"))?;
        if !ready {
            return Err(error(
                "page_unstable",
                "Интерфейс слова не завершил DOMContentLoaded; повторите позже",
            ));
        }
        let expected = serde_json::json!(word_url(&request.surface));
        let missing=self.browser().page().evaluate_expression(format!("location.href === {expected} && performance.getEntriesByType('navigation')[0]?.responseStatus === 404")).await
            .map_err(|_|error("browser_recovery_required","Не удалось проверить ответ страницы слова"))?
            .into_value::<bool>().map_err(|_|error("dom_changed","Неизвестное состояние страницы слова"))?;
        if missing {
            return Err(error(
                "word_not_found",
                "Forvo вернул 404 для точной страницы слова",
            ));
        }
        validated(self.dom(&request.surface, "expand", "").await?)
    }
}

/// Общая проверка и рейтинг наблюдённых строк для CDP и Brave-адаптера.
pub(crate) fn validated(result: DomResult) -> Result<Vec<Candidate>, AssetError> {
    if result.code != "ok" {
        return Err(error(
            &result.code,
            "Проверьте японский раздел точного слова на Forvo",
        ));
    }
    if result.candidates.len() > 128 {
        return Err(error(
            "too_many_candidates",
            "Forvo вернул более 128 записей слова",
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    for c in &result.candidates {
        c.validate()?;
        if !seen.insert(&c.record_id) {
            return Err(error("dom_changed", "Повторяющийся ID записи Forvo"));
        }
    }
    let mut candidates = result.candidates;
    rank(&mut candidates);
    Ok(candidates)
}
