package com.example.stringutils;

import org.junit.jupiter.api.Test;
import static org.junit.jupiter.api.Assertions.*;

/**
 * Integration / edge-case tests for StringUtils (flat-package layout).
 * Lives in the separate top-level tests/com.example.stringutils/ directory.
 */
class StringUtilsEdgeCaseTest {

    @Test
    void capitalise_single_char() {
        assertEquals("A", StringUtils.capitalise("a"));
    }

    @Test
    void capitalise_blank_returns_blank() {
        assertEquals("   ", StringUtils.capitalise("   "));
    }

    @Test
    void reverse_single_char() {
        assertEquals("x", StringUtils.reverse("x"));
    }

    @Test
    void reverse_empty_string() {
        assertEquals("", StringUtils.reverse(""));
    }

    @Test
    void reverse_palindrome() {
        assertEquals("racecar", StringUtils.reverse("racecar"));
    }

    @Test
    void countOccurrences_null_returns_zero() {
        assertEquals(0, StringUtils.countOccurrences(null, 'a'));
    }

    @Test
    void countOccurrences_entire_string() {
        assertEquals(5, StringUtils.countOccurrences("aaaaa", 'a'));
    }
}
