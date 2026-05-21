package com.example.springdemo.config;

import com.fasterxml.jackson.core.type.TypeReference;
import com.fasterxml.jackson.databind.ObjectMapper;
import org.postgresql.util.PGobject;
import org.springframework.context.annotation.Bean;
import org.springframework.context.annotation.Configuration;
import org.springframework.core.convert.converter.Converter;
import org.springframework.data.convert.ReadingConverter;
import org.springframework.data.convert.WritingConverter;
import org.springframework.data.jdbc.core.convert.JdbcCustomConversions;

import java.util.List;
import java.util.Map;

/**
 * Registers custom JDBC converters for mapping {@code Map<String, Object>}
 * to/from a PostgreSQL {@code JSONB} column.
 *
 * <p>Spring Data JDBC does not natively know about {@code jsonb}; the
 * {@link PGobject} adaptor from the PostgreSQL JDBC driver is the standard
 * bridge.  Each converter is annotated with {@code @WritingConverter} or
 * {@code @ReadingConverter} so Spring Data can pick the right direction.
 */
@Configuration
public class JdbcConfig {

    @Bean
    public JdbcCustomConversions jdbcCustomConversions() {
        // Use a fresh ObjectMapper; the Spring-managed one is not always
        // available when JdbcCustomConversions is wired early in the context.
        ObjectMapper mapper = new ObjectMapper();
        return new JdbcCustomConversions(List.of(
                new MapToJsonbConverter(mapper),
                new JsonbToMapConverter(mapper)
        ));
    }

    /** Converts {@code Map<String, Object>} → {@code PGobject(type=jsonb)}. */
    @WritingConverter
    static class MapToJsonbConverter implements Converter<Map<String, Object>, PGobject> {
        private final ObjectMapper mapper;

        MapToJsonbConverter(ObjectMapper mapper) {
            this.mapper = mapper;
        }

        @Override
        public PGobject convert(Map<String, Object> source) {
            try {
                PGobject json = new PGobject();
                json.setType("jsonb");
                json.setValue(mapper.writeValueAsString(source));
                return json;
            } catch (Exception e) {
                throw new IllegalStateException("Failed to serialize Map to JSONB", e);
            }
        }
    }

    /** Converts {@code PGobject(jsonb)} → {@code Map<String, Object>}. */
    @ReadingConverter
    static class JsonbToMapConverter implements Converter<PGobject, Map<String, Object>> {
        private static final TypeReference<Map<String, Object>> MAP_TYPE =
                new TypeReference<>() {};
        private final ObjectMapper mapper;

        JsonbToMapConverter(ObjectMapper mapper) {
            this.mapper = mapper;
        }

        @Override
        public Map<String, Object> convert(PGobject source) {
            try {
                String value = source.getValue();
                if (value == null || value.isBlank()) return Map.of();
                return mapper.readValue(value, MAP_TYPE);
            } catch (Exception e) {
                throw new IllegalStateException("Failed to deserialize JSONB to Map", e);
            }
        }
    }
}
