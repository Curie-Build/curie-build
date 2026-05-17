package com.example;

import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.node.ObjectNode;

/**
 * Same greeting as json-greeter, using the flat-package source layout
 * (src/com.example/) instead of the Maven directory hierarchy.
 */
public class JsonGreeter {
    public static void main(String[] args) throws Exception {
        ObjectMapper mapper = new ObjectMapper();

        ObjectNode greeting = mapper.createObjectNode();
        greeting.put("message", "Hello from Curie!");
        greeting.put("framework", "Curie");
        greeting.put("language", "Java");

        String json = mapper.writerWithDefaultPrettyPrinter().writeValueAsString(greeting);
        System.out.println(json);
    }
}
