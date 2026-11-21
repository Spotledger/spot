-- The dedicated `ai` database role (WP-AI-05.T1). Idempotent: safe to run on every migration.
-- The role may not log in directly; the pool connects as it with a per-tenant password held in
-- the secrets service. Read-only by default, with the pool budget's timeouts and memory cap.
DO $$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{{role}}') THEN CREATE ROLE {{role}} NOLOGIN; END IF; END $$;
ALTER ROLE {{role}} SET statement_timeout = '{{statement_timeout_ms}}ms';
ALTER ROLE {{role}} SET idle_in_transaction_session_timeout = '{{idle_in_transaction_timeout_ms}}ms';
ALTER ROLE {{role}} SET work_mem = '{{work_mem_kb}}kB';
ALTER ROLE {{role}} SET default_transaction_read_only = on;
-- An agent statement must use one backend, not the budget's worth of parallel workers: with
-- workers allowed, 4 pool clients became 8 active backends and human p95 and p99 missed their
-- budgets (measured in WP-AI-05.T2, see the AI load-test notes).
ALTER ROLE {{role}} SET max_parallel_workers_per_gather = 0;
