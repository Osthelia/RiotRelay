// GC-Stats — League Match-V5 cache. Valorant routes and cache remain independent.
// Copyright (c) 2026 Osthelia — RiotRelay
// License: https://github.com/Osthelia/RiotRelay/blob/main/LICENSE.md

use crate::{
    AppState, RiotFetch, db_error_response, fetch_url, json_response, unreachable_response,
};
use axum::{
    extract::{Path, State},
    http::{HeaderValue, StatusCode},
    response::Response,
};
use serde_json::{Value, json};
use std::sync::Arc;

pub const REGIONS: [&str; 4] = ["americas", "asia", "europe", "sea"];

fn platform_region(platform: &str) -> Option<&'static str> {
    match platform {
        "NA1" | "BR1" | "LA1" | "LA2" => Some("americas"),
        "EUW1" | "EUN1" | "TR1" | "RU" | "ME1" => Some("europe"),
        "KR" | "JP1" => Some("asia"),
        "OC1" | "PH2" | "SG2" | "TH2" | "TW2" | "VN2" => Some("sea"),
        _ => None,
    }
}

fn valid_id(region: &str, id: &str) -> bool {
    let Some((platform, number)) = id.split_once('_') else {
        return false;
    };
    id.len() <= 64
        && !number.is_empty()
        && number.bytes().all(|b| b.is_ascii_digit())
        && platform_region(platform) == Some(region)
}

fn valid_body(body: &str, id: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return false;
    };
    value.pointer("/metadata/matchId").and_then(Value::as_str) == Some(id)
        && value
            .pointer("/info/participants")
            .is_some_and(Value::is_array)
        && value.pointer("/info/teams").is_some_and(Value::is_array)
}

pub async fn get_match(
    State(state): State<Arc<AppState>>,
    Path((region, id)): Path<(String, String)>,
) -> Response {
    let region = region.to_ascii_lowercase();
    if !REGIONS.contains(&region.as_str()) {
        return json_response(
            StatusCode::BAD_REQUEST,
            "NONE",
            json!({"error":"invalid region", "allowed":REGIONS}).to_string(),
        );
    }
    if !valid_id(&region, &id) {
        return json_response(
            StatusCode::BAD_REQUEST,
            "NONE",
            json!({"error":"invalid match id or region mismatch"}).to_string(),
        );
    }

    let cached: Option<(String, String)> = match sqlx::query_as(
        "SELECT body, to_char(fetched_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')
         FROM lol_matches WHERE region = $1 AND match_id = $2",
    )
    .bind(&region)
    .bind(&id)
    .fetch_optional(&state.db)
    .await
    {
        Ok(cached) => cached,
        Err(e) => {
            eprintln!("db error reading League cache for {region}/{id}: {e}");
            return db_error_response();
        }
    };

    if let Some((body, fetched_at)) = cached {
        let mut response = json_response(StatusCode::OK, "HIT", body);
        if let Ok(value) = HeaderValue::from_str(&fetched_at) {
            response.headers_mut().insert("X-Cache-Fetched-At", value);
        }
        return response;
    }

    let Some(key) = state.lol_api_key.as_deref() else {
        return league_not_configured_response("MISS", None);
    };
    let url = format!("https://{region}.api.riotgames.com/lol/match/v5/matches/{id}");
    match fetch_url(&state.http, &url, key).await {
        RiotFetch::Ok(body) if valid_body(&body, &id) => {
            if let Err(e) = store_lol_match(&state.db, &region, &id, &body).await {
                eprintln!("failed to cache League match {region}/{id}: {e}");
                return db_error_response();
            }
            json_response(StatusCode::OK, "MISS", body)
        }
        RiotFetch::Ok(_) => json_response(
            StatusCode::BAD_GATEWAY,
            "MISS",
            json!({"error":"invalid_riot_response"}).to_string(),
        ),
        RiotFetch::Error(status, headers, body) => {
            let mut response = json_response(status, "MISS", body);
            response.headers_mut().extend(headers);
            response
        }
        RiotFetch::Unreachable(err) => unreachable_response("MISS", &err),
    }
}

pub async fn renew_match(
    State(state): State<Arc<AppState>>,
    Path((region, id)): Path<(String, String)>,
) -> Response {
    let region = region.to_ascii_lowercase();
    if !REGIONS.contains(&region.as_str()) {
        return json_response(
            StatusCode::BAD_REQUEST,
            "NONE",
            json!({"error":"invalid region", "allowed":REGIONS}).to_string(),
        );
    }
    if !valid_id(&region, &id) {
        return json_response(
            StatusCode::BAD_REQUEST,
            "NONE",
            json!({"error":"invalid match id or region mismatch"}).to_string(),
        );
    }

    let existed: bool = match sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM lol_matches WHERE region = $1 AND match_id = $2",
    )
    .bind(&region)
    .bind(&id)
    .fetch_one(&state.db)
    .await
    {
        Ok(count) => count > 0,
        Err(e) => {
            eprintln!("db error reading League cache for {region}/{id}: {e}");
            return db_error_response();
        }
    };

    let Some(key) = state.lol_api_key.as_deref() else {
        return league_not_configured_response(
            "RENEW-FAILED",
            Some(if existed { "true" } else { "false" }),
        );
    };
    let url = format!("https://{region}.api.riotgames.com/lol/match/v5/matches/{id}");
    let (mut response, forwarded) = match fetch_url(&state.http, &url, key).await {
        RiotFetch::Ok(body) if valid_body(&body, &id) => {
            if let Err(e) = store_lol_match(&state.db, &region, &id, &body).await {
                eprintln!("failed to cache League match {region}/{id}: {e}");
                return db_error_response();
            }
            return json_response(StatusCode::OK, "RENEWED", body);
        }
        RiotFetch::Ok(_) => (
            json_response(
                StatusCode::BAD_GATEWAY,
                "RENEW-FAILED",
                json!({"error":"invalid_riot_response"}).to_string(),
            ),
            None,
        ),
        RiotFetch::Error(status, headers, body) => {
            (json_response(status, "RENEW-FAILED", body), Some(headers))
        }
        RiotFetch::Unreachable(err) => (unreachable_response("RENEW-FAILED", &err), None),
    };

    response.headers_mut().insert(
        "X-Cache-Preserved",
        HeaderValue::from_static(if existed { "true" } else { "false" }),
    );
    if let Some(headers) = forwarded {
        response.headers_mut().extend(headers);
    }
    response
}

fn league_not_configured_response(cache_status: &str, preserved: Option<&'static str>) -> Response {
    let mut response = json_response(
        StatusCode::SERVICE_UNAVAILABLE,
        cache_status,
        json!({"error":"league_not_configured"}).to_string(),
    );
    if let Some(preserved) = preserved {
        response
            .headers_mut()
            .insert("X-Cache-Preserved", HeaderValue::from_static(preserved));
    }
    response
}

async fn store_lol_match(
    db: &sqlx::PgPool,
    region: &str,
    id: &str,
    body: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO lol_matches (region, match_id, body, fetched_at) VALUES ($1, $2, $3, now())
         ON CONFLICT (region, match_id) DO UPDATE SET body = EXCLUDED.body, fetched_at = EXCLUDED.fetched_at",
    )
    .bind(region)
    .bind(id)
    .bind(body)
    .execute(db)
    .await?;
    Ok(())
}
