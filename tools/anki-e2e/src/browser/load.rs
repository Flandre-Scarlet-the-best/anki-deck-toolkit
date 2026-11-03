//! Явная длительная нагрузка через штатный CLI и неизменённое MV3-расширение.
//! Числа времени живут в обычном отчёте browser; серию, агрегацию и историю
//! ведут benchmark-runner/benchmark-contract/benchmark-history.

use super::cli_scenarios::{Cli, MISSING_CLI, Run, run_log};
use super::fixture;
use super::observation::{self, Observer};
use super::scenarios::{Env, Result, err, files_json};
use asset_store::batch_runtime::SafeBatchRuntime;
use asset_store::forvo::RANKING;
use asset_store::forvo::batch::ForvoBatch;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

async fn resources(env: &mut Env<'_>, page: &chromiumoxide::Page) -> Result<Value> {
    env.connect().await?;
    let ping = env.call("ping", json!({})).await.map_err(err)?;
    let mut snapshot = env.call("snapshot", json!({})).await.map_err(err)?;
    let mut page_urls = env.browser.page_urls().await?;
    page_urls.sort();
    snapshot["cdp_page_urls"] = json!(page_urls);
    env.conn.take().expect("bridge").bridge.disconnect().await;
    // API расширения читается из его собственной страницы; никаких новых
    // permissions или диагностических команд в production-расширении.
    let downloads: Value = page.evaluate("(async () => (await chrome.downloads.search({})).map(d => ({id:d.id,state:d.state,exists:d.exists})))()")
        .await.map_err(|e| e.to_string())?.into_value().map_err(|e| e.to_string())?;
    Ok(
        json!({"ping":ping,"tabs":snapshot,"download_records":downloads,
        "downloads":files_json(&env.downloads()),"files":observation::files(&env.downloads())?,
        "store": if env.dir.join("store").exists() { observation::files(&env.dir.join("store"))? } else { json!({"status":"absent"}) },
        "journal":match env.dir.join("store/.journal/events.jsonl").metadata() {
            Ok(m)=>json!({"status":"measured","bytes":m.len()}),
            Err(e)=>json!({"status":"unavailable","reason":e.to_string()}),
        },
        "adapter":observation::files(&env.ctx.state_dir)?,
        "browser_processes":env.browser.pid().map(observation::browser_snapshot)}),
    )
}

fn clean(value: &Value, user_records: &Value, tabs: &Value) -> bool {
    value["ping"]["own_tab"]["exists"] == false
        && value["ping"]["operation"].is_null()
        && value["ping"]["listeners"]["active_observers"] == 0
        && value["ping"]["listeners"]["web_request"] == false
        && value["ping"]["listeners"]["determining_filename"] == false
        && value["download_records"] == *user_records
        && value["tabs"] == *tabs
        && value["files"]["temporary"] == 0
        && (value["store"]["status"] == "absent" || value["store"]["temporary"] == 0)
        && value["adapter"]["temporary"] == 0
}

fn timings(run: &Run) -> Value {
    let mut items = Vec::new();
    let mut acquisition_items = Vec::new();
    let mut imported = std::collections::HashSet::new();
    let mut stages = serde_json::Map::<String, Value>::new();
    let mut events = serde_json::Map::<String, Value>::new();
    for line in run_log(run).lines() {
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let fields = &row["fields"];
        if let Some(event) = fields["event"].as_str() {
            let n = events.entry(event.to_owned()).or_insert(json!(0));
            *n = json!(n.as_u64().unwrap_or(0) + 1);
            if event == "browser_acquisition_item" {
                if imported.remove(fields["identity"].as_str().unwrap_or_default()) {
                    items.push(fields.clone());
                } else {
                    acquisition_items.push(fields.clone());
                }
            }
            if event == "browser_acquisition_stage" {
                let name = fields["stage"].as_str().unwrap_or("unknown");
                if name == "import_mp3" {
                    imported.insert(fields["identity"].as_str().unwrap_or_default().to_owned());
                }
                stages
                    .entry(name.to_owned())
                    .or_insert(json!([]))
                    .as_array_mut()
                    .expect("array")
                    .push(fields["stage_duration_ms"].clone());
            }
        }
    }
    let distribution = distribution(
        items
            .iter()
            .filter_map(|v| v["item_duration_ms"].as_u64())
            .collect(),
    );
    json!({"items":items,"item_distribution_ms":distribution,"acquisition_items":acquisition_items,"stage_duration_ms":stages,"events":events,
        "coverage":"outer items определяются предшествующей import_mp3; candidates включает вкладку/навигацию/кандидатов; download_and_validate_mp3 включает загрузку, проверку и освобождение файла; acquire охватывает эти фазы и первое подключение; import_mp3 включает чтение blob и CAS-импорт; checkpoint и окончательная очистка входят только в batch_wall_s; вложенные фазы не суммируются с acquire"})
}

