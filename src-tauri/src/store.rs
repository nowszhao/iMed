use super::{Asset, ReviewInput, Rule, RuleInput};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

const EMBEDDER_VERSION: &str = "embeddinggemma-2:440m:768:original-image-v2";
const THUMBNAIL_CACHE_BUDGET_BYTES: i64 = 128 * 1024 * 1024;
const SEARCH_Z_GATE: f64 = 2.0;
const SEARCH_DEVIATION_FLOOR: f64 = 0.02;

pub fn initialize(db: &Connection) -> rusqlite::Result<()> {
    db.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA foreign_keys=ON;
         CREATE TABLE IF NOT EXISTS assets (
           asset_key TEXT PRIMARY KEY, source TEXT NOT NULL, name TEXT NOT NULL,
           created_at TEXT NOT NULL DEFAULT '', width INTEGER NOT NULL DEFAULT 0,
           height INTEGER NOT NULL DEFAULT 0, favorite INTEGER NOT NULL DEFAULT 0,
           media_type TEXT NOT NULL DEFAULT 'image', review_state TEXT NOT NULL DEFAULT 'pending',
           queued INTEGER NOT NULL DEFAULT 0, thumbnail TEXT, thumbnail_cached_at TEXT,
           deleted_at TEXT
         );
         CREATE TABLE IF NOT EXISTS embeddings (
           asset_key TEXT NOT NULL, model_version TEXT NOT NULL, dimensions INTEGER NOT NULL,
           vector BLOB NOT NULL, updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
           PRIMARY KEY(asset_key, model_version), FOREIGN KEY(asset_key) REFERENCES assets(asset_key) ON DELETE CASCADE
         );
         CREATE TABLE IF NOT EXISTS index_failures (
           asset_key TEXT NOT NULL, model_version TEXT NOT NULL, error TEXT NOT NULL,
           updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
           PRIMARY KEY(asset_key, model_version),
           FOREIGN KEY(asset_key) REFERENCES assets(asset_key) ON DELETE CASCADE
         );
         CREATE TABLE IF NOT EXISTS source_syncs (
           source TEXT PRIMARY KEY, media_count INTEGER NOT NULL DEFAULT 0,
           synced_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
         );
         CREATE TABLE IF NOT EXISTS rules (
           id TEXT PRIMARY KEY, name TEXT NOT NULL, prompt TEXT NOT NULL, exclude_prompt TEXT NOT NULL DEFAULT '',
           threshold REAL NOT NULL DEFAULT 0.35, enabled INTEGER NOT NULL DEFAULT 1,
           created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
         );
         CREATE TABLE IF NOT EXISTS matches (
           rule_id TEXT NOT NULL, asset_key TEXT NOT NULL, score REAL NOT NULL,
           updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
           PRIMARY KEY(rule_id, asset_key),
           FOREIGN KEY(rule_id) REFERENCES rules(id) ON DELETE CASCADE,
           FOREIGN KEY(asset_key) REFERENCES assets(asset_key) ON DELETE CASCADE
         );
         CREATE INDEX IF NOT EXISTS idx_assets_date ON assets(created_at DESC);
         CREATE INDEX IF NOT EXISTS idx_assets_queue ON assets(queued);
         CREATE INDEX IF NOT EXISTS idx_matches_rule_score ON matches(rule_id, score DESC);"
    )?;
    let has_thumbnail_cache_time = {
        let mut columns = db.prepare("PRAGMA table_info(assets)")?;
        let names = columns
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        names.iter().any(|name| name == "thumbnail_cached_at")
    };
    if !has_thumbnail_cache_time {
        db.execute("ALTER TABLE assets ADD COLUMN thumbnail_cached_at TEXT", [])?;
    }
    db.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_assets_thumbnail_cache
         ON assets(thumbnail_cached_at) WHERE thumbnail IS NOT NULL;",
    )?;
    let count: i64 = db.query_row("SELECT count(*) FROM rules", [], |r| r.get(0))?;
    if count == 0 {
        for (id, name, prompt, exclude, threshold) in [
            (
                "blur",
                "严重模糊",
                "明显失焦、手抖拖影、主体细节模糊、无法看清的照片",
                "有意拍摄的动态模糊、柔焦艺术效果",
                0.34,
            ),
            (
                "exposure",
                "过暗 / 过曝",
                "曝光严重异常、画面过暗或过曝且细节丢失的照片",
                "夜景氛围、剪影、逆光艺术照片",
                0.34,
            ),
            (
                "blocked",
                "被遮挡 / 误拍",
                "被手指遮挡、口袋误拍、镜头被挡住的照片",
                "",
                0.34,
            ),
            (
                "eyes",
                "人物闭眼",
                "合影或人像照片中人物正在闭眼或眨眼",
                "单人闭眼肖像、舞台表演",
                0.36,
            ),
        ] {
            db.execute(
                "INSERT INTO rules(id,name,prompt,exclude_prompt,threshold) VALUES(?1,?2,?3,?4,?5)",
                params![id, name, prompt, exclude, threshold],
            )?;
        }
    }
    Ok(())
}

pub fn list_rules(db: &Connection) -> rusqlite::Result<Vec<Rule>> {
    let mut stmt = db.prepare(
        "SELECT r.id,r.name,r.prompt,r.exclude_prompt,r.threshold,r.enabled,
         (SELECT count(*) FROM matches m JOIN assets a USING(asset_key) WHERE m.rule_id=r.id AND m.score>=r.threshold AND a.queued=0 AND a.review_state='pending' AND a.deleted_at IS NULL)
         FROM rules r ORDER BY r.created_at, r.name"
    )?;
    stmt.query_map([], |row| {
        Ok(Rule {
            id: row.get(0)?,
            name: row.get(1)?,
            prompt: row.get(2)?,
            exclude_prompt: row.get(3)?,
            threshold: row.get(4)?,
            enabled: row.get::<_, i64>(5)? != 0,
            candidates: row.get::<_, i64>(6)? as usize,
        })
    })?
    .collect()
}

pub fn save_rule(db: &Connection, input: RuleInput) -> rusqlite::Result<Rule> {
    let id = input.id.unwrap_or_else(|| format!("rule-{}", uuid_v4()));
    db.execute("INSERT INTO rules(id,name,prompt,exclude_prompt,threshold,enabled,updated_at) VALUES(?1,?2,?3,?4,?5,?6,CURRENT_TIMESTAMP) ON CONFLICT(id) DO UPDATE SET name=excluded.name,prompt=excluded.prompt,exclude_prompt=excluded.exclude_prompt,threshold=excluded.threshold,enabled=excluded.enabled,updated_at=CURRENT_TIMESTAMP",
        params![id,input.name,input.prompt,input.exclude_prompt,input.threshold,input.enabled])?;
    get_rule(db, &id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)
}

