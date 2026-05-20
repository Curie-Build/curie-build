package com.example;

import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.node.ObjectNode;

public class MirrorDemo {
    public static void main(String[] args) throws Exception {
        ObjectMapper mapper = new ObjectMapper();
        ObjectNode node = mapper.createObjectNode();
        node.put("message", "Resolved via repository mirror");
        node.put("mirror", "https://maven-central.storage.googleapis.com/maven2");
        System.out.println(mapper.writerWithDefaultPrettyPrinter().writeValueAsString(node));
    }
}