fn distribution(mut values: Vec<u64>) -> Value {
    if values.is_empty() {
        return json!({"status":"unavailable","reason":"нет завершённых внешних BrowserItemTimer"});
    }
    values.sort_unstable();
    let count = values.len();
    let mean = values.iter().map(|v| *v as f64).sum::<f64>() / count as f64;
    let variance = (count > 1).then(|| {
        values
            .iter()
            .map(|v| (*v as f64 - mean).powi(2))
            .sum::<f64>()
            / (count - 1) as f64
    });
    json!({"count":count,"mean":mean,"stddev":variance.map(f64::sqrt),"min":values[0],"max":values[count-1],
        "median":(values[(count-1)/2] as f64+values[count/2] as f64)/2.0,
        "p95":if count>=100 { Some(values[(count*95).div_ceil(100)-1]) } else { None },
        "p95_scope":if count>=100 { "описательный nearest-rank внутри одного пакета; наблюдения последовательны, статистическая надёжность не доказана" } else { "не вычислен: меньше 100 наблюдений" }})
}

pub(super) async fn run(env: &mut Env<'_>) -> Result<()> {
    let cli = Cli::new(env).ok_or(MISSING_CLI)?;
    let ticks = std::process::Command::new("getconf")
        .arg("CLK_TCK")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|text| text.trim().parse::<u64>().ok());
    env.out.evidence.insert("clock_ticks_per_second".into(), match ticks {
        Some(ticks)=>json!({"status":"measured","value":ticks}),
        None=>json!({"status":"unavailable","reason":"getconf CLK_TCK не дал значение; CPU остаётся в ticks"}),
    });
    let user = env.browser.user_tab("https://forvo.com/").await?;
    super::chrome::TestBrowser::user_foreign_download(&user).await?;
    let user_files = super::scenarios::settle(
        &env.downloads(),
        &["user-notes.bin"],
        Duration::from_secs(10),
    )
    .await;
    if user_files.len() != 1
        || user_files[0].2 != crate::util::sha256_hex(&env.ctx.fixture.user_file)
    {
        return Err(format!(
            "контрольная чужая загрузка не получена: {}",
            files_json(&env.downloads())
        ));
    }
    env.connect().await?;
    let extension_id = env.conn.as_ref().expect("bridge").peer.extension_id.clone();
    env.conn.take().expect("bridge").bridge.disconnect().await;
    let observer_page = env
        .browser
        .user_tab(&format!("chrome-extension://{extension_id}/manifest.json"))
        .await?;
    let initial = resources(env, &observer_page).await?;
    let baseline_tabs = initial["tabs"].clone();
    let baseline_records = initial["download_records"].clone();
    env.out
        .evidence
        .insert("initial_resources".into(), initial.clone());
    env.out.check(
        "исходное состояние без собственных ресурсов",
        clean(&initial, &baseline_records, &baseline_tabs),
        initial,
    );
    let mut batches = Vec::new();
    for batch in 0..env.ctx.batch_count as usize {
        let words: Vec<_> = (0..env.ctx.batch_items as usize)
            .map(|i| fixture::load_word(batch * env.ctx.batch_items as usize + i))
            .collect();
        let pairs: Vec<_> = words
            .iter()
            .map(|(s, r, _)| (s.as_str(), r.as_str()))
            .collect();
        let id = format!("load-{batch}");
        let plan = cli.plan(&env.dir, &id, &pairs)?;
        println!(
            "  пакет {}/{}, {} разных пар",
            batch + 1,
            env.ctx.batch_count,
            words.len()
        );
        let started = Instant::now();
        let child = cli.spawn(
            true,
            &[
                "batch-start",
                "--batch-id",
                &id,
                "--plan",
                &plan.to_string_lossy(),
                "--close-tab",
            ],
        )?;
        let mut observer = Observer::new(
            child.id().ok_or("нет PID CLI")?,
            env.browser.pid().ok_or("нет PID Chromium")?,
        );
        observer.sample();
        let finished = Cli::finish_with_limit(child, Duration::from_secs(3600));
        tokio::pin!(finished);
        let mut interval = tokio::time::interval(Duration::from_millis(250));
        let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
        let run = loop {
            tokio::select! {
                result = &mut finished => break result?,
                _ = interval.tick() => observer.sample(),
                _ = heartbeat.tick() => println!("  {id}: CLI работает {:.0} с, fixture загрузок {}", started.elapsed().as_secs_f64(), words.iter().map(|(_,_,r)|env.ctx.fixture.requests_for(r)).sum::<usize>()),
            }
        };
        let seconds = started.elapsed().as_secs_f64();
        let mut evidence_ok =
            run.code == 0 && run.out["result"]["effective_transport"] == "brave_extension";
        let store = cli.open_store()?;
        let integrity = store.verify_integrity().map_err(|e| e.message)?;
        let mut runtime = SafeBatchRuntime::open(store.root(), &id).map_err(|e| e.message)?;
        let state: ForvoBatch = runtime
            .load()
            .map_err(|e| e.message)?
            .ok_or("нет checkpoint")?;
        evidence_ok &= state.items.len() == words.len();
        for ((surface, reading, record), saved) in words.iter().zip(&state.items) {
            let expected_sha = crate::util::sha256_hex(&env.ctx.fixture.bytes_for(record));
            let Some(acquisition) = &saved.acquisition else {
                evidence_ok = false;
                continue;
            };
            let bytes = runtime
                .read_blob(&acquisition.blob)
                .map_err(|e| e.message)?;
            evidence_ok &= saved.request.surface == *surface
                && saved.request.reading == *reading
                && saved.outcome.as_ref().is_some_and(|o| o.code == "imported")
                && acquisition.evidence.selected_record_id == *record
                && acquisition.evidence.page_url == asset_store::forvo::word_url(surface)
                && acquisition.evidence.ranking_policy == RANKING
                && acquisition.evidence.downloaded_sha256 == expected_sha
                && acquisition.evidence.attempts.len() == 1
                && acquisition.evidence.attempts[0].record_id == *record
                && acquisition.evidence.attempts[0].code == "downloaded"
                && crate::util::sha256_hex(&bytes) == expected_sha
                && integrity.iter().any(|r| {
                    r.identity == saved.request.identity()
                        && r.sha256 == expected_sha
                        && r.lifecycle.as_str() != "verified"
                        && std::fs::read(store.root().join(".runtime").join(&r.storage_path))
                            .is_ok_and(|bytes| crate::util::sha256_hex(&bytes) == expected_sha)
                });
        }
        drop(runtime);
        drop(store);
        let requests_before: Vec<_> = words
            .iter()
            .map(|(_, _, r)| env.ctx.fixture.requests_for(r))
            .collect();
        let revision = run.out["result"]["revision"].clone();
        let resume_started = Instant::now();
        let resumed = cli.run(true, &["batch-resume", "--batch-id", &id]).await?;
        let resume_s = resume_started.elapsed().as_secs_f64();
        let requests_after: Vec<_> = words
            .iter()
            .map(|(_, _, r)| env.ctx.fixture.requests_for(r))
            .collect();
        let snapshot = resources(env, &observer_page).await?;
        let diagnostics = timings(&run);
        let timing_ok = diagnostics["items"]
            .as_array()
            .is_some_and(|items| items.len() == words.len())
            && [
                "acquire",
                "import_mp3",
                "candidates",
                "download_and_validate_mp3",
            ]
            .iter()
            .all(|stage| {
                diagnostics["stage_duration_ms"][stage]
                    .as_array()
                    .is_some_and(|values| values.len() == words.len())
            });
        let batch_report = json!({"batch_id":id,"plan":pairs,"batch_wall_s":seconds,"seconds_per_word":seconds/words.len() as f64,
            "successful_words_per_second":if evidence_ok { Some(words.len() as f64/seconds) } else { None },
            "imports":state.items.iter().filter(|i|i.outcome.as_ref().is_some_and(|o|o.code=="imported")).count(),
            "revision":revision,"resume_revision":resumed.out["result"]["revision"],"resume_wall_s":resume_s,
            "state_bytes":cli.store.join(".runtime/batches").join(&id).join("state.json").metadata().map_err(|e|e.to_string())?.len(),
            "logs":observation::files(&cli.store.join(".runtime/batches").join(&id).join("logs"))?,
            "requests_before_resume":requests_before,"requests_after_resume":requests_after,
            "diagnostics":diagnostics,"process_observations":observer.report(),"resources":snapshot});
        let no_repeat =
            requests_before.iter().all(|n| *n == 1) && requests_after == requests_before;
        env.out.check(
            format!("{id}: все пары, SHA, ForvoEvidence и импорт без Verified"),
            evidence_ok,
            json!({"exit":run.code,"items":state.items.len()}),
        );
        env.out.check(format!("{id}: полные таймеры без смешения вложенных items"), timing_ok, json!({"outer_items":diagnostics["items"].as_array().map(Vec::len),"inner_items":diagnostics["acquisition_items"].as_array().map(Vec::len)}));
        env.out.check(format!("{id}: resume без повторных загрузок и смены ревизии"), no_repeat && resumed.code == 0 && resumed.out["result"]["revision"] == revision, json!({"revision":revision,"resume_revision":resumed.out["result"]["revision"],"requests":requests_after}));
        // Подусловия сохраняются отдельно: провал виден без повторного прогона.
        let resources_clean = clean(&snapshot, &baseline_records, &baseline_tabs);
        let downloads_now = super::chrome::download_files(&env.downloads());
        let mut resources_evidence = snapshot.clone();
        resources_evidence["check"] = json!({"resources_clean":resources_clean,
            "user_files_kept":downloads_now == user_files,"downloads_now":downloads_now});
        env.out.check(
            format!("{id}: ресурсы освобождены, чужие вкладки, records и файл сохранены"),
            resources_clean && downloads_now == user_files,
            resources_evidence,
        );
        batches.push(batch_report);
        // Сохраняем и при последующей ошибке: динамика уже завершённых пакетов не теряется.
        env.out.evidence.insert("batches".into(), json!(batches));
        if !evidence_ok || !no_repeat || !timing_ok {
            return Err(format!("{id}: нарушены постусловия"));
        }
    }
    observer_page.close().await.map_err(|e| e.to_string())?;
    env.out
        .evidence
        .insert("items_per_batch".into(), json!(env.ctx.batch_items));
    env.out
        .evidence
        .insert("batch_count".into(), json!(env.ctx.batch_count));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::distribution;
    #[test]
    fn tails_require_enough_observations_and_never_claim_reliability() {
        assert_eq!(distribution(vec![])["status"], "unavailable");
        let small = distribution(vec![2, 4, 6]);
        assert_eq!(small["median"], 4.0);
        assert!(small["p95"].is_null());
        assert_eq!(distribution((1..=100).rev().collect())["p95"], 95);
    }
}