fn uuid_v4() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:032x}", n)
}

pub fn get_rule(db: &Connection, id: &str) -> rusqlite::Result<Option<Rule>> {
    db.query_row("SELECT r.id,r.name,r.prompt,r.exclude_prompt,r.threshold,r.enabled,(SELECT count(*) FROM matches m JOIN assets a USING(asset_key) WHERE m.rule_id=r.id AND m.score>=r.threshold AND a.queued=0 AND a.review_state='pending' AND a.deleted_at IS NULL) FROM rules r WHERE r.id=?1", [id], |row| Ok(Rule {
        id: row.get(0)?, name: row.get(1)?, prompt: row.get(2)?, exclude_prompt: row.get(3)?,
        threshold: row.get(4)?, enabled: row.get::<_,i64>(5)? != 0, candidates: row.get::<_,i64>(6)? as usize,
    })).optional()
}

pub fn save_embedding(db: &Connection, asset: &Asset, vector: &[f32]) -> rusqlite::Result<()> {
    let key = format!("{}:{}", asset.source, asset.id);
    db.execute("INSERT INTO assets(asset_key,source,name,created_at,width,height,favorite,media_type) VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(asset_key) DO UPDATE SET name=excluded.name,width=excluded.width,height=excluded.height,favorite=excluded.favorite,media_type=excluded.media_type",
        params![key,asset.source,asset.name,asset.date,asset.width,asset.height,asset.favorite,asset.media_type])?;
    let bytes: Vec<u8> = vector.iter().flat_map(|v| v.to_le_bytes()).collect();
    db.execute("INSERT INTO embeddings(asset_key,model_version,dimensions,vector) VALUES(?1,?2,?3,?4) ON CONFLICT(asset_key,model_version) DO UPDATE SET dimensions=excluded.dimensions,vector=excluded.vector,updated_at=CURRENT_TIMESTAMP", params![key,EMBEDDER_VERSION,vector.len() as i64,bytes])?;
    db.execute("DELETE FROM index_failures WHERE asset_key=?1 AND model_version=?2", params![key, EMBEDDER_VERSION])?;
    Ok(())
}

pub fn mark_index_failure(db: &Connection, asset: &Asset, error: &str) -> rusqlite::Result<()> {
    let key = format!("{}:{}", asset.source, asset.id);
    db.execute("INSERT INTO index_failures(asset_key,model_version,error,updated_at) VALUES(?1,?2,?3,CURRENT_TIMESTAMP) ON CONFLICT(asset_key,model_version) DO UPDATE SET error=excluded.error,updated_at=CURRENT_TIMESTAMP", params![key, EMBEDDER_VERSION, error])?;
    Ok(())
}

/// Clears the "fully synced" marker so a running or failed sync is never
/// reported as complete.
pub fn invalidate_source_sync(db: &Connection, source: &str) -> rusqlite::Result<()> {
    db.execute("DELETE FROM source_syncs WHERE source=?1", [source])?;
    Ok(())
}

/// Commits one complete listing for a source: upserts every asset, drops rows
/// that disappeared from the source, then records the source as fully synced.
/// Media the user already queued for deletion is kept so a pending batch is
/// never silently destroyed.
pub fn finish_source_sync(db: &Connection, source: &str, assets: &[Asset]) -> rusqlite::Result<()> {
    if !matches!(source, "photos" | "usb") || assets.iter().any(|asset| asset.source != source) {
        return Err(rusqlite::Error::InvalidParameterName(
            "source sync contains assets from another source".into(),
        ));
    }
    let tx = db.unchecked_transaction()?;
    tx.execute_batch("CREATE TEMP TABLE IF NOT EXISTS sync_keys(key TEXT PRIMARY KEY); DELETE FROM sync_keys;")?;
    for asset in assets {
        let key = format!("{}:{}", asset.source, asset.id);
        tx.execute(
            "INSERT INTO assets(asset_key,source,name,created_at,width,height,favorite,media_type) VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(asset_key) DO UPDATE SET name=excluded.name,created_at=excluded.created_at,width=excluded.width,height=excluded.height,favorite=excluded.favorite,media_type=excluded.media_type",
            params![&key, asset.source, asset.name, asset.date, asset.width, asset.height, asset.favorite, asset.media_type],
        )?;
        tx.execute("INSERT OR REPLACE INTO sync_keys(key) VALUES(?1)", [&key])?;
    }
    tx.execute(
        "DELETE FROM assets WHERE source=?1 AND queued=0 AND asset_key NOT IN (SELECT key FROM sync_keys)",
        [source],
    )?;
    tx.execute_batch("DELETE FROM sync_keys;")?;
    tx.execute(
        "INSERT INTO source_syncs(source,media_count,synced_at) VALUES(?1,?2,CURRENT_TIMESTAMP) ON CONFLICT(source) DO UPDATE SET media_count=excluded.media_count,synced_at=CURRENT_TIMESTAMP",
        params![source, assets.len() as i64],
    )?;
    tx.commit()
}

pub fn source_is_current(db: &Connection, source: &str, current_count: i64) -> rusqlite::Result<bool> {
    let synced_count = db.query_row("SELECT media_count FROM source_syncs WHERE source=?1", [source], |row| row.get::<_, i64>(0)).optional()?;
    Ok(synced_count == Some(current_count))
}

fn asset_index_status(db: &Connection, key: &str) -> rusqlite::Result<String> {
    let indexed: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM embeddings WHERE asset_key=?1 AND model_version=?2)", params![key, EMBEDDER_VERSION], |r| r.get(0))?;
    if indexed { return Ok("indexed".into()); }
    let failed: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM index_failures WHERE asset_key=?1 AND model_version=?2)", params![key, EMBEDDER_VERSION], |r| r.get(0))?;
    Ok(if failed { "failed" } else { "pending" }.into())
}

