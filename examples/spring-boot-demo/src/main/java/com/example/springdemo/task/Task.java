package com.example.springdemo.task;

import org.springframework.data.annotation.Id;
import org.springframework.data.relational.core.mapping.Column;
import org.springframework.data.relational.core.mapping.Table;

import java.util.Map;

/**
 * Spring Data JDBC entity stored in the {@code task} table.
 * The {@code metadata} field maps to a PostgreSQL {@code JSONB} column via
 * custom converters registered in {@link com.example.springdemo.config.JdbcConfig}.
 */
@Table("task")
public record Task(
        @Id Long id,
        String title,
        @Column("metadata") Map<String, Object> metadata
) {
    /** Convenience factory for creating unsaved tasks (id = null). */
    public static Task of(String title, Map<String, Object> metadata) {
        return new Task(null, title, metadata);
    }
}
