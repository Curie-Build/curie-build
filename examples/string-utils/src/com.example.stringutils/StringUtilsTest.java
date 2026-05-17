package com.example.stringutils;

import org.junit.jupiter.api.Test;
import static org.junit.jupiter.api.Assertions.*;

/**
 * Co-located unit tests for StringUtils (flat-package layout).
 * Lives next to the production source in src/com.example.stringutils/.
 */
class StringUtilsTest {

    @Test
    void isBlank_null_returns_true() {
        assertTrue(StringUtils.isBlank(null));
    }

    @Test
    void isBlank_empty_returns_true() {
        assertTrue(StringUtils.isBlank(""));
    }

    @Test
    void isBlank_whitespace_returns_true() {
        assertTrue(StringUtils.isBlank("   "));
    }

    @Test
    void isBlank_nonEmpty_returns_false() {
        assertFalse(StringUtils.isBlank("hello"));
    }

    @Test
    void capitalise_basic() {
        assertEquals("Hello", StringUtils.capitalise("hello"));
    }

    @Test
    void capitalise_already_upper() {
        assertEquals("Hello", StringUtils.capitalise("HELLO"));
    }

    @Test
    void capitalise_null_returns_null() {
        assertNull(StringUtils.capitalise(null));
    }

    @Test
    void reverse_basic() {
        assertEquals("olleh", StringUtils.reverse("hello"));
    }

    @Test
    void reverse_null_returns_null() {
        assertNull(StringUtils.reverse(null));
    }

    @Test
    void countOccurrences_basic() {
        assertEquals(3, StringUtils.countOccurrences("banana", 'a'));
    }

    @Test
    void countOccurrences_none() {
        assertEquals(0, StringUtils.countOccurrences("hello", 'z'));
    }
}