pub fn cached_embedding(db: &Connection, asset: &Asset) -> Result<Option<Vec<f32>>, String> {
    let key = format!("{}:{}", asset.source, asset.id);
    let stored = db
        .query_row(
            "SELECT dimensions,vector FROM embeddings WHERE asset_key=?1 AND model_version=?2",
            params![key, EMBEDDER_VERSION],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    let Some((dimensions, bytes)) = stored else {
        return Ok(None);
    };
    if dimensions < 1 || bytes.len() != dimensions as usize * 4 {
        return Ok(None);
    }
    let vector = bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect();
    Ok(Some(vector))
}

pub fn replace_rule_matches(
    db: &Connection,
    rule_id: &str,
    sources: &[String],
    matches: &[(Asset, f32)],
    threshold: f32,
) -> rusqlite::Result<()> {
    let tx = db.unchecked_transaction()?;
    let mut unique_sources = std::collections::HashSet::<String>::new();
    for source in sources {
        if !matches!(source.as_str(), "photos" | "usb") || !unique_sources.insert(source.clone()) {
            continue;
        }
        tx.execute(
            "DELETE FROM matches WHERE rule_id=?1 AND asset_key IN (SELECT asset_key FROM assets WHERE source=?2)",
            params![rule_id, source],
        )?;
    }
    for (asset, score) in matches {
        if *score < threshold || !unique_sources.contains(&asset.source) {
            continue;
        }
        let key = format!("{}:{}", asset.source, asset.id);
        tx.execute(
            "INSERT INTO matches(rule_id,asset_key,score) VALUES(?1,?2,?3) ON CONFLICT(rule_id,asset_key) DO UPDATE SET score=excluded.score,updated_at=CURRENT_TIMESTAMP",
            params![rule_id, key, score],
        )?;
    }
    tx.commit()
}

pub fn rule_match_count(db: &Connection, id: &str) -> rusqlite::Result<usize> {
    db.query_row(
        "SELECT count(*) FROM matches m
         JOIN rules r ON r.id=m.rule_id
         JOIN assets a USING(asset_key)
         WHERE m.rule_id=?1 AND m.score>=r.threshold
           AND a.queued=0 AND a.review_state='pending' AND a.deleted_at IS NULL",
        [id],
        |row| row.get::<_, i64>(0),
    )
    .map(|x| x as usize)
}

pub fn decorate_asset(db: &Connection, asset: &mut Asset) -> Result<(), String> {
    let key = format!("{}:{}", asset.source, asset.id);
    let state: (String, bool, Option<f32>, Option<String>) = db.query_row(
        "SELECT a.review_state,a.queued,(SELECT max(m.score) FROM matches m WHERE m.asset_key=a.asset_key), (SELECT m.rule_id FROM matches m WHERE m.asset_key=a.asset_key ORDER BY m.score DESC LIMIT 1) FROM assets a WHERE a.asset_key=?1",
        [&key], |r| Ok((r.get(0)?,r.get::<_,i64>(1)? != 0,r.get(2)?,r.get(3)?))
    ).map_err(|e| e.to_string())?;
    asset.reviewed = state.0;
    asset.queued = state.1;
    asset.score = state.2;
    asset.rule_id = state.3;
    asset.index_status = asset_index_status(db, &key).map_err(|e| e.to_string())?;
    Ok(())
}

pub fn set_review(db: &Connection, input: ReviewInput) -> rusqlite::Result<()> {
    let state = match input.action.as_str() {
        "keep" => "kept",
        "later" => "later",
        "pending" => "pending",
        "queue" => "pending",
        _ => {
            return Err(rusqlite::Error::InvalidParameterName(
                "unknown review action".into(),
            ));
        }
    };
    let tx = db.unchecked_transaction()?;
    for id in input.ids {
        if input.action == "queue" {
            tx.execute("UPDATE assets SET review_state=?2,queued=1 WHERE asset_key=?1 AND deleted_at IS NULL", params![id,state])?;
        } else if input.action == "pending" {
            tx.execute("UPDATE assets SET review_state='pending',queued=0 WHERE asset_key=?1 AND deleted_at IS NULL", [&id])?;
        } else {
            tx.execute("UPDATE assets SET review_state=?2,queued=0 WHERE asset_key=?1 AND deleted_at IS NULL", params![id,state])?;
        }
    }
    tx.commit()
}

pub fn counts(db: &Connection) -> rusqlite::Result<Value> {
    let photos: i64 = db.query_row(
        "SELECT count(*) FROM assets WHERE source='photos' AND deleted_at IS NULL AND media_type='image'",
        [],
        |r| r.get(0),
    )?;
    let usb: i64 = db.query_row(
        "SELECT count(*) FROM assets WHERE source='usb' AND deleted_at IS NULL AND media_type='image'",
        [],
        |r| r.get(0),
    )?;
    let queued: i64 = db.query_row(
        "SELECT count(*) FROM assets WHERE queued=1 AND deleted_at IS NULL",
        [],
        |r| r.get(0),
    )?;
    let candidates: i64 = db.query_row("SELECT count(DISTINCT m.asset_key) FROM matches m JOIN rules r ON r.id=m.rule_id JOIN assets a USING(asset_key) WHERE m.score>=r.threshold AND a.review_state='pending' AND a.queued=0 AND a.deleted_at IS NULL",[],|r|r.get(0))?;
    let indexed: i64 = db.query_row("SELECT count(*) FROM embeddings e JOIN assets a USING(asset_key) WHERE e.model_version=?1 AND a.deleted_at IS NULL AND a.media_type='image'", [EMBEDDER_VERSION], |r| r.get(0))?;
    let indexed_photos: i64 = db.query_row("SELECT count(*) FROM embeddings e JOIN assets a USING(asset_key) WHERE e.model_version=?1 AND a.source='photos' AND a.deleted_at IS NULL AND a.media_type='image'", [EMBEDDER_VERSION], |r| r.get(0))?;
    let indexed_usb: i64 = db.query_row("SELECT count(*) FROM embeddings e JOIN assets a USING(asset_key) WHERE e.model_version=?1 AND a.source='usb' AND a.deleted_at IS NULL AND a.media_type='image'", [EMBEDDER_VERSION], |r| r.get(0))?;
    let failed: i64 = db.query_row("SELECT count(*) FROM index_failures f JOIN assets a USING(asset_key) WHERE f.model_version=?1 AND a.deleted_at IS NULL AND a.media_type='image' AND NOT EXISTS (SELECT 1 FROM embeddings e WHERE e.asset_key=f.asset_key AND e.model_version=f.model_version)", [EMBEDDER_VERSION], |r| r.get(0))?;
    let failed_photos: i64 = db.query_row("SELECT count(*) FROM index_failures f JOIN assets a USING(asset_key) WHERE f.model_version=?1 AND a.source='photos' AND a.deleted_at IS NULL AND a.media_type='image' AND NOT EXISTS (SELECT 1 FROM embeddings e WHERE e.asset_key=f.asset_key AND e.model_version=f.model_version)", [EMBEDDER_VERSION], |r| r.get(0))?;
    let failed_usb: i64 = db.query_row("SELECT count(*) FROM index_failures f JOIN assets a USING(asset_key) WHERE f.model_version=?1 AND a.source='usb' AND a.deleted_at IS NULL AND a.media_type='image' AND NOT EXISTS (SELECT 1 FROM embeddings e WHERE e.asset_key=f.asset_key AND e.model_version=f.model_version)", [EMBEDDER_VERSION], |r| r.get(0))?;
    let known_images: i64 = db.query_row("SELECT count(*) FROM assets WHERE deleted_at IS NULL AND media_type='image'", [], |r| r.get(0))?;
    let photos_videos: i64 = db.query_row("SELECT count(*) FROM assets WHERE source='photos' AND deleted_at IS NULL AND media_type='video'", [], |r| r.get(0))?;
    let usb_videos: i64 = db.query_row("SELECT count(*) FROM assets WHERE source='usb' AND deleted_at IS NULL AND media_type='video'", [], |r| r.get(0))?;
    let unsupported: i64 = photos_videos + usb_videos;
    Ok(json!({"photos":photos,"usb":usb,"photosVideos":photos_videos,"usbVideos":usb_videos,"unsupported":unsupported,"queued":queued,"candidates":candidates,"indexed":indexed,"indexedPhotos":indexed_photos,"indexedUsb":indexed_usb,"failed":failed,"failedPhotos":failed_photos,"failedUsb":failed_usb,"unindexed":(known_images-indexed-failed).max(0)}))
}

pub fn list_library(
    db: &Connection,
    source: &str,
    _media_type: Option<&str>,
    index_status: Option<&str>,
    favorites_only: bool,
    offset: usize,
    limit: usize,
) -> Result<(Vec<Asset>, usize), String> {
    let source = if matches!(source, "photos" | "usb") { source } else { "all" };
    let media_type = _media_type.filter(|value| matches!(*value, "image" | "video" | "all")).unwrap_or("all");
    let index_status = index_status.filter(|value| matches!(*value, "all" | "indexed" | "pending" | "failed" | "unsupported")).unwrap_or("all");
    let where_clause = "a.deleted_at IS NULL AND (?1='all' OR a.source=?1) AND (?2='all' OR a.media_type=?2) AND (?3='all' OR (?3='indexed' AND a.media_type='image' AND EXISTS(SELECT 1 FROM embeddings e WHERE e.asset_key=a.asset_key AND e.model_version=?4)) OR (?3='failed' AND a.media_type='image' AND EXISTS(SELECT 1 FROM index_failures f WHERE f.asset_key=a.asset_key AND f.model_version=?4) AND NOT EXISTS(SELECT 1 FROM embeddings e WHERE e.asset_key=a.asset_key AND e.model_version=?4)) OR (?3='pending' AND a.media_type='image' AND NOT EXISTS(SELECT 1 FROM embeddings e WHERE e.asset_key=a.asset_key AND e.model_version=?4) AND NOT EXISTS(SELECT 1 FROM index_failures f WHERE f.asset_key=a.asset_key AND f.model_version=?4)) OR (?3='unsupported' AND a.media_type='video'))";
    let total: i64 = db.query_row(&format!("SELECT count(*) FROM assets a WHERE {where_clause} AND (?5=0 OR a.favorite=1)"), params![source, media_type, index_status, EMBEDDER_VERSION, favorites_only as i64], |row| row.get(0)).map_err(|e|e.to_string())?;
    let mut stmt = db.prepare(&format!("SELECT a.asset_key,a.source,a.name,a.created_at,a.width,a.height,a.favorite,a.media_type,a.thumbnail,a.review_state,a.queued,(SELECT max(m.score) FROM matches m WHERE m.asset_key=a.asset_key),(SELECT m.rule_id FROM matches m WHERE m.asset_key=a.asset_key ORDER BY m.score DESC LIMIT 1),CASE WHEN a.media_type='video' THEN 'unsupported' WHEN EXISTS(SELECT 1 FROM embeddings e WHERE e.asset_key=a.asset_key AND e.model_version=?4) THEN 'indexed' WHEN EXISTS(SELECT 1 FROM index_failures f WHERE f.asset_key=a.asset_key AND f.model_version=?4) THEN 'failed' ELSE 'pending' END FROM assets a WHERE {where_clause} AND (?5=0 OR a.favorite=1) ORDER BY a.created_at DESC,a.asset_key LIMIT ?6 OFFSET ?7")).map_err(|e|e.to_string())?;
    let rows = stmt.query_map(params![source,media_type,index_status,EMBEDDER_VERSION,favorites_only as i64,limit.clamp(1,200) as i64,offset as i64], |row| {
        let key: String = row.get(0)?;
        Ok(Asset {
            id: key.split_once(':').map(|(_, id)| id).unwrap_or(&key).to_owned(),
            source: row.get(1)?, name: row.get(2)?, date: row.get(3)?, width: row.get(4)?, height: row.get(5)?, favorite: row.get(6)?, media_type: row.get(7)?, thumbnail: row.get(8)?, reviewed: row.get(9)?, queued: row.get(10)?, score: row.get(11)?, rule_id: row.get(12)?, index_status: row.get(13)?,
        })
    }).map_err(|e|e.to_string())?;
    let assets = rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|e|e.to_string())?;
    Ok((assets, total.max(0) as usize))
}

