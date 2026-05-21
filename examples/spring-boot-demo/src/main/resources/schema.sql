-- Task Manager schema
-- Applied automatically by Spring Boot on startup when
-- spring.sql.init.mode=always (used in tests and local dev).
-- In production, prefer a migration tool like Flyway or Liquibase.

CREATE TABLE IF NOT EXISTS task (
    id       BIGSERIAL    PRIMARY KEY,
    title    TEXT         NOT NULL,
    metadata JSONB        NOT NULL DEFAULT '{}'::jsonb
);
