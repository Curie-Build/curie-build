package com.example.fatjar;

import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.node.ArrayNode;
import com.fasterxml.jackson.databind.node.ObjectNode;
import com.google.common.collect.ImmutableList;
import com.google.common.collect.ImmutableMap;

import java.util.ServiceLoader;

/**
 * Demonstrates fat/uber JAR functionality:
 *
 * <ul>
 *   <li>Multiple dependencies (Jackson, Guava) merged into one JAR</li>
 *   <li>Package relocations (Guava → shaded) to avoid version conflicts</li>
 *   <li>META-INF/services merged from all dependency JARs</li>
 *   <li>Per-dependency exclusion (SLF4J excluded via fatJar = false)</li>
 * </ul>
 *
 * Run directly with: {@code java -jar target/fat-jar-demo-0.1.0-fat.jar}
 */
public class FatJarApp {

    public static void main(String[] args) throws Exception {
        ObjectMapper mapper = new ObjectMapper();
        ObjectNode root = mapper.createObjectNode();

        root.put("application", "fat-jar-demo");
        root.put("version", "0.1.0");
        root.put("fatJar", true);

        // Use Guava (will be relocated in the fat JAR)
        ImmutableList<String> features = ImmutableList.of(
            "dependency-merging",
            "services-merging",
            "package-relocation",
            "per-dep-include-exclude",
            "deterministic-output",
            "incremental-rebuild"
        );

        ArrayNode featuresNode = root.putArray("features");
        for (String feature : features) {
            featuresNode.add(feature);
        }

        // Demonstrate Guava ImmutableMap
        ImmutableMap<String, String> metadata = ImmutableMap.of(
            "build-tool", "Curie",
            "packaging", "fat-jar",
            "guava-relocated", "true"
        );

        ObjectNode metadataNode = root.putObject("metadata");
        metadata.forEach(metadataNode::put);

        // Show discovered services (META-INF/services merging)
        ArrayNode servicesNode = root.putArray("discoveredServices");
        ServiceLoader<com.fasterxml.jackson.databind.Module> modules =
            ServiceLoader.load(com.fasterxml.jackson.databind.Module.class);
        for (com.fasterxml.jackson.databind.Module module : modules) {
            servicesNode.add(module.getModuleName());
        }

        String json = mapper.writerWithDefaultPrettyPrinter()
            .writeValueAsString(root);
        System.out.println(json);
    }
}