pub fn cached_thumbnail(db: &Connection, key: &str) -> Result<Option<String>, String> {
    db.execute(
        "UPDATE assets SET thumbnail_cached_at=strftime('%Y-%m-%dT%H:%M:%fZ','now')
         WHERE asset_key=?1 AND thumbnail IS NOT NULL",
        [key],
    )
    .map_err(|e| e.to_string())?;
    db.query_row(
        "SELECT thumbnail FROM assets WHERE asset_key=?1",
        [key],
        |r| r.get::<_, Option<String>>(0),
    )
    .optional()
    .map(Option::flatten)
    .map_err(|e| e.to_string())
}

pub fn save_thumbnail(db: &Connection, key: &str, data: &str) -> Result<(), String> {
    db.execute(
        "UPDATE assets SET thumbnail=?2,thumbnail_cached_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE asset_key=?1",
        params![key, data],
    )
    .map_err(|e| e.to_string())?;
    db.execute(
        "WITH ordered AS (
           SELECT asset_key,
                  sum(length(thumbnail)) OVER (
                    ORDER BY thumbnail_cached_at DESC,rowid DESC
                    ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
                  ) AS cached_bytes
           FROM assets WHERE thumbnail IS NOT NULL
         )
         UPDATE assets SET thumbnail=NULL,thumbnail_cached_at=NULL
         WHERE asset_key IN (SELECT asset_key FROM ordered WHERE cached_bytes>?1)",
        [THUMBNAIL_CACHE_BUDGET_BYTES],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn semantic_search(
    db: &Connection,
    query: &[f32],
    query_text: &str,
    limit: usize,
    source: &str,
    include_weak: bool,
) -> Result<Vec<Value>, String> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    if !valid_search_vector(query) {
        return Err("搜索向量无效，请重试或重新加载模型".into());
    }
    let query_text = query_text.trim();
    let source = if source == "photos" || source == "usb" {
        source
    } else {
        "all"
    };
    let mut stmt = db.prepare("SELECT a.asset_key,a.source,a.name,a.created_at,a.width,a.height,a.favorite,a.review_state,a.queued,a.thumbnail,e.vector,e.dimensions FROM embeddings e JOIN assets a USING(asset_key) WHERE e.model_version=?1 AND a.deleted_at IS NULL AND (?2='all' OR a.source=?2)").map_err(|e|e.to_string())?;
    let rows = stmt
        .query_map(params![EMBEDDER_VERSION, source], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i32>(4)?,
                r.get::<_, i32>(5)?,
                r.get::<_, bool>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, bool>(8)?,
                r.get::<_, Option<String>>(9)?,
                r.get::<_, Vec<u8>>(10)?,
                r.get::<_, i64>(11)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    let mut found = Vec::new();
    for row in rows {
        let (
            key,
            source,
            name,
            date,
            width,
            height,
            favorite,
            reviewed,
            queued,
            thumbnail,
            bytes,
            dimensions,
        ) = row.map_err(|e| e.to_string())?;
        if dimensions as usize != query.len() || bytes.len() != query.len() * 4 {
            continue;
        }
        let vector = bytes
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect::<Vec<_>>();
        if !valid_search_vector(&vector) {
            continue;
        }
        let score = super::cosine(query, &vector);
        if !score.is_finite() {
            continue;
        }
        let exact_name = normalized_exact_name_match(&name, query_text);
        found.push((score, exact_name, json!({"id":key.split_once(':').map(|x|x.1).unwrap_or(&key),"source":source,"name":name,"date":date,"width":width,"height":height,"favorite":favorite,"reviewed":reviewed,"queued":queued,"thumbnail":thumbnail,"score":score,"key":key})));
    }
    // As in DigUp's Searcher.meaning/zGate: a nearest neighbour is not
    // necessarily a match. Calibrate against this query's score distribution
    // before taking top-k; never fill the limit with background results.
    // All our vectors are images, so no per-modality centering is needed.
    let count = found.len().max(1) as f64;
    let mean = found.iter().map(|(score, _, _)| *score as f64).sum::<f64>() / count;
    let variance = found
        .iter()
        .map(|(score, _, _)| (*score as f64 - mean).powi(2))
        .sum::<f64>()
        / count;
    let deviation = variance.sqrt().max(SEARCH_DEVIATION_FLOOR);
    for (score, exact_name, row) in &mut found {
        let relevance = (*score as f64 - mean) / deviation;
        row["weakMatch"] = json!(!*exact_name && (*score <= 0.0 || relevance < SEARCH_Z_GATE));
    }
    found.retain(|(_, _, row)| include_weak || row["weakMatch"] == false);
    found.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then_with(|| a.2["key"].as_str().cmp(&b.2["key"].as_str()))
    });
    Ok(found
        .into_iter()
        .take(limit.clamp(1, 500))
        .map(|(_, _, v)| v)
        .collect())
}

