-- SPDX-FileCopyrightText: Copyright Logically Magic Inc. <hello@logicallymagic.com>
-- SPDX-FileCopyrightText: Copyright Hamid Jahanzad <Hamid@Jahanzad.com>
-- SPDX-FileCopyrightText: Copyright Contributors to the Exchange8:MD
-- SPDX-License-Identifier: Apache-2.0

-- Ex8:MD — market-data sessions, stream state, LULD bands, protected quotes.
--
-- Design: design/sentin8-exchange8-data-model-technical-design.md §3.2 (MD) + §3.9.
-- No migration code (new platform): `sentin8-cli vertical init` applies this.

CREATE SCHEMA IF NOT EXISTS md;

-- MoldUDP64/ITCH/SOUP session config (replaces MOLD_SESSION / SOUP_*).
CREATE TABLE IF NOT EXISTS md.md_session (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    env_id      UUID NOT NULL,
    session_name TEXT NOT NULL UNIQUE,   -- 10-char wire session id
    kind        TEXT NOT NULL CHECK (kind IN ('mold','itch','soup','ws')),
    address     TEXT NOT NULL,
    port        INTEGER NOT NULL,
    book_tokens TEXT[] NOT NULL DEFAULT '{}',
    enabled     BOOLEAN NOT NULL DEFAULT TRUE
);

-- Per-stream sequence state (gap detection + resend).
CREATE TABLE IF NOT EXISTS md.md_stream_state (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    env_id      UUID NOT NULL,
    session_id  UUID NOT NULL REFERENCES md.md_session(id),
    out_seq     BIGINT NOT NULL DEFAULT 1,
    last_send_ts TIMESTAMPTZ,
    UNIQUE (session_id)
);

-- Published market-data replay store — retention-purged (D4/Cron).
CREATE TABLE IF NOT EXISTS md.md_message (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    env_id      UUID NOT NULL,
    session_id  UUID NOT NULL REFERENCES md.md_session(id),
    seq         BIGINT NOT NULL,
    msg_type    CHAR(1) NOT NULL,
    msg_body    BYTEA NOT NULL,
    ts          TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (session_id, seq)
);

-- Last-trade cache (legacy security_trade_stats).
CREATE TABLE IF NOT EXISTS md.security_trade_stats (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    env_id      UUID NOT NULL,
    book_id     UUID NOT NULL,
    security_id UUID NOT NULL,
    last_price  NUMERIC(18,8),
    last_time   TIMESTAMPTZ,
    UNIQUE (env_id, book_id, security_id)
);

-- Per-instrument LULD bands (design: market-protection doc).
CREATE TABLE IF NOT EXISTS md.price_band (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    env_id      UUID NOT NULL,
    security_id UUID NOT NULL,
    upper_pct   NUMERIC(10,4) NOT NULL,  -- e.g. 5.00 for tier 1
    lower_pct   NUMERIC(10,4) NOT NULL,
    effective_date DATE NOT NULL
);

-- Current limit state per instrument.
CREATE TABLE IF NOT EXISTS md.limit_state (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    env_id      UUID NOT NULL,
    security_id UUID NOT NULL,
    state       TEXT NOT NULL CHECK (state IN ('none','limit_up','limit_down',
                'luld_pause','straddle')),
    band_upper  NUMERIC(18,8),
    band_lower  NUMERIC(18,8),
    until_ts    TIMESTAMPTZ,
    UNIQUE (env_id, security_id)
);

-- Protected-quote cache (fed by VFeeds CBBO events).
CREATE TABLE IF NOT EXISTS md.protected_quote (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    env_id      UUID NOT NULL,
    security_id UUID NOT NULL,
    bid         NUMERIC(18,8),
    ask         NUMERIC(18,8),
    bid_size    NUMERIC(18,8),
    ask_size    NUMERIC(18,8),
    source_mics TEXT[] NOT NULL DEFAULT '{}',
    ts          TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (env_id, security_id)
);

