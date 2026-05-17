package com.example;

import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.node.ObjectNode;
import org.junit.jupiter.api.Test;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertTrue;

/**
 * Basic test to exercise Jackson serialisation.  The JUnit dependency version
 * is resolved from junit-bom declared in [test-bom-imports].
 */
class JacksonBomGreeterTest {

    @Test
    void greetingContainsExpectedFields() throws Exception {
        ObjectMapper mapper = new ObjectMapper();

        ObjectNode greeting = mapper.createObjectNode();
        greeting.put("message", "Hello from Curie!");
        greeting.put("bom", "jackson-bom");
        greeting.put("language", "Java");

        String json = mapper.writeValueAsString(greeting);

        assertTrue(json.contains("Hello from Curie!"), "message field present");
        assertEquals("jackson-bom", greeting.get("bom").asText(), "bom field value");
    }
}
