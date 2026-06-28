package com.example.filtering;

import java.io.IOException;
import java.io.InputStream;
import java.util.Properties;

/**
 * Prints the filtered build-time values that curie substituted into
 * {@code app.properties}, plus the verbatim {@code notes.properties} that was
 * intentionally left unfiltered (it lives outside the filter stage's scoped
 * directory).
 */
public class App {
    public static void main(String[] args) throws IOException {
        Properties filtered = load("/app.properties");
        Properties verbatim = load("/notes.properties");

        System.out.println("version      = " + filtered.getProperty("version"));
        System.out.println("build.commit = " + filtered.getProperty("build.commit"));
        System.out.println("api.url      = " + filtered.getProperty("api.url"));
        System.out.println("notes (raw)  = " + verbatim.getProperty("note"));
    }

    private static Properties load(String resource) throws IOException {
        Properties props = new Properties();
        try (InputStream in = App.class.getResourceAsStream(resource)) {
            if (in == null) {
                throw new IllegalStateException("missing resource: " + resource);
            }
            props.load(in);
        }
        return props;
    }
}
