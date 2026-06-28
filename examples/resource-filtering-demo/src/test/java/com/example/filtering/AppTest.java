package com.example.filtering;

import org.junit.jupiter.api.Test;
import java.io.InputStream;
import java.util.Properties;
import static org.junit.jupiter.api.Assertions.*;

/**
 * Proves the independent [test-resources] scope filtered the test fixture:
 * {@code test.properties}'s @project.version@ placeholder is substituted with
 * the project version (0.1.0), with no literal '@' left behind.
 */
class AppTest {

    @Test
    void testResourcesAreFiltered() throws Exception {
        Properties props = new Properties();
        try (InputStream in = AppTest.class.getResourceAsStream("/test.properties")) {
            assertNotNull(in, "test.properties must be on the test classpath");
            props.load(in);
        }
        assertEquals("0.1.0", props.getProperty("testVersion"));
    }
}
