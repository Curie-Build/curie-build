package com.example.liquid;

import org.junit.jupiter.api.Test;
import static org.junit.jupiter.api.Assertions.*;

import java.io.InputStream;
import java.util.Properties;

class AppTest {
    @Test
    void filteredVersionIsNotLiquidTemplate() throws Exception {
        Properties props = new Properties();
        try (InputStream in = getClass().getResourceAsStream("/test.properties")) {
            assertNotNull(in, "test.properties must be on the classpath");
            props.load(in);
        }
        // The Liquid engine should have replaced {{ project.version }} with
        // the actual version from Curie.toml.
        String version = props.getProperty("testVersion");
        assertNotNull(version);
        assertFalse(version.contains("{{"), "Liquid template was not rendered: " + version);
    }
}