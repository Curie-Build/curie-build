package com.example.core;

/**
 * Simple utility class at the root workspace level.
 * Demonstrates that a library at level 0 can be depended upon
 * by members at deeper nesting levels.
 */
public final class CoreUtils {

    private CoreUtils() {}

    /** Returns the input string reversed. */
    public static String reverse(String s) {
        if (s == null) return null;
        return new StringBuilder(s).reverse().toString();
    }

    /** Returns true when the input is null or blank. */
    public static boolean isBlank(String s) {
        return s == null || s.isBlank();
    }
}