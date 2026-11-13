// GC-Stats — League Match-V5 cache. Valorant routes and cache remain independent.
// Copyright (c) 2026 Osthelia — RiotRelay
// License: https://github.com/Osthelia/RiotRelay/blob/main/LICENSE.md

use crate::{
    AppState, RiotFetch, db_error_response, fetch_url, json_response, unreachable_response,
};
use axum::{
    extract::{Path, Query, State},
    http::{HeaderValue, StatusCode},
    response::Response,
};
use reqwest::Url;
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc};

pub const REGIONS: [&str; 4] = ["americas", "asia", "europe", "sea"];

/// `X-Cache` value for routes that never touch the cache.
const BYPASS: &str = "BYPASS";

/// Queue types accepted by `matches/by-puuid`.
const MATCH_TYPES: [&str; 4] = ["ranked", "normal", "tourney", "tutorial"];

const DEFAULT_MATCH_COUNT: u64 = 20;

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
    let region = match parse_region(&region) {
        Ok(region) => region,
        Err(response) => return response,
    };
    if !valid_id(&region, &id) {
        return bad_request("invalid match id or region mismatch");
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

    let key = match lol_key(&state, "MISS") {
        Ok(key) => key,
        Err(response) => return response,
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
    let region = match parse_region(&region) {
        Ok(region) => region,
        Err(response) => return response,
    };
    if !valid_id(&region, &id) {
        return bad_request("invalid match id or region mismatch");
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

pub async fn get_timeline(
    State(state): State<Arc<AppState>>,
    Path((region, id)): Path<(String, String)>,
) -> Response {
    let region = match parse_region(&region) {
        Ok(region) => region,
        Err(response) => return response,
    };
    if !valid_id(&region, &id) {
        return bad_request("invalid match id or region mismatch");
    }

    let cached: Option<(String, String)> = match sqlx::query_as(
        "SELECT body, to_char(fetched_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')
         FROM lol_timelines WHERE region = $1 AND match_id = $2",
    )
    .bind(&region)
    .bind(&id)
    .fetch_optional(&state.db)
    .await
    {
        Ok(cached) => cached,
        Err(e) => {
            eprintln!("db error reading League timeline cache for {region}/{id}: {e}");
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

    let key = match lol_key(&state, "MISS") {
        Ok(key) => key,
        Err(response) => return response,
    };
    let url = format!("https://{region}.api.riotgames.com/lol/match/v5/matches/{id}/timeline");
    match fetch_url(&state.http, &url, key).await {
        RiotFetch::Ok(body) if valid_timeline_body(&body, &id) => {
            if let Err(e) = store_lol_timeline(&state.db, &region, &id, &body).await {
                eprintln!("failed to cache League timeline {region}/{id}: {e}");
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

pub async fn renew_timeline(
    State(state): State<Arc<AppState>>,
    Path((region, id)): Path<(String, String)>,
) -> Response {
    let region = match parse_region(&region) {
        Ok(region) => region,
        Err(response) => return response,
    };
    if !valid_id(&region, &id) {
        return bad_request("invalid match id or region mismatch");
    }

    let existed: bool = match sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM lol_timelines WHERE region = $1 AND match_id = $2",
    )
    .bind(&region)
    .bind(&id)
    .fetch_one(&state.db)
    .await
    {
        Ok(count) => count > 0,
        Err(e) => {
            eprintln!("db error reading League timeline cache for {region}/{id}: {e}");
            return db_error_response();
        }
    };

    let Some(key) = state.lol_api_key.as_deref() else {
        return league_not_configured_response(
            "RENEW-FAILED",
            Some(if existed { "true" } else { "false" }),
        );
    };
    let url = format!("https://{region}.api.riotgames.com/lol/match/v5/matches/{id}/timeline");
    let (mut response, forwarded) = match fetch_url(&state.http, &url, key).await {
        RiotFetch::Ok(body) if valid_timeline_body(&body, &id) => {
            if let Err(e) = store_lol_timeline(&state.db, &region, &id, &body).await {
                eprintln!("failed to cache League timeline {region}/{id}: {e}");
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

fn valid_timeline_body(body: &str, id: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return false;
    };
    value.pointer("/metadata/matchId").and_then(Value::as_str) == Some(id)
        && value.pointer("/info/frames").is_some_and(Value::is_array)
}

/// Lowercases the region and checks it against `REGIONS`. The region ends up
/// in the Riot hostname, so anything else is rejected with a 400.
#[allow(clippy::result_large_err)]
fn parse_region(region: &str) -> Result<String, Response> {
    let region = region.to_ascii_lowercase();
    if REGIONS.contains(&region.as_str()) {
        Ok(region)
    } else {
        Err(json_response(
            StatusCode::BAD_REQUEST,
            "NONE",
            json!({"error":"invalid region", "allowed":REGIONS}).to_string(),
        ))
    }
}

/// The League API key, or the `league_not_configured` response.
#[allow(clippy::result_large_err)]
fn lol_key<'a>(state: &'a AppState, cache_status: &str) -> Result<&'a str, Response> {
    state
        .lol_api_key
        .as_deref()
        .ok_or_else(|| league_not_configured_response(cache_status, None))
}

fn bad_request(error: &str) -> Response {
    json_response(
        StatusCode::BAD_REQUEST,
        "NONE",
        json!({ "error": error }).to_string(),
    )
}

/// account-v1 only exists on the americas, asia and europe hosts.
fn account_host(region: &str) -> &str {
    if region == "sea" { "asia" } else { region }
}

/// `https://{host}.api.riotgames.com/{base}/{segments...}` with every segment
/// percent-encoded, so names with spaces or non-Latin letters stay one segment.
/// `host` and `base` must be trusted values, never request input.
fn riot_url(host: &str, base: &str, segments: &[&str]) -> Url {
    let mut url = Url::parse(&format!("https://{host}.api.riotgames.com/{base}"))
        .expect("host and base are static values");
    url.path_segments_mut()
        .expect("https URLs have path segments")
        .extend(segments);
    url
}

/// Turns a Riot answer into a response: a 200 passes `valid` or becomes
/// `invalid_riot_response`, errors and rate-limit headers pass through.
fn relay_response(
    fetched: RiotFetch,
    cache_status: &str,
    valid: impl Fn(&str) -> bool,
) -> Response {
    match fetched {
        RiotFetch::Ok(body) if valid(&body) => json_response(StatusCode::OK, cache_status, body),
        RiotFetch::Ok(_) => json_response(
            StatusCode::BAD_GATEWAY,
            cache_status,
            json!({"error":"invalid_riot_response"}).to_string(),
        ),
        RiotFetch::Error(status, headers, body) => {
            let mut response = json_response(status, cache_status, body);
            response.headers_mut().extend(headers);
            response
        }
        RiotFetch::Unreachable(err) => unreachable_response(cache_status, &err),
    }
}

fn valid_riot_id(game_name: &str, tag_line: &str) -> bool {
    (3..=16).contains(&game_name.chars().count())
        && !game_name.contains(['#', '/'])
        && (3..=5).contains(&tag_line.chars().count())
        && tag_line.chars().all(char::is_alphanumeric)
}

/// Riot IDs are case-insensitive, and the account service is global, so the
/// cache key is the lowercased name and tag without the region.
fn riot_id_cache_key(game_name: &str, tag_line: &str) -> (String, String) {
    (game_name.to_lowercase(), tag_line.to_lowercase())
}

/// Riot PUUIDs are 78 characters of `A-Za-z0-9_-`.
fn valid_puuid(puuid: &str) -> bool {
    (1..=100).contains(&puuid.len())
        && puuid
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn valid_account_body(body: &str) -> bool {
    serde_json::from_str::<Value>(body)
        .is_ok_and(|value| value.get("puuid").is_some_and(Value::is_string))
}

fn valid_match_ids_body(body: &str) -> bool {
    serde_json::from_str::<Value>(body).is_ok_and(|value| {
        value
            .as_array()
            .is_some_and(|ids| ids.iter().all(Value::is_string))
    })
}

fn parse_number(
    params: &HashMap<String, String>,
    name: &'static str,
) -> Result<Option<u64>, &'static str> {
    params
        .get(name)
        .map(|value| value.parse::<u64>().map_err(|_| name))
        .transpose()
}

/// Rebuilds the Riot query from the whitelisted parameters only. Values are
/// re-serialized from their parsed form; the raw query string is never
/// forwarded. On failure returns the name of the offending parameter.
fn parse_match_query(
    params: &HashMap<String, String>,
) -> Result<Vec<(&'static str, String)>, &'static str> {
    let mut pairs = Vec::new();
    if let Some(value) = params.get("type") {
        let match_type = MATCH_TYPES
            .iter()
            .find(|allowed| **allowed == value)
            .ok_or("type")?;
        pairs.push(("type", match_type.to_string()));
    }
    let start_time = parse_number(params, "startTime")?;
    let end_time = parse_number(params, "endTime")?;
    if let (Some(start), Some(end)) = (start_time, end_time)
        && end <= start
    {
        return Err("endTime");
    }
    for (name, value) in [("startTime", start_time), ("endTime", end_time)] {
        if let Some(value) = value {
            pairs.push((name, value.to_string()));
        }
    }
    if let Some(start) = parse_number(params, "start")? {
        pairs.push(("start", start.to_string()));
    }
    let count = parse_number(params, "count")?.unwrap_or(DEFAULT_MATCH_COUNT);
    if !(1..=100).contains(&count) {
        return Err("count");
    }
    pairs.push(("count", count.to_string()));
    Ok(pairs)
}

fn match_ids_url(region: &str, puuid: &str, query: &[(&str, String)]) -> Url {
    let mut url = riot_url(region, "lol/match/v5/matches/by-puuid", &[puuid, "ids"]);
    url.query_pairs_mut()
        .extend_pairs(query.iter().map(|(name, value)| (*name, value.as_str())));
    url
}

pub async fn get_account_by_riot_id(
    State(state): State<Arc<AppState>>,
    Path((region, game_name, tag_line)): Path<(String, String, String)>,
) -> Response {
    let region = match parse_region(&region) {
        Ok(region) => region,
        Err(response) => return response,
    };
    if !valid_riot_id(&game_name, &tag_line) {
        return bad_request("invalid riot id");
    }
    let (name_key, tag_key) = riot_id_cache_key(&game_name, &tag_line);

    let cached: Option<(String, String)> = match sqlx::query_as(
        "SELECT body, to_char(fetched_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')
         FROM lol_accounts
         WHERE game_name = $1 AND tag_line = $2 AND fetched_at > now() - interval '3 days'",
    )
    .bind(&name_key)
    .bind(&tag_key)
    .fetch_optional(&state.db)
    .await
    {
        Ok(cached) => cached,
        Err(e) => {
            eprintln!("db error reading League account cache for {name_key}#{tag_key}: {e}");
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

    let key = match lol_key(&state, "MISS") {
        Ok(key) => key,
        Err(response) => return response,
    };
    let url = riot_url(
        account_host(&region),
        "riot/account/v1/accounts/by-riot-id",
        &[&game_name, &tag_line],
    );
    let fetched = fetch_url(&state.http, url.as_str(), key).await;
    if let RiotFetch::Ok(body) = &fetched
        && valid_account_body(body)
        && let Err(e) = store_lol_account(&state.db, &name_key, &tag_key, body).await
    {
        eprintln!("failed to cache League account {name_key}#{tag_key}: {e}");
        return db_error_response();
    }
    relay_response(fetched, "MISS", valid_account_body)
}

pub async fn get_matches_by_puuid(
    State(state): State<Arc<AppState>>,
    Path((region, puuid)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let region = match parse_region(&region) {
        Ok(region) => region,
        Err(response) => return response,
    };
    if !valid_puuid(&puuid) {
        return bad_request("invalid puuid");
    }
    let query = match parse_match_query(&params) {
        Ok(query) => query,
        Err(name) => return bad_request(&format!("invalid {name}")),
    };
    let key = match lol_key(&state, BYPASS) {
        Ok(key) => key,
        Err(response) => return response,
    };
    let url = match_ids_url(&region, &puuid, &query);
    relay_response(
        fetch_url(&state.http, url.as_str(), key).await,
        BYPASS,
        valid_match_ids_body,
    )
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

async fn store_lol_account(
    db: &sqlx::PgPool,
    game_name: &str,
    tag_line: &str,
    body: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO lol_accounts (game_name, tag_line, body, fetched_at) VALUES ($1, $2, $3, now())
         ON CONFLICT (game_name, tag_line) DO UPDATE SET body = EXCLUDED.body, fetched_at = EXCLUDED.fetched_at",
    )
    .bind(game_name)
    .bind(tag_line)
    .bind(body)
    .execute(db)
    .await?;
    Ok(())
}

async fn store_lol_timeline(
    db: &sqlx::PgPool,
    region: &str,
    id: &str,
    body: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO lol_timelines (region, match_id, body, fetched_at) VALUES ($1, $2, $3, now())
         ON CONFLICT (region, match_id) DO UPDATE SET body = EXCLUDED.body, fetched_at = EXCLUDED.fetched_at",
    )
    .bind(region)
    .bind(id)
    .bind(body)
    .execute(db)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn query(pairs: &[(&str, &str)]) -> Result<Vec<(&'static str, String)>, &'static str> {
        parse_match_query(&params(pairs))
    }

    #[test]
    fn riot_id_length_limits() {
        assert!(!valid_riot_id("ab", "EUW"));
        assert!(valid_riot_id("abc", "EUW"));
        assert!(valid_riot_id(&"a".repeat(16), "EUW"));
        assert!(!valid_riot_id(&"a".repeat(17), "EUW"));
        assert!(!valid_riot_id("Faker", "EU"));
        assert!(valid_riot_id("Faker", "12345"));
        assert!(!valid_riot_id("Faker", "123456"));
    }

    #[test]
    fn riot_id_rejects_forbidden_characters() {
        assert!(!valid_riot_id("Fa#ker", "EUW"));
        assert!(!valid_riot_id("Fa/ker", "EUW"));
        assert!(!valid_riot_id("Faker", "EU-W"));
        assert!(!valid_riot_id("Faker", "EU W"));
    }

    #[test]
    fn riot_id_accepts_unicode_names() {
        assert!(valid_riot_id("Hide on bush", "KR1"));
        assert!(valid_riot_id("불꽃 놀이", "KR1"));
        // 16 characters, but more than 16 bytes
        assert!(valid_riot_id(&"é".repeat(16), "EUW"));
    }

    #[test]
    fn account_url_encodes_names() {
        let url = riot_url(
            account_host("europe"),
            "riot/account/v1/accounts/by-riot-id",
            &["Hide on bush", "KR1"],
        );
        assert_eq!(
            url.as_str(),
            "https://europe.api.riotgames.com/riot/account/v1/accounts/by-riot-id/Hide%20on%20bush/KR1"
        );
        let url = riot_url(
            "asia",
            "riot/account/v1/accounts/by-riot-id",
            &["불꽃", "KR1"],
        );
        assert_eq!(
            url.as_str(),
            "https://asia.api.riotgames.com/riot/account/v1/accounts/by-riot-id/%EB%B6%88%EA%BD%83/KR1"
        );
    }

    #[test]
    fn account_name_cannot_change_the_path() {
        let url = riot_url(
            "asia",
            "riot/account/v1/accounts/by-riot-id",
            &["a?b%2F...", "EUW"],
        );
        assert_eq!(url.query(), None);
        assert_eq!(url.path_segments().unwrap().count(), 7);
    }

    #[test]
    fn riot_id_cache_key_ignores_case() {
        assert_eq!(
            riot_id_cache_key("Hide On Bush", "KR1"),
            riot_id_cache_key("hide on bush", "kr1")
        );
        assert_eq!(
            riot_id_cache_key("ÉLAN", "EUW"),
            ("élan".to_string(), "euw".to_string())
        );
    }

    #[test]
    fn sea_uses_the_asia_account_host() {
        assert_eq!(account_host("sea"), "asia");
        assert_eq!(account_host("asia"), "asia");
        assert_eq!(account_host("americas"), "americas");
        assert_eq!(account_host("europe"), "europe");
    }

    #[test]
    fn query_defaults_to_count_20() {
        assert_eq!(query(&[]), Ok(vec![("count", "20".to_string())]));
    }

    #[test]
    fn query_accepts_every_parameter() {
        let pairs = query(&[
            ("type", "tourney"),
            ("startTime", "100"),
            ("endTime", "200"),
            ("start", "5"),
            ("count", "100"),
        ])
        .unwrap();
        assert_eq!(
            pairs,
            vec![
                ("type", "tourney".to_string()),
                ("startTime", "100".to_string()),
                ("endTime", "200".to_string()),
                ("start", "5".to_string()),
                ("count", "100".to_string()),
            ]
        );
    }

    #[test]
    fn query_rejects_invalid_values() {
        assert_eq!(query(&[("type", "aram")]), Err("type"));
        assert_eq!(query(&[("type", "Ranked")]), Err("type"));
        assert_eq!(query(&[("count", "0")]), Err("count"));
        assert_eq!(query(&[("count", "101")]), Err("count"));
        assert_eq!(query(&[("count", "x")]), Err("count"));
        assert_eq!(query(&[("start", "-1")]), Err("start"));
        assert_eq!(query(&[("startTime", "soon")]), Err("startTime"));
        assert_eq!(query(&[("endTime", "1.5")]), Err("endTime"));
    }

    #[test]
    fn query_requires_end_time_after_start_time() {
        assert_eq!(
            query(&[("startTime", "200"), ("endTime", "200")]),
            Err("endTime")
        );
        assert_eq!(
            query(&[("startTime", "200"), ("endTime", "100")]),
            Err("endTime")
        );
        assert!(query(&[("endTime", "100")]).is_ok());
    }

    #[test]
    fn query_drops_extra_parameters() {
        let pairs = query(&[("count", "5"), ("api_key", "secret"), ("foo", "bar")]).unwrap();
        assert_eq!(pairs, vec![("count", "5".to_string())]);
    }

    #[test]
    fn query_reserializes_parsed_numbers() {
        let pairs = query(&[("start", "+7")]).unwrap();
        assert_eq!(pairs[0], ("start", "7".to_string()));
    }

    #[test]
    fn match_ids_url_is_rebuilt_from_whitelisted_pairs() {
        let pairs = query(&[("type", "ranked"), ("count", "3"), ("x", "1&y=2")]).unwrap();
        let url = match_ids_url("sea", "abc-DEF_123", &pairs);
        assert_eq!(
            url.as_str(),
            "https://sea.api.riotgames.com/lol/match/v5/matches/by-puuid/abc-DEF_123/ids?type=ranked&count=3"
        );
    }

    #[test]
    fn puuid_format() {
        assert!(valid_puuid(&"aB3-_".repeat(15)));
        assert!(valid_puuid("a"));
        assert!(valid_puuid(&"a".repeat(100)));
        assert!(!valid_puuid(""));
        assert!(!valid_puuid(&"a".repeat(101)));
        assert!(!valid_puuid("abc/def"));
        assert!(!valid_puuid("abc.def"));
        assert!(!valid_puuid("abc def"));
        assert!(!valid_puuid("abc%2Fdef"));
        assert!(!valid_puuid("püuid"));
    }

    #[test]
    fn account_body_needs_a_string_puuid() {
        assert!(valid_account_body(
            r#"{"puuid":"abc","gameName":"Faker","tagLine":"KR1"}"#
        ));
        assert!(!valid_account_body(r#"{"gameName":"Faker"}"#));
        assert!(!valid_account_body(r#"{"puuid":42}"#));
        assert!(!valid_account_body(r#"["abc"]"#));
        assert!(!valid_account_body("not json"));
    }

    #[test]
    fn match_ids_body_needs_an_array_of_strings() {
        assert!(valid_match_ids_body(r#"["EUW1_1","EUW1_2"]"#));
        assert!(valid_match_ids_body("[]"));
        assert!(!valid_match_ids_body(r#"["EUW1_1",2]"#));
        assert!(!valid_match_ids_body(r#"{"ids":["EUW1_1"]}"#));
        assert!(!valid_match_ids_body("not json"));
    }
}
