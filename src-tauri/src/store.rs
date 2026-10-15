use super::{Asset, ReviewInput, Rule, RuleInput};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

const EMBEDDER_VERSION: &str = "embeddinggemma-2:440m:768:v1";
const THUMBNAIL_CACHE_BUDGET_BYTES: i64 = 128 * 1024 * 1024;

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
    Ok(())
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
        "SELECT count(*) FROM assets WHERE source='photos' AND deleted_at IS NULL",
        [],
        |r| r.get(0),
    )?;
    let usb: i64 = db.query_row(
        "SELECT count(*) FROM assets WHERE source='usb' AND deleted_at IS NULL",
        [],
        |r| r.get(0),
    )?;
    let queued: i64 = db.query_row(
        "SELECT count(*) FROM assets WHERE queued=1 AND deleted_at IS NULL",
        [],
        |r| r.get(0),
    )?;
    let candidates: i64 = db.query_row("SELECT count(DISTINCT m.asset_key) FROM matches m JOIN rules r ON r.id=m.rule_id JOIN assets a USING(asset_key) WHERE m.score>=r.threshold AND a.review_state='pending' AND a.queued=0 AND a.deleted_at IS NULL",[],|r|r.get(0))?;
    Ok(json!({"photos":photos,"usb":usb,"queued":queued,"candidates":candidates}))
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
    limit: usize,
    source: &str,
) -> Result<Vec<Value>, String> {
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
        let score = super::cosine(query, &vector);
        found.push((score,json!({"id":key.split_once(':').map(|x|x.1).unwrap_or(&key),"source":source,"name":name,"date":date,"width":width,"height":height,"favorite":favorite,"reviewed":reviewed,"queued":queued,"thumbnail":thumbnail,"score":score,"key":key})));
    }
    found.sort_by(|a, b| b.0.total_cmp(&a.0));
    Ok(found
        .into_iter()
        .take(limit.clamp(1, 500))
        .map(|(_, v)| v)
        .collect())
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
