#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod store;

use base64::Engine as _;
use embeddinggemma_core::{
    Content, ContentEmbeddingsRequest, DeviceRequest, Embedder, EmbedderOptions, EmbeddingTask,
    EmbeddingsRequest, OneOrMany,
};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tauri::{AppHandle, Manager, State};

#[cfg(target_os = "macos")]
use swift_rs::{Int, SRString, swift};

#[cfg(target_os = "macos")]
swift!(fn shiguang_photos_status() -> SRString);
swift!(fn shiguang_photos_request_authorization() -> SRString);
swift!(fn shiguang_photos_list(offset: Int, limit: Int, favorites: Int) -> SRString);
swift!(fn shiguang_photo_thumbnail(id: &SRString) -> SRString);
swift!(fn shiguang_photo_image(id: &SRString) -> SRString);
swift!(fn shiguang_photo_embedding_image(id: &SRString) -> SRString);
swift!(fn shiguang_photos_delete(ids: &SRString) -> SRString);
swift!(fn shiguang_usb_list(offset: Int, limit: Int) -> SRString);
swift!(fn shiguang_usb_status() -> SRString);
swift!(fn shiguang_usb_thumbnail(id: &SRString) -> SRString);
swift!(fn shiguang_usb_image(id: &SRString) -> SRString);
swift!(fn shiguang_usb_embedding_image(id: &SRString) -> SRString);
swift!(fn shiguang_usb_delete(ids: &SRString) -> SRString);

#[derive(Clone)]
struct AppState {
    db: Arc<Mutex<Connection>>,
    model: Arc<Mutex<Option<Arc<Embedder>>>>,
    model_init: Arc<Mutex<()>>,
    model_state: Arc<Mutex<String>>,
    scan_state: Arc<Mutex<ScanProgress>>,
    index_state: Arc<Mutex<IndexProgress>>,
    index_cancel: Arc<AtomicBool>,
    task_active: Arc<AtomicBool>,
}

#[derive(Default, Clone, Serialize)]
struct ScanProgress {
    running: bool,
    stage: String,
    completed: usize,
    total: usize,
    message: String,
}