/// True when every word of the query matches a whole word of the file name,
/// so "hair" hits "hair.jpg" but not "hairbrush.jpg". Whole-word matching keeps
/// a common token such as "IMG" from rescuing an entire library.
fn normalized_exact_name_match(name: &str, query: &str) -> bool {
    let words = |value: &str| -> Vec<String> {
        value
            .chars()
            .flat_map(char::to_lowercase)
            .map(|character| if character.is_alphanumeric() { character } else { ' ' })
            .collect::<String>()
            .split_whitespace()
            .map(str::to_owned)
            .collect()
    };
    let query = words(query);
    if query.is_empty() {
        return false;
    }
    let name = words(name);
    query.iter().all(|part| name.iter().any(|word| word == part))
}

fn valid_search_vector(vector: &[f32]) -> bool {
    !vector.is_empty() && vector.iter().all(|v| v.is_finite()) && vector.iter().any(|v| *v != 0.0)
}

pub fn similar_assets(
    db: &Connection,
    id: &str,
    anchor_source: &str,
    source: &str,
    limit: usize,
) -> Result<Vec<Value>, String> {
    let anchor_key = format!("{anchor_source}:{id}");
    let (anchor_dimensions, anchor_bytes): (i64, Vec<u8>) = db
        .query_row(
            "SELECT e.dimensions,e.vector FROM embeddings e JOIN assets a USING(asset_key)
             WHERE e.asset_key=?1 AND e.model_version=?2 AND a.deleted_at IS NULL",
            params![anchor_key, EMBEDDER_VERSION],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "这张照片还没有向量索引，请先扫描它所在的图库".to_string())?;
    if anchor_dimensions < 1 || anchor_bytes.len() != anchor_dimensions as usize * 4 {
        return Err("这张照片的向量索引无效，请重新扫描图库".into());
    }
    let anchor = anchor_bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect::<Vec<_>>();
    let source = if matches!(source, "photos" | "usb") {
        source
    } else {
        "all"
    };
    let mut stmt = db
        .prepare(
            "SELECT a.asset_key,a.source,a.name,a.created_at,a.width,a.height,a.favorite,
                    a.review_state,a.queued,a.thumbnail,e.vector,e.dimensions
             FROM embeddings e JOIN assets a USING(asset_key)
             WHERE e.model_version=?1 AND a.deleted_at IS NULL AND a.asset_key<>?2
               AND (?3='all' OR a.source=?3)",
        )
        .map_err(|error| error.to_string())?;
    let rows = stmt
        .query_map(params![EMBEDDER_VERSION, anchor_key, source], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i32>(4)?,
                row.get::<_, i32>(5)?,
                row.get::<_, bool>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, bool>(8)?,
                row.get::<_, Option<String>>(9)?,
                row.get::<_, Vec<u8>>(10)?,
                row.get::<_, i64>(11)?,
            ))
        })
        .map_err(|error| error.to_string())?;
    let mut found = Vec::new();
    for row in rows {
        let (
            key,
            asset_source,
            name,
            date,
            width,
            height,
            favorite,
            reviewed,
            queued,
            thumbnail,
            bytes,
            dimensions,
        ) = row.map_err(|error| error.to_string())?;
        if dimensions != anchor_dimensions || bytes.len() != anchor.len() * 4 {
            continue;
        }
        let vector = bytes
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .collect::<Vec<_>>();
        let score = super::cosine(&anchor, &vector);
        found.push((
            score,
            json!({
                "id": key.split_once(':').map(|(_, id)| id).unwrap_or(&key),
                "source": asset_source,
                "name": name,
                "date": date,
                "width": width,
                "height": height,
                "favorite": favorite,
                "reviewed": reviewed,
                "queued": queued,
                "thumbnail": thumbnail,
                "score": score,
                "rule_id": Value::Null,
                "media_type": "image",
                "key": key
            }),
        ));
    }
    found.sort_by(|left, right| right.0.total_cmp(&left.0));
    Ok(found
        .into_iter()
        .take(limit.clamp(1, 500))
        .map(|(_, asset)| asset)
        .collect())
}

