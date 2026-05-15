package com.example.stringutils;

/**
 * Simple string utility library — demonstrates library compilation with Curie.
 * No main class; the JAR is the final output.
 */
public final class StringUtils {

    private StringUtils() {}

    /** Returns true when {@code s} is null or contains only whitespace. */
    public static boolean isBlank(String s) {
        return s == null || s.isBlank();
    }

    /** Capitalises the first character of {@code s} and lowercases the rest. */
    public static String capitalise(String s) {
        if (isBlank(s)) return s;
        return Character.toUpperCase(s.charAt(0)) + s.substring(1).toLowerCase();
    }

    /** Reverses the characters in {@code s}. */
    public static String reverse(String s) {
        if (s == null) return null;
        return new StringBuilder(s).reverse().toString();
    }

    /** Counts the number of occurrences of {@code ch} in {@code s}. */
    public static int countOccurrences(String s, char ch) {
        if (s == null) return 0;
        int count = 0;
        for (int i = 0; i < s.length(); i++) {
            if (s.charAt(i) == ch) count++;
        }
        return count;
    }
}
