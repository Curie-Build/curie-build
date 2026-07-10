package com.example.legacy;

import java.io.IOException;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;

/** Message from the foreign Make library; suffix comes from build-time env. */
public final class LegacyMessage {
    private LegacyMessage() {}

    public static String text() {
        String suffix = "";
        try (InputStream in = LegacyMessage.class.getResourceAsStream("suffix.txt")) {
            if (in != null) {
                suffix = new String(in.readAllBytes(), StandardCharsets.UTF_8);
            }
        } catch (IOException ignored) {
            // leave suffix empty
        }
        return "from-legacy" + suffix;
    }
}