pub fn embedding_count(db: &Connection, source: &str) -> rusqlite::Result<usize> {
    let source = if source == "photos" || source == "usb" {
        source
    } else {
        "all"
    };
    db.query_row(
        "SELECT count(*) FROM embeddings e JOIN assets a USING(asset_key) WHERE e.model_version=?1 AND a.deleted_at IS NULL AND (?2='all' OR a.source=?2)",
        params![EMBEDDER_VERSION, source],
        |row| row.get::<_, i64>(0),
    ).map(|count| count as usize)
}

pub fn assets_for_index(
    db: &Connection,
    source: &str,
    start_date: &str,
    end_date_exclusive: &str,
) -> Result<Vec<Asset>, String> {
    let source = if source == "photos" || source == "usb" { source } else { "all" };
    let mut stmt = db.prepare(
        "SELECT a.asset_key,a.source,a.name,a.created_at,a.width,a.height,a.favorite,a.media_type,
                a.thumbnail,a.review_state,a.queued,
                (SELECT max(m.score) FROM matches m WHERE m.asset_key=a.asset_key),
                (SELECT m.rule_id FROM matches m WHERE m.asset_key=a.asset_key ORDER BY m.score DESC LIMIT 1)
         FROM assets a
         WHERE a.deleted_at IS NULL AND a.media_type='image'
           AND a.created_at>=?1 AND a.created_at<?2
           AND (?3='all' OR a.source=?3)
           AND NOT EXISTS (SELECT 1 FROM embeddings e WHERE e.asset_key=a.asset_key AND e.model_version=?4)
         ORDER BY a.created_at,a.asset_key",
    ).map_err(|e| e.to_string())?;
    stmt.query_map(params![start_date, end_date_exclusive, source, EMBEDDER_VERSION], |row| {
        let key: String = row.get(0)?;
        Ok(Asset {
            id: key.split_once(':').map(|(_, id)| id).unwrap_or(&key).to_owned(),
            source: row.get(1)?,
            name: row.get(2)?,
            date: row.get(3)?,
            width: row.get(4)?,
            height: row.get(5)?,
            favorite: row.get(6)?,
            media_type: row.get(7)?,
            thumbnail: row.get(8)?,
            reviewed: row.get(9)?,
            queued: row.get::<_, i64>(10)? != 0,
            score: row.get(11)?,
            rule_id: row.get(12)?,
            index_status: "pending".into(),
        })
    }).map_err(|e| e.to_string())?.collect::<rusqlite::Result<Vec<_>>>().map_err(|e| e.to_string())
}

pub fn list_queued(
    db: &Connection,
    source: &str,
    offset: usize,
    limit: usize,
) -> Result<(Vec<Asset>, usize), String> {
    let source = if source == "photos" || source == "usb" {
        source
    } else {
        "all"
    };
    let total: usize = db.query_row(
        "SELECT count(*) FROM assets WHERE queued=1 AND deleted_at IS NULL AND (?1='all' OR source=?1)",
        [source], |row| row.get::<_, i64>(0)
    ).map_err(|e| e.to_string())? as usize;
    let mut stmt = db.prepare(
        "SELECT a.asset_key,a.source,a.name,a.created_at,a.width,a.height,a.favorite,a.media_type,
         a.review_state,a.queued,a.thumbnail,
         (SELECT m.score FROM matches m WHERE m.asset_key=a.asset_key ORDER BY m.score DESC LIMIT 1),
         (SELECT m.rule_id FROM matches m WHERE m.asset_key=a.asset_key ORDER BY m.score DESC LIMIT 1)
         FROM assets a WHERE a.queued=1 AND a.deleted_at IS NULL AND (?1='all' OR a.source=?1)
         ORDER BY a.created_at DESC LIMIT ?2 OFFSET ?3"
    ).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(
            params![source, limit.clamp(1, 200) as i64, offset as i64],
            |row| {
                let key: String = row.get(0)?;
                let source: String = row.get(1)?;
                let id = key
                    .split_once(':')
                    .map(|(_, id)| id)
                    .unwrap_or(&key)
                    .to_string();
                Ok(Asset {
                    id,
                    source,
                    name: row.get(2)?,
                    date: row.get(3)?,
                    width: row.get(4)?,
                    height: row.get(5)?,
                    favorite: row.get::<_, i64>(6)? != 0,
                    media_type: row.get(7)?,
                    reviewed: row.get(8)?,
                    queued: row.get::<_, i64>(9)? != 0,
                    thumbnail: row.get(10)?,
                    score: row.get(11)?,
                    rule_id: row.get(12)?,
                    index_status: "pending".into(),
                })
            },
        )
        .map_err(|e| e.to_string())?;
    let assets = rows
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| e.to_string())?;
    Ok((assets, total))
}

