-- GC-Stats — RiotRelay database schema (PostgreSQL)
--
-- Match cache table, executed automatically at server startup
-- (include_str! in main.rs). Manual usage: psql riotrelay < sql/schema.sql
-- The database itself must exist beforehand:
--   CREATE DATABASE riotrelay;
--
-- Copyright (c) 2026 Osthelia — RiotRelay
-- License: https://github.com/Osthelia/RiotRelay/blob/main/LICENSE.md (Osthelia Free License v2.0)
-- Repository: https://github.com/Osthelia/RiotRelay

CREATE TABLE IF NOT EXISTS matches (
    region     VARCHAR(16)  NOT NULL,
    match_id   VARCHAR(64)  NOT NULL,
    body       TEXT         NOT NULL,
    fetched_at TIMESTAMPTZ  NOT NULL,
    PRIMARY KEY (region, match_id)
);

-- League cache is independent of Valorant; no existing entries are migrated.
CREATE TABLE IF NOT EXISTS lol_matches (
    region VARCHAR(16) NOT NULL,
    match_id VARCHAR(64) NOT NULL,
    body TEXT NOT NULL,
    fetched_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (region, match_id)
);


-- League Match-V5 timeline cache; kept separate from match metadata.
CREATE TABLE IF NOT EXISTS lol_timelines (
    region VARCHAR(16) NOT NULL,
    match_id VARCHAR(64) NOT NULL,
    body TEXT NOT NULL,
    fetched_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (region, match_id)
);

-- League Riot ID -> account lookups, valid for three days (checked at read time).
-- Keyed by lowercased name and tag: Riot IDs are case-insensitive and the
-- account service is global, so there is no region column.
CREATE TABLE IF NOT EXISTS lol_accounts (
    game_name VARCHAR(64) NOT NULL,
    tag_line VARCHAR(16) NOT NULL,
    body TEXT NOT NULL,
    fetched_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (game_name, tag_line)
);