#[derive(Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct IndexProgress {
    running: bool,
    stage: String,
    completed: usize,
    total: usize,
    failed: usize,
    message: String,
    start_date: String,
    end_date: String,
    source: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IndexInput {
    start_date: String,
    end_date: String,
    start_at: String,
    end_before: String,
    source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Asset {
    id: String,
    source: String,
    name: String,
    date: String,
    width: i32,
    height: i32,
    favorite: bool,
    media_type: String,
    thumbnail: Option<String>,
    score: Option<f32>,
    reviewed: String,
    queued: bool,
    rule_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Rule {
    id: String,
    name: String,
    prompt: String,
    exclude_prompt: String,
    threshold: f32,
    enabled: bool,
    candidates: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListArgs {
    page: String,
    source: String,
    rule_id: Option<String>,
    review_state: Option<String>,
    query: Option<String>,
    offset: usize,
    limit: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuleInput {
    id: Option<String>,
    name: String,
    prompt: String,
    exclude_prompt: String,
    threshold: f32,
    enabled: bool,
}

#[derive(Deserialize)]
struct ReviewInput {
    ids: Vec<String>,
    action: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchInput {
    query: String,
    limit: usize,
    #[serde(default = "default_search_source")]
    source: String,
    #[serde(default)]
    include_weak: bool,
}

fn default_search_source() -> String {
    "all".into()
}

#[derive(Deserialize)]
struct DeleteInput {
    ids: Vec<String>,
}

fn native_json(raw: String) -> Result<Value, String> {
    serde_json::from_str(&raw).map_err(|e| format!("系统照片接口返回无效数据：{e}"))
}

fn wait_for_native_result(mut request: impl FnMut() -> String) -> Result<Value, String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    loop {
        let result = native_json(request())?;
        if result.get("pending") != Some(&Value::Bool(true)) {
            return Ok(result);
        }
        if std::time::Instant::now() >= deadline {
            return Err("系统照片操作超时，请检查图库或 iPhone 连接状态".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

#[cfg(target_os = "macos")]
fn native_string(value: SRString) -> String {
    value.as_str().to_owned()
}

#[cfg(target_os = "macos")]
fn photos_page(offset: usize, limit: usize, favorites: bool) -> Result<Value, String> {
    native_json(unsafe {
        native_string(shiguang_photos_list(
            offset as Int,
            limit as Int,
            if favorites { 1 } else { 0 },
        ))
    })
}

#[cfg(target_os = "macos")]
fn photos_authorization() -> Result<Value, String> {
    native_json(unsafe { native_string(shiguang_photos_status()) })
}

#[cfg(target_os = "macos")]
fn request_photos_authorization() -> Result<Value, String> {
    native_json(unsafe { native_string(shiguang_photos_request_authorization()) })
}

#[cfg(not(target_os = "macos"))]
fn photos_page(_offset: usize, _limit: usize, _favorites: bool) -> Result<Value, String> {
    Ok(json!({"status":"unsupported","total":0,"assets":[]}))
}

#[cfg(not(target_os = "macos"))]
fn photos_authorization() -> Result<Value, String> {
    Ok(json!({"status":"unsupported","count":0}))
}

#[cfg(not(target_os = "macos"))]
fn request_photos_authorization() -> Result<Value, String> {
    Ok(json!({"status":"unsupported","count":0}))
}

#[cfg(target_os = "macos")]
fn usb_page_range(offset: usize, limit: usize) -> Result<Value, String> {
    native_json(unsafe { native_string(shiguang_usb_list(offset as Int, limit as Int)) })
}

#[cfg(target_os = "macos")]
fn usb_status_value() -> Result<Value, String> {
    native_json(unsafe { native_string(shiguang_usb_status()) })
}

#[cfg(not(target_os = "macos"))]
fn usb_page_range(_offset: usize, _limit: usize) -> Result<Value, String> {
    Ok(json!({"status":"unsupported","total":0,"assets":[]}))
}

#[cfg(not(target_os = "macos"))]
fn usb_status_value() -> Result<Value, String> {
    Ok(json!({"status":"unsupported"}))
}

#[cfg(target_os = "macos")]
fn asset_thumbnail(id: &str, source: &str) -> Result<Option<String>, String> {
    let id = SRString::from(id);
    let result = unsafe {
        if source == "usb" {
            native_string(shiguang_usb_thumbnail(&id))
        } else {
            native_string(shiguang_photo_thumbnail(&id))
        }
    };
    Ok((!result.is_empty()).then_some(result))
}

#[cfg(target_os = "macos")]
fn asset_image_data_url(id: &str, source: &str, for_embedding: bool) -> Result<Option<String>, String> {
    let id = SRString::from(id);
    let result = unsafe {
        match (source, for_embedding) {
            ("usb", true) => native_string(shiguang_usb_embedding_image(&id)),
            ("usb", false) => native_string(shiguang_usb_image(&id)),
            (_, true) => native_string(shiguang_photo_embedding_image(&id)),
            (_, false) => native_string(shiguang_photo_image(&id)),
        }
    };
    if result.is_empty() { return Ok(None); }
    if let Some(error) = result.strip_prefix("ERR:") { return Err(error.to_owned()); }
    Ok(Some(result))
}

#[cfg(target_os = "macos")]
fn wait_for_image_data_url(
    id: &str,
    source: &str,
    for_embedding: bool,
    cancelled: impl Fn() -> bool,
) -> Result<Option<String>, String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
    loop {
        if cancelled() { return Ok(None); }
        if let Some(data) = asset_image_data_url(id, source, for_embedding)? { return Ok(Some(data)); }
        if std::time::Instant::now() >= deadline {
            return Err(format!("读取「{id}」的原图超时，请检查图库或 iPhone 连接状态"));
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
}

#[cfg(not(target_os = "macos"))]
fn asset_thumbnail(_id: &str, _source: &str) -> Result<Option<String>, String> {
    Ok(None)
}

#[cfg(not(target_os = "macos"))]
fn asset_image_data_url(_id: &str, _source: &str, _for_embedding: bool) -> Result<Option<String>, String> { Ok(None) }

#[cfg(not(target_os = "macos"))]
fn wait_for_image_data_url(_id: &str, _source: &str, _for_embedding: bool, _cancelled: impl Fn() -> bool) -> Result<Option<String>, String> { Ok(None) }

fn database_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.join("shiguang.sqlite3"))
}

fn model_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| e.to_string())?
        .join("models");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

fn ensure_model(app: &AppHandle, state: &AppState) -> Result<Arc<Embedder>, String> {
    if let Some(model) = state.model.lock().map_err(|e| e.to_string())?.clone() {
        return Ok(model);
    }
    let _initializing = state.model_init.lock().map_err(|e| e.to_string())?;
    if let Some(model) = state.model.lock().map_err(|e| e.to_string())?.clone() {
        return Ok(model);
    }
    *state.model_state.lock().map_err(|e| e.to_string())? = "downloading".into();
    let loaded = (|| -> Result<Arc<Embedder>, String> {
        let dir = model_dir(app)?;
        unsafe {
            std::env::set_var("HF_HOME", &dir);
            std::env::set_var("HF_ENDPOINT", "https://hf-mirror.com");
        }
        let options = EmbedderOptions {
            device: DeviceRequest::Auto,
            modalities: Some(embeddinggemma_core::Modalities {
                image: true,
                audio: false,
            }),
            // DigUp indexes regular photos at 140 vision soft tokens.
            // ImageIO preprocessing preserves orientation and aspect ratio at 1024 px.
            image_soft_tokens: Some(140),
            ..Default::default()
        };
        let loaded = Embedder::load_from("440m", &dir, options)
            .map_err(|e| format!("EmbeddingGemma 2 加载失败：{e}"))?;
        *state.model_state.lock().map_err(|e| e.to_string())? = "warming".into();
        loaded.warmup().map_err(|e| format!("模型预热失败：{e}"))?;
        Ok(Arc::new(loaded))
    })();
    match loaded {
        Ok(model) => {
            *state.model.lock().map_err(|e| e.to_string())? = Some(model.clone());
            *state.model_state.lock().map_err(|e| e.to_string())? = "ready".into();
            Ok(model)
        }
        Err(error) => {
            *state.model_state.lock().map_err(|e| e.to_string())? = "error".into();
            Err(error)
        }
    }
}

fn parse_native_assets(value: &Value) -> Vec<Asset> {
    value
        .get("assets")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|v| {
            Some(Asset {
                id: {
                    let id = v.get("id")?.as_str()?;
                    if v.get("source").and_then(Value::as_str) == Some("usb") {
                        id.strip_prefix("usb:").unwrap_or(id).to_string()
                    } else {
                        id.to_string()
                    }
                },
                source: v.get("source")?.as_str()?.to_string(),
                name: v
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("照片")
                    .to_string(),
                date: v
                    .get("date")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                width: v.get("width").and_then(Value::as_i64).unwrap_or(0) as i32,
                height: v.get("height").and_then(Value::as_i64).unwrap_or(0) as i32,
                favorite: v.get("favorite").and_then(Value::as_bool).unwrap_or(false),
                media_type: v
                    .get("mediaType")
                    .and_then(Value::as_str)
                    .unwrap_or("image")
                    .to_string(),
                thumbnail: None,
                score: None,
                reviewed: "pending".into(),
                queued: false,
                rule_id: None,
            })
        })
        .collect()
}

fn sync_metadata(db: &Connection, assets: &[Asset]) -> Result<(), String> {
    let tx = db.unchecked_transaction().map_err(|e| e.to_string())?;
    for a in assets {
        tx.execute("INSERT INTO assets(asset_key,source,name,created_at,width,height,favorite,media_type) VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(asset_key) DO UPDATE SET name=excluded.name,created_at=excluded.created_at,width=excluded.width,height=excluded.height,favorite=excluded.favorite,media_type=excluded.media_type",
            rusqlite::params![format!("{}:{}",a.source,a.id),a.source,a.name,a.date,a.width,a.height,a.favorite,a.media_type]).map_err(|e| e.to_string())?;
    }
    tx.commit().map_err(|e| e.to_string())
}

fn valid_calendar_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-'
        || !bytes.iter().enumerate().all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
    {
        return false;
    }
    let Some((year, month, day)) = value.split_once('-').and_then(|(year, rest)| {
        rest.split_once('-').and_then(|(month, day)| {
            Some((year.parse::<i32>().ok()?, month.parse::<u32>().ok()?, day.parse::<u32>().ok()?))
        })
    }) else { return false; };
    if value.len() != 10 || !(1..=12).contains(&month) || year < 1 { return false; }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    (1..=days).contains(&day)
}

fn valid_utc_timestamp(value: &str) -> bool {
    if value.len() != 20 || !value.ends_with('Z') { return false; }
    let date = &value[..10];
    let time = &value[11..19];
    valid_calendar_date(date)
        && value.as_bytes()[10] == b'T'
        && time.as_bytes().get(2) == Some(&b':')
        && time.as_bytes().get(5) == Some(&b':')
        && time.as_bytes().iter().enumerate().all(|(index, byte)| index == 2 || index == 5 || byte.is_ascii_digit())
        && time[0..2].parse::<u32>().is_ok_and(|hour| hour < 24)
        && time[3..5].parse::<u32>().is_ok_and(|minute| minute < 60)
        && time[6..8].parse::<u32>().is_ok_and(|second| second < 60)
}

fn set_index_progress(state: &AppState, update: impl FnOnce(&mut IndexProgress)) {
    if let Ok(mut progress) = state.index_state.lock() { update(&mut progress); }
}

fn collect_index_assets(source: &str, cancelled: impl Fn() -> bool) -> Result<Option<Vec<Asset>>, String> {
    let mut assets = Vec::new();
    if source != "usb" {
        let status = photos_authorization()?;
        if matches!(status.get("status").and_then(Value::as_str), Some("authorized" | "limited")) {
            let total = status.get("count").and_then(Value::as_u64).unwrap_or(0) as usize;
            let mut offset = 0;
            while offset < total {
                if cancelled() { return Ok(None); }
                let page = photos_page(offset, 200, false)?;
                let batch = parse_native_assets(&page);
                if batch.is_empty() { break; }
                offset += batch.len();
                assets.extend(batch);
            }
        } else if source == "photos" {
            return Err("请先允许拾光访问 macOS 照片图库".into());
        }
    }
    if source != "photos" {
        let first = usb_page_range(0, 200)?;
        let status = first.get("status").and_then(Value::as_str).unwrap_or("disconnected");
        if status == "connected" {
            let total = first.get("total").and_then(Value::as_u64).unwrap_or(0) as usize;
            let mut batch = parse_native_assets(&first);
            let mut offset = batch.len();
            assets.append(&mut batch);
            while offset < total {
                if cancelled() { return Ok(None); }
                let page = usb_page_range(offset, 200)?;
                let mut batch = parse_native_assets(&page);
                if batch.is_empty() { break; }
                offset += batch.len();
                assets.append(&mut batch);
            }
        } else if source == "usb" {
            return Err(first.get("error").and_then(Value::as_str).unwrap_or("请连接并解锁 iPhone").to_owned());
        }
    }
    Ok(Some(assets))
}

#[tauri::command]
async fn load_original_image(id: String, source: String) -> Result<Option<String>, String> {
    tauri::async_runtime::spawn_blocking(move || wait_for_image_data_url(&id, &source, false, || false))
        .await.map_err(|e| e.to_string())?
}

#[tauri::command]
async fn index_progress(state: State<'_, AppState>) -> Result<IndexProgress, String> {
    state.index_state.lock().map_err(|e| e.to_string()).map(|v| v.clone())
}

#[tauri::command]
async fn stop_indexing(state: State<'_, AppState>) -> Result<(), String> {
    state.index_cancel.store(true, Ordering::Release);
    Ok(())
}

#[tauri::command]
async fn start_indexing(app: AppHandle, state: State<'_, AppState>, input: IndexInput) -> Result<Value, String> {
    let state = state.inner().clone();
    let start_date = input.start_date.trim().to_owned();
    let end_date = input.end_date.trim().to_owned();
    let source = match input.source.as_str() { "photos" | "usb" => input.source, _ => "all".into() };
    if !valid_calendar_date(&start_date) || !valid_calendar_date(&end_date) || start_date > end_date {
        return Err("请选择有效的日期范围，开始日期不能晚于结束日期".into());
    }
    let start_at = input.start_at.trim().to_owned();
    let end_before = input.end_before.trim().to_owned();
    if !valid_utc_timestamp(&start_at) || !valid_utc_timestamp(&end_before) || start_at >= end_before {
        return Err("日期范围无效，请重新选择日期".into());
    }
    if state.task_active.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
        return Err("已有扫描或索引任务正在运行，请等待完成".into());
    }
    state.index_cancel.store(false, Ordering::Release);
    set_index_progress(&state, |progress| *progress = IndexProgress {
        running: true, stage: "preparing".into(), completed: 0, total: 0, failed: 0,
        message: "正在同步照片信息…".into(), start_date: start_date.clone(), end_date: end_date.clone(), source: source.clone(),
    });
    tauri::async_runtime::spawn_blocking(move || {
        let result = (|| -> Result<Value, String> {
            let Some(assets) = collect_index_assets(&source, || state.index_cancel.load(Ordering::Acquire))? else {
                set_index_progress(&state, |progress| { progress.running = false; progress.stage = "cancelled".into(); progress.message = "索引已停止".into(); });
                return Ok(json!({"indexed":0,"failed":0,"cancelled":true}));
            };
            sync_metadata(&*state.db.lock().map_err(|e| e.to_string())?, &assets)?;
            if state.index_cancel.load(Ordering::Acquire) {
                set_index_progress(&state, |progress| { progress.running = false; progress.stage = "cancelled".into(); progress.message = "索引已停止".into(); });
                return Ok(json!({"indexed":0,"failed":0,"cancelled":true}));
            }
            let pending = store::assets_for_index(&*state.db.lock().map_err(|e| e.to_string())?, &source, &start_at, &end_before)?;
            let total = pending.len();
            set_index_progress(&state, |progress| {
                progress.running = total != 0;
                progress.stage = if total == 0 { "done" } else { "embedding" }.into();
                progress.total = total;
                progress.message = if total == 0 { "所选日期范围内没有待建立索引的原图".into() } else { format!("正在读取原图并建立索引，共 {total} 张") };
            });
            if total == 0 { return Ok(json!({"indexed":0,"failed":0})); }
            if state.index_cancel.load(Ordering::Acquire) {
                set_index_progress(&state, |progress| { progress.running = false; progress.stage = "cancelled".into(); progress.message = "索引已停止".into(); });
                return Ok(json!({"indexed":0,"failed":0,"cancelled":true}));
            }
            set_index_progress(&state, |progress| { progress.stage = "model".into(); progress.message = "正在加载本地图片向量模型…".into(); });
            let model = ensure_model(&app, &state)?;
            let mut completed = 0usize;
            let mut failed = 0usize;
            for chunk in pending.chunks(4) {
                if state.index_cancel.load(Ordering::Acquire) { break; }
                let mut contents = Vec::new();
                let mut accepted = Vec::new();
                for asset in chunk {
                    if state.index_cancel.load(Ordering::Acquire) { break; }
                    match wait_for_image_data_url(&asset.id, &asset.source, true, || state.index_cancel.load(Ordering::Acquire)) {
                        Ok(Some(data)) => match base64::engine::general_purpose::STANDARD.decode(data.split_once(',').map_or(data.as_str(), |(_, bytes)| bytes)) {
                            Ok(bytes) => {
                                contents.push(Content::Image { data: bytes.into(), format: Some("image/jpeg".into()) });
                                accepted.push(asset.clone());
                            }
                            Err(_) => { failed += 1; completed += 1; }
                        },
                        Ok(None) if state.index_cancel.load(Ordering::Acquire) => break,
                        Ok(None) | Err(_) => { failed += 1; completed += 1; }
                    }
                }
                if state.index_cancel.load(Ordering::Acquire) { break; }
                if !contents.is_empty() {
                    match model.embed_contents(&ContentEmbeddingsRequest { input: OneOrMany::Many(contents), ..Default::default() }) {
                        Ok(result) => {
                            let db = state.db.lock().map_err(|e| e.to_string())?;
                            for (asset, vector) in accepted.into_iter().zip(result.output) {
                                if store::save_embedding(&db, &asset, &vector).is_ok() { completed += 1; }
                                else { completed += 1; failed += 1; }
                            }
                        }
                        Err(_) => { failed += accepted.len(); completed += accepted.len(); }
                    }
                }
                set_index_progress(&state, |progress| {
                    progress.completed = completed.min(total);
                    progress.failed = failed;
                    progress.message = format!("已处理 {} / {total} 张，失败 {failed} 张", completed.min(total));
                });
            }
            let cancelled = state.index_cancel.load(Ordering::Acquire);
            set_index_progress(&state, |progress| {
                progress.running = false;
                progress.completed = completed.min(total);
                progress.failed = failed;
                progress.stage = if cancelled { "cancelled" } else if failed > 0 { "done_with_errors" } else { "done" }.into();
                progress.message = if cancelled { format!("索引已停止，完成 {} / {total} 张", completed.min(total)) }
                    else { format!("索引完成，成功 {} 张，失败 {failed} 张", completed.saturating_sub(failed)) };
            });
            Ok(json!({"indexed":completed.saturating_sub(failed),"failed":failed,"cancelled":cancelled}))
        })();
        if let Err(error) = &result {
            set_index_progress(&state, |progress| { progress.running = false; progress.stage = "error".into(); progress.message = format!("索引失败：{error}"); });
        }
        state.task_active.store(false, Ordering::Release);
        result
    }).await.map_err(|e| e.to_string())?
}

#[tauri::command]
async fn bootstrap(state: State<'_, AppState>) -> Result<Value, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let status = photos_authorization()?;
        let photos = photos_page(0, 1, false)?;
        let photo_total = status.get("count").cloned().unwrap_or_else(|| photos.get("total").cloned().unwrap_or(Value::from(0)));
        let usb = usb_page_range(0, 1)?;
        let assets = parse_native_assets(&photos);
        sync_metadata(&*state.db.lock().map_err(|e| e.to_string())?, &assets)?;
        let usb_assets = parse_native_assets(&usb);
        sync_metadata(&*state.db.lock().map_err(|e| e.to_string())?, &usb_assets)?;
        let rules = store::list_rules(&*state.db.lock().map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        let mut counts = store::counts(&*state.db.lock().map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        if let Some(object) = counts.as_object_mut() {
            object.insert("photos".into(), photo_total.clone());
            object.insert("usb".into(), usb.get("total").cloned().unwrap_or_else(|| Value::from(usb_assets.len())));
        }
        let mut rows = assets;
        rows.extend(usb_assets);
        for asset in &mut rows { store::decorate_asset(&*state.db.lock().map_err(|e| e.to_string())?, asset)?; }
        let model_state = state.model_state.lock().map_err(|e| e.to_string())?.clone();
        Ok(json!({"photos":status,"usb":usb,"assets":rows,"rules":rules,"counts":counts,"modelState":model_state,"photoTotal":photo_total}))
    }).await.map_err(|e| e.to_string())?
}

#[tauri::command]
async fn photos_status() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(photos_authorization)
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn photos_authorize() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(request_photos_authorization)
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn usb_status() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(usb_status_value)
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn list_assets(state: State<'_, AppState>, args: ListArgs) -> Result<Value, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        if args.page == "queue" || args.page == "cleanup" {
            let db = state.db.lock().map_err(|e| e.to_string())?;
            if args.page == "queue" {
                let (assets, total) =
                    store::list_queued(&db, &args.source, args.offset, args.limit)?;
                return Ok(json!({"assets":assets,"total":total,"sourceState":"cached"}));
            }
            if let Some(rule_id) = args.rule_id.as_deref() {
                let review_state = match args.review_state.as_deref() {
                    Some("pending") => Some("pending"),
                    Some("later") => Some("later"),
                    _ => None,
                };
                let (assets, total, pending, later) =
                    store::list_rule_matches(
                        &db,
                        rule_id,
                        &args.source,
                        review_state,
                        args.offset,
                        args.limit,
                    )?;
                return Ok(json!({"assets":assets,"total":total,"pending":pending,"later":later,"sourceState":"cached"}));
            }
        }
        let (mut assets, total, source_state) = match args.source.as_str() {
            "usb" => {
                let p = usb_page_range(args.offset, args.limit)?;
                (
                    parse_native_assets(&p),
                    p.get("total").and_then(Value::as_u64).unwrap_or(0) as usize,
                    p.get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("disconnected")
                        .to_string(),
                )
            }
            _ => {
                let p = photos_page(args.offset, args.limit, args.page == "favorites")?;
                (
                    parse_native_assets(&p),
                    p.get("total").and_then(Value::as_u64).unwrap_or(0) as usize,
                    p.get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("ok")
                        .to_string(),
                )
            }
        };
        sync_metadata(&*state.db.lock().map_err(|e| e.to_string())?, &assets)?;
        let db = state.db.lock().map_err(|e| e.to_string())?;
        for asset in &mut assets {
            store::decorate_asset(&db, asset)?;
        }
        if let Some(query) = args.query.filter(|q| !q.trim().is_empty()) {
            let q = query.to_lowercase();
            assets.retain(|a| a.name.to_lowercase().contains(&q));
        }
        Ok(json!({"assets":assets,"total":total,"sourceState":source_state}))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn list_rules(state: State<'_, AppState>) -> Result<Vec<Rule>, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        store::list_rules(&*state.db.lock().map_err(|e| e.to_string())?).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn load_thumbnail(
    state: State<'_, AppState>,
    id: String,
    source: String,
) -> Result<Option<String>, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        if let Some(value) = store::cached_thumbnail(
            &*state.db.lock().map_err(|e| e.to_string())?,
            &format!("{source}:{id}"),
        )? {
            return Ok(Some(value));
        }
        let value = asset_thumbnail(&id, &source)?;
        if let Some(data) = &value {
            store::save_thumbnail(
                &*state.db.lock().map_err(|e| e.to_string())?,
                &format!("{source}:{id}"),
                data,
            )?;
        }
        Ok(value)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn save_rule(state: State<'_, AppState>, input: RuleInput) -> Result<Rule, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        store::save_rule(&*state.db.lock().map_err(|e| e.to_string())?, input)
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn set_review_state(state: State<'_, AppState>, input: ReviewInput) -> Result<(), String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        store::set_review(&*state.db.lock().map_err(|e| e.to_string())?, input)
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn scan_rule(
    app: AppHandle,
    state: State<'_, AppState>,
    rule_id: String,
    source: String,
) -> Result<Value, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        if state.task_active.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return Err("已有扫描或索引任务正在运行，请等待完成".into());
        }
        {
            let mut progress = state.scan_state.lock().map_err(|e| e.to_string())?;
            *progress = ScanProgress {
                running: true,
                stage: "preparing".into(),
                completed: 0,
                total: 0,
                message: "正在准备扫描…".into(),
            };
        }
        let result = (|| -> Result<Value, String> {
            let rule = store::get_rule(&*state.db.lock().map_err(|e| e.to_string())?, &rule_id)
                .map_err(|e| e.to_string())?
                .ok_or("规则不存在")?;
            let mut all = Vec::new();
            let mut scanned_sources = Vec::new();
            if source != "usb" {
                let status = photos_authorization()?;
                if matches!(
                    status.get("status").and_then(Value::as_str),
                    Some("authorized" | "limited")
                ) {
                    scanned_sources.push("photos".to_string());
                    let total = status.get("count").and_then(Value::as_u64).unwrap_or(0) as usize;
                    let mut offset = 0;
                    while offset < total {
                        let page = photos_page(offset, 40, false)?;
                        let batch = parse_native_assets(&page);
                        if batch.is_empty() {
                            break;
                        }
                        offset += batch.len();
                        all.extend(batch);
                    }
                } else if source == "photos" {
                    return Err("请先允许拾光访问 macOS 照片图库".into());
                }
            }
            if source != "photos" {
                let first_page = usb_page_range(0, 40)?;
                let usb_status = first_page
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("disconnected");
                if usb_status != "connected" && source == "usb" {
                    return Err(first_page
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("请连接并解锁 iPhone")
                        .to_string());
                }
                if usb_status == "connected" {
                    scanned_sources.push("usb".to_string());
                    let total =
                        first_page.get("total").and_then(Value::as_u64).unwrap_or(0) as usize;
                    all.extend(parse_native_assets(&first_page));
                    let mut offset = first_page
                        .get("assets")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len);
                    while offset < total {
                        let page = usb_page_range(offset, 40)?;
                        let batch = parse_native_assets(&page);
                        if batch.is_empty() {
                            break;
                        }
                        offset += batch.len();
                        all.extend(batch);
                    }
                }
            }
            if all.is_empty() {
                return Err("当前来源没有可扫描的照片".into());
            }
            let model = ensure_model(&app, &state)?;
            let positive = model
                .embed_request(&EmbeddingsRequest {
                    input: rule.prompt.clone().into(),
                    task: Some(EmbeddingTask::Query),
                    ..Default::default()
                })
                .map_err(|e| e.to_string())?
                .output
                .remove(0);
            let negative = if rule.exclude_prompt.trim().is_empty() {
                None
            } else {
                Some(
                    model
                        .embed_request(&EmbeddingsRequest {
                            input: rule.exclude_prompt.clone().into(),
                            task: Some(EmbeddingTask::Query),
                            ..Default::default()
                        })
                        .map_err(|e| e.to_string())?
                        .output
                        .remove(0),
                )
            };
            let total = all.len();
            *state.scan_state.lock().map_err(|e| e.to_string())? = ScanProgress {
                running: true,
                stage: "embedding".into(),
                completed: 0,
                total,
                message: format!("正在分析 {total} 张照片"),
            };
            sync_metadata(&*state.db.lock().map_err(|e| e.to_string())?, &all)?;
            let mut rule_matches = Vec::new();
            for chunk in all.chunks(4) {
                let mut vectors = Vec::new();
                let mut contents = Vec::new();
                let mut missing = Vec::new();
                for asset in chunk {
                    let cached = store::cached_embedding(
                        &*state.db.lock().map_err(|e| e.to_string())?,
                        asset,
                    )?;
                    if let Some(vector) = cached {
                        vectors.push((asset.clone(), vector));
                        continue;
                    }
                    match wait_for_image_data_url(&asset.id, &asset.source, true, || false)? {
                        Some(data) => {
                            let bytes = base64::engine::general_purpose::STANDARD
                                .decode(data.split_once(',').map_or(data.as_str(), |(_, bytes)| bytes))
                                .map_err(|error| error.to_string())?;
                            contents.push(Content::Image {
                                data: bytes.into(),
                                format: Some("image/jpeg".into()),
                            });
                            missing.push(asset.clone());
                        }
                        None => {}
                    }
                }
                if !contents.is_empty() {
                    let computed = model
                        .embed_contents(&ContentEmbeddingsRequest {
                            input: OneOrMany::Many(contents),
                            ..Default::default()
                        })
                        .map_err(|error| error.to_string())?
                        .output;
                    let db = state.db.lock().map_err(|e| e.to_string())?;
                    for (asset, vector) in missing.into_iter().zip(computed) {
                        store::save_embedding(&db, &asset, &vector).map_err(|error| error.to_string())?;
                        vectors.push((asset, vector));
                    }
                }
                {
                    for (asset, vector) in vectors {
                        let score = cosine(&positive, &vector)
                            - negative
                                .as_ref()
                                .map_or(0.0, |n| cosine(n, &vector).max(0.0) * 0.5);
                        rule_matches.push((asset, score));
                    }
                }
                let completed =
                    (*state.scan_state.lock().map_err(|e| e.to_string())?).completed + chunk.len();
                *state.scan_state.lock().map_err(|e| e.to_string())? = ScanProgress {
                    running: true,
                    stage: "embedding".into(),
                    completed: completed.min(total),
                    total,
                    message: format!("已分析 {} / {total} 张", completed.min(total)),
                };
            }
            store::replace_rule_matches(
                &*state.db.lock().map_err(|e| e.to_string())?,
                &rule_id,
                &scanned_sources,
                &rule_matches,
                rule.threshold,
            )
            .map_err(|e| e.to_string())?;
            let candidates =
                store::rule_match_count(&*state.db.lock().map_err(|e| e.to_string())?, &rule_id)
                    .map_err(|e| e.to_string())?;
            *state.scan_state.lock().map_err(|e| e.to_string())? = ScanProgress {
                running: false,
                stage: "done".into(),
                completed: total,
                total,
                message: format!("扫描完成，发现 {candidates} 张候选"),
            };
            Ok(json!({"scanned":total,"candidates":candidates}))
        })();
        if let Err(error) = &result {
            if let Ok(mut progress) = state.scan_state.lock() {
                let completed = progress.completed;
                let total = progress.total;
                *progress = ScanProgress {
                    running: false,
                    stage: "error".into(),
                    completed,
                    total,
                    message: format!("扫描失败：{error}"),
                };
            }
        }
        state.task_active.store(false, Ordering::Release);
        result
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn scan_progress(state: State<'_, AppState>) -> Result<ScanProgress, String> {
    state
        .scan_state
        .lock()
        .map_err(|e| e.to_string())
        .map(|v| v.clone())
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (dot, norm_a, norm_b) = a.iter().zip(b).fold(
        (0.0_f32, 0.0_f32, 0.0_f32),
        |(dot, norm_a, norm_b), (x, y)| (dot + x * y, norm_a + x * x, norm_b + y * y),
    );
    let denominator = norm_a.sqrt() * norm_b.sqrt();
    if denominator == 0.0 {
        0.0
    } else {
        dot / denominator
    }
}

#[tauri::command]
async fn semantic_search(
    app: AppHandle,
    state: State<'_, AppState>,
    input: SearchInput,
) -> Result<Vec<Value>, String> {
    let text = input.query.trim().to_owned();
    if text.is_empty() || input.limit == 0 {
        return Ok(Vec::new());
    }
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        if store::embedding_count(&*state.db.lock().map_err(|e| e.to_string())?, &input.source)
            .map_err(|e| e.to_string())?
            == 0
        {
            return Ok(Vec::new());
        }
        let model = ensure_model(&app, &state)?;
        let query = model
            .embed_request(&EmbeddingsRequest {
                input: text.clone().into(),
                task: Some(EmbeddingTask::Query),
                ..Default::default()
            })
            .map_err(|e| e.to_string())?
            .output
            .remove(0);
        store::semantic_search(
            &*state.db.lock().map_err(|e| e.to_string())?,
            &query,
            &text,
            input.limit,
            &input.source,
            input.include_weak,
        )
        .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn similar_photos(
    state: State<'_, AppState>,
    id: String,
    anchor_source: String,
    source: String,
    limit: usize,
) -> Result<Vec<Value>, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        store::similar_assets(
            &*state.db.lock().map_err(|e| e.to_string())?,
            &id,
            &anchor_source,
            &source,
            limit,
        )
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn delete_queued(state: State<'_, AppState>, input: DeleteInput) -> Result<Value, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let queued = {
            let db = state.db.lock().map_err(|e| e.to_string())?;
            store::queued_assets(&db, &input.ids)?
        };
        let photos: Vec<(String, String, String)> =
            queued.iter().filter(|x| x.1 == "photos").cloned().collect();
        let usb: Vec<(String, String, String)> =
            queued.iter().filter(|x| x.1 == "usb").cloned().collect();
        let mut deleted = Vec::new();
        let mut failures = Vec::new();
        #[cfg(target_os = "macos")]
        {
            if !photos.is_empty() {
                let native_ids: Vec<String> = photos.iter().map(|x| x.2.clone()).collect();
                let raw = SRString::from(
                    serde_json::to_string(&native_ids)
                        .map_err(|e| e.to_string())?
                        .as_str(),
                );
                let result = wait_for_native_result(|| unsafe {
                    native_string(shiguang_photos_delete(&raw))
                })
                .unwrap_or_else(|error| json!({"ok":false,"error":error}));
                if result.get("ok") == Some(&Value::Bool(true)) {
                    let photo_ids = photos.iter().map(|x| x.0.clone()).collect::<Vec<_>>();
                    deleted.extend(photo_ids.iter().cloned());
                    store::mark_deleted(&*state.db.lock().map_err(|e| e.to_string())?, &photo_ids)
                        .map_err(|e| e.to_string())?;
                } else {
                    failures.push(format!(
                        "照片图库：{}",
                        result
                            .get("error")
                            .and_then(Value::as_str)
                            .unwrap_or("删除失败")
                    ));
                }
            }
            if !usb.is_empty() {
                let native_ids: Vec<String> = usb.iter().map(|x| x.2.clone()).collect();
                let raw = SRString::from(
                    serde_json::to_string(&native_ids)
                        .map_err(|e| e.to_string())?
                        .as_str(),
                );
                let result =
                    wait_for_native_result(|| unsafe { native_string(shiguang_usb_delete(&raw)) })
                        .unwrap_or_else(|error| json!({"ok":false,"deletedIDs":[],"error":error}));
                let deleted_ids = result
                    .get("deletedIDs")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect::<std::collections::HashSet<_>>();
                deleted.extend(
                    usb.iter()
                        .filter(|x| deleted_ids.contains(x.2.as_str()))
                        .map(|x| x.0.clone()),
                );
                let usb_deleted = usb
                    .iter()
                    .filter(|x| deleted_ids.contains(x.2.as_str()))
                    .map(|x| x.0.clone())
                    .collect::<Vec<_>>();
                store::mark_deleted(&*state.db.lock().map_err(|e| e.to_string())?, &usb_deleted)
                    .map_err(|e| e.to_string())?;
                if result.get("ok") != Some(&Value::Bool(true)) {
                    failures.push(format!(
                        "iPhone：{}",
                        result
                            .get("error")
                            .and_then(Value::as_str)
                            .unwrap_or("删除失败")
                    ));
                }
            }
        }
        #[cfg(not(target_os = "macos"))]
        return Err("当前平台不支持直接删除照片".into());
        let failed = queued.len().saturating_sub(deleted.len());
        let warning = (failed > 0).then(|| {
            let summary = format!("成功删除 {} 张，{} 张未能删除", deleted.len(), failed);
            if failures.is_empty() {
                summary
            } else {
                format!("{summary}。{}", failures.join("；"))
            }
        });
        Ok(json!({"deleted":deleted.len(),"failed":failed,"warning":warning}))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn app_status(state: State<'_, AppState>) -> Result<Value, String> {
    let model = state.model_state.lock().map_err(|e| e.to_string())?.clone();
    let scan = state.scan_state.lock().map_err(|e| e.to_string())?.clone();
    let db = state.db.lock().map_err(|e| e.to_string())?;
    Ok(
        json!({"modelState":model,"scan":scan,"counts":store::counts(&db).map_err(|e|e.to_string())?}),
    )
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            let path = database_path(&app.handle()).map_err(std::io::Error::other)?;
            let db = Connection::open(path).map_err(std::io::Error::other)?;
            store::initialize(&db).map_err(std::io::Error::other)?;
            app.manage(AppState {
                db: Arc::new(Mutex::new(db)),
                model: Arc::new(Mutex::new(None)),
                model_init: Arc::new(Mutex::new(())),
                model_state: Arc::new(Mutex::new("not_loaded".into())),
                scan_state: Arc::new(Mutex::new(ScanProgress::default())),
                index_state: Arc::new(Mutex::new(IndexProgress::default())),
                index_cancel: Arc::new(AtomicBool::new(false)),
                task_active: Arc::new(AtomicBool::new(false)),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            bootstrap,
            photos_status,
            photos_authorize,
            usb_status,
            list_assets,
            list_rules,
            load_thumbnail,
            load_original_image,
            start_indexing,
            stop_indexing,
            index_progress,
            save_rule,
            set_review_state,
            scan_rule,
            scan_progress,
            semantic_search,
            similar_photos,
            delete_queued,
            app_status
        ])
        .run(tauri::generate_context!())
        .expect("拾光启动失败");
}
