-- The per-tenant grant set for the `ai` role (WP-AI-05.T1). The role gets SELECT on the
-- tenant schema's reporting views and tables, and EXECUTE on the query API's functions, and
-- nothing else: no INSERT, UPDATE, DELETE, TRUNCATE or DDL. Default privileges carry the same
-- grants to objects created later in the schema.
GRANT CONNECT ON DATABASE {{database}} TO {{role}};
GRANT USAGE ON SCHEMA {{schema}} TO {{role}};
GRANT SELECT ON ALL TABLES IN SCHEMA {{schema}} TO {{role}};
GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA {{schema}} TO {{role}};
ALTER DEFAULT PRIVILEGES IN SCHEMA {{schema}} GRANT SELECT ON TABLES TO {{role}};
ALTER DEFAULT PRIVILEGES IN SCHEMA {{schema}} GRANT EXECUTE ON FUNCTIONS TO {{role}};
