package com.example;

import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.node.ObjectNode;

/**
 * Same greeting as json-greeter, but the Jackson version is resolved from
 * jackson-bom rather than pinned directly in curie.toml.
 */
public class JacksonBomGreeter {
    public static void main(String[] args) throws Exception {
        ObjectMapper mapper = new ObjectMapper();

        ObjectNode greeting = mapper.createObjectNode();
        greeting.put("message", "Hello from Curie!");
        greeting.put("bom", "jackson-bom");
        greeting.put("language", "Java");

        String json = mapper.writerWithDefaultPrettyPrinter().writeValueAsString(greeting);
        System.out.println(json);
    }
}
