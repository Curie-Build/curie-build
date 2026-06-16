package com.example.greeter;

import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.node.ObjectNode;

public class Main {
    public static void main(String[] args) throws Exception {
        ObjectMapper mapper = new ObjectMapper();
        ObjectNode node = mapper.createObjectNode();
        node.put("greeting", "Hello from a modular Java application!");
        node.put("module", Main.class.getModule().getName());
        System.out.println(mapper.writerWithDefaultPrettyPrinter().writeValueAsString(node));
    }
}
