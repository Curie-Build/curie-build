package com.example.mapping;

import java.io.IOException;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.util.Properties;

/**
 * Prints a normal resource plus the remapped {@code META-INF/LICENSE} and
 * {@code META-INF/proguard/base.pro} that Curie copied from the project root.
 */
public class App {
    public static void main(String[] args) throws IOException {
        Properties app = loadProperties("/app.properties");
        System.out.println("name    = " + app.getProperty("name"));
        System.out.println("license = " + firstLine("/META-INF/LICENSE"));
        System.out.println("proguard= " + firstLine("/META-INF/proguard/base.pro"));
    }

    static Properties loadProperties(String resource) throws IOException {
        Properties props = new Properties();
        try (InputStream in = App.class.getResourceAsStream(resource)) {
            if (in == null) {
                throw new IllegalStateException("missing resource: " + resource);
            }
            props.load(in);
        }
        return props;
    }

    static String firstLine(String resource) throws IOException {
        try (InputStream in = App.class.getResourceAsStream(resource)) {
            if (in == null) {
                throw new IllegalStateException("missing resource: " + resource);
            }
            String text = new String(in.readAllBytes(), StandardCharsets.UTF_8);
            int nl = text.indexOf('\n');
            return nl < 0 ? text.strip() : text.substring(0, nl).strip();
        }
    }
}