pub fn list_rule_matches(
    db: &Connection,
    rule_id: &str,
    source: &str,
    review_state: Option<&str>,
    offset: usize,
    limit: usize,
) -> Result<(Vec<Asset>, usize, usize, usize), String> {
    let source = if source == "photos" || source == "usb" {
        source
    } else {
        "all"
    };
    let (all_total, pending, later): (i64, i64, i64) = db
        .query_row(
            "SELECT coalesce(sum(CASE WHEN a.queued=0 AND a.review_state IN ('pending','later') THEN 1 ELSE 0 END),0),
                coalesce(sum(CASE WHEN a.queued=0 AND a.review_state='pending' THEN 1 ELSE 0 END),0),
                coalesce(sum(CASE WHEN a.queued=0 AND a.review_state='later' THEN 1 ELSE 0 END),0)
         FROM matches m JOIN assets a USING(asset_key) JOIN rules r ON r.id=m.rule_id
         WHERE m.rule_id=?1 AND m.score>=r.threshold AND a.deleted_at IS NULL
           AND (?2='all' OR a.source=?2)",
            params![rule_id, source],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|e| e.to_string())?;
    let total = match review_state {
        Some("pending") => pending,
        Some("later") => later,
        _ => all_total,
    };
    let mut stmt = db.prepare(
        "SELECT a.asset_key,a.source,a.name,a.created_at,a.width,a.height,a.favorite,a.media_type,
         a.review_state,a.queued,a.thumbnail,m.score
         FROM matches m JOIN assets a USING(asset_key)
         JOIN rules r ON r.id=m.rule_id
         WHERE m.rule_id=?1 AND m.score>=r.threshold AND a.queued=0
           AND a.review_state IN ('pending','later') AND a.deleted_at IS NULL
           AND (?2='all' OR a.source=?2) AND (?3 IS NULL OR a.review_state=?3)
         ORDER BY m.score DESC,a.created_at DESC LIMIT ?4 OFFSET ?5"
    ).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(
            params![
                rule_id,
                source,
                review_state,
                limit.clamp(1, 200) as i64,
                offset as i64
            ],
            |row| {
                let key: String = row.get(0)?;
                let source: String = row.get(1)?;
                let id = key
                    .split_once(':')
                    .map(|(_, id)| id)
                    .unwrap_or(&key)
                    .to_string();
                Ok(Asset {
                    id,
                    source,
                    name: row.get(2)?,
                    date: row.get(3)?,
                    width: row.get(4)?,
                    height: row.get(5)?,
                    favorite: row.get::<_, i64>(6)? != 0,
                    media_type: row.get(7)?,
                    reviewed: row.get(8)?,
                    queued: row.get::<_, i64>(9)? != 0,
                    thumbnail: row.get(10)?,
                    score: row.get(11)?,
                    rule_id: Some(rule_id.to_string()),
                    index_status: "pending".into(),
                })
            },
        )
        .map_err(|e| e.to_string())?;
    let assets = rows
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| e.to_string())?;
    Ok((assets, total as usize, pending as usize, later as usize))
}

pub fn queued_assets(
    db: &Connection,
    ids: &[String],
) -> Result<Vec<(String, String, String)>, String> {
    if ids.is_empty() {
        return Err("待删除列表为空".into());
    }
    let mut out = Vec::new();
    let mut unique = std::collections::HashSet::new();
    for id in ids {
        if !unique.insert(id) {
            continue;
        }
        let row = db.query_row(
            "SELECT source,substr(asset_key,instr(asset_key,':')+1) FROM assets WHERE asset_key=?1 AND queued=1 AND deleted_at IS NULL",
            [id], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))
        ).optional().map_err(|e| e.to_string())?;
        let Some((source, native_id)) = row else {
            return Err(format!("照片不在待删除列表中：{id}"));
        };
        out.push((id.clone(), source, native_id));
    }
    if out.is_empty() {
        return Err("待删除列表为空".into());
    }
    Ok(out)
}