-- ---------------------------------------------------------------------------
-- RLS (architecture-principles.md §4.3): enable + force on all tenant tables.
-- Policies use the platform helpers sentin8_context_initialized() and
-- sentin8_current_env_id(); services call set_tenant_context on the same
-- connection as the data query.
-- ---------------------------------------------------------------------------
DO $$
DECLARE tbl TEXT;
BEGIN
    FOREACH tbl IN ARRAY ARRAY['md.md_session', 'md.md_stream_state', 'md.md_message', 'md.security_trade_stats', 'md.price_band', 'md.limit_state', 'md.protected_quote'] LOOP
        BEGIN
            EXECUTE format('ALTER TABLE %s ENABLE ROW LEVEL SECURITY', tbl);
            EXECUTE format('ALTER TABLE %s FORCE ROW LEVEL SECURITY', tbl);
            EXECUTE format('DROP POLICY IF EXISTS %1$I_select ON %2$s;
CREATE POLICY %1$I_select ON %2$s FOR SELECT USING (sentin8_context_initialized() AND env_id = sentin8_current_env_id())', split_part(tbl, '.', 2), tbl);
            EXECUTE format('DROP POLICY IF EXISTS %1$I_insert ON %2$s;
CREATE POLICY %1$I_insert ON %2$s FOR INSERT WITH CHECK (sentin8_context_initialized() AND env_id = sentin8_current_env_id())', split_part(tbl, '.', 2), tbl);
            EXECUTE format('DROP POLICY IF EXISTS %1$I_update ON %2$s;
CREATE POLICY %1$I_update ON %2$s FOR UPDATE USING (sentin8_context_initialized() AND env_id = sentin8_current_env_id())', split_part(tbl, '.', 2), tbl);
            EXECUTE format('DROP POLICY IF EXISTS %1$I_delete ON %2$s;
CREATE POLICY %1$I_delete ON %2$s FOR DELETE USING (sentin8_context_initialized() AND env_id = sentin8_current_env_id())', split_part(tbl, '.', 2), tbl);
        EXCEPTION WHEN OTHERS THEN
            RAISE NOTICE 'RLS setup failed for %: %', tbl, SQLERRM;
        END;
    END LOOP;
END $$;

-- ---------------------------------------------------------------------------
-- Per-service role grants (schema-per-service: this service owns schema
-- `md`; grants live in db `exchange8` only — design doc §1).
-- ---------------------------------------------------------------------------
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'exchange8_md_service') THEN
        ALTER ROLE exchange8_md_service SET search_path = md, public;
        GRANT USAGE ON SCHEMA md TO exchange8_md_service;
        GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA md TO exchange8_md_service;
        GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA md TO exchange8_md_service;
    END IF;
END $$;

-- ---------------------------------------------------------------------------
-- Local outbox (DBX §6.7 — generated from `sentin8_outbox::outbox_table_sql(
-- "md")`): rows written in the same DB transaction as the domain change;
-- the shell's relay (boot step §8b) pushes them to Bus `POST /v1/produce`.
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS md.outbox (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    env_id          UUID NOT NULL,
    event_type      TEXT NOT NULL,
    stream_family   TEXT NOT NULL,
    payload         JSONB NOT NULL,
    publisher_id_token  TEXT,
    publisher_subject   TEXT,
    relayed         BOOLEAN NOT NULL DEFAULT false,
    relayed_at      TIMESTAMPTZ,
    relay_attempts  INT NOT NULL DEFAULT 0,
    next_retry_at   TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_md_outbox_unrelayed
    ON md.outbox (created_at) WHERE relayed = false;
DO $$ BEGIN
    EXECUTE 'ALTER TABLE md.outbox ENABLE ROW LEVEL SECURITY';
    EXECUTE 'ALTER TABLE md.outbox FORCE ROW LEVEL SECURITY';
    EXECUTE 'DROP POLICY IF EXISTS outbox_select ON md.outbox';
    EXECUTE 'CREATE POLICY outbox_select ON md.outbox FOR SELECT
        USING (sentin8_context_initialized()
               AND env_id = sentin8_current_env_id())';
    EXECUTE 'DROP POLICY IF EXISTS outbox_insert ON md.outbox';
    EXECUTE 'CREATE POLICY outbox_insert ON md.outbox FOR INSERT
        WITH CHECK (sentin8_context_initialized()
               AND env_id = sentin8_current_env_id())';
    EXECUTE 'DROP POLICY IF EXISTS outbox_update ON md.outbox';
    EXECUTE 'CREATE POLICY outbox_update ON md.outbox FOR UPDATE
        USING (sentin8_context_initialized()
               AND env_id = sentin8_current_env_id())';
EXCEPTION WHEN OTHERS THEN
    RAISE NOTICE 'RLS setup failed for md.outbox: %', SQLERRM;
END $$;

CREATE OR REPLACE FUNCTION md.md_notify_outbox() RETURNS TRIGGER
LANGUAGE plpgsql AS $fn$
BEGIN
    PERFORM pg_notify('md_outbox_changed', '');
    RETURN NEW;
END;
$fn$;
DROP TRIGGER IF EXISTS outbox_notify_trigger ON md.outbox;
CREATE TRIGGER outbox_notify_trigger
    AFTER INSERT ON md.outbox
    FOR EACH ROW
    EXECUTE FUNCTION md.md_notify_outbox();

-- ---------------------------------------------------------------------------
-- Local audit journal (DBX §6.8 — generated from
-- `sentin8_audit::audit_journal_sql("md")`): rows written in the same
-- transaction as the domain change; the shell's pusher (boot step §8c) pushes
-- them to Sys `POST /v1/audit` (`sentin8:sys:audit:write`). A Sys outage
-- never drops audit.
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS md.audit_journal (
    id              UUID PRIMARY KEY,
    env_id          UUID NOT NULL,
    action          TEXT NOT NULL,
    target_resource_type TEXT,
    target_resource_id    TEXT,
    actor_user_id   TEXT,
    actor_subject_type TEXT,
    severity        TEXT NOT NULL DEFAULT 'info',
    correlation_id  TEXT,
    before_state    JSONB,
    after_state     JSONB,
    details         JSONB,
    pushed          BOOLEAN NOT NULL DEFAULT false,
    pushed_at       TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_md_audit_journal_unpushed
    ON md.audit_journal (created_at) WHERE pushed = false;
DO $$ BEGIN
    EXECUTE 'ALTER TABLE md.audit_journal ENABLE ROW LEVEL SECURITY';
    EXECUTE 'ALTER TABLE md.audit_journal FORCE ROW LEVEL SECURITY';
    EXECUTE 'DROP POLICY IF EXISTS audit_journal_dml ON md.audit_journal';
    EXECUTE 'CREATE POLICY audit_journal_dml ON md.audit_journal
        USING (sentin8_context_initialized()
               AND env_id = sentin8_current_env_id())
        WITH CHECK (sentin8_context_initialized()
               AND env_id = sentin8_current_env_id())';
EXCEPTION WHEN OTHERS THEN
    RAISE NOTICE 'RLS setup failed for md.audit_journal: %', SQLERRM;
END $$;

-- Outbox + journal role grants (same pattern as the table grants above).
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'exchange8_md_service') THEN
        GRANT SELECT, INSERT, UPDATE ON ALL TABLES IN SCHEMA md TO exchange8_md_service;
    END IF;
END $$;
