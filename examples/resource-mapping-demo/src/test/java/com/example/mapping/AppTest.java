package com.example.mapping;

import org.junit.jupiter.api.Test;

import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.util.Properties;

import static org.junit.jupiter.api.Assertions.*;

/**
 * Proves remapping copied project-root {@code LICENSE} and {@code proguard/*}
 * to {@code META-INF/} while leaving {@code README.md} out of the classpath,
 * and that the identity {@code src/main/resources} tree still merges in.
 */
class AppTest {

    @Test
    void identityResourceIsOnClasspath() throws Exception {
        Properties props = App.loadProperties("/app.properties");
        assertEquals("resource-mapping-demo", props.getProperty("name"));
    }

    @Test
    void licenseIsRemappedToMetaInf() throws Exception {
        String license = read("/META-INF/LICENSE");
        assertTrue(license.contains("Apache License"), license);
    }

    @Test
    void proguardFilesAreRemappedToMetaInf() throws Exception {
        String base = read("/META-INF/proguard/base.pro");
        String cache = read("/META-INF/proguard/cache.pro");
        assertTrue(base.contains("-keep class com.example.mapping.App"), base);
        assertTrue(cache.contains("-keep class com.example.mapping.**"), cache);
    }

    @Test
    void unlistedRootFileIsNotCopied() {
        assertNull(
                AppTest.class.getResourceAsStream("/README.md"),
                "README.md is next to LICENSE but not in includes");
        assertNull(
                AppTest.class.getResourceAsStream("/META-INF/README.md"),
                "README.md must not be remapped either");
    }

    private static String read(String resource) throws Exception {
        try (InputStream in = AppTest.class.getResourceAsStream(resource)) {
            assertNotNull(in, resource + " must be on the test classpath");
            return new String(in.readAllBytes(), StandardCharsets.UTF_8);
        }
    }
}