pub fn mark_deleted(db: &Connection, ids: &[String]) -> rusqlite::Result<()> {
    let tx = db.unchecked_transaction()?;
    for id in ids {
        tx.execute("UPDATE assets SET queued=0,review_state='deleted',deleted_at=CURRENT_TIMESTAMP WHERE asset_key=?1 AND queued=1",[id])?;
    }
    tx.commit()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_asset(source: &str, id: &str, media_type: &str) -> Asset {
        Asset {
            id: id.into(),
            source: source.into(),
            name: format!("{id}.{}", if media_type == "video" { "mov" } else { "jpg" }),
            date: String::new(),
            width: 800,
            height: 600,
            favorite: false,
            media_type: media_type.into(),
            thumbnail: None,
            score: None,
            reviewed: "pending".into(),
            queued: false,
            rule_id: None,
            index_status: "pending".into(),
        }
    }

    fn indexed_photo(db: &Connection, id: &str, score: f32) {
        let asset = test_asset("photos", id, "image");
        // Controlled cosine scores isolate retrieval from model inference.
        save_embedding(db, &asset, &[score, (1.0 - score * score).sqrt()]).unwrap();
    }

    #[test]
    fn source_sync_reconciles_stale_assets_and_preserves_queued_items() {
        let db = Connection::open_in_memory().unwrap();
        initialize(&db).unwrap();
        let retained = test_asset("photos", "retained", "image");
        let stale = test_asset("photos", "stale", "image");
        let queued = test_asset("photos", "queued", "image");
        save_embedding(&db, &stale, &[1.0, 0.0]).unwrap();
        finish_source_sync(&db, "photos", &[retained.clone(), stale.clone(), queued.clone()]).unwrap();
        set_review(
            &db,
            ReviewInput {
                ids: vec!["photos:queued".into()],
                action: "queue".into(),
            },
        )
        .unwrap();

        let updated = test_asset("photos", "retained", "image");
        finish_source_sync(&db, "photos", &[updated, test_asset("photos", "new", "video")]).unwrap();

        assert!(source_is_current(&db, "photos", 2).unwrap());
        let (rows, total) = list_library(&db, "photos", None, None, false, 0, 20).unwrap();
        assert_eq!(total, 3, "the queued item stays available for review");
        assert!(rows.iter().any(|asset| asset.id == "queued" && asset.queued));
        assert!(rows.iter().any(|asset| asset.id == "new" && asset.media_type == "video"));
        assert!(!rows.iter().any(|asset| asset.id == "stale"));
        assert_eq!(embedding_count(&db, "all").unwrap(), 0, "stale vectors cascade with removed assets");
    }

    #[test]
    fn invalidated_source_is_not_reported_as_fully_synced() {
        let db = Connection::open_in_memory().unwrap();
        initialize(&db).unwrap();
        finish_source_sync(&db, "photos", &[test_asset("photos", "one", "image")]).unwrap();
        assert!(source_is_current(&db, "photos", 1).unwrap());

        invalidate_source_sync(&db, "photos").unwrap();

        assert!(!source_is_current(&db, "photos", 1).unwrap());
    }

    #[test]
    fn source_sync_rejects_mixed_sources_without_changing_the_catalog() {
        let db = Connection::open_in_memory().unwrap();
        initialize(&db).unwrap();
        let original = test_asset("photos", "one", "image");
        finish_source_sync(&db, "photos", &[original]).unwrap();

        assert!(finish_source_sync(&db, "photos", &[test_asset("usb", "two", "image")]).is_err());
        assert!(source_is_current(&db, "photos", 1).unwrap());
        assert_eq!(list_library(&db, "photos", None, None, false, 0, 20).unwrap().1, 1);
    }

    #[test]
    fn library_filters_report_matching_totals_and_index_states() {
        let db = Connection::open_in_memory().unwrap();
        initialize(&db).unwrap();
        let indexed = test_asset("photos", "indexed", "image");
        let failed = test_asset("photos", "failed", "image");
        let pending = test_asset("photos", "pending", "image");
        let video = test_asset("photos", "video", "video");
        sync_test_assets(&db, &[indexed.clone(), failed.clone(), pending, video.clone()]);
        save_embedding(&db, &indexed, &[1.0, 0.0]).unwrap();
        mark_index_failure(&db, &failed, "decode failed").unwrap();

        let (rows, total) = list_library(&db, "all", Some("image"), Some("indexed"), false, 0, 1).unwrap();
        assert_eq!(total, 1);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].index_status, "indexed");
        assert_eq!(list_library(&db, "photos", Some("image"), Some("failed"), false, 0, 10).unwrap().1, 1);
        assert_eq!(list_library(&db, "photos", Some("image"), Some("pending"), false, 0, 10).unwrap().1, 1);
        let (videos, video_total) = list_library(&db, "photos", Some("video"), Some("unsupported"), false, 0, 10).unwrap();
        assert_eq!(video_total, 1);
        assert_eq!(videos[0].index_status, "unsupported");
    }

    fn sync_test_assets(db: &Connection, assets: &[Asset]) {
        for asset in assets {
            db.execute(
                "INSERT INTO assets(asset_key,source,name,created_at,width,height,favorite,media_type) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                params![format!("{}:{}", asset.source, asset.id), asset.source, asset.name, asset.date, asset.width, asset.height, asset.favorite, asset.media_type],
            ).unwrap();
        }
    }

    #[test]
    fn semantic_search_excludes_unrelated_photos_for_hair_query() {
        let db = Connection::open_in_memory().unwrap();
        initialize(&db).unwrap();
        for i in 0..30 {
            indexed_photo(
                &db,
                &format!("background-{i}"),
                0.45 + (i % 5) as f32 * 0.01,
            );
        }
        indexed_photo(&db, "hair", 0.80);
        let results = semantic_search(&db, &[1.0, 0.0], "hair", 120, "all", false).unwrap();
        let ids: Vec<_> = results.iter().map(|r| r["id"].as_str().unwrap()).collect();
        assert_eq!(
            ids,
            ["hair"],
            "search must not fill the result limit with unrelated photos"
        );
    }

    #[test]
    fn semantic_search_returns_empty_when_nothing_stands_out() {
        let db = Connection::open_in_memory().unwrap();
        initialize(&db).unwrap();
        for i in 0..30 {
            indexed_photo(
                &db,
                &format!("background-{i}"),
                0.45 + (i % 5) as f32 * 0.01,
            );
        }
        assert!(
            semantic_search(&db, &[1.0, 0.0], "unrelated", 120, "all", false)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn semantic_search_can_expand_small_or_uniform_libraries() {
        let db = Connection::open_in_memory().unwrap();
        initialize(&db).unwrap();
        indexed_photo(&db, "hair", 0.80);
        assert!(
            semantic_search(&db, &[1.0, 0.0], "portrait", 120, "all", false)
                .unwrap()
                .is_empty()
        );
        let results = semantic_search(&db, &[1.0, 0.0], "portrait", 120, "all", true).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["id"], "hair");
        assert_eq!(results[0]["weakMatch"], true);
    }

    #[test]
    fn semantic_search_does_not_amplify_tiny_score_differences() {
        let db = Connection::open_in_memory().unwrap();
        initialize(&db).unwrap();
        for i in 0..30 {
            indexed_photo(&db, &format!("background-{i}"), 0.50);
        }
        indexed_photo(&db, "noise", 0.501);
        assert!(
            semantic_search(&db, &[1.0, 0.0], "grain", 120, "all", false)
                .unwrap()
                .is_empty()
        );

        // A file-name hit is deliberate, so it survives the same gate.
        let named = semantic_search(&db, &[1.0, 0.0], "noise", 120, "all", false).unwrap();
        let ids: Vec<_> = named.iter().map(|r| r["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["noise"]);
        assert_eq!(named[0]["weakMatch"], false);

        // Partial word hits stay subject to the gate.
        assert!(
            semantic_search(&db, &[1.0, 0.0], "nois", 120, "all", false)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn semantic_search_filters_before_limiting_and_preserves_raw_scores() {
        let db = Connection::open_in_memory().unwrap();
        initialize(&db).unwrap();
        for i in 0..30 {
            indexed_photo(&db, &format!("background-{i}"), 0.45);
        }
        indexed_photo(&db, "hair", 0.80);
        indexed_photo(&db, "hair-2", 0.75);
        let results = semantic_search(&db, &[1.0, 0.0], "hair", 1, "all", false).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["id"], "hair");
        assert_eq!(results[0]["weakMatch"], false);
        assert!((results[0]["score"].as_f64().unwrap() - 0.80).abs() < 1e-6);
        assert!(
            semantic_search(&db, &[1.0, 0.0], "hair", 0, "all", true)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn semantic_search_excludes_invalid_deleted_and_other_source_vectors() {
        let db = Connection::open_in_memory().unwrap();
        initialize(&db).unwrap();
        for id in [
            "hair",
            "deleted",
            "usb",
            "nan",
            "zero",
            "dimensions",
            "old-model",
        ] {
            indexed_photo(&db, id, 0.80);
        }
        db.execute(
            "UPDATE assets SET deleted_at=CURRENT_TIMESTAMP WHERE name='deleted.jpg'",
            [],
        )
        .unwrap();
        db.execute("UPDATE assets SET source='usb' WHERE name='usb.jpg'", [])
            .unwrap();
        for (id, vector) in [("nan", [f32::NAN, 1.0]), ("zero", [0.0, 0.0])] {
            let bytes: Vec<u8> = vector.iter().flat_map(|v| v.to_le_bytes()).collect();
            db.execute(
                "UPDATE embeddings SET vector=?1 WHERE asset_key=?2",
                params![bytes, format!("photos:{id}")],
            )
            .unwrap();
        }
        db.execute(
            "UPDATE embeddings SET dimensions=3 WHERE asset_key='photos:dimensions'",
            [],
        )
        .unwrap();
        db.execute(
            "UPDATE embeddings SET model_version='obsolete' WHERE asset_key='photos:old-model'",
            [],
        )
        .unwrap();
        let results = semantic_search(&db, &[1.0, 0.0], "hair", 120, "photos", true).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["id"], "hair");
        for query in [
            &[][..],
            &[0.0, 0.0],
            &[f32::NAN, 1.0],
            &[f32::INFINITY, 1.0],
        ] {
            assert!(semantic_search(&db, query, "hair", 120, "all", false).is_err());
        }
    }
}
